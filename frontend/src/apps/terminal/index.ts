// Terminal: one window with a tab per shell. Administrators get root shells
// on the server (src/terminal; the server owner turns them on once with
// `sudo rast terminal enable`). Accounts an administrator grants the app get
// shells in their own sandbox container instead (src/sandbox): only their
// home folder, no network. Shells keep running when the window closes or the
// page reloads; opening Terminal again shows them.

import type { AppContext, AppDefinition, AppInstance } from "../../core/app";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { askAi, canAskAi, fenced, tailOf } from "../../core/ask-ai";
import { confirmDialog, copyText, errorMessage } from "../../core/ui";
import { isBusy, mountShell, parseShellList, sandboxSummary, tabLabel, type ShellInfo, type ShellView } from "./shell";

interface Overview {
  available: boolean;
  mode?: "root" | "sandbox";
  reason?: "off" | "docker" | "error";
  message?: string;
  enableCommand?: string;
  shellUser?: string;
  /** Sandboxes: is the image built yet (the first time takes minutes). */
  image?: "ready" | "building" | "missing" | "failed";
  imageError?: string | null;
  limits?: { memoryMb?: number; cpus?: string; network?: boolean };
  max?: number;
  sessions?: unknown;
}

const POLL_MS = 10_000;
/** How often the setup screen asks whether the image is ready. */
const SETUP_POLL_MS = 4_000;



export const terminalApp: AppDefinition = {
  id: "terminal",
  name: "Terminal",
  icon: "terminal",
  description: "Shells on this server: root for administrators, a private sandbox for others",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 920, h: 580 },
  minSize: { w: 420, h: 300 },
  mount(ctx) {
    return mountTerminalApp(ctx);
  },
};

interface Tab {
  view: ShellView | null;
  button: HTMLButtonElement;
  label: HTMLElement;
}

function mountTerminalApp(ctx: AppContext): AppInstance {
  ctx.root.classList.add("app-terminal");
  // The shell keeps what was typed and shown: the desktop session does not.
  ctx.root.dataset.sync = "off";
  let disposed = false;
  let max = 8;
  let pollTimer = 0;
  let setupTimer = 0;
  const tabs: Tab[] = [];
  let active: Tab | null = null;
  /** Foreground program per shell, from the last poll. */
  const programs = new Map<string, string>();

  const tabsEl = h("div", { class: "term-tabs", role: "tablist", ariaLabel: "Shells" });
  const newButton = h("button", { class: "btn btn-ghost btn-icon btn-sm term-new", type: "button", title: "New shell", ariaLabel: "New shell" }, iconSvg("plus", 15));
  const userBadge = h("span", { class: "term-user hidden", title: "Shells run as this account on the server" });
  const copyButton = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Copy selection (Ctrl+Shift+C)", ariaLabel: "Copy selection" }, iconSvg("copy", 14));
  const askButton = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Ask AI about the selection (or the latest output)", ariaLabel: "Ask AI" }, iconSvg("sparkle", 14));
  if (!canAskAi(ctx)) askButton.classList.add("hidden");
  const bar = h("div", { class: "term-bar hidden" }, tabsEl, newButton, h("div", { class: "app-toolbar-spacer" }), userBadge, askButton, copyButton);
  const stage = h("div", { class: "term-stage hidden" });
  const empty = h(
    "div",
    { class: "term-empty hidden" },
    h("p", { class: "term-empty-title" }, "No shells open"),
    h("button", { class: "btn btn-primary btn-sm", type: "button", onClick: () => newShell() }, iconSvg("plus", 14), h("span", {}, "New shell")),
  );
  const panel = h("div", { class: "term-panel hidden" });
  ctx.root.append(bar, stage, panel);
  stage.append(empty);

  newButton.addEventListener("click", () => newShell());
  copyButton.addEventListener("click", () => void active?.view?.copySelection());
  askButton.addEventListener("click", () => {
    const text = active?.view?.textForAi() ?? "";
    askAi(ctx, text ? `About this from my terminal:\n${fenced(tailOf(text, 12_000))}\n\n` : "About my terminal: ");
  });

  function updateTab(tab: Tab): void {
    const view = tab.view;
    const info = view?.info;
    const program = info ? (programs.get(info.id) ?? info.program) : "";
    tab.label.textContent = info ? tabLabel({ number: info.number, program }) : "…";
    tab.button.dataset.phase = view?.phase ?? "loading";
    tab.button.title = info ? `Shell ${info.number}${isBusy(program) ? ` — ${program} running` : ""}` : "New shell";
    newButton.disabled = tabs.length >= max;
  }

  function select(tab: Tab | null): void {
    active = tab;
    for (const t of tabs) {
      const on = t === tab;
      t.view?.setVisible(on);
      t.button.classList.toggle("is-active", on);
      t.button.setAttribute("aria-selected", String(on));
      t.button.tabIndex = on ? 0 : -1;
    }
    empty.classList.toggle("hidden", tabs.length > 0);
    tab?.view?.focus();
  }

  function addTab(session?: ShellInfo): Tab {
    const label = h("span", { class: "term-tab-label" }, "…");
    const close = h("span", { class: "term-tab-close", title: "Close shell", ariaHidden: true }, iconSvg("close", 11));
    const button = h("button", { class: "term-tab", type: "button", role: "tab" }, h("span", { class: "term-tab-dot", ariaHidden: true }), label, close) as HTMLButtonElement;
    const tab: Tab = { view: null, button, label };
    button.addEventListener("click", (event) => {
      if (close.contains(event.target as Node)) {
        void closeTab(tab);
        return;
      }
      // Choosing a shell that moved to another window brings it back here.
      if (tab.view?.phase === "detached") tab.view.reattach();
      select(tab);
    });
    button.addEventListener("auxclick", (event) => {
      if (event.button === 1) void closeTab(tab);
    });
    button.addEventListener("keydown", (event) => {
      if (event.key === "Delete") void closeTab(tab);
      if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
        const next = tabs[(tabs.indexOf(tab) + (event.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
        if (next) {
          select(next);
          next.button.focus();
        }
      }
    });
    tabs.push(tab);
    tabsEl.append(button);
    tab.view = mountShell({ ctx, ...(session ? { session } : {}), onChange: () => updateTab(tab) });
    stage.append(tab.view.el);
    updateTab(tab);
    return tab;
  }

  function newShell(): void {
    if (tabs.length >= max) {
      ctx.toast.info(`At most ${max} shells can be open at once`);
      return;
    }
    select(addTab());
  }

  async function closeTab(tab: Tab): Promise<void> {
    const view = tab.view;
    const info = view?.info;
    if (view && info && view.phase !== "ended") {
      const program = programs.get(info.id) ?? info.program;
      if (isBusy(program)) {
        const ok = await confirmDialog({
          title: "Close this shell?",
          message: `${program} is still running in shell ${info.number}. Closing the shell stops it.`,
          confirmLabel: "Close shell",
          danger: true,
          session: ctx.dialog("shell-close", { session: info.id }),
        });
        if (!ok) return;
      }
      try {
        await ctx.api.del(`/api/terminal/sessions/${info.id}`);
      } catch (error) {
        // Already gone is fine; anything else is worth saying.
        if (!/no longer open|not found/i.test(errorMessage(error))) ctx.toast.error(`Could not close the shell: ${errorMessage(error)}`);
      }
    }
    view?.detach();
    tab.button.remove();
    const index = tabs.indexOf(tab);
    if (index >= 0) tabs.splice(index, 1);
    if (active === tab) select(tabs[Math.min(index, tabs.length - 1)] ?? null);
    else select(active);
    newButton.disabled = tabs.length >= max;
  }

  function showPanel(overview: Overview | null, error?: string): void {
    bar.classList.add("hidden");
    stage.classList.add("hidden");
    panel.classList.remove("hidden");
    panel.replaceChildren();
    const command = overview?.enableCommand ?? "sudo rast terminal enable";
    const again = h("button", { class: "btn btn-ghost btn-sm", type: "button", onClick: () => void load() }, iconSvg("refresh", 13), h("span", {}, "Check again"));
    if (overview?.reason === "docker") {
      panel.append(
        h("span", { class: "term-panel-icon" }, iconSvg("terminal", 30)),
        h("p", { class: "term-panel-title" }, "Terminal is not available"),
        h("p", { class: "muted" }, "Terminal needs Docker on this server, and it is not running. Ask an administrator."),
        again,
      );
      return;
    }
    if (error || overview?.reason === "error") {
      panel.append(
        h("span", { class: "term-panel-icon" }, iconSvg("terminal", 30)),
        h("p", { class: "term-panel-title" }, "Terminal is not answering"),
        h("p", { class: "muted" }, error ?? overview?.message ?? "The terminal helper did not answer."),
        again,
      );
      return;
    }
    const copy = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Copy", ariaLabel: "Copy command" }, iconSvg("copy", 14));
    copy.addEventListener("click", () => {
      void copyText(command).then((ok) => (ok ? ctx.toast.success("Copied") : ctx.toast.error("Could not copy")));
    });
    panel.append(
      h("span", { class: "term-panel-icon" }, iconSvg("terminal", 30)),
      h("p", { class: "term-panel-title" }, "Terminal is off"),
      h("p", { class: "muted" }, "Root shells in the browser are turned on from the server itself, once. Run this on the server:"),
      h("div", { class: "term-command" }, h("code", {}, command), copy),
      again,
      h("p", { class: "small faint" }, "Turn it off again with sudo rast terminal disable."),
    );
  }

  async function poll(): Promise<void> {
    window.clearTimeout(pollTimer);
    if (disposed) return;
    try {
      const overview = await ctx.api.get<Overview>("/api/terminal");
      if (disposed || !overview.available) return;
      programs.clear();
      for (const shell of parseShellList(overview.sessions)) programs.set(shell.id, shell.program);
      for (const tab of tabs) updateTab(tab);
    } catch {
      /* the next poll tries again */
    } finally {
      if (!disposed) pollTimer = window.setTimeout(() => void poll(), POLL_MS);
    }
  }

  /** Sandboxes, the first time: the image is built on the server. */
  function showSetup(overview: Overview): void {
    bar.classList.add("hidden");
    stage.classList.add("hidden");
    panel.classList.remove("hidden");
    panel.replaceChildren();
    if (overview.image === "failed") {
      const retry = h("button", { class: "btn btn-primary btn-sm", type: "button", onClick: () => void startSetup() }, iconSvg("refresh", 13), h("span", {}, "Try again"));
      panel.append(
        h("span", { class: "term-panel-icon" }, iconSvg("terminal", 30)),
        h("p", { class: "term-panel-title" }, "Terminal could not be set up"),
        h("p", { class: "muted" }, overview.imageError || "Preparing the sandbox failed."),
        retry,
      );
      return;
    }
    panel.append(
      h("span", { class: "term-panel-icon term-setup-spin" }, iconSvg("refresh", 26)),
      h("p", { class: "term-panel-title" }, "Setting up Terminal"),
      h("p", { class: "muted" }, "The server is preparing the sandbox your shells run in. This happens once and can take a few minutes; you can close this window and come back."),
    );
    window.clearTimeout(setupTimer);
    setupTimer = window.setTimeout(() => void load(), SETUP_POLL_MS);
  }

  async function startSetup(): Promise<void> {
    try {
      const state = await ctx.api.post<Pick<Overview, "image" | "imageError">>("/api/terminal/setup");
      if (disposed) return;
      if (state.image === "ready") void load();
      else showSetup({ available: true, mode: "sandbox", ...state });
    } catch (error) {
      if (!disposed) showPanel(null, errorMessage(error));
    }
  }

  async function load(): Promise<void> {
    window.clearTimeout(setupTimer);
    let overview: Overview;
    try {
      overview = await ctx.api.get<Overview>("/api/terminal");
    } catch (error) {
      if (!disposed) showPanel(null, errorMessage(error));
      return;
    }
    if (disposed) return;
    if (!overview.available) {
      showPanel(overview);
      return;
    }
    if (overview.mode === "sandbox" && overview.image !== "ready") {
      if (overview.image === "missing") void startSetup();
      else showSetup(overview);
      return;
    }
    max = overview.max ?? max;
    panel.classList.add("hidden");
    bar.classList.remove("hidden");
    stage.classList.remove("hidden");
    if (overview.shellUser) {
      const sandboxed = overview.mode === "sandbox";
      userBadge.textContent = sandboxed ? `${overview.shellUser} · sandbox` : overview.shellUser;
      userBadge.title = sandboxed ? sandboxSummary(overview.limits) : "Shells run as this account on the server";
      userBadge.classList.toggle("is-root", !sandboxed && overview.shellUser === "root");
      userBadge.classList.remove("hidden");
    }
    if (tabs.length === 0) {
      const sessions = parseShellList(overview.sessions);
      for (const session of sessions) programs.set(session.id, session.program);
      if (sessions.length) {
        const added = sessions.map((session) => addTab(session));
        select(added[0] ?? null);
      } else {
        newShell();
      }
    }
    pollTimer = window.setTimeout(() => void poll(), POLL_MS);
  }

  ctx.setTitle("Terminal");
  void load();
  ctx.onReopen(() => active?.view?.focus());
  // "Close this shell?" opens again only while that shell still runs
  // something (otherwise opening it again would close the shell unasked).
  ctx.onDialog("shell-close", (params) => {
    const id = ((params ?? {}) as { session?: unknown }).session;
    if (typeof id !== "string") return;
    let tries = 0;
    const look = (): void => {
      if (disposed) return;
      const tab = tabs.find((candidate) => candidate.view?.info?.id === id);
      const info = tab?.view?.info;
      if (tab && info && tab.view?.phase !== "ended") {
        if (isBusy(programs.get(info.id) ?? info.program)) void closeTab(tab);
        return;
      }
      tries += 1;
      if (tries < 20) window.setTimeout(look, 250); // the shells are still connecting
    };
    look();
  });

  return {
    destroy() {
      disposed = true;
      window.clearTimeout(pollTimer);
      window.clearTimeout(setupTimer);
      // Closing the window only detaches: the shells keep running.
      for (const tab of tabs) tab.view?.detach();
      tabs.length = 0;
    },
  };
}
