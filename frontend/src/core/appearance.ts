// Per-user appearance (wallpaper + theme), persisted on the server.
//
// The server (SQLite) is the source of truth. localStorage is only a small paint
// cache for the same user so the desktop appears instantly on reload; it is
// keyed by user id and removed on sign-out so wallpapers never leak between
// accounts on a shared browser.

import type { ApiClient } from "./api";
import { ApiError } from "./api";
import { getRealtime } from "./realtime";
import type { EventStream } from "./sse";

export type ThemeSetting = "light" | "dark" | "system";
export type AccentId =
  | "ocean"
  | "forest"
  | "teal"
  | "sage"
  | "amber"
  | "terracotta"
  | "rose"
  | "plum"
  | "indigo"
  | "graphite";
export type ResolvedTheme = "light" | "dark";
export type WallpaperKind = "builtin" | "upload" | "color";
export type WallpaperFit = "cover" | "contain" | "center";
export type WallpaperPosition =
  | "center"
  | "top"
  | "bottom"
  | "left"
  | "right"
  | "top-left"
  | "top-right"
  | "bottom-left"
  | "bottom-right";

export interface WallpaperSettings {
  kind: WallpaperKind;
  builtinId: string;
  uploadId: string | null;
  color: string;
  fit: WallpaperFit;
  position: WallpaperPosition;
  /** Darkening overlay percentage, 0-60. */
  overlay: number;
}

export interface AppearanceUpload {
  id: string;
  width: number;
  height: number;
  sizeBytes: number;
  mime: string;
  createdAt: number;
  url: string;
}

/** Desktop widget ids (stable API values). */
export type WidgetId = "health" | "cpu-memory" | "storage" | "network";
export const WIDGET_IDS: ReadonlyArray<WidgetId> = ["health", "cpu-memory", "storage", "network"];

/**
 * An enabled desktop widget. `x`/`y` are fractions (0..1) of the free desktop
 * area (desktop size minus widget size), so the widget stays fully visible at
 * any browser size. Array order is stacking order (last on top).
 */
export interface DesktopWidget {
  id: WidgetId;
  /** Legacy fractional placement (kept for older servers/layouts). */
  x: number;
  y: number;
  /** Legacy fractional size (kept for older servers/layouts). */
  w?: number;
  h?: number;
  /** Schema version 2: fixed CSS-pixel grid placements (viewport independent). */
  gridVersion?: 2;
  pxX?: number;
  pxY?: number;
  pxW?: number;
  pxH?: number;
}

/** An app placed on the desktop and its grid cell (column, row from the top left). */
export interface DesktopIcon {
  id: string;
  /** Legacy coarse cell, kept for older servers/clients (approximate for v2). */
  col: number;
  row: number;
  /** Version 2: fine-grid position in CSS pixels relative to the icon area. */
  gridVersion?: 2;
  pxX?: number;
  pxY?: number;
}

/** Grid limit for desktop icons (matches the server). */
export const MAX_DESKTOP_CELL = 63;

export interface Appearance {
  theme: ThemeSetting;
  accent: AccentId;
  wallpaper: WallpaperSettings;
  widgets: DesktopWidget[];
  /**
   * False until the account saves its own widget list: the desktop then shows
   * {@link DEFAULT_WIDGETS}-style defaults (see shell/widgets.ts). Missing in
   * older responses or caches, where it reads as true (no defaults).
   */
  widgetsCustomized: boolean;
  /** Pinned taskbar application ids, in order (the account's own choice). */
  taskbarPins: string[];
  /**
   * The apps on the desktop and their cells; null until the account arranges
   * its desktop (every app it may open, in the default order).
   */
  desktopIcons: DesktopIcon[] | null;
  uploads: AppearanceUpload[];
  limits?: { maxBytes: number; maxUploads: number };
}

/** Taskbar pins of an account that never changed them (server default). */
export const DEFAULT_TASKBAR_PINS: ReadonlyArray<string> = ["files", "monitor", "controlpanel"];

/** Payload of the `appearance.changed` realtime event. */
export interface AppearanceChangedEvent {
  reason: string;
  originSessionId?: string | null;
  appearance?: Appearance | null;
}

/** Payload of the SSE `hello` event. */
export interface RealtimeHello {
  seq?: number;
  ts?: number;
  userId?: number;
  admin?: boolean;
  sessionId?: string;
  version?: string;
}

let previewDepth = 0;

/**
 * While an appearance editor previews a draft, other appearance updates (own
 * upload response, external sessions) must not overwrite the preview.
 */
export function beginAppearancePreview(): void {
  previewDepth += 1;
}

export function endAppearancePreview(): void {
  previewDepth = Math.max(0, previewDepth - 1);
}

export function isAppearancePreviewActive(): boolean {
  return previewDepth > 0;
}

export interface BuiltinWallpaper {
  id: string;
  label: string;
  detail: string;
  file: string;
}

/** Bundled nature wallpapers (see docs/WALLPAPERS.md for licence). */
export const BUILTIN_WALLPAPERS: ReadonlyArray<BuiltinWallpaper> = [
  {
    id: "alpine-dawn",
    label: "Alpine Dawn",
    detail: "Calm lake below snow-capped peaks at sunrise",
    file: "/wallpapers/alpine-dawn.jpg",
  },
  {
    id: "forest-mist",
    label: "Forest Mist",
    detail: "Pine forest with soft morning light rays",
    file: "/wallpapers/forest-mist.jpg",
  },
  {
    id: "coastal-calm",
    label: "Coastal Calm",
    detail: "Turquoise water, smooth rocks and a pale sky",
    file: "/wallpapers/coastal-calm.jpg",
  },
  {
    id: "desert-dusk",
    label: "Desert Dusk",
    detail: "Warm dunes under a violet evening sky",
    file: "/wallpapers/desert-dusk.jpg",
  },
];

export interface AccentPreset {
  id: AccentId;
  label: string;
  /** Accent used in light mode. */
  light: string;
  /** Accent used in dark mode (lighter for contrast). */
  dark: string;
}

/** Exactly ten accent presets; ids are stable API values. */
export const ACCENT_PRESETS: ReadonlyArray<AccentPreset> = [
  { id: "ocean", label: "Ocean Blue", light: "#2563EB", dark: "#7CB3F4" },
  { id: "forest", label: "Forest Green", light: "#15803D", dark: "#79BC8B" },
  { id: "teal", label: "Teal", light: "#0F766E", dark: "#6CCABD" },
  { id: "sage", label: "Sage", light: "#4D7C0F", dark: "#BECCA5" },
  { id: "amber", label: "Amber", light: "#B45309", dark: "#E9B560" },
  { id: "terracotta", label: "Terracotta", light: "#B84D34", dark: "#E99A80" },
  { id: "rose", label: "Rose", light: "#BE3455", dark: "#EE93A9" },
  { id: "plum", label: "Plum", light: "#7E3F98", dark: "#CCA1DB" },
  { id: "indigo", label: "Indigo", light: "#4F46B8", dark: "#ABA7EF" },
  { id: "graphite", label: "Graphite", light: "#475569", dark: "#BBC7D4" },
];

export const DEFAULT_ACCENT: AccentId = "ocean";

export function accentPreset(id: AccentId): AccentPreset {
  return ACCENT_PRESETS.find((preset) => preset.id === id) ?? ACCENT_PRESETS[0]!;
}

export function isAccentId(value: string): value is AccentId {
  return ACCENT_PRESETS.some((preset) => preset.id === value);
}

export const DEFAULT_APPEARANCE: Appearance = {
  theme: "light",
  accent: DEFAULT_ACCENT,
  wallpaper: {
    kind: "builtin",
    builtinId: "alpine-dawn",
    uploadId: null,
    color: "#1B2430",
    fit: "cover",
    position: "center",
    overlay: 20,
  },
  widgets: [],
  widgetsCustomized: true,
  taskbarPins: [...DEFAULT_TASKBAR_PINS],
  desktopIcons: null,
  uploads: [],
  limits: { maxBytes: 10 * 1024 * 1024, maxUploads: 20 },
};

export const WALLPAPER_POSITIONS: ReadonlyArray<{ id: WallpaperPosition; label: string }> = [
  { id: "center", label: "Center" },
  { id: "top", label: "Top" },
  { id: "bottom", label: "Bottom" },
  { id: "left", label: "Left" },
  { id: "right", label: "Right" },
  { id: "top-left", label: "Top left" },
  { id: "top-right", label: "Top right" },
  { id: "bottom-left", label: "Bottom left" },
  { id: "bottom-right", label: "Bottom right" },
];

export function builtinWallpaper(id: string): BuiltinWallpaper | undefined {
  return BUILTIN_WALLPAPERS.find((wallpaper) => wallpaper.id === id);
}

export function builtinUrl(id: string): string {
  return builtinWallpaper(id)?.file ?? BUILTIN_WALLPAPERS[0]?.file ?? "/wallpapers/alpine-dawn.jpg";
}

/** CSS `background-position` for a logical position. */
export function positionCss(position: WallpaperPosition): string {
  const left = position.includes("left");
  const right = position.includes("right");
  const top = position.includes("top");
  const bottom = position.includes("bottom");
  if (left || right) {
    return `${right ? "right" : "left"}${top ? " top" : bottom ? " bottom" : ""}`;
  }
  if (top) return "top";
  if (bottom) return "bottom";
  return "center";
}

/** CSS `background-size` for a fit mode (center keeps natural size). */
export function fitCss(fit: WallpaperFit): string {
  if (fit === "cover") return "cover";
  if (fit === "contain") return "contain";
  return "auto";
}

export function resolveTheme(theme: ThemeSetting): ResolvedTheme {
  if (theme === "dark" || theme === "light") return theme;
  try {
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  } catch {
    return "light";
  }
}

const CACHE_PREFIX = "rast.appearance.v1.";

function cacheKey(userId: number): string {
  return `${CACHE_PREFIX}${userId}`;
}

export function readCachedAppearance(userId: number): Appearance | null {
  try {
    const raw = window.localStorage.getItem(cacheKey(userId));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Appearance;
    if (!parsed || typeof parsed !== "object" || !parsed.wallpaper) return null;
    return normalizeAppearance(parsed);
  } catch {
    return null;
  }
}

export function writeCachedAppearance(userId: number, appearance: Appearance): void {
  try {
    window.localStorage.setItem(cacheKey(userId), JSON.stringify(appearance));
  } catch {
    /* storage disabled or full: the server copy still applies */
  }
}

/** Remove this user's cache (also used on sign-out so accounts never share it). */
export function clearCachedAppearance(userId?: number | null): void {
  try {
    if (typeof userId === "number") {
      window.localStorage.removeItem(cacheKey(userId));
      return;
    }
    for (const key of Object.keys(window.localStorage)) {
      if (key.startsWith(CACHE_PREFIX)) window.localStorage.removeItem(key);
    }
  } catch {
    /* ignore */
  }
}

function clampFraction(value: unknown): number {
  const number = Number(value);
  if (!Number.isFinite(number)) return 0;
  return Math.round(Math.max(0, Math.min(1, number)) * 10_000) / 10_000;
}

/** Known, unique widgets (at most one of each) with clamped placement. */
export function normalizeWidgets(input: unknown): DesktopWidget[] {
  if (!Array.isArray(input)) return [];
  const out: DesktopWidget[] = [];
  for (const item of input) {
    const id = (item as { id?: unknown } | null)?.id;
    if (typeof id !== "string" || !WIDGET_IDS.includes(id as WidgetId)) continue;
    if (out.some((widget) => widget.id === id)) continue;
    const widget: DesktopWidget = {
      id: id as WidgetId,
      x: clampFraction((item as DesktopWidget).x),
      y: clampFraction((item as DesktopWidget).y),
    };
    const w = Number((item as DesktopWidget).w);
    const h = Number((item as DesktopWidget).h);
    if (Number.isFinite(w) && w > 0) widget.w = Math.round(Math.max(0.05, Math.min(1, w)) * 10_000) / 10_000;
    if (Number.isFinite(h) && h > 0) widget.h = Math.round(Math.max(0.05, Math.min(1, h)) * 10_000) / 10_000;
    const gridVersion = Number((item as DesktopWidget).gridVersion);
    const pxX = Number((item as DesktopWidget).pxX);
    const pxY = Number((item as DesktopWidget).pxY);
    const pxW = Number((item as DesktopWidget).pxW);
    const pxH = Number((item as DesktopWidget).pxH);
    const px = (value: number, min: number, max: number): number | undefined =>
      Number.isFinite(value) ? Math.round(Math.max(min, Math.min(max, value))) : undefined;
    const keptX = px(pxX, 0, 100_000);
    const keptY = px(pxY, 0, 100_000);
    const keptW = px(pxW, 32, 10_000);
    const keptH = px(pxH, 32, 10_000);
    if (gridVersion === 2 && keptX !== undefined && keptY !== undefined && keptW !== undefined && keptH !== undefined) {
      widget.gridVersion = 2;
      widget.pxX = keptX;
      widget.pxY = keptY;
      widget.pxW = keptW;
      widget.pxH = keptH;
    }
    out.push(widget);
  }
  return out.slice(0, WIDGET_IDS.length);
}

/** A built-in app id, or an installed Linux app (`linux:<id>`). */
const SHORTCUT_ID = /^(?:[a-z]+|linux:[a-z0-9][a-z0-9-]{0,47})$/;

/** Unique application ids in order; defaults when the field is missing. */
export function normalizeTaskbarPins(input: unknown): string[] {
  if (!Array.isArray(input)) return [...DEFAULT_TASKBAR_PINS];
  const out: string[] = [];
  for (const item of input) {
    if (typeof item !== "string" || !SHORTCUT_ID.test(item) || out.includes(item)) continue;
    out.push(item);
  }
  return out;
}

/** Valid, unique icons (ids and cells); null keeps the default layout. */
export function normalizeDesktopIcons(input: unknown): DesktopIcon[] | null {
  if (!Array.isArray(input)) return null;
  const out: DesktopIcon[] = [];
  for (const item of input) {
    if (!item || typeof item !== "object") continue;
    const { id, col, row } = item as DesktopIcon;
    if (typeof id !== "string" || !SHORTCUT_ID.test(id)) continue;
    if (!Number.isInteger(col) || !Number.isInteger(row) || col < 0 || row < 0 || col > MAX_DESKTOP_CELL || row > MAX_DESKTOP_CELL) continue;
    const gridVersion = Number((item as DesktopIcon).gridVersion);
    const pxX = Number((item as DesktopIcon).pxX);
    const pxY = Number((item as DesktopIcon).pxY);
    const hasPx =
      gridVersion === 2 &&
      Number.isFinite(pxX) &&
      Number.isFinite(pxY) &&
      pxX >= 0 &&
      pxY >= 0 &&
      pxX <= 100_000 &&
      pxY <= 100_000;
    if (out.some((icon) => icon.id === id || (!hasPx && icon.gridVersion !== 2 && icon.col === col && icon.row === row))) continue;
    const icon: DesktopIcon = { id, col, row };
    if (hasPx) {
      icon.gridVersion = 2;
      icon.pxX = Math.round(pxX);
      icon.pxY = Math.round(pxY);
    }
    out.push(icon);
  }
  return out;
}

/** Fill in missing fields defensively (older cache entries, partial responses). */
export function normalizeAppearance(input: Partial<Appearance> | null | undefined): Appearance {
  const wallpaper = { ...DEFAULT_APPEARANCE.wallpaper, ...(input?.wallpaper ?? {}) };
  const overlay = Math.max(0, Math.min(60, Math.round(Number(wallpaper.overlay) || 0)));
  return {
    theme: (input?.theme as ThemeSetting) ?? DEFAULT_APPEARANCE.theme,
    accent: isAccentId(String(input?.accent ?? "")) ? (input!.accent as AccentId) : DEFAULT_ACCENT,
    wallpaper: {
      kind: (wallpaper.kind as WallpaperKind) ?? "builtin",
      builtinId: wallpaper.builtinId || DEFAULT_APPEARANCE.wallpaper.builtinId,
      uploadId: wallpaper.uploadId ?? null,
      color: /^#[0-9a-fA-F]{6}$/.test(wallpaper.color) ? wallpaper.color : DEFAULT_APPEARANCE.wallpaper.color,
      fit: (wallpaper.fit as WallpaperFit) ?? "cover",
      position: (wallpaper.position as WallpaperPosition) ?? "center",
      overlay,
    },
    widgets: normalizeWidgets(input?.widgets),
    widgetsCustomized: input?.widgetsCustomized !== false,
    taskbarPins: normalizeTaskbarPins(input?.taskbarPins),
    desktopIcons: normalizeDesktopIcons(input?.desktopIcons),
    uploads: Array.isArray(input?.uploads) ? input!.uploads! : [],
    limits: input?.limits ?? DEFAULT_APPEARANCE.limits,
  };
}

/** URL used as the CSS background for an upload; requires the session cookie. */
export function uploadUrl(uploadId: string): string {
  return `/api/appearance/wallpapers/${encodeURIComponent(uploadId)}`;
}

/**
 * Apply appearance to the document. `preview` renders without touching the
 * cache or the theme chrome (used by the Personalize dialog while dragging
 * sliders / trying wallpapers).
 */
export function applyAppearance(appearance: Appearance, options: { preview?: boolean } = {}): void {
  const root = document.documentElement;
  const wallpaper = appearance.wallpaper;
  const url =
    wallpaper.kind === "upload" && wallpaper.uploadId
      ? uploadUrl(wallpaper.uploadId)
      : wallpaper.kind === "builtin"
        ? builtinUrl(wallpaper.builtinId)
        : "";

  if (url) {
    root.style.setProperty("--wallpaper-image", `url("${url}")`);
  } else {
    root.style.removeProperty("--wallpaper-image");
  }
  root.style.setProperty("--wallpaper-color", wallpaper.color || DEFAULT_APPEARANCE.wallpaper.color);
  root.style.setProperty("--wallpaper-size", fitCss(wallpaper.fit));
  root.style.setProperty("--wallpaper-position", positionCss(wallpaper.position));
  root.style.setProperty("--wallpaper-overlay", String(wallpaper.overlay / 100));
  root.dataset.accent = appearance.accent;
  root.dataset.wallpaperKind = wallpaper.kind;
  if (wallpaper.kind !== "color" && !url) {
    root.dataset.wallpaperKind = "color";
  }
  if (!options.preview) {
    root.dataset.wallpaperApplied = "true";
    // The boot splash (index.html + /boot-wallpaper.js) paints this on the
    // next refresh before the app bundle knows who is signed in.
    try {
      if (!url || wallpaper.kind === "color") {
        window.localStorage.removeItem("rast.wallpaper.splash.v1");
      } else {
        window.localStorage.setItem(
          "rast.wallpaper.splash.v1",
          JSON.stringify({
            url,
            color: wallpaper.color,
            overlay: wallpaper.overlay,
            fit: wallpaper.fit,
            position: positionCss(wallpaper.position),
          }),
        );
      }
    } catch {
      /* storage unavailable: the splash keeps its built-in background */
    }
  }
}

/** Revert document CSS variables to a previously captured state. */
export function captureWallpaper(): Partial<CSSStyleDeclaration> | null {
  const root = document.documentElement;
  const props = ["--wallpaper-image", "--wallpaper-color", "--wallpaper-size", "--wallpaper-position", "--wallpaper-overlay"];
  const snapshot: Record<string, string> = {};
  for (const prop of props) {
    const value = root.style.getPropertyValue(prop);
    if (value) snapshot[prop] = value;
  }
  return snapshot as unknown as Partial<CSSStyleDeclaration>;
}

export function restoreWallpaper(snapshot: Partial<CSSStyleDeclaration> | null): void {
  const root = document.documentElement;
  const props = ["--wallpaper-image", "--wallpaper-color", "--wallpaper-size", "--wallpaper-position", "--wallpaper-overlay"];
  for (const prop of props) {
    const value = snapshot ? (snapshot as Record<string, string>)[prop] : "";
    if (value) root.style.setProperty(prop, value);
    else root.style.removeProperty(prop);
  }
}

/**
 * Thrown when an appearance request finishes after the signed-in account
 * changed (sign-out, account switch). Callers ignore it: the response belongs
 * to the previous account and must never reach the new one.
 */
export class StaleAppearanceError extends Error {
  constructor() {
    super("appearance request belongs to a previous account");
    this.name = "StaleAppearanceError";
  }
}

export function isStaleAppearanceError(error: unknown): boolean {
  return error instanceof StaleAppearanceError;
}

/**
 * Where `AppearanceService.value` came from:
 *   idle    -> no account loaded yet (defaults, paint only)
 *   loading -> first fetch for this account in progress, nothing cached
 *   cached  -> this account's paint cache, server not confirmed yet
 *   ready   -> confirmed by the server
 *   error   -> the server could not be reached and nothing is cached:
 *              the value is the default look and must never seed an editor
 *              or a save.
 */
export type AppearanceStatus = "idle" | "loading" | "cached" | "ready" | "error";

/** Fields of an appearance update; omitted fields keep their saved value. */
export interface AppearancePatch {
  theme?: ThemeSetting;
  accent?: AccentId;
  wallpaper?: Partial<WallpaperSettings>;
  /** The complete widget list (replaces the saved one). */
  widgets?: DesktopWidget[];
  /** The complete ordered pin list (replaces the saved one). */
  taskbarPins?: string[];
  /** The complete desktop layout (replaces the saved one); null restores the default. */
  desktopIcons?: DesktopIcon[] | null;
}

/**
 * The editor settings (theme, accent, wallpaper) of `draft` that differ from
 * `baseline`, as an update payload. Widgets are arranged on the desktop, not
 * in the editors, so they are never part of an editor's changes.
 * Saving only what the user changed means a stale baseline (cached copy,
 * another device saved meanwhile) can never overwrite untouched settings.
 */
export function appearancePatch(baseline: Appearance, draft: Appearance): AppearancePatch {
  const patch: AppearancePatch = {};
  if (draft.theme !== baseline.theme) patch.theme = draft.theme;
  if (draft.accent !== baseline.accent) patch.accent = draft.accent;
  const wallpaper: Partial<WallpaperSettings> = {};
  const keys: Array<keyof WallpaperSettings> = ["kind", "builtinId", "uploadId", "color", "fit", "position", "overlay"];
  for (const key of keys) {
    if (draft.wallpaper[key] !== baseline.wallpaper[key]) {
      (wallpaper as Record<string, unknown>)[key] = draft.wallpaper[key];
    }
  }
  // An uploaded wallpaper is only valid together with its id.
  if (wallpaper.kind === "upload" && wallpaper.uploadId === undefined) wallpaper.uploadId = draft.wallpaper.uploadId;
  if (Object.keys(wallpaper).length > 0) patch.wallpaper = wallpaper;
  return patch;
}

function isEmptyPatch(patch: AppearancePatch): boolean {
  return (
    patch.theme === undefined &&
    patch.accent === undefined &&
    patch.wallpaper === undefined &&
    patch.widgets === undefined &&
    patch.taskbarPins === undefined &&
    patch.desktopIcons === undefined
  );
}

/** True when `draft` changes a saved setting (uploads and limits are not settings). */
export function hasAppearanceChanges(baseline: Appearance, draft: Appearance): boolean {
  return !isEmptyPatch(appearancePatch(normalizeAppearance(baseline), normalizeAppearance(draft)));
}

/** Fields arranged on the desktop itself and saved with coalescing. */
type DesktopField = "widgets" | "taskbarPins" | "desktopIcons";

interface DesktopQueue<T> {
  wanted: T | null;
  latest: T | null;
  saving: Promise<boolean> | null;
}

function emptyQueue<T>(): DesktopQueue<T> {
  return { wanted: null, latest: null, saving: null };
}

/**
 * API client for /api/appearance, scoped to the signed-in account.
 *
 * Every asynchronous operation remembers the account generation it started
 * in; a response that arrives after sign-out or an account switch is dropped
 * (never cached, never emitted) and the call rejects with
 * {@link StaleAppearanceError}.
 */
export class AppearanceService {
  private current: Appearance = normalizeAppearance(DEFAULT_APPEARANCE);
  private userId: number | null = null;
  private generation = 0;
  private statusValue: AppearanceStatus = "idle";
  private lastErrorValue: unknown = null;
  private listeners = new Set<(appearance: Appearance) => void>();
  private themeWanted: ThemeSetting | null = null;
  private themeSaving: Promise<boolean> | null = null;
  /** Coalesced saves of desktop-arranged fields (not part of any editor). */
  private desktopQueues: { [K in DesktopField]: DesktopQueue<NonNullable<AppearancePatch[K]>> } = {
    widgets: emptyQueue(),
    taskbarPins: emptyQueue(),
    desktopIcons: emptyQueue(),
  };

  constructor(private readonly api: ApiClient) {}

  get value(): Appearance {
    return this.current;
  }

  get currentUserId(): number | null {
    return this.userId;
  }

  get status(): AppearanceStatus {
    return this.statusValue;
  }

  /** The last load error while the status is `error` or `cached`. */
  get lastError(): unknown {
    return this.lastErrorValue;
  }

  /**
   * True when `value` is this account's saved appearance (server-confirmed
   * or its own paint cache): only then may it seed an editor or the theme.
   */
  get isKnown(): boolean {
    return this.statusValue === "ready" || this.statusValue === "cached";
  }

  /** True while a quick theme save is queued or in flight. */
  get savingTheme(): boolean {
    return this.themeSaving !== null;
  }

  /** The widget layout being saved (newest wins), or null when idle. */
  get pendingWidgets(): DesktopWidget[] | null {
    const queue = this.desktopQueues.widgets;
    return queue.saving ? queue.latest : null;
  }

  /** The desktop layout being saved (newest wins), or null when idle. */
  get pendingDesktopIcons(): DesktopIcon[] | null {
    const queue = this.desktopQueues.desktopIcons;
    return queue.saving ? queue.latest : null;
  }

  /** The pin list being saved (newest wins), or null when idle. */
  get pendingTaskbarPins(): string[] | null {
    const queue = this.desktopQueues.taskbarPins;
    return queue.saving ? queue.latest : null;
  }

  subscribe(listener: (appearance: Appearance) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(): void {
    for (const listener of this.listeners) {
      try {
        listener(this.current);
      } catch {
        /* ignore listener errors */
      }
    }
  }

  private assertCurrent(generation: number): void {
    if (generation !== this.generation) throw new StaleAppearanceError();
  }

  /** Adopt a server snapshot for the account of `generation`. */
  private commit(generation: number, next: Appearance, options: { emit?: boolean } = {}): Appearance {
    this.assertCurrent(generation);
    this.current = next;
    this.statusValue = "ready";
    this.lastErrorValue = null;
    if (this.userId !== null) writeCachedAppearance(this.userId, next);
    if (options.emit !== false) this.emit();
    return next;
  }

  /** Switch to `userId` (dropping everything that belonged to another account). */
  private bind(userId: number): void {
    if (this.userId === userId) return;
    this.generation += 1;
    this.userId = userId;
    this.current = normalizeAppearance(DEFAULT_APPEARANCE);
    this.statusValue = "idle";
    this.lastErrorValue = null;
    this.themeWanted = null;
    this.themeSaving = null;
    this.desktopQueues = { widgets: emptyQueue(), taskbarPins: emptyQueue(), desktopIcons: emptyQueue() };
  }

  /**
   * Load the signed-in account's appearance: its paint cache first (if not
   * yet confirmed), then the server. Resolves with the best known value; a
   * failure only rejects when nothing is known for this account (`error`).
   */
  async load(userId: number): Promise<Appearance> {
    this.bind(userId);
    const generation = this.generation;
    if (this.statusValue !== "ready") {
      const cached = readCachedAppearance(userId);
      if (cached) {
        this.current = cached;
        this.statusValue = "cached";
        this.emit();
      } else if (this.statusValue !== "cached") {
        this.statusValue = "loading";
      }
    }
    try {
      const fresh = normalizeAppearance(await this.api.get<Appearance>(`/api/appearance?t=${Date.now()}`));
      return this.commit(generation, fresh);
    } catch (error) {
      this.assertCurrent(generation);
      if (isStaleAppearanceError(error)) throw error;
      this.lastErrorValue = error;
      if (this.isKnown) return this.current;
      this.statusValue = "error";
      this.emit();
      throw error;
    }
  }

  /**
   * Adopt an appearance that came from the server (another session, or a
   * refreshed snapshot) without making a network request.
   */
  adopt(input: Appearance): Appearance {
    return this.commit(this.generation, normalizeAppearance(input));
  }

  /** Re-read the signed-in account's appearance from the server. */
  async reload(): Promise<Appearance> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    const fresh = normalizeAppearance(await this.api.get<Appearance>(`/api/appearance?t=${Date.now()}`));
    return this.commit(generation, fresh);
  }

  /**
   * Save the given fields. Only what is in `patch` is sent: omitted fields
   * keep their saved value on the server, whatever this tab believes.
   */
  async save(patch: AppearancePatch, options: { emit?: boolean } = {}): Promise<Appearance> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    if (isEmptyPatch(patch)) {
      if (this.statusValue === "ready") return this.current;
      return this.reload();
    }
    const payload: Record<string, unknown> = {};
    if (patch.theme !== undefined) payload.theme = patch.theme;
    if (patch.accent !== undefined) payload.accent = patch.accent;
    if (patch.wallpaper && Object.keys(patch.wallpaper).length > 0) payload.wallpaper = { ...patch.wallpaper };
    if (patch.widgets !== undefined) payload.widgets = normalizeWidgets(patch.widgets);
    if (patch.taskbarPins !== undefined) payload.taskbarPins = normalizeTaskbarPins(patch.taskbarPins);
    if (patch.desktopIcons !== undefined) payload.desktopIcons = patch.desktopIcons === null ? null : normalizeDesktopIcons(patch.desktopIcons);
    const updated = normalizeAppearance(await this.api.put<Appearance>("/api/appearance", payload));
    return this.commit(generation, updated, options);
  }

  /** Upload a wallpaper file (raw body). Returns the created upload. */
  async upload(file: File): Promise<AppearanceUpload> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    const response = await fetch("/api/appearance/wallpapers", {
      method: "POST",
      headers: {
        "Content-Type": file.type,
        "X-CSRF-Token": this.api.getCsrfToken(),
      },
      credentials: "same-origin",
      body: file,
    });
    if (!response.ok) {
      let message = `Upload failed (${response.status})`;
      let code = "upload_failed";
      try {
        const payload = (await response.json()) as { error?: { code?: string; message?: string } };
        message = payload.error?.message ?? message;
        code = payload.error?.code ?? code;
      } catch {
        /* keep default message */
      }
      throw new ApiError(response.status, code, message);
    }
    const upload = (await response.json()) as AppearanceUpload;
    this.assertCurrent(generation);
    this.current = {
      ...this.current,
      uploads: [upload, ...this.current.uploads.filter((item) => item.id !== upload.id)],
    };
    if (this.statusValue === "ready" && this.userId !== null) writeCachedAppearance(this.userId, this.current);
    this.emit();
    return upload;
  }

  async removeUpload(uploadId: string): Promise<Appearance> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    const result = await this.api.del<{ ok: boolean; appearance?: Appearance }>(
      `/api/appearance/wallpapers/${encodeURIComponent(uploadId)}`,
    );
    if (result.appearance) return this.commit(generation, normalizeAppearance(result.appearance));
    this.assertCurrent(generation);
    return this.reload();
  }

  async reset(): Promise<Appearance> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    const updated = normalizeAppearance(await this.api.post<Appearance>("/api/appearance/reset"));
    return this.commit(generation, updated);
  }

  /** Persist only the accent (quick action); resolves `false` on failure. */
  async saveAccent(accent: AccentId): Promise<boolean> {
    if (this.statusValue === "ready" && this.current.accent === accent) return true;
    try {
      await this.save({ accent });
      return true;
    } catch (error) {
      if (isStaleAppearanceError(error)) return true;
      console.warn("could not persist accent preference", error);
      return false;
    }
  }

  /**
   * Persist only the theme (tray toggle, account menu). Rapid changes are
   * coalesced: one request at a time, the latest choice wins, and only the
   * final response is announced so the UI never flips back in between.
   * Resolves `false` when the choice could not be saved.
   */
  saveTheme(theme: ThemeSetting): Promise<boolean> {
    this.themeWanted = theme;
    if (this.themeSaving) return this.themeSaving;
    const generation = this.generation;
    let saved = false;
    const run = async (): Promise<boolean> => {
      // Let the caller's synchronous code finish before the first request.
      await Promise.resolve();
      try {
        while (this.themeWanted !== null && generation === this.generation) {
          const next = this.themeWanted;
          this.themeWanted = null;
          if (this.statusValue === "ready" && this.current.theme === next) continue;
          await this.save({ theme: next }, { emit: false });
          saved = true;
        }
        return true;
      } catch (error) {
        if (isStaleAppearanceError(error)) return true;
        if (generation === this.generation) this.themeWanted = null;
        console.warn("could not persist theme preference", error);
        return false;
      } finally {
        if (this.themeSaving === promise) this.themeSaving = null;
        if (saved && generation === this.generation) this.emit();
      }
    };
    const promise = run();
    this.themeSaving = promise;
    return promise;
  }

  /**
   * Persist the complete desktop widget list (add, remove, move). Like
   * {@link saveTheme}: one request at a time, the latest layout wins, and only
   * the final response is announced. Resolves `false` when it failed.
   */
  /** Save widgets and shortcuts together in one appearance request. */
  async saveDesktopLayout(widgets: DesktopWidget[], icons: DesktopIcon[] | null): Promise<boolean> {
    if (this.userId === null) throw new StaleAppearanceError();
    const generation = this.generation;
    const payload = {
      widgets: normalizeWidgets(widgets),
      desktopIcons: normalizeDesktopIcons(icons),
    };
    const updated = normalizeAppearance(await this.api.put<Appearance>("/api/appearance", payload));
    this.commit(generation, updated);
    return true;
  }

  saveWidgets(widgets: DesktopWidget[]): Promise<boolean> {
    // An account showing the default set has not saved a list yet: even an
    // unchanged (or empty) list must be stored to make it the account's own.
    return this.saveDesktopField("widgets", normalizeWidgets(widgets), (current, next) =>
      current.widgetsCustomized && JSON.stringify(current.widgets) === JSON.stringify(next),
    );
  }

  /** Persist the complete ordered taskbar pin list (same coalescing as widgets). */
  saveTaskbarPins(pins: string[]): Promise<boolean> {
    return this.saveDesktopField("taskbarPins", normalizeTaskbarPins(pins), (current, next) =>
      JSON.stringify(current.taskbarPins) === JSON.stringify(next),
    );
  }

  /** Persist the complete desktop layout (same coalescing as widgets). */
  saveDesktopIcons(icons: DesktopIcon[]): Promise<boolean> {
    return this.saveDesktopField("desktopIcons", normalizeDesktopIcons(icons) ?? [], (current, next) =>
      JSON.stringify(current.desktopIcons) === JSON.stringify(next),
    );
  }

  /** Go back to the default desktop layout (every app, default order). */
  async resetDesktopIcons(): Promise<boolean> {
    try {
      await this.save({ desktopIcons: null });
      return true;
    } catch (error) {
      if (isStaleAppearanceError(error)) return true;
      console.warn("could not reset the desktop icons", error);
      return false;
    }
  }

  private saveDesktopField<K extends DesktopField>(
    field: K,
    value: NonNullable<AppearancePatch[K]>,
    alreadySaved: (current: Appearance, next: NonNullable<AppearancePatch[K]>) => boolean,
  ): Promise<boolean> {
    const queue = this.desktopQueues[field] as DesktopQueue<NonNullable<AppearancePatch[K]>>;
    queue.wanted = value;
    queue.latest = value;
    if (queue.saving) return queue.saving;
    const generation = this.generation;
    let saved = false;
    const run = async (): Promise<boolean> => {
      await Promise.resolve();
      try {
        while (queue.wanted !== null && generation === this.generation) {
          const next = queue.wanted;
          queue.wanted = null;
          if (this.statusValue === "ready" && alreadySaved(this.current, next)) continue;
          await this.save({ [field]: next } as AppearancePatch, { emit: false });
          saved = true;
        }
        return true;
      } catch (error) {
        if (isStaleAppearanceError(error)) return true;
        if (generation === this.generation) queue.wanted = null;
        console.warn(`could not save desktop ${field}`, error);
        return false;
      } finally {
        if (queue.saving === promise) {
          queue.saving = null;
          queue.latest = null;
        }
        if (generation === this.generation) {
          // Announce the final state; on failure listeners fall back to the saved value.
          if (saved || this.statusValue === "ready") this.emit();
        }
      }
    };
    const promise = run();
    queue.saving = promise;
    return promise;
  }

  /** Forget the signed-in account (sign-out): pending responses are dropped. */
  clear(): void {
    if (this.userId !== null) clearCachedAppearance(this.userId);
    else clearCachedAppearance();
    this.generation += 1;
    this.userId = null;
    this.current = normalizeAppearance(DEFAULT_APPEARANCE);
    this.statusValue = "idle";
    this.lastErrorValue = null;
    this.themeWanted = null;
    this.themeSaving = null;
    this.desktopQueues = { widgets: emptyQueue(), taskbarPins: emptyQueue(), desktopIcons: emptyQueue() };
  }
}

// ---------------------------------------------------------------------------
// Cross-session appearance synchronization.
//
// The server publishes `appearance.changed` to the owner's other sessions with
// the originating session id. This coordinator:
//   * remembers our own session id from the SSE `hello` frame,
//   * ignores the echo of our own save (guarded by a short grace window),
//   * adopts external changes when no editor has a dirty draft,
//   * reports a conflict to open editors when the user has unsaved changes.
// ---------------------------------------------------------------------------

export type AppearanceConflictListener = (appearance: Appearance, event: AppearanceChangedEvent) => void;
export type AppearanceAdoptListener = (appearance: Appearance) => void;

const LOCAL_SAVE_GRACE_MS = 4_000;

export class AppearanceSync {
  private readonly realtime;
  private readonly unsubscribers: Array<() => void> = [];
  private readonly dirtySources = new Set<() => boolean>();
  private readonly conflictListeners = new Set<AppearanceConflictListener>();
  private readonly adoptListeners = new Set<AppearanceAdoptListener>();
  private sessionId: string | null = null;
  private lastLocalSaveAt = 0;
  private disposed = false;

  constructor(
    private readonly service: AppearanceService,
    events: EventStream,
  ) {
    this.realtime = getRealtime(events);
    this.unsubscribers.push(
      this.realtime.on<RealtimeHello>("hello", (data) => {
        if (data && typeof data.sessionId === "string") this.sessionId = data.sessionId;
      }),
    );
    this.unsubscribers.push(
      this.realtime.on<AppearanceChangedEvent>("appearance.changed", (data) => this.handleChanged(data)),
    );
    this.unsubscribers.push(
      this.realtime.onResync(() => {
        void this.resync();
      }),
    );
  }

  get session(): string | null {
    return this.sessionId;
  }

  /** Called right after a successful save so the echo event is ignored. */
  markLocalSave(): void {
    this.lastLocalSaveAt = Date.now();
  }

  /** Register a predicate telling the coordinator whether a draft is dirty. */
  registerDirtySource(source: () => boolean): () => void {
    this.dirtySources.add(source);
    return () => this.dirtySources.delete(source);
  }

  hasDirtyDraft(): boolean {
    for (const source of this.dirtySources) {
      try {
        if (source()) return true;
      } catch {
        /* ignore a broken predicate */
      }
    }
    return false;
  }

  onConflict(listener: AppearanceConflictListener): () => void {
    this.conflictListeners.add(listener);
    return () => this.conflictListeners.delete(listener);
  }

  onAdopt(listener: AppearanceAdoptListener): () => void {
    this.adoptListeners.add(listener);
    return () => this.adoptListeners.delete(listener);
  }

  /** Adopt an appearance (e.g. resolved "Use updated" conflict). */
  adopt(input: Appearance): Appearance {
    const next = this.service.adopt(input);
    for (const listener of this.adoptListeners) {
      try {
        listener(next);
      } catch (error) {
        console.error("appearance adopt listener failed", error);
      }
    }
    return next;
  }

  /** True when the event was triggered by this tab's own recent save. */
  isOwnEcho(event: AppearanceChangedEvent): boolean {
    if (!event.originSessionId) return false;
    if (event.originSessionId !== this.sessionId) return false;
    return Date.now() - this.lastLocalSaveAt < LOCAL_SAVE_GRACE_MS;
  }

  private handleChanged(event: AppearanceChangedEvent): void {
    if (this.disposed || !event || this.service.currentUserId === null) return;
    if (!event.appearance) {
      // Some events (e.g. a deletion from elsewhere) only carry the reason.
      void this.resync();
      return;
    }
    if (this.isOwnEcho(event)) return;
    const appearance = normalizeAppearance(event.appearance);
    const current = normalizeAppearance(this.service.value);
    // The event may echo a save whose response we already applied (different
    // tab of the same session, or a race before the grace window was marked).
    // Identical state needs no adoption and must never look like a conflict.
    if (JSON.stringify(appearance) === JSON.stringify(current)) return;
    // Only widgets (or the upload list) changed: no editor edits those, so
    // take the new state without disturbing an open draft.
    if (!hasAppearanceChanges(current, appearance)) {
      this.service.adopt(appearance);
      return;
    }
    if (this.hasDirtyDraft()) {
      for (const listener of this.conflictListeners) {
        try {
          listener(appearance, event);
        } catch (error) {
          console.error("appearance conflict listener failed", error);
        }
      }
      return;
    }
    this.adopt(appearance);
  }

  private async resync(): Promise<void> {
    if (this.disposed || this.service.currentUserId === null) return;
    try {
      const before = this.service.status === "ready" ? normalizeAppearance(this.service.value) : null;
      const fresh = await this.service.reload();
      // Every reconnect resyncs: an unchanged snapshot (or one where only
      // widgets moved, which reload() already announced) is not news for the
      // editors and must never raise a conflict over an open draft.
      if (before !== null && !hasAppearanceChanges(before, fresh)) return;
      if (this.hasDirtyDraft()) {
        for (const listener of this.conflictListeners) {
          try {
            listener(fresh, { reason: "resync", originSessionId: null, appearance: fresh });
          } catch (error) {
            console.error("appearance conflict listener failed", error);
          }
        }
        return;
      }
      for (const listener of this.adoptListeners) {
        try {
          listener(fresh);
        } catch (error) {
          console.error("appearance adopt listener failed", error);
        }
      }
    } catch {
      /* stale (account changed) or offline: the next reconnect will retry */
    }
  }

  dispose(): void {
    this.disposed = true;
    for (const unsubscribe of this.unsubscribers.splice(0)) {
      try {
        unsubscribe();
      } catch {
        /* ignore */
      }
    }
    this.dirtySources.clear();
    this.conflictListeners.clear();
    this.adoptListeners.clear();
  }
}

const SYNC_MAP = new WeakMap<ApiClient, AppearanceSync>();

/** Shared cross-session appearance coordinator for this tab. */
export function getAppearanceSync(api: ApiClient, events: EventStream): AppearanceSync {
  let sync = SYNC_MAP.get(api);
  if (!sync) {
    sync = new AppearanceSync(getAppearanceService(api), events);
    SYNC_MAP.set(api, sync);
  }
  return sync;
}

// ---------------------------------------------------------------------------
// Shared instance: every part of the UI (desktop, Control Panel, Personalize)
// talks to the same service so there is exactly one appearance state.
// ---------------------------------------------------------------------------

const SERVICE_MAP = new WeakMap<ApiClient, AppearanceService>();

export function getAppearanceService(api: ApiClient): AppearanceService {
  let service = SERVICE_MAP.get(api);
  if (!service) {
    service = new AppearanceService(api);
    SERVICE_MAP.set(api, service);
  }
  return service;
}
