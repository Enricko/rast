// A Linux app window: starts the person's own copy of the app on the server
// and shows it in a sandboxed frame. Closing the window stops the copy.

import type { AppContext, AppDefinition } from "../../core/app";
import { h } from "../../core/dom";
import { appIconSvg } from "../../core/icons";
import { getRealtime } from "../../core/realtime";
import { errorMessage } from "../../core/ui";
import { AppLifecycle } from "../../components/lifecycle";
import {
  isOpenable,
  LINUX_PREFIX,
  linuxAppById,
  linuxApps,
  linuxOverview,
  type LinuxSession,
} from "./catalog";

const POLL_MS = 2_000;
const FRAME_SANDBOX = "allow-scripts allow-forms allow-pointer-lock allow-downloads allow-modals";
const FRAME_ALLOW = "clipboard-read; clipboard-write; autoplay; fullscreen";

const definitions = new Map<string, { name: string; def: AppDefinition }>();

/** One desktop app per installed Linux app that has a finished build. */
export function linuxAppDefinitions(): AppDefinition[] {
  const result: AppDefinition[] = [];
  const seen = new Set<string>();
  for (const app of linuxApps()) {
    if (!isOpenable(app)) continue;
    const id = `${LINUX_PREFIX}${app.id}`;
    seen.add(id);
    let entry = definitions.get(id);
    if (!entry || entry.name !== app.name) {
      const appId = app.id;
      entry = {
        name: app.name,
        def: {
          id,
          name: app.name,
          icon: "box",
          description: app.description || "Linux app",
          adminOnly: true,
          singleton: true,
          defaultSize: { w: 1100, h: 720 },
          minSize: { w: 480, h: 360 },
          mount: (ctx) => new LinuxWindow(ctx, appId).start(),
        },
      };
      definitions.set(id, entry);
    }
    result.push(entry.def);
  }
  for (const id of [...definitions.keys()]) if (!seen.has(id)) definitions.delete(id);
  return result;
}

class LinuxWindow {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly appId: string;
  private readonly name: string;
  private readonly stage: HTMLElement;
  private session: LinuxSession | null = null;
  private frame: HTMLIFrameElement | null = null;
  private pollTimer = 0;
  private opening = false;

  constructor(ctx: AppContext, appId: string) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.appId = appId;
    this.name = linuxAppById(appId)?.name ?? "Linux app";
    this.stage = h("div", { class: "linux-stage" });
    ctx.root.classList.add("linux-window");
    ctx.root.appendChild(this.stage);

    const realtime = getRealtime(ctx.events);
    this.lc.add(
      realtime.on<LinuxSession>("linuxapps.session", (data) => {
        if (data && this.session && data.id === this.session.id) this.apply(data);
      }),
    );
    this.lc.add(realtime.onResync(() => void this.refresh()));
    this.lc.add(() => this.stop());
  }

  start(): void {
    this.ctx.setTitle(this.name);
    void this.open();
  }

  private async open(): Promise<void> {
    if (this.opening || this.lc.isDisposed) return;
    this.opening = true;
    this.session = null;
    this.removeFrame();
    this.showStarting();
    try {
      const session = await this.ctx.api.post<LinuxSession>(`/api/linux-apps/${encodeURIComponent(this.appId)}/open`);
      if (this.lc.isDisposed) {
        void this.closeSession(session);
        return;
      }
      this.apply(session);
    } catch (error) {
      if (!this.lc.isDisposed) this.showMessage(`${this.name} could not start`, errorMessage(error), true);
    } finally {
      this.opening = false;
    }
  }

  private apply(session: LinuxSession): void {
    if (this.lc.isDisposed) return;
    this.session = session;
    if (session.status === "ready" && session.url) {
      this.stopPolling();
      this.showFrame(session.url);
    } else if (session.status === "starting") {
      this.showStarting();
      this.startPolling();
    } else {
      this.stopPolling();
      this.removeFrame();
      this.showEnded(session);
    }
  }

  /** Server events can be missed; ask while the copy is starting. */
  private startPolling(): void {
    if (this.pollTimer) return;
    this.pollTimer = this.lc.setInterval(() => void this.refresh(), POLL_MS);
  }

  private stopPolling(): void {
    if (!this.pollTimer) return;
    this.lc.clearInterval(this.pollTimer);
    this.pollTimer = 0;
  }

  private async refresh(): Promise<void> {
    const session = this.session;
    if (!session || session.status === "ended" || this.lc.isDisposed) return;
    try {
      const fresh = await this.ctx.api.get<LinuxSession>(`/api/linux-apps/sessions/${encodeURIComponent(session.id)}`);
      if (this.session?.id === fresh.id) this.apply(fresh);
    } catch (error) {
      if ((error as { status?: number }).status === 404 && this.session?.id === session.id) {
        this.apply({ ...session, status: "ended", url: null, reason: "closed" });
      }
    }
  }

  private showStarting(): void {
    if (this.frame) return;
    this.stage.replaceChildren(
      h(
        "div",
        { class: "linux-state", role: "status", ariaLive: "polite" },
        this.icon(),
        h("div", { class: "linux-state-title" }, `Starting ${this.name}…`),
        h("span", { class: "spinner" }),
      ),
    );
  }

  private showFrame(url: string): void {
    if (this.frame && this.frame.dataset.url === url) return;
    this.removeFrame();
    const frame = document.createElement("iframe");
    frame.className = "linux-frame";
    frame.title = this.name;
    frame.setAttribute("sandbox", FRAME_SANDBOX);
    frame.setAttribute("allow", FRAME_ALLOW);
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.dataset.url = url;
    frame.src = url;
    this.frame = frame;
    this.stage.replaceChildren(frame);
    frame.addEventListener("load", () => frame.focus(), { once: true });
  }

  private removeFrame(): void {
    if (!this.frame) return;
    this.frame.src = "about:blank";
    this.frame.remove();
    this.frame = null;
  }

  private showEnded(session: LinuxSession): void {
    const idleMinutes = linuxOverview()?.limits.idleMinutes ?? 30;
    switch (session.reason) {
      case "removed":
        this.showMessage(`${this.name} was uninstalled`, "", false);
        return;
      case "idle":
        this.showMessage(`${this.name} was stopped`, `It was not used for ${idleMinutes} minutes.`, true);
        return;
      case "error":
        this.showMessage(`${this.name} could not start`, session.error ?? "", true);
        return;
      default:
        this.showMessage(`${this.name} was closed`, "", true);
    }
  }

  private showMessage(title: string, detail: string, canReopen: boolean): void {
    let action: HTMLButtonElement | null = null;
    if (canReopen && linuxAppById(this.appId)) {
      action = h("button", { class: "btn btn-primary btn-sm", type: "button" }, "Open again");
      action.addEventListener("click", () => void this.open());
    }
    this.stage.replaceChildren(
      h(
        "div",
        { class: "linux-state", role: "status", ariaLive: "polite" },
        this.icon(),
        h("div", { class: "linux-state-title" }, title),
        detail ? h("div", { class: "linux-state-detail" }, detail) : null,
        action,
      ),
    );
    action?.focus();
  }

  private icon(): HTMLElement {
    return h("div", { class: "linux-state-icon", ariaHidden: true }, appIconSvg(`${LINUX_PREFIX}${this.appId}`, "box", 56));
  }

  /** The window closed: stop this copy now instead of waiting for the server to notice. */
  private stop(): void {
    this.stopPolling();
    this.removeFrame();
    const session = this.session;
    this.session = null;
    if (session && session.status !== "ended") void this.closeSession(session);
  }

  private async closeSession(session: LinuxSession): Promise<void> {
    try {
      await this.ctx.api.post(`/api/linux-apps/sessions/${encodeURIComponent(session.id)}/close`);
    } catch {
      /* the server stops it on its own once nobody is watching */
    }
  }
}
