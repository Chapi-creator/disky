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

/// Traza temporal de diagnóstico del hijo elevado (ver dónde se atasca).
#[cfg(debug_assertions)]
fn trace_elevated(msg: &str) {
    eprintln!("[elev] {msg}");
}
#[cfg(not(debug_assertions))]
fn trace_elevated(_msg: &str) {}

/// Modo hijo elevado: escanea `root`, guarda el snapshot en la BD indicada y
/// escribe el resultado en `out`. Devuelve el código de salida del proceso.
///
/// El snapshot es atómico (transacción del store): si algo falla a mitad, no
/// queda rastro. `ok: false` también se serializa para que el padre muestre
/// el error en la UI.
#[must_use]
#[allow(clippy::expect_used)]
pub fn elevated_scan(root: &str, out: &str, db: &str) -> i32 {
    trace_elevated("arranca hijo");
    let started_at = unix_now();
    let cancel = std::sync::atomic::AtomicBool::new(false);

    let mut store = match open_store(db) {
        Ok(store) => store,
        Err(error) => return write_single_fail(out, &error),
    };
    match scan_one_volume(root, &mut store, started_at, &cancel) {
        Ok(volume) => {
            write_result(
                out,
                &ElevatedResult {
                    ok: true,
                    snapshot_id: Some(volume.snapshot_id),
                    total_files: volume.files,
                    total_bytes: volume.bytes,
                    read_errors: volume.read_errors,
                    duration_ms: volume.duration_ms,
                    error: None,
                },
            );
            0
        }
        Err(error) => write_single_fail(out, &error),
    }
}

/// Escribe el fallo del modo de una unidad y devuelve su código de salida.
fn write_single_fail(out: &str, error: &str) -> i32 {
    write_result(
        out,
        &ElevatedResult {
            ok: false,
            snapshot_id: None,
            total_files: 0,
            total_bytes: 0,
            read_errors: 0,
            duration_ms: 0,
            error: Some(error.to_owned()),
        },
    );
    2
}

/// Modo hijo elevado de **todas las unidades fijas** con un solo UAC.
///
/// Recorre las unidades en orden y, al terminar cada una, **añade una línea
/// JSONL** a `out` para que el padre refresque la UI unidad a unidad sin esperar
/// a que el lote termine. El padre ya conoce la lista (misma función
/// `fixed_volume_roots`), así que la línea solo dice qué unidad terminó y si
/// falló: el resto se lee de la base de datos.
///
/// Devuelve 0 si todas las unidades se procesaron (con o sin errores
/// recuperables) y 2 si la base de datos o la enumeración fallaron.
#[must_use]
pub fn elevated_scan_all(out: &str, db: &str) -> i32 {
    trace_elevated("arranca hijo multiunidad");
    let started_at = unix_now();

    let mut store = match open_store(db) {
        Ok(store) => store,
        Err(error) => {
            // Sin store no hay dónde guardar snapshots: se escribe una sola
            // línea para que el padre muestre el motivo y no se cuelgue.
            append_unit(
                out,
                &UnitResult {
                    letter: String::new(),
                    error: Some(error),
                },
            );
            return 2;
        }
    };
    let roots = match disky_core::fixed_volume_roots() {
        Ok(roots) => roots,
        Err(error) => {
            append_unit(
                out,
                &UnitResult {
                    letter: String::new(),
                    error: Some(error.to_string()),
                },
            );
            return 2;
        }
    };

    let cancel = std::sync::atomic::AtomicBool::new(false);
    for root in roots {
        let unit = match scan_one_volume(&root, &mut store, started_at, &cancel) {
            Ok(volume) => {
                trace_elevated(&format!(
                    "{} ok en {} ms",
                    volume.snapshot_id, volume.duration_ms
                ));
                UnitResult {
                    letter: root,
                    error: None,
                }
            }
            Err(error) => {
                trace_elevated(&format!("{root} error: {error}"));
                UnitResult {
                    letter: root,
                    error: Some(error),
                }
            }
        };
        append_unit(out, &unit);
    }
    0
}

/// Resultado por unidad del escaneo multiunidad: una línea JSONL por unidad
/// terminada, en el orden en que el hijo las termina.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnitResult {
    /// Raíz escaneada ya normalizada (p. ej. `C:\`); vacía si el fallo fue
    /// previo a elegir unidad (base de datos o enumeración).
    pub letter: String,
    /// Motivo del fallo; `None` si la unidad se escaneó y guardó bien.
    pub error: Option<String>,
}

/// Totales de una unidad terminada. El padre no los lee del JSON sino de la
/// base de datos; el hijo solo los usa para el `ElevatedResult` de una unidad.
struct VolumeScan {
    snapshot_id: u64,
    files: u64,
    bytes: u64,
    read_errors: u64,
    duration_ms: i64,
}

/// Directorios por lote al escribir el snapshot (compromiso latencia/memoria):
/// lo que hay en memoria son 512 filas, no el snapshot entero.
const DIR_BATCH: usize = 512;

/// Escanea una unidad y deja su snapshot guardado en `store`.
///
/// Los directorios se persisten en lotes de [`DIR_BATCH`] mientras llegan. La
/// atomicidad la garantiza el `ROLLBACK` del writer si no se llega a `finish`,
/// así que un fallo a mitad no deja rastro parcial.
///
/// El MFT es más rápido y corre en el proceso elevado; si el volumen no es
/// NTFS (o el formato sorprende) se cae al walker, que siempre funciona.
fn scan_one_volume(
    root: &str,
    store: &mut disky_core::SqliteStore,
    started_at: i64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<VolumeScan, String> {
    let root_path = std::path::PathBuf::from(root);
    if !root_path.is_dir() {
        return Err(format!("La ruta no existe o no es un directorio: `{root}`"));
    }
    // Clave canónica en la BD: separadores nativos (p. ej. llega `C:/x` desde bash).
    let root = disky_core::platform::path_norm::normalize_path_separators(root);
    let started = std::time::Instant::now();
    let mut writer = store
        .open_writer(&root, started_at)
        .map_err(|e| e.to_string())?;

    let mut batch: Vec<disky_core::DirStat> = Vec::with_capacity(DIR_BATCH);
    let mut write_error: Option<String> = None;
    let mut push = |dir: disky_core::DirStat| {
        if write_error.is_some() {
            return;
        }
        batch.push(dir);
        if batch.len() < DIR_BATCH {
            return;
        }
        let full = std::mem::replace(&mut batch, Vec::with_capacity(DIR_BATCH));
        if let Err(e) = writer.write_dirs(&full) {
            write_error = Some(format!("El snapshot no pudo guardarse: {e}"));
        }
    };

    let totals = {
        trace_elevated("antes de mft_scan");
        match disky_core::mft_scan(&root_path, cancel, &mut push, &mut |_| {}) {
            Ok(totals) => {
                trace_elevated("mft_scan OK");
                totals
            }
            // Cancelación real: caer al walker solo repetiría el mismo error.
            Err(disky_core::MftError::Cancelled) => return Err("Escaneo cancelado".into()),
            Err(e) => {
                // Los directorios que el MFT alcanzó a escribir los reemite el
                // walker y `dirs` es PRIMARY KEY: el REPLACE los pisa.
                let err_label = e.to_string();
                trace_elevated(&format!("mft_scan: {err_label}; fallback a walker"));
                match disky_core::walk_tree(&root_path, cancel, &mut push, &mut |_| {}) {
                    Ok(totals) => totals,
                    Err(walk_err) => return Err(format!("{err_label} (walk: {walk_err})")),
                }
            }
        }
    };
    // Lote final: lo que quedó sin llegar a DIR_BATCH (un escaneo con menos de
    // 512 carpetas no volcó nada todavía).
    if !batch.is_empty() && write_error.is_none() {
        if let Err(e) = writer.write_dirs(&batch) {
            write_error = Some(format!("El snapshot no pudo guardarse: {e}"));
        }
    }
    if let Some(error) = write_error {
        return Err(error);
    }

    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (files, bytes, read_errors) = (totals.files, totals.bytes, totals.read_errors);
    let snapshot_id = writer
        .finish(totals, duration_ms)
        .map_err(|e| e.to_string())?;
    Ok(VolumeScan {
        snapshot_id,
        files,
        bytes,
        read_errors,
        duration_ms,
    })
}

/// Abre la BD del hijo o devuelve el motivo (texto) del fallo.
fn open_store(db: &str) -> Result<disky_core::SqliteStore, String> {
    disky_core::SqliteStore::open(std::path::Path::new(db))
        .map_err(|_| "No se pudo abrir la base de datos de snapshots".to_owned())
}

/// Añade una línea JSONL al archivo de resultados (append + flush: el padre la
/// lee mientras el hijo sigue corriendo).
fn append_unit(out: &str, unit: &UnitResult) {
    let Ok(json) = serde_json::to_string(unit) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{json}");
        let _ = file.flush();
    }
}

/// Segundos UNIX actuales (0 si el reloj del sistema está antes del epoch).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or_default())
        .unwrap_or_default()
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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // La base de datos vive en la carpeta de datos de la app:
            // %APPDATA%/com.breiner.disky/snapshots.db
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("snapshots.db");
            let store = disky_core::SqliteStore::open(&db_path)?;
            app.manage(state::AppState::new(store, db_path.clone()));

            // Compacta el WAL residual de un cierre forzado sin bloquear la
            // apertura de la ventana (conexión propia, fuera del hilo de setup).
            std::thread::Builder::new()
                .name("wal-checkpoint".into())
                .spawn(move || {
                    let _ = disky_core::truncate_wal(&db_path);
                })
                .expect("no se pudo crear el hilo de checkpoint");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_volumes,
            commands::scan_start,
            commands::scan_all_start,
            commands::scan_all_elevated_start,
            commands::scan_cancel,
            commands::scan_quick_start,
            commands::snapshots_list,
            commands::delete_snapshot,
            commands::growth_report,
            commands::largest_files,
            commands::largest_dirs,
            commands::treemap_nodes,
            commands::timeline_series,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
