// What is inside a window: where its scrollable areas are scrolled to and
// what was typed into its text fields. Saved with the window in the desktop
// session, so a reload, another tab or another device shows the same, and
// followed live from the account's other pages.
//
// Scroll areas and fields are named by where they are in the window (the
// app's DOM is the same everywhere; only the screen differs). A scroll
// position is kept in pixels for a window of the same width and as a share
// of the way down for other widths. Only text the user typed is kept (not
// what the app filled in), never password, one-time-code or card fields,
// and nothing (typed or scrolled) in an element marked `data-sync="off"`,
// such as the Terminal (its shell keeps what was typed and shown). Text in a field the user is typing
// in on this page is never replaced by another page's.

export interface ScrollPosition {
  path: string;
  x: number;
  y: number;
  /** Share of the way down (0..1), for windows of another width. */
  ratio: number;
  /** Width of the scroll area the pixels are for. */
  width: number;
}

export interface FieldText {
  field: string;
  value: string;
}

export interface WindowView {
  scroll?: ScrollPosition[];
  fields?: FieldText[];
}

const MAX_SCROLLS = 32;
const MAX_FIELDS = 64;
/** Longer text is not kept (the server takes at most 64 KiB per field). */
export const MAX_FIELD_CHARS = 16_000;
/** All of one window's view, as JSON. */
export const MAX_VIEW_CHARS = 48_000;
const MAX_NAME = 512;
/** How long to wait for an app to show the area or field to restore. */
const PENDING_MS = 10_000;
const PENDING_POLL_MS = 200;
/** A field typed in this recently here keeps its text over another page's. */
const TYPING_MS = 4_000;

const TEXT_TYPES = new Set(["text", "search", "email", "url", "tel", "number"]);
/** Classes that tell state, not what an element is (ignored in names). */
const STATE_CLASS =
  /^(is-|has-)|^(active|selected|hidden|loading|open|focused|disabled|entering|closing|dragging|dirty|error|empty|busy)$/;
const SECRET_AUTOCOMPLETE = /password|one-time-code|cc-/i;

type TextField = HTMLInputElement | HTMLTextAreaElement;

function signature(el: Element): string {
  const stable = Array.from(el.classList).find((name) => !STATE_CLASS.test(name) && !/[>~\s]/.test(name));
  return stable ? `${el.localName}.${stable}` : el.localName;
}

/** Name of `el` inside `root` (`""` for the root), or null when outside it. */
export function pathOf(el: Element, root: Element): string | null {
  const parts: string[] = [];
  let node: Element = el;
  while (node !== root) {
    const parent = node.parentElement;
    if (!parent) return null;
    const sig = signature(node);
    let index = 0;
    for (const sibling of Array.from(parent.children)) {
      if (sibling === node) break;
      if (signature(sibling) === sig) index += 1;
    }
    parts.push(index ? `${sig}~${index}` : sig);
    node = parent;
  }
  return parts.reverse().join(">");
}

export function findByPath(root: Element, path: string): Element | null {
  if (path === "") return root;
  let node: Element = root;
  for (const part of path.split(">")) {
    const tilde = part.lastIndexOf("~");
    const sig = tilde >= 0 ? part.slice(0, tilde) : part;
    const wanted = tilde >= 0 ? Number(part.slice(tilde + 1)) : 0;
    let index = 0;
    let found: Element | null = null;
    for (const child of Array.from(node.children)) {
      if (signature(child) !== sig) continue;
      if (index === wanted) {
        found = child;
        break;
      }
      index += 1;
    }
    if (!found) return null;
    node = found;
  }
  return node;
}

/** A text field whose typed text may be kept. */
export function syncableField(el: unknown): el is TextField {
  let field: TextField;
  if (el instanceof HTMLTextAreaElement) field = el;
  else if (el instanceof HTMLInputElement && TEXT_TYPES.has(el.type)) field = el;
  else return false;
  if (field.readOnly || field.disabled) return false;
  if (SECRET_AUTOCOMPLETE.test(field.getAttribute("autocomplete") ?? "")) return false;
  return !field.closest('[data-sync="off"], .xterm');
}

function describe(field: TextField): string {
  const label =
    field.name ||
    field.getAttribute("aria-label") ||
    field.placeholder ||
    field.labels?.[0]?.textContent?.trim() ||
    "";
  return label.slice(0, 200);
}

/** Name of a field: what it is called, and which of the fields so called. */
function fieldName(field: TextField, root: Element): string | null {
  const label = describe(field);
  if (!label) {
    const path = pathOf(field, root);
    return path === null ? null : `@${path}`;
  }
  let index = 0;
  for (const other of Array.from(root.querySelectorAll("input, textarea"))) {
    if (other === field) return `${field.localName}|${label}|${index}`;
    if (syncableField(other) && other.localName === field.localName && describe(other) === label) index += 1;
  }
  return null;
}

function findField(root: Element, name: string): TextField | null {
  if (name.startsWith("@")) {
    const el = findByPath(root, name.slice(1));
    return syncableField(el) ? el : null;
  }
  for (const el of Array.from(root.querySelectorAll("input, textarea"))) {
    if (syncableField(el) && fieldName(el, root) === name) return el;
  }
  return null;
}

const round4 = (value: number): number => Math.round(value * 10_000) / 10_000;

/** Keep a view within the limits (drop the longest texts first). */
export function boundedView(view: WindowView): WindowView | undefined {
  const scroll = (view.scroll ?? []).slice(0, MAX_SCROLLS);
  let fields = (view.fields ?? []).filter((f) => f.value.length <= MAX_FIELD_CHARS).slice(0, MAX_FIELDS);
  const size = (): number => JSON.stringify({ scroll, fields }).length;
  while (fields.length && size() > MAX_VIEW_CHARS) {
    const longest = fields.reduce((a, b) => (b.value.length > a.value.length ? b : a));
    fields = fields.filter((f) => f !== longest);
  }
  if (!scroll.length && !fields.length) return undefined;
  return { ...(scroll.length ? { scroll } : {}), ...(fields.length ? { fields } : {}) };
}

/** Entries of a saved view that this version can use. */
export function readableView(value: unknown): { scroll: ScrollPosition[]; fields: FieldText[] } {
  const view = (value ?? {}) as WindowView;
  const finite = (n: unknown): n is number => typeof n === "number" && Number.isFinite(n);
  const scroll = Array.isArray(view.scroll)
    ? view.scroll.filter(
        (p) =>
          p &&
          typeof p.path === "string" &&
          p.path.length <= MAX_NAME &&
          finite(p.x) &&
          finite(p.y) &&
          finite(p.ratio) &&
          finite(p.width),
      )
    : [];
  const fields = Array.isArray(view.fields)
    ? view.fields.filter(
        (f) => f && typeof f.field === "string" && f.field.length <= MAX_NAME && typeof f.value === "string",
      )
    : [];
  return { scroll, fields };
}

/** Watches one window's content: what was scrolled and typed, and restores it. */
export class ViewTracker {
  private readonly scrolled = new Set<Element>();
  private readonly edited = new Set<TextField>();
  private readonly typedAt = new WeakMap<Element, number>();
  /** Saved positions shown here, kept while the area stays where it was put. */
  private readonly kept = new Map<string, { el: Element; x: number; y: number; saved: ScrollPosition }>();
  /** Waiting for the app to show the area or field. */
  private pendingScroll = new Map<string, ScrollPosition>();
  private pendingFields = new Map<string, string>();
  private pendingLive = false;
  private poll: number | undefined;
  private deadline = 0;
  private disposed = false;
  private readonly cleanup: Array<() => void> = [];

  constructor(
    private readonly root: HTMLElement,
    private readonly onChange: () => void,
  ) {
    const listen = (type: string, handler: (event: Event) => void, options: AddEventListenerOptions): void => {
      root.addEventListener(type, handler, options);
      this.cleanup.push(() => root.removeEventListener(type, handler, options));
    };
    listen(
      "scroll",
      (event) => {
        const target = event.target;
        if (!(target instanceof Element) || !root.contains(target)) return;
        if (target.closest('[data-sync="off"]')) return;
        this.scrolled.add(target);
        this.onChange();
      },
      { capture: true, passive: true },
    );
    // The user scrolls or clicks: do not jump to a saved position after that.
    const userScrolls = (): void => this.pendingScroll.clear();
    for (const type of ["wheel", "touchstart", "pointerdown"]) listen(type, userScrolls, { capture: true, passive: true });
    listen(
      "keydown",
      (event) => {
        const key = (event as KeyboardEvent).key;
        if (/^(Page(Up|Down)|Arrow(Up|Down)|Home|End| )$/.test(key)) userScrolls();
      },
      { capture: true, passive: true },
    );
    listen(
      "input",
      (event) => {
        if (!event.isTrusted || !syncableField(event.target)) return;
        const field = event.target;
        this.edited.add(field);
        this.typedAt.set(field, Date.now());
        if (this.pendingFields.size) {
          const name = fieldName(field, root);
          if (name) this.pendingFields.delete(name);
        }
        this.onChange();
      },
      { capture: true, passive: true },
    );
  }

  /** What is scrolled and typed in the window (undefined: nothing). */
  capture(): WindowView | undefined {
    if (this.disposed) return undefined;
    const scroll = new Map<string, ScrollPosition>();
    for (const el of this.scrolled) {
      if (!el.isConnected || !this.root.contains(el)) {
        this.scrolled.delete(el);
        continue;
      }
      const path = pathOf(el, this.root);
      if (path === null || path.length > MAX_NAME) continue;
      const x = Math.max(0, Math.round(el.scrollLeft));
      const y = Math.max(0, Math.round(el.scrollTop));
      const kept = this.kept.get(path);
      if (kept && kept.el === el && Math.abs(kept.x - x) <= 1 && Math.abs(kept.y - y) <= 1) {
        scroll.set(path, kept.saved);
        continue;
      }
      this.kept.delete(path);
      if (x === 0 && y === 0) continue;
      const range = el.scrollHeight - el.clientHeight;
      scroll.set(path, {
        path,
        x,
        y,
        ratio: range > 0 ? Math.min(1, round4(y / range)) : 0,
        width: Math.round(el.clientWidth),
      });
    }
    for (const [path, position] of this.pendingScroll) if (!scroll.has(path)) scroll.set(path, position);

    const fields = new Map<string, string>();
    for (const field of this.edited) {
      if (!field.isConnected || !this.root.contains(field) || !syncableField(field)) {
        this.edited.delete(field);
        continue;
      }
      const name = fieldName(field, this.root);
      if (name && name.length <= MAX_NAME) fields.set(name, field.value);
    }
    for (const [name, value] of this.pendingFields) if (!fields.has(name)) fields.set(name, value);

    const byName = <T>(entries: Array<[string, T]>): Array<[string, T]> =>
      entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return boundedView({
      scroll: byName(Array.from(scroll)).map(([, position]) => position),
      fields: byName(Array.from(fields)).map(([field, value]) => ({ field, value })),
    });
  }

  /**
   * Show a saved view: now for what the app already shows, and as the app
   * shows the rest. `live`: another page of the account changed it while
   * this one is open (areas it no longer has scrolled go back to the top).
   */
  apply(value: WindowView | undefined, live: boolean): void {
    if (this.disposed) return;
    const view = readableView(value);
    this.pendingScroll = new Map(view.scroll.map((position) => [position.path, position]));
    if (live) {
      for (const el of this.scrolled) {
        const path = pathOf(el, this.root);
        if (path === null || this.pendingScroll.has(path)) continue;
        this.kept.delete(path);
        if (el.scrollTop || el.scrollLeft) el.scrollTo({ top: 0, left: 0 });
      }
    }
    this.pendingFields = new Map(view.fields.map((field) => [field.field, field.value]));
    this.pendingLive = live;
    this.deadline = Date.now() + PENDING_MS;
    this.tryPending(false);
    if (this.hasPending() && this.poll === undefined) {
      this.poll = window.setInterval(() => {
        const expired = Date.now() >= this.deadline;
        this.tryPending(expired);
        if (expired || !this.hasPending()) this.stopPending();
      }, PENDING_POLL_MS);
    }
  }

  dispose(): void {
    this.disposed = true;
    this.stopPending();
    for (const undo of this.cleanup.splice(0)) undo();
    this.scrolled.clear();
    this.edited.clear();
    this.kept.clear();
  }

  private hasPending(): boolean {
    return this.pendingScroll.size > 0 || this.pendingFields.size > 0;
  }

  private stopPending(): void {
    if (this.poll !== undefined) window.clearInterval(this.poll);
    this.poll = undefined;
    this.pendingScroll.clear();
    this.pendingFields.clear();
  }

  /** Restore what the app shows by now; `force`: as far as it goes. */
  private tryPending(force: boolean): void {
    for (const [path, position] of Array.from(this.pendingScroll)) {
      const el = findByPath(this.root, path);
      if (!el) continue;
      const range = el.scrollHeight - el.clientHeight;
      const sameWidth = Math.abs(el.clientWidth - position.width) <= 1;
      const y = sameWidth ? position.y : Math.round(position.ratio * Math.max(0, range));
      // Not as long as it was yet: the app is still loading what it shows.
      if (!force && ((range <= 0 && position.y > 0) || range < y - 1)) continue;
      el.scrollTop = y;
      el.scrollLeft = sameWidth ? position.x : 0;
      this.scrolled.add(el);
      this.kept.set(path, {
        el,
        x: Math.max(0, Math.round(el.scrollLeft)),
        y: Math.max(0, Math.round(el.scrollTop)),
        saved: position,
      });
      this.pendingScroll.delete(path);
    }
    for (const [name, value] of Array.from(this.pendingFields)) {
      const field = findField(this.root, name);
      if (!field) continue;
      this.pendingFields.delete(name);
      const typing =
        this.pendingLive &&
        field === document.activeElement &&
        Date.now() - (this.typedAt.get(field) ?? 0) < TYPING_MS;
      if (typing) continue; // the user here is typing in it: theirs is newer
      this.edited.add(field);
      if (field.value === value) continue;
      field.value = value;
      // Let the app react as to typing (filter a list, enable Save).
      field.dispatchEvent(new Event("input", { bubbles: true }));
    }
  }
}
