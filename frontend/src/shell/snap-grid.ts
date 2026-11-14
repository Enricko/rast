// Shared desktop coordinates and snapping.
//
// Everything on the desktop is positioned in CSS pixels relative to the
// desktop container, inside a 16px safe inset and above the taskbar. Widget
// positions/sizes snap to an 8px grid; app icons keep their readable cell
// size (a multiple of 8) so both live on the same grid.

export const GRID = 8;
export const SAFE_INSET = 16;
/** Minimum readable widget size. */
export const MIN_WIDGET_WIDTH = 200;
export const MIN_WIDGET_HEIGHT = 200;
/** Gap used by the default right-side widget group. */
export const WIDGET_GAP = 16;

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Bounds {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Round to the nearest grid line. */
export function snap(value: number, grid = GRID): number {
  return Math.round(value / grid) * grid;
}

/** Round up to the next grid line (used for sizes). */
export function snapUp(value: number, grid = GRID): number {
  return Math.ceil(value / grid) * grid;
}

/** The usable desktop rectangle, excluding the safe inset and the taskbar. */
export function usableBounds(width: number, height: number, taskbarHeight: number): Bounds {
  return {
    x: SAFE_INSET,
    y: SAFE_INSET,
    w: Math.max(0, width - SAFE_INSET * 2),
    h: Math.max(0, height - taskbarHeight - SAFE_INSET * 2),
  };
}

export function rectsIntersect(a: Rect, b: Rect, gap = 0): boolean {
  return (
    a.x < b.x + b.w + gap &&
    a.x + a.w + gap > b.x &&
    a.y < b.y + b.h + gap &&
    a.y + a.h + gap > b.y
  );
}

/**
 * Snap size and position to the grid and keep the whole rectangle inside
 * `bounds`. The right/bottom edge wins: a rectangle that would stick out is
 * moved left/up and remains on the grid.
 */
export function fitRect(input: Rect, bounds: Bounds, minW = MIN_WIDGET_WIDTH, minH = MIN_WIDGET_HEIGHT): Rect {
  const maxW = Math.max(0, bounds.w);
  const maxH = Math.max(0, bounds.h);
  const w = Math.min(maxW, Math.max(Math.min(minW, maxW), snapUp(input.w)));
  const h = Math.min(maxH, Math.max(Math.min(minH, maxH), snapUp(input.h)));
  const minX = bounds.x;
  const minY = bounds.y;
  const maxX = Math.max(minX, Math.floor((bounds.x + bounds.w - w) / GRID) * GRID);
  const maxY = Math.max(minY, Math.floor((bounds.y + bounds.h - h) / GRID) * GRID);
  const x = Math.min(Math.max(snap(input.x), minX), maxX);
  const y = Math.min(Math.max(snap(input.y), minY), maxY);
  return { x, y, w, h };
}

/**
 * Nearest grid-aligned position for `size` that does not intersect any
 * obstacle and stays inside `bounds`. Returns null when nothing fits.
 */
export function nearestFreeRect(
  desired: Rect,
  size: { w: number; h: number },
  bounds: Bounds,
  obstacles: Rect[],
  gap = 0,
): Rect | null {
  const w = Math.min(size.w, bounds.w);
  const h = Math.min(size.h, bounds.h);
  const minX = bounds.x;
  const minY = bounds.y;
  const maxX = bounds.x + bounds.w - w;
  const maxY = bounds.y + bounds.h - h;
  if (maxX < minX || maxY < minY) return null;
  const startX = Math.min(Math.max(snap(desired.x), minX), maxX);
  const startY = Math.min(Math.max(snap(desired.y), minY), maxY);
  let best: Rect | null = null;
  let bestDistance = Number.POSITIVE_INFINITY;
  for (let y = minY; y <= maxY; y += GRID) {
    for (let x = minX; x <= maxX; x += GRID) {
      const candidate = { x, y, w, h };
      if (obstacles.some((other) => rectsIntersect(other, candidate, gap))) continue;
      const distance = (x - startX) ** 2 + (y - startY) ** 2;
      if (distance < bestDistance) {
        best = candidate;
        bestDistance = distance;
      }
    }
  }
  return best;
}

export interface WidgetSize {
  w: number;
  h: number;
}

/**
 * The default right-side widget group: uniform sizes, 16px gaps, right
 * aligned, in two columns when the width allows (one otherwise). Order goes
 * down each column.
 */
export function defaultWidgetGroup(sizes: WidgetSize[], bounds: Bounds, gap = WIDGET_GAP): Rect[] {
  if (sizes.length === 0 || bounds.w <= 0 || bounds.h <= 0) return [];
  const width = Math.min(
    Math.max(...sizes.map((size) => size.w)),
    Math.max(0, bounds.w),
  );
  const height = Math.min(
    Math.max(...sizes.map((size) => size.h)),
    Math.max(0, bounds.h),
  );
  const rowsThatFit = Math.max(1, Math.floor((bounds.h + gap) / (height + gap)));
  const columns = bounds.w >= width * 2 + gap ? 2 : 1;
  const perColumn = columns === 2 ? Math.ceil(sizes.length / 2) : sizes.length;
  const result: Rect[] = [];
  let index = 0;
  for (let column = columns - 1; column >= 0 && index < sizes.length; column -= 1) {
    const count = Math.min(columns === 2 ? perColumn : sizes.length, sizes.length - index);
    const x = bounds.x + bounds.w - width - (columns - 1 - column) * (width + gap);
    let y = bounds.y;
    for (let row = 0; row < count && index < sizes.length; row += 1) {
      if (row >= rowsThatFit) break;
      result.push({ x, y, w: width, h: height });
      y += height + gap;
      index += 1;
    }
  }
  return result;
}

export function rectsOverlapAny(rect: Rect, others: Rect[]): boolean {
  return others.some((other) => rectsIntersect(other, rect));
}
