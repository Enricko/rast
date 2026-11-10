// Small reusable UI building blocks used by the shell and admin apps.
// Built on top of h()/textContent only — never innerHTML with data.

import { h } from "./dom";
import { formatBytes } from "./format";
import { iconSvg } from "./icons";
import { copyText } from "./ui";

export type Tone = "ok" | "warn" | "error" | "info" | "neutral";

export function badge(label: string, tone: Tone = "neutral", title?: string): HTMLElement {
  const el = h("span", { class: `badge${tone === "neutral" ? "" : ` ${tone}`}` }, label);
  if (title) el.title = title;
  return el;
}

const HEALTH_TONE: Record<string, Tone> = {
  ok: "ok",
  success: "ok",
  running: "ok",
  active: "ok",
  warning: "warn",
  partial: "warn",
  deploying: "info",
  disabled: "neutral",
  stopped: "neutral",
  error: "error",
  failed: "error",
  failure: "error",
  denied: "warn",
};

/** Colour-coded badge for status strings coming from the API. */
export function statusBadge(status: string): HTMLElement {
  const text = status || "unknown";
  return badge(text, HEALTH_TONE[text] ?? "neutral");
}

export interface CardOptions {
  title?: string;
  subtitle?: string;
  actions?: Node | Node[];
  children?: Array<Node | string | null | false>;
  class?: string;
  id?: string;
  padded?: boolean;
}

/** Bordered content card with an optional header. */
export function card(options: CardOptions): HTMLElement {
  const header =
    options.title || options.subtitle || options.actions
      ? h(
          "div",
          { class: "card-head" },
          h(
            "div",
            { class: "card-heading" },
            options.title ? h("h3", { class: "card-title" }, options.title) : null,
            options.subtitle ? h("div", { class: "card-subtitle" }, options.subtitle) : null,
          ),
          options.actions ? h("div", { class: "card-actions" }, ...(Array.isArray(options.actions) ? options.actions : [options.actions])) : null,
        )
      : null;
  const body = h("div", { class: `card-body${options.padded === false ? " is-flush" : ""}` });
  for (const child of options.children ?? []) {
    if (child === null || child === undefined || child === false) continue;
    body.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
  const el = h("section", {
    class: `card${options.class ? ` ${options.class}` : ""}`,
    ...(options.id ? { id: options.id } : {}),
  });
  if (header) el.appendChild(header);
  el.appendChild(body);
  return el;
}

export function banner(options: {
  kind: "info" | "warn" | "error" | "success";
  title?: string;
  message: string | Node;
  action?: Node;
  class?: string;
}): HTMLElement {
  const iconName = options.kind === "error" ? "warning" : options.kind === "warn" ? "warning" : options.kind === "success" ? "check" : "info";
  const el = h("div", { class: `banner banner-${options.kind}${options.class ? ` ${options.class}` : ""}`, role: options.kind === "error" ? "alert" : "status" });
  el.appendChild(h("span", { class: "banner-icon" }, iconSvg(iconName, 17)));
  const body = h("div", { class: "banner-body" });
  if (options.title) body.appendChild(h("div", { class: "banner-title" }, options.title));
  body.appendChild(typeof options.message === "string" ? h("div", { class: "banner-message" }, options.message) : options.message);
  el.appendChild(body);
  if (options.action) el.appendChild(h("div", { class: "banner-action" }, options.action));
  return el;
}

export interface IconButtonOptions {
  variant?: "ghost" | "subtle" | "primary" | "danger";
  size?: "sm" | "md";
  title?: string;
  disabled?: boolean;
}

export function iconButton(iconName: string, label: string, onClick: (event: MouseEvent) => void, options: IconButtonOptions = {}): HTMLButtonElement {
  const size = options.size === "sm" ? " btn-sm" : "";
  const btn = h(
    "button",
    {
      class: `btn btn-icon btn-${options.variant ?? "ghost"}${size}`,
      type: "button",
      ariaLabel: label,
      title: options.title ?? label,
      onClick,
    },
    iconSvg(iconName, options.size === "sm" ? 15 : 17),
  );
  if (options.disabled) btn.disabled = true;
  return btn;
}

export function textButton(label: string, onClick: (event: MouseEvent) => void, options: IconButtonOptions & { icon?: string; type?: "button" | "submit" } = {}): HTMLButtonElement {
  const btn = h(
    "button",
    {
      class: `btn btn-${options.variant ?? "ghost"}${options.size === "sm" ? " btn-sm" : ""}`,
      type: options.type ?? "button",
      onClick,
    },
    options.icon ? iconSvg(options.icon, 16) : null,
    label,
  );
  if (options.disabled) btn.disabled = true;
  if (options.title) btn.title = options.title;
  return btn;
}

export interface SwitchControl {
  el: HTMLElement;
  input: HTMLInputElement;
  isChecked(): boolean;
  setChecked(value: boolean): void;
}

/** Accessible toggle switch backed by a real checkbox. */
export function switchControl(options: {
  label: string;
  checked?: boolean;
  disabled?: boolean;
  hint?: string;
  onChange?: (checked: boolean) => void;
  id?: string;
}): SwitchControl {
  const input = h("input", {
    class: "switch-input",
    type: "checkbox",
    checked: options.checked ?? false,
    ariaLabel: options.label,
    ...(options.id ? { id: options.id } : {}),
  });
  if (options.disabled) input.disabled = true;
  const el = h(
    "label",
    { class: `switch${options.disabled ? " is-disabled" : ""}` },
    input,
    h("span", { class: "switch-track", ariaHidden: true }, h("span", { class: "switch-thumb" })),
    h("span", { class: "switch-label" }, options.label, options.hint ? h("span", { class: "switch-hint" }, options.hint) : null),
  );
  input.addEventListener("change", () => options.onChange?.(input.checked));
  return {
    el,
    input,
    isChecked: () => input.checked,
    setChecked: (value: boolean) => {
      input.checked = value;
    },
  };
}

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
  icon?: string;
}

export interface Segmented<T extends string> {
  el: HTMLElement;
  value(): T;
  setValue(value: T): void;
}

/** Segmented control (radio group) with arrow-key support. */
export function segmented<T extends string>(options: {
  items: Array<SegmentedOption<T>>;
  value: T;
  ariaLabel: string;
  onChange?: (value: T) => void;
}): Segmented<T> {
  let current = options.value;
  const buttons: HTMLButtonElement[] = [];
  const el = h("div", { class: "segmented", role: "radiogroup", ariaLabel: options.ariaLabel });
  const select = (value: T, fire = true) => {
    current = value;
    for (const button of buttons) {
      const isActive = button.dataset.value === value;
      button.classList.toggle("is-active", isActive);
      button.setAttribute("aria-checked", String(isActive));
      button.tabIndex = isActive ? 0 : -1;
    }
    if (fire) options.onChange?.(value);
  };
  for (const item of options.items) {
    const button = h(
      "button",
      {
        class: "segment",
        type: "button",
        role: "radio",
        dataset: { value: item.value },
        onClick: () => select(item.value),
        onKeyDown: (event) => {
          if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
          event.preventDefault();
          const index = buttons.indexOf(button);
          const delta = event.key === "ArrowRight" ? 1 : -1;
          const next = buttons[(index + delta + buttons.length) % buttons.length];
          next?.focus();
          const value = next?.dataset.value;
          if (value) select(value as T);
        },
      },
      item.icon ? iconSvg(item.icon, 15) : null,
      h("span", {}, item.label),
    );
    buttons.push(button);
    el.appendChild(button);
  }
  select(current, false);
  return {
    el,
    value: () => current,
    setValue: (value: T) => select(value, false),
  };
}

export interface Pager {
  el: HTMLElement;
  update(state: { page: number; totalPages: number; totalItems: number; label?: string }): void;
}

/** Bounded pagination footer (prev / page indicator / next). */
export function pager(onChange: (page: number) => void): Pager {
  const prev = h("button", { class: "btn btn-sm", type: "button", ariaLabel: "Previous page" }, "Previous");
  const next = h("button", { class: "btn btn-sm", type: "button", ariaLabel: "Next page" }, "Next");
  const label = h("div", { class: "small muted pager-label" });
  const el = h("div", { class: "pager" }, h("div", { class: "pager-spacer" }), label, prev, next);
  let page = 1;
  let totalPages = 1;
  prev.addEventListener("click", () => {
    if (page <= 1) return;
    onChange(page - 1);
  });
  next.addEventListener("click", () => {
    if (page >= totalPages) return;
    onChange(page + 1);
  });
  return {
    el,
    update: (state) => {
      page = state.page;
      totalPages = Math.max(1, state.totalPages);
      prev.disabled = page <= 1;
      next.disabled = page >= totalPages;
      label.textContent =
        state.label ??
        `Page ${page} of ${totalPages} · ${state.totalItems.toLocaleString()} item${state.totalItems === 1 ? "" : "s"}`;
    },
  };
}

/** Two-or-three column definition list; values may be Nodes for badges. */
export function kvList(rows: Array<[string, Node | string | null]>): HTMLElement {
  const dl = h("dl", { class: "kv" });
  for (const [term, value] of rows) {
    dl.appendChild(h("dt", {}, term));
    const dd = h("dd", {});
    if (value === null) dd.textContent = "—";
    else if (typeof value === "string") dd.textContent = value;
    else dd.appendChild(value);
    dl.appendChild(dd);
  }
  return dl;
}

/** Horizontal usage bar with a textual summary. */
export function usageBar(used: number, total: number, options: { caption?: string; unlimitedLabel?: string } = {}): HTMLElement {
  const unlimited = !total || total <= 0;
  const percent = unlimited ? 0 : Math.min(100, Math.max(0, (used / total) * 100));
  const tone = percent >= 95 ? " is-danger" : percent >= 85 ? " is-warn" : "";
  const bar = h(
    "div",
    { class: "usage" },
    h("div", { class: "progress", role: "progressbar", ariaLabel: "Storage usage", ariaValueNow: String(Math.round(percent)), ariaValueMin: "0", ariaValueMax: "100" }, h("div", { class: `progress-bar${tone}`, style: `width:${percent.toFixed(1)}%` })),
    h(
      "div",
      { class: "usage-caption small muted" },
      options.caption ??
        (unlimited
          ? `${formatBytes(used)} used · ${options.unlimitedLabel ?? "no quota"}`
          : `${formatBytes(used)} of ${formatBytes(total)} used (${percent.toFixed(0)}%)`),
    ),
  );
  return bar;
}

/** Icon-only copy button with a transient "Copied" state. */
export function copyButton(value: string, label = "Copy"): HTMLButtonElement {
  const btn = iconButton("copy", label, () => {
    void copyText(value).then((ok) => {
      btn.classList.add(ok ? "is-copied" : "is-copy-failed");
      window.setTimeout(() => {
        btn.classList.remove("is-copied", "is-copy-failed");
      }, 1400);
    });
  }, { size: "sm", variant: "subtle" });
  btn.classList.add("copy-button");
  return btn;
}

/** Monospace value with a copy affordance, useful for paths and IDs. */
export function codeValue(value: string, options: { copyLabel?: string } = {}): HTMLElement {
  return h(
    "span",
    { class: "code-value" },
    h("code", { class: "mono" }, value),
    copyButton(value, options.copyLabel ?? "Copy value"),
  );
}

/** Placeholder block used while a card is loading (avoids layout jumps). */
export function skeletonBlock(lines = 3): HTMLElement {
  const el = h("div", { class: "skeleton-block", ariaHidden: true });
  for (let index = 0; index < lines; index += 1) {
    el.appendChild(h("div", { class: `skeleton-line${index === lines - 1 ? " is-short" : ""}` }));
  }
  return el;
}

/** Full-width loading indicator with a label. */
export function loadingBlock(label = "Loading…"): HTMLElement {
  return h(
    "div",
    { class: "loading-block", role: "status" },
    h("span", { class: "spinner" }),
    h("span", { class: "muted" }, label),
  );
}
