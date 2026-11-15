// RAST Terminal: a first-class terminal app using xterm.js over a WebSocket to
// the native PTY backend (see model.ts for the pure protocol helpers).
//
// Window model: the app is NOT a singleton and the window manager opens one
// window per launch (core/wm.ts keys windows by id, not app id), so every
// "New window" / File Station "Open in Terminal" click gets its own window
// with its own tabs. Tabs multiplex terminal sessions inside one window; a
// window closes when its last tab closes.
//
// Keyboard policy: only Ctrl/Cmd+Shift shortcuts are taken (copy, paste,
// search, new tab) through a capture listener and xterm's custom handler.
// Plain Ctrl+C/D/Z, Ctrl+V (readline), Tab completion, arrows and Unicode go
// to the shell untouched. Browser-reserved chords stay browser-controlled:
// Ctrl+W / Ctrl+Shift+W, Ctrl+T / Ctrl+Shift+N, Ctrl+Shift+I/J/C (devtools),
// Ctrl+L, F5/F11. Every action also has a toolbar button, so nothing is
// unreachable when the browser owns a chord.

import type { AppContext, AppDefinition } from "../../core/app";
import { ApiError } from "../../core/api";
import { AppLifecycle } from "../../components/lifecycle";
import { clear, h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { promptDialog } from "../../core/ui";
import { NoticeBar } from "../../components/live";
import { renderLoadFailure } from "../../components/states";
import {
  activateTab as activateTabModel,
  activeTab,
  addTab as addTabModel,
  closeTab as closeTabModel,
  createTabSet,
  normalizeTerminalEnv,
  normalizeTerminalParams,
  parseTerminalSessions,
  readFontSize,
  runtimeSummary,
  stepFontSize,
  terminalExitNotice,
  terminalFolderLabel,
  terminalPhaseLabel,
  terminalShortcut,
  terminalTabTitle,
  TERMINAL_ENV_PATH,
  TERMINAL_SESSIONS_PATH,
  validateRuntimeOverrides,
  writeFontSize,
  type TerminalEnv,
  type TerminalFolder,
  type TerminalSessionInfo,
  type TerminalState,
  type TerminalTabSet,
} from "./model";
import { TerminalSession } from "./session";
import { openRuntimePanel } from "./runtime-panel";

interface TabEntry {
  id: string;
  ordinal: number;
  folder: TerminalFolder | null;
  session: TerminalSession;
  state: TerminalState;
  pane: HTMLElement;
  overlay: HTMLElement;
  tabRoot: HTMLElement;
  tabMain: HTMLButtonElement;
  titleEl: HTMLElement;
  closeButton: HTMLButtonElement;
}

/**
 * Running shells already shown by a tab of this page, so two Terminal windows
 * restored together do not both attach to the same one.
 */
const claimedSessions = new Set<string>();
/** A viewer that is going away (page reload) is seen as gone within this time. */
const RESTORE_RECHECK_MS = 2_000;

/** localStorage is unavailable in some privacy modes; never let that break the app. */
function localStorageOrNull(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

class TerminalApp {
  private readonly ctx: AppContext;
  private readonly lifecycle: AppLifecycle;
  private readonly el: HTMLElement;
  private readonly notice: NoticeBar;
  private readonly toolbar: HTMLElement;
  private readonly tabbar: HTMLElement;
  private readonly body: HTMLElement;
  private readonly panesHost: HTMLElement;
  private readonly stateHost: HTMLElement;
  private readonly statusbar: HTMLElement;
  private readonly newTabButton: HTMLButtonElement;
  private readonly newWindowButton: HTMLButtonElement;
  private readonly copyButton: HTMLButtonElement;
  private readonly pasteButton: HTMLButtonElement;
  private readonly clearButton: HTMLButtonElement;
  private readonly searchButton: HTMLButtonElement;
  private readonly envButton: HTMLButtonElement;
  private readonly fontDownButton: HTMLButtonElement;
  private readonly fontUpButton: HTMLButtonElement;
  private readonly fontSizeLabel: HTMLElement;
  private readonly statusText: HTMLElement;
  private readonly statusDot: HTMLElement;
  private readonly statusShell: HTMLElement;
  private readonly statusRoot: HTMLElement;
  private readonly statusCwd: HTMLElement;
  private readonly statusSession: HTMLElement;
  private readonly statusRuntimes: HTMLElement;
  private readonly tabNewButton: HTMLButtonElement;
  private readonly searchOverlay: HTMLElement;
  private readonly searchInput: HTMLInputElement;
  private readonly searchCount: HTMLElement;

  private tabs: TerminalTabSet<TabEntry> = createTabSet();
  private env: TerminalEnv | null = null;
  private envError: unknown = null;
  private overrides: Record<string, string> = {};
  private fontSize: number;
  private tabSeq = 0;
  private tabOrdinal = 0;
  private disposed = false;
  private searchOpen = false;
  private searchCaseSensitive = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lifecycle = new AppLifecycle(ctx);
    this.fontSize = readFontSize(localStorageOrNull());
    this.notice = new NoticeBar(this.lifecycle);

    // ------------------------------------------------------------- toolbar
    this.newTabButton = this.iconButton("plus", "New tab (Ctrl+Shift+T)", () => this.addTab(null), "New tab");
    this.copyButton = this.iconButton("copy", "Copy selection (Ctrl+Shift+C)", () => void this.copyActive(), "Copy");
    this.pasteButton = this.iconButton("download", "Paste (Ctrl+Shift+V)", () => void this.pasteActive(), "Paste");
    this.clearButton = this.iconButton("trash", "Clear scrollback", () => this.clearActive());
    this.searchButton = this.iconButton("search", "Find in terminal (Ctrl+Shift+F)", () => this.openSearch());
    this.envButton = this.iconButton("settings", "Runtime environment", () => this.openRuntimePanel(), "Runtimes");
    this.newWindowButton = this.iconButton("external", "New Terminal window", () => this.openNewWindow());

    this.fontDownButton = h(
      "button",
      { class: "btn btn-ghost btn-icon btn-sm term-font-btn", type: "button", title: "Decrease font size", ariaLabel: "Decrease font size" },
      "−",
    );
    this.fontUpButton = h(
      "button",
      { class: "btn btn-ghost btn-icon btn-sm term-font-btn", type: "button", title: "Increase font size", ariaLabel: "Increase font size" },
      "+",
    );
    this.fontSizeLabel = h("span", { class: "term-fontsize", ariaLive: "polite" });
    this.fontDownButton.addEventListener("click", () => this.setFontSize(stepFontSize(this.fontSize, -1)));
    this.fontUpButton.addEventListener("click", () => this.setFontSize(stepFontSize(this.fontSize, 1)));

    this.statusDot = h("span", { class: "term-dot", ariaHidden: true });
    this.statusText = h("span", { class: "term-pill-text" }, "Loading…");
    const statusPill = h("span", { class: "term-pill", role: "status", ariaLive: "polite" }, this.statusDot, this.statusText);

    this.toolbar = h(
      "div",
      { class: "app-toolbar term-toolbar", role: "toolbar", ariaLabel: "Terminal actions" },
      this.newTabButton,
      this.copyButton,
      this.pasteButton,
      this.clearButton,
      this.searchButton,
      h("span", { class: "term-toolbar-sep", ariaHidden: true }),
      h("div", { class: "term-fontgroup" }, this.fontDownButton, this.fontSizeLabel, this.fontUpButton),
      h("div", { class: "app-toolbar-spacer" }),
      statusPill,
      this.envButton,
      this.newWindowButton,
    );

    // --------------------------------------------------------------- tabs
    this.tabbar = h("div", { class: "term-tabbar", role: "tablist", ariaLabel: "Terminal tabs" });
    this.tabNewButton = h("button", { class: "term-tab-new", type: "button", title: "New tab", ariaLabel: "New tab" }, "+");
    this.tabNewButton.addEventListener("click", () => this.addTab(null));

    // --------------------------------------------------------------- body
    this.panesHost = h("div", { class: "term-panes" });
    this.stateHost = h("div", { class: "term-state-host" });
    this.searchOverlay = this.buildSearchOverlay();
    this.searchInput = this.searchOverlay.querySelector<HTMLInputElement>(".term-search-input") as HTMLInputElement;
    this.searchCount = this.searchOverlay.querySelector<HTMLElement>(".term-search-count") as HTMLElement;
    this.body = h("div", { class: "term-body" }, this.panesHost, this.stateHost, this.searchOverlay);

    // ---------------------------------------------------------- status bar
    this.statusShell = h("span", { class: "term-status-item mono" }, "—");
    this.statusRoot = h(
      "span",
      {
        class: "term-status-item term-root-badge",
        title: "Administrator shell: runs as root on this server",
        hidden: true,
      },
      "root",
    );
    this.statusCwd = h("span", { class: "term-status-item term-status-cwd mono", title: "" }, "—");
    this.statusSession = h("span", { class: "term-status-item term-status-session mono muted" }, "");
    this.statusRuntimes = h("span", { class: "term-status-item term-status-runtimes muted" }, "");
    this.statusbar = h(
      "div",
      { class: "term-statusbar", role: "status", ariaLive: "off" },
      this.statusShell,
      this.statusRoot,
      this.statusCwd,
      h("span", { class: "term-status-spacer" }),
      this.statusRuntimes,
      this.statusSession,
    );

    this.el = h("div", { class: "app term-app", dataset: { phase: "loading" } }, this.toolbar, this.tabbar, this.notice.el, this.body, this.statusbar);
    this.ctx.root.classList.add("term-root");
    this.ctx.root.appendChild(this.el);

    // Window resize and disposal.
    this.lifecycle.add(ctx.onResize(() => this.refit()));
    // Non-singleton: the shell opens a new window instead of reusing this one,
    // but a shell that does support reopening gets an extra tab.
    ctx.onReopen((params) => this.addTab(normalizeTerminalParams(params)));
    this.lifecycle.add(() => this.cleanup());
    this.el.addEventListener("keydown", (event) => this.onKeyDown(event), true);

    this.renderFontSize();
    this.renderTabs();
    this.renderToolbarState();
  }

  async start(): Promise<void> {
    this.ctx.setTitle("Terminal");
    this.renderBodyState(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), h("span", { class: "spinner-label" }, "Loading terminal…")));
    await this.loadEnv();
  }

  // ------------------------------------------------------------- lifecycle

  private cleanup(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const entry of [...this.tabs.tabs]) this.disposeEntry(entry);
    this.tabs = createTabSet();
    this.el.remove();
  }

  private disposeEntry(entry: TabEntry): void {
    const sessionId = entry.session.status.sessionId;
    if (sessionId) claimedSessions.delete(sessionId);
    entry.session.dispose();
  }

  private refit(): void {
    for (const entry of this.tabs.tabs) entry.session.fitSoon();
  }

  // ------------------------------------------------------------ env + tabs

  private async loadEnv(): Promise<void> {
    if (this.disposed) return;
    this.renderBodyState(
      h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), h("span", { class: "spinner-label" }, "Loading terminal…")),
    );
    try {
      const raw = await this.ctx.api.get<unknown>(TERMINAL_ENV_PATH);
      if (this.disposed) return;
      const env = normalizeTerminalEnv(raw);
      this.env = env;
      this.envError = null;
      this.renderToolbarState();
      if (!env.enabled) {
        this.renderDisabled();
        return;
      }
      if (this.tabs.tabs.length === 0) {
        // Shells keep running on the server when the page is reloaded or the
        // browser is closed: show them again before starting anything new.
        const folder = normalizeTerminalParams(this.ctx.params);
        const restored = await this.restoreSessions(true);
        if (this.disposed) return;
        this.renderBodyState(null);
        if (folder || (restored === 0 && this.tabs.tabs.length === 0)) this.addTab(folder);
      } else {
        this.renderBodyState(null);
      }
    } catch (error) {
      if (this.disposed) return;
      this.env = null;
      this.envError = error;
      // The server reports a disabled terminal as 503 "disabled …" and a
      // revoked app permission as 403; both are states, not failures.
      if (error instanceof ApiError && error.isForbidden) {
        this.renderForbidden(error.message);
        return;
      }
      if (error instanceof ApiError && error.isUnavailable && /disabled/i.test(error.message)) {
        this.renderDisabled(error.message);
        return;
      }
      this.renderEnvFailure(error);
    }
  }

  private renderDisabled(detail?: string): void {
    this.tabbar.hidden = true;
    this.stateHost.replaceChildren(
      h(
        "div",
        { class: "term-blocked", role: "status" },
        h("div", { class: "term-blocked-icon" }, iconSvg("terminal", 34)),
        h("div", { class: "term-blocked-title" }, "Terminal is disabled by the administrator"),
        h(
          "div",
          { class: "term-blocked-detail small muted" },
          detail ?? "This account may not open terminals on this server. File Station and the rest of the desktop keep working.",
        ),
      ),
    );
    this.stateHost.hidden = false;
    this.statusText.textContent = "Disabled";
    this.el.dataset.phase = "failed";
    this.renderToolbarState();
  }

  private renderForbidden(detail?: string): void {
    this.tabbar.hidden = true;
    this.stateHost.replaceChildren(
      h(
        "div",
        { class: "term-blocked", role: "status" },
        h("div", { class: "term-blocked-icon" }, iconSvg("lock", 30)),
        h("div", { class: "term-blocked-title" }, "Terminal is not available for your account"),
        h("div", { class: "term-blocked-detail small muted" }, detail ?? "Ask an administrator for access to the Terminal app."),
      ),
    );
    this.stateHost.hidden = false;
    this.statusText.textContent = "Forbidden";
    this.el.dataset.phase = "failed";
    this.renderToolbarState();
  }

  private renderEnvFailure(error: unknown): void {
    this.tabbar.hidden = true;
    const host = h("div", { class: "term-env-failure" });
    renderLoadFailure(host, error, {
      title: "Terminal unavailable",
      hint: "The Terminal app needs the RAST native terminal service. Ask an administrator to finish enabling it.",
      retry: () => void this.loadEnv(),
    });
    this.renderBodyState(host);
    this.statusText.textContent = "Unavailable";
    this.el.dataset.phase = "failed";
    this.renderToolbarState();
  }

  /**
   * Open a tab for every running shell of this account that nobody is
   * viewing. Returns how many were attached. A failure only means nothing is
   * restored: the caller opens a fresh shell.
   */
  private async restoreSessions(recheck: boolean): Promise<number> {
    let sessions: TerminalSessionInfo[] | null = null;
    try {
      sessions = parseTerminalSessions(await this.ctx.api.get<unknown>(TERMINAL_SESSIONS_PATH));
    } catch {
      return 0;
    }
    if (this.disposed || !sessions || !this.env?.enabled) return 0;
    let restored = 0;
    let busy = false;
    for (const info of sessions) {
      if (claimedSessions.has(info.id)) continue;
      if (info.viewers > 0) {
        busy = true;
        continue;
      }
      this.addTab(null, info.id);
      restored += 1;
    }
    // Right after a reload the server may still count the old page as a
    // viewer; look once more when it has noticed.
    if (busy && recheck) {
      window.setTimeout(() => {
        if (!this.disposed) void this.restoreSessions(false);
      }, RESTORE_RECHECK_MS);
    }
    return restored;
  }

  private addTab(folder: TerminalFolder | null, attach: string | null = null): void {
    if (this.disposed || !this.env?.enabled) return;
    if (attach) claimedSessions.add(attach);
    const id = `term-tab-${++this.tabSeq}`;
    const ordinal = ++this.tabOrdinal;
    const runtimes = validateRuntimeOverrides(this.env, this.overrides);
    const overlay = h("div", { class: "term-pane-state hidden", role: "status" });
    const pane = h("div", { class: "term-pane", dataset: { tabId: id } }, overlay);
    const session = new TerminalSession({
      api: this.ctx.api,
      folder,
      runtimes: attach ? {} : runtimes,
      attach,
      fontSize: this.fontSize,
      onState: (state) => this.onSessionState(id, state),
      onSearchResults: (index, count) => this.onSearchResults(index, count),
    });
    pane.appendChild(session.el);

    const titleEl = h("span", { class: "term-tab-title" });
    const tabMain = h(
      "button",
      { class: "term-tab-main", type: "button", role: "tab", ariaSelected: false },
      h("span", { class: "term-tab-dot", ariaHidden: true }),
      titleEl,
    );
    const closeButton = h("button", { class: "term-tab-close", type: "button", title: "Close tab", ariaLabel: "Close tab" }, "×");
    const tabRoot = h("div", { class: "term-tab", role: "presentation", dataset: { tabId: id } }, tabMain, closeButton);
    tabMain.addEventListener("click", () => this.activateTab(id));
    closeButton.addEventListener("click", (event) => {
      event.stopPropagation();
      this.closeTab(id);
    });

    const entry: TabEntry = {
      id,
      ordinal,
      folder,
      session,
      state: session.status,
      pane,
      overlay,
      tabRoot,
      tabMain,
      titleEl,
      closeButton,
    };
    this.tabs = addTabModel(this.tabs, entry);
    this.panesHost.appendChild(pane);
    this.renderTabs();
    this.activateTab(id);
    this.renderToolbarState();
    void session.start();
  }

  private closeTab(id: string): void {
    const entry = this.tabs.tabs.find((tab) => tab.id === id);
    if (!entry) return;
    this.disposeEntry(entry);
    entry.pane.remove();
    const result = closeTabModel(this.tabs, id);
    this.tabs = result.set;
    this.renderTabs();
    if (result.closedLast) {
      this.ctx.window.close();
      return;
    }
    this.applyActiveTab(true);
  }

  private activateTab(id: string): void {
    if (id !== this.tabs.activeId) {
      // Search highlights belong to the terminal that was being searched.
      if (this.searchOpen) this.closeSearch();
      activeTab(this.tabs)?.session.clearSearch();
      this.searchCount.textContent = "";
    }
    this.tabs = activateTabModel(this.tabs, id);
    this.applyActiveTab(true);
  }

  private applyActiveTab(focus: boolean): void {
    for (const entry of this.tabs.tabs) {
      const active = entry.id === this.tabs.activeId;
      entry.pane.classList.toggle("is-active", active);
      entry.tabRoot.classList.toggle("is-active", active);
      entry.tabMain.setAttribute("aria-selected", String(active));
      entry.tabMain.tabIndex = active ? 0 : -1;
    }
    const current = activeTab(this.tabs);
    if (current) {
      current.session.fitSoon();
      if (focus && current.state.phase === "connected") current.session.focus();
    }
    this.renderTabs();
    this.renderToolbarState();
    this.renderStatusbar();
    this.updateWindowTitle();
  }

  private onSessionState(id: string, state: TerminalState): void {
    const entry = this.tabs.tabs.find((tab) => tab.id === id);
    if (!entry) return;
    const previous = entry.state;
    entry.state = state;
    if (state.sessionId) claimedSessions.add(state.sessionId);
    entry.tabRoot.dataset.phase = state.phase;
    entry.pane.dataset.phase = state.phase;
    const title = this.tabTitle(entry);
    entry.titleEl.textContent = title;
    entry.tabMain.setAttribute("aria-label", title);
    entry.tabMain.title = title;
    this.renderOverlay(entry);

    if (previous.phase !== state.phase) {
      if (state.phase === "failed" && state.error) {
        this.ctx.toast.error(state.error);
        this.notice.show(state.error, { kind: "error", timeoutMs: 0, action: { label: "Retry", onClick: () => entry.session.retry() } });
      } else if (state.phase === "disconnected" && state.error) {
        this.ctx.toast.warning(state.error);
        this.notice.show(state.error, { kind: "warning", timeoutMs: 0, action: { label: "Retry", onClick: () => entry.session.retry() } });
      } else if (state.phase === "connected") {
        this.notice.dismiss();
        // A restored tab attaches quietly; only a dropped connection that
        // came back is worth a message.
        if (state.reconnected && previous.phase === "reconnecting") this.ctx.toast.success("Terminal reconnected");
      }
    }
    if (entry.id === this.tabs.activeId) {
      this.el.dataset.phase = state.phase;
      this.renderToolbarState();
      this.renderStatusbar();
      this.updateWindowTitle();
    }
  }

  private renderOverlay(entry: TabEntry): void {
    const state = entry.state;
    const visible = state.phase !== "connected";
    entry.overlay.classList.toggle("hidden", !visible);
    if (!visible) {
      clear(entry.overlay);
      return;
    }
    clear(entry.overlay);
    const title = h("div", { class: "term-pane-state-title" });
    const detail = h("div", { class: "term-pane-state-detail" });
    const actions = h("div", { class: "term-pane-state-actions" });
    switch (state.phase) {
      case "loading":
      case "connecting":
        entry.overlay.append(h("span", { class: "spinner" }), h("div", { class: "term-pane-state-title" }, "Connecting…"));
        return;
      case "reconnecting":
        entry.overlay.append(h("span", { class: "spinner" }), h("div", { class: "term-pane-state-title" }, "Reconnecting…"), h("div", { class: "term-pane-state-detail" }, "The connection dropped; trying once more."));
        return;
      case "disconnected":
        title.textContent = "Disconnected";
        detail.textContent = state.error ?? "The terminal connection was lost.";
        actions.append(
          this.stateButton("Retry", "primary", () => entry.session.retry()),
          this.stateButton("New terminal", "ghost", () => entry.session.newSession()),
        );
        break;
      case "exited":
        title.textContent = "Session ended";
        detail.textContent = state.error ?? terminalExitNotice(state.exitCode);
        actions.append(this.stateButton("New terminal", "primary", () => entry.session.newSession()));
        break;
      case "failed":
        title.textContent = "Terminal error";
        detail.textContent = state.error ?? "The terminal could not start.";
        actions.append(
          this.stateButton("Retry", "primary", () => entry.session.retry()),
          this.stateButton("New terminal", "ghost", () => entry.session.newSession()),
        );
        break;
      default:
        return;
    }
    entry.overlay.append(title, detail, actions);
  }

  private stateButton(label: string, variant: "primary" | "ghost", onClick: () => void): HTMLButtonElement {
    const button = h("button", { class: `btn btn-${variant} btn-sm`, type: "button" }, label);
    button.addEventListener("click", onClick);
    return button;
  }

  // ---------------------------------------------------------------- render

  private renderBodyState(node: Node | null): void {
    this.stateHost.replaceChildren();
    if (node) this.stateHost.appendChild(node);
    this.stateHost.hidden = node === null;
  }

  private renderTabs(): void {
    clear(this.tabbar);
    for (const entry of this.tabs.tabs) {
      const active = entry.id === this.tabs.activeId;
      entry.tabRoot.classList.toggle("is-active", active);
      entry.tabRoot.dataset.phase = entry.state.phase;
      const title = this.tabTitle(entry);
      entry.titleEl.textContent = title;
      entry.tabMain.setAttribute("aria-label", title);
      entry.tabMain.title = title;
      entry.tabMain.setAttribute("aria-selected", String(active));
      this.tabbar.appendChild(entry.tabRoot);
    }
    this.tabbar.appendChild(this.tabNewButton);
  }

  private renderToolbarState(): void {
    const current = activeTab(this.tabs);
    const connected = current?.state.phase === "connected";
    const enabled = Boolean(this.env?.enabled) && !this.disposed;
    this.newTabButton.disabled = !enabled;
    this.tabNewButton.disabled = !enabled;
    this.newWindowButton.disabled = !enabled;
    this.newWindowButton.title = enabled ? "New Terminal window" : "Terminal is not available";
    this.copyButton.disabled = !connected;
    this.pasteButton.disabled = !connected;
    this.clearButton.disabled = !connected;
    this.searchButton.disabled = !connected;
    this.fontDownButton.disabled = this.fontSize <= 12;
    this.fontUpButton.disabled = this.fontSize >= 20;
    this.envButton.disabled = this.env === null;
    this.applyTabbarVisibility();
    if (current) {
      this.el.dataset.phase = current.state.phase;
      this.statusText.textContent = terminalPhaseLabel(current.state);
      this.statusText.title = current.state.error ?? terminalPhaseLabel(current.state);
    } else if (this.env?.enabled === false) {
      this.el.dataset.phase = "failed";
      this.statusText.textContent = "Disabled";
      this.statusText.title = "";
    } else if (this.env === null) {
      this.el.dataset.phase = this.envError ? "failed" : "loading";
      this.statusText.textContent = this.envError ? "Unavailable" : "Loading…";
      this.statusText.title = "";
    } else {
      this.el.dataset.phase = "disconnected";
      this.statusText.textContent = "No session";
      this.statusText.title = "";
    }
  }

  private applyTabbarVisibility(): void {
    this.tabbar.hidden = this.env === null || this.env.enabled === false;
  }

  private renderStatusbar(): void {
    const current = activeTab(this.tabs);
    this.statusShell.textContent = current?.state.shell || this.env?.shell || "—";
    this.statusRoot.hidden = current?.state.root !== true;
    const cwd = current?.state.cwd || (current?.folder ? terminalFolderLabel(current.folder) : "—");
    this.statusCwd.textContent = cwd;
    this.statusCwd.title = cwd;
    this.statusSession.textContent = current?.state.sessionId ? current.state.sessionId.slice(0, 8) : "";
    this.statusRuntimes.textContent = this.runtimeLine(current ?? null);
    this.statusRuntimes.title = this.statusRuntimes.textContent;
  }

  private runtimeLine(entry: TabEntry | null): string {
    if (!this.env) return "";
    const effective = entry ? entry.state.runtimes : {};
    const keys = Object.keys(effective);
    if (keys.length > 0) {
      const names = new Map(this.env.runtimes.map((runtime) => [runtime.id, runtime.name]));
      return keys
        .map((id) => `${names.get(id) ?? id} ${effective[id]}`)
        .join(" · ");
    }
    const next = runtimeSummary(this.env, validateRuntimeOverrides(this.env, this.overrides));
    if (next.length === 0) return "";
    return entry ? `Runtimes: ${next.join(" · ")}` : `Next tabs: ${next.join(" · ")}`;
  }

  private tabTitle(entry: TabEntry): string {
    if (entry.folder) return terminalTabTitle(entry.folder, entry.ordinal);
    if (entry.state.phase === "connected" && entry.state.cwd) {
      const base = entry.state.cwd.split("/").filter(Boolean).pop();
      if (base) return `Terminal — ${base}`;
    }
    return `Terminal ${entry.ordinal}`;
  }

  private updateWindowTitle(): void {
    const current = activeTab(this.tabs);
    this.ctx.setTitle(current ? this.tabTitle(current) : "Terminal");
  }

  private renderFontSize(): void {
    this.fontSizeLabel.textContent = `${this.fontSize} px`;
  }

  // -------------------------------------------------------------- actions

  private openNewWindow(): void {
    this.ctx.openApp("terminal");
  }

  private clearActive(): void {
    const current = activeTab(this.tabs);
    current?.session.clear();
  }

  private async copyActive(): Promise<void> {
    const current = activeTab(this.tabs);
    if (!current) return;
    if (current.state.phase !== "connected") {
      this.ctx.toast.warning("The terminal is not connected.");
      return;
    }
    const copied = await current.session.copySelection();
    if (copied) this.ctx.toast.success("Copied");
    else this.ctx.toast.info("Select text in the terminal first");
  }

  private async pasteActive(): Promise<void> {
    const current = activeTab(this.tabs);
    if (!current) return;
    if (current.state.phase !== "connected") {
      this.ctx.toast.warning("The terminal is not connected.");
      return;
    }
    if (await current.session.pasteFromClipboard()) return;
    // Clipboard read was blocked (insecure context / permissions): let the
    // user paste into a field, then send it as keyboard input.
    const text = await promptDialog({
      title: "Paste into terminal",
      label: "Text to send",
      placeholder: "Press Ctrl+V in this field, then confirm",
      confirmLabel: "Send",
    });
    if (text !== null && !current.session.sendText(text)) this.ctx.toast.warning("The terminal is not connected.");
  }

  private setFontSize(size: number): void {
    this.fontSize = writeFontSize(localStorageOrNull(), size);
    this.renderFontSize();
    for (const entry of this.tabs.tabs) entry.session.setFontSize(this.fontSize);
    this.renderToolbarState();
  }

  private openRuntimePanel(): void {
    openRuntimePanel({
      ctx: this.ctx,
      env: this.env,
      error: this.envError,
      overrides: this.overrides,
      onOverridesChange: (next) => {
        this.overrides = next;
        this.renderStatusbar();
      },
      onEnvUpdated: (next) => {
        this.env = next;
        this.envError = null;
        this.renderToolbarState();
        this.renderStatusbar();
      },
    });
  }

  // --------------------------------------------------------------- search

  private buildSearchOverlay(): HTMLElement {
    const input = h("input", {
      class: "input term-search-input",
      type: "search",
      placeholder: "Find in terminal…",
      ariaLabel: "Search terminal output",
    });
    const prev = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Previous match (Shift+Enter)", ariaLabel: "Previous match" }, iconSvg("arrowUp", 14));
    const next = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Next match (Enter)", ariaLabel: "Next match" }, iconSvg("arrowDown", 14));
    const caseButton = h("button", { class: "btn btn-ghost btn-sm term-search-case", type: "button", title: "Match case", ariaLabel: "Match case", ariaPressed: false }, "Aa");
    const count = h("span", { class: "term-search-count" });
    const close = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Close search (Esc)", ariaLabel: "Close search" }, iconSvg("close", 14));
    const overlay = h(
      "div",
      { class: "term-search hidden", role: "search", ariaLabel: "Search terminal output" },
      input,
      count,
      prev,
      next,
      caseButton,
      close,
    );

    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        this.runSearch(!event.shiftKey);
      } else if (event.key === "Escape") {
        event.preventDefault();
        this.closeSearch();
      }
    });
    input.addEventListener("input", () => {
      if (input.value) this.runSearch(true);
      else this.clearSearchHighlights();
    });
    prev.addEventListener("click", () => this.runSearch(false));
    next.addEventListener("click", () => this.runSearch(true));
    caseButton.addEventListener("click", () => {
      this.searchCaseSensitive = !this.searchCaseSensitive;
      caseButton.classList.toggle("is-active", this.searchCaseSensitive);
      caseButton.setAttribute("aria-pressed", String(this.searchCaseSensitive));
      this.runSearch(true);
    });
    close.addEventListener("click", () => this.closeSearch());
    overlay.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        this.closeSearch();
      }
    });
    return overlay;
  }

  private openSearch(): void {
    if (!activeTab(this.tabs)) return;
    this.searchOpen = true;
    this.searchOverlay.classList.remove("hidden");
    this.searchInput.focus();
    this.searchInput.select();
    if (this.searchInput.value) this.runSearch(true);
  }

  private closeSearch(): void {
    this.searchOpen = false;
    this.searchOverlay.classList.add("hidden");
    this.searchCount.textContent = "";
    this.clearSearchHighlights();
    const current = activeTab(this.tabs);
    if (current?.state.phase === "connected") current.session.focus();
  }

  private runSearch(forward: boolean): void {
    const current = activeTab(this.tabs);
    const query = this.searchInput.value;
    if (!current || current.state.phase !== "connected" || !query) {
      this.clearSearchHighlights();
      return;
    }
    const found = forward
      ? current.session.searchNext(query, this.searchCaseSensitive)
      : current.session.searchPrevious(query, this.searchCaseSensitive);
    if (!found) this.searchCount.textContent = "No match";
  }

  private clearSearchHighlights(): void {
    const current = activeTab(this.tabs);
    current?.session.clearSearch();
  }

  private onSearchResults(resultIndex: number, resultCount: number): void {
    if (!this.searchOpen) return;
    if (resultCount <= 0) {
      this.searchCount.textContent = "No match";
      return;
    }
    if (resultIndex < 0) {
      this.searchCount.textContent = `${resultCount} matches`;
      return;
    }
    this.searchCount.textContent = `${resultIndex + 1} of ${resultCount}`;
  }

  // ------------------------------------------------------------- keyboard

  private onKeyDown(event: KeyboardEvent): void {
    const shortcut = terminalShortcut(event);
    if (!shortcut) return;
    event.preventDefault();
    event.stopPropagation();
    switch (shortcut) {
      case "copy":
        void this.copyActive();
        break;
      case "paste":
        void this.pasteActive();
        break;
      case "search":
        if (this.searchOpen) this.closeSearch();
        else this.openSearch();
        break;
      case "new-tab":
        this.addTab(null);
        break;
    }
  }

  // ---------------------------------------------------------------- helpers

  private iconButton(icon: string, title: string, onClick: () => void, label?: string): HTMLButtonElement {
    const button = h("button", { class: "btn btn-ghost btn-sm term-tool", type: "button", title, ariaLabel: title });
    button.appendChild(iconSvg(icon, 14));
    if (label) button.appendChild(h("span", { class: "term-btn-label" }, label));
    button.addEventListener("click", onClick);
    return button;
  }
}


export const terminalApp: AppDefinition = {
  id: "terminal",
  name: "Terminal",
  icon: "terminal",
  description: "Command line access to this RAST server",
  // Administrators always may; regular accounts need an explicit grant in
  // Groups (the server default is admin-only, matching this fallback).
  adminOnly: true,
  defaultSize: { w: 980, h: 620 },
  minSize: { w: 420, h: 280 },
  async mount(ctx: AppContext) {
    const app = new TerminalApp(ctx);
    await app.start();
  },
};
