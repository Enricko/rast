// Shared loading / error / "not available yet" states and banners.
//
// Backend features are implemented in parallel; when an endpoint answers 501,
// 503 or 404 the UI must explain that the capability is not available yet
// instead of showing a generic failure or dummy data.

import { ApiError } from "../core/api";
import { button, emptyState, errorState, h, spinner } from "../core/dom";
import { iconSvg } from "../core/icons";
import { errorMessage } from "../core/ui";

/** Request failed because the backend capability is missing/disabled. */
export function isUnavailableError(error: unknown): error is ApiError {
  return (
    error instanceof ApiError &&
    (error.status === 404 || error.status === 501 || error.status === 503)
  );
}

export function unavailableReason(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.message) return error.message;
    if (error.status === 501) return "The server does not implement this capability yet.";
    if (error.status === 503) return "This subsystem is currently unavailable.";
    if (error.status === 404) return "The server endpoint was not found.";
  }
  return errorMessage(error);
}

export function loadingBlock(label = "Loading…"): HTMLElement {
  return spinner(label);
}

/**
 * Clear "not available yet" placeholder; the reason string comes from the
 * server error so operators see why (feature flag off, worker down, …).
 */
export function unavailableState(options: {
  title: string;
  reason: string;
  hint?: string;
  retry?: () => void;
}): HTMLElement {
  const box = h(
    "div",
    { class: "not-available", role: "status" },
    h("div", { class: "na-icon" }, iconSvg("warning", 26)),
    h("div", { class: "na-title" }, options.title),
    h("div", { class: "na-detail" }, options.reason),
    h(
      "div",
      { class: "na-hint" },
      options.hint ??
        "This capability is not available on this server yet. No placeholder data is shown.",
    ),
  );
  if (options.retry) {
    box.appendChild(button("Try again", () => options.retry?.(), { variant: "ghost" }));
  }
  return box;
}

/**
 * Render either the "not available" state or a normal error with retry,
 * replacing the target's children.
 */
export function renderLoadFailure(
  target: HTMLElement,
  error: unknown,
  options: { title: string; hint?: string; retry?: () => void },
): void {
  if (isUnavailableError(error)) {
    target.replaceChildren(
      unavailableState({
        title: options.title,
        reason: unavailableReason(error),
        hint: options.hint,
        retry: options.retry,
      }),
    );
    return;
  }
  target.replaceChildren(
    errorState(
      options.title,
      errorMessage(error),
      options.retry,
    ),
  );
}

export type BannerKind = "info" | "success" | "warning" | "error";

/** Inline status banner (docker status, monitor staleness, disabled features). */
export function banner(kind: BannerKind, title: string, detail?: string, extra?: Node): HTMLElement {
  const el = h(
    "div",
    { class: `banner banner-${kind}`, role: kind === "error" ? "alert" : "status" },
    h("div", { class: "banner-glyph" }, iconSvg(kind === "success" ? "check" : kind === "info" ? "info" : "warning", 18)),
    h(
      "div",
      { class: "banner-body" },
      h("div", { class: "banner-title" }, title),
      detail ? h("div", { class: "banner-detail" }, detail) : null,
    ),
  );
  if (extra) el.appendChild(extra);
  return el;
}

/** Small inline error line used by forms. */
export function formError(message: string): HTMLElement {
  return h("div", { class: "form-error", role: "alert" }, message);
}

export function emptyBlock(title: string, detail?: string, action?: Node): HTMLElement {
  return emptyState(title, detail, action);
}

export interface ProgressHandle {
  el: HTMLElement;
  update(fraction: number, label?: string): void;
  setError(failed: boolean): void;
}

/** Bounded progress bar helper (width capped to 0..1). */
export function progressBar(): ProgressHandle {
  const bar = h("div", { class: "progress-bar" });
  bar.style.width = "0%";
  const el = h("div", { class: "progress", role: "progressbar", ariaLabel: "Progress" }, bar);
  el.setAttribute("aria-valuemin", "0");
  el.setAttribute("aria-valuemax", "100");
  return {
    el,
    update(fraction: number, label?: string) {
      const clamped = Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0));
      bar.style.width = `${(clamped * 100).toFixed(1)}%`;
      el.setAttribute("aria-valuenow", String(Math.round(clamped * 100)));
      if (label !== undefined) el.title = label;
    },
    setError(failed: boolean) {
      bar.classList.toggle("is-error", failed);
    },
  };
}
