// One container's page in the Docker app: logs, configuration, a terminal and
// live usage, with start/stop/restart/delete in the header (Docker Desktop's
// container view). Mounted into the app's main area; the caller disposes it.

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { formatBytes, formatDateTime, formatPercent, formatTime, relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { confirmDialog, contextMenu, copyText, errorMessage, sparkline, type ContextMenuEntry } from "../../core/ui";
import { isAbortError, type AppLifecycle } from "../../components/lifecycle";
import {
  containerStateLabel,
  containerTone,
  dockerTime,
  isRunning,
  maskEnv,
  portLinks,
  pushSample,
  shortId,
  type ContainerDetail,
  type ContainerStat,
  type LogLine,
} from "./model";
import { mountTerminal, type TerminalView } from "./terminal";

export interface ContainerPageDeps {
  ctx: AppContext;
  lc: AppLifecycle;
  id: string;
  canChange: () => boolean;
  onBack: () => void;
  /** Lists changed (started, stopped, deleted…). */
  onChanged: () => void;
  openApp: (appId: string) => void;
  importContainers: (ids: string[], label: string) => Promise<boolean>;
}

export interface ContainerPage {
  el: HTMLElement;
  /** Re-read the container (after an outside change). */
  refresh(): void;
  dispose(): void;
}

type Tab = "logs" | "inspect" | "terminal" | "stats";

const TAILS = [300, 1000, 2000];
const LOG_POLL_MS = 2500;
const STATS_POLL_MS = 3000;
const MAX_LOG_LINES = 2000;

export function mountContainerPage(deps: ContainerPageDeps): ContainerPage {
  const { ctx, lc } = deps;
  const path = `/api/docker/containers/${encodeURIComponent(deps.id)}`;
  let disposed = false;
  let detail: ContainerDetail | null = null;
  let loadError: unknown = null;
  let tab: Tab = "logs";
  let busy = false;
  const timers: number[] = [];
  const stopTimers = (): void => {
    for (const id of timers.splice(0)) lc.clearInterval(id);
  };

  const head = h("div", { class: "dk-page-head" });
  const tabs = h("div", { class: "tabs dk-tabs", role: "tablist", ariaLabel: "Container views" });
  const body = h("div", { class: "dk-page-body" });
  const el = h("div", { class: "dk-page" }, head, tabs, body);

  // ------------------------------------------------------------------ data

  async function load(): Promise<void> {
    try {
      const next = await ctx.api.get<ContainerDetail>(path, undefined, { signal: lc.signal });
      if (disposed) return;
      detail = next;
      loadError = null;
    } catch (error) {
      if (disposed || isAbortError(error)) return;
      loadError = error;
    }
    renderHead();
    renderTabs();
    // A running log view or terminal keeps going; everything else repaints.
    const live = (tab === "logs" && logsMounted) || (tab === "terminal" && terminal !== null);
    if (!live) renderBody();
  }

  // ---------------------------------------------------------------- header

  function renderHead(): void {
    clear(head);
    const back = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Back to containers", ariaLabel: "Back to containers" }, iconSvg("arrowLeft", 16));
    back.addEventListener("click", () => deps.onBack());
    if (!detail) {
      head.append(h("div", { class: "dk-page-title-row" }, back, h("h2", { class: "dk-page-title" }, loadError ? "Container not available" : "Loading…")));
      if (loadError) head.append(h("p", { class: "dk-page-sub muted" }, errorMessage(loadError)));
      return;
    }
    const c = detail.container;
    const running = isRunning(c);
    const actions = h("div", { class: "dk-page-actions" });
    if (deps.canChange() && busy) {
      actions.append(h("span", { class: "dk-busy muted small" }, h("span", { class: "spinner spinner-sm" }), "Working…"));
    } else if (deps.canChange()) {
      if (running) {
        actions.append(
          actionButton("stop", "Stop", () => void act("stop")),
          actionButton("restart", "Restart", () => void act("restart")),
        );
      } else {
        actions.append(actionButton("play", "Start", () => void act("start"), true));
      }
      const more = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "More actions", ariaLabel: "More actions", disabled: busy }, iconSvg("more", 16));
      more.addEventListener("click", () => {
        const rect = more.getBoundingClientRect();
        contextMenu(rect.right - 200, rect.bottom + 4, menuItems(), { ariaLabel: c.name });
      });
      actions.append(more);
    }
    const links = portLinks(c.ports, window.location);
    const sub = h(
      "div",
      { class: "dk-page-sub" },
      h("span", { class: "mono", title: c.image }, c.image),
      h("span", { class: "dk-sep", ariaHidden: true }, "·"),
      h("span", { class: "mono muted", title: c.id }, shortId(c.id)),
      ...links.slice(0, 3).flatMap((link) => [
        h("span", { class: "dk-sep", ariaHidden: true }, "·"),
        link.href
          ? h("a", { class: "dk-port", href: link.href, target: "_blank", rel: "noopener noreferrer" }, link.label, iconSvg("external", 11))
          : h("span", { class: "dk-port is-plain" }, link.label),
      ]),
    );
    if (detail.managed && detail.appId) {
      const appLink = h("button", { class: "dk-link", type: "button" }, `App: ${detail.slug}`);
      const appId = detail.appId;
      appLink.addEventListener("click", () => deps.openApp(appId));
      sub.append(h("span", { class: "dk-sep", ariaHidden: true }, "·"), appLink);
    }
    head.append(
      h(
        "div",
        { class: "dk-page-title-row" },
        back,
        h("span", { class: `dk-dot is-${containerTone(c)}`, ariaHidden: true }),
        h("h2", { class: "dk-page-title", title: c.name }, c.name),
        h("span", { class: "dk-state muted" }, containerStateLabel(c)),
        h("div", { class: "app-toolbar-spacer" }),
        actions,
      ),
      sub,
    );
  }

  function actionButton(icon: string, label: string, onClick: () => void, primary = false): HTMLButtonElement {
    const button = h("button", { class: `btn ${primary ? "btn-primary" : "btn-ghost"} btn-sm`, type: "button", disabled: busy }, iconSvg(icon, 14), h("span", {}, label));
    button.addEventListener("click", onClick);
    return button;
  }

  function menuItems(): ContextMenuEntry[] {
    if (!detail) return [];
    const c = detail.container;
    const items: ContextMenuEntry[] = [];
    const link = portLinks(c.ports, window.location).find((l) => l.href);
    if (link?.href) {
      const href = link.href;
      items.push({ label: `Open ${link.label.split(":")[0]} in browser`, icon: "external", onClick: () => window.open(href, "_blank", "noopener") });
    }
    items.push({
      label: "Copy container ID",
      icon: "copy",
      onClick: () => void copyText(c.id).then((ok) => (ok ? ctx.toast.success("Container ID copied") : ctx.toast.error("Could not copy"))),
    });
    if (detail.managed && detail.appId) {
      const appId = detail.appId;
      items.push("separator", { label: "Open app", icon: "docker", onClick: () => deps.openApp(appId) });
    } else {
      items.push(
        "separator",
        { label: "Manage as RAST app…", icon: "download", onClick: () => void importThis() },
        { label: "Delete…", icon: "trash", danger: true, onClick: () => void removeThis() },
      );
    }
    return items;
  }

  async function act(action: "start" | "stop" | "restart"): Promise<void> {
    if (!detail || busy) return;
    const c = detail.container;
    busy = true;
    renderHead();
    const done = { start: "started", stop: "stopped", restart: "restarted" } as const;
    try {
      await ctx.api.post(`${path}/${action}`);
      ctx.toast.success(`${c.name} ${done[action]}`);
    } catch (error) {
      ctx.toast.error(`${c.name}: ${errorMessage(error)}`);
    } finally {
      busy = false;
    }
    if (disposed) return;
    deps.onChanged();
    await load();
    // The shell ended with the container; show the new state (or a new shell).
    if (tab === "logs" || tab === "terminal") renderBody();
  }

  async function importThis(): Promise<void> {
    if (!detail) return;
    const c = detail.container;
    const ok = await deps.importContainers([c.id], c.name);
    if (ok && !disposed) await load();
  }

  async function removeThis(): Promise<void> {
    if (!detail) return;
    const c = detail.container;
    const confirmed = await confirmDialog({
      title: "Delete container",
      message: `Delete “${c.name}”? It is stopped and removed. Its volumes and data stay.`,
      confirmLabel: "Delete",
      danger: true,
      requireTyped: c.name,
    });
    if (!confirmed || disposed) return;
    try {
      await ctx.api.del(path, { confirm: c.name });
      ctx.toast.success(`${c.name} deleted`);
      deps.onChanged();
      deps.onBack();
    } catch (error) {
      ctx.toast.error(`Could not delete ${c.name}: ${errorMessage(error)}`);
    }
  }

  // ------------------------------------------------------------------ tabs

  function renderTabs(): void {
    clear(tabs);
    const entries: Array<[Tab, string]> = [
      ["logs", "Logs"],
      ["inspect", "Inspect"],
      ["terminal", "Terminal"],
      ["stats", "Stats"],
    ];
    for (const [id, label] of entries) {
      const button = h("button", { class: `tab${tab === id ? " is-active" : ""}`, type: "button", role: "tab", ariaSelected: tab === id }, label);
      button.addEventListener("click", () => {
        if (tab === id) return;
        tab = id;
        renderTabs();
        renderBody();
      });
      tabs.append(button);
    }
  }

  function renderBody(): void {
    stopTimers();
    logsMounted = false;
    terminal?.dispose();
    terminal = null;
    clear(body);
    if (!detail) {
      if (loadError) body.append(h("div", { class: "dk-empty" }, h("p", {}, errorMessage(loadError)), retryButton()));
      else body.append(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading…"));
      return;
    }
    if (tab === "logs") mountLogs();
    else if (tab === "inspect") renderInspect();
    else if (tab === "terminal") mountTerminalTab();
    else mountStats();
  }

  function retryButton(): HTMLElement {
    const retry = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, "Try again");
    retry.addEventListener("click", () => void load());
    return retry;
  }

  // ------------------------------------------------------------------ logs

  let logsMounted = false;
  let logLines: LogLine[] = [];
  let lastTs = 0;
  let seenAtLast = new Set<string>();
  let follow = true;
  let tail = TAILS[0]!;
  let filter = "";
  let logPre: HTMLElement | null = null;
  let logStatus: HTMLElement | null = null;
  let polling = false;

  function restartLogs(): void {
    if (tab !== "logs" || !detail) return;
    renderBody();
  }

  function mountLogs(): void {
    logsMounted = true;
    logLines = [];
    lastTs = 0;
    seenAtLast = new Set();
    const search = h("input", { class: "input input-search dk-log-search", type: "search", placeholder: "Search logs", ariaLabel: "Search logs", value: filter });
    search.addEventListener("input", () => {
      filter = search.value;
      paintLogs(true);
    });
    const tailSelect = h("select", { class: "input dk-log-tail", ariaLabel: "Lines to show" }) as HTMLSelectElement;
    for (const value of TAILS) tailSelect.append(h("option", { value: String(value), selected: value === tail }, `Last ${value} lines`));
    tailSelect.addEventListener("change", () => {
      tail = Number(tailSelect.value) || TAILS[0]!;
      restartLogs();
    });
    const followButton = h("button", { class: `btn btn-ghost btn-sm${follow ? " is-on" : ""}`, type: "button", ariaPressed: follow, title: "Show new lines as they arrive" }, iconSvg(follow ? "pause" : "play", 13), h("span", {}, follow ? "Pause" : "Follow"));
    followButton.addEventListener("click", () => {
      follow = !follow;
      restartLogs();
    });
    const copyButton = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Copy logs", ariaLabel: "Copy logs" }, iconSvg("copy", 14));
    copyButton.addEventListener("click", () => {
      const text = visibleLines().map((line) => `${line.ts ? new Date(line.ts).toISOString() : ""} ${line.message}`).join("\n");
      void copyText(text).then((ok) => (ok ? ctx.toast.success("Logs copied") : ctx.toast.error("Could not copy")));
    });
    logStatus = h("span", { class: "dk-log-status muted small", role: "status" });
    logPre = h("div", { class: "dk-log", role: "log", ariaLabel: "Container logs", tabIndex: 0 });
    body.append(h("div", { class: "dk-log-toolbar" }, search, tailSelect, followButton, copyButton, h("div", { class: "app-toolbar-spacer" }), logStatus), logPre);
    void pollLogs(true);
    if (follow) timers.push(lc.setInterval(() => void pollLogs(false), LOG_POLL_MS));
  }

  function lineKey(line: LogLine): string {
    return `${line.stream}|${line.message}`;
  }

  async function pollLogs(initial: boolean): Promise<void> {
    if (polling || disposed || !logPre) return;
    if (!initial && (document.hidden || ctx.window.isMinimized())) return;
    polling = true;
    const query: Record<string, unknown> = { tail: initial ? tail : MAX_LOG_LINES };
    if (!initial && lastTs > 0) query.since = Math.floor(lastTs / 1000);
    try {
      const data = await ctx.api.get<{ lines?: LogLine[] }>(`${path}/logs`, query, { signal: lc.signal });
      if (disposed || !logPre) return;
      const incoming = Array.isArray(data.lines) ? data.lines : [];
      const fresh: LogLine[] = [];
      for (const line of incoming) {
        if (!initial && line.ts && line.ts < lastTs) continue;
        if (!initial && line.ts === lastTs && seenAtLast.has(lineKey(line))) continue;
        fresh.push(line);
      }
      for (const line of fresh) {
        if (line.ts > lastTs) {
          lastTs = line.ts;
          seenAtLast = new Set();
        }
        if (line.ts === lastTs) seenAtLast.add(lineKey(line));
      }
      logLines = initial ? fresh : [...logLines, ...fresh];
      if (logLines.length > MAX_LOG_LINES) logLines = logLines.slice(logLines.length - MAX_LOG_LINES);
      if (logStatus) logStatus.textContent = follow ? `Live · ${formatTime(Date.now())}` : `Paused · ${logLines.length} lines`;
      paintLogs(initial || fresh.length > 0);
    } catch (error) {
      if (disposed || isAbortError(error)) return;
      if (logStatus) logStatus.textContent = errorMessage(error);
      if (initial && logPre) logPre.textContent = "";
    } finally {
      polling = false;
    }
  }

  function visibleLines(): LogLine[] {
    const needle = filter.trim().toLowerCase();
    return needle ? logLines.filter((line) => line.message.toLowerCase().includes(needle)) : logLines;
  }

  function paintLogs(changed: boolean): void {
    if (!logPre || !changed) return;
    const atBottom = logPre.scrollTop + logPre.clientHeight >= logPre.scrollHeight - 8;
    const lines = visibleLines();
    if (lines.length === 0) {
      logPre.replaceChildren(h("div", { class: "dk-log-empty muted" }, logLines.length === 0 ? "No log output." : "No lines match your search."));
      return;
    }
    const fragment = document.createDocumentFragment();
    for (const line of lines) {
      fragment.append(
        h(
          "div",
          { class: `dk-log-line${line.stream === "stderr" ? " is-stderr" : ""}` },
          h("span", { class: "dk-log-time", title: line.ts ? formatDateTime(line.ts) : "" }, line.ts ? formatTime(line.ts) : ""),
          h("span", { class: "dk-log-text" }, line.message),
        ),
      );
    }
    logPre.replaceChildren(fragment);
    if (atBottom || logLines.length === lines.length) logPre.scrollTop = logPre.scrollHeight;
  }

  // --------------------------------------------------------------- inspect

  let showEnv = false;

  function renderInspect(): void {
    if (!detail) return;
    const info = detail.inspect;
    const kv = (label: string, value: string | Node | null | undefined): HTMLElement | null => {
      if (value === null || value === undefined || value === "") return null;
      return h("div", { class: "dk-kv" }, h("div", { class: "dk-kv-key" }, label), h("div", { class: "dk-kv-value" }, value));
    };
    const created = dockerTime(info.createdAt);
    const started = dockerTime(info.startedAt);
    const finished = dockerTime(info.finishedAt);
    const section = (title: string, ...children: Array<Node | null>): HTMLElement | null => {
      const items = children.filter((child): child is Node => Boolean(child));
      if (items.length === 0) return null;
      return h("section", { class: "dk-section" }, h("h3", { class: "dk-section-title" }, title), ...items);
    };
    const overview = section(
      "Overview",
      h(
        "div",
        { class: "dk-kv-grid" },
        kv("Image", h("span", { class: "mono" }, info.image)),
        kv("Command", info.command ? h("span", { class: "mono" }, info.command) : null),
        kv("Entrypoint", info.entrypoint ? h("span", { class: "mono" }, info.entrypoint) : null),
        kv("Working folder", info.workingDir ? h("span", { class: "mono" }, info.workingDir) : null),
        kv("User", info.user),
        kv("Restart", info.restartPolicy || "no"),
        kv("Created", created ? `${formatDateTime(created)} (${relativeTime(created)})` : null),
        kv(isRunning(detail.container) ? "Started" : "Last started", started ? `${formatDateTime(started)} (${relativeTime(started)})` : null),
        !isRunning(detail.container) ? kv("Stopped", finished ? `${formatDateTime(finished)} (${relativeTime(finished)})` : null) : null,
        !isRunning(detail.container) && info.exitCode !== null ? kv("Exit code", String(info.exitCode)) : null,
        kv("Health", info.health),
        kv("Network", [info.networkMode, ...info.networks.filter((n) => n !== info.networkMode)].filter(Boolean).join(", ")),
        info.privileged ? kv("Privileged", "Yes") : null,
      ),
    );
    const ports = info.ports.filter((p) => p.hostPort);
    const portsSection = section(
      "Ports",
      ports.length
        ? h(
            "div",
            { class: "dk-list" },
            ...ports.map((p) =>
              h("div", { class: "dk-list-row mono" }, `${p.hostIp ?? "0.0.0.0"}:${p.hostPort}`, h("span", { class: "muted" }, " → "), `${p.containerPort}/${p.protocol}`),
            ),
          )
        : null,
    );
    const mounts = section(
      "Storage",
      info.mounts.length
        ? h(
            "div",
            { class: "dk-list" },
            ...info.mounts.map((m) =>
              h(
                "div",
                { class: "dk-list-row" },
                h("span", { class: "dk-chip" }, m.type === "volume" ? "Volume" : m.type === "bind" ? "Folder" : m.type),
                h("span", { class: "mono dk-mount-source", title: m.source }, m.type === "volume" && m.name ? m.name : m.source),
                h("span", { class: "muted" }, " → "),
                h("span", { class: "mono" }, m.target),
                m.readOnly ? h("span", { class: "dk-chip is-quiet" }, "read-only") : null,
              ),
            ),
          )
        : null,
    );
    const envToggle = h("button", { class: "btn btn-ghost btn-sm", type: "button", ariaPressed: showEnv }, iconSvg(showEnv ? "eyeOff" : "eye", 13), h("span", {}, showEnv ? "Hide values" : "Show values"));
    envToggle.addEventListener("click", () => {
      showEnv = !showEnv;
      renderBody();
    });
    const env = info.env.length
      ? h(
          "section",
          { class: "dk-section" },
          h("div", { class: "dk-section-head" }, h("h3", { class: "dk-section-title" }, "Environment"), envToggle),
          h(
            "div",
            { class: "dk-list" },
            ...info.env.map((entry) => {
              const { key, value } = maskEnv(entry);
              return h("div", { class: "dk-list-row mono" }, h("span", { class: "dk-env-key" }, key), h("span", { class: "muted" }, "="), h("span", { class: "dk-env-value" }, showEnv ? value : value ? "••••" : ""));
            }),
          ),
        )
      : null;
    const labelEntries = Object.entries(info.labels ?? {});
    const labels = labelEntries.length
      ? h(
          "details",
          { class: "dk-section dk-details" },
          h("summary", { class: "dk-section-title" }, `Labels (${labelEntries.length})`),
          h("div", { class: "dk-list" }, ...labelEntries.map(([k, v]) => h("div", { class: "dk-list-row mono" }, h("span", { class: "dk-env-key" }, k), h("span", { class: "muted" }, "="), h("span", {}, v)))),
        )
      : null;
    body.append(h("div", { class: "dk-inspect" }, overview, portsSection, mounts, env, labels));
  }

  // ----------------------------------------------------------------- stats

  // -------------------------------------------------------------- terminal

  let terminal: TerminalView | null = null;

  function mountTerminalTab(): void {
    if (!detail) return;
    const c = detail.container;
    const empty = (icon: string, title: string, text: string, action?: HTMLElement): void => {
      body.append(h("div", { class: "dk-empty dk-term-empty" }, h("span", { class: "dk-term-empty-icon" }, iconSvg(icon, 28)), h("p", { class: "dk-term-empty-title" }, title), h("p", { class: "muted" }, text), action ?? null));
    };
    if (!deps.canChange()) {
      empty("lock", "Terminal not available", "You have read-only access to Docker.");
      return;
    }
    if (!detail.terminal.allowed) {
      empty("lock", "Terminal turned off for this container", detail.terminal.reason);
      return;
    }
    if (!isRunning(c)) {
      const start = h("button", { class: "btn btn-primary btn-sm", type: "button", disabled: busy }, iconSvg("play", 13), h("span", {}, "Start container"));
      start.addEventListener("click", () => void act("start"));
      empty("terminal", "The container is stopped", "Start it to open a terminal.", start);
      return;
    }
    terminal = mountTerminal({ ctx, id: c.id, name: c.name });
    body.append(terminal.el);
  }

  let cpuSeries: number[] = [];
  let memSeries: number[] = [];
  let lastStat: ContainerStat | null = null;

  function mountStats(): void {
    if (!detail) return;
    const host = h("div", { class: "dk-stats" });
    body.append(host);
    if (!isRunning(detail.container)) {
      host.append(h("div", { class: "dk-empty" }, h("p", { class: "muted" }, "Start the container to see its live usage.")));
      return;
    }
    const paint = (): void => {
      clear(host);
      const s = lastStat;
      const tile = (label: string, value: string, extra?: Node | null, detailText?: string): HTMLElement =>
        h("div", { class: "dk-stat" }, h("div", { class: "dk-stat-label" }, label), h("div", { class: "dk-stat-value" }, value), detailText ? h("div", { class: "dk-stat-detail muted" }, detailText) : null, extra ?? null);
      if (!s) {
        host.append(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Measuring…"));
        return;
      }
      host.append(
        tile("CPU", formatPercent(s.cpuPercent, 1), sparkline(cpuSeries, { width: 220, height: 40 })),
        tile("Memory", formatBytes(s.memoryBytes), sparkline(memSeries, { width: 220, height: 40, color: "var(--accent-2, var(--accent))" }), s.memoryLimitBytes ? `of ${formatBytes(s.memoryLimitBytes)} (${formatPercent(s.memoryPercent, 1)})` : undefined),
        tile("Network", `↓ ${formatBytes(s.netRxBytes)}`, null, `↑ ${formatBytes(s.netTxBytes)} sent`),
        tile("Disk", `${formatBytes(s.blockReadBytes)} read`, null, `${formatBytes(s.blockWriteBytes)} written`),
        tile("Processes", String(s.pids)),
      );
    };
    const sample = async (): Promise<void> => {
      if (disposed || document.hidden || ctx.window.isMinimized()) return;
      try {
        const data = await ctx.api.get<{ items?: ContainerStat[] }>("/api/docker/stats", undefined, { signal: lc.signal });
        if (disposed || !detail) return;
        const c = detail.container;
        const found = (data.items ?? []).find((item) => item.id === c.id || c.id.startsWith(item.id) || item.name === c.name) ?? null;
        lastStat = found;
        if (found) {
          cpuSeries = pushSample(cpuSeries, found.cpuPercent);
          memSeries = pushSample(memSeries, found.memoryBytes);
        }
        if (tab === "stats") paint();
      } catch (error) {
        if (disposed || isAbortError(error)) return;
        clear(host);
        host.append(h("div", { class: "dk-empty" }, h("p", { class: "muted" }, `Usage is not available: ${errorMessage(error)}`)));
      }
    };
    paint();
    void sample();
    timers.push(lc.setInterval(() => void sample(), STATS_POLL_MS));
  }

  renderHead();
  renderTabs();
  renderBody();
  void load();

  return {
    el,
    refresh: () => void load(),
    dispose: () => {
      disposed = true;
      stopTimers();
      terminal?.dispose();
      terminal = null;
      el.remove();
    },
  };
}
