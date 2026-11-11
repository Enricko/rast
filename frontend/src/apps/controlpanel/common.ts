// Shared helpers for the Control Panel tabs.

import type { AppContext } from "../../core/app";
import { badge, type Tone } from "../../core/components";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { formatBytes, relativeTime } from "../../core/format";
import type {
  AccessRuleSource,
  AppRule,
  FolderLevel,
  FolderRule,
  Role,
  User,
} from "../../core/types";
import { errorMessage } from "../../core/ui";

export interface TabRenderContext {
  ctx: AppContext;
  /** Content host for this tab (already inside the scrolled area). */
  host: HTMLElement;
  /** Slot in the tab header for primary actions. */
  actions: HTMLElement;
  /** Deep-link target requested via ctx.params.section. */
  section?: string;
}

export type TabRenderer = (tab: TabRenderContext) => Promise<(() => void) | void> | (() => void) | void;

/**
 * Lead-in for a Control Panel page. Pass `null` as the title when the page has
 * no sub-heading of its own: the Control Panel header already shows the
 * section name, and repeating it as a second heading adds nothing.
 */
export function pageIntro(title: string | null, detail: string): HTMLElement {
  return h(
    "div",
    { class: "cp-intro" },
    ...(title ? [h("h2", { class: "cp-intro-title" }, title)] : []),
    h("p", { class: "cp-intro-detail muted" }, detail),
  );
}

export function sectionErrorState(message: string, retry: () => void): HTMLElement {
  return h(
    "div",
    { class: "error-state inline-state" },
    h("div", { class: "error-title" }, "Could not load this section"),
    h("div", { class: "error-detail" }, message),
    h("button", { class: "btn btn-sm", type: "button", onClick: retry }, "Retry"),
  );
}

export function notifyError(ctx: AppContext, error: unknown): void {
  ctx.toast.error(errorMessage(error));
}

export function roleBadge(role: Role): HTMLElement {
  return role === "admin" ? badge("Administrator", "info") : badge("User", "neutral");
}

export function activeBadge(active: boolean): HTMLElement {
  return active ? badge("Active", "ok") : badge("Disabled", "neutral");
}

export function statusTone(status: string): Tone {
  if (status === "ok" || status === "success" || status === "running" || status === "active") return "ok";
  if (status === "warning" || status === "partial" || status === "deploying") return "warn";
  if (status === "error" || status === "failed" || status === "failure") return "error";
  if (status === "info") return "info";
  return "neutral";
}

/** Compact "used / quota" bar used in the users table. */
export function quotaCell(user: User): HTMLElement {
  const unlimited = !user.quotaBytes || user.quotaBytes <= 0;
  const percent = unlimited ? 0 : Math.min(100, Math.round((user.usedBytes / user.quotaBytes) * 100));
  const tone = percent >= 95 ? " is-danger" : percent >= 85 ? " is-warn" : "";
  return h(
    "div",
    { class: "quota-cell" },
    h(
      "div",
      { class: "quota-cell-text small" },
      h("span", { class: "mono" }, formatBytes(user.usedBytes)),
      h("span", { class: "faint" }, unlimited ? " / unlimited" : ` / ${formatBytes(user.quotaBytes)}`),
    ),
    h(
      "div",
      { class: "progress progress-thin", role: "progressbar", ariaLabel: "Quota usage", ariaValueNow: String(percent), ariaValueMin: "0", ariaValueMax: "100" },
      h("div", { class: `progress-bar${tone}`, style: `width:${unlimited ? 0 : Math.max(percent, 1.5)}%` }),
    ),
  );
}

export function lastLoginCell(user: User): HTMLElement {
  if (!user.lastLoginAt) return h("span", { class: "muted small" }, "Never");
  return h("span", { class: "small", title: new Date(user.lastLoginAt).toLocaleString() }, relativeTime(user.lastLoginAt));
}

/** Human-readable short description of a browser user agent string. */
export function describeUserAgent(raw: string): string {
  const ua = raw || "";
  if (!ua.trim()) return "Unknown device";
  const browser = /Edg\//.test(ua)
    ? "Edge"
    : /OPR\/|Opera/.test(ua)
      ? "Opera"
      : /Firefox\//.test(ua)
        ? "Firefox"
        : /Chrome\//.test(ua)
          ? "Chrome"
          : /Safari\//.test(ua)
            ? "Safari"
            : /curl|wget|python|httpie/i.test(ua)
              ? "Command-line client"
              : "Browser";
  const os = /Windows NT/.test(ua)
    ? "Windows"
    : /Android/.test(ua)
      ? "Android"
      : /iPhone|iPad|iOS/.test(ua)
        ? "iOS"
        : /Mac OS X|Macintosh/.test(ua)
          ? "macOS"
          : /Linux/.test(ua)
            ? "Linux"
            : "";
  const detail = os ? `${browser} · ${os}` : browser;
  return detail.length > 80 ? `${detail.slice(0, 77)}…` : detail;
}

export function sessionIcon(ua: string): string {
  if (/Android|iPhone|iPad|Mobile/i.test(ua)) return "monitor";
  return "monitor";
}

/** Show a small "saved" confirmation next to a control. */
export function flashSaved(target: HTMLElement, message = "Saved"): void {
  target.textContent = message;
  target.classList.add("saved-flash");
  window.setTimeout(() => {
    target.textContent = "";
    target.classList.remove("saved-flash");
  }, 2400);
}

// ---------------------------------------------------------------------------
// Realtime helpers for tabs that rebuild their content on events.
// ---------------------------------------------------------------------------

const TAB_REALTIME = new WeakMap<HTMLElement, () => void>();

/**
 * Attach live-update subscriptions to a tab. Re-attaching (e.g. after the tab
 * rebuilds its content) replaces the previous subscriptions so nothing leaks.
 */
export function attachTabRealtime(host: HTMLElement, setup: () => () => void): void {
  const previous = TAB_REALTIME.get(host);
  previous?.();
  TAB_REALTIME.set(host, setup());
}

export function disposeTabRealtime(host: HTMLElement): void {
  const cleanup = TAB_REALTIME.get(host);
  if (!cleanup) return;
  cleanup();
  TAB_REALTIME.delete(host);
}

export function scrollToSection(host: HTMLElement, section: string | undefined): void {
  if (!section) return;
  window.setTimeout(() => {
    const el = host.querySelector<HTMLElement>(`#${CSS.escape(section)}`);
    if (!el) return;
    el.scrollIntoView({ behavior: "smooth", block: "center" });
    el.classList.add("is-highlighted");
    window.setTimeout(() => el.classList.remove("is-highlighted"), 2600);
  }, 120);
}

// ---------------------------------------------------------------------------
// Generic save workflow (same states/labels as the appearance editor)
// ---------------------------------------------------------------------------

export type SaveState = "clean" | "dirty" | "saving" | "saved" | "invalid" | "error";

export interface SaveWorkflowOptions<T> {
  save: () => Promise<T>;
  onSaved?: (value: T) => void;
  onStateChange?: (state: SaveState) => void;
  labels?: Partial<{ clean: string; dirty: string; saving: string; saved: string; retry: string }>;
}

export interface SaveWorkflow {
  el: HTMLElement;
  saveButton: HTMLButtonElement;
  status: HTMLElement;
  error: HTMLElement;
  readonly state: SaveState;
  isDirty(): boolean;
  setDirty(dirty: boolean): void;
  /** Call when the user edits the draft after a failed save. */
  markEdited(): void;
  setValidationError(message: string | null): void;
  save(): Promise<void>;
  reset(): void;
  dispose(): void;
}

const SAVE_LABELS = {
  clean: "All changes saved",
  dirty: "Save changes",
  saving: "Saving…",
  saved: "Saved",
  retry: "Try again",
};

/**
 * Small reusable dirty/saving/saved/error state machine for editors that have a
 * single Save action. Renders the same `appearance-save` bar used by the
 * appearance editor, so the visual language stays identical.
 */
export function createSaveWorkflow<T>(options: SaveWorkflowOptions<T>): SaveWorkflow {
  const labels = { ...SAVE_LABELS, ...(options.labels ?? {}) };
  let state: SaveState = "clean";
  let dirty = false;
  let validation: string | null = null;
  let savedTimer: number | undefined;
  let disposed = false;
  let inFlight = false;
  /** Last save failure; kept visible until the draft changes or a retry. */
  let failure: string | null = null;

  const error = h("div", { class: "banner banner-error", hidden: true, role: "alert" });
  const status = h("div", { class: "form-status" });
  const saveButton = h("button", { class: "btn btn-primary", type: "button", disabled: true });
  const leading = h("div", { class: "appearance-actions-leading" });
  const actions = h("div", { class: "appearance-actions" }, leading, h("div", { class: "spacer" }), status, saveButton);
  const el = h("div", { class: "appearance-save save-workflow" }, error, actions);

  function setContent(label: string, icon?: string, loading = false): void {
    saveButton.classList.toggle("is-loading", loading);
    saveButton.setAttribute("aria-busy", String(loading));
    const nodes: Node[] = [];
    if (icon && !loading) nodes.push(iconSvg(icon, 15));
    nodes.push(document.createTextNode(label));
    saveButton.replaceChildren(...nodes);
  }

  function render(): void {
    if (disposed) return;
    if (savedTimer !== undefined) {
      window.clearTimeout(savedTimer);
      savedTimer = undefined;
    }
    error.hidden = true;
    error.textContent = "";
    status.className = "form-status";
    switch (state) {
      case "clean":
        saveButton.disabled = true;
        setContent(labels.dirty);
        status.textContent = labels.clean;
        status.classList.add("is-saved");
        break;
      case "dirty":
        saveButton.disabled = false;
        setContent(labels.dirty, "check");
        status.textContent = "Unsaved changes";
        status.classList.add("is-dirty");
        break;
      case "saving":
        saveButton.disabled = true;
        setContent(labels.saving, undefined, true);
        status.textContent = labels.saving;
        status.classList.add("is-saving");
        break;
      case "saved":
        saveButton.disabled = true;
        setContent(labels.saved, "check");
        status.textContent = labels.saved;
        status.classList.add("is-saved");
        break;
      case "invalid":
        saveButton.disabled = true;
        setContent(labels.dirty, "check");
        status.textContent = validation ?? "Fix the highlighted fields";
        status.classList.add("is-error");
        if (validation) {
          error.hidden = false;
          error.textContent = validation;
        }
        break;
      case "error":
        saveButton.disabled = false;
        setContent(labels.retry, "refresh");
        status.textContent = "Could not save";
        status.classList.add("is-error");
        if (failure) {
          error.hidden = false;
          error.textContent = failure;
        }
        break;
    }
    options.onStateChange?.(state);
  }

  function reset(): void {
    if (disposed) return;
    state = "clean";
    dirty = false;
    validation = null;
    failure = null;
    render();
  }

  function setDirty(next: boolean): void {
    if (disposed || inFlight) return;
    if (!next) {
      if (state === "clean") return;
      reset();
      return;
    }
    if (state === "dirty" && dirty) return; // no-op changes must not re-render
    // A failed save stays visible (with Retry) while the draft is unchanged;
    // editors call setDirty(true) on every state change, which must not hide
    // the server's error message.
    if (state === "error" && dirty) return;
    dirty = true;
    validation = null;
    failure = null;
    state = "dirty";
    render();
  }

  /** The draft was edited after a failed save: clear the failure message. */
  function markEdited(): void {
    if (disposed || inFlight) return;
    if (state === "error") {
      failure = null;
      state = "dirty";
      render();
    }
  }

  function setValidationError(message: string | null): void {
    if (disposed || inFlight) return;
    if (message) {
      if (state === "invalid" && validation === message) return;
      dirty = true;
      validation = message;
      state = "invalid";
      render();
      return;
    }
    if (state !== "invalid") return;
    validation = null;
    state = dirty ? "dirty" : "clean";
    render();
  }

  async function runSave(): Promise<void> {
    if (disposed || inFlight || !dirty) {
      render();
      return;
    }
    inFlight = true;
    failure = null;
    state = "saving";
    render();
    try {
      const value = await options.save();
      if (disposed) return;
      inFlight = false;
      dirty = false;
      validation = null;
      state = "saved";
      render();
      options.onSaved?.(value);
      savedTimer = window.setTimeout(() => {
        savedTimer = undefined;
        if (disposed || state !== "saved") return;
        reset();
      }, 1800);
    } catch (saveError) {
      if (disposed) return;
      inFlight = false;
      dirty = true;
      failure = errorMessage(saveError);
      state = "error";
      render();
    }
  }

  saveButton.addEventListener("click", () => void runSave());
  render();

  return {
    el,
    saveButton,
    status,
    error,
    get state() {
      return state;
    },
    isDirty: () => dirty,
    setDirty,
    markEdited,
    setValidationError,
    save: runSave,
    reset,
    dispose: () => {
      disposed = true;
      if (savedTimer !== undefined) window.clearTimeout(savedTimer);
    },
  };
}

// ---------------------------------------------------------------------------
// Access policy helpers (docs/GROUPS.md)
// ---------------------------------------------------------------------------

export const FOLDER_RULE_OPTIONS: Array<{ value: FolderRule; label: string }> = [
  { value: "inherit", label: "Inherit" },
  { value: "none", label: "No Access" },
  { value: "read", label: "Read Only" },
  { value: "write", label: "Read & Write" },
];

export const APP_RULE_OPTIONS: Array<{ value: AppRule; label: string }> = [
  { value: "inherit", label: "Inherit" },
  { value: "none", label: "No Access" },
  { value: "allow", label: "Allow" },
];

/** Fixed executable app catalogue used by group/user rule editors. */
export const RULE_APPS: Array<{ id: string; label: string }> = [
  { id: "files", label: "File Station" },
  { id: "applications", label: "Applications" },
  { id: "monitor", label: "Resource Monitor" },
  { id: "backup", label: "Backup Center" },
  { id: "packages", label: "Package Center" },
  { id: "logs", label: "Activity Logs" },
  { id: "controlpanel", label: "Control Panel" },
];

export function appLabel(appId: string): string {
  return RULE_APPS.find((app) => app.id === appId)?.label ?? appId;
}

export function folderLevelLabel(level: FolderLevel): string {
  switch (level) {
    case "admin":
      return "Admin";
    case "write":
      return "Read & Write";
    case "read":
      return "Read Only";
    default:
      return "No Access";
  }
}

export function folderLevelTone(level: FolderLevel): Tone {
  switch (level) {
    case "admin":
      return "info";
    case "write":
      return "ok";
    case "read":
      return "warn";
    default:
      return "error";
  }
}

/** Normalised provenance for an access source (tolerates `kind` vs `type`). */
export function sourceLabel(source: AccessRuleSource): string {
  const type = source.type ?? source.kind ?? "direct";
  if (type === "direct") return "Direct";
  return source.groupName ? `Group “${source.groupName}”` : "Group";
}

/**
 * Explain an explicit-deny conflict: an explicit `none` rule always wins over
 * grants, regardless of whether it comes from the user or a group.
 */
export function denyConflict(sources: AccessRuleSource[]): string | null {
  const deny = sources.filter((source) => source.rule === "none");
  const grant = sources.filter((source) => source.rule === "read" || source.rule === "write" || source.rule === "allow");
  if (deny.length === 0 || grant.length === 0) return null;
  const deniedBy = deny.map(sourceLabel).join(", ");
  return `No Access from ${deniedBy} overrides the grant${grant.length > 1 ? "s" : ""} — explicit No Access always wins.`;
}
