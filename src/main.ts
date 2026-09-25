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
  LargestDir,
  LargestFile,
  ScanAllUnit,
  ScanDonePayload,
  ScanProgress,
  ScanQuickDonePayload,
  SnapshotSummary,
  TimelinePointDto,
  TreemapNodeDto,
  Volume,
} from "./api";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;
let volumesEl: HTMLElement | null;
let scanRootEl: HTMLInputElement | null;
let scanBtnEl: HTMLButtonElement | null;
let scanQuickBtnEl: HTMLButtonElement | null;
let scanCancelBtnEl: HTMLButtonElement | null;
let scanProgressEl: HTMLElement | null;
let scanProgressTextEl: HTMLElement | null;
let snapshotsEl: HTMLElement | null;
let historyRootEl: HTMLSelectElement | null;
let historyGroupEl: HTMLSelectElement | null;
let historyEl: HTMLElement | null;
let growthEl: HTMLElement | null;
let growthThresholdEl: HTMLInputElement | null;
let largestEl: HTMLElement | null;
let largestDirsEl: HTMLElement | null;

/** `true` mientras un "Escanear todo" está en curso. */
let scanAllActive = false;

/** Botón del escaneo de todas las unidades. */
let scanAllBtnEl: HTMLButtonElement | null;

/**
 * Escaneo de todas las unidades fijas: una a la vez, refrescando lo
 * acumulado tras cada una sin cerrar el estado de progreso.
 */
async function startScanAll(): Promise<void> {
  if (scanAllActive) return;
  try {
    await invoke("scan_all_start");
    scanAllActive = true;
    if (scanBtnEl) scanBtnEl.disabled = true;
    if (scanQuickBtnEl) scanQuickBtnEl.disabled = true;
    if (scanAllBtnEl) scanAllBtnEl.disabled = true;
    if (scanCancelBtnEl) scanCancelBtnEl.disabled = false;
    if (scanProgressEl) scanProgressEl.classList.remove("hidden");
    if (scanProgressTextEl) scanProgressTextEl.textContent = "Preparando el barrido de unidades…";
  } catch (err) {
    showScanError(String(err));
  }
}

/** Como `handleScanDone`, pero manteniendo el estado ocupado del lote. */
function handleScanDoneKeepBusy(payload: ScanDonePayload): void {
  if (payload.snapshot) {
    refreshData(payload.snapshot.root);
  }
  // Los errores de unidad se muestran sin abortar el resto del lote.
  if (payload.error && scanProgressTextEl && !payload.error.includes("cancelado")) {
    scanProgressTextEl.textContent = `${payload.error} — continuando con la siguiente unidad…`;
  }
}

/** Último diff recibido del backend; alimenta el drill-down. */
let lastGrowth: GrowthDiff | null = null;
/** Carpeta en la que se hizo drill-down (null = vista completa). */
let drillPath: string | null = null;

/** Raíz del treemap actual y pila de navegación (breadcrumb). */
let treemapRoot = "";
let treemapCrumb: string[] = [];
let treemapEl: SVGElement | null;
let treemapCrumbEl: HTMLElement | null;
let timelineEl: SVGElement | null;
let timelineLegendEl: HTMLElement | null;

/** Nodos del último treemap cargado: alimentan la comparación multi-línea. */
let lastTreemapNodes: TreemapNodeDto[] = [];

/** Tokens de secuencia: la respuesta a un drill-down viejo no pinta sobre uno
 *  nuevo (las consultas IPC llegan fuera de orden). */
let treemapSeq = 0;
let timelineSeq = 0;

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

// ── Escaneo ─────────────────────────────────────────────────────────

function currentScanRoot(): string {
  return scanRootEl?.value.trim() || "C:\\";
}

function setScanBusy(busy: boolean, progressText = "Preparando…"): void {
  if (scanBtnEl) scanBtnEl.disabled = busy;
  if (scanQuickBtnEl) scanQuickBtnEl.disabled = busy;
  if (scanCancelBtnEl) scanCancelBtnEl.disabled = !busy;
  if (scanProgressEl) scanProgressEl.classList.toggle("hidden", !busy);
  if (scanProgressTextEl) scanProgressTextEl.textContent = progressText;
  // Un escaneo nuevo anula cualquier auto-ocultado pendiente: no ocultar el
  // panel a mitad de un barrido activo.
  if (busy) cancelHideProgress();
}

/** Timer de auto-ocultado del panel de progreso (ninguno = nunca se oculta). */
let progressHideTimer: number | null = null;

function scheduleHideProgress(delay: number): void {
  if (progressHideTimer !== null) window.clearTimeout(progressHideTimer);
  progressHideTimer = window.setTimeout(() => {
    scanProgressEl?.classList.add("hidden");
    progressHideTimer = null;
  }, delay);
}

function cancelHideProgress(): void {
  if (progressHideTimer !== null) {
    window.clearTimeout(progressHideTimer);
    progressHideTimer = null;
  }
}

/** Muestra un error del escaneo en la zona de progreso, con auto-ocultado. */
function showScanError(message: string): void {
  if (!scanProgressEl || !scanProgressTextEl) return;
  scanProgressEl.classList.remove("hidden");
  scanProgressTextEl.innerHTML = `<span class="error">${escapeHtml(message)}</span>`;
  scheduleHideProgress(4000);
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
    <td><button type="button" class="row-action" data-delete-snapshot="${snapshot.id}" title="Borrar este escaneo">🗑</button></td>
  </tr>`;
}

/** Umbral de alerta de crecimiento en bytes (editable en la UI). */
function growthAlertThresholdBytes(): number | null {
  if (!growthThresholdEl) return null;
  return Math.max(0, Number(growthThresholdEl.value) || 0) * 1024 * 1024;
}

function growthRow(report: GrowthReport): string {
  const cls = report.delta_bytes > 0 ? "delta-pos" : report.delta_bytes < 0 ? "delta-neg" : "";
  const perDay = report.delta_bytes / Math.max(report.elapsed_seconds, 1) * 86_400;
  const threshold = growthAlertThresholdBytes();
  const alert =
    threshold !== null &&
    report.delta_bytes >= threshold &&
    (report.delta_bytes > 0 || report.new_bytes > 0)
      ? ' class="grow-alert"'
      : "";
  return `<tr${alert} class="clickable" data-path="${escapeHtml(report.path)}">
    <td class="path">${escapeHtml(report.path)}</td>
    <td class="num">${formatBytes(report.old_bytes)}</td>
    <td class="num">${formatBytes(report.new_bytes)}</td>
    <td class="num ${cls}">${formatDelta(report.delta_bytes)}</td>
    <td class="num ${cls}">${formatDelta(Math.round(perDay))}</td>
  </tr>`;
}

function largestRow(file: LargestFile, index: number): string {
  const when =
    file.mtime_unix > 0
      ? new Date(file.mtime_unix * 1000).toLocaleDateString()
      : "—";
  return `<tr>
    <td class="num">${index + 1}</td>
    <td class="path">${escapeHtml(file.path)}</td>
    <td class="num">${formatBytes(file.size_bytes)}</td>
    <td class="num">${escapeHtml(when)}</td>
  </tr>`;
}

function largestDirsRow(dir: LargestDir, index: number): string {
  return `<tr>
    <td class="num">${index + 1}</td>
    <td class="path">${escapeHtml(dir.path)}</td>
    <td class="num">${formatBytes(dir.size_bytes)}</td>
    <td class="num">${dir.files.toLocaleString()}</td>
  </tr>`;
}

/** Vacía la tabla si no queda nada pintable. */
function emptyTable(el: HTMLElement | null, cols: number, msg: string): void {
  if (el) el.innerHTML = `<tr><td colspan="${cols}">${msg}</td></tr>`;
}

/** Carga los archivos más pesados del último snapshot de la raíz actual. */
async function loadLargest(): Promise<void> {
  const el = largestEl;
  if (!el) return;
  try {
    const snaps = await invoke<SnapshotSummary[]>("snapshots_list", {
      root: currentScanRoot(),
    });
    const latest = snaps[0];
    if (!latest) {
      el.innerHTML = `<tr><td colspan="4">Aún no hay escaneos guardados</td></tr>`;
      return;
    }
    const files = await invoke<LargestFile[]>("largest_files", {
      snapshotId: latest.id,
    });
    el.innerHTML =
      files.length > 0
        ? files.map(largestRow).join("")
        : `<tr><td colspan="4">Este snapshot no registró archivos pesados</td></tr>`;
  } catch (err) {
    el.innerHTML = `<tr><td colspan="4" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

/** Carga las carpetas más pesadas del último snapshot de la raíz actual. */
async function loadLargestDirs(): Promise<void> {
  const el = largestDirsEl;
  if (!el) return;
  try {
    const snaps = await invoke<SnapshotSummary[]>("snapshots_list", {
      root: currentScanRoot(),
    });
    const latest = snaps[0];
    if (!latest) {
      el.innerHTML = `<tr><td colspan="4">Aún no hay escaneos guardados</td></tr>`;
      return;
    }
    const dirs = await invoke<LargestDir[]>("largest_dirs", {
      snapshotId: latest.id,
    });
    el.innerHTML =
      dirs.length > 0
        ? dirs.map(largestDirsRow).join("")
        : `<tr><td colspan="4">Este snapshot no registró carpetas pesadas</td></tr>`;
  } catch (err) {
    el.innerHTML = `<tr><td colspan="4" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

async function loadSnapshots(): Promise<void> {
  if (!snapshotsEl) return;
  try {
    const snapshots = await invoke<SnapshotSummary[]>("snapshots_list");
    snapshotsEl.innerHTML =
      snapshots.length > 0
        ? snapshots.map(snapshotRow).join("")
        : `<tr><td colspan="6">Aún no hay escaneos guardados</td></tr>`;
  } catch (err) {
    snapshotsEl.innerHTML = `<tr><td colspan="6" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

// ── Historial (cómo cambió el almacenamiento en el tiempo) ───────────────────

/** Snapshots del historial: cache para re-dibujar al cambiar de grupo. */
let historySnapshots: SnapshotSummary[] = [];

/** Raíz seleccionada en el historial. */
let historyRoot = "";

/** Etiqueta de período para `day`/`month`/`year` (es-es). */
function periodLabel(group: string, ts: number): string {
  const d = new Date(ts * 1000);
  return new Intl.DateTimeFormat("es", {
    ...(group === "day" ? { day: "2-digit", month: "short", year: "numeric" } : {}),
    ...(group === "month" ? { month: "long", year: "numeric" } : {}),
    ...(group === "year" ? { year: "numeric" } : {}),
  }).format(d);
}

/**
 * Agrupa los snapshots por período (día, mes, año) y conserva el **último** de
 * cada período, en orden cronológico. `all` devuelve todos.
 */
function bucketSnapshots(group: string, snaps: SnapshotSummary[]): SnapshotSummary[] {
  if (group === "all") return snaps;
  const keyOf = (ts: number): string => periodLabel(group, ts);
  const last: Map<string, SnapshotSummary> = new Map();
  for (const s of snaps) last.set(keyOf(s.started_at), s);
  return [...last.values()].sort((a, b) => a.started_at - b.started_at);
}

/** Fila del historial: un punto de la serie (cada escaneo o período). */
function historyRow(point: SnapshotSummary, prev: SnapshotSummary | null, isBase: boolean): string {
  const when = new Date(point.started_at * 1000).toLocaleString();
  const seconds = Math.round(point.duration_ms / 1000);
  const delta = prev
    ? point.total_bytes - prev.total_bytes
    : 0;
  const elapsed = prev ? Math.max(point.started_at - prev.started_at, 1) : 1;
  const perDay = Math.round((delta / elapsed) * 86_400);
  const vsBase = point.total_bytes - (baseBytes);
  const cls = delta > 0 ? "delta-pos" : delta < 0 ? "delta-neg" : "";
  const baseBadge = isBase ? ' <span class="badge-base">base</span>' : "";
  return `<tr>
    <td>${when} (${seconds}s)${baseBadge}</td>
    <td class="num">${formatBytes(point.total_bytes)}</td>
    <td class="num ${cls}">${formatDelta(delta)}</td>
    <td class="num ${cls}">${formatDelta(perDay)}</td>
    <td class="num ${vsBase > 0 ? "delta-pos" : vsBase < 0 ? "delta-neg" : ""}">${formatDelta(vsBase)}</td>
    <td class="num">${point.total_files.toLocaleString()}</td>
  </tr>`;
}

/** Bytes de la línea base (primer snapshot) de la raíz del historial. */
let baseBytes = 0;

/** Carga raíces con snapshots y pinta el historial de la raíz elegida. */
async function loadHistory(): Promise<void> {
  if (!historyEl) return;
  try {
    historySnapshots = await invoke<SnapshotSummary[]>("snapshots_list");
    const roots = [...new Set(historySnapshots.map((s) => s.root))].sort();
    if (!roots.includes(historyRoot)) historyRoot = roots[0] ?? "";
    if (historyRootEl) {
      const selected = historyRootEl.value;
      historyRootEl.innerHTML = roots
        .map((r) => `<option value="${escapeHtml(r)}">${escapeHtml(r)}</option>`)
        .join("");
      historyRootEl.disabled = roots.length === 0;
      if (roots.includes(selected)) historyRoot = selected;
      historyRootEl.value = historyRoot;
    }
    renderHistory();
  } catch (err) {
    historyEl.innerHTML = `<tr><td colspan="6" class="error">Error: ${escapeHtml(String(err))}</td></tr>`;
  }
}

/** Re-dibuja el historial con la raíz y grupo actuales (basado en cache). */
function renderHistory(): void {
  if (!historyEl) return;
  const group = historyGroupEl?.value ?? "all";
  const snaps = bucketSnapshots(
    group,
    historySnapshots
      .filter((s) => s.root === historyRoot)
      .sort((a, b) => a.started_at - b.started_at),
  );
  if (snaps.length === 0) {
    historyEl.innerHTML = `<tr><td colspan="6">Aún no hay escaneos de esta raíz</td></tr>`;
    return;
  }
  baseBytes = snaps[0].total_bytes;
  historyEl.innerHTML =
    `<tr class="period"><td colspan="6">Línea base: ${escapeHtml(formatBytes(baseBytes))} · agrupación: ${escapeHtml(group)}</td></tr>` +
    snaps.map((s, i) => historyRow(s, i > 0 ? snaps[i - 1] : null, i === 0)).join("");
}

/** `true` mientras ya se disparó el escaneo de línea base del primer uso. */
let baselineTriggered = false;

/**
 * Primer uso (BD sin snapshots): arranca el "Escanear todo" para que exista la
 * línea base desde la que se comparará todo lo demás.
 */
async function ensureBaseline(): Promise<void> {
  if (baselineTriggered) return;
  baselineTriggered = true; // evita arranques simultáneos
  try {
    const snaps = await invoke<SnapshotSummary[]>("snapshots_list");
    if (snaps.length === 0) void startScanAll();
  } catch (_err) {
    // Fallo transitorio (BD ocupada en el arranque): reintentar una vez en
    // vez de dejar el primer uso sin línea base para siempre.
    baselineTriggered = false;
    window.setTimeout(() => void ensureBaseline(), 1500);
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
  // El backend guarda rutas con el separador nativo del SO.
  const sep = navigator.platform.startsWith("Win") ? "\\" : "/";
  const prefix = `${folder}${sep}`;
  return lastGrowth.rows.filter((row) => {
    if (!row.path.startsWith(prefix)) return false;
    return !row.path.slice(prefix.length).includes(sep);
  });
}

/** Pinta la tabla de crecimiento: vista completa o drill-down. */
function growthTop3(): string {
  if (!lastGrowth) return "";
  const top = [...lastGrowth.rows]
    .sort((a, b) => b.delta_bytes - a.delta_bytes)
    .slice(0, 3)
    .filter((r) => r.delta_bytes > 0);
  if (top.length === 0) return "";
  return " · <strong>Más crecieron:</strong> " +
    top.map((r) => `${escapeHtml(r.path)} (${formatDelta(r.delta_bytes)})`).join(", ");
}

function renderGrowth(): void {
  if (!growthEl) return;
  if (!lastGrowth) {
    emptyTable(growthEl, 5, "Requiere dos escaneos de la misma raíz");
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
      ? `<tr class="period"><td colspan="5">${period}${growthTop3()} · clic en una carpeta para ver sus hijos</td></tr>` +
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

/** Refresco común tras un escaneo: historial, ¿qué creció? y treemap/timeline. */
function refreshData(root: string): void {
  void loadSnapshots();
  void loadHistory();
  void loadGrowth();
  void loadLargest();
  void loadLargestDirs();
  void refreshTreemapForRoot(root);
}

function handleScanDone(payload: ScanDonePayload): void {
  setScanBusy(false);
  if (payload.error) {
    showScanError(payload.error);
    return;
  }
  // La raíz del snapshot, no el input vivo: si el campo cambió mientras el
  // escaneo corría, refrescar con él pintaría datos de otra raíz.
  refreshData(payload.snapshot?.root ?? currentScanRoot());
}

/** Resultado del escaneo elevado: refresco idéntico al normal. */
function handleQuickScanDone(payload: ScanQuickDonePayload): void {
  setScanBusy(false);
  if (payload.error) {
    showScanError(payload.error);
    return;
  }
  refreshData(payload.snapshot?.root ?? currentScanRoot());
}

// ── Treemap ─────────────────────────────────────────────────────────────

/** Color del rectángulo según el delta: verde creció, naranja encogió, gris neutro. */
function treemapFill(delta: number): string {
  if (delta > 0) return "#3f9d63";
  if (delta < 0) return "#c47f2e";
  return "#3a4152";
}

/** Etiqueta visible del nodo (recortada si el rectángulo es angosto). */
function nodeLabel(node: TreemapNodeDto): string {
  const chars = Math.max(0, Math.floor(node.w / 7));
  return node.name.length > chars ? `${node.name.slice(0, Math.max(chars - 1, 1))}…` : node.name;
}

async function loadTreemap(folder?: string): Promise<void> {
  if (!treemapEl) return;
  const seq = ++treemapSeq;
  try {
    const nodes = await invoke<TreemapNodeDto[]>("treemap_nodes", {
      root: treemapRoot,
      folder: folder ?? null,
    });
    if (seq !== treemapSeq) return; // un drill anterior pidió después: descartar
    treemapEl.innerHTML = nodes
      .map(
        (node) => `
      <g class="tm-node" data-path="${escapeHtml(node.path)}">
        <rect x="${node.x + 1}" y="${node.y + 1}" width="${Math.max(node.w - 2, 0)}" height="${Math.max(node.h - 2, 0)}"
          rx="4" fill="${treemapFill(node.delta_bytes)}" />
        <text x="${node.x + 8}" y="${node.y + 20}" fill="#e6e9ef" font-size="13">${escapeHtml(nodeLabel(node))}</text>
        <text x="${node.x + 8}" y="${node.y + 38}" fill="#9aa3b2" font-size="11">${formatBytes(node.size_bytes)}</text>
      </g>`,
      )
      .join("");
    lastTreemapNodes = nodes;
  } catch (err) {
    if (seq !== treemapSeq) return;
    treemapEl.innerHTML = `<text x="16" y="40" class="error">${escapeHtml(String(err))}</text>`;
    lastTreemapNodes = [];
  }
}

/** Pinta el breadcrumb y gestiona el clic en "volver". */
function renderCrumb(): void {
  if (!treemapCrumbEl) return;
  const parts = treemapCrumb.map(
    (crumb, i) =>
      `<a href="#" data-depth="${i}" class="crumb-link">${escapeHtml(crumb === "" ? "raíz" : crumb)}</a>`,
  );
  treemapCrumbEl.innerHTML = parts.join('<span class="crumb-sep">›</span>');
  treemapCrumbEl.querySelectorAll("a").forEach((a) => {
    a.addEventListener("click", (e) => {
      e.preventDefault();
      const depth = Number((a as HTMLAnchorElement).dataset.depth ?? 0);
      treemapCrumb = treemapCrumb.slice(0, depth + 1);
      const folder = depth === 0 ? null : treemapCrumb[depth];
      void loadTreemap(folder ?? undefined);
      void loadTimeline(folder ?? undefined);
    });
  });
}

/** Entra a una carpeta del treemap (navegación hacia abajo). */
function drillIntoTreemap(path: string): void {
  if (!path) return; // el nodo [archivos] no navega
  treemapCrumb = [...treemapCrumb, path];
  void loadTreemap(path);
  void loadTimeline(path);
}

// ── Timeline (gráfico de líneas) ─────────────────────────────────────

/** Paleta de la comparación: hasta 6 series distinguibles. */
const SERIES_COLORS = ["#4f8cff", "#3f9d63", "#c47f2e", "#b569c9", "#e0c04f", "#9aa3b2"];

/** Carpeta cuyo timeline se muestra (null = la raíz actual). */
let timelineFolder: string | null = null;

/** Etiqueta corta de una ruta para la leyenda. */
function shortName(path: string): string {
  return path.split("\\").pop() || path;
}

/**
 * Dibuja el timeline comparativo: la carpeta vista más sus hijos más grandes
 * (hasta 5), una línea por carpeta con escala temporal y de bytes comunes.
 */
async function loadTimeline(folder?: string): Promise<void> {
  const seq = ++timelineSeq;
  if (folder !== undefined) timelineFolder = folder ?? null;
  if (!timelineEl || !timelineLegendEl) return;
  const W = 1000;
  const H = 240;
  const PAD = 52;

  const empty = (msg: string): void => {
    if (timelineEl && timelineLegendEl) {
      timelineEl.innerHTML = `<text x="16" y="40" fill="#9aa3b2" font-size="13">${escapeHtml(msg)}</text>`;
      timelineLegendEl.innerHTML = "";
    }
  };

  // La carpeta vista + sus hijos directos más grandes (los del treemap ya
  // vienen ordenados por tamaño), excluyendo el nodo sintético [archivos].
  const base = timelineFolder ?? treemapRoot;
  const series: { path: string; label: string }[] = [
    { path: base, label: base ? shortName(base) : "raíz" },
    ...lastTreemapNodes
      .filter((n) => !n.is_files)
      .slice(0, 5)
      .map((n) => ({ path: n.path, label: n.name })),
  ];

  try {
    const responses = await Promise.all(
      series.map((s) =>
        invoke<TimelinePointDto[]>("timeline_series", {
          root: treemapRoot,
          folder: s.path,
          limit: 20,
        }).catch(() => [] as TimelinePointDto[]),
      ),
    );
    if (seq !== timelineSeq) return; // respuesta obsoleta de otro drill

    const valid = series
      .map((s, i) => ({ ...s, points: responses[i] }))
      .filter((s) => s.points.length >= 2);
    if (valid.length === 0) {
      empty("Necesita al menos dos escaneos de esta carpeta para dibujar las líneas");
      return;
    }

    // Escala temporal y de bytes comunes a todas las series.
    const allTimes = valid.flatMap((s) => s.points.map((p) => p.measured_at));
    const t0 = Math.min(...allTimes);
    const t1 = Math.max(...allTimes);
    const spanT = Math.max(t1 - t0, 1);
    const xOf = (t: number): number => PAD + ((t - t0) / spanT) * (W - PAD * 2);
    const allSizes = valid.flatMap((s) => s.points.map((p) => p.size_bytes));
    const min = Math.min(...allSizes);
    const max = Math.max(...allSizes);
    const spanY = Math.max(max - min, 1);
    const yOf = (v: number): number => H - PAD - ((v - min) / spanY) * (H - PAD * 2);

    const gridLines = [min, (min + max) / 2, max]
      .map(
        (v) =>
          `<line x1="${PAD}" y1="${yOf(v).toFixed(1)}" x2="${W - PAD}" y2="${yOf(v).toFixed(1)}" stroke="#262b36" stroke-dasharray="3 4" /><text x="8" y="${(yOf(v) + 4).toFixed(1)}" fill="#9aa3b2" font-size="10">${formatBytes(v)}</text>`,
      )
      .join("");

    const paths = valid
      .map((s, i) => {
        const color = SERIES_COLORS[i % SERIES_COLORS.length];
        const d = s.points
          .map((p, j) => `${j === 0 ? "M" : "L"}${xOf(p.measured_at).toFixed(1)},${yOf(p.size_bytes).toFixed(1)}`)
          .join(" ");
        const dots = s.points
          .map(
            (p) =>
              `<circle cx="${xOf(p.measured_at).toFixed(1)}" cy="${yOf(p.size_bytes).toFixed(1)}" r="3.5" fill="${color}"><title>${escapeHtml(s.label)} · ${escapeHtml(new Date(p.measured_at * 1000).toLocaleString())} · ${formatBytes(p.size_bytes)} (Δ ${formatDelta(p.delta_bytes)})</title></circle>`,
          )
          .join("");
        return `<path d="${d}" fill="none" stroke="${color}" stroke-width="2" />${dots}`;
      })
      .join("");

    timelineEl.innerHTML = gridLines + paths;
    timelineLegendEl.innerHTML = valid
      .map(
        (s, i) =>
          `<span class="legend-item"><span class="legend-swatch" style="background:${SERIES_COLORS[i % SERIES_COLORS.length]}"></span>${escapeHtml(s.label)}</span>`,
      )
      .join("");
  } catch (err) {
    if (seq !== timelineSeq) return;
    empty(String(err));
  }
}

async function refreshTreemapForRoot(root: string): Promise<void> {
  treemapRoot = root;
  treemapCrumb = [""];
  renderCrumb();
  await loadTreemap();
  await loadTimeline();
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
  scanRootEl = document.querySelector("#scan-root");
  scanBtnEl = document.querySelector("#scan-btn");
  scanQuickBtnEl = document.querySelector("#scan-quick-btn");
  scanAllBtnEl = document.querySelector("#scan-all-btn");
  scanCancelBtnEl = document.querySelector("#scan-cancel-btn");
  scanProgressEl = document.querySelector("#scan-progress");
  scanProgressTextEl = document.querySelector("#scan-progress-text");
  snapshotsEl = document.querySelector("#snapshots-table tbody");
  historyRootEl = document.querySelector("#history-root");
  historyGroupEl = document.querySelector("#history-group");
  historyEl = document.querySelector("#history-table tbody");
  growthEl = document.querySelector("#growth-table tbody");
  growthThresholdEl = document.querySelector("#growth-alert-threshold");
  largestEl = document.querySelector("#largest-table tbody");
  largestDirsEl = document.querySelector("#largest-dirs");
  treemapEl = document.querySelector("#treemap");
  treemapCrumbEl = document.querySelector("#treemap-crumb");
  timelineEl = document.querySelector("#timeline");
  timelineLegendEl = document.querySelector("#timeline-legend");

  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    void greet();
  });
  scanBtnEl?.addEventListener("click", () => void startScan());
  scanQuickBtnEl?.addEventListener("click", () => void startQuickScan());
  scanAllBtnEl?.addEventListener("click", () => void startScanAll());
  scanCancelBtnEl?.addEventListener("click", () => void cancelScan());

  growthThresholdEl?.addEventListener("input", () => renderGrowth());
  historyRootEl?.addEventListener("change", () => void loadHistory());
  historyGroupEl?.addEventListener("change", () => renderHistory());
  growthEl?.addEventListener("click", (event) => {
    const row = (event.target as HTMLElement).closest("tr[data-path]");
    if (!row) return;
    drillPath = row.getAttribute("data-path");
    renderGrowth();
  });

  snapshotsEl?.addEventListener("click", (event) => {
    const btn = (event.target as HTMLElement).closest("[data-delete-snapshot]");
    if (!btn) return;
    const id = Number(btn.getAttribute("data-delete-snapshot"));
    const root = btn.closest("tr")?.firstElementChild?.textContent ?? "";
    const ok = window.confirm(
      `¿Borrar el escaneo de ${root} (id ${id})? No se puede deshacer.`,
    );
    if (!ok) return;
    void (async () => {
      try {
        await invoke("delete_snapshot", { snapshotId: id });
        await loadSnapshots();
        // largest/growth/historial/treemap podían apuntar al snapshot borrado.
        await loadLargest();
        await loadLargestDirs();
        await loadGrowth();
        await loadHistory();
        if (treemapRoot) await refreshTreemapForRoot(treemapRoot);
      } catch (err) {
        window.alert(`No se pudo borrar el escaneo: ${String(err)}`);
      }
    })();
  });

  // Treemap: clic en un rectángulo para entrar a la carpeta.
  treemapEl?.addEventListener("click", (event) => {
    const group = (event.target as Element).closest("g.tm-node");
    if (!group) return;
    drillIntoTreemap(group.getAttribute("data-path") ?? "");
  });

  void listen<ScanProgress>("scan-progress", (event) => {
    const p = event.payload;
    if (scanProgressTextEl) {
      scanProgressTextEl.textContent = `${p.files.toLocaleString()} archivos · ${p.dirs.toLocaleString()} carpetas · ${formatBytes(p.bytes)} · ${p.read_errors.toLocaleString()} errores de lectura`;
    }
  });
  void listen<ScanQuickDonePayload>("scan-quick-done", (event) => {
    handleQuickScanDone(event.payload);
  });
  void listen<ScanAllUnit>("scan-all-unit", (event) => {
    const { letter, index, total } = event.payload;
    if (scanProgressTextEl) {
      scanProgressTextEl.textContent = `Unidad ${index}/${total}: ${letter} — recorriendo…`;
    }
  });
  void listen<ScanDonePayload>("scan-done", (event) => {
    // Durante "Escanear todo", el scan-done por unidad refresca sin cerrar
    // el estado de progreso (el handler normal lo cerraría cada unidad).
    if (scanAllActive) {
      handleScanDoneKeepBusy(event.payload);
    } else {
      handleScanDone(event.payload);
    }
  });
  void listen<unknown>("scan-all-done", () => {
    scanAllActive = false;
    setScanBusy(false, "Escaneo completo de todas las unidades terminado.");
    scheduleHideProgress(3500);
  });

  void loadVolumes();
  void loadSnapshots();
  void loadHistory();
  // Los listados "más pesados" son los queries costosos del arranque: se
  // difieren un tick para que volúmenes y snapshots pinten primero.
  window.setTimeout(() => void loadLargest(), 0);
  window.setTimeout(() => void loadLargestDirs(), 0);
  void ensureBaseline();
});
