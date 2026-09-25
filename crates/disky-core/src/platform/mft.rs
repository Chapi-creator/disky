//! Escaneo MFT (la vía rápida elevada).
//!
//! Debate de diseño (verificado contra el formato on-disk de NTFS): los
//! registros que devuelve `FSCTL_ENUM_USN_DATA` NO llevan el tamaño del
//! archivo (`USN_RECORD_V2` solo tiene FRN, padre, nombre y tiempos). Para
//! construir el snapshot con tamaños hace falta leer los `FILE_RECORD` crudos
//! del `$MFT`, que sí contienen `$FILE_NAME` (padre, nombre, `RealSize`).
//!
//! Flujo, estilo "Everything":
//! 1. Abrir `\\.\X:` (elevado) y leer el boot sector → `$MFT` LCN y tamaño de
//!    registro.
//! 2. Leer el registro 0 (`$MFT`) y su atributo `$DATA` no-residente → run
//!    list (extents). Con eso se recorre todo el `$MFT` aunque esté
//!    fragmentado.
//! 3. Por cada `FILE_RECORD` en uso y no-extensión: `$FILE_NAME` → FRN padre,
//!    nombre, tamaño real, mtime.
//! 4. Se arma el índice padre→hijos, se resuelve la raíz pedida por
//!    componentes (case-insensitive, NTFS no distingue) desde el FRN raíz 5,
//!    y se agrega en post-orden como el walker.
//!
//! Límites deliberados (ponytail):
//! - Requiere elevación (mismo handle que `usn.rs`). Si `mft_scan` falla, el
//!   llamador cae al walker sin admin.
//! - Solo FS NTFS.
//! - El índice es de todo el volumen: ~200 MB de RAM en discos grandes. Si
//!   esto molesta, pasará a índices `frn→vec idx` o lectura en dos pasadas.
//! - Archivos huérfanos (padre ausente de la MFT) se descartan: sin camino
//!   real no aportan a ninguna carpeta (coincide con lo que el usuario ve).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::OVERLAPPED;

use super::PlatformError;
use crate::domain::scan::{DirStat, LargestFile, ScanProgress, ScanTotals};
use crate::domain::usn::filetime_to_unix;

/// Archivos más pesados recogidos (el resto se descarta; igual que el walker).
const TOP_N: usize = 50;

/// Tamaño máximo de cada lectura del `$MFT`.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Tamaño máximo aceptado para un registro de la MFT (64 KiB, el tope real
/// de NTFS). Más arriba el boot sector está corrupto: se rechaza en vez de
/// intentar un `vec![0u8; …]` de gigabytes.
const MAX_RECORD_SIZE: usize = 64 * 1024;

/// FRN convencional de la raíz de un volumen NTFS.
const NTFS_ROOT_FRN: u64 = 5;

/// Constante de atributo `$FILE_NAME`.
const ATTR_FILE_NAME: u32 = 0x30;
/// Constante de atributo `$DATA`.
const ATTR_DATA: u32 = 0x80;
/// `FILE_RECORD` en uso.
const FLAG_IN_USE: u16 = 0x0001;
/// `FILE_RECORD` de directorio.
const FLAG_DIRECTORY: u16 = 0x0002;
/// Fin de la lista de atributos.
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Info parseada del boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BootInfo {
    /// Bytes por cluster (`sector` × `sectores`).
    cluster_bytes: u64,
    /// LCN donde empieza el `$MFT`.
    mft_lcn: u64,
    /// Tamaño en bytes de cada `FILE_RECORD`.
    record_size: usize,
}

/// Un `FILE_RECORD` ya parseado (solo los campos que interesan).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRecord {
    /// FRN del directorio padre.
    parent_frn: u64,
    /// Nombre de `$FILE_NAME` preferido (no-DOS si existe).
    name: String,
    /// True si el record es un directorio.
    is_dir: bool,
    /// Tamaño real del archivo (`$FILE_NAME.RealSize`); 0 en directorios.
    size: u64,
    /// Última modificación (UNIX, segundos).
    mtime_unix: i64,
}

/// Error del escaneo MFT.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MftError {
    /// La raíz no existe o no es un directorio accesible.
    #[error("la raíz no existe o no es un directorio accesible: `{0}`")]
    InvalidRoot(String),
    /// El usuario pidió cancelar.
    #[error("escaneo cancelado")]
    Cancelled,
    /// El volumen no es NTFS o el boot sector no se pudo leer.
    #[error("volumen no NTFS o boot sector ilegible")]
    NotNtfs,
    /// El `$MFT` no respondió como se esperaba (formato inesperado).
    #[error("formato MFT inesperado: {0}")]
    BadMft(String),
    /// La raíz pedida no existe en el árbol de la MFT.
    #[error("la ruta `{0}` no existe en la MFT del volumen")]
    PathNotFound(String),
    /// Windows rechazó una operación.
    #[error("error de Windows: {0}")]
    Windows(#[from] PlatformError),
}

/// Traduce el código de un `windows::core::Error` (HRESULT) al número Win32.
/// Los HRESULT de error Win32 son `0x8007xxxx`; `xxxx` es el código original.
#[allow(clippy::cast_sign_loss)] // la palabra baja de un HRESULT de error es >= 0
fn win32_code(err: &windows::core::Error) -> u32 {
    (err.code().0 & 0xFFFF) as u32
}

/// Handle de volumen con cierre automático.
struct VolumeHandle(HANDLE);

impl VolumeHandle {
    fn open(letter: char) -> Result<Self, PlatformError> {
        let device = format!(r"\\.\{letter}:");
        let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| PlatformError::WindowsApi {
            letter: letter.to_string(),
            code: win32_code(&e),
        })?;
        Ok(Self(handle))
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Lee exactamente `buf.len()` bytes del volumen desde `offset` (posición
/// absoluta en el dispositivo).
fn read_at(handle: HANDLE, offset: u64, buf: &mut [u8]) -> Result<(), PlatformError> {
    let seek = i64::try_from(offset).unwrap_or(i64::MAX);
    unsafe {
        windows::Win32::Storage::FileSystem::SetFilePointerEx(handle, seek, None, FILE_BEGIN)
    }
    .map_err(|e| PlatformError::WindowsApi {
        letter: "<volumen>".to_owned(),
        code: win32_code(&e),
    })?;

    let mut total = 0usize;
    while total < buf.len() {
        let mut read = 0u32;
        // SAFETY: `buf[total..]` es un slice válido; lectura síncrona sin overlap.
        let ok = unsafe {
            ReadFile(
                handle,
                Some(&mut buf[total..]),
                Some(&raw mut read),
                None as Option<*mut OVERLAPPED>,
            )
        };
        if ok.is_err() || read == 0 {
            return Err(PlatformError::WindowsApi {
                letter: "<volumen>".to_owned(),
                code: 0,
            });
        }
        total += read as usize;
    }
    Ok(())
}

/// Parsea el boot sector NTFS (primeros 512 bytes del volumen).
#[allow(clippy::cast_sign_loss)] // `raw as u64` solo ocurre con raw > 0
fn parse_boot(boot: &[u8]) -> Option<BootInfo> {
    if boot.len() < 0x48 || &boot[0x03..0x0B] != b"NTFS    " {
        return None;
    }
    let bytes_per_sector = u64::from(u16::from_le_bytes(boot[0x0B..0x0D].try_into().ok()?));
    let sectors_per_cluster = u64::from(boot[0x0D]);
    let cluster_bytes = bytes_per_sector * sectors_per_cluster;
    let mft_lcn = u64::from_le_bytes(boot[0x30..0x38].try_into().ok()?);

    // Clusters por registro: positivo = clusters; negativo = 2^-n bytes.
    #[allow(clippy::cast_possible_wrap)] // 0x40 es el que decide el signo
    let raw = boot[0x40].cast_signed();
    #[allow(clippy::comparison_chain, clippy::cast_lossless)] // orden de precedencia intencional
    let record_size = if raw > 0 {
        cluster_bytes.checked_mul(raw as u64)?
    } else if raw < 0 {
        // `-(raw)` llega hasta 128 (byte 0x80..): un shift ≥ 64 fuera de rango
        // haría panic de Rust en `1u64 << shift` — boot sector corrupto.
        let shift = -(i32::from(raw));
        if shift >= 64 {
            return None;
        }
        1u64 << shift
    } else {
        1024 // valor predeterminado histórico de NTFS
    };
    let record_size = usize::try_from(record_size).ok()?;
    // Un registro real de NTFS va de 0x100 a 64 KiB; acotar impide que un boot
    // corrupto provoque una asignación de cientos de MB en `vec![0u8; …]`.
    if !(64..=MAX_RECORD_SIZE).contains(&record_size) || cluster_bytes == 0 {
        return None;
    }
    Some(BootInfo {
        cluster_bytes,
        mft_lcn,
        record_size,
    })
}

/// Parsea el run list (mapping pairs) de un atributo no-residente.
///
/// Devuelve lista de `(lcn, clusters)`; la lista termina con un byte 0.
fn parse_runlist(data: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    let mut i = 0usize;
    while i < data.len() {
        let header = data[i];
        i += 1;
        if header == 0 {
            break;
        }
        let len_bytes = (header >> 4) as usize;
        let off_bytes = (header & 0x0F) as usize;
        if i + len_bytes + off_bytes > data.len() {
            return None;
        }
        let len = le_unsigned(&data[i..i + len_bytes])?;
        i += len_bytes;
        let delta = le_signed(&data[i..i + off_bytes])?;
        i += off_bytes;
        lcn = lcn.checked_add(delta)?;
        runs.push((u64::try_from(lcn).ok()?, len));
    }
    Some(runs)
}

/// Lee un entero *little-endian* sin signo de ancho variable (≤ 8 bytes).
#[allow(clippy::cast_possible_wrap)]
fn le_unsigned(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut v = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        v |= (u64::from(*b)) << (8 * i);
    }
    Some(v)
}

/// Lee un entero *little-endian* con signo de ancho variable (≤ 8 bytes),
/// extendiendo el signo desde el byte más significativo.
fn le_signed(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut v = 0i64;
    for (i, b) in bytes.iter().enumerate() {
        v |= (i64::from(*b)) << (8 * i);
    }
    if bytes[bytes.len() - 1] & 0x80 != 0 {
        let shift = 8 * bytes.len();
        if shift < 64 {
            v |= -1i64 << shift;
        }
    }
    Some(v)
}

/// Parsea un `FILE_RECORD` y devuelve la entrada si está en uso, no es
/// extensión y tiene al menos un `$FILE_NAME`. `None` en otro caso.
#[must_use]
fn parse_record(raw: &[u8]) -> Option<FileRecord> {
    if raw.len() < 24 || &raw[0..4] != b"FILE" {
        return None;
    }
    let flags = u16::from_le_bytes(raw[0x16..0x18].try_into().ok()?);
    if flags & FLAG_IN_USE == 0 {
        return None;
    }
    // Registros de extensión (base FRN != -1) pertenecen a otro archivo.
    let base = u64::from_le_bytes(raw[0x20..0x28].try_into().ok()?);
    if base != u64::MAX {
        return None;
    }
    let is_dir = flags & FLAG_DIRECTORY != 0;
    let used = u32::from_le_bytes(raw[0x18..0x1C].try_into().ok()?) as usize;
    let first_attr = usize::from(u16::from_le_bytes(raw[0x14..0x16].try_into().ok()?));
    let end = used.min(raw.len());

    let mut best: Option<(u8, String, u64, u64, i64)> = None; // ns, name, parent, size, mtime

    let mut off = first_attr;
    // Los atributos terminan en `$END` (0xFFFFFFFF) o en ceros de relleno.
    while off + 8 <= end {
        let attr_type = u32::from_le_bytes(raw[off..off + 4].try_into().ok()?);
        if attr_type == ATTR_END || attr_type == 0 {
            break;
        }
        let attr_len = u32::from_le_bytes(raw[off + 4..off + 8].try_into().ok()?) as usize;
        if attr_len < 16 || off + attr_len > end {
            break;
        }
        let attr = &raw[off..off + attr_len];
        if attr_type == ATTR_FILE_NAME {
            consider_file_name(attr, &mut best);
        }
        off += attr_len;
    }

    let (_, name, parent_frn, size, mtime_unix) = best?;
    Some(FileRecord {
        parent_frn,
        name,
        is_dir,
        size,
        mtime_unix,
    })
}

/// Evalúa un atributo `$FILE_NAME` residente y lo promueve si es mejor que el
/// actual (prefiere nombres no-DOS; ante igual preferencia, el más largo).
fn consider_file_name(attr: &[u8], best: &mut Option<(u8, String, u64, u64, i64)>) {
    if attr.len() < 8 || attr[8] != 0 {
        return; // no-residente: los FILE_NAME no lo son
    }
    let Some(value_len_bytes) = <[u8; 4]>::try_from(&attr[0x10..0x14]).ok() else {
        return;
    };
    let value_len = u32::from_le_bytes(value_len_bytes) as usize;
    let Some(value_off_bytes) = <[u8; 2]>::try_from(&attr[0x14..0x16]).ok() else {
        return;
    };
    let value_off = usize::from(u16::from_le_bytes(value_off_bytes));
    let Some(value) = attr.get(value_off..value_off + value_len) else {
        return;
    };
    // FILE_NAME: Parent(8) Creation(8) LastMod(8) Access(8) Alloc(8) Real(8)
    //            Flags(4) Reparse(4) NameLen(1) NS(1) Name[]
    if value.len() < 0x3A {
        return;
    }
    let Some(parent_bytes) = <[u8; 8]>::try_from(&value[0..8]).ok() else {
        return;
    };
    let parent_frn = u64::from_le_bytes(parent_bytes);
    let Some(mtime_bytes) = <[u8; 8]>::try_from(&value[0x10..0x18]).ok() else {
        return;
    };
    let mtime = i64::from_le_bytes(mtime_bytes);
    let Some(size_bytes) = <[u8; 8]>::try_from(&value[0x28..0x30]).ok() else {
        return;
    };
    let size = u64::from_le_bytes(size_bytes);
    let name_len = usize::from(value[0x38]);
    let namespace = value[0x39];
    if name_len == 0 || 0x3A + name_len * 2 > value.len() {
        return;
    }
    let name_u16: Vec<u16> = value[0x3A..0x3A + name_len * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name = String::from_utf16_lossy(&name_u16);

    let better = match best {
        None => true,
        Some((cur_ns, cur_name, _, _, _)) => {
            let cur_pref = *cur_ns != 2;
            let new_pref = namespace != 2;
            (new_pref && !cur_pref) || (new_pref == cur_pref && name.len() > cur_name.len())
        }
    };
    if better {
        *best = Some((namespace, name, parent_frn, size, filetime_to_unix(mtime)));
    }
}

/// Devuelve los extents (lcn, clusters) del `$MFT` a partir de su registro 0.
fn mft_extents(rec0: &[u8]) -> Result<Vec<(u64, u64)>, MftError> {
    if rec0.len() < 0x28 || &rec0[0..4] != b"FILE" {
        return Err(MftError::BadMft("registro 0 inválido".into()));
    }
    let first_attr = usize::from(u16::from_le_bytes(
        rec0[0x14..0x16].try_into().unwrap_or([0; 2]),
    ));
    let end = u32::from_le_bytes(rec0[0x18..0x1C].try_into().unwrap_or([0; 4])) as usize;
    let mut off = first_attr;
    while off + 8 <= end.min(rec0.len()) {
        let attr_type = u32::from_le_bytes(rec0[off..off + 4].try_into().unwrap_or([0; 4]));
        if attr_type == ATTR_END || attr_type == 0 {
            break;
        }
        let attr_len =
            u32::from_le_bytes(rec0[off + 4..off + 8].try_into().unwrap_or([0; 4])) as usize;
        if attr_len < 16 || off + attr_len > rec0.len() {
            break;
        }
        let attr = &rec0[off..off + attr_len];
        // $DATA no-residente: el run list vive desde `mp_off` hasta el final
        // del atributo y termina en un byte 0.
        if attr_type == ATTR_DATA && attr.len() > 0x22 && attr[8] == 1 {
            let mp_off = usize::from(u16::from_le_bytes(
                attr[0x20..0x22].try_into().unwrap_or([0; 2]),
            ));
            let map = attr
                .get(mp_off..)
                .ok_or_else(|| MftError::BadMft("run list fuera de rango".into()))?;
            let runs =
                parse_runlist(map).ok_or_else(|| MftError::BadMft("run list ilegible".into()))?;
            return Ok(runs);
        }
        off += attr_len;
    }
    Err(MftError::BadMft("$MFT sin atributo de datos".into()))
}

/// Índice completo de la MFT `(entradas, hijos)`; el `u64` son lecturas rotas.
type MftIndex = (HashMap<u64, FileRecord>, HashMap<u64, Vec<u64>>, u64);

/// Escanea todo el `$MFT` del volumen y devuelve `(entradas, hijos, errores)`.
fn read_mft_index(letter: char) -> Result<MftIndex, MftError> {
    let handle = VolumeHandle::open(letter)?;
    let boot = read_boot_sector(&handle)?;
    let info = parse_boot(&boot).ok_or(MftError::NotNtfs)?;

    // Registro 0 = `$MFT`; su $DATA no-residente trae el run list.
    let mft_base = info.mft_lcn.saturating_mul(info.cluster_bytes);
    let mut rec0 = vec![0u8; info.record_size];
    read_at(handle.0, mft_base, &mut rec0).map_err(MftError::from)?;
    let extents = mft_extents(&rec0)?;
    if extents.is_empty() {
        return Err(MftError::BadMft("el $MFT no tiene datos".into()));
    }

    let mut entries: HashMap<u64, FileRecord> = HashMap::new();
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut read_errors = 0u64;

    // `len_read` es múltiplo del tamaño de registro, para no partir ninguno.
    let chunk_cap = CHUNK_BYTES.max(info.record_size);
    let per_chunk = (chunk_cap / info.record_size) * info.record_size;
    let mut chunk = vec![0u8; per_chunk];
    let mut frn: u64 = 0;

    for (lcn, clusters) in extents {
        let start = lcn.saturating_mul(info.cluster_bytes);
        let bytes = clusters.saturating_mul(info.cluster_bytes);
        let mut covered = 0u64;
        while covered < bytes {
            let want = usize::try_from((bytes - covered).min(per_chunk as u64)).unwrap_or(0);
            if want == 0 {
                break;
            }
            if read_at(handle.0, start + covered, &mut chunk[..want]).is_err() {
                read_errors += 1;
                break;
            }
            for (i, rec) in chunk[..want].chunks_exact(info.record_size).enumerate() {
                let this_frn = frn + i as u64;
                if let Some(file_rec) = parse_record(rec) {
                    children
                        .entry(file_rec.parent_frn)
                        .or_default()
                        .push(this_frn);
                    entries.insert(this_frn, file_rec);
                } else {
                    // `None` es lo NORMAL en un volumen real: ranuras sin
                    // usar (borrados/compactación) y registros de extensión
                    // (ficheros con lista de atributos). Solo un registro "en
                    // uso" sin `$FILE_NAME` legible es una lectura defectuosa.
                    let in_use_base = rec.len() >= 0x28
                        && rec[0..4] == *b"FILE"
                        && u16::from_le_bytes(rec[0x16..0x18].try_into().unwrap_or_default())
                            & FLAG_IN_USE
                            != 0
                        && u64::from_le_bytes(rec[0x20..0x28].try_into().unwrap_or([0xFF; 8]))
                            == u64::MAX;
                    if in_use_base {
                        read_errors += 1;
                    }
                }
            }
            frn += (want / info.record_size) as u64;
            covered += want as u64;
        }
    }

    Ok((entries, children, read_errors))
}

/// Resuelve `components` desde la raíz del volumen hasta un FRN, con
/// comparación case-insensitive (NTFS no distingue mayúsculas).
fn resolve_frn(
    components: &[String],
    entries: &HashMap<u64, FileRecord>,
    children: &HashMap<u64, Vec<u64>>,
) -> Result<u64, String> {
    let mut cur = NTFS_ROOT_FRN;
    for comp in components {
        let frns = children.get(&cur).cloned().unwrap_or_default();
        let next = frns.into_iter().find(|f| {
            entries
                .get(f)
                .is_some_and(|e| e.name.eq_ignore_ascii_case(comp))
        });
        cur = next.ok_or_else(|| comp.clone())?;
    }
    Ok(cur)
}

/// ¿Se puede leer el `$MFT` del volumen `letter` en este proceso?
///
/// Abrir `\\.\X:` con `GENERIC_READ` exige elevación; si el proceso corre como
/// admin, el handle abre y podemos usar [`mft_scan`] in-process (muy rápido) en
/// vez del walker. Un `false` no es un error de escaneo: solo indica que toca
/// el fallback sin admin.
#[must_use]
pub fn mft_available(letter: char) -> bool {
    VolumeHandle::open(letter).is_ok()
}

/// Escanea el subárbol de `root` leyendo el `$MFT` del volumen.
///
/// Emite `DirStat` en post-orden (igual contrato que [`super::walk::walk_tree`]).
///
/// # Errors
/// [`MftError`] si falla el acceso al volumen, no es NTFS, o la raíz no
/// existe en la MFT; el llamador decide caer al walker sin admin.
pub fn mft_scan(
    root: &Path,
    cancel: &AtomicBool,
    on_dir: &mut dyn FnMut(DirStat),
    _on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<ScanTotals, MftError> {
    if !root.is_dir() {
        return Err(MftError::InvalidRoot(root.display().to_string()));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(MftError::Cancelled);
    }

    let s = root.to_string_lossy().replace('/', "\\");
    // Letra de unidad desde la ruta (C:\foo → 'C').
    let letter = s
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase())
        .ok_or_else(|| MftError::InvalidRoot(root.display().to_string()))?;

    let (entries, children, read_errors) = read_mft_index(letter)?;

    // Componentes bajo la raíz del volumen.
    let components: Vec<String> = s
        .split(['\\', '/'])
        .skip(1) // "C:"
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_owned)
        .collect();

    let target = resolve_frn(&components, &entries, &children).map_err(MftError::PathNotFound)?;

    // Post-orden iterativo sobre los directorios del subárbol.
    let mut postorder: Vec<u64> = Vec::new();
    let mut visit_stack: Vec<(u64, bool)> = vec![(target, false)];
    while let Some((frn, exit)) = visit_stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(MftError::Cancelled);
        }
        if exit {
            postorder.push(frn);
            continue;
        }
        let Some(entry) = entries.get(&frn) else {
            continue;
        };
        if !entry.is_dir {
            continue;
        }
        visit_stack.push((frn, true));
        let kids = children.get(&frn).cloned().unwrap_or_default();
        for kid in kids.into_iter().rev() {
            visit_stack.push((kid, false));
        }
    }

    let totals = accumulate(
        &postorder,
        target,
        s,
        read_errors,
        &entries,
        &children,
        on_dir,
    );
    Ok(totals)
}

/// Rutas absolutas, acumulación hijo→padre (post-orden) y emisión de
/// [`DirStat`]. Separado de [`mft_scan`] para mantenerlo legible.
#[allow(clippy::too_many_arguments)]
fn accumulate(
    postorder: &[u64],
    target: u64,
    root_path: String,
    read_errors: u64,
    entries: &HashMap<u64, FileRecord>,
    children: &HashMap<u64, Vec<u64>>,
    on_dir: &mut dyn FnMut(DirStat),
) -> ScanTotals {
    let mut dir_info: HashMap<u64, (String, FileRecord)> = HashMap::new();
    let mut dfs = vec![(target, root_path)];
    while let Some((frn, path)) = dfs.pop() {
        let Some(entry) = entries.get(&frn) else {
            continue;
        };
        if !entry.is_dir {
            continue;
        }
        dir_info.insert(frn, (path.clone(), entry.clone()));
        if let Some(kids) = children.get(&frn) {
            for &kid in kids {
                if let Some(e) = entries.get(&kid) {
                    let sep = if path.ends_with('\\') { "" } else { "\\" };
                    dfs.push((kid, format!("{path}{sep}{}", e.name)));
                }
            }
        }
    }

    // Acumular hijo→padre en post-orden: cada directorio aparece tras sus hijos.
    let mut acc_size: HashMap<u64, u64> = HashMap::new();
    let mut acc_files: HashMap<u64, u64> = HashMap::new();
    let mut top: Vec<LargestFile> = Vec::new();
    let mut dirs_emitted = 0u64;

    for frn in postorder {
        let Some((path, entry)) = dir_info.get(frn).cloned() else {
            continue;
        };
        let mut size = 0u64;
        let mut files = 0u64;
        let kids = children.get(frn).cloned().unwrap_or_default();
        for kid in kids {
            let Some(k) = entries.get(&kid) else {
                continue;
            };
            if k.is_dir {
                size = size.saturating_add(acc_size.get(&kid).copied().unwrap_or(0));
                files = files.saturating_add(acc_files.get(&kid).copied().unwrap_or(0));
            } else {
                size = size.saturating_add(k.size);
                files += 1;
                push_top(&mut top, &path, k);
            }
        }
        acc_size.insert(*frn, size);
        acc_files.insert(*frn, files);

        on_dir(DirStat {
            path,
            size_bytes: size,
            mtime_unix: entry.mtime_unix,
            files,
        });
        dirs_emitted += 1;
    }

    let files = acc_files.get(&target).copied().unwrap_or(0);
    let bytes = acc_size.get(&target).copied().unwrap_or(0);
    top.sort_by_key(|b| std::cmp::Reverse(b.size_bytes));
    top.truncate(TOP_N);

    ScanTotals {
        files,
        dirs: dirs_emitted,
        bytes,
        read_errors,
        top,
    }
}

fn push_top(top: &mut Vec<LargestFile>, dir_path: &str, entry: &FileRecord) {
    let sep = if dir_path.ends_with('\\') { "" } else { "\\" };
    if top.len() < TOP_N || entry.size > top[top.len() - 1].size_bytes {
        top.push(LargestFile {
            path: format!("{dir_path}{sep}{}", entry.name),
            size_bytes: entry.size,
            mtime_unix: entry.mtime_unix,
        });
        top.sort_by_key(|b| std::cmp::Reverse(b.size_bytes));
        top.truncate(TOP_N);
    }
}

fn read_boot_sector(handle: &VolumeHandle) -> Result<[u8; 512], PlatformError> {
    let mut buf = [0u8; 512];
    read_at(handle.0, 0, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::cast_possible_truncation)]
    use super::*;

    fn le_u64(v: u64) -> [u8; 8] {
        v.to_le_bytes()
    }
    fn le_u32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn le_u16(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }

    /// FILETIME (100 ns desde 1601) equivalente a los segundos UNIX.
    const FILETIME_FOR_UNIX: i64 = (1_650_000_000 + 11_644_473_600) * 10_000_000;

    /// Construye un `FILE_RECORD` sintético con un atributo `$FILE_NAME`.
    fn fake_record(parent: u64, name: &str, is_dir: bool, size: u64, mtime: i64) -> Vec<u8> {
        let name_u16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let value_len = 0x3A + name_u16.len();
        let attr_len = 0x18 + value_len;
        let record_size = 0x40 + attr_len + 4;
        let mut raw = vec![0u8; record_size];

        raw[0..4].copy_from_slice(b"FILE");
        raw[0x14..0x16].copy_from_slice(&le_u16(0x40)); // first attr
        let flags = if is_dir {
            FLAG_IN_USE | FLAG_DIRECTORY
        } else {
            FLAG_IN_USE
        };
        raw[0x16..0x18].copy_from_slice(&le_u16(flags));
        raw[0x18..0x1C].copy_from_slice(&le_u32(record_size as u32)); // used size
        raw[0x20..0x28].copy_from_slice(&le_u64(u64::MAX)); // base = -1 (no extensión)

        let off = 0x40usize;
        raw[off..off + 4].copy_from_slice(&le_u32(ATTR_FILE_NAME));
        raw[off + 4..off + 8].copy_from_slice(&le_u32(attr_len as u32));
        raw[off + 8] = 0; // residente
        raw[off + 0x10..off + 0x14].copy_from_slice(&le_u32(value_len as u32));
        raw[off + 0x14..off + 0x16].copy_from_slice(&le_u16(0x18)); // value offset
        let v = off + 0x18;
        raw[v..v + 8].copy_from_slice(&le_u64(parent));
        raw[v + 0x10..v + 0x18].copy_from_slice(&mtime.to_le_bytes());
        raw[v + 0x28..v + 0x30].copy_from_slice(&le_u64(size));
        raw[v + 0x38] = (name_u16.len() / 2) as u8;
        raw[v + 0x39] = 1; // Win32
        raw[v + 0x3A..v + 0x3A + name_u16.len()].copy_from_slice(&name_u16);
        raw
    }

    #[test]
    fn parses_boot_sector() {
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&le_u16(512));
        boot[0x0D] = 8;
        boot[0x30..0x38].copy_from_slice(&le_u64(4)); // MFT LCN 4
        boot[0x40] = 0xF6; // -10 → 1024 bytes/registro

        let info = parse_boot(&boot).expect("boot NTFS válido");
        assert_eq!(info.cluster_bytes, 4096);
        assert_eq!(info.mft_lcn, 4);
        assert_eq!(info.record_size, 1024);
    }

    #[test]
    fn rejects_non_ntfs_boot() {
        let boot = [0u8; 512];
        assert_eq!(parse_boot(&boot), None);
    }

    #[test]
    fn rejects_corrupt_record_size_shift() {
        fn boot_with(record_byte: u8) -> Vec<u8> {
            let mut boot = vec![0u8; 512];
            boot[3..11].copy_from_slice(b"NTFS    ");
            boot[0x0B..0x0D].copy_from_slice(&le_u16(512));
            boot[0x0D] = 8;
            boot[0x30..0x38].copy_from_slice(&le_u64(4));
            boot[0x40] = record_byte;
            boot
        }
        // 0xE0 = -32 → 1 << 32 ≈ 4 GiB de "registro": se rechaza sin panic.
        assert_eq!(parse_boot(&boot_with(0xE0)), None);
        // 0x80 = -128 → shift 128: el guard evita el panic por overflow.
        assert_eq!(parse_boot(&boot_with(0x80)), None);
        // Tamaño positivo absurdo: se rechaza por el tope de 64 KiB.
        let mut huge = boot_with(0x0A); // raw = 10 clusters/registro
        huge[0x0D] = 244; // 244 sectores/cluster × 512 = 124 928 × 10 > tope
        assert_eq!(parse_boot(&huge), None);
    }

    #[test]
    fn parses_file_record_with_name_and_size() {
        let raw = fake_record(
            NTFS_ROOT_FRN,
            "documentos.txt",
            false,
            1_024_000,
            FILETIME_FOR_UNIX,
        );
        let rec = parse_record(&raw).expect("registro bien formado");
        assert!(!rec.is_dir);
        assert_eq!(rec.parent_frn, NTFS_ROOT_FRN);
        assert_eq!(rec.name, "documentos.txt");
        assert_eq!(rec.size, 1_024_000);
        assert_eq!(rec.mtime_unix, 1_650_000_000);
    }

    #[test]
    fn parses_directory_record() {
        let raw = fake_record(NTFS_ROOT_FRN, "Carpeta", true, 0, 1_650_000_000);
        let rec = parse_record(&raw).expect("directorio");
        assert!(rec.is_dir);
        assert_eq!(rec.name, "Carpeta");
    }

    #[test]
    fn skips_extension_and_free_records() {
        let mut raw = fake_record(NTFS_ROOT_FRN, "libre", false, 10, 0);
        raw[0x16] = 0; // no en uso
        assert_eq!(parse_record(&raw), None);

        let mut raw2 = fake_record(NTFS_ROOT_FRN, "base", false, 10, 0);
        raw2[0x20..0x28].copy_from_slice(&le_u64(42)); // base FRN != -1
        assert_eq!(parse_record(&raw2), None);
    }

    #[test]
    fn prefers_non_dos_name() {
        // DOS (8.3) primero, luego Win32: debe ganar el Win32 largo.
        let mut raw = fake_record(NTFS_ROOT_FRN, "DOCUME~1", false, 100, 0);
        // El record va seguido de 4 bytes de relleno; truncarlo para que el
        // segundo atributo quede justo después del primero (sin hueco).
        let first_len = 0x40 + (0x18 + (0x3A + 16));
        raw.truncate(first_len);
        raw[0x18..0x1C].copy_from_slice(&le_u32(first_len as u32)); // used size

        let name2: Vec<u8> = "documentos.txt"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let value2_len = 0x3A + name2.len();
        let attr2_len = 0x18 + value2_len;
        let base = raw.len();
        raw.resize(base + attr2_len, 0);
        let off = base;
        raw[off..off + 4].copy_from_slice(&le_u32(ATTR_FILE_NAME));
        raw[off + 4..off + 8].copy_from_slice(&le_u32(attr2_len as u32));
        raw[off + 8] = 0;
        raw[off + 0x10..off + 0x14].copy_from_slice(&le_u32(value2_len as u32));
        raw[off + 0x14..off + 0x16].copy_from_slice(&le_u16(0x18));
        let v = off + 0x18;
        raw[v..v + 8].copy_from_slice(&le_u64(NTFS_ROOT_FRN));
        raw[v + 0x28..v + 0x30].copy_from_slice(&le_u64(500));
        raw[v + 0x38] = (name2.len() / 2) as u8;
        raw[v + 0x39] = 1; // Win32
        raw[v + 0x3A..v + 0x3A + name2.len()].copy_from_slice(&name2);
        // Actualizar "used size" del record para que el parser lo recorra.
        let used = u32::try_from(raw.len()).unwrap_or(u32::MAX);
        raw[0x18..0x1C].copy_from_slice(&le_u32(used));

        let rec = parse_record(&raw).expect("record con dos nombres");
        assert_eq!(rec.name, "documentos.txt");
        assert_eq!(rec.size, 500);
    }

    #[test]
    fn parses_runlist() {
        // header 0x22: len=2 bytes, off=2 bytes. len=100, lcn=5000 (0x1388 LE).
        let data = [0x22u8, 100, 0, 0x88, 0x13, 0];
        let runs = parse_runlist(&data).expect("run list válido");
        assert_eq!(runs, vec![(5000, 100)]);
    }

    #[test]
    fn resolves_paths_case_insensitively() {
        let mut entries = HashMap::new();
        entries.insert(
            1u64,
            FileRecord {
                parent_frn: 5,
                name: "Users".into(),
                is_dir: true,
                size: 0,
                mtime_unix: 1,
            },
        );
        entries.insert(
            2u64,
            FileRecord {
                parent_frn: 1,
                name: "Breiner".into(),
                is_dir: false,
                size: 5,
                mtime_unix: 1,
            },
        );
        let mut children = HashMap::new();
        children.insert(5u64, vec![1]);
        children.insert(1u64, vec![2]);

        let resolved = resolve_frn(&["Users".into(), "breiner".into()], &entries, &children)
            .expect("resuelve");
        assert_eq!(resolved, 2);
    }

    fn rec(frn: u64, parent_frn: u64, name: &str, is_dir: bool, size: u64) -> (u64, FileRecord) {
        (
            frn,
            FileRecord {
                parent_frn,
                name: name.to_owned(),
                is_dir,
                size,
                mtime_unix: 0,
            },
        )
    }

    /// Árbol sintético: raíz(5) ← carpeta(12) ← [fileA(21), fileB(22)]
    fn sample_index() -> (HashMap<u64, FileRecord>, HashMap<u64, Vec<u64>>) {
        let entries = [
            rec(5, u64::MAX, "C:", true, 0),
            rec(12, 5, "Usuarios", true, 0),
            rec(21, 12, "a.txt", false, 100),
            rec(22, 12, "b.bin", false, 300),
        ]
        .into_iter()
        .collect();
        let children = HashMap::from([(5u64, vec![12]), (12u64, vec![21, 22])]);
        (entries, children)
    }

    #[test]
    fn accumulate_emits_post_order_with_sizes() {
        let (entries, children) = sample_index();
        let captured = std::cell::RefCell::new(Vec::new());
        let totals = accumulate(
            &[21, 22, 12],
            12,
            "C:\\Usuarios".to_owned(),
            0,
            &entries,
            &children,
            &mut |dir| captured.borrow_mut().push(dir),
        );
        assert_eq!(totals.files, 2);
        assert_eq!(totals.dirs, 1);
        assert_eq!(totals.bytes, 400);
        let dirs = captured.borrow();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, "C:\\Usuarios");
        assert_eq!(dirs[0].files, 2);
        assert_eq!(dirs[0].size_bytes, 400);
        let top_paths: Vec<&str> = totals.top.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(top_paths, ["C:\\Usuarios\\b.bin", "C:\\Usuarios\\a.txt"]);
    }

    #[test]
    fn accumulate_rolls_up_nested_dir_sizes() {
        let (entries, children) = sample_index();
        // Añadir una carpeta anidada: 12 ← 30("Deep") ← 31("c.dat", 50) y
        // colgar 30 bajo la raíz para que el post-orden sea [21,22,31,30,12].
        let mut entries = entries;
        let mut children = children;
        entries.insert(
            30,
            FileRecord {
                parent_frn: 12,
                name: "Deep".into(),
                is_dir: true,
                size: 0,
                mtime_unix: 0,
            },
        );
        entries.insert(
            31,
            FileRecord {
                parent_frn: 30,
                name: "c.dat".into(),
                is_dir: false,
                size: 50,
                mtime_unix: 0,
            },
        );
        children.get_mut(&12).expect("hijos de 12").push(30);
        children.insert(30, vec![31]);

        let captured = std::cell::RefCell::new(Vec::new());
        let totals = accumulate(
            &[21, 22, 31, 30, 12],
            12,
            "C:\\Usuarios".to_owned(),
            0,
            &entries,
            &children,
            &mut |dir| captured.borrow_mut().push(dir),
        );
        assert_eq!(totals.files, 3);
        assert_eq!(totals.bytes, 450);
        let dirs = captured.borrow();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[1].path, "C:\\Usuarios");
        assert_eq!(dirs[1].size_bytes, 450);
    }
}
