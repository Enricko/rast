// In-desktop notifications: stacked, prioritised, deduplicated toasts.
//
// Errors and warnings are placed at the top of the stack so they are never
// pushed out of view by a burst of informational messages. Identical messages
// received in quick succession collapse into one toast with a counter.

import { h } from "./dom";

export type ToastKind = "info" | "success" | "error" | "warning";

export interface ToastOptions {
  kind?: ToastKind;
  /** 0 keeps the toast until dismissed. */
  timeoutMs?: number;
  action?: { label: string; onClick: () => void };
}

interface ToastRecord {
  el: HTMLElement;
  kind: ToastKind;
  message: string;
  count: number;
  countEl: HTMLElement;
  timer: number | undefined;
  timeoutMs: number;
  startedAt: number;
  remaining: number;
  dismiss(): void;
}

const PRIORITY: Record<ToastKind, number> = { error: 0, warning: 1, success: 2, info: 3 };
const HOVER_PAUSE = true;

export class ToastCenter {
  private readonly container: HTMLElement;
  private readonly records: ToastRecord[] = [];
  private readonly maxVisible: number;

  constructor(parent: HTMLElement, options: { maxVisible?: number } = {}) {
    this.maxVisible = Math.max(1, options.maxVisible ?? 4);
    this.container = h("div", {
      class: "toast-center",
      role: "region",
      ariaLabel: "Notifications",
    });
    this.container.setAttribute("aria-live", "polite");
    parent.appendChild(this.container);
  }

  show(message: string, options: ToastOptions = {}): () => void {
    const kind = options.kind ?? "info";
    const existing = this.records.find(
      (record) => record.message === message && record.kind === kind && !record.el.classList.contains("toast-leaving"),
    );
    if (existing) {
      existing.count += 1;
      existing.countEl.textContent = `${existing.count}`;
      existing.countEl.hidden = false;
      this.restartTimer(existing);
      return () => existing.dismiss();
    }

    const dot = h("div", { class: "toast-dot" });
    const messageEl = h("div", { class: "toast-message" }, message);
    const countEl = h("span", { class: "toast-count", hidden: true });
    const toast = h(
      "div",
      {
        class: `toast toast-${kind}`,
        role: kind === "error" || kind === "warning" ? "alert" : "status",
      },
      dot,
      h("div", { class: "toast-content" }, messageEl, countEl),
    );

    let removed = false;
    const record: ToastRecord = {
      el: toast,
      kind,
      message,
      count: 1,
      countEl,
      timer: undefined,
      timeoutMs: options.timeoutMs ?? (kind === "error" ? 9000 : kind === "warning" ? 6500 : 4500),
      startedAt: 0,
      remaining: 0,
      dismiss: () => dismiss(),
    };

    if (options.action) {
      const btn = h("button", { class: "toast-action", type: "button" }, options.action.label);
      btn.addEventListener("click", () => {
        options.action?.onClick();
        dismiss();
      });
      toast.appendChild(btn);
    }
    const close = h(
      "button",
      { class: "toast-close", type: "button", ariaLabel: "Dismiss notification" },
      "×",
    );
    close.addEventListener("click", () => dismiss());
    toast.appendChild(close);

    const dismiss = () => {
      if (removed) return;
      removed = true;
      if (record.timer !== undefined) window.clearTimeout(record.timer);
      toast.classList.add("toast-leaving");
      window.setTimeout(() => {
        toast.remove();
        const index = this.records.indexOf(record);
        if (index >= 0) this.records.splice(index, 1);
      }, 180);
    };

    // Insert by priority: high-priority toasts at the top of the stack.
    const index = this.records.findIndex((other) => PRIORITY[other.kind] > PRIORITY[kind]);
    if (index < 0) {
      this.records.push(record);
      this.container.appendChild(toast);
    } else {
      this.records.splice(index, 0, record);
      this.container.insertBefore(toast, this.container.children[index] ?? null);
    }

    this.trim();
    this.restartTimer(record);
    if (HOVER_PAUSE) {
      toast.addEventListener("pointerenter", () => this.pause(record));
      toast.addEventListener("pointerleave", () => this.restartTimer(record));
      toast.addEventListener("focusin", () => this.pause(record));
      toast.addEventListener("focusout", () => this.restartTimer(record));
    }
    return () => dismiss();
  }

  info(message: string, options?: ToastOptions): void {
    this.show(message, { ...options, kind: "info" });
  }

  success(message: string, options?: ToastOptions): void {
    this.show(message, { ...options, kind: "success" });
  }

  error(message: string, options?: ToastOptions): void {
    this.show(message, { ...options, kind: "error" });
  }

  warning(message: string, options?: ToastOptions): void {
    this.show(message, { ...options, kind: "warning" });
  }

  clear(): void {
    for (const record of [...this.records]) record.dismiss();
  }

  private trim(): void {
    while (this.records.length > this.maxVisible) {
      // Prefer dropping the oldest low-priority toast; never drop errors first.
      let victim = this.records.find((record) => record.kind === "info" || record.kind === "success");
      victim ??= this.records.find((record) => record.kind === "warning");
      victim ??= this.records[0];
      if (!victim) break;
      victim.dismiss();
      const index = this.records.indexOf(victim);
      if (index < 0) break;
      this.records.splice(index, 1);
    }
  }

  private pause(record: ToastRecord): void {
    if (record.timer === undefined) return;
    window.clearTimeout(record.timer);
    record.timer = undefined;
    const elapsed = Date.now() - record.startedAt;
    record.remaining = Math.max(0, record.remaining - elapsed);
  }

  private restartTimer(record: ToastRecord): void {
    if (record.timer !== undefined) window.clearTimeout(record.timer);
    record.timer = undefined;
    if (record.timeoutMs <= 0) return;
    if (record.remaining <= 0) record.remaining = record.timeoutMs;
    record.startedAt = Date.now();
    record.timer = window.setTimeout(() => record.dismiss(), record.remaining);
  }
}
