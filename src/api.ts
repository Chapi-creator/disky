/** Tipos compartidos con el backend Rust.
 *
 * Deben reflejar los structs serde de `disky-core`; el dominio Rust es la
 * única fuente de verdad. Los u64 llegan como number de JS (precisión de
 * 2^53; suficiente para IDs y tamaños de este proyecto).
 */

/** Un volumen montado con letra de unidad. */
export interface Volume {
  letter: string;
  label: string | null;
  total_bytes: number;
  free_bytes: number;
  kind: DriveKind;
}

export type DriveKind =
  | "fixed"
  | "removable"
  | "remote"
  | "cd_rom"
  | "ram_disk"
  | "unknown";

/** Estado del USN Journal de un volumen. */
export interface UsnStatus {
  journal_id: number;
  next_usn: number;
  first_usn: number;
  max_usn: number;
  max_size: number;
}

/** Un registro del USN Journal ya parseado. */
export interface JournalRecord {
  frn: number;
  parent_frn: number;
  usn: number;
  timestamp_unix: number;
  reasons: number;
  reason_labels: string[];
  file_name: string;
}

/** Progreso periódico de un escaneo (evento `scan-progress`). */
export interface ScanProgress {
  files: number;
  dirs: number;
  bytes: number;
  read_errors: number;
}

/** Resumen de un snapshot guardado en SQLite. */
export interface SnapshotSummary {
  id: number;
  root: string;
  started_at: number;
  duration_ms: number;
  total_files: number;
  total_bytes: number;
  read_errors: number;
}

/** Crecimiento de una carpeta entre dos snapshots. */
export interface GrowthReport {
  path: string;
  old_bytes: number;
  new_bytes: number;
  delta_bytes: number;
  elapsed_seconds: number;
}

/** Comparación de los dos snapshots más recientes de una raíz. */
export interface GrowthDiff {
  old: SnapshotSummary;
  new: SnapshotSummary;
  rows: GrowthReport[];
}

/** Payload del evento `scan-done`. */
export interface ScanDonePayload {
  snapshot: SnapshotSummary | null;
  growth: GrowthDiff | null;
  error: string | null;
}

/** Payload del evento `scan-quick-done` (escaneo elevado con UAC). */
export interface ScanQuickDonePayload {
  snapshot: SnapshotSummary | null;
  growth: GrowthDiff | null;
  error: string | null;
}
