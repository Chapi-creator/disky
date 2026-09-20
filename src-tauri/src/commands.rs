//! Comandos IPC del frontend → core.
//!
//! Regla del shell: traducir, no decidir. Cada comando valida entrada mínima,
//! delega en [`disky_core`] y devuelve tipos serializables del dominio.
//! El escaneo corre en su propio hilo y reporta progreso por eventos
//! (`scan-progress`, `scan-done`) para no bloquear la UI.
//
// Los comandos Tauri toman sus parámetros por valor porque el macro
// `generate_handler` los deserializa como dueños; pedir referencias rompería
// la convención del framework.
#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use disky_core::{
    growth_ranking, journal_status, list_volumes as core_list_volumes, match_by_path,
    recent_records, squarify, walk_tree, GrowthReport, JournalRecord, PlatformError,
    SnapshotStore as _, SnapshotSummary, TreemapItem, UsnStatus, WalkError,
};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{lock_store, AppState};

/// Máximo de filas del informe de crecimiento enviado a la UI.
const MAX_GROWTH_ROWS: usize = 50;

/// Lienzo del treemap en coordenadas de layout (el SVG escala con viewBox).
const TREEMAP_W: f64 = 1_000.0;
const TREEMAP_H: f64 = 700.0;

/// Nodo del treemap listo para pintar en el SVG.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TreemapNodeDto {
    /// Ruta de la carpeta (vacía para el nodo sintético `[archivos]`).
    pub path: String,
    /// Nombre corto (último componente) para la etiqueta.
    pub name: String,
    /// Geometría en el lienzo de [`TREEMAP_W`] × [`TREEMAP_H`].
    pub x: f64,
    /// Geometría en el lienzo de [`TREEMAP_W`] × [`TREEMAP_H`].
    pub y: f64,
    /// Geometría en el lienzo de [`TREEMAP_W`] × [`TREEMAP_H`].
    pub w: f64,
    /// Geometría en el lienzo de [`TREEMAP_W`] × [`TREEMAP_H`].
    pub h: f64,
    /// Tamaño roll-up actual.
    pub size_bytes: u64,
    /// Delta vs. el snapshot anterior (0 para el primer escaneo).
    pub delta_bytes: i64,
    /// `true` para el nodo sintético de archivos sueltos.
    pub is_files: bool,
}

/// Treemap de los hijos directos de `folder` (o de la raíz) según el snapshot
/// más reciente, con los deltas contra el anterior.
////// Los archivos sueltos de la carpeta (no agrupados en ningún hijo) aparecen
/// como el nodo sintético `[archivos]`.
///
/// # Errors
/// `String` si la raíz no tiene escaneos o la consulta falla.
#[tauri::command]
pub fn treemap_nodes(
    state: State<'_, AppState>,
    root: String,
    folder: Option<String>,
) -> Result<Vec<TreemapNodeDto>, String> {
    let store = lock_store(&state.store);
    let snaps = store
        .list_snapshots(Some(&root), 2)
        .map_err(|e| e.to_string())?;
    let Some(latest) = snaps.first() else {
        return Err("Aún no hay escaneos de esta raíz".into());
    };
    let samples = store
        .load_dir_samples(latest.id)
        .map_err(|e| e.to_string())?;
    let prev: HashMap<String, u64> = if snaps.len() > 1 {
        store
            .load_dir_samples(snaps[1].id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| (s.path, s.size_bytes))
            .collect()
    } else {
        HashMap::new()
    };
    drop(store);

    let folder_path = folder.unwrap_or_else(|| root.clone());
    let prefix = format!("{folder_path}\\");
    let folder_size = samples
        .iter()
        .find(|s| s.path == folder_path)
        .map_or(0_u64, |s| s.size_bytes);

    let mut children: Vec<TreemapItem> = samples
        .iter()
        .filter(|s| s.path.starts_with(&prefix) && !s.path[prefix.len()..].contains('\\'))
        .map(|s| TreemapItem {
            path: s.path.clone(),
            size_bytes: s.size_bytes,
        })
        .collect();
    // squarify exige orden descendente por tamaño.
    children.sort_by_key(|c| std::cmp::Reverse(c.size_bytes));

    // Nodo sintético: bytes de archivos sueltos (total de la carpeta menos
    // lo que ya cubren los hijos).
    let children_total: u64 = children.iter().map(|c| c.size_bytes).sum();
    let files_node = folder_size.saturating_sub(children_total);
    if files_node > 0 {
        children.push(TreemapItem {
            path: String::new(),
            size_bytes: files_node,
        });
    }

    Ok(squarify(&children, TREEMAP_W, TREEMAP_H)
        .into_iter()
        .map(|node| {
            let is_files = node.path.is_empty();
            let delta = if is_files {
                delta_bytes(
                    files_node,
                    files_size_of(&prev, &folder_path, children_total),
                )
            } else {
                delta_bytes(
                    node.size_bytes,
                    prev.get(node.path.as_str()).copied().unwrap_or(0),
                )
            };
            TreemapNodeDto {
                name: if is_files {
                    "[archivos]".to_owned()
                } else {
                    node.path
                        .rsplit('\\')
                        .next()
                        .unwrap_or(&node.path)
                        .to_owned()
                },
                path: node.path,
                x: node.rect.x,
                y: node.rect.y,
                w: node.rect.w,
                h: node.rect.h,
                size_bytes: node.size_bytes,
                delta_bytes: delta,
                is_files,
            }
        })
        .collect())
}

/// Tamaño que tenían los archivos sueltos de `folder` en el snapshot anterior.
fn files_size_of(prev: &HashMap<String, u64>, folder: &str, children_total_now: u64) -> u64 {
    prev.get(folder)
        .map_or(0, |total| total.saturating_sub(children_total_now))
}

/// Delta `new − old` con saturación (los tamaños nunca superan `i64::MAX`).
fn delta_bytes(new: u64, old: u64) -> i64 {
    let delta = i128::from(new) - i128::from(old);
    i64::try_from(delta).unwrap_or(if delta > 0 { i64::MAX } else { i64::MIN })
}

/// Mensaje de bienvenida; queda como ejemplo del patrón command → core.
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hola, {name}! Te saluda el núcleo de Rust de disky.")
}

/// Enumera los volúmenes montados del sistema (solo lectura).
///
/// # Errors
/// [`PlatformError`] renderizado como `String` para el frontend.
#[tauri::command]
pub fn list_volumes() -> Result<Vec<disky_core::Volume>, String> {
    core_list_volumes().map_err(render_error)
}

/// Estado del USN Journal de una unidad (`"C"`, `"d"`, ...).
///
/// # Errors
/// [`PlatformError`] renderizado como `String` para el frontend.
#[tauri::command]
pub fn usn_status(letter: &str) -> Result<UsnStatus, String> {
    journal_status(letter).map_err(render_error)
}

/// Los registros más recientes del USN Journal de una unidad.
///
/// `max_records` limita la respuesta (por defecto 15); los más nuevos al final.
///
/// # Errors
/// [`PlatformError`] renderizado como `String` para el frontend.
#[tauri::command]
pub fn usn_recent(letter: &str, max_records: Option<usize>) -> Result<Vec<JournalRecord>, String> {
    recent_records(letter, max_records.unwrap_or(15)).map_err(render_error)
}

/// Payload del evento `scan-done`: resultado del escaneo y crecimiento vs. el
/// snapshot anterior de la misma raíz.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanDonePayload {
    /// Snapshot guardado (ninguno si hubo error/cancelación).
    pub snapshot: Option<SnapshotSummary>,
    /// Crecimiento entre los dos snapshots más recientes de la raíz.
    pub growth: Option<GrowthDiff>,
    /// Mensaje de error accionable, si falló o se canceló.
    pub error: Option<String>,
}

/// Informe de crecimiento entre dos snapshots de la misma raíz.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrowthDiff {
    /// Snapshot más antiguo de la comparación.
    pub old: SnapshotSummary,
    /// Snapshot más reciente de la comparación.
    pub new: SnapshotSummary,
    /// Ranking por delta descendente (limitado a [`MAX_GROWTH_ROWS`]).
    pub rows: Vec<GrowthReport>,
}

/// Arranca un escaneo de `root` en un hilo dedicado.
///
/// El progreso llega por el evento `scan-progress` y el resultado por
/// `scan-done`. Solo un escaneo a la vez; repetir la llamada devuelve error.
///
/// # Errors
/// `String` si ya hay un escaneo o la raíz no es un directorio accesible.
#[tauri::command]
pub fn scan_start(
    window: tauri::Window,
    state: State<'_, AppState>,
    root: String,
) -> Result<(), String> {
    // `swap` evita la carrera entre dos invocaciones simultáneas.
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("Ya hay un escaneo en curso".into());
    }
    state.cancel.store(false, Ordering::SeqCst);

    let path = PathBuf::from(root.trim());
    if !path.is_dir() {
        state.scanning.store(false, Ordering::SeqCst);
        return Err(format!("La ruta no existe o no es un directorio: `{root}`"));
    }
    let root = path.display().to_string();

    let handle = window.app_handle().clone();
    std::thread::spawn(move || run_scan(handle, root, path));
    Ok(())
}

/// Pide cancelar el escaneo en curso (el walker aborta en el próximo chequeo).
///
/// # Errors
/// `String` si no hay ningún escaneo en curso.
#[tauri::command]
pub fn scan_cancel(state: State<'_, AppState>) -> Result<(), String> {
    if !state.scanning.load(Ordering::SeqCst) {
        return Err("No hay ningún escaneo en curso".into());
    }
    state.cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Lista los snapshots guardados (más nuevos primero), opcionalmente de una raíz.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn snapshots_list(
    state: State<'_, AppState>,
    root: Option<String>,
) -> Result<Vec<SnapshotSummary>, String> {
    let store = lock_store(&state.store);
    store
        .list_snapshots(root.as_deref(), 100)
        .map_err(|e| e.to_string())
}

/// Comparación de los dos snapshots más recientes de `root`.
///
/// Devuelve `None` si hay menos de dos snapshots de esa raíz.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn growth_report(
    state: State<'_, AppState>,
    root: String,
) -> Result<Option<GrowthDiff>, String> {
    let store = lock_store(&state.store);
    let snaps = store
        .list_snapshots(Some(&root), 2)
        .map_err(|e| e.to_string())?;
    if snaps.len() < 2 {
        return Ok(None);
    }
    let new_snapshot = snaps[0].clone();
    let old_snapshot = snaps[1].clone();
    let old_samples = store
        .load_dir_samples(old_snapshot.id)
        .map_err(|e| e.to_string())?;
    let new_samples = store
        .load_dir_samples(new_snapshot.id)
        .map_err(|e| e.to_string())?;

    let rows = growth_ranking(&match_by_path(&old_samples, &new_samples))
        .into_iter()
        .take(MAX_GROWTH_ROWS)
        .collect();

    Ok(Some(GrowthDiff {
        old: old_snapshot,
        new: new_snapshot,
        rows,
    }))
}

/// Traduce errores del core a mensajes accionables para la UI.
#[must_use]
pub fn render_error(err: PlatformError) -> String {
    match err {
        // Error 1 = ERROR_INVALID_FUNCTION: el kernel rechaza el FSCTL.
        PlatformError::WindowsApi { letter, code: 1 } => {
            format!("La unidad {letter} no permite el FSCTL (¿no es NTFS?) o faltan permisos")
        }
        // Error 5 = ERROR_ACCESS_DENIED: el proceso no está elevado.
        PlatformError::WindowsApi { letter, code: 5 } => {
            format!("Leer la MFT/journal de {letter} requiere permisos de administrador")
        }
        PlatformError::WindowsApi { letter, code } => {
            format!("Windows devolvió el error {code} al consultar la unidad {letter}")
        }
        PlatformError::InvalidDriveLetter(letter) => {
            format!("`{letter}` no es una letra válida (usa una sola letra A–Z)")
        }
        PlatformError::UnsupportedPlatform => {
            "Este sistema operativo aún no tiene adaptador de volúmenes".to_owned()
        }
    }
}

/// Cuerpo del escaneo, ejecutado en un hilo dedicado.
///
/// Escribe el snapshot de forma atómica (invisible hasta `finish`), emite
/// progreso periódico y cierra con el evento `scan-done` en todos los casos.
fn run_scan(handle: AppHandle, root: String, root_path: PathBuf) {
    let state = handle.state::<AppState>();
    let started_at = unix_now();
    let started = Instant::now();

    let mut store = lock_store(&state.store);
    let Ok(mut writer) = store.open_writer(&root, started_at) else {
        emit_done(
            &handle,
            None,
            None,
            Some("No se pudo abrir la base de datos para guardar el escaneo".into()),
        );
        state.scanning.store(false, Ordering::SeqCst);
        return;
    };

    let scan_result = walk_tree(
        &root_path,
        &state.cancel,
        // Un directorio por llamada: el escritor se ocupa del lote.
        &mut |dir| {
            let _ = writer.write_dirs(std::slice::from_ref(&dir));
        },
        &mut |progress| {
            let _ = handle.emit("scan-progress", progress);
        },
    );

    match scan_result {
        Ok(totals) => {
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            match writer.finish(totals, duration_ms) {
                Ok(id) => {
                    let snapshot = SnapshotSummary {
                        id,
                        root: root.clone(),
                        started_at,
                        duration_ms,
                        total_files: totals.files,
                        total_bytes: totals.bytes,
                        read_errors: totals.read_errors,
                    };
                    let growth = compute_growth(&store, &root);
                    emit_done(&handle, Some(snapshot), growth, None);
                }
                Err(err) => emit_done(&handle, None, None, Some(err.to_string())),
            }
        }
        // Cancelación: el escritor se descarta y la transacción hace rollback,
        // así que no queda un snapshot a medias.
        Err(WalkError::Cancelled) => {
            emit_done(&handle, None, None, Some("Escaneo cancelado".into()));
        }
        Err(err) => emit_done(&handle, None, None, Some(err.to_string())),
    }

    state.scanning.store(false, Ordering::SeqCst);
}

/// Comparación de los dos snapshots más recientes de `root`, o `None` si hay
/// menos de dos. Los errores de store se degradan a "sin comparación".
fn compute_growth(store: &disky_core::SqliteStore, root: &str) -> Option<GrowthDiff> {
    let snaps = store.list_snapshots(Some(root), 2).ok()?;
    if snaps.len() < 2 {
        return None;
    }
    let new_snapshot = snaps[0].clone();
    let old_snapshot = snaps[1].clone();
    let old_samples = store.load_dir_samples(old_snapshot.id).ok()?;
    let new_samples = store.load_dir_samples(new_snapshot.id).ok()?;

    let rows = growth_ranking(&match_by_path(&old_samples, &new_samples))
        .into_iter()
        .take(MAX_GROWTH_ROWS)
        .collect();

    Some(GrowthDiff {
        old: old_snapshot,
        new: new_snapshot,
        rows,
    })
}

/// Emite `scan-done` con el payload completo.
fn emit_done(
    handle: &AppHandle,
    snapshot: Option<SnapshotSummary>,
    growth: Option<GrowthDiff>,
    error: Option<String>,
) {
    let _ = handle.emit(
        "scan-done",
        ScanDonePayload {
            snapshot,
            growth,
            error,
        },
    );
}

/// Payload del evento `scan-quick-done`: resultado del escaneo elevado.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanQuickDonePayload {
    /// Snapshot guardado por el hijo elevado (ninguno si falló/canceló).
    pub snapshot: Option<SnapshotSummary>,
    /// Crecimiento refrescado contra los dos snapshots más recientes.
    pub growth: Option<GrowthDiff>,
    /// Mensaje accionable (UAC cancelado, hijo falló, resultado corrupto...).
    pub error: Option<String>,
}

/// Campos del JSON del hijo elevado que interesan al padre; el resto de los
/// campos del JSON se ignoran (serde los descarta por defecto).
#[derive(Debug, serde::Deserialize)]
struct ElevatedJson {
    ok: bool,
    error: Option<String>,
}

/// Escaneo rápido con UAC (opción A): relanza disky elevado con `--elevated-scan`,
/// espera al hijo y emite el resultado por `scan-quick-done`.
///
/// Igual que el walk normal: un escaneo a la vez, progreso no disponible (el
/// hijo no emite eventos al proceso padre), cancelación no disponible.
///
/// # Errors
/// `String` si ya hay un escaneo en curso o la raíz no es válida.
#[tauri::command]
pub fn scan_quick_start(
    window: tauri::Window,
    state: State<'_, AppState>,
    root: String,
) -> Result<(), String> {
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("Ya hay un escaneo en curso".into());
    }

    let path = PathBuf::from(root.trim());
    if !path.is_dir() {
        state.scanning.store(false, Ordering::SeqCst);
        return Err(format!("La ruta no existe o no es un directorio: `{root}`"));
    }
    let root = path.display().to_string();
    let handle = window.app_handle().clone();

    std::thread::spawn(move || {
        let state = handle.state::<AppState>();

        let Some(exe) = std::env::current_exe().ok() else {
            state.scanning.store(false, Ordering::SeqCst);
            emit_quick_error(&handle, "No se pudo ubicar el ejecutable de disky".into());
            return;
        };

        // Archivo temporal único para esta corrida.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_default();
        let out_path = std::env::temp_dir().join(format!("disky-elevated-{stamp}.json"));
        let args = format!(
            "--elevated-scan {} --out {} --db {}",
            quote_arg(&root),
            quote_arg(&out_path.display().to_string()),
            quote_arg(&state.db_path.display().to_string()),
        );

        let launch = disky_core::platform::elevate::run_elevated(&exe, &args);
        state.scanning.store(false, Ordering::SeqCst);

        match launch {
            Ok(()) => finish_quick_scan(&handle, &state, &root, &out_path),
            Err(disky_core::platform::elevate::ElevateError::Cancelled) => {
                emit_quick_error(&handle, "Elevación cancelada por el usuario".into());
            }
            Err(err) => emit_quick_error(&handle, err.to_string()),
        }
    });
    Ok(())
}
/// Lee el JSON del hijo, valida el éxito y emite `scan-quick-done` con el
/// snapshot nuevo y el crecimiento refrescado.
fn finish_quick_scan(handle: &AppHandle, state: &State<'_, AppState>, root: &str, out_path: &Path) {
    let json = std::fs::read_to_string(out_path)
        .ok()
        .filter(|s| !s.trim().is_empty());
    let _ = std::fs::remove_file(out_path);
    let parsed: Option<ElevatedJson> = json.and_then(|s| serde_json::from_str(&s).ok());

    let Some(parsed) = parsed else {
        emit_quick_error(handle, "El proceso elevado no devolvió resultado".into());
        return;
    };
    if !parsed.ok {
        let error = parsed
            .error
            .unwrap_or_else(|| "El escaneo elevado falló".to_owned());
        emit_quick_error(handle, error);
        return;
    }

    // El hijo guardó el snapshot en la misma BD: lo recargamos para la UI.
    let store = lock_store(&state.store);
    let snapshot = store
        .list_snapshots(Some(root), 1)
        .ok()
        .and_then(|s| s.into_iter().next());
    let growth = compute_growth(&store, root);
    drop(store);
    let _ = handle.emit(
        "scan-quick-done",
        ScanQuickDonePayload {
            snapshot,
            growth,
            error: None,
        },
    );
}

/// Emite `scan-quick-done` con solo un error.
fn emit_quick_error(handle: &AppHandle, error: String) {
    let _ = handle.emit(
        "scan-quick-done",
        ScanQuickDonePayload {
            snapshot: None,
            growth: None,
            error: Some(error),
        },
    );
}

/// Escapa un argumento con comillas dobles para la línea de comandos de Windows.
fn quote_arg(value: &str) -> String {
    format!("\"{value}\"")
}

/// Segundos UNIX actuales (0 si el reloj del sistema está antes del epoch).
fn unix_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
    )
    .unwrap_or_default()
}
