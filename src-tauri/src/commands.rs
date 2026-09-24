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
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use disky_core::platform::path_norm::normalize_path_separators;
use disky_core::{
    growth_ranking, list_volumes as core_list_volumes, match_by_path,
    squarify, walk_tree, DirStat, DirWriter, GrowthReport, LargestDir, LargestFile,
    MftError, PlatformError, SnapshotStore as _, SnapshotSummary, SqliteStore, TreemapItem,
    WalkError,
};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{lock_store, AppState};

/// Máximo de filas del informe de crecimiento enviado a la UI.
const MAX_GROWTH_ROWS: usize = 50;

/// Máximo de carpetas del listado "más pesadas" enviado a la UI.
const MAX_LARGEST_DIRS: u32 = 50;

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
    // Solo la carpeta vista y su primer nivel: una consulta por prefijo en vez
    // de cargar el snapshot completo en cada drill-down.
    let folder_view = normalize_path_separators(&folder.clone().unwrap_or_else(|| root.clone()));
    let samples = store
        .load_dir_samples_prefixed(latest.id, &folder_view)
        .map_err(|e| e.to_string())?;
    let prev: HashMap<String, u64> = if snaps.len() > 1 {
        store
            .load_dir_samples_prefixed(snaps[1].id, &folder_view)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| (s.path, s.size_bytes))
            .collect()
    } else {
        HashMap::new()
    };
    drop(store);

    // El snapshot más reciente siempre trae separadores nativos (el store
    // normaliza al escribir); `folder_view` ya viene normalizado.
    let folder_path = folder_view;
    let prefix = format!("{folder_path}{}", std::path::MAIN_SEPARATOR);
    let folder_size = samples
        .iter()
        .find(|s| s.path == folder_path)
        .map_or(0_u64, |s| s.size_bytes);

    let mut children: Vec<TreemapItem> = samples
        .iter()
        .filter(|s| {
            s.path.starts_with(&prefix)
                && !s.path[prefix.len()..].contains(std::path::MAIN_SEPARATOR)
        })
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

/// Punto del timeline de una carpeta: tamaño y delta respecto al punto previo.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelinePointDto {
    /// Cuándo se midió (UNIX, segundos).
    pub measured_at: i64,
    /// Tamaño roll-up de la carpeta.
    pub size_bytes: u64,
    /// Delta contra el punto anterior de la serie (0 en el primero).
    pub delta_bytes: i64,
}

/// Serie temporal de `folder` bajo `root` (últimos `limit` escaneos).
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn timeline_series(
    state: State<'_, AppState>,
    root: String,
    folder: String,
    limit: Option<u32>,
) -> Result<Vec<TimelinePointDto>, String> {
    let store = lock_store(&state.store);
    let points = store
        .folder_series(&root, &folder, limit.unwrap_or(20))
        .map_err(|e| e.to_string())?;
    drop(store);

    Ok(points
        .iter()
        .enumerate()
        .map(|(i, point)| TimelinePointDto {
            measured_at: point.measured_at,
            size_bytes: point.size_bytes,
            delta_bytes: if i == 0 {
                0
            } else {
                delta_bytes(point.size_bytes, points[i - 1].size_bytes)
            },
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

/// Payload del evento `scan-done`: resultado del escaneo y crecimiento vs. el
/// snapshot anterior de la misma raíz.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanDonePayload {
    /// Snapshot guardado (ninguno si hubo error/cancelación).
    pub snapshot: Option<SnapshotSummary>,
    /// Crecimiento entre los dos snapshots más recientes de la raíz.
    pub growth: Option<GrowthDiff>,
    /// Archivos más pesados del snapshot (vacío si no hay snapshot).
    pub largest: Vec<LargestFile>,
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

/// Arranca el escaneo de **todas las unidades fijas** en secuencia, un hilo
/// para el lote completo. Por cada unidad emite `scan-all-unit` (arranque de
/// unidad, con la letra) y reutiliza `scan-progress`/`scan-done` por unidad.
///
/// # Errors
/// `String` si ya hay un escaneo en curso o no hay unidades fijas.
#[tauri::command]
pub fn scan_all_start(window: tauri::Window, state: State<'_, AppState>) -> Result<(), String> {
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("Ya hay un escaneo en curso".into());
    }
    state.cancel.store(false, Ordering::SeqCst);

    let fixed: Vec<String> = core_list_volumes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|v| matches!(v.kind, disky_core::DriveKind::Fixed) && v.total_bytes > 0)
        .map(|v| format!("{}\\", v.letter))
        .collect();
    if fixed.is_empty() {
        state.scanning.store(false, Ordering::SeqCst);
        return Err("No hay unidades fijas para escanear".into());
    }

    let handle = window.app_handle().clone();
    std::thread::spawn(move || {
        // State<'_> no puede cruzar al hilo: se re-deriva del handle.
        let state = handle.state::<AppState>();
        for (i, root) in fixed.iter().enumerate() {
            if state.cancel.load(Ordering::Relaxed) {
                break;
            }
            let _ = handle.emit(
                "scan-all-unit",
                ScanAllUnit {
                    letter: root.clone(),
                    index: i + 1,
                    total: fixed.len(),
                },
            );
            let root_path = PathBuf::from(root);
            let (snapshot, largest, error) = perform_scan(&handle, &state, root, &root_path);
            // El evento por unidad: el frontend refresca lo acumulado.
            let growth = if snapshot.is_some() {
                compute_growth_root(&state, root)
            } else {
                None
            };
            emit_done(&handle, snapshot, growth, largest, error.clone());
            if error.is_some() {
                // Falla o cancelación de la unidad: se continúa con la siguiente
                // salvo que se haya pedido cancelar.
                if state.cancel.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
        let _ = handle.emit("scan-all-done", ());
        state.scanning.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Aviso de unidad en curso dentro de un escaneo de todas.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanAllUnit {
    /// Raíz de la unidad (ej. `C:\`).
    pub letter: String,
    /// 1-based dentro del lote.
    pub index: usize,
    /// Cuántas unidades hay en el lote.
    pub total: usize,
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
    let root = normalize_path_separators(&path.display().to_string());

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

/// Elimina un snapshot (y sus directorios y top-N) por su id.
///
/// Idempotente: borrar un id inexistente es un no-op.
///
/// # Errors
/// `String` si el borrado en la base de datos falla.
#[tauri::command]
pub fn delete_snapshot(state: State<'_, AppState>, snapshot_id: u64) -> Result<(), String> {
    let mut store = lock_store(&state.store);
    store.delete_snapshot(snapshot_id).map_err(|e| e.to_string())
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

/// Archivos más pesados de un snapshot (los recogió el walker como top-N).
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn largest_files(
    state: State<'_, AppState>,
    snapshot_id: u64,
) -> Result<Vec<LargestFile>, String> {
    let store = lock_store(&state.store);
    store.load_top_files(snapshot_id).map_err(|e| e.to_string())
}

/// Carpetas más pesadas de un snapshot (roll-up de su subárbol), ordenadas
/// descendentemente por tamaño y sin incluir la raíz.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn largest_dirs(
    state: State<'_, AppState>,
    snapshot_id: u64,
) -> Result<Vec<LargestDir>, String> {
    let store = lock_store(&state.store);
    store
        .load_top_dirs(snapshot_id, MAX_LARGEST_DIRS)
        .map_err(|e| e.to_string())
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

/// Empuja un dir al búfer y lo vuelca a la BD cada [`DIR_BATCH`] entradas.
fn flush_dir(writer: &mut dyn DirWriter, batch: &mut Vec<DirStat>, dir: DirStat) {
    batch.push(dir);
    if batch.len() >= DIR_BATCH {
        let _ = writer.write_dirs(batch);
        batch.clear();
    }
}

/// Cuerpo del escaneo, ejecutado en un hilo dedicado.
///
/// Abre una **conexión `SQLite` propia** en vez de bloquear el store global: WAL
/// permite un escritor + N lectores concurrentes, así que la UI puede seguir
/// consultando (`treemap_nodes`, `snapshots_list`...) mientras el walk corre.
/// El snapshot es atómico (invisible hasta `finish`), emite progreso periódico
/// y cierra con el evento `scan-done` en todos los casos.
///
/// Devuelve el snapshot creado (`None` si se canceló o falló), los archivos
/// más pesados y el texto de error si lo hubo. El llamador es dueño del flag
/// `scanning` y del evento final.
fn perform_scan(
    handle: &AppHandle,
    state: &AppState,
    root: &str,
    root_path: &Path,
) -> (Option<SnapshotSummary>, Vec<LargestFile>, Option<String>) {
    let started_at = unix_now();
    let started = Instant::now();

    let Ok(mut store) = SqliteStore::open(&state.db_path) else {
        return (
            None,
            Vec::new(),
            Some("No se pudo abrir la base de datos para guardar el escaneo".into()),
        );
    };
    let Ok(mut writer) = store.open_writer(root, started_at) else {
        return (
            None,
            Vec::new(),
            Some("No se pudo iniciar el snapshot del escaneo".into()),
        );
    };

    // Búfer de directorios: escribe en la BD en lotes en vez de fila a fila.
    let mut batch: Vec<DirStat> = Vec::with_capacity(DIR_BATCH);
    // El MFT (rápido, segundos) se usa cuando corremos elevados; si no, el
    // walker. Ambos emiten DirStat en post-orden, así que el margen es idéntico.
    let use_mft = root
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)
        .is_some_and(|c| disky_core::mft_available(c.to_ascii_uppercase()));
    let mut scan_result = if use_mft {
        disky_core::mft_scan(
            root_path,
            &state.cancel,
            &mut |dir| flush_dir(writer.as_mut(), &mut batch, dir),
            &mut |_| {},
        )
        .map_err(ScanError::Mft)
    } else {
        walk_tree(
            root_path,
            &state.cancel,
            &mut |dir| flush_dir(writer.as_mut(), &mut batch, dir),
            &mut |progress| {
                let _ = handle.emit("scan-progress", progress);
            },
        )
        .map_err(ScanError::Walk)
    };

    // Si el MFT falló (y no fue cancelación), se descarta el intento (rollback
    // al soltar `writer`) y se reintenta con el walker, que siempre funciona.
    let mft_error = match &scan_result {
        Err(ScanError::Mft(err)) if !matches!(err, MftError::Cancelled) => Some(err.to_string()),
        _ => None,
    };
    if let Some(mft_error) = mft_error {
        drop(writer);
        let Ok(new_writer) = store.open_writer(root, started_at) else {
            return (
                None,
                Vec::new(),
                Some(format!(
                    "Falló el MFT ({mft_error}) y tampoco se pudo abrir el snapshot para reintentar"
                )),
            );
        };
        writer = new_writer;
        batch.clear();
        scan_result = walk_tree(
            root_path,
            &state.cancel,
            &mut |dir| flush_dir(writer.as_mut(), &mut batch, dir),
            &mut |progress| {
                let _ = handle.emit("scan-progress", progress);
            },
        )
        .map_err(ScanError::Walk);
    }
    // El resto del lote (y en cancelación, también: el writer descartado
    // hace rollback, así que escribir el resto aquí solo gasta CPU).
    if !batch.is_empty() {
        let _ = writer.write_dirs(&batch);
    }

    let outcome = match scan_result {
        Ok(totals) => {
            let (top, total_files, total_bytes, read_errors) =
                (totals.top.clone(), totals.files, totals.bytes, totals.read_errors);
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            match writer.finish(totals, duration_ms) {
                Ok(id) => {
                    let snapshot = SnapshotSummary {
                        id,
                        root: root.to_owned(),
                        started_at,
                        duration_ms,
                        total_files,
                        total_bytes,
                        read_errors,
                    };
                    (Some(snapshot), top, None)
                }
                Err(err) => (None, Vec::new(), Some(err.to_string())),
            }
        }
        // Cancelación: el escritor se descarta y la transacción hace rollback,
        // así que no queda un snapshot a medias.
        Err(scan_error) => (None, Vec::new(), Some(scan_error.to_string())),
    };

    // La conexión dedicada se cierra al soltar `store`: el guard global del
    // Mutex ya no existe y los lectores de la UI nunca se bloquearon.
    outcome
}

/// Error unificado de un escaneo (MFT o walker). La cancelación ya trae su
/// propio texto (`MftError::Cancelled` / `WalkError::Cancelled`).
enum ScanError {
    Walk(WalkError),
    Mft(MftError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Walk(err) => err.fmt(f),
            ScanError::Mft(err) => err.fmt(f),
        }
    }
}

/// Directorios por lote al escribir el snapshot (compromiso latencia/memoria).
const DIR_BATCH: usize = 512;

fn run_scan(handle: AppHandle, root: String, root_path: PathBuf) {
    let state = handle.state::<AppState>();
    let (snapshot, largest, error) = perform_scan(&handle, &state, &root, &root_path);

    // Growth fresco para la raíz escaneada (aunque falle, el evento informa).
    let growth = if snapshot.is_some() {
        compute_growth_root(&state, &root)
    } else {
        None
    };
    emit_done(&handle, snapshot, growth, largest, error);
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

/// Abre una conexión propia para leer el growth (el hilo llamador puede ser el
/// de un escaneo que retiene el store global; WAL permite leer en paralelo).
fn compute_growth_root(state: &AppState, root: &str) -> Option<GrowthDiff> {
    let Ok(store) = SqliteStore::open(&state.db_path) else {
        return None;
    };
    compute_growth(&store, root)
}

/// Emite `scan-done` con el payload completo.
fn emit_done(
    handle: &AppHandle,
    snapshot: Option<SnapshotSummary>,
    growth: Option<GrowthDiff>,
    largest: Vec<LargestFile>,
    error: Option<String>,
) {
    let _ = handle.emit(
        "scan-done",
        ScanDonePayload {
            snapshot,
            growth,
            largest,
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
    /// Archivos más pesados del snapshot (vacío si no hay snapshot).
    pub largest: Vec<LargestFile>,
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
    let root = normalize_path_separators(&path.display().to_string());
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
    let Ok(store) = SqliteStore::open(&state.db_path) else {
        emit_quick_error(
            handle,
            "No se pudo leer la base de datos tras el escaneo elevado".into(),
        );
        return;
    };
    let snapshot = store
        .list_snapshots(Some(root), 1)
        .ok()
        .and_then(|s| s.into_iter().next());
    let growth = compute_growth(&store, root);
    let largest = snapshot
        .as_ref()
        .and_then(|s| store.load_top_files(s.id).ok())
        .unwrap_or_default();
    drop(store);
    let _ = handle.emit(
        "scan-quick-done",
        ScanQuickDonePayload {
            snapshot,
            growth,
            largest,
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
            largest: Vec::new(),
            error: Some(error),
        },
    );
}

/// Escapa un argumento con comillas dobles para la línea de comandos de Windows.
///
/// Rechaza `"` embebidos (rutas legales de Windows no los contienen): envolver
/// un path que contenga una comilla rompería la estructura de `lpParameters`
/// en `ShellExecuteExW` (inyección de argumentos al hijo elevado). Fail-closed.
fn quote_arg(value: &str) -> String {
    if value.contains('"') {
        return String::new();
    }
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
