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
}

export interface WidgetLayer {
  /** Show exactly this layout. */
  sync(widgets: DesktopWidget[]): void;
  /** Show the layout being saved, else the account's saved (or default) layout. */
  showSaved(): void;
  openCatalog(): void;
  /** Re-evaluate access after a permission change. */
  permissionsChanged(): void;
  dispose(): void;
}

const LAYER_GAP = 12;
const KEY_STEP = 24;

export function createWidgetLayer(options: WidgetLayerOptions): WidgetLayer {
  const { layer, appearance, toast } = options;
  const feed = new MonitorFeed(options.api, options.realtime, options.isMonitorAllowed);
  const cards = new Map<WidgetId, Card>();
  let layout: DesktopWidget[] = [];
  let dragging: WidgetId | null = null;
  let disposed = false;
  let keySaveTimer: number | undefined;
  let lastLayerSize: { w: number; h: number } | null = null;
  /** Snap grid for widget drops: half a 2×2 tile (tile width + gap). */
  const SNAP_STEP = 144;
  const snapValue = (value: number, max: number): number =>
    Math.max(0, Math.min(max, Math.round(value / SNAP_STEP) * SNAP_STEP));
  const dropPreview = h("div", {
    class: "widget-drop-preview",
    ariaHidden: true,
    hidden: true,
  });
  layer.appendChild(dropPreview);
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

  function freeArea(card: Card): { width: number; height: number } {
    return {
      width: Math.max(0, layer.clientWidth - card.el.offsetWidth),
      height: Math.max(0, layer.clientHeight - card.el.offsetHeight),
    };
  }

  function position(card: Card, widget: DesktopWidget): void {
    const area = freeArea(card);
    card.el.style.left = `${Math.round(widget.x * area.width)}px`;
    card.el.style.top = `${Math.round(widget.y * area.height)}px`;
  }

  function positionAll(): void {
    if (disposed) return;
    // Widgets are fixed-size tiles (roughly a 2x2 block of app icons), not
    // scaled with the screen: changing the monitor must never shrink the text
    // or clip titles. When the saved arrangement would overlap on a smaller
    // screen, the cards reflow into columns instead (see below).
    const scale = 1;
    layer.style.setProperty("--widget-scale", scale.toFixed(3));
    const visible = layout.flatMap((widget) => {
      if (widget.id === dragging) return [];
      const card = cards.get(widget.id);
      if (!card) return [];
      const area = freeArea(card);
      return [{
        widget,
        card,
        x: Math.round(widget.x * area.width),
        y: Math.round(widget.y * area.height),
        w: card.el.offsetWidth,
        h: card.el.offsetHeight,
      }];
    });
    const gap = Math.max(8, Math.round(10 * scale));
    const collides = (a: (typeof visible)[number], b: (typeof visible)[number]): boolean =>
      a.x < b.x + b.w + gap &&
      a.x + a.w + gap > b.x &&
      a.y < b.y + b.h + gap &&
      a.y + a.h + gap > b.y;
    const hasOverlap = visible.some((a, index) => visible.slice(index + 1).some((b) => collides(a, b)));
    const layerSize = { w: layer.clientWidth, h: layer.clientHeight };
    const viewportChanged =
      lastLayerSize !== null &&
      (lastLayerSize.w !== layerSize.w || lastLayerSize.h !== layerSize.h);
    lastLayerSize = layerSize;
    if (!hasOverlap || !viewportChanged) {
      for (const item of visible) {
        item.card.el.style.left = `${item.x}px`;
        item.card.el.style.top = `${item.y}px`;
      }
      return;
    }
    // The saved fractional layout is only relative: on a smaller monitor the
    // cards would pile on top of each other. Reflow them into non-overlapping,
    // right-aligned columns instead (order is preserved).
    const width = Math.max(0, ...visible.map((item) => item.w));
    const height = layer.clientHeight;
    let columnX = Math.max(0, layer.clientWidth - width);
    let cursorY = 0;
    for (const item of visible) {
      if (cursorY > 0 && cursorY + item.h > height) {
        columnX = Math.max(0, columnX - width - gap);
        cursorY = 0;
      }
      item.card.el.style.left = `${columnX + Math.max(0, width - item.w)}px`;
      item.card.el.style.top = `${cursorY}px`;
      cursorY += item.h + gap;
    }
  }

  function fractionsFor(card: Card, left: number, top: number): { x: number; y: number } {
    const area = freeArea(card);
    const x = area.width > 0 ? left / area.width : 0;
    const y = area.height > 0 ? top / area.height : 0;
    const round = (value: number): number => Math.round(Math.max(0, Math.min(1, value)) * 10_000) / 10_000;
    return { x: round(x), y: round(y) };
  }

  function persist(): void {
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
      foot,
    );
    const card: Card = { id, el, head, pill, body, foot };
    attachDrag(card);
    attachKeys(card);
    resizeObserver?.observe(el);
    renderCard(card);
    return card;
  }

  // ------------------------------------------------------------- dragging
  function attachDrag(card: Card): void {
    // The whole tile is a handle (the title bar keeps the grab cursor), except
    // the action buttons; the body is not selectable, so this cannot steal a
    // text selection.
    card.el.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      if ((event.target as Element).closest("button, a, input, select, textarea")) return;
      event.preventDefault();
      const startX = event.clientX;
      const startY = event.clientY;
      const startLeft = card.el.offsetLeft;
      const startTop = card.el.offsetTop;
      dragging = card.id;
      bringToFront(card.id);
      card.el.classList.add("is-dragging");
      card.el.setPointerCapture(event.pointerId);
      const area = freeArea(card);
      let left = startLeft;
      let top = startTop;
      let targetLeft = startLeft;
      let targetTop = startTop;
      const move = (moveEvent: PointerEvent): void => {
        left = Math.max(0, Math.min(area.width, startLeft + moveEvent.clientX - startX));
        top = Math.max(0, Math.min(area.height, startTop + moveEvent.clientY - startY));
        targetLeft = snapValue(left, area.width);
        targetTop = snapValue(top, area.height);
        card.el.style.left = `${Math.round(left)}px`;
        card.el.style.top = `${Math.round(top)}px`;
        // Show where the tile will be clipped into place on drop.
        dropPreview.hidden = false;
        dropPreview.style.left = `${targetLeft}px`;
        dropPreview.style.top = `${targetTop}px`;
        dropPreview.style.width = `${card.el.offsetWidth}px`;
        dropPreview.style.height = `${card.el.offsetHeight}px`;
        dropPreview.classList.add("is-visible");
      };
      const end = (): void => {
        card.el.removeEventListener("pointermove", move);
        card.el.removeEventListener("pointerup", end);
        card.el.removeEventListener("pointercancel", end);
        card.el.classList.remove("is-dragging");
        dropPreview.classList.remove("is-visible");
        dropPreview.hidden = true;
        dragging = null;
        const moved = Math.abs(left - startLeft) > 1 || Math.abs(top - startTop) > 1;
        const snappedLeft = snapValue(left, area.width);
        const snappedTop = snapValue(top, area.height);
        const { x, y } = fractionsFor(card, snappedLeft, snappedTop);
        layout = layout.map((widget) => (widget.id === card.id ? { ...widget, x, y } : widget));
        position(card, layout.find((widget) => widget.id === card.id)!);
        if (moved || layout[layout.length - 1]?.id === card.id) persist();
      };
      card.el.addEventListener("pointermove", move);
      card.el.addEventListener("pointerup", end);
      card.el.addEventListener("pointercancel", end);
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
      const step = event.shiftKey ? 4 : KEY_STEP;
      const delta: Record<string, [number, number]> = {
        ArrowLeft: [-step, 0],
        ArrowRight: [step, 0],
        ArrowUp: [0, -step],
        ArrowDown: [0, step],
      };
      const move = delta[event.key];
      if (!move) return;
      event.preventDefault();
      const area = freeArea(card);
      const left = snapValue(card.el.offsetLeft + move[0], area.width);
      const top = snapValue(card.el.offsetTop + move[1], area.height);
      const { x, y } = fractionsFor(card, left, top);
      layout = layout.map((widget) => (widget.id === card.id ? { ...widget, x, y } : widget));
      position(card, layout.find((widget) => widget.id === card.id)!);
      window.clearTimeout(keySaveTimer);
      keySaveTimer = window.setTimeout(() => persist(), 500);
    });
  }

  function bringToFront(id: WidgetId): void {
    const index = layout.findIndex((widget) => widget.id === id);
    if (index < 0 || index === layout.length - 1) return;
    const [widget] = layout.splice(index, 1);
    layout.push(widget!);
    const card = cards.get(id);
    if (card) layer.appendChild(card.el);
  }

  // ------------------------------------------------------- add / remove
  /** First free slot in the right-hand column (icons occupy the left). */
  function defaultPlacement(card: Card): { x: number; y: number } {
    const area = freeArea(card);
    const height = card.el.offsetHeight;
    const width = card.el.offsetWidth;
    const right = layer.clientWidth - width;
    const occupied = layout
      .map((widget) => cards.get(widget.id))
      .filter((other): other is Card => Boolean(other) && other !== card)
      .map((other) => ({ top: other.el.offsetTop, bottom: other.el.offsetTop + other.el.offsetHeight, left: other.el.offsetLeft, right: other.el.offsetLeft + other.el.offsetWidth }));
    const candidates = [0, ...occupied.map((rect) => rect.bottom + LAYER_GAP)].sort((a, b) => a - b);
    for (const top of candidates) {
      if (top + height > layer.clientHeight) break;
      const overlaps = occupied.some((rect) => top < rect.bottom && top + height > rect.top && right < rect.right && right + width > rect.left);
      if (!overlaps) return fractionsFor(card, right, top);
    }
    // Column full: start a second column to the left.
    return { x: area.width > 0 ? Math.max(0, (right - width - LAYER_GAP) / area.width) : 0, y: 0 };
  }

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
    const placement = defaultPlacement(card);
    layout = [...layout, { id, ...placement }];
    position(card, layout[layout.length - 1]!);
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

  return { sync, showSaved, openCatalog, permissionsChanged, dispose };
}
