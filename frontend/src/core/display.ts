// Display: the desktop's resolution and scale, independent of browser zoom.
//
// See display-math.ts for the model. This module applies it to the page: the
// <body> gets the desktop's layout size and a scale transform that fills the
// window, `--vw/--vh/--stage-w/--stage-h` replace viewport units, and the
// helpers below convert pointer coordinates (which stay in on-screen pixels)
// into desktop pixels. The chosen scale is kept per device and per screen in
// localStorage, like an operating system's per-monitor display scale.

import {
  computeGeometry,
  defaultScaleFor,
  emptyStoredDisplay,
  geometryChanged,
  normalizeScale,
  parseStoredDisplay,
  scaleOptionsFor,
  screenKey,
  type DisplayGeometry,
  type ScreenDisplay,
  type StoredDisplay,
} from "./display-math";

export { formatScale } from "./display-math";
export type { DisplayGeometry } from "./display-math";

const STORAGE_KEY = "rast.display.v1";

export interface DisplayInfo extends DisplayGeometry {
  recommended: number;
  options: number[];
}

type Listener = (geometry: DisplayGeometry) => void;

let stored: StoredDisplay = emptyStoredDisplay();
let geometry: DisplayGeometry = computeGeometry(
  {
    cssWidth: typeof window === "undefined" ? 1280 : window.innerWidth,
    cssHeight: typeof window === "undefined" ? 720 : window.innerHeight,
    devicePixelRatio: 1,
  },
  1,
);
let started = false;
let dprQuery: MediaQueryList | null = null;
const listeners = new Set<Listener>();

function readStored(): StoredDisplay {
  try {
    return parseStoredDisplay(window.localStorage.getItem(STORAGE_KEY));
  } catch {
    return emptyStoredDisplay();
  }
}

function writeStored(): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored));
  } catch {
    /* private mode or storage disabled: the scale lasts for this page only */
  }
}

function currentScreenKey(): string {
  return screenKey(window.screen?.width ?? 0, window.screen?.height ?? 0);
}

function screenEntry(): ScreenDisplay {
  const key = currentScreenKey();
  const existing = stored.screens[key];
  if (existing) return existing;
  // First visit on this screen: start from what the page looked like so far.
  const recommended = defaultScaleFor(window.devicePixelRatio);
  const entry: ScreenDisplay = { scale: recommended, recommended };
  stored.screens[key] = entry;
  writeStored();
  return entry;
}

function viewportSize(): { width: number; height: number } {
  const root = document.documentElement;
  return {
    width: root.clientWidth || window.innerWidth,
    height: root.clientHeight || window.innerHeight,
  };
}

/** Recompute the desktop size and apply it to the page. */
export function applyDisplay(): void {
  const viewport = viewportSize();
  const next = computeGeometry(
    { cssWidth: viewport.width, cssHeight: viewport.height, devicePixelRatio: window.devicePixelRatio || 1 },
    screenEntry().scale,
  );
  const root = document.documentElement;
  const body = document.body;
  if (body) {
    body.style.width = `${next.width}px`;
    body.style.height = `${next.height}px`;
    body.style.transform = next.factor === 1 ? "" : `scale(${next.factor})`;
  }
  root.style.setProperty("--stage-w", `${next.width}px`);
  root.style.setProperty("--stage-h", `${next.height}px`);
  root.style.setProperty("--vw", `${next.width / 100}px`);
  root.style.setProperty("--vh", `${next.height / 100}px`);
  root.dataset.displayScaled = next.factor === 1 ? "false" : "true";
  const changed = geometryChanged(geometry, next);
  geometry = next;
  if (changed && started) {
    for (const listener of listeners) {
      try {
        listener(next);
      } catch (error) {
        console.error("display listener failed", error);
      }
    }
  }
}

function watchDevicePixelRatio(): void {
  dprQuery?.removeEventListener("change", onDevicePixelRatioChange);
  if (typeof window.matchMedia !== "function") return;
  dprQuery = window.matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
  dprQuery.addEventListener("change", onDevicePixelRatioChange);
}

function onDevicePixelRatioChange(): void {
  // Zoom or a move to another monitor without a resize event.
  applyDisplay();
  watchDevicePixelRatio();
}

/** Start following the window. Call once, before the shell renders. */
export function startDisplay(): void {
  if (started) return;
  stored = readStored();
  applyDisplay();
  started = true;
  window.addEventListener("resize", applyDisplay);
  watchDevicePixelRatio();
  window.addEventListener("storage", (event) => {
    if (event.key !== STORAGE_KEY) return;
    stored = readStored();
    applyDisplay();
  });
}

export function getDisplay(): DisplayInfo {
  const entry = started ? screenEntry() : { scale: geometry.scale, recommended: geometry.scale };
  return { ...geometry, recommended: entry.recommended, options: scaleOptionsFor(entry) };
}

/** Set this screen's scale (like an OS display scale) and apply it. */
export function setDisplayScale(scale: number): void {
  const value = normalizeScale(scale);
  if (value === null) return;
  const entry = screenEntry();
  entry.scale = value;
  writeStored();
  applyDisplay();
}

export function onDisplayChange(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** On-screen pixels per desktop pixel. Divide pointer deltas by this. */
export function displayFactor(): number {
  return geometry.factor;
}

/** Desktop layout size in CSS pixels (replaces window.innerWidth/innerHeight). */
export function stageSize(): { width: number; height: number } {
  return { width: geometry.width, height: geometry.height };
}

/** Convert a pointer/client position into desktop coordinates. */
export function clientToStage(clientX: number, clientY: number): { x: number; y: number } {
  const f = geometry.factor;
  return { x: clientX / f, y: clientY / f };
}

/** Convert a getBoundingClientRect() result into desktop coordinates. */
export function rectToStage(rect: DOMRectReadOnly): { left: number; top: number; right: number; bottom: number; width: number; height: number } {
  const f = geometry.factor;
  return {
    left: rect.left / f,
    top: rect.top / f,
    right: rect.right / f,
    bottom: rect.bottom / f,
    width: rect.width / f,
    height: rect.height / f,
  };
}
