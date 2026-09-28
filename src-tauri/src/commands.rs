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

use disky_core::platform::path_norm::{child_prefix, normalize_path_separators};
use disky_core::{
    fixed_volume_roots as core_fixed_volume_roots, list_volumes as core_list_volumes, squarify,
    walk_tree, ChangesError, DirStat, DirWriter, GrowthReport, GrowthTop, JournalChange,
    LargestDir, LargestFile, MftError, PlatformError, SnapshotStore as _, SnapshotSummary,
    SqliteStore, TreemapItem, WalkError,
};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{lock_store, AppState};
use crate::UnitResult;

/// Máximo de filas del informe de crecimiento enviado a la UI.
const MAX_GROWTH_ROWS: usize = 50;

/// Máximo de carpetas del listado "más pesadas" enviado a la UI.
const MAX_LARGEST_DIRS: u32 = 50;

/// Máximo de resultados de la búsqueda de archivos grandes.
const MAX_BIG_RESULTS: u32 = 200;

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
    base_id: Option<u64>,
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
    // Deltas contra la línea base elegida (default: el scan anterior). Una
    // base inválida no tumba el treemap: se cae al snapshot anterior.
    let prev: HashMap<String, u64> = old_snapshot(&store, &root, latest, base_id)
        .ok()
        .flatten()
        .map(|old| {
            store
                .load_dir_samples_prefixed(old.id, &folder_view)
                .map(|rows| rows.into_iter().map(|s| (s.path, s.size_bytes)).collect())
        })
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    drop(store);

    // El snapshot más reciente siempre trae separadores nativos (el store
    // normaliza al escribir); `folder_view` ya viene normalizado.
    let folder_path = folder_view;
    // `child_prefix` añade el separador final salvo en la raíz de volumen:
    // `C:\Users\` delimita los hijos reales de `C:\Users` y `C:\` cubre la
    // raíz (un sufijo `C:\\` doble barra filtraría TODO en la raíz).
    let prefix = child_prefix(&folder_path);
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
    // Archivos sueltos viejos = total viejo de la carpeta menos lo que
    // cubrían los MISMOS hijos en el snapshot anterior. Usar el total de
    // hijos NUEVO contaría el crecimiento de los hijos dos veces.
    let old_children: u64 = children
        .iter()
        .filter_map(|c| prev.get(&c.path).copied())
        .sum();
    let old_files = prev
        .get(&folder_path)
        .copied()
        .unwrap_or(0)
        .saturating_sub(old_children);
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
                delta_bytes(files_node, old_files)
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
                        .rsplit(std::path::MAIN_SEPARATOR)
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

/// Delta `new − old` con saturación (los tamaños nunca superan `i64::MAX`).
fn delta_bytes(new: u64, old: u64) -> i64 {
    let delta = i128::from(new) - i128::from(old);
    i64::try_from(delta).unwrap_or(if delta > 0 { i64::MAX } else { i64::MIN })
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

    let fixed = match core_fixed_volume_roots() {
        Ok(v) => v,
        Err(e) => {
            // No dejar `scanning=true` fijado: todos los escaneos futuros
            // responderían "ya hay un escaneo en curso" hasta reiniciar.
            state.scanning.store(false, Ordering::SeqCst);
            return Err(e.to_string());
        }
    };
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
    // Escaneo elevado: el hijo no ve el flag atómico (proceso aparte), así que
    // la señal le llega por su archivo centinela.
    state.cancel_elevated();
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
    store
        .delete_snapshot(snapshot_id)
        .map_err(|e| e.to_string())
}

/// Comparación de los dos snapshots más recientes de `root`.
///
/// Devuelve `None` si hay menos de dos snapshots de esa raíz.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
/// Resuelve el snapshot de línea base contra el que comparar `latest`.
///
/// `base_id` explícito (elegido en la UI) si existe, es más viejo que `latest`
/// y pertenece a `root`; si no, el inmediatamente anterior. `None` = la raíz
/// aún no tiene un segundo escaneo.
fn old_snapshot(
    store: &SqliteStore,
    root: &str,
    latest: &SnapshotSummary,
    base_id: Option<u64>,
) -> Result<Option<SnapshotSummary>, String> {
    match base_id {
        None => Ok(store
            .list_snapshots(Some(root), 2)
            .map_err(|e| e.to_string())?
            .get(1)
            .cloned()),
        Some(id) if id == latest.id => {
            Err("La línea base debe ser un escaneo anterior al más reciente".into())
        }
        Some(id) => Ok(store
            .list_snapshots(Some(root), 64)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|s| s.id == id && s.id < latest.id)),
    }
}

/// ¿Qué creció? `growth_report` con una línea base elegible vs. solo el scan
/// anterior: misma lógica pura del core, distinto snapshot de comparación.
///
/// # Errors
/// `String` si la línea base pedida no existe o es más reciente que la actual.
#[tauri::command]
pub fn growth_report(
    state: State<'_, AppState>,
    root: String,
    base_id: Option<u64>,
) -> Result<Option<GrowthDiff>, String> {
    let store = lock_store(&state.store);
    build_growth_diff(&store, &root, base_id)
}

/// Construye el diff de crecimiento de `root` contra la línea base elegida.
///
/// `None` cuando la raíz aún no tiene dos escaneos comparables; un `base_id`
/// inválido cae al snapshot anterior (misma política que el treemap). Extraído
/// para compartirlo entre [`growth_report`] y [`export_growth_csv`].
///
/// # Errors
/// `String` si una consulta a la base de datos falla.
fn build_growth_diff(
    store: &SqliteStore,
    root: &str,
    base_id: Option<u64>,
) -> Result<Option<GrowthDiff>, String> {
    let Some(new_snapshot) = store
        .list_snapshots(Some(root), 2)
        .map_err(|e| e.to_string())?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let Some(old_snapshot) = old_snapshot(store, root, &new_snapshot, base_id)? else {
        return Ok(None);
    };
    // Solo se materializa el lado nuevo (ordenado por ruta); el viejo se
    // recorre en streaming y se empareja por bisección. Antes esto cargaba los
    // dos snapshots completos más un `HashMap` con todas las rutas para
    // quedarse con 50 filas: en un `C:` real eran ~100 MB por llamada, y se
    // ejecuta tras cada escaneo y por cada unidad.
    let new_samples = store
        .load_dir_samples(new_snapshot.id)
        .map_err(|e| e.to_string())?;
    let mut top = GrowthTop::new(&new_samples, MAX_GROWTH_ROWS);
    store
        .for_each_dir_sample(old_snapshot.id, &mut |sample| top.push(&sample))
        .map_err(|e| e.to_string())?;

    Ok(Some(GrowthDiff {
        old: old_snapshot,
        new: new_snapshot,
        rows: top.finish(),
    }))
}

/// Escapa un campo para CSV: solo se entrecomilla si contiene un delimitador,
/// comilla o salto (las rutas de Windows pueden llevar `;` o comillas en el
/// nombre de un archivo).
fn csv_field(value: &str) -> String {
    if value.contains([';', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// Serializa un [`GrowthDiff`] a CSV (separador `;`, apto para Excel en
/// español). Los tamaños van en bytes crudos y además normalizados por día.
#[must_use]
pub fn growth_csv(diff: &GrowthDiff) -> String {
    // `write!` sobre el `String` ya existente evita una asignación por fila.
    use std::fmt::Write as _;

    let mut out =
        String::from("Carpeta;Antes (bytes);Ahora (bytes);Delta (bytes);Segundos;Bytes por dia\n");
    for row in &diff.rows {
        // Escribir en un `String` no falla nunca; el resultado se descarta.
        let _ = writeln!(
            out,
            "{};{};{};{};{};{:.0}",
            csv_field(&row.path),
            row.old_bytes,
            row.new_bytes,
            row.delta_bytes,
            row.elapsed_seconds,
            row.bytes_per_day(),
        );
    }
    out
}

/// Sufijo del nombre del informe: raíz saneada (sin separadores de ruta, que
/// romperían el nombre en Windows) más la marca de tiempo del momento.
fn csv_stamp(root: &str) -> String {
    let safe: String = root
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let safe = safe.trim_matches('-');
    let stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{}-{stamp}", if safe.is_empty() { "raiz" } else { safe })
}

/// Escribe el informe de «¿Qué creció?» como CSV en la carpeta de descargas y
/// devuelve la ruta del archivo (para revelarlo en el explorador).
///
/// Es la única escritura de disky, y nunca toca nada escaneado: el informe sale
/// a Descargas; si no se puede resolver, a la carpeta temporal.
///
/// # Errors
/// `String` si no hay dos escaneos que comparar o el archivo no se puede escribir.
#[tauri::command]
pub fn export_growth_csv(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    root: String,
    base_id: Option<u64>,
) -> Result<String, String> {
    let diff = {
        let store = lock_store(&state.store);
        build_growth_diff(&store, &root, base_id)?
    };
    let Some(diff) = diff else {
        return Err("Todavía no hay dos escaneos de esta raíz para comparar".into());
    };
    let dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().temp_dir())
        .map_err(|_| "No se pudo ubicar una carpeta para guardar el informe".to_owned())?;
    let file = dir.join(format!("disky-informe-{}.csv", csv_stamp(&root)));
    std::fs::write(&file, growth_csv(&diff))
        .map_err(|e| format!("No se pudo escribir el informe: {e}"))?;
    Ok(file.display().to_string())
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

/// Busca por nombre entre los archivos grandes (≥ 32 MiB) del snapshot.
///
/// `query` es un fragmento de ruta; vacío devuelve los más pesados. Responde a
/// "¿dónde está mi .iso de 40 GB?", que el top-N no cubre.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn search_big_files(
    state: State<'_, AppState>,
    snapshot_id: u64,
    query: String,
) -> Result<Vec<LargestFile>, String> {
    let store = lock_store(&state.store);
    store
        .search_big_files(snapshot_id, &query, MAX_BIG_RESULTS)
        .map_err(|e| e.to_string())
}

/// Duplicados probables entre los archivos grandes (≥ 32 MiB) de un snapshot:
/// mismo nombre y mismo tamaño, agrupados y ordenados por peso.
///
/// Es un heurístico de criba, no un hash: reduce el ruido a un puñado de
/// candidatos que valen la pena mirar, y la UI los etiqueta como tales.
///
/// # Errors
/// `String` si la consulta a la base de datos falla.
#[tauri::command]
pub fn find_duplicates(
    state: State<'_, AppState>,
    snapshot_id: u64,
) -> Result<Vec<disky_core::DuplicateGroup>, String> {
    let store = lock_store(&state.store);
    // Consulta vacía = índice completo de archivos grandes, por peso: el tope
    // es el del propio índice, no el de la búsqueda interactiva.
    let limit = u32::try_from(disky_core::BIG_FILE_MAX).unwrap_or(u32::MAX);
    let files = store
        .search_big_files(snapshot_id, "", limit)
        .map_err(|e| e.to_string())?;
    drop(store);
    Ok(disky_core::duplicate_groups(&files))
}

/// Máximo de cambios del journal que muestra el panel «¿qué cambió?».
const MAX_USN_CHANGES: usize = 200;

/// JSON que deja el hijo elevado del panel USN.
#[derive(Debug, serde::Deserialize)]
struct UsnJson {
    ok: bool,
    changes: Vec<JournalChange>,
    error: Option<String>,
}

/// Payload del evento `usn-changes`.
#[derive(Clone, serde::Serialize)]
struct UsnChangesPayload {
    letter: String,
    changes: Vec<JournalChange>,
    error: Option<String>,
}

/// Cambios recientes del journal de una unidad, con la ruta de cada archivo.
///
/// El journal no se puede leer sin permisos de administrador (el kernel rechaza
/// los FSCTL sin `GENERIC_READ` al volumen), así que el comando intenta primero
/// la lectura en este proceso —gratis si la app ya corre elevada— y solo si
/// Windows responde `ACCESS_DENIED` relanza el hijo con UAC.
///
/// El resultado viaja por el evento `usn-changes` en vez de por el valor de
/// retorno: reconstruir las rutas barre el índice de la MFT, que tarda segundos
/// en un disco grande, y bloquear el hilo de la UI durante eso congelaría la
/// ventana.
///
/// # Errors
/// `String` si ya hay una consulta en curso o la letra no es válida.
#[tauri::command]
pub fn usn_changes_start(
    window: tauri::Window,
    state: State<'_, AppState>,
    letter: String,
) -> Result<(), String> {
    if state.usn_reading.swap(true, Ordering::SeqCst) {
        return Err("Ya hay una consulta de cambios en curso".into());
    }
    // Validar la letra ya: el error se ve al instante, sin hilo ni UAC de por medio.
    let letter = match disky_core::platform::drive_letter(&letter) {
        Ok(valid) => valid.to_string(),
        Err(error) => {
            state.usn_reading.store(false, Ordering::SeqCst);
            return Err(render_error(error));
        }
    };

    let handle = window.app_handle().clone();
    std::thread::spawn(move || {
        let state = handle.state::<AppState>();
        match read_changes(&letter) {
            Ok(changes) => emit_usn(&handle, &letter, changes, None),
            Err(message) => emit_usn(&handle, &letter, Vec::new(), Some(message)),
        }
        state.usn_reading.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Lee los cambios recientes, elevándose solo si hace falta.
///
/// Corre elevado el proceso de la app, la lectura es un éxito sin más; si no,
/// el `ACCESS_DENIED` del kernel no es un fallo sino la señal de que toca pedir
/// el UAC. Cualquier otro error sí es un error.
fn read_changes(letter: &str) -> Result<Vec<JournalChange>, String> {
    match disky_core::recent_changes(letter, MAX_USN_CHANGES) {
        Ok(changes) => Ok(changes),
        Err(ChangesError::Journal(PlatformError::WindowsApi { code: 5, .. })) => {
            run_elevated_usn(letter)
        }
        Err(ChangesError::Journal(other)) => Err(render_error(other)),
        Err(ChangesError::Paths(error)) => Err(format!(
            "Se leyó el journal, pero no se pudieron reconstruir las rutas: {error}"
        )),
    }
}

/// Relanza disky elevado (`--elevated-usn`) y devuelve lo que dejó en su JSON.
///
/// Se reutiliza el canal del escaneo elevado: un archivo por corrida. Aquí no se
/// pasa `--cancel-file` porque la lectura tarda segundos, no minutos: cancelarla
/// no aporta y añadiría estado que nadie consulta.
fn run_elevated_usn(letter: &str) -> Result<Vec<JournalChange>, String> {
    let exe = std::env::current_exe()
        .map_err(|_| "No se pudo ubicar el ejecutable de disky".to_owned())?;
    let out_path = elevated_out_path("usn", "json");
    let args = format!(
        "--elevated-usn {} --out {} --limit {MAX_USN_CHANGES}",
        quote_arg(letter),
        quote_arg(&out_path.display().to_string()),
    );

    let launched = disky_core::platform::elevate::run_elevated(&exe, &args, &mut || {});
    let parsed: Option<UsnJson> = std::fs::read_to_string(&out_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let _ = std::fs::remove_file(&out_path);

    if let Some(json) = &parsed {
        if !json.ok {
            return Err(json
                .error
                .clone()
                .unwrap_or_else(|| "El lector elevado del journal falló".to_owned()));
        }
    }
    match launched {
        Ok(()) => Ok(parsed.map(|json| json.changes).unwrap_or_default()),
        Err(disky_core::platform::elevate::ElevateError::Cancelled) => {
            Err("Elevación cancelada por el usuario".to_owned())
        }
        Err(error) => {
            // El hijo deja el motivo real en el JSON aunque salga con código 2:
            // ese detalle es más útil que «código de salida N».
            let detail = parsed
                .and_then(|json| json.error)
                .filter(|s| !s.trim().is_empty());
            Err(detail.unwrap_or_else(|| error.to_string()))
        }
    }
}

/// Emite el resultado del panel USN (`usn-changes`).
fn emit_usn(handle: &AppHandle, letter: &str, changes: Vec<JournalChange>, error: Option<String>) {
    let _ = handle.emit(
        "usn-changes",
        UsnChangesPayload {
            letter: letter.to_owned(),
            changes,
            error,
        },
    );
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

/// Resultado de un escaneo descartado: sin snapshot, sin top-N y con el motivo.
fn discarded(reason: String) -> (Option<SnapshotSummary>, Vec<LargestFile>, Option<String>) {
    (None, Vec::new(), Some(reason))
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
        return discarded("No se pudo abrir la base de datos para guardar el escaneo".into());
    };
    let Ok(mut writer) = store.open_writer(root, started_at) else {
        return discarded("No se pudo iniciar el snapshot del escaneo".into());
    };

    // Búfer de directorios: escribe en la BD en lotes en vez de fila a fila.
    let mut batch: Vec<DirStat> = Vec::with_capacity(DIR_BATCH);
    // El MFT (rápido, segundos) se usa cuando corremos elevados; si no, el
    // walker. Ambos emiten DirStat en post-orden, así que el margen es idéntico.
    // El MFT indexa el volumen entero: en una subcarpeta las rutas se
    // reconstruirían desde el prefijo equivocado. Mismo guard que el hijo
    // elevado (`src/lib.rs::is_volume_root`), para que ningún camino escanee
    // una subruta con MFT.
    let use_mft = root_path.parent().is_none()
        && root
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
            return discarded(format!(
                "Falló el MFT ({mft_error}) y tampoco se pudo abrir el snapshot para reintentar"
            ));
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
        // Un escaneo sin carpetas es un fallo, no un snapshot vacío: guardarlo
        // envenenaría la línea base. El `writer` se descarta sin `finish`, así
        // que la transacción hace rollback y no queda rastro.
        Ok(totals) if totals.collected_nothing() => discarded(totals.discard_reason()),
        Ok(totals) => {
            let (top, total_files, total_bytes, read_errors) = (
                totals.top.clone(),
                totals.files,
                totals.bytes,
                totals.read_errors,
            );
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
        Err(scan_error) => discarded(scan_error.to_string()),
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
///
/// Delega en [`build_growth_diff`] para no tener dos implementaciones del mismo
/// diff (antes cada una hacía su propia carga completa).
fn compute_growth(store: &disky_core::SqliteStore, root: &str) -> Option<GrowthDiff> {
    build_growth_diff(store, root, None).ok().flatten()
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
    state.cancel.store(false, Ordering::SeqCst);

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
        let out_path = elevated_out_path("quick", "json");
        // El centinela solo existe si el usuario pulsa Cancelar: el hijo lo
        // sondea y aborta por su cuenta.
        let cancel_path = elevated_out_path("quick-cancel", "flag");
        let args = format!(
            "--elevated-scan {} --out {} --db {} --cancel-file {}",
            quote_arg(&root),
            quote_arg(&out_path.display().to_string()),
            quote_arg(&state.db_path.display().to_string()),
            quote_arg(&cancel_path.display().to_string()),
        );
        state.set_elevated_sentinel(Some(cancel_path.clone()));

        let launch = disky_core::platform::elevate::run_elevated(&exe, &args, &mut || {});
        state.set_elevated_sentinel(None);
        let _ = std::fs::remove_file(&cancel_path);

        match launch {
            Ok(()) => finish_quick_scan(&handle, &state, &root, &out_path),
            Err(disky_core::platform::elevate::ElevateError::Cancelled) => {
                emit_quick_error(&handle, "Elevación cancelada por el usuario".into());
            }
            Err(err) => {
                // El hijo a veces deja el JSON con el motivo real (exit 2):
                // priorizar ese detalle sobre "código de salida N".
                let detail = std::fs::read_to_string(&out_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<ElevatedJson>(&s).ok())
                    .and_then(|j| j.error)
                    .filter(|s| !s.trim().is_empty());
                let _ = std::fs::remove_file(&out_path);
                let msg = detail.unwrap_or_else(|| err.to_string());
                emit_quick_error(&handle, msg);
            }
        }
        // Liberar recién tras procesar el resultado: si se liberaba antes, el
        // usuario podía lanzar un segundo escaneo mientras este se leía (doble
        // escritura en la BD → SQLITE_BUSY espurio).
        state.scanning.store(false, Ordering::SeqCst);
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
    match read_unit_from_db(state, root) {
        Ok(done) => {
            let _ = handle.emit(
                "scan-quick-done",
                ScanQuickDonePayload {
                    snapshot: done.snapshot,
                    growth: done.growth,
                    largest: done.largest,
                    error: None,
                },
            );
        }
        Err(e) => emit_quick_error(handle, e),
    }
}

/// Lee de la BD el snapshot más reciente de `root` con su crecimiento y sus
/// archivos más pesados. Abre conexión propia: el hilo llamador puede estar
/// reteniendo el store global y WAL permite leer en paralelo.
fn read_unit_from_db(state: &State<'_, AppState>, root: &str) -> Result<ScanDonePayload, String> {
    let store = SqliteStore::open(&state.db_path)
        .map_err(|_| "No se pudo leer la base de datos tras el escaneo elevado".to_owned())?;
    let snapshot = store
        .list_snapshots(Some(root), 1)
        .ok()
        .and_then(|s| s.into_iter().next());
    let growth = compute_growth(&store, root);
    let largest = snapshot
        .as_ref()
        .and_then(|s| store.load_top_files(s.id).ok())
        .unwrap_or_default();
    Ok(ScanDonePayload {
        snapshot,
        growth,
        largest,
        error: None,
    })
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

/// Ruta temporal única para el archivo de resultados de una corrida elevada
/// (`disky-elevated-<tag>-<nanos>.<ext>`).
fn elevated_out_path(tag: &str, ext: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("disky-elevated-{tag}-{stamp}.{ext}"))
}

/// Escanea **todas las unidades fijas con un solo UAC**.
///
/// El hijo (`--elevated-scan-all`) recorre las unidades y deja una línea JSONL
/// por unidad terminada. El padre sondea ese archivo durante la espera y, por
/// cada línea, refresca la UI con `scan-all-unit` + `scan-done`: el frontend
/// acumula los resultados igual que en el escaneo sin admin.
///
/// # Errors
/// `String` si ya hay un escaneo en curso o no hay unidades fijas.
#[tauri::command]
pub fn scan_all_elevated_start(
    window: tauri::Window,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("Ya hay un escaneo en curso".into());
    }
    state.cancel.store(false, Ordering::SeqCst);

    // La misma lista que usa el hijo (`fixed_volume_roots`): así los índices
    // que ve la UI y las unidades que escanea el proceso elevado coinciden.
    let fixed = match core_fixed_volume_roots() {
        Ok(v) => v,
        Err(e) => {
            state.scanning.store(false, Ordering::SeqCst);
            return Err(e.to_string());
        }
    };
    if fixed.is_empty() {
        state.scanning.store(false, Ordering::SeqCst);
        return Err("No hay unidades fijas para escanear".into());
    }

    let handle = window.app_handle().clone();
    std::thread::spawn(move || {
        let state = handle.state::<AppState>();
        let Some(exe) = std::env::current_exe().ok() else {
            state.scanning.store(false, Ordering::SeqCst);
            let _ = handle.emit(
                "scan-done",
                ScanDonePayload {
                    snapshot: None,
                    growth: None,
                    largest: Vec::new(),
                    error: Some("No se pudo ubicar el ejecutable de disky".into()),
                },
            );
            let _ = handle.emit("scan-all-done", ());
            return;
        };

        let out_path = elevated_out_path("all", "jsonl");
        let cancel_path = elevated_out_path("all-cancel", "flag");
        let args = format!(
            "--elevated-scan-all --out {} --db {} --cancel-file {}",
            quote_arg(&out_path.display().to_string()),
            quote_arg(&state.db_path.display().to_string()),
            quote_arg(&cancel_path.display().to_string()),
        );
        state.set_elevated_sentinel(Some(cancel_path.clone()));

        let total = fixed.len();
        let mut pending = String::new();
        let mut done_units = 0usize;
        // Cuántos bytes del JSONL del hijo ya se copiaron a `pending`.
        let mut read_offset = 0usize;
        // Antes del UAC: la primera unidad ya está "en curso" para la UI.
        let _ = handle.emit(
            "scan-all-unit",
            ScanAllUnit {
                letter: fixed[0].clone(),
                index: 1,
                total,
            },
        );

        let mut tick = || {
            // El hijo solo añade al JSONL (append), así que basta con copiar los
            // bytes nuevos: releer el archivo entero reemitiría unidades ya
            // procesadas.
            if let Ok(bytes) = std::fs::read(&out_path) {
                append_new_bytes(&bytes, &mut read_offset, &mut pending);
                drain_unit_lines(&mut pending, &handle, &state, &fixed, &mut done_units);
            }
        };
        let launch = disky_core::platform::elevate::run_elevated(&exe, &args, &mut tick);
        state.set_elevated_sentinel(None);
        let _ = std::fs::remove_file(&cancel_path);

        // Última pasada con un salto de línea forzado: si el hijo murió entre
        // el `write` y el `flush` de la última unidad, su línea no contaría.
        pending.push('\n');
        drain_unit_lines(&mut pending, &handle, &state, &fixed, &mut done_units);
        let _ = std::fs::remove_file(&out_path);

        if let Err(err) = launch {
            let msg = match err {
                disky_core::platform::elevate::ElevateError::Cancelled => {
                    "Elevación cancelada por el usuario".to_owned()
                }
                other => other.to_string(),
            };
            let _ = handle.emit(
                "scan-done",
                ScanDonePayload {
                    snapshot: None,
                    growth: None,
                    largest: Vec::new(),
                    error: Some(msg),
                },
            );
        }
        let _ = handle.emit("scan-all-done", ());
        state.scanning.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Copia al `pending` solo los bytes que aún no se habían leído del JSONL del
/// hijo y avanza el offset. El hijo escribe en modo append, así que el archivo
/// solo crece: releerlo entero reprocesaría unidades ya emitidas.
fn append_new_bytes(bytes: &[u8], offset: &mut usize, pending: &mut String) -> bool {
    if bytes.len() <= *offset {
        return false;
    }
    pending.push_str(&String::from_utf8_lossy(&bytes[*offset..]));
    *offset = bytes.len();
    true
}

/// Consume las líneas nuevas del JSONL del hijo y refresca la UI por cada
/// unidad terminada. Deja en `pending` lo que todavía no forma una línea
/// completa (el hijo escribe y hace flush en caliente: una línea puede estar a
/// medias cuando el padre la lee).
fn drain_unit_lines(
    pending: &mut String,
    handle: &AppHandle,
    state: &State<'_, AppState>,
    fixed: &[String],
    done: &mut usize,
) {
    for unit in parse_unit_lines(pending) {
        if unit.letter.is_empty() {
            // Fallo previo a elegir unidad (base de datos o enumeración): no
            // hay snapshot que leer, solo el motivo.
            let _ = handle.emit(
                "scan-done",
                ScanDonePayload {
                    snapshot: None,
                    growth: None,
                    largest: Vec::new(),
                    error: unit.error,
                },
            );
            continue;
        }
        *done += 1;
        // La UI ya tiene el `scan-done` de cada unidad: ahora el crecimiento y
        // los archivos más pesados, leídos de la BD que el hijo acaba de escribir.
        let payload = match read_unit_from_db(state, &unit.letter) {
            Ok(mut payload) => {
                payload.error = unit.error;
                payload
            }
            Err(e) => ScanDonePayload {
                snapshot: None,
                growth: None,
                largest: Vec::new(),
                error: Some(e),
            },
        };
        let _ = handle.emit("scan-done", payload);
        // La siguiente unidad ya está corriendo en el hijo.
        if *done < fixed.len() {
            let _ = handle.emit(
                "scan-all-unit",
                ScanAllUnit {
                    letter: fixed[*done].clone(),
                    index: *done + 1,
                    total: fixed.len(),
                },
            );
        }
    }
}

/// Extrae del buffer las líneas JSONL completas y deja la cola incompleta.
///
/// Una línea a medias es normal: el hijo escribe y hace flush en caliente, así
/// que el padre puede leerla por la mitad. Se ignoran las líneas que no
/// parsean (un JSON truncado no debe tumbar el escaneo).
fn parse_unit_lines(pending: &mut String) -> Vec<UnitResult> {
    let mut units = Vec::new();
    while let Some(pos) = pending.find('\n') {
        let line: String = pending.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(unit) = serde_json::from_str::<UnitResult>(trimmed) {
            units.push(unit);
        }
    }
    units
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
    // Windows CommandLineToArgvW: un `\` pegado a la comilla final la escapa
    // (la raíz `C:\` fusionaba todo el comando en un arg). Duplicar las barras
    // finales las vuelve literales y la comilla cierra.
    let n = value.bytes().rev().take_while(|&b| b == b'\\').count();
    format!("\"{value}{}\"", "\\".repeat(n))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{append_new_bytes, csv_field, growth_csv, parse_unit_lines, quote_arg, GrowthDiff};
    use crate::{append_unit, UnitResult};
    use disky_core::{GrowthReport, SnapshotSummary};

    #[test]
    fn csv_field_quotes_only_when_needed() {
        // Un `;` o una comilla en el nombre de un archivo rompería las columnas.
        assert_eq!(csv_field("C:\\Users"), "C:\\Users");
        assert_eq!(csv_field("C:\\a;b"), "\"C:\\a;b\"");
        assert_eq!(csv_field("di\"r"), "\"di\"\"r\"");
    }

    #[test]
    fn growth_csv_has_header_and_one_row_per_folder() {
        let summary = |id: u64| SnapshotSummary {
            id,
            root: "C:\\".into(),
            started_at: 0,
            duration_ms: 0,
            total_files: 0,
            total_bytes: 0,
            read_errors: 0,
        };
        let diff = GrowthDiff {
            old: summary(1),
            new: summary(2),
            rows: vec![GrowthReport {
                path: "C:\\A".into(),
                old_bytes: 100,
                new_bytes: 400,
                delta_bytes: 300,
                elapsed_seconds: 86_400,
            }],
        };

        let csv = growth_csv(&diff);
        let mut lines = csv.lines();
        assert_eq!(
            lines.next(),
            Some("Carpeta;Antes (bytes);Ahora (bytes);Delta (bytes);Segundos;Bytes por dia")
        );
        assert_eq!(lines.next(), Some("C:\\A;100;400;300;86400;300"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn append_new_bytes_ignores_already_read_data() {
        // El tick del padre relee el JSONL entero cada 400 ms: si copiara todo,
        // las unidades ya emitidas saldrían duplicadas.
        let mut offset = 0usize;
        let mut pending = String::new();

        let first = b"{\"letter\":\"C:\\\\\",\"error\":null}\n{\"letter\":\"D:\\\\\",\"err";
        assert!(append_new_bytes(first, &mut offset, &mut pending));
        assert_eq!(parse_unit_lines(&mut pending).len(), 1);
        let after_first = pending.clone();

        // Segunda lectura sin novedades: no debe añadir ni un byte.
        assert!(!append_new_bytes(first, &mut offset, &mut pending));
        assert_eq!(pending, after_first);

        // Cuando el hijo termina la línea, solo se copia la cola.
        let grown =
            b"{\"letter\":\"C:\\\\\",\"error\":null}\n{\"letter\":\"D:\\\\\",\"error\":null}\n";
        assert!(append_new_bytes(grown, &mut offset, &mut pending));
        let units = parse_unit_lines(&mut pending);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].letter, r"D:\");
    }

    #[test]
    fn quote_arg_escapes_trailing_backslashes() {
        // La raíz `C:\` debe parsearse como un argumento único, no fusionar el resto.
        assert_eq!(quote_arg(r"C:\"), r#""C:\\""#);
        assert_eq!(quote_arg(r"C:\Users"), r#""C:\Users""#);
        assert_eq!(quote_arg(""), r#""""#);
    }

    #[test]
    fn parse_unit_lines_keeps_partial_line_for_next_read() {
        // El hijo escribe y hace flush en caliente: el padre puede leer una
        // línea a medias. Solo se consume lo que terminó en '\n'.
        let mut pending = String::new();
        assert!(parse_unit_lines(&mut pending).is_empty());
        pending.push_str("{\"letter\":\"C:\\\\\",\"error\":null}\n{\"letter\":\"D:\\\\\",\"err");
        let units = parse_unit_lines(&mut pending);

        assert_eq!(units.len(), 1);
        assert_eq!(units[0].letter, r"C:\");
        assert!(units[0].error.is_none());
        assert_eq!(pending, r#"{"letter":"D:\\","err"#);

        // El resto llega en la siguiente lectura, ahora completo.
        pending.push_str("or\":\"sin permiso\"}\n");
        let units = parse_unit_lines(&mut pending);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].letter, r"D:\");
        assert_eq!(units[0].error.as_deref(), Some("sin permiso"));
        assert!(pending.is_empty());
    }

    #[test]
    fn parse_unit_lines_skips_blank_and_corrupt_lines() {
        let mut pending =
            String::from("\n{\"letter\":\"C:\\\\\"\n{\"letter\":\"E:\\\\\",\"error\":null}\n");
        let units = parse_unit_lines(&mut pending);

        // La línea truncada no tumba el escaneo; la buena sí se entrega.
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].letter, r"E:\");
        assert!(pending.is_empty());
    }

    /// Contrato real padre↔hijo del escaneo multiunidad: lo que el hijo escribe
    /// con `append_unit` tiene que salir de `parse_unit_lines` en el mismo orden.
    #[test]
    fn jsonl_written_by_child_is_parsed_by_parent() {
        let path = std::env::temp_dir().join(format!("disky-jsonl-{}.jsonl", std::process::id()));
        let out = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);

        append_unit(
            &out,
            &UnitResult {
                letter: r"C:\".into(),
                error: None,
            },
        );
        append_unit(
            &out,
            &UnitResult {
                letter: r"D:\".into(),
                error: Some("sin permiso".into()),
            },
        );

        let mut pending = std::fs::read_to_string(&path).expect("el hijo dejó el JSONL");
        let units = parse_unit_lines(&mut pending);
        let _ = std::fs::remove_file(&path);

        assert_eq!(units.len(), 2);
        assert_eq!(units[0].letter, r"C:\");
        assert!(units[0].error.is_none());
        assert_eq!(units[1].letter, r"D:\");
        assert_eq!(units[1].error.as_deref(), Some("sin permiso"));
        assert!(pending.is_empty());
    }
}
