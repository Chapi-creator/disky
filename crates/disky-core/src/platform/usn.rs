//! Adaptador Windows para leer la MFT y el Change Journal de NTFS.
//!
//! **Hallazgo empírico del spike** (verificado en Windows 10 con usuario
//! estándar y con el comportamiento de Everything como referencia):
//!
//! - Abrir `\\.\C:` con acceso cero **sí funciona**, pero NTFS rechaza
//!   `FSCTL_ENUM_USN_DATA`, `FSCTL_QUERY_USN_JOURNAL` y
//!   `FSCTL_READ_USN_JOURNAL` con error 1 (`INVALID_FUNCTION`).
//! - Con `GENERIC_READ` el handle requiere **proceso elevado** (error 5
//!   `ACCESS_DENIED` si no). Everything exige lo mismo: servicio o UAC.
//! - Consecuencia de producto: el escaneo corre elevado (prompt UAC o
//!   agente estilo Frostbyte), y/o un fallback por recorrido de directorios
//!   sin admin. El core es agnóstico: solo implementa los FSCTL.
//!
//! Todos los buffers se empaquetan/parsean con `to_le_bytes`/`from_le_bytes`
//! (sin transmutes) para que la única parte `unsafe` sea la llamada Win32.

use super::PlatformError;
use crate::domain::usn::{parse_usn_batch, parse_usn_records, JournalRecord};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};
use windows::Win32::System::IO::DeviceIoControl;

/// Tamaño del struct `USN_JOURNAL_DATA_V0` en bytes (6 campos de 8).
const USN_JOURNAL_DATA_V0_LEN: usize = 48;

/// Cuánto retrocedemos desde el final del journal al pedir el lote de
/// registros recientes (1 MiB ≈ varios miles de registros).
const READ_BACK_BYTES: i64 = 1024 * 1024;

/// Buffer de salida para `FSCTL_READ_USN_JOURNAL` (256 KiB por lote).
const READ_BUFFER_BYTES: usize = 256 * 1024;

/// Buffer para la sonda de `FSCTL_ENUM_USN_DATA` (64 KiB por lote).
const ENUM_BUFFER_BYTES: usize = 64 * 1024;

/// Tamaño del struct `MFT_ENUM_DATA_V1` en bytes (FRN + USNs + versiones + flags).
const MFT_ENUM_DATA_V1_LEN: usize = 32;

/// `MFT_ENUM_FLAG_ALLOW_ZERO_ACCESS | MFT_ENUM_FLAG_VISIBLE` (winioctl.h).
/// El flag de acceso cero es el que habilita el escaneo sin admin (Win10 1703+);
/// `VISIBLE` (= `ENUM | EMPTY | TERMS`) filtra a los archivos visibles normales.
const MFT_ENUM_FLAGS_ZERO_ACCESS: u32 = 0x0000_00E1;

/// Estado del USN Journal de un volumen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[must_use]
pub struct UsnStatus {
    /// ID del journal (persistente mientras el journal exista).
    pub journal_id: u64,
    /// Siguiente USN a escribir: el "final" actual del journal.
    pub next_usn: i64,
    /// Primer USN aún disponible en el journal (lo anterior se recicló).
    pub first_usn: i64,
    /// USN máximo alcanzable antes de que el journal se recicle.
    pub max_usn: i64,
    /// Tamaño máximo configurado del journal, en bytes.
    pub max_size: u64,
}

/// Handle de volumen con cierre automático (RAII).
struct VolumeHandle(HANDLE);

impl VolumeHandle {
    /// Abre el volumen con acceso cero (ver docs del módulo).
    ///
    /// # Errors
    /// [`PlatformError::WindowsApi`] si Windows rechaza la apertura.
    fn open(letter: char) -> Result<Self, PlatformError> {
        let device = format!(r"\\.\{letter}:");
        let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0, // los FSCTL exigen acceso de lectura al volumen
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None, // sin template
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
        // Best-effort: si falla, el SO cierra el handle al morir el proceso.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Traduce el código de un `windows::core::Error` (HRESULT) al número Win32.
/// Los HRESULT de error Win32 son `0x8007xxxx`; `xxxx` es el código original.
#[allow(clippy::cast_sign_loss)] // la palabra baja de un HRESULT de error es >= 0
fn win32_code(err: &windows::core::Error) -> u32 {
    (err.code().0 & 0xFFFF) as u32
}

/// Convierte el tamaño de un buffer pequeño a `u32` para la API Win32.
/// Los buffers de este módulo son ≤ 256 KiB, así que nunca trunca.
#[allow(clippy::cast_possible_truncation)]
fn len_u32(bytes_len: usize) -> u32 {
    bytes_len as u32
}

/// Lee un `u64` little-endian de un slice de exactamente 8 bytes.
/// Los llamadores usan rangos literales, así que el panic es inalcanzable.
fn le_u64(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    u64::from_le_bytes(arr)
}

/// Lee un `i64` little-endian de un slice de exactamente 8 bytes.
fn le_i64(bytes: &[u8]) -> i64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    i64::from_le_bytes(arr)
}

/// Empaqueta `READ_USN_JOURNAL_DATA_V1` como bytes (layout de `winioctl.h`).
///
/// Solo pedimos registros V2 (`Min/MaxMajorVersion = 2`), que es lo que el
/// parser del dominio entiende; los volúmenes con FRN de 128 bits (V3) son
/// raros y quedan fuera del spike.
fn pack_read_request(journal_id: u64, start_usn: i64) -> [u8; 48] {
    let mut buf = [0u8; 48];
    buf[0..8].copy_from_slice(&start_usn.to_le_bytes()); // StartUsn
    buf[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // ReasonMask: todos
    buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // ReturnOnlyOnClose: 0
    buf[16..24].copy_from_slice(&0u64.to_le_bytes()); // Timeout: 0 (no espera)
    buf[24..32].copy_from_slice(&0u64.to_le_bytes()); // BytesToWaitFor: 0 (ya)
    buf[32..40].copy_from_slice(&journal_id.to_le_bytes()); // UsnJournalID
    buf[40..42].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
    buf[42..44].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion
    buf
}

/// Ejecuta `FSCTL_QUERY_USN_JOURNAL` sobre un handle ya abierto.
fn query_status(handle: &VolumeHandle, letter: char) -> Result<UsnStatus, PlatformError> {
    // Buffer alineado a 8: el struct del kernel contiene u64.
    #[repr(C, align(8))]
    struct Aligned([u8; USN_JOURNAL_DATA_V0_LEN]);
    let mut out = Aligned([0; USN_JOURNAL_DATA_V0_LEN]);

    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(out.0.as_mut_ptr().cast()),
            len_u32(USN_JOURNAL_DATA_V0_LEN),
            None,
            None,
        )
    }
    .map_err(|e| PlatformError::WindowsApi {
        letter: letter.to_string(),
        code: win32_code(&e),
    })?;

    Ok(UsnStatus {
        journal_id: le_u64(&out.0[0..8]),
        next_usn: le_i64(&out.0[8..16]),
        first_usn: le_i64(&out.0[16..24]),
        max_usn: le_i64(&out.0[24..32]),
        max_size: le_u64(&out.0[32..40]),
    })
}

/// Consulta el estado del USN Journal de una unidad (`"C"`, `"d"`, ...).
///
/// # Errors
/// [`PlatformError::WindowsApi`] si la unidad no existe o no tiene journal
/// (p. ej. FAT32/exFAT no lo soportan: Windows devuelve error 1).
pub fn journal_status(letter: &str) -> Result<UsnStatus, PlatformError> {
    let letter = super::drive_letter(letter)?;
    let handle = VolumeHandle::open(letter)?;
    query_status(&handle, letter)
}

/// Lee un lote de los registros más recientes del journal de una unidad.
///
/// Empieza hasta 1 MiB antes del final del journal (o desde `first_usn` si el
/// journal es más pequeño) y devuelve como máximo `max_records`, los más
/// nuevos al final. Útil para verificar el pipeline antes de construir el
/// watcher incremental.
///
/// # Errors
/// [`PlatformError::WindowsApi`] si la unidad no existe o no tiene journal.
pub fn recent_records(
    letter: &str,
    max_records: usize,
) -> Result<Vec<JournalRecord>, PlatformError> {
    let letter = super::drive_letter(letter)?;
    let handle = VolumeHandle::open(letter)?;
    let status = query_status(&handle, letter)?;

    // Nunca pedir por debajo de first_usn: el kernel rechazaría la lectura.
    let start_usn = status.first_usn.max(status.next_usn - READ_BACK_BYTES);
    let input = pack_read_request(status.journal_id, start_usn);
    let mut out = vec![0u8; READ_BUFFER_BYTES];
    let mut returned = 0u32;

    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_READ_USN_JOURNAL,
            Some(input.as_ptr().cast()),
            len_u32(input.len()),
            Some(out.as_mut_ptr().cast()),
            len_u32(out.len()),
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(|e| PlatformError::WindowsApi {
        letter: letter.to_string(),
        code: win32_code(&e),
    })?;

    // Solo los bytes que el kernel realmente escribió; el resto es relleno.
    let written = (returned as usize).min(out.len());
    let (_next_usn, mut records) = parse_usn_batch(&out[..written]);
    if records.len() > max_records {
        let drop_count = records.len() - max_records;
        records.drain(..drop_count);
    }
    Ok(records)
}

/// Empaqueta `MFT_ENUM_DATA_V1` (layout de `winioctl.h`) para el escaneo
/// inicial: desde el FRN 0, sin filtros de USN, versiones de registro 2 y
/// flags de acceso cero + visibles.
fn pack_mft_enum_request() -> [u8; MFT_ENUM_DATA_V1_LEN] {
    let mut buf = [0u8; MFT_ENUM_DATA_V1_LEN];
    // StartFileReferenceNumber(0..8)=0, LowUsn(8..16)=0, HighUsn(16..24)=0
    buf[24..26].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
    buf[26..28].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion
    buf[28..32].copy_from_slice(&MFT_ENUM_FLAGS_ZERO_ACCESS.to_le_bytes()); // Flags
    buf
}

/// Sonda del escaneo MFT: una llamada a `FSCTL_ENUM_USN_DATA` con acceso cero.
///
/// Lee el primer lote de la MFT de la unidad y devuelve cuántos registros
/// válidos trajo. Es el fundamento del escaneo inicial de disky: si esto
/// funciona sin admin, el producto completo puede correr sin admin.
///
/// # Errors
/// [`PlatformError::WindowsApi`] si la unidad no existe o el kernel rechaza
/// la operación (p. ej. unidad no NTFS).
pub fn mft_probe(letter: &str) -> Result<usize, PlatformError> {
    let letter = super::drive_letter(letter)?;
    let handle = VolumeHandle::open(letter)?;

    // `MFT_ENUM_DATA_V1` con `ALLOW_ZERO_ACCESS`: sin esto, el kernel rechaza
    // el FSCTL con ERROR_INVALID_FUNCTION (hallazgo del spike).
    let input = pack_mft_enum_request();
    let mut out = vec![0u8; ENUM_BUFFER_BYTES];
    let mut returned = 0u32;

    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_ENUM_USN_DATA,
            Some(input.as_ptr().cast()),
            len_u32(input.len()),
            Some(out.as_mut_ptr().cast()),
            len_u32(out.len()),
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(|e| PlatformError::WindowsApi {
        letter: letter.to_string(),
        code: win32_code(&e),
    })?;

    let written = (returned as usize).min(out.len());
    if written < 8 {
        return Ok(0);
    }
    // Los primeros 8 bytes son el `StartFileReferenceNumber` para la
    // siguiente llamada; después vienen registros USN V2 contiguos.
    Ok(parse_usn_records(&out[8..written]).len())
}

#[cfg(test)]
mod tests {
    // `panic!` aquí es el mecanismo de reporte del diagnóstico: imprime el
    // resumen completo de variantes probadas.
    #![allow(clippy::expect_used, clippy::format_push_string, clippy::panic)]

    use super::*;

    /// Prueba de integración real (solo Windows, solo lectura). Requiere un
    /// proceso **elevado**: sin admin, el kernel rechaza el FSCTL (error 1).
    /// En CI (`windows-latest`, elevado) corre con `--include-ignored`.
    #[test]
    #[ignore = "requiere proceso elevado: FSCTL_ENUM_USN_DATA exige GENERIC_READ al volumen"]
    fn enumerates_mft_from_system_drive() {
        let count = mft_probe("C").expect("C: es NTFS y ENUM_USN_DATA debe funcionar");
        assert!(count > 0, "el primer lote de la MFT debe traer registros");
    }

    /// Diagnóstico temporal: prueba variantes de apertura y reporta el
    /// resultado de cada una en el panic (correr con --nocapture).
    #[test]
    #[ignore = "solo diagnóstico: cargo test -p disky-core diagnostic -- --ignored --nocapture"]
    fn diagnostic() {
        let mut report = String::from("\n=== DIAGNÓSTICO FSCTL ===\n");

        // ¿El proceso corre elevado? GENERIC_READ al volumen lo revela.
        let elevated = try_enum('C', true);
        report.push_str(&format!("ENUM con GENERIC_READ (admin?): {elevated:?}\n"));

        // Acceso cero: el camino sin admin que Everything usa.
        let zero = try_enum('C', false);
        report.push_str(&format!("ENUM con acceso cero: {zero:?}\n"));

        // QUERY con acceso cero, para confirmar lo visto antes.
        let query = journal_status("C")
            .map(|_| "ok".to_owned())
            .map_err(|e| e.to_string());
        report.push_str(&format!("QUERY con acceso cero: {query:?}\n"));

        panic!("{report}");
    }

    /// Abre `C:` con el acceso indicado y hace UNA llamada a `FSCTL_ENUM_USN_DATA`.
    /// `generic_read = true` usa `GENERIC_READ` (solo funciona elevado).
    fn try_enum(letter: char, generic_read: bool) -> Result<usize, String> {
        let device = format!(r"\\.\{letter}:");
        let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let desired_access = if generic_read { 0x8000_0000u32 } else { 0u32 };

        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| format!("CreateFile: código {:#x}", e.code().0))?;

        let input = pack_mft_enum_request();
        let mut out = vec![0u8; ENUM_BUFFER_BYTES];
        let mut returned = 0u32;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(input.as_ptr().cast()),
                len_u32(input.len()),
                Some(out.as_mut_ptr().cast()),
                len_u32(out.len()),
                Some(&raw mut returned),
                None,
            )
        }
        .map(|()| {
            let written = (returned as usize).min(out.len());
            parse_usn_records(&out[8..written]).len()
        })
        .map_err(|e| format!("DeviceIoControl: código {:#x}", e.code().0));

        let _ = unsafe { CloseHandle(handle) };
        result
    }

    /// Documenta la limitación hallada en el spike: los FSCTL del journal
    /// exigen `GENERIC_READ` al volumen, que a su vez exige elevación.
    #[test]
    #[ignore = "requiere proceso elevado: FSCTL_QUERY_USN_JOURNAL exige GENERIC_READ al volumen"]
    fn reads_journal_from_system_drive() {
        let status = journal_status("C").expect("C: debería tener USN Journal (NTFS)");
        assert_ne!(status.journal_id, 0, "el journal debe tener un ID válido");
        assert!(status.next_usn > 0, "NextUsn debe ser positivo");
        assert!(status.max_size > 0, "el journal debe tener tamaño máximo");
    }
}
