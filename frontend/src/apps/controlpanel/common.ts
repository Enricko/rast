// Shared helpers for the Control Panel tabs.

import type { AppContext } from "../../core/app";
import { badge, type Tone } from "../../core/components";
import { h } from "../../core/dom";
import { formatBytes, relativeTime } from "../../core/format";
import type { Role, User } from "../../core/types";
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

export type TabRenderer = (tab: TabRenderContext) => Promise<(() => void) | void>;

export function pageIntro(title: string, detail: string): HTMLElement {
  return h("div", { class: "cp-intro" }, h("h2", { class: "cp-intro-title" }, title), h("p", { class: "cp-intro-detail muted" }, detail));
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
