//! Lanzador de procesos elevados (opción A: UAC por escaneo).
//!
//! `ShellExecuteExW` con el verbo `runas` muestra el prompt UAC y arranca el
//! proceso como administrador. El shell de disky se relanza a sí mismo con el
//! flag `--elevated-scan`; el hijo escribe el resultado en un archivo JSON y
//! el padre lo lee al terminar.
//!
//! Convención de éxito: `hInstApp > 32` (documentado en `ShellExecuteExW`);
//! con el usuario cancelando el UAC, `GetLastError` devuelve
//! `ERROR_CANCELLED` (1223).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

/// Timeout de espera del hijo: 30 minutos (un escaneo muy lento de un HDD).
/// Formato en milisegundos porque así lo pide `WaitForSingleObject`.
const CHILD_TIMEOUT_MILLIS: u32 = 30 * 60 * 1000;

/// Cada cuánto se despierta la espera para llamar a `on_tick` (y volver a
/// comprobar el deadline). 400 ms es la granularidad del progreso por unidad.
const CHILD_TICK_MILLIS: u32 = 400;

/// Código Win32 de `ERROR_CANCELLED`.
const ERROR_CANCELLED_CODE: u32 = 1223;

/// Motivos por los que el flujo elevado no pudo completarse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElevateError {
    /// El usuario canceló el prompt UAC (o una política lo bloqueó).
    #[error("el usuario canceló el UAC o una política bloqueó la elevación")]
    Cancelled,
    /// El hijo elevado terminó con código distinto de cero.
    #[error("el proceso elevado falló con código de salida {0}")]
    ChildFailed(i32),
    /// El hijo tardó más del tiempo límite o la espera falló.
    #[error("el proceso elevado no terminó a tiempo o no se pudo esperar")]
    Abandoned,
    /// Error de Win32 no clasificado.
    #[error("error de Windows {0}")]
    Windows(u32),
}

/// Lanza `exe` elevado con los argumentos dados y espera su salida.
///
/// `on_tick` se invoca cada ~400 ms mientras el hijo corre: es lo que permite
/// al padre leer resultados parciales (JSONL por unidad) sin perder el timeout.
///
/// # Errors
/// [`ElevateError`] si el UAC se cancela, el hijo falla o se pasa de tiempo.
pub fn run_elevated(exe: &Path, args: &str, on_tick: &mut dyn FnMut()) -> Result<(), ElevateError> {
    // Los buffers deben vivir mientras ShellExecuteExW lee los punteros.
    let exe_wide = to_wide(exe.as_os_str());
    let verb_wide = to_wide(OsStr::new("runas"));
    let args_wide = to_wide(OsStr::new(args));

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or_default(),
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(exe_wide.as_ptr()),
        lpParameters: PCWSTR(args_wide.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };

    // SAFETY: todos los punteros apuntan a buffers vivos en esta función y el
    // struct está completamente inicializado. La llamada no retiene nada.
    let result = unsafe { ShellExecuteExW(&raw mut sei) };

    if result.is_err() || (sei.hInstApp.0 as usize) <= 32 {
        let code = unsafe { GetLastError() }.0;
        return Err(match code {
            ERROR_CANCELLED_CODE => ElevateError::Cancelled,
            _ => ElevateError::Windows(code),
        });
    }

    if sei.hProcess.is_invalid() {
        return Err(ElevateError::Abandoned);
    }

    // Espera en ticks: el callback lee el progreso del hijo, y el deadline
    // conserva el timeout de 30 minutos del contrato original.
    let deadline = Instant::now() + Duration::from_millis(u64::from(CHILD_TIMEOUT_MILLIS));
    let abandoned = |h| {
        // No dejar huérfano al hijo elevado: seguiría escaneando el disco
        // (y escribiendo la BD) sin que nadie lo espere. Se le mata.
        let _ = unsafe { TerminateProcess(h, 1) };
        let _ = unsafe { CloseHandle(h) };
        ElevateError::Abandoned
    };
    loop {
        let waited = unsafe { WaitForSingleObject(sei.hProcess, CHILD_TICK_MILLIS) };
        if waited == WAIT_OBJECT_0 {
            break;
        }
        if waited != WAIT_TIMEOUT || Instant::now() >= deadline {
            return Err(abandoned(sei.hProcess));
        }
        on_tick();
    }

    let mut exit_code: u32 = 0;
    let exit = unsafe { GetExitCodeProcess(sei.hProcess, &raw mut exit_code) };
    let _ = unsafe { CloseHandle(sei.hProcess) };
    exit.map_err(|_| ElevateError::Abandoned)?;

    if exit_code != 0 {
        return Err(ElevateError::ChildFailed(
            i32::try_from(exit_code).unwrap_or(-1),
        ));
    }
    Ok(())
}

/// Codifica a UTF-16 con terminador nulo (`OsStrExt::encode_wide`).
fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}
