// Modal dialogs, confirmations and small UI utilities.

import { clientToStage, displayFactor, stageSize } from "./display";
import { h } from "./dom";
import { iconSvg } from "./icons";

/**
 * Ties a dialog to the window that opened it (`AppContext.dialog`): the
 * desktop keeps it, and what is typed in it, with the window.
 */
export interface ModalSession {
  opened(el: HTMLElement, close: () => void): void;
  closed(): void;
  /** Something inside the window the dialog belongs to: it opens over that window. */
  readonly host?: HTMLElement;
}

/** A dialog kept with its window (`AppContext.dialog`). */
export interface DialogSession extends ModalSession {
  /**
   * What the dialog is about changed while it is open (e.g. how many rows it
   * has), so that opening it again builds the same dialog.
   */
  update(params: unknown): void;
}

export interface ModalOptions {
  title: string;
  body?: Node | string;
  wide?: boolean;
  /** When false the dialog cannot be dismissed (used for forced password change). */
  dismissible?: boolean;
  actions?: Array<{
    label: string;
    variant?: "primary" | "danger" | "ghost";
    onClick: (close: () => void) => void | Promise<void>;
    keepOpen?: boolean;
  }>;
  onClose?: () => void;
  /** Keep the dialog with its window (see `AppContext.dialog`). */
  session?: ModalSession;
  /**
   * Something inside the app window the dialog belongs to (e.g. `ctx.root`):
   * the dialog opens over that window only, like an app's dialog on other
   * systems, and leaves the rest of the desktop usable. Without it (and
   * without a window's `session`) the dialog covers the whole desktop.
   */
  within?: HTMLElement;
}

/** Sent to a window's element when it closes: its dialogs close too. */
export const WINDOW_CLOSING = "rast:window-closing";

/** The window a dialog opened `within` this element belongs to, if any. */
function ownerWindow(el: HTMLElement | undefined): HTMLElement | null {
  return el?.closest<HTMLElement>(".window") ?? null;
}

/** Dialogs that cover the whole desktop (not one window). */
function desktopDialogs(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>("body > .modal-backdrop:not(.modal-leaving)"));
}

/**
 * Whether a dialog keeps the user from working at `el`: one over the whole
 * desktop, or one over the window `el` is in. Global shortcuts pass nothing
 * and only stop for dialogs over the whole desktop.
 */
export function dialogOpen(el?: Element | null): boolean {
  if (desktopDialogs().length > 0) return true;
  const win = el?.closest(".window");
  return Boolean(win?.querySelector(":scope > .modal-backdrop:not(.modal-leaving)"));
}

function focusables(root: HTMLElement): HTMLElement[] {
  return Array.from(
    root.querySelectorAll<HTMLElement>(
      'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
    ),
  ).filter((el) => el.offsetParent !== null || el === document.activeElement);
}

export function modal(options: ModalOptions): () => void {
  const dismissible = options.dismissible !== false;
  const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const owner = ownerWindow(options.within ?? options.session?.host);
  const backdrop = h("div", {
    class: `modal-backdrop${owner ? " modal-in-window" : ""}`,
    role: "presentation",
  });
  const modalEl = h(
    "div",
    { class: `modal${options.wide ? " modal-wide" : ""}`, role: "dialog", ariaModal: true, ariaLabel: options.title },
  );

  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    backdrop.classList.add("modal-leaving");
    document.removeEventListener("keydown", onKey, true);
    owner?.removeEventListener(WINDOW_CLOSING, close);
    window.setTimeout(() => {
      backdrop.remove();
      // Back to where the user was, unless they went on to work elsewhere.
      const active = document.activeElement;
      if (!owner || !active || active === document.body || owner.contains(active)) {
        previousFocus?.focus({ preventScroll: true });
      }
    }, 120);
    options.session?.closed();
    options.onClose?.();
  };

  /** Keys belong to the topmost dialog of where the user is working. */
  const ownsKeys = (): boolean => {
    if (!backdrop.isConnected) return false;
    const desktop = desktopDialogs();
    if (!owner) return desktop[desktop.length - 1] === backdrop;
    if (desktop.length > 0) return false;
    // Only while its window is the one in front, and the keys are not for
    // something outside it (the start menu, another window's field).
    if (!owner.classList.contains("is-focused")) return false;
    const active = document.activeElement;
    if (active && active !== document.body && !owner.contains(active)) return false;
    const mine = owner.querySelectorAll(":scope > .modal-backdrop:not(.modal-leaving)");
    return mine[mine.length - 1] === backdrop;
  };

  const onKey = (event: KeyboardEvent) => {
    if (!ownsKeys()) return;
    if (event.key === "Escape") {
      if (dismissible) {
        event.preventDefault();
        event.stopPropagation();
        close();
      }
      return;
    }
    if (event.key !== "Tab") return;
    const items = focusables(modalEl);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (!first || !last) return;
    if (!modalEl.contains(document.activeElement)) {
      event.preventDefault();
      (event.shiftKey ? last : first).focus();
    } else if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  const head = h(
    "div",
    { class: "modal-head" },
    h("div", { class: "modal-title" }, options.title),
    dismissible
      ? h(
          "button",
          { class: "win-control win-control-close modal-close", type: "button", ariaLabel: "Close dialog", onClick: close },
          "×",
        )
      : null,
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
        {
          class: `btn ${action.variant === "danger" ? "btn-danger" : action.variant === "ghost" ? "btn-ghost" : "btn-primary"}`,
          type: "button",
        },
        action.label,
      );
      btn.addEventListener("click", async () => {
        if (btn.disabled) return;
        btn.disabled = true;
        btn.setAttribute("aria-busy", "true");
        try {
          await action.onClick(close);
          if (!action.keepOpen) close();
        } catch {
          // Validation failures keep the dialog open; the caller shows details inline.
        } finally {
          if (!closed) {
            btn.disabled = false;
            btn.removeAttribute("aria-busy");
          }
        }
      });
      foot.appendChild(btn);
    }
    modalEl.appendChild(foot);
  }

  const focusInitial = () => {
    const initial =
      modalEl.querySelector<HTMLElement>("[data-autofocus]") ??
      focusables(modalEl).find((el) => !el.classList.contains("modal-close")) ??
      modalEl.querySelector<HTMLElement>(".modal-foot .btn-primary");
    initial?.focus({ preventScroll: true });
  };

  backdrop.appendChild(modalEl);
  backdrop.addEventListener("pointerdown", (event) => {
    if (event.target !== backdrop) return;
    if (owner) {
      // The window's content waits for its dialog (as on other systems):
      // clicking it brings the dialog back to attention instead of closing it.
      event.preventDefault();
      modalEl.classList.remove("modal-nudge");
      void modalEl.offsetWidth;
      modalEl.classList.add("modal-nudge");
      if (!modalEl.contains(document.activeElement)) focusInitial();
    } else if (dismissible) {
      close();
    }
  });
  document.addEventListener("keydown", onKey, true);
  if (owner) {
    owner.appendChild(backdrop);
    owner.addEventListener(WINDOW_CLOSING, close);
  } else {
    document.body.appendChild(backdrop);
  }
  options.session?.opened(modalEl, close);

  // Move focus inside the dialog once it is attached.
  window.setTimeout(focusInitial, 20);
  return close;
}

export function confirmDialog(options: {
  title: string;
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
  dismissible?: boolean;
  requireTyped?: string;
  session?: ModalSession;
  /** See `ModalOptions.within`. */
  within?: HTMLElement;
}): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    let closeFn: (() => void) | null = null;
    const body = h("div", { class: "col" });
    body.appendChild(h("p", { class: "confirm-message" }, options.message));
    let typedInput: HTMLInputElement | null = null;
    let typedError: HTMLElement | null = null;
    if (options.requireTyped) {
      body.appendChild(h("p", { class: "small muted" }, `Type "${options.requireTyped}" to confirm.`));
      typedInput = h("input", {
        class: "input",
        type: "text",
        placeholder: options.requireTyped,
        autocomplete: "off",
        dataset: { autofocus: "true" },
      });
      typedError = h("div", { class: "field-error", hidden: true });
      body.append(typedInput, typedError);
    }
    const finish = (value: boolean) => {
      if (settled) return;
      settled = true;
      closeFn?.();
      resolve(value);
    };
    closeFn = modal({
      ...(options.session ? { session: options.session } : {}),
      ...(options.within ? { within: options.within } : {}),
      title: options.title,
      body,
      dismissible: options.dismissible !== false,
      onClose: () => finish(false),
      actions: [
        {
          label: options.cancelLabel ?? "Cancel",
          variant: "ghost",
          onClick: () => finish(false),
        },
        {
          label: options.confirmLabel ?? "Confirm",
          variant: options.danger ? "danger" : "primary",
          keepOpen: true,
          onClick: () => {
            if (typedInput && typedInput.value.trim() !== options.requireTyped) {
              typedInput.classList.add("has-error");
              typedInput.setAttribute("aria-invalid", "true");
              if (typedError) {
                typedError.textContent = `Enter "${options.requireTyped}" exactly to continue.`;
                typedError.hidden = false;
              }
              typedInput.focus();
              throw new Error("confirmation text does not match");
            }
            finish(true);
          },
        },
      ],
    });
    if (typedInput) {
      window.setTimeout(() => typedInput?.select(), 30);
    }
  });
}

export function promptDialog(options: {
  title: string;
  label: string;
  value?: string;
  placeholder?: string;
  type?: string;
  confirmLabel?: string;
  autocomplete?: string;
  validate?: (value: string) => string | null;
  session?: ModalSession;
  /** See `ModalOptions.within`. */
  within?: HTMLElement;
}): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    let closeFn: (() => void) | null = null;
    const input = h("input", {
      class: "input",
      type: options.type ?? "text",
      value: options.value ?? "",
      placeholder: options.placeholder ?? "",
      autocomplete: options.autocomplete ?? "off",
      dataset: { autofocus: "true" },
    });
    const error = h("div", { class: "field-error", hidden: true });
    const body = h(
      "div",
      {},
      h("label", { class: "field" }, h("span", { class: "field-label" }, options.label), input),
      error,
    );
    const finish = (value: string | null) => {
      if (settled) return;
      settled = true;
      closeFn?.();
      resolve(value);
    };
    closeFn = modal({
      ...(options.session ? { session: options.session } : {}),
      ...(options.within ? { within: options.within } : {}),
      title: options.title,
      body,
      onClose: () => finish(null),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => finish(null) },
        {
          label: options.confirmLabel ?? "Save",
          variant: "primary",
          keepOpen: true,
          onClick: () => {
            const value = input.value;
            const message = options.validate?.(value) ?? null;
            if (message) {
              error.textContent = message;
              error.hidden = false;
              input.classList.add("has-error");
              input.setAttribute("aria-invalid", "true");
              input.focus();
              throw new Error("validation failed");
            }
            finish(value);
          },
        },
      ],
    });
    input.addEventListener("input", () => {
      input.classList.remove("has-error");
      input.removeAttribute("aria-invalid");
      error.hidden = true;
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        (body.closest(".modal")?.querySelector<HTMLButtonElement>(".modal-foot .btn-primary"))?.click();
      }
    });
  });
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

export interface ContextMenuItem {
  label: string;
  icon?: string;
  danger?: boolean;
  disabled?: boolean;
  shortcut?: string;
  /** A second, smaller line under the label. */
  hint?: string;
  /** A choice among several: checked shows a tick (menuitemradio). */
  checked?: boolean;
  onClick: () => void;
}

export type ContextMenuEntry = ContextMenuItem | "separator";

/**
 * Anchor an accessible context menu at pointer coordinates, or under (else
 * above) an `anchor` element such as the button that opened it.
 */
export function contextMenu(
  x: number,
  y: number,
  items: ContextMenuEntry[],
  options: { ariaLabel?: string; anchor?: Element } = {},
): () => void {
  const menu = h("div", {
    class: "context-menu",
    role: "menu",
    ariaLabel: options.ariaLabel ?? "Context menu",
    tabIndex: -1,
  });
  const close = () => {
    menu.classList.add("is-closing");
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey, true);
    window.setTimeout(() => menu.remove(), 80);
  };
  const outside = (event: PointerEvent) => {
    if (!menu.contains(event.target as Node)) close();
  };
  const onKey = (event: KeyboardEvent) => {
    const buttons = Array.from(menu.querySelectorAll<HTMLButtonElement>(".menu-item:not(:disabled)"));
    if (event.key === "Escape") {
      event.preventDefault();
      const restore = menu.dataset.invoker;
      close();
      if (restore) document.getElementById(restore)?.focus();
      return;
    }
    if (event.key === "Tab") {
      close();
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp" && event.key !== "Home" && event.key !== "End") {
      return;
    }
    event.preventDefault();
    if (buttons.length === 0) return;
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    let next: number;
    if (event.key === "Home") next = 0;
    else if (event.key === "End") next = buttons.length - 1;
    else if (event.key === "ArrowDown") next = index < 0 ? 0 : (index + 1) % buttons.length;
    else next = index <= 0 ? buttons.length - 1 : index - 1;
    buttons[next]?.focus();
  };

  for (const item of items) {
    if (item === "separator") {
      menu.appendChild(h("div", { class: "menu-sep", role: "separator" }));
      continue;
    }
    const btn = h("button", {
      class: `menu-item${item.danger ? " menu-item-danger" : ""}${item.hint ? " has-hint" : ""}`,
      type: "button",
      role: item.checked === undefined ? "menuitem" : "menuitemradio",
      onClick: () => {
        if (btn.disabled) return;
        close();
        item.onClick();
      },
    });
    if (item.icon) btn.appendChild(iconSvg(item.icon, 15));
    const label = h("span", { class: "menu-item-label" }, item.label);
    btn.appendChild(item.hint ? h("span", { class: "menu-item-text" }, label, h("span", { class: "menu-item-hint" }, item.hint)) : label);
    if (item.shortcut) btn.appendChild(h("span", { class: "menu-item-shortcut" }, item.shortcut));
    if (item.checked !== undefined) {
      btn.setAttribute("aria-checked", String(item.checked));
      if (item.checked) btn.appendChild(h("span", { class: "menu-item-check" }, iconSvg("check", 14)));
    }
    if (item.disabled) btn.disabled = true;
    menu.appendChild(btn);
  }

  // Callers pass on-screen (client) coordinates; the menu lives in desktop pixels.
  const box = options.anchor?.getBoundingClientRect();
  const at = box ? clientToStage(box.left, box.bottom + 4) : clientToStage(x, y);
  menu.style.left = `${at.x}px`;
  menu.style.top = `${at.y}px`;
  document.body.appendChild(menu);
  // Keep the menu inside the desktop.
  const f = displayFactor();
  const rect = menu.getBoundingClientRect();
  const width = rect.width / f;
  const height = rect.height / f;
  const stage = stageSize();
  if (at.x + width > stage.width - 8) {
    const right = box ? clientToStage(box.right, 0).x : at.x;
    menu.style.left = `${Math.max(8, right - width)}px`;
  }
  if (at.y + height > stage.height - 8) {
    const top = box ? clientToStage(0, box.top - 4).y : at.y;
    menu.style.top = `${Math.max(8, top - height)}px`;
  }
  if (box) {
    // Focus returns to the button when the menu is dismissed with Escape.
    const invoker = options.anchor as HTMLElement;
    if (!invoker.id) invoker.id = `menu-invoker-${Math.random().toString(36).slice(2, 9)}`;
    menu.dataset.invoker = invoker.id;
  }
  document.addEventListener("pointerdown", outside, true);
  document.addEventListener("keydown", onKey, true);
  window.setTimeout(
    () =>
      (
        menu.querySelector<HTMLButtonElement>('.menu-item[aria-checked="true"]:not(:disabled)') ??
        menu.querySelector<HTMLButtonElement>(".menu-item:not(:disabled)")
      )?.focus({ preventScroll: true }),
    10,
  );
  return close;
}

/** Copy text to the clipboard, returning false when the browser blocks it. */
export async function copyText(value: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    /* fall through to the legacy path */
  }
  try {
    const area = h("textarea", { class: "sr-only" });
    area.value = value;
    document.body.appendChild(area);
    area.focus();
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    return ok;
  } catch {
    return false;
  }
}

/** Simple inline sparkline renderer used by the monitor app. */
export function sparkline(
  values: number[],
  options: { width?: number; height?: number; color?: string } = {},
): SVGSVGElement {
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
      .map(
        (value, index) =>
          `${(index * step).toFixed(1)},${(height - ((value - min) / range) * (height - 4) - 2).toFixed(1)}`,
      )
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
