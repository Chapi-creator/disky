//! Adaptadores de plataforma: enumerar volúmenes reales y leer el USN Journal.
//!
//! La firma pública es portable; detrás hay una implementación Windows
//! (`windows` crate, user-mode, sin admin) y una banda neutral para tests/CI.

use crate::domain::{DriveKind, Volume};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub mod elevate;
pub mod mft;
pub mod path_norm;
pub mod sqlite;
pub mod usn;
pub mod walk;

/// Errores del adaptador de plataforma.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// Windows devolvió un error al consultar la unidad.
    #[error("error de Windows consultando `{letter}`: {code}")]
    WindowsApi {
        /// Letra de la unidad consultada.
        letter: String,
        /// Código de error Win32 (la palabra baja del HRESULT).
        code: u32,
    },
    /// La letra recibida no es una unidad válida (una letra ASCII A–Z).
    #[error("letra de unidad inválida: `{0}`")]
    InvalidDriveLetter(String),
    /// La plataforma actual no tiene adaptador implementado.
    #[error("adaptador de plataforma no disponible en este sistema operativo")]
    UnsupportedPlatform,
}

/// Valida una letra de unidad (`"c"`, `"D"`, ...) y la devuelve en mayúscula.
///
/// # Errors
/// [`PlatformError::InvalidDriveLetter`] si no es exactamente una letra ASCII.
pub fn drive_letter(letter: &str) -> Result<char, PlatformError> {
    let mut chars = letter.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Ok(c.to_ascii_uppercase());
        }
    }
    Err(PlatformError::InvalidDriveLetter(letter.to_owned()))
}

/// Handle de volumen (`\\.\C:`) con cierre automático (RAII).
///
/// Compartido por [`mft`](mft/index.html) y [`usn`](usn/index.html): abrir con
/// `GENERIC_READ` exige elevación, y los FSCTL exigen ese acceso.
pub(crate) struct VolumeHandle(pub(crate) HANDLE);

impl VolumeHandle {
    /// Abre el volumen con acceso de lectura.
    ///
    /// # Errors
    /// [`PlatformError::WindowsApi`] si Windows rechaza la apertura (p. ej.
    /// error 5 sin elevación).
    pub(crate) fn open(letter: char) -> Result<Self, PlatformError> {
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
        // Best-effort: si falla, el SO cierra el handle al morir el proceso.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Traduce el código de un `windows::core::Error` (HRESULT) al número Win32.
/// Los HRESULT de error Win32 son `0x8007xxxx`; `xxxx` es el código original.
#[allow(clippy::cast_sign_loss)] // la palabra baja de un HRESULT de error es >= 0
pub(crate) fn win32_code(err: &windows::core::Error) -> u32 {
    (err.code().0 & 0xFFFF) as u32
}

/// Enumera los volúmenes montados con letra de unidad (`C:`, `D:`, ...).
///
/// Solo lectura: nunca bloquea ni modifica nada. Las unidades extraíbles/
/// ópticas sin medio se reportan con `total_bytes == 0` en vez de fallar,
/// porque son un estado normal, no un error.
///
/// # Errors
/// Devuelve [`PlatformError`] si la consulta al sistema falla.
pub fn list_volumes() -> Result<Vec<Volume>, PlatformError> {
    imp::list_volumes()
}

/// Raíces (`C:\`) de las unidades fijas que tienen medio, en orden de letra.
///
/// Filtro **compartido** por el padre y el hijo elevado del escaneo de todas las
/// unidades: si cada uno enumerara por su cuenta, un USB enchufado entre el UAC
/// y el arranque del hijo haría que ambos vieran unidades distintas.
///
/// # Errors
/// Devuelve [`PlatformError`] si la enumeración falla.
pub fn fixed_volume_roots() -> Result<Vec<String>, PlatformError> {
    Ok(list_volumes()?
        .into_iter()
        .filter(|v| matches!(v.kind, DriveKind::Fixed) && v.total_bytes > 0)
        .map(|v| format!("{}\\", v.letter))
        .collect())
}

#[cfg(windows)]
mod imp {
    //! Implementación Windows sobre `GetLogicalDrives` y `GetDiskFreeSpaceExW`.
    //! Todo user-mode, sin permisos especiales.

    use super::PlatformError;
    use crate::domain::{DriveKind, Volume};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };

    pub fn list_volumes() -> Result<Vec<Volume>, PlatformError> {
        let bitmask = unsafe { GetLogicalDrives() };
        if bitmask == 0 {
            return Err(PlatformError::WindowsApi {
                letter: "<logical drives>".to_owned(),
                code: unsafe { windows::Win32::Foundation::GetLastError() }.0,
            });
        }

        let mut volumes = Vec::new();
        for (_, letter) in (0u32..26)
            .zip(b'A'..=b'Z')
            .filter(|&(bit, _)| bitmask & (1 << bit) != 0)
        {
            let drive = format!("{}:\\", letter as char);
            let wide: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();

            let kind = drive_kind(unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) });

            let mut total_bytes = 0u64;
            let mut free_bytes = 0u64;
            let has_medium = unsafe {
                GetDiskFreeSpaceExW(
                    PCWSTR(wide.as_ptr()),
                    None,
                    Some(&raw mut total_bytes),
                    Some(&raw mut free_bytes),
                )
            }
            .is_ok();

            volumes.push(Volume {
                letter: format!("{}:", letter as char),
                label: None,
                // Unidades sin medio (bandeja vacía, lector de SD sin tarjeta):
                // estado normal, no error → se reportan vacías.
                total_bytes: if has_medium { total_bytes } else { 0 },
                free_bytes: if has_medium { free_bytes } else { 0 },
                kind,
            });
        }
        Ok(volumes)
    }

    const fn drive_kind(raw: u32) -> DriveKind {
        match raw {
            // Constantes de `GetDriveTypeW` (winbase.h), inline para no arrastrar
            // el feature completo solo por 6 números.
            3 => DriveKind::Fixed,
            2 => DriveKind::Removable,
            4 => DriveKind::Remote,
            5 => DriveKind::CdRom,
            6 => DriveKind::RamDisk,
            _ => DriveKind::Unknown,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! Banda neutral para CI en Linux/macOS: sin datos reales, sin panics.

    use super::PlatformError;
    use crate::domain::{DriveKind, Volume};

    pub fn list_volumes() -> Result<Vec<Volume>, PlatformError> {
        Err(PlatformError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_ascii_letters() {
        assert_eq!(drive_letter("c"), Ok('C'));
        assert_eq!(drive_letter("D"), Ok('D'));
    }

    #[test]
    fn rejects_invalid_drive_letters() {
        assert!(drive_letter("").is_err());
        assert!(drive_letter("CC").is_err());
        assert!(drive_letter("1").is_err());
        assert!(drive_letter("ñ").is_err());
        assert!(drive_letter("C:\\").is_err());
    }
}
