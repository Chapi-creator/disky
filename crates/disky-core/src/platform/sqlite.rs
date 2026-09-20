//! Store de snapshots en `SQLite` (`rusqlite`), adaptador del port del dominio.
//!
//! Diseño:
//! - WAL + `NORMAL`: escrituras frecuentes de lotes sin sacrificar durabilidad
//!   razonable, y lectores concurrentes mientras se escanea.
//! - Escritura **atómica**: el snapshot nace en una transacción dedicada y
//!   solo se hace visible en `finish`. Si el escaneo se cancela o el proceso
//!   muere, la transacción se hace rollback y no queda rastro.
//! - Esquema con versión (`PRAGMA user_version = 1`) para poder migrar después.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::domain::scan::{
    DirStat, DirWriter, ScanTotals, SeriesPoint, SnapshotStore, SnapshotSummary, StoreError,
};
use crate::domain::UsageSample;

/// Esquema actual de la base de datos.
const SCHEMA_VERSION: i32 = 1;

/// Ruta por defecto de la base de datos: `%LOCALAPPDATA%\disky\snapshots.db`
/// en Windows, `~/.local/state/disky/snapshots.db` en el resto.
///
/// Se usa cuando el llamador no pasa una ruta explícita (tests y CLI usan la
/// suya).
#[must_use]
pub fn default_db_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(|base| Path::new(&base).join("disky").join("snapshots.db"))
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
            .map(|base| base.join("disky").join("snapshots.db"))
    }
}

/// Store de snapshots sobre un archivo `SQLite`.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Abre (o crea) la base de datos en `path` y aplica el esquema.
    ///
    /// # Errors
    /// [`StoreError::Db`] si no se puede abrir, configurar o crear el esquema.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Db(format!("creando carpeta de la BD: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| StoreError::Db(format!("abriendo {}: {e}", path.display())))?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Configura pragmas de rendimiento y durabilidad.
    fn configure(conn: &Connection) -> Result<(), StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err("activando WAL"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err("configurando synchronous"))?;
        Ok(())
    }

    /// Crea el esquema si no existe y verifica la versión.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snapshots (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                root         TEXT NOT NULL,
                started_at   INTEGER NOT NULL,
                duration_ms  INTEGER NOT NULL DEFAULT 0,
                total_files  INTEGER NOT NULL DEFAULT 0,
                total_bytes  INTEGER NOT NULL DEFAULT 0,
                read_errors  INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS dirs (
                snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
                path        TEXT NOT NULL,
                size_bytes  INTEGER NOT NULL,
                mtime_unix  INTEGER NOT NULL,
                files       INTEGER NOT NULL,
                PRIMARY KEY (snapshot_id, path)
            );
            CREATE INDEX IF NOT EXISTS idx_dirs_path ON dirs(path);
            PRAGMA foreign_keys = ON;",
        )
        .map_err(db_err("creando esquema"))?;
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_err("leyendo user_version"))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::Db(format!(
                "la BD fue creada por una versión más nueva (esquema {version})"
            )));
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err("fijando user_version"))?;
        Ok(())
    }

    /// Cuenta los directorios de un snapshot (helper de tests y verificación).
    #[must_use]
    pub fn dir_count(&self, snapshot_id: u64) -> u64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM dirs WHERE snapshot_id = ?1",
                [to_db(snapshot_id)],
                |row| row.get::<_, i64>(0),
            )
            .map(non_neg)
            .unwrap_or_default()
    }
}

impl SnapshotStore for SqliteStore {
    fn open_writer(
        &mut self,
        root: &str,
        started_at: i64,
    ) -> Result<Box<dyn DirWriter + '_>, StoreError> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(db_err("abriendo transacción"))?;
        self.conn
            .execute(
                "INSERT INTO snapshots(root, started_at) VALUES (?1, ?2)",
                rusqlite::params![root, started_at],
            )
            .map_err(|e| {
                // Si el INSERT falla la transacción queda abierta: rollback.
                let _ = self.conn.execute_batch("ROLLBACK");
                db_err("insertando snapshot")(e)
            })?;
        let id = self.conn.last_insert_rowid();
        Ok(Box::new(SqliteDirWriter {
            conn: &self.conn,
            id,
            finished: false,
        }))
    }

    fn list_snapshots(
        &self,
        root: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SnapshotSummary>, StoreError> {
        let sql = match root {
            Some(_) => {
                "SELECT id, root, started_at, duration_ms, total_files, total_bytes, read_errors
                 FROM snapshots WHERE root = ?1
                 ORDER BY id DESC LIMIT ?2"
            }
            None => {
                "SELECT id, root, started_at, duration_ms, total_files, total_bytes, read_errors
                 FROM snapshots
                 ORDER BY id DESC LIMIT ?1"
            }
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(db_err("listando snapshots"))?;
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<SnapshotSummary> {
            Ok(SnapshotSummary {
                id: non_neg(row.get::<_, i64>(0)?),
                root: row.get(1)?,
                started_at: row.get(2)?,
                duration_ms: row.get(3)?,
                total_files: non_neg(row.get::<_, i64>(4)?),
                total_bytes: non_neg(row.get::<_, i64>(5)?),
                read_errors: non_neg(row.get::<_, i64>(6)?),
            })
        };

        let rows = match root {
            Some(root) => stmt.query_map(rusqlite::params![root, limit], map_row)?,
            None => stmt.query_map(rusqlite::params![limit], map_row)?,
        };
        rows.map(|row| row.map_err(db_err("leyendo fila de snapshot")))
            .collect()
    }

    fn load_dir_samples(&self, snapshot_id: u64) -> Result<Vec<UsageSample>, StoreError> {
        let exists: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
                [to_db(snapshot_id)],
                |row| row.get(0),
            )
            .map_err(db_err("verificando snapshot"))?;
        if exists == 0 {
            return Err(StoreError::UnknownSnapshot(snapshot_id));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT path, started_at, size_bytes FROM dirs d JOIN snapshots s ON s.id = d.snapshot_id WHERE d.snapshot_id = ?1")
            .map_err(db_err("preparando consulta de dirs"))?;
        let rows = stmt
            .query_map([to_db(snapshot_id)], |row| {
                Ok(UsageSample {
                    path: row.get(0)?,
                    measured_at: row.get(1)?,
                    size_bytes: non_neg(row.get::<_, i64>(2)?),
                })
            })
            .map_err(db_err("leyendo dirs"))?;
        rows.map(|row| row.map_err(db_err("leyendo fila de dir")))
            .collect()
    }

    fn folder_series(
        &self,
        root: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<SeriesPoint>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.started_at, d.size_bytes
                 FROM snapshots s
                 JOIN dirs d ON d.snapshot_id = s.id
                 WHERE s.root = ?1 AND d.path = ?2
                 ORDER BY s.started_at DESC
                 LIMIT ?3",
            )
            .map_err(db_err("preparando serie temporal"))?;
        let rows = stmt
            .query_map(rusqlite::params![root, folder, limit], |row| {
                Ok(SeriesPoint {
                    measured_at: row.get(0)?,
                    size_bytes: non_neg(row.get::<_, i64>(1)?),
                })
            })
            .map_err(db_err("leyendo serie"))?;
        let mut points: Vec<SeriesPoint> = rows
            .map(|row| row.map_err(db_err("leyendo punto de serie")))
            .collect::<Result<_, _>>()?;
        // Se tomaron los más recientes (DESC); el contrato pide orden temporal
        // ascendente para dibujar la línea de izquierda a derecha.
        points.reverse();
        Ok(points)
    }

    fn prune(&mut self, root: &str, keep: u32) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM snapshots WHERE root = ?1 AND id NOT IN (
                    SELECT id FROM snapshots WHERE root = ?1 ORDER BY id DESC LIMIT ?2
                )",
                rusqlite::params![root, keep],
            )
            .map_err(db_err("podando snapshots"))?;
        Ok(())
    }
}

/// Escritor de un snapshot: transacción abierta (`BEGIN IMMEDIATE`), invisible
/// para los demás hasta `COMMIT` en `finish`.
struct SqliteDirWriter<'a> {
    conn: &'a Connection,
    id: i64,
    finished: bool,
}

impl Drop for SqliteDirWriter<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Snapshot sin confirmar (cancelación, error, proceso muerto):
            // rollback para que nunca se vea un snapshot a medias.
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

impl DirWriter for SqliteDirWriter<'_> {
    fn write_dirs(&mut self, dirs: &[DirStat]) -> Result<(), StoreError> {
        if dirs.is_empty() {
            return Ok(());
        }
        let mut stmt = self
            .conn
            .prepare_cached(
                "INSERT OR REPLACE INTO dirs(snapshot_id, path, size_bytes, mtime_unix, files)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(db_err("preparando INSERT de dirs"))?;
        for dir in dirs {
            let size = i64::try_from(dir.size_bytes).unwrap_or(i64::MAX);
            let files = i64::try_from(dir.files).unwrap_or(i64::MAX);
            stmt.execute(rusqlite::params![
                self.id,
                dir.path,
                size,
                dir.mtime_unix,
                files
            ])
            .map_err(db_err("insertando dir"))?;
        }
        Ok(())
    }

    fn finish(
        mut self: Box<Self>,
        totals: ScanTotals,
        duration_ms: i64,
    ) -> Result<u64, StoreError> {
        self.conn
            .execute(
                "UPDATE snapshots
                 SET duration_ms = ?1, total_files = ?2, total_bytes = ?3, read_errors = ?4
                 WHERE id = ?5",
                rusqlite::params![
                    duration_ms,
                    i64::try_from(totals.files).unwrap_or(i64::MAX),
                    i64::try_from(totals.bytes).unwrap_or(i64::MAX),
                    i64::try_from(totals.read_errors).unwrap_or(i64::MAX),
                    self.id
                ],
            )
            .map_err(db_err("actualizando totales"))?;
        self.conn
            .execute_batch("COMMIT")
            .map_err(db_err("confirmando snapshot"))?;
        self.finished = true;
        Ok(non_neg(self.id))
    }
}

/// Convierte un `i64` de la BD en `u64` (los conteos/tamaños nunca son negativos).
fn non_neg(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

/// Convierte un `u64` del dominio en `i64` para la BD (`SQLite` usa enteros con signo).
fn to_db(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Convierte un error de rusqlite en [`StoreError::Db`] con contexto.
fn db_err(context: &'static str) -> impl Fn(rusqlite::Error) -> StoreError {
    move |e| StoreError::Db(format!("{context}: {e}"))
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    // En tests, `expect`/`panic!` son idiomáticos para fallar con mensaje claro.
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::scan::match_by_path;
    use crate::platform::walk::walk_tree;
    use std::fs;
    use tempfile::TempDir;

    fn open_tmp() -> (TempDir, SqliteStore) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(&tmp.path().join("test.db")).expect("abrir store");
        (tmp, store)
    }

    fn write_all(writer: &mut Box<dyn DirWriter + '_>, dirs: &[(String, u64, i64, u64)]) {
        let stats: Vec<DirStat> = dirs
            .iter()
            .map(|(path, size, mtime, files)| DirStat {
                path: path.clone(),
                size_bytes: *size,
                mtime_unix: *mtime,
                files: *files,
            })
            .collect();
        writer.write_dirs(&stats).expect("escribir dirs");
    }

    #[test]
    fn write_finish_and_list_roundtrip() {
        let (_tmp, mut store) = open_tmp();

        let id = {
            let mut writer = store
                .open_writer("C:\\Datos", 1_700_000_000)
                .expect("writer");
            write_all(
                &mut writer,
                &[
                    ("C:\\Datos\\A".into(), 100, 1_700_000_001, 2),
                    ("C:\\Datos\\B".into(), 200, 1_700_000_002, 3),
                ],
            );
            writer
                .finish(
                    ScanTotals {
                        files: 5,
                        dirs: 3,
                        bytes: 300,
                        read_errors: 1,
                    },
                    1_500,
                )
                .expect("finish")
        };

        let snaps = store.list_snapshots(Some("C:\\Datos"), 10).expect("listar");
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, id);
        assert_eq!(snaps[0].root, "C:\\Datos");
        assert_eq!(snaps[0].total_files, 5);
        assert_eq!(snaps[0].total_bytes, 300);
        assert_eq!(snaps[0].read_errors, 1);
        assert_eq!(snaps[0].duration_ms, 1_500);

        assert_eq!(store.dir_count(id), 2);
    }

    #[test]
    fn unfinished_writer_is_invisible() {
        let (_tmp, mut store) = open_tmp();
        {
            let mut writer = store.open_writer("C:\\X", 1_700_000_000).expect("writer");
            write_all(&mut writer, &[("C:\\X\\A".into(), 1, 0, 1)]);
            // Sin finish: se descarta y hace rollback al soltarse.
        }
        assert!(store
            .list_snapshots(Some("C:\\X"), 10)
            .expect("listar")
            .is_empty());
    }

    #[test]
    fn dropped_writer_with_rollback_leaves_no_rows() {
        // Igual que el anterior pero explícito: el Drop hace ROLLBACK y la
        // transacción muere con la conexión intacta.
        let (_tmp, mut store) = open_tmp();
        {
            let mut writer = store.open_writer("C:\\Y", 1_700_000_000).expect("writer");
            write_all(&mut writer, &[("C:\\Y\\A".into(), 1, 0, 1)]);
            drop(writer);
        }
        assert_eq!(store.dir_count(1), 0);
    }

    #[test]
    fn load_dir_samples_returns_measured_at() {
        let (_tmp, mut store) = open_tmp();
        let id = {
            let mut writer = store.open_writer("C:\\Z", 1_234_567_890).expect("writer");
            write_all(&mut writer, &[("C:\\Z\\A".into(), 42, 0, 1)]);
            writer.finish(ScanTotals::default(), 0).expect("finish")
        };

        let samples = store.load_dir_samples(id).expect("cargar muestras");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].path, "C:\\Z\\A");
        assert_eq!(samples[0].size_bytes, 42);
        assert_eq!(samples[0].measured_at, 1_234_567_890);
    }

    #[test]
    fn load_unknown_snapshot_errors() {
        let (_tmp, store) = open_tmp();
        assert_eq!(
            store.load_dir_samples(999),
            Err(StoreError::UnknownSnapshot(999))
        );
    }

    #[test]
    fn prune_keeps_newest_only() {
        let (_tmp, mut store) = open_tmp();
        for started_at in [1, 2, 3] {
            let writer = store.open_writer("C:\\P", started_at).expect("writer");
            writer.finish(ScanTotals::default(), 0).expect("finish");
        }
        store.prune("C:\\P", 2).expect("podar");

        let snaps = store.list_snapshots(Some("C:\\P"), 10).expect("listar");
        assert_eq!(snaps.len(), 2);
        // Los más nuevos primero: ids 3 y 2.
        assert_eq!(snaps[0].id, 3);
        assert_eq!(snaps[1].id, 2);
    }

    #[test]
    fn prune_other_roots_is_untouched() {
        let (_tmp, mut store) = open_tmp();
        for root in ["C:\\A", "C:\\B", "C:\\A"] {
            let writer = store.open_writer(root, 1).expect("writer");
            writer.finish(ScanTotals::default(), 0).expect("finish");
        }
        store.prune("C:\\A", 1).expect("podar");

        assert_eq!(store.list_snapshots(Some("C:\\A"), 10).expect("a").len(), 1);
        assert_eq!(store.list_snapshots(Some("C:\\B"), 10).expect("b").len(), 1);
    }

    #[test]
    fn folder_series_returns_ascending_points() {
        let (_tmp, mut store) = open_tmp();
        // Tres snapshots de la misma raíz; la carpeta cambia de tamaño en cada uno.
        let sizes = [(1_000, 100_u64), (2_000, 250), (3_000, 150)];
        for (started_at, size) in sizes {
            let mut writer = store.open_writer("C:\\S", started_at).expect("writer");
            writer
                .write_dirs(&[
                    DirStat {
                        path: "C:\\S\\Carpeta".into(),
                        size_bytes: size,
                        mtime_unix: 0,
                        files: 1,
                    },
                    DirStat {
                        path: "C:\\S\\Otra".into(),
                        size_bytes: 1,
                        mtime_unix: 0,
                        files: 1,
                    },
                ])
                .expect("escribir");
            writer.finish(ScanTotals::default(), 0).expect("finish");
        }

        let series = store
            .folder_series("C:\\S", "C:\\S\\Carpeta", 100)
            .expect("serie");
        assert_eq!(
            series,
            vec![
                SeriesPoint {
                    measured_at: 1_000,
                    size_bytes: 100
                },
                SeriesPoint {
                    measured_at: 2_000,
                    size_bytes: 250
                },
                SeriesPoint {
                    measured_at: 3_000,
                    size_bytes: 150
                },
            ]
        );
        // Limitar funciona y respeta el orden (los más recientes).
        let limited = store
            .folder_series("C:\\S", "C:\\S\\Carpeta", 2)
            .expect("serie");
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].measured_at, 2_000);
        // Carpeta que nunca existió: serie vacía, no error.
        assert!(store
            .folder_series("C:\\S", "C:\\S\\Nunca", 100)
            .expect("serie vacía")
            .is_empty());
    }

    #[test]
    fn two_snapshots_diff_end_to_end() {
        // El caso de uso real: escanear hoy y mañana, y preguntar qué creció.
        let (_tmp, mut store) = open_tmp();

        let old_id = {
            let mut w = store.open_writer("C:\\Datos", 1_700_000_000).expect("w");
            write_all(
                &mut w,
                &[
                    ("C:\\Datos\\Discord".into(), 1_000, 0, 10),
                    ("C:\\Datos\\Docs".into(), 500, 0, 5),
                ],
            );
            w.finish(ScanTotals::default(), 0).expect("finish")
        };
        let new_id = {
            let mut w = store.open_writer("C:\\Datos", 1_700_086_400).expect("w");
            write_all(
                &mut w,
                &[
                    ("C:\\Datos\\Discord".into(), 7_000, 0, 12),
                    ("C:\\Datos\\Docs".into(), 500, 0, 5),
                ],
            );
            w.finish(ScanTotals::default(), 0).expect("finish")
        };

        let old = store.load_dir_samples(old_id).expect("old");
        let new = store.load_dir_samples(new_id).expect("new");
        let ranking = crate::domain::growth_ranking(&match_by_path(&old, &new));

        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].path, "C:\\Datos\\Discord");
        assert_eq!(ranking[0].delta_bytes, 6_000);
        assert_eq!(ranking[1].path, "C:\\Datos\\Docs");
        assert_eq!(ranking[1].delta_bytes, 0);
    }

    #[test]
    fn walk_store_diff_end_to_end() {
        // El flujo real completo sobre una carpeta temporal: escanear, mutar
        // archivos reales, volver a escanear y verificar el diff exacto.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("crece")).expect("crear crece");
        fs::create_dir_all(root.join("estable")).expect("crear estable");
        fs::write(root.join("crece").join("datos.bin"), vec![0_u8; 1_000]).expect("escribir");
        fs::write(root.join("estable").join("fijo.txt"), vec![0_u8; 100]).expect("escribir");
        let root_str = root.display().to_string();

        let (_db_tmp, mut store) = open_tmp();
        let scan = |store: &mut SqliteStore, started_at: i64| -> u64 {
            let mut dirs: Vec<DirStat> = Vec::new();
            let totals = walk_tree(
                root,
                &std::sync::atomic::AtomicBool::new(false),
                &mut |d| dirs.push(d),
                &mut |_| {},
            )
            .expect("walk");
            let mut writer = store.open_writer(&root_str, started_at).expect("writer");
            writer.write_dirs(&dirs).expect("escribir dirs");
            writer.finish(totals, 0).expect("finish")
        };
        let old_id = scan(&mut store, 1_000);

        // Mutaciones reales entre escaneos:
        fs::write(root.join("crece").join("datos.bin"), vec![0_u8; 2_500]).expect("crecer");
        fs::create_dir_all(root.join("nueva")).expect("crear nueva");
        fs::write(root.join("nueva").join("extra.bin"), vec![0_u8; 700]).expect("nueva");
        fs::remove_file(root.join("estable").join("fijo.txt")).expect("borrar");

        let new_id = scan(&mut store, 1_000 + 86_400);

        let old = store.load_dir_samples(old_id).expect("old");
        let new = store.load_dir_samples(new_id).expect("new");
        let ranking = crate::domain::growth_ranking(&match_by_path(&old, &new));

        let delta_of = |suffix: &str| {
            ranking
                .iter()
                .find(|r| r.path.ends_with(suffix))
                .expect("carpeta presente en el ranking")
                .delta_bytes
        };
        // `crece`: +1 500. `estable`: el archivo borrado lo encoge −100.
        // La raíz suma ambos MÁS la carpeta nueva (+700): el rollup de la raíz
        // contiene todo su subárbol aunque `nueva` no aparezca como fila.
        let root_delta = ranking
            .iter()
            .find(|r| r.path == root_str)
            .expect("la raíz presente en el ranking")
            .delta_bytes;
        assert_eq!(delta_of("crece"), 1_500);
        assert_eq!(delta_of("estable"), -100);
        assert_eq!(root_delta, 1_500 - 100 + 700);
        assert!(ranking.iter().all(|r| !r.path.ends_with("nueva")));
    }
}
