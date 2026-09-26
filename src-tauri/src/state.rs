//! Estado compartido de la aplicación gestionado por Tauri.
//!
//! - El store vive bajo [`std::sync::Mutex`] porque `rusqlite::Connection` no
//!   es `Sync`; los comandos de lectura lo bloquean milisegundos y el escaneo
//!   lo mantiene durante toda su escritura.
//! - Los flags de escaneo/cancelación son atómicos porque el hilo del escaneo
//!   y los comandos IPC viven en hilos distintos.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use disky_core::SqliteStore;

/// Estado global de disky.
pub struct AppState {
    /// Store de snapshots (`SQLite` en la carpeta de datos de la app).
    pub store: Mutex<SqliteStore>,
    /// Ruta absoluta de la base de datos (se la pasa al hijo elevado).
    pub db_path: PathBuf,
    /// `true` mientras hay un escaneo en curso (impone un escaneo a la vez).
    pub scanning: Arc<AtomicBool>,
    /// Señal de cancelación que el walker consulta periódicamente.
    pub cancel: Arc<AtomicBool>,
    /// Archivo centinela del escaneo elevado en curso, si lo hay.
    ///
    /// Un proceso no puede matar a un hijo elevado (`TerminateProcess` da
    /// access denied), así que "Cancelar" se signal **creando este archivo** y
    /// el hijo lo sondea. `None` = el escaneo actual corre en este proceso y
    /// ya lee el flag atómico de arriba.
    pub elevated_sentinel: Mutex<Option<PathBuf>>,
}

impl AppState {
    /// Crea el estado con un store ya abierto y su ruta en disco.
    #[must_use]
    pub fn new(store: SqliteStore, db_path: PathBuf) -> Self {
        Self {
            store: Mutex::new(store),
            db_path,
            scanning: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            elevated_sentinel: Mutex::new(None),
        }
    }

    /// Señala cancelación al escaneo elevado en curso, si lo hay, y devuelve si
    /// había uno. El padre no puede terminar al hijo (admin), solo escribir.
    pub fn cancel_elevated(&self) -> bool {
        let Ok(guard) = self.elevated_sentinel.lock() else {
            return false;
        };
        let Some(path) = guard.as_ref() else {
            return false;
        };
        std::fs::write(path, b"1").is_ok()
    }

    /// Registra (o limpia) el centinela del escaneo elevado en curso.
    pub fn set_elevated_sentinel(&self, path: Option<PathBuf>) {
        match self.elevated_sentinel.lock() {
            Ok(mut guard) => *guard = path,
            Err(poisoned) => *poisoned.into_inner() = path,
        }
    }
}

/// Bloquea el store recuperándose de un pánico previo (poisoned mutex).
///
/// Si otro hilo paniqueó mientras escribía, la transacción de `SQLite` ya hizo
/// rollback al soltarse: continuar es seguro.
pub fn lock_store(store: &Mutex<SqliteStore>) -> MutexGuard<'_, SqliteStore> {
    store.lock().unwrap_or_else(PoisonError::into_inner)
}
