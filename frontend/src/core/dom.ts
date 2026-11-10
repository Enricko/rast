// Tiny DOM helpers. Everything is built with createElement/textContent so user
// data can never be interpreted as HTML (no innerHTML with untrusted input).

type Child = Node | string | number | null | undefined | false;

export interface ElementAttrs {
  class?: string;
  id?: string;
  title?: string;
  type?: string;
  value?: string;
  placeholder?: string;
  href?: string;
  target?: string;
  rel?: string;
  disabled?: boolean;
  checked?: boolean;
  selected?: boolean;
  hidden?: boolean;
  tabIndex?: number;
  role?: string;
  ariaLabel?: string;
  name?: string;
  accept?: string;
  multiple?: boolean;
  autocomplete?: string;
  colspan?: number;
  rowspan?: number;
  min?: string;
  max?: string;
  step?: string;
  style?: string;
  dataset?: Record<string, string>;
  onClick?: (event: MouseEvent) => void;
  onInput?: (event: Event) => void;
  onChange?: (event: Event) => void;
  onKeyDown?: (event: KeyboardEvent) => void;
  onSubmit?: (event: SubmitEvent) => void;
}

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: ElementAttrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  applyAttrs(el, attrs);
  appendChildren(el, children);
  return el;
}

export function applyAttrs(el: HTMLElement, attrs: ElementAttrs): void {
  if (attrs.class) el.className = attrs.class;
  if (attrs.id) el.id = attrs.id;
  if (attrs.title) el.title = attrs.title;
  if (attrs.role) el.setAttribute("role", attrs.role);
  if (attrs.ariaLabel) el.setAttribute("aria-label", attrs.ariaLabel);
  if (attrs.tabIndex !== undefined) el.tabIndex = attrs.tabIndex;
  if (attrs.dataset) {
    for (const [key, value] of Object.entries(attrs.dataset)) el.dataset[key] = value;
  }
  if (attrs.style) el.setAttribute("style", attrs.style);

  if (el instanceof HTMLInputElement) {
    if (attrs.type) el.setAttribute("type", attrs.type);
    if (attrs.value !== undefined) el.value = attrs.value;
    if (attrs.placeholder) el.placeholder = attrs.placeholder;
    if (attrs.name) el.name = attrs.name;
    if (attrs.accept) el.accept = attrs.accept;
    if (attrs.multiple) el.multiple = true;
    if (attrs.autocomplete) el.setAttribute("autocomplete", attrs.autocomplete);
    if (attrs.min !== undefined) el.min = attrs.min;
    if (attrs.max !== undefined) el.max = attrs.max;
    if (attrs.step !== undefined) el.step = attrs.step;
    if (attrs.disabled) el.disabled = true;
    if (attrs.checked) el.checked = true;
  }
  if (el instanceof HTMLButtonElement) {
    if (attrs.type) el.setAttribute("type", attrs.type);
    if (attrs.disabled) el.disabled = true;
  }
  if (el instanceof HTMLSelectElement) {
    if (attrs.name) el.name = attrs.name;
    if (attrs.disabled) el.disabled = true;
  }
  if (el instanceof HTMLOptionElement) {
    if (attrs.value !== undefined) el.value = attrs.value;
    if (attrs.selected) el.selected = true;
    if (attrs.disabled) el.disabled = true;
  }
  if (attrs.href !== undefined && (el instanceof HTMLAnchorElement)) el.href = attrs.href;
  if (attrs.target && el instanceof HTMLAnchorElement) el.target = attrs.target;
  if (attrs.rel && el instanceof HTMLAnchorElement) el.rel = attrs.rel;

  if (attrs.colspan !== undefined && el instanceof HTMLTableCellElement) {
    el.colSpan = attrs.colspan;
  }
  if (attrs.rowspan !== undefined && el instanceof HTMLTableCellElement) {
    el.rowSpan = attrs.rowspan;
  }
  if (attrs.onClick) el.addEventListener("click", attrs.onClick as EventListener);
  if (attrs.onInput) el.addEventListener("input", attrs.onInput);
  if (attrs.onChange) el.addEventListener("change", attrs.onChange);
  if (attrs.onKeyDown) el.addEventListener("keydown", attrs.onKeyDown as EventListener);
  if (attrs.onSubmit) el.addEventListener("submit", attrs.onSubmit as EventListener);
}

export function appendChildren(el: HTMLElement, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      el.appendChild(document.createTextNode(String(child)));
    } else {
      el.appendChild(child);
    }
  }
}

export function clear(el: HTMLElement): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}

export function qs<T extends Element = HTMLElement>(selector: string, root: ParentNode = document): T | null {
  return root.querySelector<T>(selector);
}

export function qsa<T extends Element = HTMLElement>(selector: string, root: ParentNode = document): T[] {
  return Array.from(root.querySelectorAll<T>(selector));
}

export function on<K extends keyof HTMLElementEventMap>(
  el: EventTarget,
  type: K,
  handler: (event: HTMLElementEventMap[K]) => void,
  options?: AddEventListenerOptions,
): () => void {
  el.addEventListener(type, handler as EventListener, options);
  return () => el.removeEventListener(type, handler as EventListener, options);
}

export function debounce<A extends unknown[]>(fn: (...args: A) => void, wait = 250): (...args: A) => void {
  let timer: number | undefined;
  return (...args: A) => {
    if (timer !== undefined) window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  };
}

/** Inject text safely. */
export function text(value: string): Text {
  return document.createTextNode(value);
}

/** Small helper for building a labelled form field. */
export function field(label: string, input: HTMLElement, hint?: string): HTMLElement {
  return h(
    "label",
    { class: "field" },
    h("span", { class: "field-label" }, label),
    input,
    hint ? h("span", { class: "field-hint" }, hint) : null,
  );
}

export function button(
  label: string,
  onClick: (event: MouseEvent) => void,
  opts: { variant?: "primary" | "ghost" | "danger" | "subtle"; icon?: Node; disabled?: boolean } = {},
): HTMLButtonElement {
  const btn = h("button", { class: `btn btn-${opts.variant ?? "ghost"}`, type: "button", onClick });
  if (opts.icon) btn.appendChild(opts.icon);
  if (label) btn.appendChild(document.createTextNode(label));
  if (opts.disabled) btn.disabled = true;
  return btn;
}

export function spinner(label?: string): HTMLElement {
  return h(
    "div",
    { class: "spinner-wrap", role: "status" },
    h("span", { class: "spinner" }),
    label ? h("span", { class: "spinner-label" }, label) : null,
  );
}

export function emptyState(title: string, detail?: string, action?: Node): HTMLElement {
  return h(
    "div",
    { class: "empty-state" },
    h("div", { class: "empty-title" }, title),
    detail ? h("div", { class: "empty-detail" }, detail) : null,
    action ?? null,
  );
}

export function errorState(title: string, detail?: string, retry?: () => void): HTMLElement {
  return h(
    "div",
    { class: "error-state" },
    h("div", { class: "error-title" }, title),
    detail ? h("div", { class: "error-detail" }, detail) : null,
    retry ? button("Retry", () => retry()) : null,
  );
}
