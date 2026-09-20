//! Estado compartido de la aplicación gestionado por Tauri.
//!
//! - El store vive bajo [`std::sync::Mutex`] porque `rusqlite::Connection` no
//!   es `Sync`; los comandos de lectura lo bloquean milisegundos y el escaneo
//!   lo mantiene durante toda su escritura.
//! - Los flags de escaneo/cancelación son atómicos porque el hilo del escaneo
//!   y los comandos IPC viven en hilos distintos.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use disky_core::SqliteStore;

/// Estado global de disky.
pub struct AppState {
    /// Store de snapshots (`SQLite` en la carpeta de datos de la app).
    pub store: Mutex<SqliteStore>,
    /// Ruta absoluta de la base de datos (se la pasa al hijo elevado).
    pub db_path: std::path::PathBuf,
    /// `true` mientras hay un escaneo en curso (impone un escaneo a la vez).
    pub scanning: Arc<AtomicBool>,
    /// Señal de cancelación que el walker walker consulta periódicamente.
    pub cancel: Arc<AtomicBool>,
}

impl AppState {
    /// Crea el estado con un store ya abierto y su ruta en disco.
    #[must_use]
    pub fn new(store: SqliteStore, db_path: std::path::PathBuf) -> Self {
        Self {
            store: Mutex::new(store),
            db_path,
            scanning: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
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
