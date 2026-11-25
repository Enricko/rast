// Display geometry and settings (pure, no DOM): shared by core/display.ts and
// the unit tests.
//
// The browser window is the desktop's "monitor". Its size in device pixels
// (CSS size × devicePixelRatio) does not change when the page is zoomed, only
// when the window is resized, so it is the desktop's resolution. A scale chosen
// per screen, like an operating system's display scale, turns that resolution
// into the desktop's layout size: 1920 × 1080 at 125 % lays the desktop out at
// 1536 × 864. The <body> is then transformed by `factor` to fill the window, so
// browser zoom changes neither the layout nor the size of anything on screen.

/** Scales offered in Display settings (1 = 100 %). */
export const SCALE_OPTIONS: readonly number[] = [0.75, 1, 1.25, 1.5, 1.75, 2, 2.25, 2.5, 3];
export const MIN_SCALE = 0.5;
export const MAX_SCALE = 4;

/** Below this difference the body is left untransformed (no resampling). */
const FACTOR_EPSILON = 0.005;

export interface ViewportMetrics {
  /** Viewport size in CSS pixels. */
  cssWidth: number;
  cssHeight: number;
  devicePixelRatio: number;
}

export interface DisplayGeometry {
  /** Browser window in device pixels: the desktop's resolution. */
  resolutionWidth: number;
  resolutionHeight: number;
  /** Desktop layout size in CSS pixels (the size of <body>). */
  width: number;
  height: number;
  /** On-screen CSS pixels per desktop pixel (the <body> transform; 1 = none). */
  factor: number;
  scale: number;
}

export interface ScreenDisplay {
  scale: number;
  /** The scale this screen started with (its device pixel ratio then). */
  recommended: number;
}

export interface StoredDisplay {
  version: 1;
  /** Per screen (see `screenKey`), like an operating system's per-monitor scale. */
  screens: Record<string, ScreenDisplay>;
}

const MAX_SCREENS = 16;

function finite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

/** A scale rounded to whole percent within the supported range, or null. */
export function normalizeScale(value: unknown): number | null {
  if (!finite(value) || value <= 0) return null;
  const clamped = Math.min(MAX_SCALE, Math.max(MIN_SCALE, value));
  return Math.round(clamped * 100) / 100;
}

/** The starting scale for a screen: its device pixel ratio (what the page looked like before). */
export function defaultScaleFor(devicePixelRatio: number): number {
  return normalizeScale(finite(devicePixelRatio) && devicePixelRatio > 0 ? devicePixelRatio : 1) ?? 1;
}

/**
 * Identifies the physical screen. `screen.width/height` are not affected by page
 * zoom in Chromium and Safari, so zooming keeps the key while moving the window
 * to another monitor changes it. Orientation is ignored.
 */
export function screenKey(screenWidth: number, screenHeight: number): string {
  const w = finite(screenWidth) && screenWidth > 0 ? Math.round(screenWidth) : 0;
  const h = finite(screenHeight) && screenHeight > 0 ? Math.round(screenHeight) : 0;
  return `${Math.max(w, h)}x${Math.min(w, h)}`;
}

export function computeGeometry(viewport: ViewportMetrics, scale: number): DisplayGeometry {
  const dpr = finite(viewport.devicePixelRatio) && viewport.devicePixelRatio > 0 ? viewport.devicePixelRatio : 1;
  const cssWidth = finite(viewport.cssWidth) && viewport.cssWidth > 0 ? viewport.cssWidth : 1;
  const cssHeight = finite(viewport.cssHeight) && viewport.cssHeight > 0 ? viewport.cssHeight : 1;
  const s = normalizeScale(scale) ?? defaultScaleFor(dpr);
  let factor = s / dpr;
  if (Math.abs(factor - 1) < FACTOR_EPSILON) factor = 1;
  return {
    resolutionWidth: Math.round(cssWidth * dpr),
    resolutionHeight: Math.round(cssHeight * dpr),
    width: cssWidth / factor,
    height: cssHeight / factor,
    factor,
    scale: s,
  };
}

/** True when two geometries lay the desktop out differently (sub-pixel noise ignored). */
export function geometryChanged(a: DisplayGeometry, b: DisplayGeometry): boolean {
  return (
    Math.abs(a.width - b.width) >= 0.5 ||
    Math.abs(a.height - b.height) >= 0.5 ||
    Math.abs(a.factor - b.factor) > 1e-6 ||
    a.scale !== b.scale ||
    a.resolutionWidth !== b.resolutionWidth ||
    a.resolutionHeight !== b.resolutionHeight
  );
}

export function emptyStoredDisplay(): StoredDisplay {
  return { version: 1, screens: {} };
}

/** Parse the persisted settings, dropping anything malformed. */
export function parseStoredDisplay(raw: string | null | undefined): StoredDisplay {
  const result = emptyStoredDisplay();
  if (!raw) return result;
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch {
    return result;
  }
  if (!data || typeof data !== "object") return result;
  const screens = (data as { screens?: unknown }).screens;
  if (!screens || typeof screens !== "object") return result;
  for (const [key, value] of Object.entries(screens as Record<string, unknown>)) {
    if (!/^\d{1,5}x\d{1,5}$/.test(key) || !value || typeof value !== "object") continue;
    const scale = normalizeScale((value as { scale?: unknown }).scale);
    const recommended = normalizeScale((value as { recommended?: unknown }).recommended);
    if (scale === null) continue;
    result.screens[key] = { scale, recommended: recommended ?? scale };
    if (Object.keys(result.screens).length >= MAX_SCREENS) break;
  }
  return result;
}

/** The scales to list for a screen: the standard ones plus its recommended and current scale. */
export function scaleOptionsFor(entry: ScreenDisplay): number[] {
  const all = new Set<number>(SCALE_OPTIONS);
  all.add(entry.recommended);
  all.add(entry.scale);
  return [...all].sort((a, b) => a - b);
}

export function formatScale(scale: number): string {
  return `${Math.round(scale * 100)}%`;
}
