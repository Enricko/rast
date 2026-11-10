// Shared realtime UI widgets: the "Live" connection badge and a dismissible
// notice bar used for background-change conflicts ("folder changed while you
// were renaming…"). Both are driven by core/realtime and never remount an app.

import { h } from "../core/dom";
import type { RealtimeStore } from "../core/realtime";
import type { ConnectionState } from "../core/sse";
import type { AppLifecycle } from "./lifecycle";

export interface LiveIndicator {
  el: HTMLElement;
}

const STATE_LABELS: Record<ConnectionState, string> = {
  connected: "Live",
  connecting: "Connecting…",
  reconnecting: "Reconnecting…",
  offline: "Offline",
};

/** Small connection badge; automatically follows the shared stream state. */
export function createLiveIndicator(realtime: RealtimeStore, lifecycle: AppLifecycle): LiveIndicator {
  const dot = h("span", { class: "live-dot" });
  const label = h("span", { class: "live-label" });
  const el = h(
    "span",
    { class: "live-badge", role: "status", ariaLive: "polite", title: "Realtime update status" },
    dot,
    label,
  );
  const render = (state: ConnectionState): void => {
    el.dataset.state = state;
    label.textContent = STATE_LABELS[state] ?? state;
    el.title =
      state === "connected"
        ? "Realtime updates are connected"
        : `${STATE_LABELS[state] ?? state} — changes will refresh when the connection is back`;
  };
  lifecycle.add(realtime.onStatus(render));
  render(realtime.status);
  return { el };
}

export interface NoticeOptions {
  kind?: "info" | "warning" | "error" | "success";
  timeoutMs?: number;
  action?: { label: string; onClick: () => void };
}

/**
 * One-line, dismissible notice bar for background changes that collide with an
 * open form. Lives above the app content and never replaces it.
 */
export class NoticeBar {
  readonly el: HTMLElement;
  private readonly textEl: HTMLElement;
  private readonly actionHost: HTMLElement;
  private timer = 0;

  constructor(private readonly lifecycle: AppLifecycle) {
    this.textEl = h("span", { class: "realtime-notice-text" });
    this.actionHost = h("span", { class: "realtime-notice-action" });
    const close = h("button", { class: "realtime-notice-close", type: "button", ariaLabel: "Dismiss notice" }, "×");
    close.addEventListener("click", () => this.dismiss());
    this.el = h(
      "div",
      { class: "realtime-notice hidden", role: "status", ariaLive: "polite" },
      h("span", { class: "realtime-notice-dot" }),
      this.textEl,
      this.actionHost,
      close,
    );
    lifecycle.add(() => this.cancelTimer());
  }

  show(message: string, options: NoticeOptions = {}): void {
    const kind = options.kind ?? "info";
    this.el.dataset.kind = kind;
    this.textEl.textContent = message;
    this.actionHost.replaceChildren();
    if (options.action) {
      const btn = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, options.action.label);
      btn.addEventListener("click", () => {
        options.action?.onClick();
        this.dismiss();
      });
      this.actionHost.appendChild(btn);
    }
    this.el.classList.remove("hidden");
    this.cancelTimer();
    const timeout = options.timeoutMs ?? (kind === "error" || kind === "warning" ? 12000 : 7000);
    if (timeout > 0) {
      this.timer = this.lifecycle.setTimeout(() => {
        this.timer = 0;
        this.el.classList.add("hidden");
      }, timeout);
    }
  }

  dismiss(): void {
    this.cancelTimer();
    this.el.classList.add("hidden");
  }

  private cancelTimer(): void {
    if (this.timer) {
      this.lifecycle.clearTimeout(this.timer);
      this.timer = 0;
    }
  }
}

/** Title-case a logical path for notices without leaking markup. */
export function quotedPath(path: string): string {
  return `“${path}”`;
}
