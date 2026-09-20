/**
 * Frontend de disky (shell TypeScript vanilla).
 *
 * Convención: todo acceso al backend Rust pasa por `invoke` con los tipos de
 * `api.ts`; este archivo solo orquesta DOM y escapa todo lo que venga del
 * sistema (los nombres de archivo son entrada no confiable).
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  GrowthDiff,
  GrowthReport,
  JournalRecord,
  ScanDonePayload,
  ScanProgress,
  ScanQuickDonePayload,
  SnapshotSummary,
  UsnStatus,
  Volume,
} from "./api";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;
let volumesEl: HTMLElement | null;
let usnLetterEl: HTMLInputElement | null;
let usnStatusEl: HTMLElement | null;
let usnRecordsEl: HTMLElement | null;
let scanRootEl: HTMLInputElement | null;
let scanBtnEl: HTMLButtonElement | null;
let scanQuickBtnEl: HTMLButtonElement | null;
let scanCancelBtnEl: HTMLButtonElement | null;
let scanProgressEl: HTMLElement | null;
let scanProgressTextEl: HTMLElement | null;
let snapshotsEl: HTMLElement | null;
let growthEl: HTMLElement | null;

/** Último diff recibido del backend; alimenta el drill-down. */
let lastGrowth: GrowthDiff | null = null;
/** Carpeta en la que se hizo drill-down (null = vista completa). */
let drillPath: string | null = null;

/** Escapa texto arbitrario para insertarlo en HTML de forma segura. */
function escapeHtml(text: string): string {
  const map: Record<string, string> = {
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  };
  return text.replace(/[&<>"']/g, (ch) => map[ch] ?? ch);
}

function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

// ── Volúmenes ────────────────────────────────────────────────────────────────

function volumeRow(volume: Volume): string {
  const used = formatBytes(volume.total_bytes - volume.free_bytes);
  const total = formatBytes(volume.total_bytes);
  const pct =
    volume.total_bytes > 0
      ? (((volume.total_bytes - volume.free_bytes) / volume.total_bytes) * 100).toFixed(1)
      : "—";
  return `<tr>
    <td class="letter">${escapeHtml(volume.letter)}</td>
    <td>${escapeHtml(volume.kind)}</td>
    <td>${escapeHtml(volume.label ?? "")}</td>
    <td class="num">${used} / ${total}</td>
    <td class="num">${pct}%</td>
  </tr>`;
}

async function loadVolumes(): Promise<void> {
  if (!volumesEl) return;
  try {
    const volumes = await invoke<Volume[]>("list_volumes");
    volumesEl.innerHTML =
      volumes.length > 0
        ? volumes.map(volumeRow).join("")
        : `<tr><td colspan="5">No se detectaron volúmenes</td></tr>`;
  } catch (err) {
    volumesEl.innerHTML = `<tr><td colspan="5" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

// ── Spike: USN Journal ───────────────────────────────────────────────────────

function currentLetter(): string {
  return usnLetterEl?.value.trim() || "C";
}

function renderUsnStatus(status: UsnStatus): string {
  return `
    <dl class="usn-status">
      <div><dt>Journal ID</dt><dd>${status.journal_id}</dd></div>
      <div><dt>NextUsn</dt><dd>${status.next_usn}</dd></div>
      <div><dt>FirstUsn</dt><dd>${status.first_usn}</dd></div>
      <div><dt>Tamaño máx.</dt><dd>${formatBytes(status.max_size)}</dd></div>
    </dl>`;
}

async function loadUsnStatus(): Promise<void> {
  if (!usnStatusEl) return;
  try {
    const status = await invoke<UsnStatus>("usn_status", { letter: currentLetter() });
    usnStatusEl.innerHTML = renderUsnStatus(status);
  } catch (err) {
    usnStatusEl.innerHTML = `<span class="error">${escapeHtml(String(err))}</span>`;
  }
}

function recordRow(record: JournalRecord): string {
  const when =
    record.timestamp_unix > 0
      ? new Date(record.timestamp_unix * 1000).toLocaleTimeString()
      : "—";
  return `<tr>
    <td>${escapeHtml(record.file_name)}</td>
    <td>${record.reason_labels.map(escapeHtml).join(", ")}</td>
    <td>${when}</td>
    <td class="num">${record.frn.toString(16)}</td>
  </tr>`;
}

async function loadUsnRecords(): Promise<void> {
  if (!usnRecordsEl) return;
  try {
    const records = await invoke<JournalRecord[]>("usn_recent", {
      letter: currentLetter(),
      maxRecords: 15,
    });
    usnRecordsEl.innerHTML =
      records.length > 0
        ? records.map(recordRow).join("")
        : `<tr><td colspan="4">Sin registros recientes: el journal está al día</td></tr>`;
  } catch (err) {
    usnRecordsEl.innerHTML = `<tr><td colspan="4" class="error">${escapeHtml(String(err))}</td></tr>`;
  }
}

// ── Escaneo sin admin ──────────────────────────────────────────────────

function currentScanRoot(): string {
  return scanRootEl?.value.trim() || "C:\\";
}

function setScanBusy(busy: boolean, progressText = "Preparando…"): void {
  if (scanBtnEl) scanBtnEl.disabled = busy;
  if (scanQuickBtnEl) scanQuickBtnEl.disabled = busy;
  if (scanCancelBtnEl) scanCancelBtnEl.disabled = !busy;
  if (scanProgressEl) scanProgressEl.classList.toggle("hidden", !busy);
  if (scanProgressTextEl) scanProgressTextEl.textContent = progressText;
}

/** Muestra un error del escaneo en la zona de progreso, con auto-ocultado. */
function showScanError(message: string): void {
  if (!scanProgressEl || !scanProgressTextEl) return;
  scanProgressEl.classList.remove("hidden");
  scanProgressTextEl.innerHTML = `<span class="error">${escapeHtml(message)}</span>`;
  window.setTimeout(() => scanProgressEl?.classList.add("hidden"), 4000);
}

function formatDelta(bytes: number): string {
  const sign = bytes > 0 ? "+" : bytes < 0 ? "−" : "";
  return `${sign}${formatBytes(Math.abs(bytes))}`;
}

function snapshotRow(snapshot: SnapshotSummary): string {
  const when = new Date(snapshot.started_at * 1000).toLocaleString();
  const seconds = Math.round(snapshot.duration_ms / 1000);
  return `<tr>
    <td>${escapeHtml(snapshot.root)}</td>
    <td>${when} (${seconds}s)</td>
    <td class="num">${formatBytes(snapshot.total_bytes)}</td>
    <td class="num">${snapshot.total_files.toLocaleString()}</td>
    <td class="num">${snapshot.read_errors.toLocaleString()}</td>
  </tr>`;
}

function growthRow(report: GrowthReport): string {
  const cls = report.delta_bytes > 0 ? "delta-pos" : report.delta_bytes < 0 ? "delta-neg" : "";
  const perDay = report.delta_bytes / Math.max(report.elapsed_seconds, 1) * 86_400;
  return `<tr class="clickable" data-path="${escapeHtml(report.path)}">
    <td class="path">${escapeHtml(report.path)}</td>
    <td class="num">${formatBytes(report.old_bytes)}</td>
    <td class="num">${formatBytes(report.new_bytes)}</td>
    <td class="num ${cls}">${formatDelta(report.delta_bytes)}</td>
    <td class="num ${cls}">${formatDelta(Math.round(perDay))}</td>
  </tr>`;
}

async function loadSnapshots(): Promise<void> {
  if (!snapshotsEl) return;
  try {
    const snapshots = await invoke<SnapshotSummary[]>("snapshots_list");
    snapshotsEl.innerHTML =
      snapshots.length > 0
        ? snapshots.map(snapshotRow).join("")
        : `<tr><td colspan="5">Aún no hay escaneos guardados</td></tr>`;
  } catch (err) {
    snapshotsEl.innerHTML = `<tr><td colspan="5" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

/** Etiqueta del período comparado del último diff. */
function growthPeriodLabel(): string {
  if (!lastGrowth) return "";
  const since = new Date(lastGrowth.old.started_at * 1000).toLocaleString();
  const until = new Date(lastGrowth.new.started_at * 1000).toLocaleString();
  return `Entre ${escapeHtml(since)} y ${escapeHtml(until)}`;
}

/** Hijos directos de `folder` presentes en el diff (una sola profundidad). */
function childrenOf(folder: string): GrowthReport[] {
  if (!lastGrowth) return [];
  const prefix = `${folder}\\`;
  return lastGrowth.rows.filter((row) => {
    if (!row.path.startsWith(prefix)) return false;
    return !row.path.slice(prefix.length).includes("\\");
  });
}

/** Pinta la tabla de crecimiento: vista completa o drill-down. */
function renderGrowth(): void {
  if (!growthEl) return;
  if (!lastGrowth) {
    growthEl.innerHTML = `<tr><td colspan="5">Requiere dos escaneos de la misma raíz</td></tr>`;
    return;
  }
  const period = growthPeriodLabel();

  if (drillPath) {
    const children = childrenOf(drillPath);
    const name = escapeHtml(drillPath);
    growthEl.innerHTML =
      `<tr class="period"><td colspan="5">${period} · dentro de <strong>${name}</strong> · <a href="#" id="growth-back">↑ volver</a></td></tr>` +
      (children.length > 0
        ? children.map(growthRow).join("")
        : `<tr><td colspan="5">Sin subcarpetas con cambios medidos aquí</td></tr>`);
    document.querySelector("#growth-back")?.addEventListener("click", (e) => {
      e.preventDefault();
      drillPath = null;
      renderGrowth();
    });
    return;
  }

  growthEl.innerHTML =
    lastGrowth.rows.length > 0
      ? `<tr class="period"><td colspan="5">${period} · clic en una carpeta para ver sus hijos</td></tr>` +
        lastGrowth.rows.map(growthRow).join("")
      : `<tr><td colspan="5">Sin diferencias de tamaño entre los dos escaneos</td></tr>`;
}

async function loadGrowth(): Promise<void> {
  if (!growthEl) return;
  try {
    const diff = await invoke<GrowthDiff | null>("growth_report", {
      root: currentScanRoot(),
    });
    lastGrowth = diff;
    drillPath = null;
    renderGrowth();
  } catch (err) {
    growthEl.innerHTML = `<tr><td colspan="5" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

async function startScan(): Promise<void> {
  try {
    await invoke("scan_start", { root: currentScanRoot() });
    setScanBusy(true);
  } catch (err) {
    showScanError(String(err));
  }
}

/** Escaneo con UAC: sin progreso ni cancelación, resuelve carpetas protegidas. */
async function startQuickScan(): Promise<void> {
  try {
    await invoke("scan_quick_start", { root: currentScanRoot() });
    setScanBusy(true, "Escaneo elevado en curso — confirma el diálogo de UAC…");
  } catch (err) {
    showScanError(String(err));
  }
}

async function cancelScan(): Promise<void> {
  try {
    await invoke("scan_cancel");
  } catch (err) {
    if (scanProgressTextEl) scanProgressTextEl.textContent = String(err);
  }
}

function handleScanDone(payload: ScanDonePayload): void {
  setScanBusy(false);
  if (payload.error) {
    showScanError(payload.error);
    return;
  }
  void loadSnapshots();
  void loadGrowth();
}

/** Resultado del escaneo elevado (mismo flujo de refresco que el normal). */
function handleQuickScanDone(payload: ScanQuickDonePayload): void {
  setScanBusy(false);
  if (payload.error) {
    showScanError(payload.error);
    return;
  }
  void loadSnapshots();
  void loadGrowth();
}

// ── Arranque ─────────────────────────────────────────────────────────────────

async function greet(): Promise<void> {
  if (greetMsgEl && greetInputEl) {
    greetMsgEl.textContent = await invoke<string>("greet", {
      name: greetInputEl.value,
    });
  }
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  volumesEl = document.querySelector("#volumes");
  usnLetterEl = document.querySelector("#usn-letter");
  usnStatusEl = document.querySelector("#usn-status");
  usnRecordsEl = document.querySelector("#usn-records");
  scanRootEl = document.querySelector("#scan-root");
  scanBtnEl = document.querySelector("#scan-btn");
  scanQuickBtnEl = document.querySelector("#scan-quick-btn");
  scanCancelBtnEl = document.querySelector("#scan-cancel-btn");
  scanProgressEl = document.querySelector("#scan-progress");
  scanProgressTextEl = document.querySelector("#scan-progress-text");
  snapshotsEl = document.querySelector("#snapshots-table tbody");
  growthEl = document.querySelector("#growth-table tbody");

  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    void greet();
  });
  document.querySelector("#usn-status-btn")?.addEventListener("click", () => {
    void loadUsnStatus();
  });
  document.querySelector("#usn-recent-btn")?.addEventListener("click", () => {
    void loadUsnRecords();
  });
  scanBtnEl?.addEventListener("click", () => void startScan());
  scanQuickBtnEl?.addEventListener("click", () => void startQuickScan());
  scanCancelBtnEl?.addEventListener("click", () => void cancelScan());

  // Delegación de clics para el drill-down (las filas se recrean a menudo).
  growthEl?.addEventListener("click", (event) => {
    const row = (event.target as HTMLElement).closest("tr[data-path]");
    if (!row) return;
    drillPath = row.getAttribute("data-path");
    renderGrowth();
  });

  void listen<ScanProgress>("scan-progress", (event) => {
    const p = event.payload;
    if (scanProgressTextEl) {
      scanProgressTextEl.textContent = `${p.files.toLocaleString()} archivos · ${p.dirs.toLocaleString()} carpetas · ${formatBytes(p.bytes)} · ${p.read_errors.toLocaleString()} errores de lectura`;
    }
  });
  void listen<ScanDonePayload>("scan-done", (event) => {
    handleScanDone(event.payload);
  });
  void listen<ScanQuickDonePayload>("scan-quick-done", (event) => {
    handleQuickScanDone(event.payload);
  });

  void loadVolumes();
  void loadSnapshots();
});
