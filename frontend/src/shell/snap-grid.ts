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
 * The default right-side widget group: each widget keeps its own preferred
 * size, with 16px gaps, right aligned, in two columns when both fit (one
 * otherwise). Order goes down the left column, then the right one. Widgets
 * never adopt the largest widget's dimensions.
 */
export function defaultWidgetGroup(sizes: WidgetSize[], bounds: Bounds, gap = WIDGET_GAP): Rect[] {
  if (sizes.length === 0 || bounds.w <= 0 || bounds.h <= 0) return [];
  const clampSize = (size: WidgetSize): WidgetSize => ({
    w: Math.min(size.w, bounds.w),
    h: Math.min(size.h, bounds.h),
  });
  const items = sizes.map(clampSize);
  const placeColumn = (column: WidgetSize[], x: number, columnWidth: number): Rect[] => {
    const rects: Rect[] = [];
    let y = bounds.y;
    for (const item of column) {
      rects.push({ x: x + columnWidth - item.w, y, w: item.w, h: item.h });
      y += item.h + gap;
    }
    return rects;
  };
  const singleWidth = Math.min(bounds.w, Math.max(...items.map((item) => item.w)));
  const single = (): Rect[] => {
    const x = bounds.x + bounds.w - singleWidth;
    return placeColumn(items, x, singleWidth);
  };
  if (items.length === 1) return single();
  const split = Math.ceil(items.length / 2);
  const leftItems = items.slice(0, split);
  const rightItems = items.slice(split);
  const leftWidth = Math.max(...leftItems.map((item) => item.w));
  const rightWidth = Math.max(...rightItems.map((item) => item.w));
  const total = leftWidth + gap + rightWidth;
  if (total > bounds.w || rightItems.length === 0) return single();
  const leftX = bounds.x + bounds.w - total;
  const rightX = leftX + leftWidth + gap;
  return [...placeColumn(leftItems, leftX, leftWidth), ...placeColumn(rightItems, rightX, rightWidth)];
}

export function rectsOverlapAny(rect: Rect, others: Rect[]): boolean {
  return others.some((other) => rectsIntersect(other, rect));
}
