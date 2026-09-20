//! Walker de directorios (el fallback sin admin).
//!
//! Recorre el árbol con `std::fs` y permisos normales — sin `WinAPI`, sin `MFT`,
//! sin elevación — y emite cada directorio en **post-orden** con el roll-up de
//! su subárbol, de modo que el diff de crecimiento solo necesita directorios.
//!
//! Decisiones de diseño (deliberadas, no descuidos):
//! - Los enlaces simbólicos y junctions **no se recorren** (bucles, doble
//!   conteo) y no se emiten: su contenido real pertenece al destino.
//! - Las carpetas que no se pueden listar (permisos, carreras con el FS) se
//!   emiten con tamaño 0 y cuentan como error legible, no detienen el escaneo.
//! - Es portable (`std::fs` puro): funciona igual en CI que en Windows.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use crate::domain::scan::{DirStat, ScanProgress, ScanTotals};

/// Frecuencia de emisión de progreso (entradas procesadas).
const PROGRESS_EVERY: u64 = 8_192;

/// Error del walker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalkError {
    /// La raíz no existe o no es un directorio accesible.
    #[error("la raíz no existe o no es un directorio accesible: `{0}`")]
    InvalidRoot(String),
    /// El usuario pidió cancelar; el escaneo se aborta sin guardar nada.
    #[error("escaneo cancelado")]
    Cancelled,
}

/// Recorre el árbol bajo `root` en post-orden.
///
/// Por cada directorio completado llama a `on_dir` con su [`DirStat`]
/// (incluida la raíz, al final). Cada [`PROGRESS_EVERY`] entradas llama a
/// `on_progress` con los totales acumulados. Si `cancel` se activa, aborta con
/// [`WalkError::Cancelled`] lo antes posible.
///
/// # Errors
/// [`WalkError::InvalidRoot`] si la raíz no es un directorio accesible;
/// [`WalkError::Cancelled`] si se pidió cancelar. Los errores por-entrada no
/// abortan: se acumulan en [`ScanTotals::read_errors`].
pub fn walk_tree(
    root: &Path,
    cancel: &AtomicBool,
    on_dir: &mut dyn FnMut(DirStat),
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<ScanTotals, WalkError> {
    if !root.is_dir() {
        return Err(WalkError::InvalidRoot(root.display().to_string()));
    }

    let mut acc = Acc::default();
    visit_dir(root, cancel, &mut acc, on_dir, on_progress)?;
    Ok(acc.totals())
}

/// Acumuladores mutables del escaneo en curso.
#[derive(Default)]
struct Acc {
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    /// Entradas vistas (para la cadencia de progreso).
    seen: u64,
}

impl Acc {
    const fn totals(&self) -> ScanTotals {
        ScanTotals {
            files: self.files,
            dirs: self.dirs,
            bytes: self.bytes,
            read_errors: self.errors,
        }
    }
}

/// Visita recursiva: devuelve el roll-up (bytes, archivos) del subárbol.
///
/// # Errors
/// [`WalkError::Cancelled`] si se pidió cancelar.
fn visit_dir(
    dir: &Path,
    cancel: &AtomicBool,
    acc: &mut Acc,
    on_dir: &mut dyn FnMut(DirStat),
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<(u64, u64), WalkError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(WalkError::Cancelled);
    }

    let mtime = mtime_unix(dir);

    let Ok(entries) = fs::read_dir(dir) else {
        // Sin permisos o carrera con el FS: la carpeta se emite vacía y el
        // escaneo sigue. Así la UI muestra el hueco en vez de fallar todo.
        acc.errors += 1;
        acc.dirs += 1;
        on_dir(DirStat {
            path: dir.display().to_string(),
            size_bytes: 0,
            mtime_unix: mtime,
            files: 0,
        });
        return Ok((0, 0));
    };

    let mut subtree_bytes = 0;
    let mut subtree_files = 0;

    for entry in entries {
        acc.seen += 1;
        if acc.seen.is_multiple_of(PROGRESS_EVERY) {
            on_progress(ScanProgress {
                files: acc.files,
                dirs: acc.dirs,
                bytes: acc.bytes,
                read_errors: acc.errors,
            });
            if cancel.load(Ordering::Relaxed) {
                return Err(WalkError::Cancelled);
            }
        }

        let Ok(entry) = entry else {
            acc.errors += 1;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            acc.errors += 1;
            continue;
        };

        if file_type.is_symlink() {
            // Ni se recorre ni se emite: su contenido pertenece al destino.
            continue;
        }

        let entry_path = entry.path();
        if file_type.is_dir() {
            let (bytes, files) = visit_dir(&entry_path, cancel, acc, on_dir, on_progress)?;
            subtree_bytes += bytes;
            subtree_files += files;
            continue;
        }

        // `DirEntry::metadata` no sigue enlaces y en Windows sale del listado
        // del directorio: sin syscall extra por archivo.
        let Ok(meta) = entry.metadata() else {
            acc.errors += 1;
            continue;
        };
        let size = meta.len();
        subtree_bytes += size;
        subtree_files += 1;
        acc.bytes += size;
        acc.files += 1;
    }

    acc.dirs += 1;
    on_dir(DirStat {
        path: dir.display().to_string(),
        size_bytes: subtree_bytes,
        mtime_unix: mtime,
        files: subtree_files,
    });
    Ok((subtree_bytes, subtree_files))
}

/// mtime de una ruta en segundos UNIX (0 si no se pudo leer).
fn mtime_unix(path: &Path) -> i64 {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::cell::RefCell;

    fn write_file(path: &Path, size: usize) {
        fs::write(path, vec![0_u8; size]).expect("escribir archivo de prueba");
    }

    #[test]
    fn rolls_up_sizes_in_post_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("sub").join("deep")).expect("crear dirs");
        write_file(&root.join("a.txt"), 100);
        write_file(&root.join("sub").join("b.bin"), 1_000);
        write_file(&root.join("sub").join("deep").join("c.dat"), 5);

        let dirs = RefCell::new(Vec::new());
        let totals = walk_tree(
            root,
            &AtomicBool::new(false),
            &mut |d| dirs.borrow_mut().push(d),
            &mut |_| {},
        )
        .expect("walk ok");

        assert_eq!(totals.files, 3);
        assert_eq!(totals.bytes, 1_105);
        assert_eq!(totals.dirs, 3);
        assert_eq!(totals.read_errors, 0);

        let dirs = dirs.into_inner();
        let position_of = |suffix: &str| {
            dirs.iter()
                .position(|d| d.path.ends_with(suffix))
                .expect("directorio presente")
        };
        let (deep_pos, sub_pos) = (position_of("deep"), position_of("sub"));
        assert!(deep_pos < sub_pos, "post-orden: deep antes que sub");
        assert!(sub_pos < dirs.len() - 1, "la raíz se emite al final");

        assert_eq!(dirs[deep_pos].size_bytes, 5);
        assert_eq!(dirs[deep_pos].files, 1);
        assert_eq!(dirs[sub_pos].size_bytes, 1_005);
        assert_eq!(dirs[sub_pos].files, 2);
        assert_eq!(dirs[dirs.len() - 1].size_bytes, 1_105);
        assert_eq!(dirs[dirs.len() - 1].files, 3);
        assert!(dirs[dirs.len() - 1].mtime_unix > 0);
    }

    #[test]
    fn empty_dirs_are_emitted_with_zero_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("vacio")).expect("crear dir");

        let dirs = RefCell::new(Vec::new());
        let totals = walk_tree(
            tmp.path(),
            &AtomicBool::new(false),
            &mut |d| dirs.borrow_mut().push(d),
            &mut |_| {},
        )
        .expect("walk ok");

        assert_eq!(totals.dirs, 2);
        assert_eq!(totals.files, 0);
        let vacio = dirs
            .into_inner()
            .into_iter()
            .find(|d| d.path.ends_with("vacio"))
            .expect("directorio vacío presente");
        assert_eq!(vacio.size_bytes, 0);
        assert_eq!(vacio.files, 0);
    }

    #[test]
    fn cancel_before_start_returns_cancelled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cancel = AtomicBool::new(true);

        let result = walk_tree(tmp.path(), &cancel, &mut |_| {}, &mut |_| {});

        assert_eq!(result, Err(WalkError::Cancelled));
    }

    #[test]
    fn invalid_root_is_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("no-existe");

        let result = walk_tree(&missing, &AtomicBool::new(false), &mut |_| {}, &mut |_| {});

        assert!(matches!(result, Err(WalkError::InvalidRoot(_))));
    }
}
