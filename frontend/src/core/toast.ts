// In-desktop notifications.

import { h } from "./dom";

export type ToastKind = "info" | "success" | "error" | "warning";

export interface ToastOptions {
  kind?: ToastKind;
  timeoutMs?: number;
  action?: { label: string; onClick: () => void };
}

export class ToastCenter {
  private readonly container: HTMLElement;
  private readonly maxVisible = 4;

  constructor(parent: HTMLElement) {
    this.container = h("div", { class: "toast-center", role: "region", ariaLabel: "Notifications" });
    parent.appendChild(this.container);
  }

  show(message: string, options: ToastOptions = {}): () => void {
    const kind = options.kind ?? "info";
    const toast = h(
      "div",
      { class: `toast toast-${kind}`, role: "status" },
      h("div", { class: "toast-dot" }),
      h("div", { class: "toast-message" }, message),
    );
    if (options.action) {
      const btn = h("button", { class: "toast-action", type: "button" }, options.action.label);
      btn.addEventListener("click", () => {
        options.action?.onClick();
        dismiss();
      });
      toast.appendChild(btn);
    }
    const close = h("button", { class: "toast-close", type: "button", ariaLabel: "Dismiss" }, "×");
    close.addEventListener("click", () => dismiss());
    toast.appendChild(close);

    let removed = false;
    const dismiss = () => {
      if (removed) return;
      removed = true;
      toast.classList.add("toast-leaving");
      window.setTimeout(() => toast.remove(), 180);
    };

    this.container.appendChild(toast);
    while (this.container.children.length > this.maxVisible) {
      (this.container.firstElementChild as HTMLElement | null)?.remove();
    }
    const timeout = options.timeoutMs ?? (kind === "error" ? 8000 : 4000);
    if (timeout > 0) window.setTimeout(dismiss, timeout);
    return dismiss;
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
    while (this.container.firstChild) this.container.removeChild(this.container.firstChild);
  }
}
