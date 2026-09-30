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
    match_by_path, DirStat, DirWriter, GrowthTop, LargestDir, LargestFile, ScanProgress,
    ScanTotals, SeriesPoint, SnapshotStore, SnapshotSummary, StoreError, BIG_FILE_MAX,
};
pub use domain::treemap::{squarify, TreemapItem, TreemapNode};
pub use domain::usn::{JournalChange, JournalRecord};
pub use domain::{
    duplicate_groups, growth_between, growth_ranking, DriveKind, DuplicateGroup, GrowthReport,
    UsageSample, Volume,
};
pub use platform::mft::{mft_available, mft_scan, resolve_paths, MftError};
pub use platform::sqlite::{truncate_wal, SqliteStore};
pub use platform::usn::{
    diag_usn_follow, diag_usn_touch, diag_usn_variants, journal_status, recent_changes,
    recent_records, ChangesError, UsnStatus,
};
pub use platform::walk::{walk_tree, WalkError};
pub use platform::{fixed_volume_roots, list_volumes, PlatformError};
