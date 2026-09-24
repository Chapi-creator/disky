//! # disky-core
//!
//! Núcleo de **disky** — *"¿qué creció en mi disco?"*.
//!
//! Arquitectura hexagonal: este crate no conoce UI ni IPC.
//!
//! - [`domain`]: tipos del dominio y el motor de crecimiento (funciones puras, 100% testeable).
//! - [`platform`]: adaptador de plataforma (enumera volúmenes reales vía Windows APIs).
//!
//! El shell de Tauri (`src-tauri`) solo traduce comandos IPC hacia aquí, de modo que
//! el núcleo pueda moverse a CLI, otro backend de UI o tests sin cambios.

pub mod domain;
pub mod platform;

pub use domain::scan::{
    match_by_path, DirStat, DirWriter, LargestDir, LargestFile, ScanProgress, ScanTotals,
    SeriesPoint, SnapshotStore, SnapshotSummary, StoreError,
};
pub use domain::treemap::{squarify, TreemapItem, TreemapNode};
pub use domain::usn::JournalRecord;
pub use domain::{growth_between, growth_ranking, DriveKind, GrowthReport, UsageSample, Volume};
pub use platform::sqlite::{truncate_wal, SqliteStore};
pub use platform::mft::{mft_available, mft_scan, MftError};
pub use platform::usn::{journal_status, recent_records, UsnStatus};
pub use platform::walk::{walk_tree, WalkError};
pub use platform::{list_volumes, PlatformError};
