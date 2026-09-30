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

/// Traza de diagnóstico del hijo elevado.
///
/// En release el proceso elevado no tiene consola (`windows_subsystem`), así que
/// un fallo del escaneo con admin no dejaba rastro alguno. Este log vive junto a
/// la base de datos (`elevated-last.log`) y se reescribe en cada corrida: es el
/// sitio donde mirar cuando «el escaneo con admin no hace nada».
struct ElevatedLog {
    /// Archivo de traza; `None` si no se pudo resolver (no es fatal).
    path: Option<std::path::PathBuf>,
}

impl ElevatedLog {
    /// Prepara la traza junto a `db`, truncando la corrida anterior.
    ///
    /// Interesa la última ejecución, no un histórico que crecería sin control.
    fn beside_db(db: &str) -> Self {
        if db.is_empty() {
            return Self { path: None };
        }
        let path = std::path::Path::new(db).with_file_name("elevated-last.log");
        // Best-effort: si no se puede truncar, el `append` de `write` seguirá
        // funcionando (solo se acumularían líneas de corridas anteriores).
        let _ = std::fs::write(&path, b"");
        Self { path: Some(path) }
    }

    /// Añade una línea a la traza. Nunca tumba el escaneo: el log es auxiliar.
    fn write(&self, msg: &str) {
        #[cfg(debug_assertions)]
        eprintln!("[elev] {msg}");
        let Some(path) = &self.path else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write as _;
            let _ = writeln!(file, "{msg}");
        }
    }
}

/// Modo hijo elevado: escanea `root`, guarda el snapshot en la BD indicada y
/// escribe el resultado en `out`. Devuelve el código de salida del proceso.
///
/// El snapshot es atómico (transacción del store): si algo falla a mitad, no
/// queda rastro. `ok: false` también se serializa para que el padre muestre
/// el error en la UI.
#[must_use]
#[allow(clippy::expect_used)]
pub fn elevated_scan(root: &str, out: &str, db: &str, cancel_file: &str) -> i32 {
    let log = ElevatedLog::beside_db(db);
    log.write(&format!("arranca hijo · raiz={root}"));
    let started_at = unix_now();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    watch_cancel_file(cancel_file, &cancel);

    let mut store = match open_store(db) {
        Ok(store) => store,
        Err(error) => return write_single_fail(out, &error),
    };
    match scan_one_volume(root, &mut store, started_at, &cancel, &log) {
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
pub fn elevated_scan_all(out: &str, db: &str, cancel_file: &str) -> i32 {
    let log = ElevatedLog::beside_db(db);
    log.write("arranca hijo multiunidad");
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

    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    watch_cancel_file(cancel_file, &cancel);
    for root in roots {
        let unit = match scan_one_volume(&root, &mut store, started_at, &cancel, &log) {
            Ok(volume) => {
                log.write(&format!(
                    "{} ok en {} ms",
                    volume.snapshot_id, volume.duration_ms
                ));
                UnitResult {
                    letter: root,
                    error: None,
                }
            }
            Err(error) => {
                log.write(&format!("{root} error: {error}"));
                UnitResult {
                    letter: root,
                    error: Some(error),
                }
            }
        };
        append_unit(out, &unit);
        // Cancelado: no seguir con la siguiente unidad (cada una sería un
        // snapshot que se descarta en el rollback).
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
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

/// ¿Es `path` la raíz de un volumen (`C:\`) y no una subcarpeta?
///
/// `Path::parent` devuelve `None` justo cuando la ruta termina en la raíz o el
/// prefijo (`C:\`, `\`, `\\servidor\recurso\`); cualquier subcarpeta tiene
/// padre.
fn is_volume_root(path: &std::path::Path) -> bool {
    path.parent().is_none()
}

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
    log: &ElevatedLog,
) -> Result<VolumeScan, String> {
    // La raíz canónica (separadores nativos, una sola barra final) se calcula
    // ANTES de derivar la ruta de escaneo. Si se escanea con una raíz sin
    // normalizar (`C:\\`), todos los descendientes salen con doble separador:
    // la clave de la BD sí se normaliza, así que la raíz queda `C:\`
    // mientras `C:\Users` se guarda como `C:\\Users` y el treemap solo ve la
    // raíz. Una sola forma para la clave y para el escáner.
    let root = disky_core::platform::path_norm::normalize_path_separators(root);
    let root_path = std::path::PathBuf::from(&root);
    if !root_path.is_dir() {
        return Err(format!("La ruta no existe o no es un directorio: `{root}`"));
    }
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

    let totals = if is_volume_root(&root_path) {
        log.write("antes de mft_scan");
        match disky_core::mft_scan(&root_path, cancel, &mut push, &mut |_| {}) {
            // Un `Ok` sin carpetas no es un escaneo: es la lectura del `$MFT`
            // que volvió en blanco. Antes se aceptaba igual y el guard final
            // descartaba el snapshot, que es justo «el modo admin no hace
            // nada». Se trata como un fallo del MFT y se cae al walker.
            Ok(totals) if totals.collected_nothing() => {
                log.write("mft_scan sin carpetas; fallback a walker");
                match disky_core::walk_tree(&root_path, cancel, &mut push, &mut |_| {}) {
                    Ok(totals) => totals,
                    Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                        return Err("Escaneo cancelado".into());
                    }
                    Err(walk_err) => {
                        return Err(format!(
                            "El MFT no devolvió ninguna carpeta (walk: {walk_err})"
                        ));
                    }
                }
            }
            Ok(totals) => {
                log.write("mft_scan OK");
                totals
            }
            // Cancelación real: caer al walker solo repetiría el mismo error.
            Err(disky_core::MftError::Cancelled) => return Err("Escaneo cancelado".into()),
            Err(e) => {
                // Los directorios que el MFT alcanzó a escribir los reemite el
                // walker y `dirs` es PRIMARY KEY: el REPLACE los pisa.
                let err_label = e.to_string();
                log.write(&format!("mft_scan: {err_label}; fallback a walker"));
                match disky_core::walk_tree(&root_path, cancel, &mut push, &mut |_| {}) {
                    Ok(totals) => totals,
                    // Cancelado: no adornar el error con el fallo del MFT.
                    Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                        return Err("Escaneo cancelado".into());
                    }
                    Err(walk_err) => return Err(format!("{err_label} (walk: {walk_err})")),
                }
            }
        }
    } else {
        // El MFT solo indexa la unidad entera: escanear una subcarpeta con él
        // daría el volumen completo y costaría lo mismo. Para subrutas, walker.
        log.write("subruta: solo walker");
        match disky_core::walk_tree(&root_path, cancel, &mut push, &mut |_| {}) {
            Ok(totals) => totals,
            Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                return Err("Escaneo cancelado".into());
            }
            Err(walk_err) => return Err(walk_err.to_string()),
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
    // Un escaneo que no emitió ni una carpeta es un fallo, no un snapshot: un
    // `$MFT` ilegible devuelve el índice vacío y confirmarlo dejaría la app en
    // blanco (el treemap y el timeline usan el escaneo más reciente). El writer
    // se descarta aquí, así que la transacción hace rollback.
    if totals.collected_nothing() {
        log.write("escaneo sin carpetas: se descarta el snapshot");
        return Err(totals.discard_reason());
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

/// Levanta un hilo que vigila el archivo centinela del padre y enciende `cancel`
/// en cuanto aparece.
///
/// El canal inverso no existe: un proceso sin privilegios no puede
/// `TerminateProcess` sobre un hijo elevado, así que la cancelación viaja como
/// un archivo que el padre crea y este hijo sondea.
fn watch_cancel_file(path: &str, cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
    if path.is_empty() {
        return;
    }
    let path = std::path::PathBuf::from(path);
    let cancel = std::sync::Arc::clone(cancel);
    // El hilo muere con el proceso; solo para de girar cuando hay cancelación.
    let _ = std::thread::Builder::new()
        .name("cancel-watch".into())
        .spawn(move || {
            while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                if path.exists() {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
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
    write_json(out, result);
}

/// Serializa `value` a JSON y lo deja en `out`.
///
/// Nunca falla en voz alta: si el archivo no aparece, el padre lo interpreta
/// como «el hijo no devolvió resultado» y muestra ese motivo, que es más útil
/// que un error de escritura que nadie puede leer.
fn write_json<T: serde::Serialize>(out: &str, value: &T) {
    if let Ok(json) = serde_json::to_string(value) {
        let _ = std::fs::write(out, json);
    }
}

/// Máximo de cambios del journal que pide el panel «¿qué cambió?» por defecto.
pub const USN_DEFAULT_LIMIT: usize = 200;

/// Resultado JSON que deja el hijo elevado del panel USN.
///
/// Mismo canal que el escaneo elevado (un archivo por corrida, sin canal
/// inverso): el panel es una consulta a demanda, no un watcher en vivo.
#[derive(serde::Serialize, serde::Deserialize)]
struct UsnResult {
    ok: bool,
    changes: Vec<disky_core::JournalChange>,
    error: Option<String>,
}

/// Modo hijo elevado del panel USN: lee los cambios recientes del journal de una
/// unidad, resuelve la ruta de cada uno desde la MFT y escribe el resultado en
/// `out`. Devuelve el código de salida del proceso (0 ok, 2 fallo).
///
/// El journal exige `GENERIC_READ` al volumen, que a su vez exige elevación: por
/// eso este trabajo vive en un hijo con UAC y no en el proceso de la app.
#[must_use]
pub fn elevated_usn(letter: &str, out: &str, limit: usize) -> i32 {
    match disky_core::recent_changes(letter, limit) {
        Ok(changes) => {
            write_json(
                out,
                &UsnResult {
                    ok: true,
                    changes,
                    error: None,
                },
            );
            0
        }
        Err(error) => {
            write_json(
                out,
                &UsnResult {
                    ok: false,
                    changes: Vec::new(),
                    error: Some(error.to_string()),
                },
            );
            2
        }
    }
}

/// Hijo de diagnóstico USN (`--elevated-diag-usn C --out <json>`): corre la
/// matriz de variantes de parámetros contra el journal y vuelca la tabla de
/// resultados. El kernel no dice QUÉ parámetro rechaza con el 87, así que se
/// prueban todos y se lee cuál pasa.
#[must_use]
pub fn elevated_diag_usn(letter: &str, out: &str) -> i32 {
    let letter = letter.chars().next().unwrap_or('C');
    let mut variants = disky_core::diag_usn_variants(letter);
    variants.extend(disky_core::diag_usn_follow(letter));
    variants.extend(disky_core::diag_usn_touch(letter));
    match disky_core::diag_mft_index(letter) {
        Ok((entries, children, errors)) => variants.push((
            "mft_index".to_owned(),
            format!("entries={entries} children={children} read_errors={errors}"),
        )),
        Err(error) => variants.push(("mft_index".to_owned(), format!("err:{error}"))),
    }
    let rows: Vec<serde_json::Value> = variants
        .iter()
        .map(|(name, result)| serde_json::json!({"variant": name, "result": result}))
        .collect();
    write_json(out, &rows);
    0
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
            let mut store = disky_core::SqliteStore::open(&db_path)?;
            // Autocuración al arrancar: los escaneos que no guardaron ni una
            // carpeta (rastro de un fallo de lectura confirmado por una versión
            // anterior) dejan el treemap y el timeline en blanco. Mejor esfuerzo:
            // si el `DELETE` falla, la app arranca igual y lo reintenta luego.
            let _ = store.delete_empty_snapshots();
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
            commands::export_growth_csv,
            commands::largest_files,
            commands::largest_dirs,
            commands::search_big_files,
            commands::find_duplicates,
            commands::usn_changes_start,
            commands::treemap_nodes,
            commands::timeline_series,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::is_volume_root;

    #[test]
    fn volume_root_detection_separates_unit_from_subfolder() {
        // El MFT solo tiene sentido en la raíz: en una subcarpeta indexaría la
        // unidad entera.
        assert!(is_volume_root(std::path::Path::new(r"C:\")));
        assert!(!is_volume_root(std::path::Path::new(r"C:\Users\Breiner")));
        assert!(!is_volume_root(std::path::Path::new(r"D:\Datos\cosas")));
    }
}
