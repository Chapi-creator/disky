//! Shell de Tauri para disky.
//!
//! Este binario es un adaptador delgado: traduce comandos IPC del frontend a
//! llamadas de [`disky_core`] y nada más. Toda la lógica vive en el core, que
//! es testeable sin UI y reutilizable por un CLI futuro.

/// Comandos IPC expuestos al frontend.
mod commands;

/// Estado compartido de la app (store SQLite + flags de escaneo).
mod state;

use disky_core::SnapshotStore as _;
use tauri::Manager;

/// Resultado JSON que escribe el hijo elevado tras su escaneo.
#[derive(serde::Serialize)]
struct ElevatedResult {
    ok: bool,
    snapshot_id: Option<u64>,
    total_files: u64,
    total_bytes: u64,
    read_errors: u64,
    duration_ms: i64,
    error: Option<String>,
}

/// Modo hijo elevado: escanea `root`, guarda el snapshot en la BD indicada y
/// escribe el resultado en `out`. Devuelve el código de salida del proceso.
///
/// El snapshot es atómico (transacción del store): si algo falla a mitad, no
/// queda rastro. `ok: false` también se serializa para que el padre muestre
/// el error en la UI.
#[must_use]
#[allow(clippy::expect_used)]
pub fn elevated_scan(root: &str, out: &str, db: &str) -> i32 {
    let started = std::time::Instant::now();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or_default())
        .unwrap_or_default();

    let fail = |error: String| {
        write_result(
            out,
            &ElevatedResult {
                ok: false,
                snapshot_id: None,
                total_files: 0,
                total_bytes: 0,
                read_errors: 0,
                duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                error: Some(error),
            },
        );
        2
    };

    let root_path = std::path::PathBuf::from(root);
    if !root_path.is_dir() {
        return fail(format!("La ruta no existe o no es un directorio: `{root}`"));
    }

    let Ok(mut store) = disky_core::SqliteStore::open(std::path::Path::new(db)) else {
        return fail("No se pudo abrir la base de datos de snapshots".into());
    };
    let Ok(mut writer) = store.open_writer(root, started_at) else {
        return fail("No se pudo iniciar el snapshot".into());
    };

    let cancel = std::sync::atomic::AtomicBool::new(false);
    let Ok(totals) = disky_core::walk_tree(
        &root_path,
        &cancel,
        &mut |dir| {
            let _ = writer.write_dirs(std::slice::from_ref(&dir));
        },
        &mut |_| {},
    ) else {
        return fail("El escaneo elevado no pudo completarse".into());
    };

    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    match writer.finish(totals, duration_ms) {
        Ok(id) => {
            write_result(
                out,
                &ElevatedResult {
                    ok: true,
                    snapshot_id: Some(id),
                    total_files: totals.files,
                    total_bytes: totals.bytes,
                    read_errors: totals.read_errors,
                    duration_ms,
                    error: None,
                },
            );
            0
        }
        Err(err) => fail(err.to_string()),
    }
}

/// Serializa el resultado del hijo (mejor esfuerzo: si falla, el padre verá
/// un archivo vacío y lo reportará).
fn write_result(out: &str, result: &ElevatedResult) {
    if let Ok(json) = serde_json::to_string(result) {
        let _ = std::fs::write(out, json);
    }
}

/// Arranca la aplicación Tauri (punto de entrada real, llamado desde `main`).
///
/// # Panics
/// Si Tauri no puede inicializarse (contexto inválido, fallo de ventana) no hay
/// UI de la cual recuperarse, así que abortar es el comportamiento correcto.
/// Es el único `expect` del proyecto, permitido explícitamente aquí.
#[allow(clippy::expect_used)]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // La base de datos vive en la carpeta de datos de la app:
            // %APPDATA%/com.breiner.disky/snapshots.db
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("snapshots.db");
            let store = disky_core::SqliteStore::open(&db_path)?;
            app.manage(state::AppState::new(store, db_path));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::list_volumes,
            commands::usn_status,
            commands::usn_recent,
            commands::scan_start,
            commands::scan_cancel,
            commands::scan_quick_start,
            commands::snapshots_list,
            commands::growth_report,
            commands::treemap_nodes,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
