// Resource Monitor: live host/container metrics from SSE `monitor` events with
// an HTTP snapshot fallback, bounded sparklines, disks, network throughput,
// container resources and component health. OWNER: teammate `frontend-apps`.

import type { AppDefinition, AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import {
  formatBytes,
  formatDuration,
  formatPercent,
  formatSpeed,
  relativeTime,
} from "../../core/format";
import { iconSvg } from "../../core/icons";
import type {
  ComponentHealth,
  ContainerMetric,
  MonitorDisk,
  MonitorSnapshot,
  Permissions,
} from "../../core/types";
import { sparkline } from "../../core/ui";
import { debounceTask, getRealtime, type RealtimeStore } from "../../core/realtime";
import { AppLifecycle, isAbortError } from "../../components/lifecycle";
import { createLiveIndicator, type LiveIndicator } from "../../components/live";
import { banner, renderLoadFailure } from "../../components/states";

const SERIES_POINTS = 60;
const MIN_RENDER_GAP_MS = 1000;

/** `docker.changed` payload (docs/REALTIME.md). */
interface DockerChangedEvent {
  enabled?: boolean;
  available?: boolean;
  version?: string | null;
  reason?: string | null;
}

interface MetricCard {
  el: HTMLElement;
  value: HTMLElement;
  sub: HTMLElement;
  spark: HTMLElement;
  fill?: HTMLElement;
  warn?: HTMLElement;
}

interface MonitorSeries {
  cpu: number[];
  memory: number[];
  swap: number[];
  rx: number[];
  tx: number[];
}

class MonitorApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly contentHost: HTMLElement;
  private readonly bannerHost: HTMLElement;
  private readonly summaryHost: HTMLElement;
  private readonly disksHost: HTMLElement;
  private readonly containersHost: HTMLElement;
  private readonly componentsHost: HTMLElement;
  private readonly ticker: HTMLElement;
  private readonly staleBadge: HTMLElement;
  private readonly pauseButton: HTMLButtonElement;
  private readonly realtime: RealtimeStore;
  private readonly liveIndicator: LiveIndicator;
  private readonly syncSnapshot: { run: () => void; cancel: () => void };
  private readonly accessCheck: { run: () => void; cancel: () => void };
  private localPermissions: Permissions | null = null;
  private denied = false;
  private lastAccessSignature = "";
  private lastPermissionsRef: unknown = undefined;

  private lastGood: MonitorSnapshot | null = null;
  private latestReceivedAt = 0;
  private unavailableReasonText = "";
  private fatalError: unknown = null;
  private paused = false;
  private layoutBuilt = false;
  private lastRenderAt = 0;
  private renderTimer = 0;
  private unsubMonitor: (() => void) | null = null;
  private monitorController: AbortController | null = null;
  private fallbackContainers: ContainerMetric[] | null = null;
  private fallbackComponents: ComponentHealth[] | null = null;
  private fallbackFetched = false;
  private disposed = false;

  private readonly series: MonitorSeries = { cpu: [], memory: [], swap: [], rx: [], tx: [] };
  private cardCpu: MetricCard | null = null;
  private cardMemory: MetricCard | null = null;
  private cardSwap: MetricCard | null = null;
  private cardLoad: MetricCard | null = null;
  private cardUptime: MetricCard | null = null;
  private cardRx: MetricCard | null = null;
  private cardTx: MetricCard | null = null;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.realtime = getRealtime(ctx.events);
    this.liveIndicator = createLiveIndicator(this.realtime, this.lc);
    this.syncSnapshot = debounceTask(() => void this.refreshSnapshot(), 250);
    this.lc.add(() => this.syncSnapshot.cancel());

    this.ticker = h("span", { class: "mon-ticker small muted", role: "status", ariaLive: "polite" }, "Waiting for first sample…");
    this.staleBadge = h("span", { class: "badge mon-stale hidden" }, "stale");
    this.pauseButton = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("stop", 13), document.createTextNode("Pause"));
    this.pauseButton.addEventListener("click", () => this.togglePause());
    const toolbar = h(
      "div",
      { class: "app-toolbar mon-toolbar", role: "toolbar", ariaLabel: "Monitor controls" },
      this.pauseButton,
      this.liveIndicator.el,
      h("div", { class: "app-toolbar-spacer" }),
      this.staleBadge,
      this.ticker,
    );

    this.bannerHost = h("div", { class: "mon-banners" });
    this.summaryHost = h("div", { class: "mon-summary" });
    this.disksHost = h("section", { class: "mon-section" });
    this.containersHost = h("section", { class: "mon-section" });
    this.componentsHost = h("section", { class: "mon-section" });
    this.contentHost = h(
      "div",
      { class: "app-content mon-content" },
      this.bannerHost,
      this.summaryHost,
      this.disksHost,
      this.containersHost,
      this.componentsHost,
    );

    ctx.root.classList.add("monitor-app");
    ctx.root.append(toolbar, this.contentHost);

    this.lc.add(() => {
      this.disposed = true;
      this.unsubMonitor?.();
      this.unsubMonitor = null;
      this.monitorController?.abort();
    });
    this.accessCheck = debounceTask(() => void this.onAccessChanged(), 150);
    this.lastAccessSignature = `${this.ctx.store.get().user?.id ?? 0}|${this.ctx.store.get().user?.role ?? ""}`;
    this.lastPermissionsRef = this.ctx.store.get().permissions ?? null;
    this.lc.add(() => this.accessCheck.cancel());
    this.lc.add(
      this.ctx.store.subscribe(() => {
        const state = this.ctx.store.get() as unknown as {
          permissions?: unknown;
          user?: { id?: number; role?: string } | null;
        };
        const permissionsRef = state.permissions ?? null;
        const signature = `${state.user?.id ?? 0}|${state.user?.role ?? ""}`;
        if (permissionsRef === this.lastPermissionsRef && signature === this.lastAccessSignature) return;
        this.lastPermissionsRef = permissionsRef;
        this.lastAccessSignature = signature;
        this.accessCheck.run();
      }),
    );
    this.lc.add(this.realtime.on("access.changed", () => this.accessCheck.run()));
  }

  // ----------------------------------------------------------- access policy


  private readGrant(): { allowed: boolean; readOnly: boolean } {
    const state = this.ctx.store.get() as unknown as { permissions?: Permissions | null };
    const grant = state.permissions?.apps?.monitor ?? this.localPermissions?.apps?.monitor;
    if (grant) return { allowed: Boolean(grant.allowed), readOnly: Boolean(grant.readOnly) };
    // Resource Monitor is available to every authenticated user by default.
    return { allowed: true, readOnly: false };
  }

  private async onAccessChanged(): Promise<void> {
    if (this.disposed) return;
    const state = this.ctx.store.get() as unknown as { permissions?: Permissions | null };
    if (!state.permissions) {
      try {
        const me = await this.ctx.api.get<{ permissions?: Permissions }>("/api/auth/me");
        this.localPermissions = me.permissions ?? null;
      } catch {
        this.localPermissions = null;
      }
    }
    if (this.disposed) return;
    const grant = this.readGrant();
    if (!grant.allowed) {
      this.denied = true;
      this.renderDenied();
      return;
    }
    if (this.denied) {
      this.denied = false;
      this.layoutBuilt = false;
      this.buildLayout();
    }
    // Still allowed: refetch the snapshot/history for the new access scope.
    void this.refreshSnapshot();
    void this.loadAllHistory();
    this.scheduleRender();
  }

  private renderDenied(): void {
    this.paused = true;
    clear(this.contentHost);
    this.contentHost.appendChild(
      h(
        "div",
        { class: "app-access-state", role: "alert" },
        h("div", { class: "app-access-title" }, "Resource Monitor access changed"),
        h(
          "div",
          { class: "app-access-detail" },
          "Your access to Resource Monitor was revoked by a group policy change. Ask an administrator if you need access again.",
        ),
        (() => {
          const btn = h("button", { class: "btn btn-ghost", type: "button" }, "Close window");
          btn.addEventListener("click", () => this.ctx.window.close());
          return btn;
        })(),
      ),
    );
    this.ticker.textContent = "Access changed";
    this.staleBadge.classList.add("hidden");
    this.layoutBuilt = false;
  }

  async start(): Promise<void> {
    this.ctx.setTitle("Resource Monitor");
    if (!this.ctx.store.get().permissions) {
      try {
        const me = await this.ctx.api.get<{ permissions?: Permissions }>("/api/auth/me");
        if (me.permissions) this.localPermissions = me.permissions;
      } catch {
        /* fall back to the default (monitor allowed) */
      }
    }
    if (!this.readGrant().allowed) {
      this.denied = true;
      this.renderDenied();
      return;
    }
    this.buildLayout();
    this.subscribe();
    this.lc.setInterval(() => this.tick(), 1000);
    await this.refreshSnapshot();
    void this.loadAllHistory();
    void this.loadFallbackData();
  }

  // ------------------------------------------------------------- data flow

  private subscribe(): void {
    this.unsubMonitor?.();
    this.unsubMonitor = this.realtime.on<MonitorSnapshot>("monitor", (snapshot) => {
      if (this.paused || this.disposed) return;
      this.onSample(snapshot);
    });
    this.lc.add(
      this.realtime.on<DockerChangedEvent>("docker.changed", (data) => {
        if (this.disposed || !data || typeof data !== "object") return;
        if (!this.paused) this.syncSnapshot.run();
      }),
    );
    this.lc.add(
      this.realtime.onResync(() => {
        if (this.disposed || this.paused) return;
        void this.refreshSnapshot();
        void this.loadAllHistory();
      }),
    );
  }

  private onSample(raw: unknown): void {
    if (!raw || typeof raw !== "object") return;
    const snapshot = raw as MonitorSnapshot;
    this.latestReceivedAt = Date.now();
    if (snapshot.ts && snapshot.ts > 0) this.latestReceivedAt = Date.now();
    if (snapshot.host) {
      this.lastGood = snapshot;
      this.unavailableReasonText = snapshot.unavailable ? snapshot.reason ?? "" : "";
      this.fatalError = null;
    } else if (snapshot.unavailable) {
      this.unavailableReasonText = snapshot.reason || "The monitor could not sample this host.";
    }
    this.scheduleRender();
  }

  private async refreshSnapshot(): Promise<void> {
    this.monitorController?.abort();
    this.monitorController = this.lc.trackController(new AbortController());
    const request = this.monitorController;
    try {
      const snapshot = await this.ctx.api.get<MonitorSnapshot>("/api/monitor/snapshot", undefined, {
        signal: request.signal,
      });
      if (request.signal.aborted || this.disposed) return;
      this.fatalError = null;
      this.onSample(snapshot);
    } catch (error) {
      if (isAbortError(error) || request.signal.aborted || this.disposed) return;
      if (!this.lastGood) {
        this.fatalError = error;
        this.renderFatal();
      } else {
        this.unavailableReasonText = `Sampling failed: ${error instanceof Error ? error.message : String(error)}`;
        this.renderBanners();
      }
    } finally {
      this.lc.releaseController(request);
    }
  }

  private async loadAllHistory(): Promise<void> {
    await Promise.all([this.loadHistory("cpu"), this.loadHistory("memory"), this.loadNetworkHistory()]);
  }

  /** /api/monitor/history supports cpu | mem | net; net fills both network cards. */
  private async loadNetworkHistory(): Promise<void> {
    try {
      const payload = await this.ctx.api.get<unknown>("/api/monitor/history", {
        metric: "network",
        points: SERIES_POINTS,
      });
      if (this.disposed) return;
      const { rx, tx } = parseNetworkHistory(payload);
      if (rx.length > 0) {
        this.series.rx.length = 0;
        this.series.rx.push(...rx.slice(-SERIES_POINTS));
      }
      if (tx.length > 0) {
        this.series.tx.length = 0;
        this.series.tx.push(...tx.slice(-SERIES_POINTS));
      }
    } catch {
      // History is an enhancement; the live series still renders without it.
    }
  }

  private async loadHistory(metric: "cpu" | "memory"): Promise<void> {
    try {
      const payload = await this.ctx.api.get<unknown>("/api/monitor/history", {
        metric,
        points: SERIES_POINTS,
      });
      if (this.disposed) return;
      const values = parseHistory(payload);
      if (values.length === 0) return;
      const target = this.series[metric];
      target.length = 0;
      target.push(...values.slice(-SERIES_POINTS));
    } catch {
      // History is an enhancement; the live series still renders without it.
    }
  }

  private async loadFallbackData(): Promise<void> {
    if (this.fallbackFetched || this.disposed) return;
    this.fallbackFetched = true;
    const snapshot = this.lastGood;
    if (!snapshot?.containers) {
      try {
        const payload = await this.ctx.api.get<ContainerMetric[] | { containers?: ContainerMetric[]; items?: ContainerMetric[] }>(
          "/api/monitor/containers",
        );
        if (this.disposed) return;
        if (Array.isArray(payload)) this.fallbackContainers = payload;
        else if (Array.isArray(payload?.containers)) this.fallbackContainers = payload.containers;
        else if (Array.isArray(payload?.items)) this.fallbackContainers = payload.items;
      } catch {
        this.fallbackContainers = [];
      }
    }
    if (!snapshot?.components) {
      try {
        const payload = await this.ctx.api.get<{ components?: ComponentHealth[] } | ComponentHealth[]>("/api/system/components");
        if (this.disposed) return;
        if (Array.isArray(payload)) this.fallbackComponents = payload;
        else if (Array.isArray(payload?.components)) this.fallbackComponents = payload.components;
      } catch {
        this.fallbackComponents = [];
      }
    }
    this.scheduleRender();
  }

  // --------------------------------------------------------------- timers

  private tick(): void {
    if (this.disposed) return;
    this.renderTicker();
    if (this.paused) return;
    if (!this.layoutBuilt && this.lastGood) this.buildLayout();
  }

  private isStale(): boolean {
    if (this.paused) return false;
    if (this.latestReceivedAt === 0) return true;
    const interval = this.lastGood?.intervalSecs ?? 5;
    const staleAfter = Math.max(15000, interval * 3 * 1000);
    return Date.now() - this.latestReceivedAt > staleAfter;
  }

  private togglePause(): void {
    this.paused = !this.paused;
    clear(this.pauseButton);
    this.pauseButton.appendChild(iconSvg(this.paused ? "play" : "stop", 13));
    this.pauseButton.appendChild(document.createTextNode(this.paused ? "Resume" : "Pause"));
    this.staleBadge.classList.add("hidden");
    if (this.paused) {
      this.ticker.textContent = "Updates paused";
    } else {
      void this.refreshSnapshot();
      this.scheduleRender();
    }
    this.renderBanners();
  }

  // ------------------------------------------------------------- rendering

  private scheduleRender(): void {
    if (this.paused || this.disposed) return;
    if (this.renderTimer) return;
    const wait = Math.max(0, MIN_RENDER_GAP_MS - (Date.now() - this.lastRenderAt));
    this.renderTimer = this.lc.setTimeout(() => {
      this.renderTimer = 0;
      this.lc.frame(() => {
        this.lastRenderAt = Date.now();
        this.renderNow();
      });
    }, wait);
  }

  private renderNow(): void {
    if (this.disposed || this.paused || this.denied) return;
    if (!this.layoutBuilt) {
      if (this.lastGood) this.buildLayout();
      else return;
    }
    const snapshot = this.lastGood;
    if (!snapshot?.host) {
      this.renderTicker();
      return;
    }
    this.renderBanners();
    this.renderSummary(snapshot);
    this.renderDisks(snapshot.disks ?? []);
    const containers = snapshot.containers ?? this.fallbackContainers ?? [];
    this.renderContainers(containers);
    const components = snapshot.components ?? this.fallbackComponents ?? [];
    this.renderComponents(components);
    this.renderTicker();
  }

  private renderFatal(): void {
    clear(this.contentHost);
    if (!this.fatalError) return;
    renderLoadFailure(this.contentHost, this.fatalError, {
      title: "Resource monitor unavailable",
      retry: () => {
        this.fatalError = null;
        void this.refreshSnapshot();
      },
    });
    this.layoutBuilt = false;
    this.cardCpu = null;
    this.cardMemory = null;
    this.cardSwap = null;
    this.cardLoad = null;
    this.cardUptime = null;
    this.cardRx = null;
    this.cardTx = null;
    this.renderTicker();
  }

  private buildLayout(): void {
    if (this.layoutBuilt) return;
    this.layoutBuilt = true;
    if (!this.contentHost.contains(this.bannerHost)) {
      this.contentHost.append(
        this.bannerHost,
        this.summaryHost,
        this.disksHost,
        this.containersHost,
        this.componentsHost,
      );
    }
    clear(this.summaryHost);
    this.summaryHost.appendChild(
      h(
        "div",
        { class: "mon-section-intro" },
        h("div", { class: "mon-section-title" }, "Host resources"),
        h("div", { class: "small muted" }, "System-wide totals sampled from the host — not per-process values. Container usage is listed separately below."),
      ),
    );
    const grid = h("div", { class: "stat-grid mon-stat-grid" });
    this.cardCpu = this.createCard("cpu", "CPU", "CPU utilisation");
    this.cardMemory = this.createCard("memory", "Memory", "RAM usage");
    this.cardSwap = this.createCard("memory", "Swap", "Swap usage");
    this.cardLoad = this.createCard("monitor", "Load average", "1 · 5 · 15 minute load");
    this.cardUptime = this.createCard("clock", "Uptime", "Host system uptime");
    this.cardRx = this.createCard("arrowDown", "Network receive", "Inbound throughput");
    this.cardTx = this.createCard("arrowUp", "Network send", "Outbound throughput");
    for (const card of [this.cardCpu, this.cardMemory, this.cardSwap, this.cardLoad, this.cardUptime, this.cardRx, this.cardTx]) {
      if (card) grid.appendChild(card.el);
    }
    this.summaryHost.appendChild(grid);
    this.renderBanners();
  }

  private createCard(icon: string, label: string, hint: string): MetricCard {
    const value = h("div", { class: "stat-value" }, "—");
    const sub = h("div", { class: "stat-sub" }, hint);
    const spark = h("div", { class: "mon-spark" });
    const bar = h("div", { class: "progress mon-card-bar" });
    const fill = h("div", { class: "progress-bar" });
    fill.style.width = "0%";
    bar.appendChild(fill);
    const isBar = label === "Memory" || label === "Swap";
    const el = h(
      "div",
      { class: "stat-card mon-card" },
      h("div", { class: "stat-label" }, iconSvg(icon, 14), label),
      value,
      sub,
      isBar ? bar : null,
      spark,
    );
    return { el, value, sub, spark, fill: isBar ? fill : undefined };
  }

  private renderBanners(): void {
    clear(this.bannerHost);
    if (this.paused) {
      this.bannerHost.appendChild(
        banner("info", "Live updates paused", "Values stay frozen until you resume. The server keeps sampling in the background."),
      );
    }
    if (this.unavailableReasonText && !this.paused) {
      this.bannerHost.appendChild(
        banner("warning", "Monitoring is degraded", this.unavailableReasonText),
      );
    }
    if (!this.paused && this.latestReceivedAt > 0 && this.isStale()) {
      const retry = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, "Retry now");
      retry.addEventListener("click", () => void this.refreshSnapshot());
      this.bannerHost.appendChild(
        banner(
          "warning",
          "Live metrics are stale",
          `No sample has arrived for ${relativeTime(this.latestReceivedAt).replace(" ago", "")}. The dashboard shows the last known values.`,
          retry,
        ),
      );
    }
    if (!this.paused && !this.ctx.events.isConnected && this.latestReceivedAt > 0) {
      this.bannerHost.appendChild(
        banner("info", "Live connection interrupted", "The event stream is reconnecting; a polling fallback runs every 5 seconds."),
      );
    }
  }

  private renderTicker(): void {
    if (this.paused) {
      this.ticker.textContent = "Updates paused";
      this.staleBadge.classList.add("hidden");
      return;
    }
    const stale = this.isStale();
    this.staleBadge.classList.toggle("hidden", !stale);
    if (this.latestReceivedAt === 0) {
      this.ticker.textContent = "Waiting for first sample…";
      return;
    }
    const interval = this.lastGood?.intervalSecs ?? 0;
    this.ticker.textContent = `Updated ${relativeTime(this.latestReceivedAt)}${interval > 0 ? ` · every ${interval}s` : ""}${
      stale ? " · stale" : ""
    }`;
  }

  private renderSummary(snapshot: MonitorSnapshot): void {
    const host = snapshot.host;
    if (!host) return;
    const cpu = clampPercent(host.cpuPercent);
    this.updateCard(this.cardCpu, formatPercent(cpu, 1), `${host.cpuCores} core(s) · 1m load ${formatLoad(host.loadAvg, 0, 1)}`, pushSeries(this.series.cpu, cpu));
    if (this.cardLoad) {
      const load = host.loadAvg ?? [];
      this.cardLoad.value.textContent = load[0] !== undefined && Number.isFinite(load[0]) ? load[0].toFixed(2) : "—";
      this.cardLoad.sub.textContent = `5 min ${formatLoad(load, 1, 2)} · 15 min ${formatLoad(load, 2, 3)}`;
    }
    if (this.cardUptime) {
      this.cardUptime.value.textContent = formatDuration(host.uptimeSecs);
      this.cardUptime.sub.textContent = "Host system uptime";
    }
    const memoryPercent = host.memoryTotal > 0 ? clampPercent((host.memoryUsed / host.memoryTotal) * 100) : 0;
    this.updateCard(
      this.cardMemory,
      formatPercent(memoryPercent, 1),
      `${formatBytes(host.memoryUsed)} of ${formatBytes(host.memoryTotal)}`,
      pushSeries(this.series.memory, memoryPercent),
      memoryPercent / 100,
    );
    const swapPercent = host.swapTotal > 0 ? clampPercent((host.swapUsed / host.swapTotal) * 100) : 0;
    this.updateCard(
      this.cardSwap,
      host.swapTotal > 0 ? formatPercent(swapPercent, 1) : "—",
      host.swapTotal > 0 ? `${formatBytes(host.swapUsed)} of ${formatBytes(host.swapTotal)}` : "No swap configured",
      pushSeries(this.series.swap, swapPercent),
      host.swapTotal > 0 ? swapPercent / 100 : 0,
    );
    const network = snapshot.network;
    this.updateCard(
      this.cardRx,
      formatSpeed(network?.rxBytesPerSec),
      `received ${formatBytes(network?.rxTotal)} total`,
      pushSeries(this.series.rx, network?.rxBytesPerSec ?? 0),
    );
    this.updateCard(
      this.cardTx,
      formatSpeed(network?.txBytesPerSec),
      `sent ${formatBytes(network?.txTotal)} total`,
      pushSeries(this.series.tx, network?.txBytesPerSec ?? 0),
    );
  }

  private updateCard(
    card: MetricCard | null,
    value: string,
    sub: string,
    series: number[],
    barFraction?: number,
  ): void {
    if (!card) return;
    card.value.textContent = value;
    card.sub.textContent = sub;
    if (card.fill && barFraction !== undefined) {
      const clamped = Math.min(1, Math.max(0, barFraction));
      card.fill.style.width = `${(clamped * 100).toFixed(1)}%`;
      card.fill.classList.toggle("is-error", clamped > 0.9);
    }
    clear(card.spark);
    if (series.length > 1) {
      card.spark.appendChild(sparkline(series, { width: 168, height: 34, color: "var(--accent)" }));
    }
  }

  private renderDisks(disks: MonitorDisk[]): void {
    clear(this.disksHost);
    const header = h(
      "div",
      { class: "mon-section-head" },
      h("h3", { class: "mon-section-title" }, "Disks"),
      h("span", { class: "small muted" }, "Filesystem capacity"),
    );
    this.disksHost.appendChild(header);
    if (disks.length === 0) {
      this.disksHost.appendChild(h("div", { class: "mon-empty small muted" }, "No disk information reported by the monitor."));
      return;
    }
    const head = h(
      "tr",
      {},
      h("th", {}, "Mount"),
      h("th", { class: "mon-col-fs" }, "Filesystem"),
      h("th", { class: "num" }, "Used"),
      h("th", { class: "num" }, "Total"),
      h("th", { class: "num" }, "Free"),
      h("th", { class: "mon-col-usage" }, "Usage"),
    );
    const body = h("tbody");
    for (const disk of disks.slice(0, 50)) {
      const total = Number(disk.totalBytes) || 0;
      const used = Number(disk.usedBytes) || 0;
      const percent = total > 0 ? Math.min(100, Math.max(0, (used / total) * 100)) : 0;
      const fill = h("div", { class: "progress-bar" });
      fill.style.width = `${percent.toFixed(1)}%`;
      if (percent > 90) fill.classList.add("is-error");
      body.appendChild(
        h(
          "tr",
          {},
          h("td", { class: "mono" }, disk.mount),
          h("td", { class: "mon-col-fs mono small muted" }, disk.filesystem ?? "—"),
          h("td", { class: "num" }, formatBytes(used)),
          h("td", { class: "num" }, formatBytes(total)),
          h("td", { class: "num" }, formatBytes(Math.max(0, total - used))),
          h(
            "td",
            { class: "mon-col-usage" },
            h("div", { class: "mon-disk-usage" }, h("div", { class: "progress" }, fill), h("span", { class: "small" }, formatPercent(percent, 0))),
          ),
        ),
      );
    }
    this.disksHost.appendChild(
      h("div", { class: "table-wrap mon-table-wrap" }, h("table", { class: "table" }, h("thead", {}, head), body)),
    );
  }

  private renderContainers(containers: ContainerMetric[]): void {
    clear(this.containersHost);
    const header = h(
      "div",
      { class: "mon-section-head" },
      h("h3", { class: "mon-section-title" }, `Containers (${containers.length})`),
      h("span", { class: "small muted" }, "Docker resource usage, separate from host totals"),
    );
    this.containersHost.appendChild(header);
    if (containers.length === 0) {
      const dockerEnabled = this.ctx.store.get().features.docker;
      this.containersHost.appendChild(
        h(
          "div",
          { class: "mon-empty small muted" },
          dockerEnabled
            ? "No container metrics available. Docker containers may not be running."
            : "Docker integration is disabled on this server, so no container metrics are collected.",
        ),
      );
      return;
    }
    const head = h("tr", {}, h("th", {}, "Container"), h("th", { class: "num" }, "CPU"), h("th", { class: "num" }, "Memory"));
    const body = h("tbody");
    for (const container of containers.slice(0, 100)) {
      const fill = h("div", { class: "progress-bar" });
      const cpu = clampPercent(container.cpuPercent);
      fill.style.width = `${Math.min(100, cpu).toFixed(1)}%`;
      body.appendChild(
        h(
          "tr",
          {},
          h("td", { class: "mono" }, container.name),
          h(
            "td",
            { class: "num mon-container-cpu" },
            h("div", { class: "mon-container-cell" }, h("div", { class: "progress mon-container-bar" }, fill), h("span", { class: "small" }, formatPercent(cpu, 1))),
          ),
          h("td", { class: "num" }, formatBytes(container.memoryBytes)),
        ),
      );
    }
    this.containersHost.appendChild(
      h("div", { class: "table-wrap mon-table-wrap" }, h("table", { class: "table" }, h("thead", {}, head), body)),
    );
  }

  private renderComponents(components: ComponentHealth[]): void {
    clear(this.componentsHost);
    this.componentsHost.appendChild(
      h(
        "div",
        { class: "mon-section-head" },
        h("h3", { class: "mon-section-title" }, "Components"),
        h("span", { class: "small muted" }, "Internal service health"),
      ),
    );
    if (components.length === 0) {
      this.componentsHost.appendChild(h("div", { class: "mon-empty small muted" }, "No component health information reported."));
      return;
    }
    const list = h("div", { class: "mon-components" });
    for (const component of components.slice(0, 50)) {
      list.appendChild(
        h(
          "div",
          { class: "mon-component" },
          h("span", { class: `badge ${componentStatusClass(component.status)}` }, component.status),
          h("span", { class: "mon-component-name" }, component.name || component.id),
          h("span", { class: "small muted mon-component-detail" }, component.detail || ""),
        ),
      );
    }
    this.componentsHost.appendChild(list);
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function clampPercent(value: number | undefined | null): number {
  if (value === undefined || value === null || Number.isNaN(value)) return 0;
  return Math.min(100, Math.max(0, value));
}

function formatLoad(load: number[] | undefined, from = 0, to = 3): string {
  if (!load || load.length === 0) return "—";
  return load
    .slice(from, to)
    .map((value) => (Number.isFinite(value) ? value.toFixed(2) : "—"))
    .join(" · ");
}

function pushSeries(series: number[], value: number): number[] {
  series.push(Number.isFinite(value) ? value : 0);
  while (series.length > SERIES_POINTS) series.shift();
  return series;
}

function parseHistory(payload: unknown): number[] {
  if (Array.isArray(payload)) {
    const values: number[] = [];
    for (const entry of payload) {
      const value =
        typeof entry === "number"
          ? entry
          : entry && typeof entry === "object"
            ? Number((entry as Record<string, unknown>).value ?? (entry as Record<string, unknown>).percent)
            : Number.NaN;
      if (Number.isFinite(value)) values.push(value);
    }
    return values;
  }
  if (payload && typeof payload === "object") {
    const record = payload as Record<string, unknown>;
    for (const key of ["points", "values", "history", "data", "samples"]) {
      const candidate = record[key];
      if (Array.isArray(candidate)) return parseHistory(candidate);
    }
  }
  return [];
}

function parseNetworkHistory(payload: unknown): { rx: number[]; tx: number[] } {
  const entries = historyEntries(payload);
  const rx: number[] = [];
  const tx: number[] = [];
  for (const entry of entries) {
    if (!entry || typeof entry !== "object") continue;
    const record = entry as Record<string, unknown>;
    const rxValue = Number(record.rxBytesPerSec ?? record.rx);
    const txValue = Number(record.txBytesPerSec ?? record.tx);
    if (Number.isFinite(rxValue)) rx.push(rxValue);
    if (Number.isFinite(txValue)) tx.push(txValue);
  }
  return { rx, tx };
}

function historyEntries(payload: unknown): unknown[] {
  if (Array.isArray(payload)) return payload;
  if (payload && typeof payload === "object") {
    const record = payload as Record<string, unknown>;
    for (const key of ["points", "values", "history", "data", "samples"]) {
      const candidate = record[key];
      if (Array.isArray(candidate)) return candidate;
    }
  }
  return [];
}

function componentStatusClass(status: string): string {
  switch (status) {
    case "ok":
      return "ok";
    case "warning":
      return "warn";
    case "error":
      return "error";
    default:
      return "";
  }
}

// ---------------------------------------------------------------------------

export const monitorApp: AppDefinition = {
  id: "monitor",
  name: "Resource Monitor",
  icon: "monitor",
  description: "Live CPU, memory, disk and network metrics",
  singleton: true,
  defaultSize: { w: 1020, h: 680 },
  minSize: { w: 380, h: 320 },
  async mount(ctx: AppContext) {
    const app = new MonitorApp(ctx);
    await app.start();
  },
};
