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

/** Un archivo individual por peso (top-N del escaneo). */
export interface LargestFile {
  path: string;
  size_bytes: number;
  mtime_unix: number;
}

/** Una carpeta por tamaño (roll-up de su subárbol, top-N del snapshot). */
export interface LargestDir {
  path: string;
  size_bytes: number;
  files: number;
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
  largest: LargestFile[];
  error: string | null;
}

/** Payload del evento `scan-quick-done` (escaneo elevado con UAC). */
export interface ScanQuickDonePayload {
  snapshot: SnapshotSummary | null;
  growth: GrowthDiff | null;
  largest: LargestFile[];
  error: string | null;
}

/** Unidad en curso dentro de un "Escanear todo". */
export interface ScanAllUnit {
  letter: string;
  index: number;
  total: number;
}

/** Un rectángulo del treemap listo para pintar en el SVG. */
export interface TreemapNodeDto {
  path: string;
  name: string;
  x: number;
  y: number;
  w: number;
  h: number;
  size_bytes: number;
  delta_bytes: number;
  is_files: boolean;
}

/** Un punto del timeline de una carpeta. */
export interface TimelinePointDto {
  measured_at: number;
  size_bytes: number;
  delta_bytes: number;
}

/**
 * Grupo de archivos grandes candidatos a duplicado: comparten nombre y tamaño
 * exactos. Es un heurístico de criba, no un hash, así que puede haber falsos
 * positivos (por eso la UI lo etiqueta como «probable»).
 */
export interface DuplicateGroup {
  name: string;
  size_bytes: number;
  paths: string[];
}
