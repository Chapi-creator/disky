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

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::domain::scan::{DirStat, LargestFile, ScanProgress, ScanTotals};

/// Frecuencia de emisión de progreso (entradas procesadas).
const PROGRESS_EVERY: u64 = 8_192;

/// Archivos más pesados que se recogen por escaneo (el resto se descarta).
///
/// Los tuples usan `Reverse` para que el `BinaryHeap` (por defecto un
/// max-heap) se comporte como un min-heap: el tope es siempre el archivo
/// MENOS pesado de los que caben, y `pop` lo descarta al llenarse.
const TOP_N: usize = 50;

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

/// Recorre el árbol bajo `root` en post-orden, paralelizando con un pool de
/// hilos (uno por núcleo lógico, tope 16).
///
/// Por cada directorio completado, el hilo llamador invoca `on_dir` con su
/// [`DirStat`] (incluida la raíz, al final) — los workers solo recorren y
/// `on_dir`/`on_progress` corren en el hilo que llamó a `walk_tree`. Cada
/// [`PROGRESS_EVERY`] entradas se invoca `on_progress` con los totales
/// acumulados. Si `cancel` se activa, aborta con [`WalkError::Cancelled`] lo
/// antes posible (sin emitir los directorios pendientes).
///
/// El post-orden está garantizado por construcción: un directorio solo se
/// completa cuando todos sus subdirectorios terminaron.
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
    if cancel.load(Ordering::Relaxed) {
        return Err(WalkError::Cancelled);
    }

    let (done_tx, done_rx) = mpsc::channel::<DirStat>();
    let shared = WalkShared {
        queue: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        inflight: AtomicUsize::new(1),
        cancel,
        cancelled: AtomicBool::new(false),
        seen: AtomicU64::new(0),
        files: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        done_tx,
    };
    let root_node = Arc::new(Node::new(root.to_path_buf(), mtime_unix(root), None));
    shared
        .queue
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(root_node);

    let worker_count = thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 16));
    let mut heaps: Vec<TopHeap> = (0..worker_count).map(|_| TopHeap::new()).collect();

    thread::scope(|scope| {
        for heap in &mut heaps {
            scope.spawn(|| worker(&shared, heap));
        }
        drain(&shared, &done_rx, on_dir, on_progress);
    });

    if shared.cancelled.load(Ordering::SeqCst) {
        return Err(WalkError::Cancelled);
    }

    // El top-N global sale de fusionar los heaps de cada worker.
    let mut merged = TopHeap::new();
    for heap in heaps {
        for item in heap {
            merged.push(item);
            if merged.len() > TOP_N {
                merged.pop();
            }
        }
    }
    let mut top: Vec<LargestFile> = merged
        .into_iter()
        .map(|Reverse((size, mtime, path))| LargestFile {
            path,
            size_bytes: size,
            mtime_unix: mtime,
        })
        .collect();
    top.sort_by_key(|f| std::cmp::Reverse(f.size_bytes));

    Ok(ScanTotals {
        files: shared.files.load(Ordering::Relaxed),
        dirs: shared.dirs.load(Ordering::Relaxed),
        bytes: shared.bytes.load(Ordering::Relaxed),
        read_errors: shared.errors.load(Ordering::Relaxed),
        top,
    })
}

/// Top-N de archivos más pesados: min-heap de `(peso, mtime, ruta)`.
type TopHeap = BinaryHeap<Reverse<(u64, i64, String)>>;

/// Una entrada de directorio con sus metadatos ya resueltos.
struct RawEntry {
    name: OsString,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime: i64,
}

/// Lista `path` en una sola pasada resolviendo nombre, tipo, tamaño y mtime
/// **sin** syscall extra por archivo. En Windows usa `FindFirstFileW`, que
/// entrega todo junto; el fallback `std::fs` (CI/otros SO) repite el patrón
/// anterior con `read_dir` + `metadata`.
fn list_dir(path: &Path) -> Result<Vec<RawEntry>, ()> {
    #[cfg(windows)]
    {
        list_dir_win32(path)
    }
    #[cfg(not(windows))]
    {
        list_dir_std(path)
    }
}

/// Versión Win32 de [`list_dir`]: `FindFirstFileW` devuelve `WIN32_FIND_DATAW`
/// con atributos, tamaño y fechas en la misma llamada.
#[cfg(windows)]
fn list_dir_win32(path: &Path) -> Result<Vec<RawEntry>, ()> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
    };

    // Patrón de búsqueda: el prefijo verbatim `\\?\` evita el límite de 260
    // caracteres (std lo añade solo; `FindFirstFileW` no). La ruta ya viene
    // normalizada con separadores `\`, así que el prefijo es seguro.
    let raw = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    let mut pattern = Vec::with_capacity(raw.len() + 8);
    let is_unc = raw.len() >= 2 && raw[0] == '\\' as u16 && raw[1] == '\\' as u16;
    if is_unc {
        // UNC: `\\server\share\...` → `\\?\UNC\server\share\...`
        pattern.extend(r"\\?\UNC\".encode_utf16());
        pattern.extend(raw.iter().skip(2).copied());
    } else {
        pattern.extend(r"\\?\".encode_utf16());
        pattern.extend(raw);
    }
    if pattern.last().copied() != Some('\\' as u16) {
        pattern.push('\\' as u16);
    }
    pattern.push('*' as u16);
    pattern.push(0);

    let mut find_data = WIN32_FIND_DATAW::default();
    let handle = match unsafe {
        FindFirstFileW(windows::core::PCWSTR(pattern.as_ptr()), &raw mut find_data)
    } {
        Ok(h) => h,
        // Directorio vacío: se reporta como lista vacía, no como error.
        Err(e) if e.code().0 & 0xFFFF == 2 => return Ok(Vec::new()),
        Err(_) => return Err(()),
    };

    let mut out = Vec::new();
    let mut done = false;
    while !done {
        let name_len = find_data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(find_data.cFileName.len());
        let name = OsString::from_wide(&find_data.cFileName[..name_len]);
        if name != "." && name != ".." {
            let flags = find_data.dwFileAttributes;
            out.push(RawEntry {
                name,
                is_dir: flags & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                is_symlink: flags & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
                size: (u64::from(find_data.nFileSizeHigh) << 32)
                    | u64::from(find_data.nFileSizeLow),
                mtime: filetime_to_unix(find_data.ftLastWriteTime),
            });
        }
        match unsafe { FindNextFileW(handle, &raw mut find_data) } {
            Ok(()) => {}
            Err(_) => done = true,
        }
    }
    let _ = unsafe { FindClose(handle) };
    Ok(out)
}

/// FILETIME (100 ns desde 1601, tiempo UTC) → segundos UNIX; 0 si la fecha es
/// anterior a la época (cdebe ser imposible para archivos reales).
#[cfg(windows)]
fn filetime_to_unix(ft: windows::Win32::Foundation::FILETIME) -> i64 {
    let hundred_ns = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let unix_offset = 11_644_473_600_u64; // segundos entre 1601 y 1970
    i64::try_from(hundred_ns / 10_000_000)
        .map(|secs| secs.saturating_sub(i64::try_from(unix_offset).unwrap_or(0)))
        .unwrap_or_default()
}

/// Fallback portable de [`list_dir`] para SO no Windows: `read_dir` + metadatos
/// por archivo (2 syscalls por entrada, es aceptable fuera del caso objetivo).
#[cfg(not(windows))]
fn list_dir_std(path: &Path) -> Result<Vec<RawEntry>, ()> {
    let entries = fs::read_dir(path).map_err(|_| ())?;
    let mut out = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return Err(());
        };
        let Ok(file_type) = entry.file_type() else {
            return Err(());
        };
        let is_symlink = file_type.is_symlink();
        let meta = entry.metadata().map_err(|_| ())?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or_default();
        out.push(RawEntry {
            name: entry.file_name(),
            is_dir: file_type.is_dir(),
            is_symlink,
            size: meta.len(),
            mtime,
        });
    }
    Ok(out)
}

/// Un directorio en curso: acumula su propio peso y espera a sus hijos.
struct Node {
    path: PathBuf,
    mtime: i64,
    parent: Option<Arc<Node>>,
    /// Subdirectorios aún no terminados.
    pending: AtomicUsize,
    /// `true` cuando este worker terminó de listar el directorio.
    done: AtomicBool,
    /// Evita colapsar el mismo nodo dos veces (worker vs. último hijo).
    collapsed: AtomicBool,
    /// Peso acumulado: archivos directos + subárboles ya colapsados.
    sub_bytes: AtomicU64,
    /// Archivos directos + subárboles ya colapsados.
    sub_files: AtomicU64,
}

impl Node {
    fn new(path: PathBuf, mtime: i64, parent: Option<Arc<Node>>) -> Self {
        Self {
            path,
            mtime,
            parent,
            pending: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            collapsed: AtomicBool::new(false),
            sub_bytes: AtomicU64::new(0),
            sub_files: AtomicU64::new(0),
        }
    }
}

/// Estado compartido por los workers y el hilo que drena los resultados.
struct WalkShared<'a> {
    queue: Mutex<VecDeque<Arc<Node>>>,
    wake: Condvar,
    /// Directorios creados pero aún no colapsados (1 raíz + 1 por subdir).
    inflight: AtomicUsize,
    cancel: &'a AtomicBool,
    cancelled: AtomicBool,
    seen: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    done_tx: mpsc::Sender<DirStat>,
}

/// Candidato al top-N mediante comparación (sin syscall extra).
fn push_top(heap: &mut TopHeap, size: u64, mtime: i64, path: &Path) {
    let keep = if heap.len() < TOP_N {
        true
    } else {
        size > heap.peek().map_or(0, |Reverse((min, _, _))| *min)
    };
    if keep {
        heap.push(Reverse((size, mtime, path.display().to_string())));
        if heap.len() > TOP_N {
            heap.pop();
        }
    }
}

/// Ciclo del worker: toma directorios de la cola hasta agotarlos.
fn worker(shared: &WalkShared<'_>, top: &mut TopHeap) {
    loop {
        let node = {
            let mut queue = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(node) = queue.pop_front() {
                    break node;
                }
                if shared.inflight.load(Ordering::Relaxed) == 0 {
                    return;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        process_node(shared, node, top);
    }
}

/// Lista un directorio: suma los archivos directos, encola subdirectorios y,
/// cuando el nodo queda completo, lo colapsa (emite el roll-up al hilo que
/// drena). Si `cancel` se activó, el nodo se descarta sin emitir.
fn process_node(shared: &WalkShared<'_>, node: Arc<Node>, top: &mut TopHeap) {
    if shared.cancelled.load(Ordering::SeqCst) {
        node.done.store(true, Ordering::SeqCst);
        collapse(shared, node);
        return;
    }
    if shared.cancel.load(Ordering::Relaxed) {
        shared.cancelled.store(true, Ordering::SeqCst);
        node.done.store(true, Ordering::SeqCst);
        collapse(shared, node);
        return;
    }

    let Ok(entries) = list_dir(&node.path) else {
        // Sin permisos o carrera con el FS: la carpeta se emite vacía y el
        // escaneo sigue. Así la UI muestra el hueco en vez de fallar todo.
        shared.errors.fetch_add(1, Ordering::Relaxed);
        node.done.store(true, Ordering::SeqCst);
        collapse(shared, node);
        return;
    };

    for entry in entries {
        shared.seen.fetch_add(1, Ordering::Relaxed);
        if entry.is_symlink {
            // Ni se recorre ni se emite: su contenido pertenece al destino.
            continue;
        }
        if shared.cancel.load(Ordering::Relaxed) {
            shared.cancelled.store(true, Ordering::SeqCst);
            break;
        }

        let entry_path = node.path.join(&entry.name);
        if entry.is_dir {
            let child = Arc::new(Node::new(entry_path, entry.mtime, Some(Arc::clone(&node))));
            node.pending.fetch_add(1, Ordering::SeqCst);
            shared.inflight.fetch_add(1, Ordering::SeqCst);
            shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(child);
            shared.wake.notify_one();
            continue;
        }

        push_top(top, entry.size, entry.mtime, &entry_path);
        node.sub_bytes.fetch_add(entry.size, Ordering::SeqCst);
        node.sub_files.fetch_add(1, Ordering::SeqCst);
        shared.bytes.fetch_add(entry.size, Ordering::Relaxed);
        shared.files.fetch_add(1, Ordering::Relaxed);
    }

    if shared.cancelled.load(Ordering::SeqCst) {
        node.done.store(true, Ordering::SeqCst);
        collapse(shared, node);
        return;
    }
    node.done.store(true, Ordering::SeqCst);
    if node.pending.load(Ordering::SeqCst) == 0 {
        collapse(shared, node);
    }
}

/// Emite el roll-up de un nodo completo y lo propaga hacia la raíz.
///
/// Un nodo se colapsa una sola vez (su último hijo lo completa, o el worker
/// que lo listó si no tenía hijos). Emitir hacia arriba conserva el post-orden
/// del árbol.
fn collapse(shared: &WalkShared<'_>, node: Arc<Node>) {
    let mut current = Some(node);
    while let Some(node) = current {
        current = None;
        if node.collapsed.swap(true, Ordering::SeqCst) {
            return;
        }
        let bytes = node.sub_bytes.load(Ordering::SeqCst);
        let files = node.sub_files.load(Ordering::SeqCst);
        if !shared.cancelled.load(Ordering::SeqCst) {
            let _ = shared.done_tx.send(DirStat {
                path: node.path.display().to_string(),
                size_bytes: bytes,
                mtime_unix: node.mtime,
                files,
            });
            shared.dirs.fetch_add(1, Ordering::Relaxed);
        }
        let was = shared.inflight.fetch_sub(1, Ordering::SeqCst);
        let Some(parent) = &node.parent else {
            if was == 1 {
                shared.wake.notify_all();
            }
            return;
        };
        parent.sub_bytes.fetch_add(bytes, Ordering::SeqCst);
        parent.sub_files.fetch_add(files, Ordering::SeqCst);
        if parent.pending.fetch_sub(1, Ordering::SeqCst) == 1 && parent.done.load(Ordering::SeqCst)
        {
            current = Some(Arc::clone(parent));
        }
    }
}

/// Drena los `DirStat`s de los workers hacia `on_dir`, emitiendo `on_progress`
/// con la cadencia de [`PROGRESS_EVERY`]. Termina cuando se colapsa la raíz.
fn drain(
    shared: &WalkShared<'_>,
    rx: &mpsc::Receiver<DirStat>,
    on_dir: &mut dyn FnMut(DirStat),
    on_progress: &mut dyn FnMut(ScanProgress),
) {
    let mut last_pct = 0_u64;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(dir) => {
                on_dir(dir);
                emit_progress(shared, &mut last_pct, on_progress);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                emit_progress(shared, &mut last_pct, on_progress);
                if shared.inflight.load(Ordering::SeqCst) == 0 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Vaciar lo que quedara ya en el canal antes de cerrar.
    while let Ok(dir) = rx.try_recv() {
        on_dir(dir);
        emit_progress(shared, &mut last_pct, on_progress);
    }
}

fn emit_progress(
    shared: &WalkShared<'_>,
    last_pct: &mut u64,
    on_progress: &mut dyn FnMut(ScanProgress),
) {
    let pct = shared.seen.load(Ordering::Relaxed) / PROGRESS_EVERY;
    if pct > *last_pct {
        *last_pct = pct;
        on_progress(ScanProgress {
            files: shared.files.load(Ordering::Relaxed),
            dirs: shared.dirs.load(Ordering::Relaxed),
            bytes: shared.bytes.load(Ordering::Relaxed),
            read_errors: shared.errors.load(Ordering::Relaxed),
        });
    }
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
        assert_eq!(
            totals.top.iter().map(|f| f.size_bytes).collect::<Vec<_>>(),
            vec![1_000, 100, 5],
            "top-N ordenado desc por peso"
        );
        assert!(totals.top[0].path.ends_with("b.bin"));

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

    #[test]
    fn top_n_keeps_only_the_largest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // 60 archivos con pesos 1..60: el top-N (50) debe quedarse con los
        // 50 más pesados (11..60).
        for i in 1..=60 {
            write_file(&root.join(format!("f{i:03}.dat")), i);
        }

        let totals =
            walk_tree(root, &AtomicBool::new(false), &mut |_| {}, &mut |_| {}).expect("walk ok");

        assert_eq!(totals.files, 60);
        let weights = totals
            .top
            .iter()
            .map(|f| f.size_bytes)
            .collect::<Vec<u64>>();
        assert_eq!(weights.len(), 50);
        assert_eq!(weights[0], 60, "el mayor primero");
        assert_eq!(weights[49], 11, "el menor del top-N retenido");
        assert!(!weights.contains(&10), "fuera del top-N");
    }

    #[cfg(windows)]
    #[test]
    fn win32_list_dir_matches_stdlib() {
        // El listado FindFirstFileW debe coincidir (nombre, tamaño, dir) con lo
        // que ve la stdlib en la misma carpeta; así no regresamos basura/carrera.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("sub")).expect("crear dirs");
        write_file(&root.join("a.txt"), 100);
        write_file(&root.join("sub").join("b.bin"), 2_000);
        // Longitud por encima de 260 chars: solo el patrón verbatim la aguanta.
        let long = root.join("long-".repeat(40));
        write_file(&long, 7);

        let win = list_dir(root).expect("list_dir win32");
        let mut std_entries = Vec::new();
        for e in fs::read_dir(root).expect("read_dir") {
            let e = e.expect("entry");
            let meta = e.metadata().expect("metadata");
            std_entries.push((e.file_name(), meta.is_dir(), meta.len()));
        }

        let mut got: Vec<(OsString, bool, u64)> = win
            .into_iter()
            .map(|e| (e.name.clone(), e.is_dir, e.size))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        std_entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, std_entries, "dirs, archivos y tamaños idénticos");

        let sub = list_dir(&root.join("sub")).expect("list_dir sub");
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "b.bin");
        assert_eq!(sub[0].size, 2_000);
        assert!(!sub[0].is_dir);
    }
}
