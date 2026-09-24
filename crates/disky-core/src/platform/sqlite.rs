//! Store de snapshots en `SQLite` (`rusqlite`), adaptador del port del dominio.
//!
//! Diseño:
//! - WAL + `NORMAL`: escrituras frecuentes de lotes sin sacrificar durabilidad
//!   razonable, y lectores concurrentes mientras se escanea.
//! - Escritura **atómica**: el snapshot nace en una transacción dedicada y
//!   solo se hace visible en `finish`. Si el escaneo se cancela o el proceso
//!   muere, la transacción se hace rollback y no queda rastro.
//! - Esquema con versión (`PRAGMA user_version = 1`) para poder migrar después.
//! - **Normalización de separadores**: la BD guarda `C:\a\b` (forma canónica
//!   del SO) y toda escritura/lectura pasa por
//!   [`crate::platform::path_norm::normalize_path_separators`], de modo que la
//!   misma ruta escrita como `C:/a/b` (CLI, bash) y consultada como `C:\a\b`
//!   (UI de Windows) coinciden.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::domain::scan::{
    DirStat, DirWriter, LargestDir, LargestFile, ScanTotals, SeriesPoint, SnapshotStore,
    SnapshotSummary, StoreError,
};
use crate::domain::UsageSample;
use crate::platform::path_norm::normalize_path_separators;

/// Esquema actual de la base de datos.
const SCHEMA_VERSION: i32 = 2;

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
            CREATE INDEX IF NOT EXISTS idx_dirs_size ON dirs(snapshot_id, size_bytes DESC);
            CREATE TABLE IF NOT EXISTS top_files (
                snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
                path        TEXT NOT NULL,
                size_bytes  INTEGER NOT NULL,
                mtime_unix  INTEGER NOT NULL,
                PRIMARY KEY (snapshot_id, path)
            );
            CREATE INDEX IF NOT EXISTS idx_top_files_size ON top_files(snapshot_id, size_bytes);
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

/// Clave canónica de ruta para la BD: separadores normalizados al del SO.
fn to_key(path: &str) -> String {
    normalize_path_separators(path)
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
                rusqlite::params![to_key(root), started_at],
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
        let root = root.map(normalize_path_separators);
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
            .prepare("SELECT path, started_at, size_bytes FROM dirs d JOIN snapshots s ON s.id = d.snapshot_id WHERE d.snapshot_id = ?1 ORDER BY d.path")
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
            .query_map(
                rusqlite::params![to_key(root), to_key(folder), limit],
                |row| {
                    Ok(SeriesPoint {
                        measured_at: row.get(0)?,
                        size_bytes: non_neg(row.get::<_, i64>(1)?),
                    })
                },
            )
            .map_err(db_err("leyendo serie"))?;
        let mut points: Vec<SeriesPoint> = rows
            .map(|row| row.map_err(db_err("leyendo punto de serie")))
            .collect::<Result<_, _>>()?;
        // Se tomaron los más recientes (DESC); el contrato pide orden temporal
        // ascendente para dibujar la línea de izquierda a derecha.
        points.reverse();
        Ok(points)
    }

    fn load_dir_samples_prefixed(
        &self,
        snapshot_id: u64,
        prefix: &str,
    ) -> Result<Vec<UsageSample>, StoreError> {
        let prefix = to_key(prefix);
        // Rango cerrado [prefix, prefix + max] sobre el índice de `path`: lee
        // solo las filas bajo el prefijo (p. ej. los hijos de una carpeta para
        // el treemap) sin escanear el snapshot completo.
        // El upper bound es el prefijo con su último BYTE incrementado. Se
        // calcula sobre bytes (no `char`): con prefijos no-ASCII el último
        // byte puede ser >0x7F y `(byte+1) as char` fabricaría un char
        // inválido o partiría un codepoint UTF-8 a la mitad (Upper suelto
        // malformado = resultados incorrectos). 0xFF... = rango abierto.
        let upper = {
            let mut bytes = prefix.clone().into_bytes();
            let Some(last) = bytes.pop() else {
                return Ok(Vec::new()); // prefijo vacío: ningún path lo empieza
            };
            if last == 0xFF {
                None // prefijo termina en 0xFF → sin upper (abierto)
            } else {
                bytes.push(last + 1);
                // `last+1 <= 0xFE`; si `last` era ASCII el resultado sigue
                // siendo un prefijo válido. Si `last` era >0x7F (multi-byte),
                // `last+1` ya no es un sufijo UTF-8 válido: `from_utf8` falla
                // y devolvemos abierto (fail-safe, sin resultados rotos).
                String::from_utf8(bytes).ok()
            }
        };
        let sql =
            "SELECT d.path, s.started_at, d.size_bytes FROM dirs d JOIN snapshots s ON s.id = d.snapshot_id
             WHERE d.snapshot_id = ?1 AND d.path >= ?2 AND (?3 IS NULL OR d.path < ?3)
             ORDER BY d.path";
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(db_err("preparando consulta prefijada de dirs"))?;
        let rows = stmt
            .query_map(
                rusqlite::params![to_db(snapshot_id), prefix, upper],
                |row| {
                    Ok(UsageSample {
                        path: row.get(0)?,
                        measured_at: row.get(1)?,
                        size_bytes: non_neg(row.get::<_, i64>(2)?),
                    })
                },
            )
            .map_err(db_err("leyendo dirs prefijados"))?;
        rows.map(|row| row.map_err(db_err("leyendo fila de dir prefijada")))
            .collect()
    }

    fn prune(&mut self, root: &str, keep: u32) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM snapshots WHERE root = ?1 AND id NOT IN (
                    SELECT id FROM snapshots WHERE root = ?1 ORDER BY id DESC LIMIT ?2
                )",
                rusqlite::params![to_key(root), keep],
            )
            .map_err(db_err("podando snapshots"))?;
        Ok(())
    }

    fn delete_snapshot(&mut self, snapshot_id: u64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM snapshots WHERE id = ?1",
                [to_db(snapshot_id)],
            )
            .map_err(db_err("borrando snapshot"))?;
        Ok(())
    }

    fn load_top_files(&self, snapshot_id: u64) -> Result<Vec<LargestFile>, StoreError> {
        let sql = "SELECT path, size_bytes, mtime_unix FROM top_files
                   WHERE snapshot_id = ?1 ORDER BY size_bytes DESC, path ASC";
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(db_err("preparando consulta de top_files"))?;
        let rows = stmt
            .query_map([to_db(snapshot_id)], |row| {
                Ok(LargestFile {
                    path: row.get(0)?,
                    size_bytes: non_neg(row.get::<_, i64>(1)?),
                    mtime_unix: row.get(2)?,
                })
            })
            .map_err(db_err("leyendo top_files"))?;
        rows.map(|row| row.map_err(db_err("leyendo fila de top_files")))
            .collect()
    }

    fn load_top_dirs(&self, snapshot_id: u64, limit: u32) -> Result<Vec<LargestDir>, StoreError> {
        // Excluye la propia raíz (siempre sería #1 y es trivial); el JOIN
        // evita que el comando resuelva la raíz del snapshot por separado.
        let sql = "SELECT d.path, d.size_bytes, d.files FROM dirs d
                   JOIN snapshots s ON s.id = d.snapshot_id
                   WHERE d.snapshot_id = ?1 AND d.path <> s.root
                   ORDER BY d.size_bytes DESC
                   LIMIT ?2";
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(db_err("preparando consulta de top_dirs"))?;
        let rows = stmt
            .query_map(rusqlite::params![to_db(snapshot_id), limit], |row| {
                Ok(LargestDir {
                    path: row.get(0)?,
                    size_bytes: non_neg(row.get::<_, i64>(1)?),
                    files: non_neg(row.get::<_, i64>(2)?),
                })
            })
            .map_err(db_err("leyendo top_dirs"))?;
        rows.map(|row| row.map_err(db_err("leyendo fila de top_dirs")))
            .collect()
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
                to_key(&dir.path),
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
        for file in &totals.top {
            let size = i64::try_from(file.size_bytes).unwrap_or(i64::MAX);
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO top_files(snapshot_id, path, size_bytes, mtime_unix)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![self.id, to_key(&file.path), size, file.mtime_unix],
                )
                .map_err(db_err("insertando archivo del top-N"))?;
        }
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
        // Flush del WAL a la BD principal: la transacción gigante del escaneo
        // no permite auto-checkpoint mientras está abierta, así que se fuerza
        // aquí. Si el proceso muere después, el WAL residual es minúsculo.
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(db_err("compactando WAL tras el escaneo"))?;
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

/// Compacta el WAL de la BD usando una conexión temporal (residuo de un cierre
/// forzado a mitad de escaneo). No bloquea: si otra conexión tiene una lectura
/// activa, SQLite devuelve `busy` y lo deja para el próximo arranque.
///
/// # Errors
/// [`StoreError::Db`] si no se puede abrir la BD.
pub fn truncate_wal(path: &Path) -> Result<(), StoreError> {
    let conn = Connection::open(path)
        .map_err(|e| StoreError::Db(format!("abriendo {}: {e}", path.display())))?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(db_err("compactando WAL"))?;
    Ok(())
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
                        top: Vec::new(),
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
    fn delete_snapshot_removes_cascade() {
        let (_tmp, mut store) = open_tmp();
        for started_at in [1, 2] {
            let mut writer = store.open_writer("C:\\P", started_at).expect("writer");
            write_all(&mut writer, &[("C:\\P\\x".into(), 10, 0, 1)]);
            writer.finish(ScanTotals::default(), 0).expect("finish");
        }
        let snaps = store.list_snapshots(Some("C:\\P"), 10).expect("listar");
        assert_eq!(snaps.len(), 2);
        let deleted_id = snaps[0].id;
        store.delete_snapshot(deleted_id).expect("borrar");

        let snaps = store.list_snapshots(Some("C:\\P"), 10).expect("listar");
        assert_eq!(snaps.len(), 1);
        assert_ne!(snaps[0].id, deleted_id);
        assert_eq!(
            store.load_dir_samples(deleted_id),
            Err(StoreError::UnknownSnapshot(deleted_id))
        );
    }

    #[test]
    fn delete_snapshot_is_idempotent() {
        let (_tmp, mut store) = open_tmp();
        store.delete_snapshot(999).expect("borrar inexistente es no-op");
        assert_eq!(store.list_snapshots(None, 10).expect("listar").len(), 0);
    }

    #[test]
    fn mixed_separators_match_one_canonical_form() {
        let (_tmp, mut store) = open_tmp();
        // Escrito con separadores `/` (como llega de bash o del modo CLI).
        let id = {
            let mut writer = store.open_writer("C:/Mix", 1_000).expect("writer");
            write_all(&mut writer, &[("C:/Mix/Sub".into(), 50, 0, 1)]);
            writer.finish(ScanTotals::default(), 10).expect("finish")
        };

        // Las lecturas con `\` (como llega de la UI de Windows) encuentran
        // exactamente lo mismo: una sola forma canónica en la BD.
        assert_eq!(store.dir_count(id), 1);
        assert_eq!(
            store
                .list_snapshots(Some("C:\\Mix"), 10)
                .expect("listar")
                .len(),
            1
        );
        let samples = store.load_dir_samples(id).expect("muestras");
        assert_eq!(samples[0].path, "C:\\Mix\\Sub");
        let series = store
            .folder_series("C:\\Mix", "C:\\Mix\\Sub", 10)
            .expect("serie");
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].size_bytes, 50);

        // Poda con la forma canónica sobre una raíz guardada con `/`.
        store.prune("C:\\Mix", 5).expect("podar");
        assert_eq!(
            store
                .list_snapshots(Some("C:\\Mix"), 10)
                .expect("listar")
                .len(),
            1
        );

        // Segunda escritura con `\` cae en la misma clave: la serie con `/`
        // sigue encontrando ambos puntos.
        let mut writer = store.open_writer("C:\\Mix", 2_000).expect("writer 2");
        write_all(&mut writer, &[("C:\\Mix\\Sub".into(), 80, 0, 1)]);
        writer.finish(ScanTotals::default(), 5).expect("finish 2");
        let series = store
            .folder_series("C:/Mix", "C:/Mix/Sub", 10)
            .expect("serie 2");
        assert_eq!(series.len(), 2);
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
    fn load_dir_samples_prefixed_matches_full_load_filtered() {
        let (_tmp, mut store) = open_tmp();
        let id = {
            let mut w = store.open_writer("C:\\Pfx", 1_000).expect("w");
            write_all(
                &mut w,
                &[
                    ("C:\\Pfx".into(), 999, 0, 1),
                    ("C:\\Pfx\\Hijo1".into(), 100, 0, 1),
                    ("C:\\Pfx\\Hijo1\\Nieto".into(), 60, 0, 1),
                    ("C:\\Pfx\\Hijo2".into(), 200, 0, 1),
                    ("C:\\Otra".into(), 5, 0, 1),
                    ("C:\\PfxZ".into(), 7, 0, 1), // comparte prefijo corto, no es hija
                ],
            );
            w.finish(ScanTotals::default(), 0).expect("finish")
        };

        let prefixed = store
            .load_dir_samples_prefixed(id, "C:\\Pfx\\")
            .expect("prefijada");
        let expected: Vec<String> = store
            .load_dir_samples(id)
            .expect("completa")
            .into_iter()
            .filter(|s| s.path.starts_with("C:\\Pfx\\"))
            .map(|s| s.path)
            .collect();
        let got: Vec<String> = prefixed.iter().map(|s| s.path.clone()).collect();

        assert_eq!(got, expected);
        assert_eq!(got.len(), 3); // Hijo1, Nieto, Hijo2 — sin PfxZ ni Otra
        assert!(got.contains(&"C:\\Pfx\\Hijo1\\Nieto".to_string()));
    }

    #[test]
    fn load_dir_samples_prefixed_upper_bound_utf8_safe() {
        // Regresión: el upper bound se calculaba con `(byte+1) as char`; con
        // prefijos no-ASCII (>0x7F) eso pateaba el slice a mitad de un
        // codepoint y/o frabricaba un char inválido (p. ej. `C:\Á` → `\u{C3C4}`),
        // excluyendo hijos reales del drill-down. Ahora: bytes + from_utf8,
        // abriendo el rango si el incremento no es UTF-8 válido.
        let (_tmp, mut store) = open_tmp();
        let prefix = "C:\\Á"; // último byte 0xC1 (>0x7F)
        let id = {
            let mut w = store.open_writer(prefix, 1_000).expect("w");
            write_all(
                &mut w,
                &[
                    ("C:\\Á".into(), 999, 0, 1),
                    ("C:\\Á\\Hijo1".into(), 100, 0, 1),
                    ("C:\\Á\\Hijo1\\Nieto".into(), 60, 0, 1),
                    ("C:\\ÁZ".into(), 7, 0, 1), // comparte prefijo, no hija
                    ("C:\\B".into(), 5, 0, 1),
                ],
            );
            w.finish(ScanTotals::default(), 0).expect("finish")
        };

        let prefixed = store
            .load_dir_samples_prefixed(id, "C:\\Á\\")
            .expect("prefijada utf8");
        let expected: Vec<String> = store
            .load_dir_samples(id)
            .expect("completa")
            .into_iter()
            .filter(|s| s.path.starts_with("C:\\Á\\"))
            .map(|s| s.path)
            .collect();
        let got: Vec<String> = prefixed.iter().map(|s| s.path.clone()).collect();

        assert_eq!(got, expected);
        assert!(got.contains(&"C:\\Á\\Hijo1\\Nieto".to_string()));
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

    #[test]
    fn top_files_roundtrip_and_ordered() {
        let (_tmp, mut store) = open_tmp();
        let totals = ScanTotals {
            top: vec![
                LargestFile {
                    path: "C:\\Datos\\a.bin".into(),
                    size_bytes: 100,
                    mtime_unix: 1,
                },
                LargestFile {
                    path: "C:\\Datos\\b.bin".into(),
                    size_bytes: 900,
                    mtime_unix: 2,
                },
                LargestFile {
                    path: "C:\\Datos\\c.bin".into(),
                    size_bytes: 500,
                    mtime_unix: 3,
                },
            ],
            ..ScanTotals::default()
        };
        let id = {
            let w = store.open_writer("C:\\Datos", 1_700_000_000).expect("w");
            w.finish(totals, 0).expect("finish")
        };

        let got = store.load_top_files(id).expect("load");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].path, "C:\\Datos\\b.bin", "el mayor primero");
        assert_eq!(got[0].size_bytes, 900);
        assert_eq!(got[1].path, "C:\\Datos\\c.bin");
        assert_eq!(got[2].path, "C:\\Datos\\a.bin");

        let id2 = {
            let w = store.open_writer("C:\\Datos", 1_700_086_400).expect("w");
            w.finish(ScanTotals::default(), 0).expect("finish")
        };
        assert!(store.load_top_files(id2).expect("load vacío").is_empty());
        store.prune("C:\\Datos", 1).expect("prune");
        assert!(store.load_top_files(id).expect("prune cascada").is_empty());
    }

    #[test]
    fn top_dirs_ordered_desc_excluding_root() {
        let (_tmp, mut store) = open_tmp();
        let id = {
            let mut w = store.open_writer("C:\\Base", 1_000).expect("w");
            write_all(
                &mut w,
                &[
                    ("C:\\Base".into(), 999, 0, 5),
                    ("C:\\Base\\Grande".into(), 500, 0, 10),
                    ("C:\\Base\\Medio".into(), 200, 0, 3),
                    ("C:\\Base\\Pequeno".into(), 30, 0, 1),
                ],
            );
            w.finish(ScanTotals::default(), 0).expect("finish")
        };

        let top = store.load_top_dirs(id, 50).expect("top dirs");
        assert_eq!(top.len(), 3, "la raíz se excluye del top");
        assert_eq!(top[0].path, "C:\\Base\\Grande");
        assert_eq!(top[0].size_bytes, 500);
        assert_eq!(top[0].files, 10);
        assert_eq!(top[1].path, "C:\\Base\\Medio");
        assert_eq!(top[2].path, "C:\\Base\\Pequeno");

        // El límite recorta desde el mayor.
        let limited = store.load_top_dirs(id, 2).expect("limit");
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].size_bytes, 500);
        assert_eq!(limited[1].size_bytes, 200);

        // Snapshot sin hijos (solo la raíz): lista vacía, sin error.
        let bare = {
            let w = store.open_writer("C:\\Vacio", 2_000).expect("w");
            w.finish(ScanTotals::default(), 0).expect("finish")
        };
        assert!(store
            .load_top_dirs(bare, 50)
            .expect("vacío es válido")
            .is_empty());
    }
}
