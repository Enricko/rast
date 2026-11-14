// Desktop resource widgets: System Health, CPU & Memory, Storage, Network.
//
// Data: one shared feed per desktop uses the Resource Monitor's own data path
// (GET /api/monitor/snapshot + /api/monitor/history, then the realtime
// `monitor` events). The server applies the same application policy and
// container redaction as for the Resource Monitor, so a widget never shows
// more than that window would. States come from the data itself:
//   loading     -> no sample yet
//   live        -> samples arriving
//   stale       -> no sample for 3 sampling intervals (>= 15 s)
//   unavailable -> the monitor reported that it cannot sample this host
//   denied      -> the account may not use Resource Monitor
//   error       -> the snapshot could not be fetched (with Retry)
//
// Layout: widgets live in a layer covering the usable desktop (above the icons,
// below windows). A widget's position is stored as fractions of the free area
// (desktop size minus widget size), so it always stays fully visible when the
// browser gets smaller. Changes are saved to the account (appearance API).

import type { ApiClient } from "../core/api";
import { ApiError } from "../core/api";
import type { Appearance, AppearanceService, DesktopWidget, WidgetId } from "../core/appearance";
import { h } from "../core/dom";
import { formatBytes, formatDuration, formatPercent, formatSpeed, relativeTime } from "../core/format";
import { iconSvg } from "../core/icons";
import type { RealtimeStore } from "../core/realtime";
import type { ToastCenter } from "../core/toast";
import type { ComponentHealth, MonitorDisk, MonitorSnapshot } from "../core/types";
import { modal, sparkline } from "../core/ui";
import {
  GRID,
  WIDGET_GAP,
  defaultWidgetGroup,
  fitRect,
  nearestFreeRect,
  rectsIntersect,
  snap,
  snapUp,
  type Bounds,
  type Rect,
} from "./snap-grid";

export interface WidgetDefinition {
  id: WidgetId;
  title: string;
  description: string;
  icon: string;
}

export const WIDGET_CATALOG: ReadonlyArray<WidgetDefinition> = [
  { id: "health", title: "System Health", description: "Component status and uptime", icon: "activity" },
  { id: "cpu-memory", title: "CPU & Memory", description: "Current usage and recent history", icon: "cpu" },
  { id: "storage", title: "Storage", description: "Used and available capacity", icon: "disk" },
  { id: "network", title: "Network", description: "Current receive and transmit rates", icon: "network" },
];

function definition(id: WidgetId): WidgetDefinition {
  return WIDGET_CATALOG.find((item) => item.id === id) ?? WIDGET_CATALOG[0]!;
}

// ---------------------------------------------------------------------------
// Shared monitor feed
// ---------------------------------------------------------------------------

export type FeedStatus = "idle" | "loading" | "live" | "stale" | "unavailable" | "denied" | "error";

export interface FeedState {
  status: FeedStatus;
  snapshot: MonitorSnapshot | null;
  /** Local time the last sample arrived (0 = never). */
  receivedAt: number;
  reason: string;
  cpu: number[];
  mem: number[];
  rx: number[];
  tx: number[];
}

const SERIES_POINTS = 40;
const STALE_CHECK_MS = 5_000;

function push(series: number[], value: number | undefined): number[] {
  if (typeof value !== "number" || !Number.isFinite(value)) return series;
  const next = [...series, value];
  return next.length > SERIES_POINTS ? next.slice(next.length - SERIES_POINTS) : next;
}

/** `ts → points` series from /api/monitor/history (cpu, mem: `value`; net: rx/tx). */
function historyPoints(payload: unknown): Array<Record<string, number>> {
  const points = (payload as { points?: unknown } | null)?.points;
  return Array.isArray(points) ? (points as Array<Record<string, number>>) : [];
}

/** "8 s", "3 min", "2 h": how long ago the last sample arrived. */
export function sampleAge(receivedAt: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - receivedAt) / 1000));
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  return `${Math.round(minutes / 60)} h`;
}

export function staleAfterMs(snapshot: MonitorSnapshot | null): number {
  const interval = snapshot?.intervalSecs ?? 5;
  return Math.max(15_000, interval * 3 * 1000);
}

export class MonitorFeed {
  state: FeedState = { status: "idle", snapshot: null, receivedAt: 0, reason: "", cpu: [], mem: [], rx: [], tx: [] };
  private readonly listeners = new Set<() => void>();
  private offs: Array<() => void> = [];
  private timer: number | undefined;
  private running = false;
  private generation = 0;

  constructor(
    private readonly api: ApiClient,
    private readonly realtime: RealtimeStore,
    private readonly allowed: () => boolean,
  ) {}

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(): void {
    for (const listener of this.listeners) {
      try {
        listener();
      } catch (error) {
        console.error("widget feed listener failed", error);
      }
    }
  }

  private set(patch: Partial<FeedState>): void {
    this.state = { ...this.state, ...patch };
    this.emit();
  }

  get isRunning(): boolean {
    return this.running;
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    this.offs.push(this.realtime.on<MonitorSnapshot>("monitor", (snapshot) => this.accept(snapshot)));
    this.offs.push(this.realtime.onResync(() => void this.refresh(false)));
    this.timer = window.setInterval(() => this.tick(), STALE_CHECK_MS);
    void this.refresh(true);
  }

  stop(): void {
    this.running = false;
    this.generation += 1;
    for (const off of this.offs.splice(0)) off();
    if (this.timer !== undefined) window.clearInterval(this.timer);
    this.timer = undefined;
    this.state = { status: "idle", snapshot: null, receivedAt: 0, reason: "", cpu: [], mem: [], rx: [], tx: [] };
  }

  /** Fetch a fresh snapshot (and the recent history when asked). */
  async refresh(withHistory: boolean): Promise<void> {
    if (!this.running) return;
    const generation = ++this.generation;
    if (!this.allowed()) {
      this.set({ status: "denied", snapshot: null, reason: "Resource Monitor is not available for your account." });
      return;
    }
    if (!this.state.snapshot) this.set({ status: "loading", reason: "" });
    try {
      const [snapshot, cpu, mem, net] = await Promise.all([
        this.api.get<MonitorSnapshot>("/api/monitor/snapshot"),
        withHistory ? this.history("cpu") : Promise.resolve(null),
        withHistory ? this.history("mem") : Promise.resolve(null),
        withHistory ? this.history("net") : Promise.resolve(null),
      ]);
      if (generation !== this.generation || !this.running) return;
      if (cpu) this.state.cpu = cpu.map((point) => point.value ?? 0).slice(-SERIES_POINTS);
      if (mem) this.state.mem = mem.map((point) => point.value ?? 0).slice(-SERIES_POINTS);
      if (net) {
        this.state.rx = net.map((point) => point.rxBytesPerSec ?? 0).slice(-SERIES_POINTS);
        this.state.tx = net.map((point) => point.txBytesPerSec ?? 0).slice(-SERIES_POINTS);
      }
      this.accept(snapshot, { fromHistory: withHistory });
    } catch (error) {
      if (generation !== this.generation || !this.running) return;
      if (error instanceof ApiError && (error.status === 403 || error.status === 401)) {
        this.set({ status: "denied", snapshot: null, reason: "Resource Monitor is not available for your account." });
        return;
      }
      const reason = error instanceof Error ? error.message : String(error);
      // Keep showing the last data (marked stale) rather than blanking it.
      if (this.state.snapshot) this.set({ status: "stale", reason });
      else this.set({ status: "error", reason });
    }
  }

  private async history(metric: "cpu" | "mem" | "net"): Promise<Array<Record<string, number>> | null> {
    try {
      const payload = await this.api.get<unknown>("/api/monitor/history", { metric, points: String(SERIES_POINTS) });
      if ((payload as { available?: boolean } | null)?.available === false) return null;
      return historyPoints(payload);
    } catch {
      return null; // history is optional; live samples still fill the charts
    }
  }

  private accept(snapshot: MonitorSnapshot, options: { fromHistory?: boolean } = {}): void {
    if (!this.running || !snapshot) return;
    if (!this.allowed()) {
      this.set({ status: "denied", snapshot: null, reason: "Resource Monitor is not available for your account." });
      return;
    }
    const host = snapshot.host;
    const next: Partial<FeedState> = { snapshot, receivedAt: Date.now() };
    if (snapshot.unavailable) {
      next.status = "unavailable";
      next.reason = snapshot.reason || "The monitor could not sample this host.";
    } else {
      next.status = "live";
      next.reason = "";
      // The history already ends with (about) this sample.
      if (!options.fromHistory || this.state.cpu.length === 0) {
        next.cpu = push(this.state.cpu, host?.cpuPercent);
        next.mem = push(this.state.mem, host && host.memoryTotal > 0 ? (host.memoryUsed / host.memoryTotal) * 100 : undefined);
        next.rx = push(this.state.rx, snapshot.network?.rxBytesPerSec);
        next.tx = push(this.state.tx, snapshot.network?.txBytesPerSec);
      }
    }
    this.set(next);
  }

  private tick(): void {
    if (!this.running) return;
    const { status, receivedAt, snapshot } = this.state;
    if (status === "live" && receivedAt > 0 && Date.now() - receivedAt > staleAfterMs(snapshot)) {
      this.set({ status: "stale", reason: "" });
      return;
    }
    // Refresh "updated … ago" labels.
    this.emit();
  }
}

// ---------------------------------------------------------------------------
// Widget cards
// ---------------------------------------------------------------------------

interface Card {
  id: WidgetId;
  el: HTMLElement;
  head: HTMLElement;
  pill: HTMLElement;
  body: HTMLElement;
  foot: HTMLElement;
}

const STATUS_LABEL: Record<FeedStatus, string> = {
  idle: "Loading",
  loading: "Loading",
  live: "Live",
  stale: "Stale",
  unavailable: "Unavailable",
  denied: "No access",
  error: "Error",
};

function statusDot(status: ComponentHealth["status"]): HTMLElement {
  return h("span", { class: `widget-dot is-${status}`, ariaHidden: true });
}

function componentLabel(status: ComponentHealth["status"]): string {
  if (status === "ok") return "OK";
  if (status === "warning") return "Warning";
  if (status === "error") return "Error";
  return "Off";
}

function meter(percent: number, label: string): HTMLElement {
  const clamped = Math.max(0, Math.min(100, percent));
  const level = clamped >= 90 ? "is-danger" : clamped >= 75 ? "is-warning" : "";
  return h(
    "div",
    { class: `widget-meter ${level}`, role: "meter", ariaLabel: label, ariaValueMin: "0", ariaValueMax: "100", ariaValueNow: String(Math.round(clamped)) },
    h("span", { class: "widget-meter-fill", style: `width:${clamped.toFixed(1)}%` }),
  );
}

function renderHealth(snapshot: MonitorSnapshot): Node[] {
  const components = snapshot.components ?? [];
  const attention = components.filter((item) => item.status === "warning" || item.status === "error");
  const summary =
    components.length === 0
      ? "No components reported"
      : attention.length === 0
        ? "All components OK"
        : `${attention.length} need${attention.length === 1 ? "s" : ""} attention`;
  return [
    h(
      "div",
      { class: "widget-row widget-row-split" },
      h("span", { class: "widget-strong" }, summary),
      h("span", { class: "widget-muted" }, `Up ${formatDuration(snapshot.host?.uptimeSecs ?? 0)}`),
    ),
    h(
      "ul",
      { class: "widget-list" },
      ...components.map((component) =>
        h(
          "li",
          { class: "widget-list-item", ...(component.detail ? { title: component.detail } : {}) },
          statusDot(component.status),
          h("span", { class: "widget-list-name" }, component.name),
          h("span", { class: `widget-list-value is-${component.status}` }, componentLabel(component.status)),
        ),
      ),
    ),
  ];
}

function renderCpuMemory(snapshot: MonitorSnapshot, state: FeedState): Node[] {
  const host = snapshot.host;
  const memPercent = host && host.memoryTotal > 0 ? (host.memoryUsed / host.memoryTotal) * 100 : 0;
  const load = host?.loadAvg?.[0];
  return [
    h(
      "div",
      { class: "widget-metric" },
      h(
        "div",
        { class: "widget-metric-head" },
        h("span", { class: "widget-muted" }, "CPU"),
        h("span", { class: "widget-value" }, formatPercent(host?.cpuPercent ?? 0)),
      ),
      sparkline(state.cpu, { width: 220, height: 30 }),
      h(
        "div",
        { class: "widget-muted widget-small" },
        `${host?.cpuCores ?? "?"} cores${typeof load === "number" ? ` · load ${load.toFixed(2)}` : ""}`,
      ),
    ),
    h(
      "div",
      { class: "widget-metric" },
      h(
        "div",
        { class: "widget-metric-head" },
        h("span", { class: "widget-muted" }, "Memory"),
        h("span", { class: "widget-value" }, formatPercent(memPercent)),
      ),
      sparkline(state.mem, { width: 220, height: 30, color: "var(--chart-2)" }),
      h("div", { class: "widget-muted widget-small" }, `${formatBytes(host?.memoryUsed ?? 0)} of ${formatBytes(host?.memoryTotal ?? 0)}`),
    ),
  ];
}

const MAX_DISKS = 3;

function renderStorage(snapshot: MonitorSnapshot): Node[] {
  const disks = [...(snapshot.disks ?? [])]
    .filter((disk) => disk.totalBytes > 0)
    .sort((a, b) => b.totalBytes - a.totalBytes);
  if (disks.length === 0) return [h("div", { class: "widget-muted" }, "No disks reported")];
  const rows = disks.slice(0, MAX_DISKS).map((disk: MonitorDisk) => {
    const percent = (disk.usedBytes / disk.totalBytes) * 100;
    const free = Math.max(0, disk.totalBytes - disk.usedBytes);
    return h(
      "div",
      { class: "widget-disk" },
      h(
        "div",
        { class: "widget-row widget-row-split" },
        h("span", { class: "widget-strong widget-ellipsis", title: disk.mount }, disk.mount),
        h("span", { class: "widget-muted" }, formatPercent(percent)),
      ),
      meter(percent, `${disk.mount} used`),
      h("div", { class: "widget-muted widget-small" }, `${formatBytes(free)} free of ${formatBytes(disk.totalBytes)}`),
    );
  });
  if (disks.length > MAX_DISKS) {
    rows.push(h("div", { class: "widget-muted widget-small" }, `+${disks.length - MAX_DISKS} more in Resource Monitor`));
  }
  return rows;
}

function renderNetwork(snapshot: MonitorSnapshot, state: FeedState): Node[] {
  const net = snapshot.network;
  const line = (label: string, icon: string, rate: number | undefined, series: number[], color: string): HTMLElement =>
    h(
      "div",
      { class: "widget-metric" },
      h(
        "div",
        { class: "widget-metric-head" },
        h("span", { class: "widget-muted widget-icon-label" }, iconSvg(icon, 13), label),
        h("span", { class: "widget-value" }, formatSpeed(rate ?? 0)),
      ),
      sparkline(series, { width: 220, height: 26, color }),
    );
  return [
    line("Receive", "arrowDown", net?.rxBytesPerSec, state.rx, "var(--accent)"),
    line("Transmit", "arrowUp", net?.txBytesPerSec, state.tx, "var(--chart-3)"),
    h("div", { class: "widget-muted widget-small" }, `Total ↓ ${formatBytes(net?.rxTotal ?? 0)} · ↑ ${formatBytes(net?.txTotal ?? 0)}`),
  ];
}

function renderBody(id: WidgetId, state: FeedState): Node[] {
  const snapshot = state.snapshot;
  if (!snapshot) return [];
  if (id === "health") return renderHealth(snapshot);
  if (id === "cpu-memory") return renderCpuMemory(snapshot, state);
  if (id === "storage") return renderStorage(snapshot);
  return renderNetwork(snapshot, state);
}

// ---------------------------------------------------------------------------
// Layer controller
// ---------------------------------------------------------------------------

/**
 * Widgets shown to an account that never chose its own (and may use Resource
 * Monitor): System Health top-right, CPU & Memory bottom-right. Opposite
 * corners keep them apart whatever their content height.
 */
export const DEFAULT_WIDGETS: ReadonlyArray<DesktopWidget> = [
  { id: "health", x: 1, y: 0 },
  { id: "cpu-memory", x: 1, y: 1 },
];

/** The layout to show: the account's own list, or the defaults it never replaced. */
export function effectiveWidgets(
  appearance: Pick<Appearance, "widgets" | "widgetsCustomized">,
  monitorAllowed: boolean,
): DesktopWidget[] {
  if (appearance.widgetsCustomized) return appearance.widgets.map((widget) => ({ ...widget }));
  return monitorAllowed ? DEFAULT_WIDGETS.map((widget) => ({ ...widget })) : [];
}

export interface WidgetLayerOptions {
  layer: HTMLElement;
  api: ApiClient;
  realtime: RealtimeStore;
  appearance: AppearanceService;
  toast: ToastCenter;
  isMonitorAllowed: () => boolean;
  openMonitor: () => void;
  /** Desktop app-icon rectangles + the desktop grid cell size. */
  obstacles?: () => { rects: Array<{ x: number; y: number; w: number; h: number }>; cellW: number; cellH: number };
  /** Called when widget geometry changes (icons avoid the widgets). */
  onGeometryChange?: () => void;
}

export interface WidgetLayer {
  /** Show exactly this layout. */
  sync(widgets: DesktopWidget[]): void;
  /** Show the layout being saved, else the account's saved (or default) layout. */
  showSaved(): void;
  openCatalog(): void;
  /** Current widget rectangles in desktop coordinates (for icon collision). */
  occupiedRects(): Rect[];
  /** Current widget rectangles with ids (desktop selection). */
  rectEntries(): Array<{ id: WidgetId; rect: Rect }>;
  /** Preferred widget definitions with the current on-screen pixel positions. */
  snapshot(): DesktopWidget[];
  /** Move widgets by updating their pixel positions (no save). */
  applyPixels(positions: Map<WidgetId, { x: number; y: number }>): void;
  /** Re-evaluate access after a permission change. */
  permissionsChanged(): void;
  dispose(): void;
}


export function createWidgetLayer(options: WidgetLayerOptions): WidgetLayer {
  const { layer, appearance, toast } = options;
  const feed = new MonitorFeed(options.api, options.realtime, options.isMonitorAllowed);
  const cards = new Map<WidgetId, Card>();
  let layout: DesktopWidget[] = [];
  let dragging: WidgetId | null = null;
  let disposed = false;
  let keySaveTimer: number | undefined;
  const dropPreview = h("div", {
    class: "widget-drop-preview",
    ariaHidden: true,
    hidden: true,
  });
  layer.appendChild(dropPreview);
  const gridOverlay = h("div", { class: "desktop-grid-overlay", ariaHidden: true, hidden: true });
  layer.appendChild(gridOverlay);
  function showGrid(): void {
    const area = bounds();
    gridOverlay.style.left = `${area.x}px`;
    gridOverlay.style.top = `${area.y}px`;
    gridOverlay.style.width = `${area.w}px`;
    gridOverlay.style.height = `${area.h}px`;
    gridOverlay.hidden = false;
  }
  function hideGrid(): void {
    gridOverlay.hidden = true;
  }
  let catalogRefresh: (() => void) | null = null;

  const offFeed = feed.subscribe(() => {
    for (const card of cards.values()) renderCard(card);
  });

  // Keep every widget inside the usable desktop: on resize and whenever a
  // card's own size changes (content arrives, fonts load).
  const resizeObserver =
    typeof ResizeObserver === "function" ? new ResizeObserver(() => positionAll()) : null;
  resizeObserver?.observe(layer);
  const onWindowResize = (): void => positionAll();
  if (!resizeObserver) window.addEventListener("resize", onWindowResize);

  /** Readable, content-aware preferred sizes (multiples of the 8px grid). */
  const WIDGET_SIZES: Record<WidgetId, { w: number; h: number; minW: number; minH: number }> = {
    health: { w: 232, h: 312, minW: 208, minH: 280 },
    "cpu-memory": { w: 232, h: 256, minW: 208, minH: 224 },
    storage: { w: 224, h: 232, minW: 200, minH: 208 },
    network: { w: 224, h: 232, minW: 200, minH: 208 },
  };
  const MIN_WIDGET_WIDTH = 200;
  const MIN_WIDGET_HEIGHT = 200;
  let migrationQueued = false;
  let overflowWarned = false;

  const bounds = (): Bounds => ({ x: 0, y: 0, w: layer.clientWidth, h: layer.clientHeight });
  const iconObstacles = (): Rect[] => options.obstacles?.().rects ?? [];
  const clampPx = (value: number, min: number, max: number): number =>
    Math.round(Math.max(min, Math.min(max, value)));

  /** The widget's preferred size in fixed CSS pixels (never viewport-scaled). */
  function preferredSize(widget: DesktopWidget): { w: number; h: number } {
    const spec = WIDGET_SIZES[widget.id];
    if (
      widget.gridVersion === 2 &&
      typeof widget.pxW === "number" &&
      typeof widget.pxH === "number"
    ) {
      return {
        w: clampPx(snapUp(widget.pxW), spec.minW, 4000),
        h: clampPx(snapUp(widget.pxH), spec.minH, 4000),
      };
    }
    return { w: spec.w, h: spec.h };
  }

  /** On-screen size: the preference, compacted only when the viewport is too small. */
  function displaySize(preferred: { w: number; h: number }, area: Bounds): { w: number; h: number } {
    return {
      w: Math.max(Math.min(GRID, area.w), Math.min(preferred.w, area.w)),
      h: Math.max(Math.min(GRID, area.h), Math.min(preferred.h, area.h)),
    };
  }

  /** Preferred top-left in fixed pixels (legacy fractions are only for migration). */
  function preferredPosition(widget: DesktopWidget, area: Bounds): { x: number; y: number } {
    if (
      widget.gridVersion === 2 &&
      typeof widget.pxX === "number" &&
      typeof widget.pxY === "number"
    ) {
      return { x: snap(widget.pxX), y: snap(widget.pxY) };
    }
    const size = preferredSize(widget);
    const freeW = Math.max(0, area.w - size.w);
    const freeH = Math.max(0, area.h - size.h);
    return {
      x: snap(area.x + Math.max(0, Math.min(1, widget.x)) * freeW),
      y: snap(area.y + Math.max(0, Math.min(1, widget.y)) * freeH),
    };
  }

  /** Fractions for older clients; the pixel fields are authoritative. */
  function legacyFractions(rect: Rect): { x: number; y: number; w: number; h: number } {
    const area = bounds();
    const freeW = Math.max(0, area.w - rect.w);
    const freeH = Math.max(0, area.h - rect.h);
    const round = (value: number): number => Math.round(Math.max(0, Math.min(1, value)) * 10_000) / 10_000;
    return {
      x: round(freeW > 0 ? (rect.x - area.x) / freeW : 0),
      y: round(freeH > 0 ? (rect.y - area.y) / freeH : 0),
      w: round(area.w > 0 ? rect.w / area.w : 0),
      h: round(area.h > 0 ? rect.h / area.h : 0),
    };
  }

  /** One-time, versioned migration of legacy fractional widgets to pixels. */
  function migrateLegacyWidgets(): void {
    if (!appearance.isKnown || layout.every((widget) => widget.gridVersion === 2)) return;
    const area = bounds();
    layout = layout.map((widget) => {
      if (widget.gridVersion === 2) return widget;
      const size = preferredSize(widget);
      const position = preferredPosition(widget, area);
      return {
        ...widget,
        gridVersion: 2,
        pxX: position.x,
        pxY: position.y,
        pxW: size.w,
        pxH: size.h,
      };
    });
    if (migrationQueued) return;
    migrationQueued = true;
    window.setTimeout(() => {
      migrationQueued = false;
      if (!disposed && appearance.isKnown) void appearance.saveWidgets(layout);
    }, 400);
  }

  function applyRect(card: Card, rect: Rect): void {
    card.el.style.left = `${rect.x}px`;
    card.el.style.top = `${rect.y}px`;
    card.el.style.width = `${rect.w}px`;
    card.el.style.height = `${rect.h}px`;
  }

  function rectOf(card: Card): Rect {
    return { x: card.el.offsetLeft, y: card.el.offsetTop, w: card.el.offsetWidth, h: card.el.offsetHeight };
  }

  function otherWidgetRects(except: WidgetId): Rect[] {
    const out: Rect[] = [];
    for (const [id, card] of cards) {
      if (id === except || !card.el.isConnected) continue;
      out.push(rectOf(card));
    }
    return out;
  }

  /**
   * Responsive placement pass. It never changes the saved preferences: the
   * preferred size/position are fixed pixels, and a smaller viewport only
   * produces a temporary compact/repositioned rect.
   */
  function positionAll(): void {
    if (disposed) return;
    migrateLegacyWidgets();
    const area = bounds();
    const icons = iconObstacles();
    const useDefaultGroup = !appearance.isKnown || !appearance.value.widgetsCustomized;
    if (useDefaultGroup) {
      const preferred = layout.map((widget) => preferredSize(widget));
      const group = defaultWidgetGroup(preferred, area, WIDGET_GAP);
      layout.forEach((widget, index) => {
        const card = cards.get(widget.id);
        if (!card) return;
        const pref = preferred[index]!;
        const target = group[index] ?? fitRect(
          { x: area.x, y: area.y, w: pref.w, h: pref.h },
          area,
          Math.min(pref.w, area.w),
          Math.min(pref.h, area.h),
        );
        const compact = target.w < pref.w || target.h < pref.h;
        card.el.classList.toggle("is-compact", compact);
        applyRect(card, target);
      });
      return;
    }
    const placed: Rect[] = [...icons];
    let overflow = false;
    layout.forEach((widget) => {
      const card = cards.get(widget.id);
      if (!card) return;
      const pref = preferredSize(widget);
      const size = displaySize(pref, area);
      const position = preferredPosition(widget, area);
      let target = fitRect(
        { x: position.x, y: position.y, w: size.w, h: size.h },
        area,
        size.w,
        size.h,
      );
      if (placed.some((other) => rectsIntersect(other, target))) {
        const free = nearestFreeRect(target, size, area, placed);
        if (free) {
          target = free;
        } else {
          overflow = true;
        }
      }
      const compact = size.w < pref.w || size.h < pref.h;
      card.el.classList.toggle("is-compact", compact);
      card.el.classList.toggle("is-overflow", overflow);
      applyRect(card, target);
      placed.push(target);
    });
    if (overflow && !overflowWarned) {
      overflowWarned = true;
      toast.warning(
        "There is not enough desktop space for every widget at its saved size. Move or resize them, or use a larger window.",
      );
    }
    options.onGeometryChange?.();
  }

  function persist(): void {
    options.onGeometryChange?.();
    void appearance.saveWidgets(layout).then((ok) => {
      if (ok || disposed) return;
      toast.warning("Could not save the widget layout to your account. Showing the saved layout.");
      showSaved();
    });
  }

  function renderCard(card: Card): void {
    const state = feed.state;
    const status = state.status;
    card.el.dataset.status = status;
    card.pill.className = `widget-pill is-${status}`;
    card.pill.textContent = STATUS_LABEL[status];
    let body: Node[];
    let foot = "";
    if (status === "idle" || status === "loading" || (status === "live" && !state.snapshot)) {
      body = [
        h("div", { class: "skeleton-line", style: "width:70%" }),
        h("div", { class: "skeleton-line", style: "width:90%" }),
        h("div", { class: "skeleton-line", style: "width:55%" }),
      ];
      foot = "Loading live data…";
    } else if (status === "denied") {
      body = [h("div", { class: "widget-message" }, iconSvg("lock", 16), h("span", {}, state.reason))];
      foot = "Remove it, or ask an administrator for access.";
    } else if (status === "unavailable") {
      body = [h("div", { class: "widget-message" }, iconSvg("warning", 16), h("span", {}, state.reason))];
      foot = state.receivedAt ? `Reported ${relativeTime(state.receivedAt)}` : "";
    } else if (status === "error") {
      body = [
        h("div", { class: "widget-message" }, iconSvg("warning", 16), h("span", {}, `Could not load monitoring data: ${state.reason}`)),
        h(
          "button",
          { class: "btn btn-sm", type: "button", onClick: () => void feed.refresh(true) },
          iconSvg("refresh", 13),
          "Retry",
        ),
      ];
    } else {
      body = renderBody(card.id, state);
      foot =
        status === "stale"
          ? state.reason
            ? `No new data for ${sampleAge(state.receivedAt)}: ${state.reason}`
            : `No new data for ${sampleAge(state.receivedAt)}. Waiting for the live connection…`
          : `Updated ${relativeTime(state.receivedAt)}`;
    }
    card.el.classList.toggle("is-stale", status === "stale");
    card.body.replaceChildren(...body);
    card.foot.textContent = foot;
    card.foot.hidden = !foot;
  }

  function buildCard(id: WidgetId): Card {
    const def = definition(id);
    const pill = h("span", { class: "widget-pill" });
    const removeButton = h(
      "button",
      {
        class: "widget-action",
        type: "button",
        ariaLabel: `Remove ${def.title} widget`,
        title: "Remove widget",
        onClick: () => remove(id),
      },
      iconSvg("close", 13),
    );
    const openButton = h(
      "button",
      {
        class: "widget-action",
        type: "button",
        ariaLabel: `Open Resource Monitor`,
        title: "Open Resource Monitor",
        onClick: () => options.openMonitor(),
      },
      iconSvg("external", 13),
    );
    const head = h(
      "div",
      { class: "widget-head" },
      h("span", { class: "widget-head-icon" }, iconSvg(def.icon, 15)),
      h("span", { class: "widget-title" }, def.title),
      pill,
      h("span", { class: "widget-actions" }, openButton, removeButton),
    );
    const body = h("div", { class: "widget-body" });
    const foot = h("div", { class: "widget-foot" });
    const el = h(
      "section",
      {
        class: "desktop-widget",
        tabIndex: 0,
        role: "group",
        ariaLabel: `${def.title} widget. Drag the title or use the arrow keys to move it; Delete removes it.`,
        dataset: { widget: id },
      },
      head,
      body,
      h("div", { class: "widget-resize", ariaHidden: true }),
      foot,
    );
    const card: Card = { id, el, head, pill, body, foot };
    attachDrag(card);
    attachKeys(card);
    attachResize(card);
    resizeObserver?.observe(el);
    renderCard(card);
    return card;
  }

  function bringToFront(id: WidgetId): void {
    const index = layout.findIndex((widget) => widget.id === id);
    if (index < 0 || index === layout.length - 1) return;
    const [widget] = layout.splice(index, 1);
    layout.push(widget!);
    const card = cards.get(id);
    if (card) layer.appendChild(card.el);
  }

  // ------------------------------------------------------------- dragging
  function dragObstacles(card: Card): Rect[] {
    return [...iconObstacles(), ...otherWidgetRects(card.id)];
  }

  function attachDrag(card: Card): void {
    card.el.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      if ((event.target as Element).closest("button, a, input, select, textarea, .widget-resize")) return;
      event.preventDefault();
      const layerRect = layer.getBoundingClientRect();
      const startPointerX = event.clientX - layerRect.left;
      const startPointerY = event.clientY - layerRect.top;
      const startRect = rectOf(card);
      const area = bounds();
      dragging = card.id;
      bringToFront(card.id);
      card.el.classList.add("is-dragging");
      card.el.setPointerCapture(event.pointerId);
      showGrid();
      let target: Rect = startRect;
      let blocked = false;
      const move = (moveEvent: PointerEvent): void => {
        const pointerX = moveEvent.clientX - layerRect.left;
        const pointerY = moveEvent.clientY - layerRect.top;
        const raw = fitRect(
          {
            x: startRect.x + pointerX - startPointerX,
            y: startRect.y + pointerY - startPointerY,
            w: startRect.w,
            h: startRect.h,
          },
          area,
          MIN_WIDGET_WIDTH,
          MIN_WIDGET_HEIGHT,
        );
        target = raw;
        card.el.style.left = `${raw.x}px`;
        card.el.style.top = `${raw.y}px`;
        blocked = dragObstacles(card).some((other) => rectsIntersect(other, raw));
        dropPreview.hidden = false;
        dropPreview.style.left = `${raw.x}px`;
        dropPreview.style.top = `${raw.y}px`;
        dropPreview.style.width = `${raw.w}px`;
        dropPreview.style.height = `${raw.h}px`;
        dropPreview.classList.add("is-visible");
        dropPreview.classList.toggle("is-blocked", blocked);
      };
      const end = (): void => {
        card.el.removeEventListener("pointermove", move);
        card.el.removeEventListener("pointerup", end);
        card.el.removeEventListener("pointercancel", end);
        card.el.classList.remove("is-dragging");
        dropPreview.classList.remove("is-visible", "is-blocked");
        dropPreview.hidden = true;
        hideGrid();
        dragging = null;
        const obstacles = dragObstacles(card);
        const final = blocked
          ? nearestFreeRect(target, { w: target.w, h: target.h }, area, obstacles) ?? target
          : target;
        applyRect(card, final);
        layout = layout.map((widget) => {
          if (widget.id !== card.id) return widget;
          const preferred = preferredSize(widget);
          return {
            ...widget,
            ...legacyFractions({ ...final, w: preferred.w, h: preferred.h }),
            gridVersion: 2,
            pxX: snap(final.x),
            pxY: snap(final.y),
            pxW: preferred.w,
            pxH: preferred.h,
          };
        });
        persist();
      };
      card.el.addEventListener("pointermove", move);
      card.el.addEventListener("pointerup", end);
      card.el.addEventListener("pointercancel", end);
    });
  }

  function attachResize(card: Card): void {
    const handle = card.el.querySelector<HTMLElement>(".widget-resize");
    if (!handle) return;
    handle.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.stopPropagation();
      const layerRect = layer.getBoundingClientRect();
      const startPointerX = event.clientX - layerRect.left;
      const startPointerY = event.clientY - layerRect.top;
      const startRect = rectOf(card);
      const area = bounds();
      const spec = WIDGET_SIZES[card.id];
      handle.setPointerCapture(event.pointerId);
      showGrid();
      let target = startRect;
      const move = (moveEvent: PointerEvent): void => {
        const dx = moveEvent.clientX - layerRect.left - startPointerX;
        const dy = moveEvent.clientY - layerRect.top - startPointerY;
        target = fitRect(
          {
            x: startRect.x,
            y: startRect.y,
            w: Math.max(spec.minW, snapUp(startRect.w + dx)),
            h: Math.max(spec.minH, snapUp(startRect.h + dy)),
          },
          area,
          spec.minW,
          spec.minH,
        );
        applyRect(card, target);
        dropPreview.hidden = false;
        dropPreview.style.left = `${target.x}px`;
        dropPreview.style.top = `${target.y}px`;
        dropPreview.style.width = `${target.w}px`;
        dropPreview.style.height = `${target.h}px`;
        dropPreview.classList.add("is-visible");
        dropPreview.classList.toggle("is-blocked", false);
      };
      const end = (): void => {
        handle.removeEventListener("pointermove", move);
        handle.removeEventListener("pointerup", end);
        handle.removeEventListener("pointercancel", end);
        dropPreview.classList.remove("is-visible", "is-blocked");
        dropPreview.hidden = true;
        hideGrid();
        const obstacles = dragObstacles(card);
        const final = obstacles.some((other) => rectsIntersect(other, target))
          ? nearestFreeRect(target, { w: target.w, h: target.h }, area, obstacles) ?? target
          : target;
        applyRect(card, final);
        layout = layout.map((widget) =>
          widget.id === card.id
            ? {
                ...widget,
                ...legacyFractions(final),
                gridVersion: 2,
                pxX: snap(final.x),
                pxY: snap(final.y),
                pxW: snap(final.w),
                pxH: snap(final.h),
              }
            : widget,
        );
        persist();
      };
      handle.addEventListener("pointermove", move);
      handle.addEventListener("pointerup", end);
      handle.addEventListener("pointercancel", end);
    });
  }

  function attachKeys(card: Card): void {
    card.el.addEventListener("keydown", (event) => {
      if (event.target !== card.el) return;
      if (event.key === "Delete" || event.key === "Backspace") {
        event.preventDefault();
        remove(card.id);
        return;
      }
      const step = event.shiftKey ? GRID * 8 : GRID;
      const delta: Record<string, [number, number]> = {
        ArrowLeft: [-step, 0],
        ArrowRight: [step, 0],
        ArrowUp: [0, -step],
        ArrowDown: [0, step],
      };
      const move = delta[event.key];
      if (!move) return;
      event.preventDefault();
      const area = bounds();
      const current = rectOf(card);
      const desired = fitRect(
        { x: current.x + move[0], y: current.y + move[1], w: current.w, h: current.h },
        area,
        MIN_WIDGET_WIDTH,
        MIN_WIDGET_HEIGHT,
      );
      const obstacles = dragObstacles(card);
      const target = obstacles.some((other) => rectsIntersect(other, desired))
        ? nearestFreeRect(desired, { w: desired.w, h: desired.h }, area, obstacles) ?? desired
        : desired;
      applyRect(card, target);
      layout = layout.map((widget) => {
        if (widget.id !== card.id) return widget;
        const preferred = preferredSize(widget);
        return {
          ...widget,
          ...legacyFractions({ ...target, w: preferred.w, h: preferred.h }),
          gridVersion: 2,
          pxX: snap(target.x),
          pxY: snap(target.y),
          pxW: preferred.w,
          pxH: preferred.h,
        };
      });
      window.clearTimeout(keySaveTimer);
      keySaveTimer = window.setTimeout(() => persist(), 500);
    });
  }

  // ------------------------------------------------------- add / remove
  function add(id: WidgetId): void {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet, so widgets cannot be changed. Try again in a moment.");
      return;
    }
    if (layout.some((widget) => widget.id === id)) return;
    const card = cards.get(id) ?? buildCard(id);
    cards.set(id, card);
    card.el.style.visibility = "hidden";
    layer.appendChild(card.el);
    if (!feed.isRunning) feed.start();
    renderCard(card);
    layout = [...layout, { id, x: 0.7, y: 0 }];
    positionAll();
    card.el.style.visibility = "";
    persist();
    catalogRefresh?.();
  }

  function remove(id: WidgetId): void {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet, so widgets cannot be changed. Try again in a moment.");
      return;
    }
    const card = cards.get(id);
    const hadFocus = card?.el.contains(document.activeElement) ?? false;
    layout = layout.filter((widget) => widget.id !== id);
    if (card) {
      resizeObserver?.unobserve(card.el);
      card.el.remove();
      cards.delete(id);
    }
    if (layout.length === 0) feed.stop();
    persist();
    catalogRefresh?.();
    // Keep keyboard users on the desktop rather than losing focus to <body>.
    if (hadFocus) {
      const next = layout[layout.length - 1];
      const nextCard = next ? cards.get(next.id) : undefined;
      nextCard?.el.focus({ preventScroll: true });
    }
  }

  // ---------------------------------------------------------------- sync
  function occupiedRects(): Rect[] {
    return [...cards.values()].filter((card) => card.el.isConnected).map(rectOf);
  }

  function rectEntries(): Array<{ id: WidgetId; rect: Rect }> {
    return [...cards.entries()]
      .filter(([, card]) => card.el.isConnected)
      .map(([id, card]) => ({ id, rect: rectOf(card) }));
  }

  function snapshot(): DesktopWidget[] {
    return layout.map((widget) => {
      const card = cards.get(widget.id);
      const size = preferredSize(widget);
      if (!card) return widget;
      const rect = rectOf(card);
      return {
        ...widget,
        ...legacyFractions({ ...rect, w: size.w, h: size.h }),
        gridVersion: 2,
        pxX: snap(rect.x),
        pxY: snap(rect.y),
        pxW: size.w,
        pxH: size.h,
      };
    });
  }

  function applyPixels(positions: Map<WidgetId, { x: number; y: number }>): void {
    layout = layout.map((widget) => {
      const position = positions.get(widget.id);
      if (!position) return widget;
      return { ...widget, gridVersion: 2, pxX: snap(position.x), pxY: snap(position.y) };
    });
    positionAll();
  }

  function sync(widgets: DesktopWidget[]): void {
    if (disposed) return;
    // A drag in progress owns its card; its drop saves the final layout.
    if (dragging) return;
    layout = widgets.map((widget) => ({ ...widget }));
    const wanted = new Set(layout.map((widget) => widget.id));
    for (const [id, card] of cards) {
      if (wanted.has(id)) continue;
      resizeObserver?.unobserve(card.el);
      card.el.remove();
      cards.delete(id);
    }
    for (const widget of layout) {
      let card = cards.get(widget.id);
      if (!card) {
        card = buildCard(widget.id);
        cards.set(widget.id, card);
      }
      // Array order is stacking order.
      layer.appendChild(card.el);
    }
    if (layout.length > 0 && !feed.isRunning) feed.start();
    if (layout.length === 0 && feed.isRunning) feed.stop();
    positionAll();
    catalogRefresh?.();
  }

  function showSaved(): void {
    if (!appearance.isKnown) return;
    sync(appearance.pendingWidgets ?? effectiveWidgets(appearance.value, options.isMonitorAllowed()));
  }

  // ------------------------------------------------------------- catalog
  function openCatalog(): void {
    const list = h("ul", { class: "widget-catalog", role: "list" });
    const notice = h("div", { class: "banner banner-warning", hidden: true, role: "status" });
    const render = (): void => {
      const known = appearance.isKnown;
      const allowed = options.isMonitorAllowed();
      notice.hidden = known && allowed;
      notice.textContent = !known
        ? "Your saved desktop could not be loaded yet, so widgets cannot be changed. Try again in a moment."
        : "These widgets show Resource Monitor data, which is not available for your account.";
      list.replaceChildren(
        ...WIDGET_CATALOG.map((def) => {
          const active = layout.some((widget) => widget.id === def.id);
          const button = h(
            "button",
            {
              class: `btn btn-sm ${active ? "btn-ghost" : "btn-primary"}`,
              type: "button",
              ariaPressed: active,
              ariaLabel: active ? `Remove ${def.title} from the desktop` : `Add ${def.title} to the desktop`,
              disabled: !known || (!active && !allowed),
              onClick: () => (active ? remove(def.id) : add(def.id)),
            },
            iconSvg(active ? "close" : "plus", 13),
            active ? "Remove" : "Add",
          );
          return h(
            "li",
            { class: `widget-catalog-item${active ? " is-active" : ""}` },
            h("span", { class: "widget-catalog-icon" }, iconSvg(def.icon, 18)),
            h(
              "span",
              { class: "widget-catalog-text" },
              h("span", { class: "widget-catalog-title" }, def.title, active ? h("span", { class: "badge" }, "On desktop") : null),
              h("span", { class: "widget-catalog-detail" }, def.description),
            ),
            button,
          );
        }),
      );
    };
    render();
    catalogRefresh = render;
    modal({
      title: "Widgets",
      body: h(
        "div",
        { class: "widget-catalog-wrap" },
        h(
          "p",
          { class: "muted small" },
          "Live resource widgets for your desktop. Drag a widget by its title to move it; your layout is saved to your account.",
        ),
        notice,
        list,
      ),
      actions: [{ label: "Done", variant: "primary", onClick: (close) => close() }],
      onClose: () => {
        if (catalogRefresh === render) catalogRefresh = null;
      },
    });
  }

  function permissionsChanged(): void {
    // Defaults depend on access: show them once Resource Monitor is granted,
    // drop them when it is revoked (a saved layout shows "No access" instead).
    showSaved();
    if (feed.isRunning) void feed.refresh(true);
    catalogRefresh?.();
  }

  function dispose(): void {
    disposed = true;
    window.clearTimeout(keySaveTimer);
    offFeed();
    feed.stop();
    resizeObserver?.disconnect();
    if (!resizeObserver) window.removeEventListener("resize", onWindowResize);
    for (const card of cards.values()) card.el.remove();
    cards.clear();
  }

  return { sync, showSaved, openCatalog, permissionsChanged, occupiedRects, rectEntries, snapshot, applyPixels, dispose };
}
