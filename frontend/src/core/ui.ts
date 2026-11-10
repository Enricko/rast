// Modal dialogs, confirmations and small UI utilities.

import { h } from "./dom";

export interface ModalOptions {
  title: string;
  body?: Node | string;
  wide?: boolean;
  actions?: Array<{
    label: string;
    variant?: "primary" | "danger" | "ghost";
    onClick: (close: () => void) => void | Promise<void>;
    keepOpen?: boolean;
  }>;
  onClose?: () => void;
}

export function modal(options: ModalOptions): () => void {
  const backdrop = h("div", { class: "modal-backdrop", role: "presentation" });
  const close = () => {
    backdrop.remove();
    document.removeEventListener("keydown", onKey);
    options.onClose?.();
  };
  const onKey = (event: KeyboardEvent) => {
    if (event.key === "Escape") close();
  };

  const modalEl = h(
    "div",
    { class: `modal${options.wide ? " modal-wide" : ""}`, role: "dialog", ariaLabel: options.title },
  );
  const head = h(
    "div",
    { class: "modal-head" },
    h("div", { class: "modal-title" }, options.title),
    h("button", {
      class: "win-control win-control-close",
      type: "button",
      ariaLabel: "Close dialog",
      onClick: close,
    }, "×"),
  );
  const body = h("div", { class: "modal-body" });
  if (typeof options.body === "string") {
    body.appendChild(document.createTextNode(options.body));
  } else if (options.body) {
    body.appendChild(options.body);
  }
  modalEl.append(head, body);

  if (options.actions && options.actions.length > 0) {
    const foot = h("div", { class: "modal-foot" });
    for (const action of options.actions) {
      const btn = h(
        "button",
        { class: `btn ${action.variant === "danger" ? "btn-danger" : action.variant === "ghost" ? "btn-ghost" : "btn-primary"}`, type: "button" },
        action.label,
      );
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
          await action.onClick(close);
        } finally {
          btn.disabled = false;
        }
        if (!action.keepOpen) close();
      });
      foot.appendChild(btn);
    }
    modalEl.appendChild(foot);
  }

  backdrop.appendChild(modalEl);
  backdrop.addEventListener("pointerdown", (event) => {
    if (event.target === backdrop) close();
  });
  document.addEventListener("keydown", onKey);
  document.body.appendChild(backdrop);
  return close;
}

export function confirmDialog(options: {
  title: string;
  message: string;
  confirmLabel?: string;
  danger?: boolean;
  requireTyped?: string;
}): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const body = h("div", { class: "col" });
    body.appendChild(h("p", {}, options.message));
    let typedInput: HTMLInputElement | null = null;
    if (options.requireTyped) {
      body.appendChild(
        h("p", { class: "small muted" }, `Type "${options.requireTyped}" to confirm.`),
      );
      typedInput = h("input", { class: "input", type: "text", placeholder: options.requireTyped });
      body.appendChild(typedInput);
    }
    const finish = (value: boolean) => {
      if (settled) return;
      settled = true;
      close();
      resolve(value);
    };
    const close = modal({
      title: options.title,
      body,
      onClose: () => finish(false),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: options.confirmLabel ?? "Confirm",
          variant: options.danger ? "danger" : "primary",
          onClick: () => {
            if (typedInput && typedInput.value.trim() !== options.requireTyped) {
              typedInput.focus();
              typedInput.classList.add("has-error");
              settled = false;
              return;
            }
            finish(true);
          },
        },
      ],
    });
  });
}

export function promptDialog(options: {
  title: string;
  label: string;
  value?: string;
  placeholder?: string;
  type?: string;
  confirmLabel?: string;
  validate?: (value: string) => string | null;
}): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    const input = h("input", {
      class: "input",
      type: options.type ?? "text",
      value: options.value ?? "",
      placeholder: options.placeholder ?? "",
    });
    const error = h("div", { class: "field-hint", style: "color:var(--danger)" });
    const body = h("div", { class: "col" }, h("label", { class: "field" }, h("span", { class: "field-label" }, options.label), input), error);
    const finish = (value: string | null) => {
      if (settled) return;
      settled = true;
      close();
      resolve(value);
    };
    const close = modal({
      title: options.title,
      body,
      onClose: () => finish(null),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: options.confirmLabel ?? "Save",
          variant: "primary",
          onClick: () => {
            const value = input.value;
            const message = options.validate?.(value) ?? null;
            if (message) {
              error.textContent = message;
              settled = false;
              throw new Error("validation");
            }
            finish(value);
          },
        },
      ],
    });
    window.setTimeout(() => {
      input.focus();
      input.select();
    }, 30);
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        (document.querySelector(".modal-foot .btn-primary") as HTMLButtonElement | null)?.click();
      }
    });
  });
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

/** Anchor a context menu at pointer coordinates. */
export function contextMenu(
  x: number,
  y: number,
  items: Array<{ label: string; icon?: string; danger?: boolean; disabled?: boolean; onClick: () => void } | "separator">,
): () => void {
  const menu = h("div", { class: "context-menu", style: `left:${x}px;top:${y}px;position:fixed;z-index:400` });
  const close = () => {
    menu.remove();
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey);
  };
  const outside = (event: PointerEvent) => {
    if (!menu.contains(event.target as Node)) close();
  };
  const onKey = (event: KeyboardEvent) => {
    if (event.key === "Escape") close();
  };
  for (const item of items) {
    if (item === "separator") {
      menu.appendChild(h("div", { class: "menu-sep" }));
      continue;
    }
    const btn = h("button", {
      class: `menu-item${item.danger ? " menu-item-danger" : ""}`,
      type: "button",
      onClick: () => {
        close();
        item.onClick();
      },
    }, item.label);
    if (item.disabled) btn.disabled = true;
    menu.appendChild(btn);
  }
  document.body.appendChild(menu);
  // Keep the menu inside the viewport.
  const rect = menu.getBoundingClientRect();
  if (rect.right > window.innerWidth - 8) menu.style.left = `${Math.max(8, x - rect.width)}px`;
  if (rect.bottom > window.innerHeight - 8) menu.style.top = `${Math.max(8, y - rect.height)}px`;
  document.addEventListener("pointerdown", outside, true);
  document.addEventListener("keydown", onKey);
  return close;
}

/** Simple inline sparkline renderer used by the monitor app. */
export function sparkline(values: number[], options: { width?: number; height?: number; color?: string } = {}): SVGSVGElement {
  const width = options.width ?? 160;
  const height = options.height ?? 36;
  const color = options.color ?? "var(--accent)";
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("width", String(width));
  svg.setAttribute("height", String(height));
  svg.classList.add("sparkline");
  if (values.length > 1) {
    const min = Math.min(...values);
    const max = Math.max(...values);
    const range = max - min || 1;
    const step = width / (values.length - 1);
    const points = values
      .map((value, index) => `${(index * step).toFixed(1)},${(height - ((value - min) / range) * (height - 4) - 2).toFixed(1)}`)
      .join(" ");
    const polyline = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
    polyline.setAttribute("points", points);
    polyline.setAttribute("fill", "none");
    polyline.setAttribute("stroke", color);
    polyline.setAttribute("stroke-width", "1.6");
    polyline.setAttribute("stroke-linejoin", "round");
    polyline.setAttribute("stroke-linecap", "round");
    svg.appendChild(polyline);
  }
  return svg;
}
