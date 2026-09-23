//! Escaneo de directorios y snapshots de uso (el fallback sin admin).
//!
//! Flujo: el walker de [`crate::platform`] recorre el árbol en post-orden y
//! emite [`DirStat`] (cada carpeta con el roll-up de sus descendientes). El
//! store los escribe incrementalmente en `SQLite`. Con dos snapshots de la misma
//! raíz, [`match_by_path`] + [`growth_ranking`] producen el informe
//! "¿qué creció?" sin lógica duplicada: el diff usa las mismas funciones puras
//! que el resto del dominio.

use std::collections::HashMap;

use crate::domain::UsageSample;

/// Estadística roll-up de un directorio, emitida cuando su subárbol terminó.
///
/// `size_bytes` y `files` incluyen a **todos** los descendientes (post-orden),
/// de modo que el diff de crecimiento se puede hacer solo con directorios,
/// sin cargar los archivos individuales.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct DirStat {
    /// Ruta absoluta del directorio.
    pub path: String,
    /// Bytes de todos los archivos descendentes.
    pub size_bytes: u64,
    /// Última modificación del propio directorio (UNIX, segundos; 0 si no se pudo leer).
    pub mtime_unix: i64,
    /// Cantidad de archivos descendentes.
    pub files: u64,
}

/// Totales de un escaneo completado.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct ScanTotals {
    /// Archivos visitados.
    pub files: u64,
    /// Directorios emitidos (incluye la raíz).
    pub dirs: u64,
    /// Bytes sumados de todos los archivos.
    pub bytes: u64,
    /// Entradas que no se pudieron leer (permisos, carreras con el FS...).
    pub read_errors: u64,
    /// Los archivos más pesados del escaneo, ordenados desc por peso.
    pub top: Vec<LargestFile>,
}

/// Un archivo individual por peso: lo recoge el walker mientras recorre el
/// árbol (top-N, no todos), para "¿qué archivo ocupa más?". Ordenado
/// descendentemente por `size_bytes`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct LargestFile {
    /// Ruta absoluta del archivo.
    pub path: String,
    /// Tamaño en bytes.
    pub size_bytes: u64,
    /// Última modificación (UNIX, segundos; 0 si no se pudo leer).
    pub mtime_unix: i64,
}

/// Progreso de un escaneo en curso (para eventos de UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct ScanProgress {
    /// Archivos visitados hasta ahora.
    pub files: u64,
    /// Directorios emitidos hasta ahora.
    pub dirs: u64,
    /// Bytes acumulados hasta ahora.
    pub bytes: u64,
    /// Errores de lectura acumulados.
    pub read_errors: u64,
}

/// Resumen serializable de un snapshot guardado en el store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct SnapshotSummary {
    /// Id asignado por el store.
    pub id: u64,
    /// Raíz escaneada (ruta absoluta).
    pub root: String,
    /// Cuándo empezó el escaneo (UNIX, segundos).
    pub started_at: i64,
    /// Duración del escaneo.
    pub duration_ms: i64,
    /// Archivos totales visitados.
    pub total_files: u64,
    /// Bytes totales acumulados.
    pub total_bytes: u64,
    /// Errores de lectura del escaneo.
    pub read_errors: u64,
}

/// Un punto de la serie temporal de una carpeta: su tamaño en un snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct SeriesPoint {
    /// Cuándo se tomó la medición (UNIX, segundos).
    pub measured_at: i64,
    /// Tamaño roll-up de la carpeta en ese momento.
    pub size_bytes: u64,
}

/// Error del store de snapshots.
///
/// El port vive en el dominio pero **no** conoce `SQLite`: el adaptador traduce
/// sus errores concretos a la variante [`StoreError::Db`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Fallo de la capa de persistencia (mensaje ya traducido por el adaptador).
    #[error("base de datos: {0}")]
    Db(String),
    /// El snapshot solicitado no existe (¿podado entre medidas?).
    #[error("el snapshot {0} no existe")]
    UnknownSnapshot(u64),
}

/// Port de persistencia de snapshots.
///
/// Contrato de escritura: [`SnapshotStore::open_writer`] abre un snapshot
/// *tentativo* (invisible para los lectores hasta `finish`); si el escritor se
/// descarta sin hacer `finish`, el snapshot desaparece (atomicidad). Los
/// snapshots viejos se recortan con [`SnapshotStore::prune`].
pub trait SnapshotStore {
    /// Abre un escritor atómico para un nuevo snapshot de `root`.
    ///
    /// # Errors
    /// [`StoreError`] si la base de datos falla al abrir la transacción.
    fn open_writer(
        &mut self,
        root: &str,
        started_at: i64,
    ) -> Result<Box<dyn DirWriter + '_>, StoreError>;

    /// Lista los snapshots (más nuevos primero), opcionalmente de una sola raíz.
    ///
    /// # Errors
    /// [`StoreError`] si la consulta falla.
    fn list_snapshots(
        &self,
        root: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SnapshotSummary>, StoreError>;

    /// Carga los directorios de un snapshot como [`UsageSample`] (medidos en
    /// `started_at` del snapshot), listos para [`crate::domain::growth_ranking`].
    ///
    /// # Errors
    /// [`StoreError::UnknownSnapshot`] si el id no existe; [`StoreError::Db`]
    /// si la consulta falla.
    fn load_dir_samples(&self, snapshot_id: u64) -> Result<Vec<UsageSample>, StoreError>;

    /// Serie temporal de `folder` bajo `root`: sus tamaños en los últimos
    /// `limit` snapshots, en orden temporal ascendente. Los snapshots donde la
    /// carpeta no existía simplemente no aportan punto.
    ///
    /// # Errors
    /// [`StoreError`] si la consulta falla.
    fn folder_series(
        &self,
        root: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<SeriesPoint>, StoreError>;

    /// Carga los directorios de un snapshot cuya ruta empieza por `prefix`,
    /// como [`UsageSample`] listos para filtrar hijos directos en la UI.
    ///
    /// Pensado para el treemap: una consulta de rango por el índice de `path`
    /// en lugar de cargar el snapshot completo por cada drill-down.
    ///
    /// # Errors
    /// [`StoreError`] si la consulta falla.
    fn load_dir_samples_prefixed(
        &self,
        snapshot_id: u64,
        prefix: &str,
    ) -> Result<Vec<UsageSample>, StoreError> {
        // Por defecto delega en la carga completa y filtra: los adaptadores
        // pueden redefinirlo con una consulta de rango real.
        Ok(self
            .load_dir_samples(snapshot_id)?
            .into_iter()
            .filter(|s| s.path.starts_with(prefix))
            .collect())
    }

    /// Elimina los snapshots más viejos de `root`, dejando los últimos `keep`.
    ///
    /// # Errors
    /// [`StoreError`] si el borrado falla.
    fn prune(&mut self, root: &str, keep: u32) -> Result<(), StoreError>;

    /// Elimina un snapshot concreto (y sus directorios y top-N, en cascada).
    ///
    /// No es un error borrar un id inexistente: es idempotente.
    ///
    /// # Errors
    /// [`StoreError::Db`] si el borrado falla.
    fn delete_snapshot(&mut self, snapshot_id: u64) -> Result<(), StoreError>;

    /// Carga los archivos más pesados de un snapshot (top-N recogido por el
    /// walker), ordenados descendentemente por peso.
    ///
    /// # Errors
    /// [`StoreError`] si la consulta falla.
    fn load_top_files(&self, snapshot_id: u64) -> Result<Vec<LargestFile>, StoreError>;
}

/// Escritura incremental de un snapshot en curso (post-orden de directorios).
pub trait DirWriter {
    /// Persiste un lote de directorios. Los lotes pueden ser de cualquier tamaño,
    /// incluso uno solo (`std::slice::from_ref`).
    ///
    /// # Errors
    /// [`StoreError`] si el INSERT falla.
    fn write_dirs(&mut self, dirs: &[DirStat]) -> Result<(), StoreError>;

    /// Confirma el snapshot con sus totales, recorta los viejos y devuelve el id.
    ///
    /// # Errors
    /// [`StoreError`] si el UPDATE/commit falla.
    fn finish(self: Box<Self>, totals: ScanTotals, duration_ms: i64) -> Result<u64, StoreError>;
}

/// Empareja las muestras nuevas con las viejas por ruta exacta.
///
/// Las rutas que existen solo en un lado (carpetas nuevas/borradas) se
/// descartan: el crecimiento de carpetas nuevas ya está visible en su padre.
#[must_use]
pub fn match_by_path<'a>(
    old: &'a [UsageSample],
    new: &'a [UsageSample],
) -> Vec<(&'a UsageSample, &'a UsageSample)> {
    let index: HashMap<&str, &UsageSample> = old.iter().map(|s| (s.path.as_str(), s)).collect();
    new.iter()
        .filter_map(|n| index.get(n.path.as_str()).map(|o| (*o, n)))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::cast_possible_wrap)]

    use super::*;
    use crate::domain::growth_ranking;
    use pretty_assertions::assert_eq;

    const BASE: i64 = 1_700_000_000;

    fn sample(path: &str, at: i64, size: u64) -> UsageSample {
        UsageSample {
            path: path.to_owned(),
            measured_at: at,
            size_bytes: size,
        }
    }

    #[test]
    fn match_pairs_only_shared_paths() {
        let old = vec![
            sample("C:\\A", BASE, 100),
            sample("C:\\B", BASE, 200),
            sample("C:\\SoloVieja", BASE, 999),
        ];
        let new = vec![
            sample("C:\\A", BASE + 60, 150),
            sample("C:\\SoloNueva", BASE + 60, 10_000),
            sample("C:\\B", BASE + 60, 50),
        ];

        let pairs = match_by_path(&old, &new);

        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0.path, "C:\\A");
        assert_eq!(pairs[1].0.path, "C:\\B");
    }

    #[test]
    fn empty_sides_produce_no_pairs() {
        let old = vec![sample("C:\\A", BASE, 1)];
        let new: Vec<UsageSample> = Vec::new();

        assert!(match_by_path(&old, &new).is_empty());
        assert!(match_by_path(&new, &old).is_empty());
    }

    #[test]
    fn pairs_feed_growth_ranking_end_to_end() {
        // Historia real: dos snapshots del mismo árbol, tres carpetas.
        let old = vec![
            sample("C:\\Discord", BASE, 4_000),
            sample("C:\\Cache", BASE, 1_000),
            sample("C:\\Docs", BASE, 500),
        ];
        let new = vec![
            sample("C:\\Discord", BASE + 3_600, 10_000),
            sample("C:\\Cache", BASE + 3_600, 400),
            sample("C:\\Docs", BASE + 3_600, 500),
        ];

        let ranking = growth_ranking(&match_by_path(&old, &new));

        assert_eq!(ranking.len(), 3);
        // Ordenado por delta descendente: Discord (+6 000), Docs (0), Cache (-600).
        assert_eq!(ranking[0].path, "C:\\Discord");
        assert_eq!(ranking[0].delta_bytes, 6_000);
        assert_eq!(ranking[1].path, "C:\\Docs");
        assert_eq!(ranking[2].path, "C:\\Cache");
        assert_eq!(ranking[2].delta_bytes, -600);
    }
}
