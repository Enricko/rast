// Free space on the desktop, shared by icons and widgets so that neither ever
// covers the other: pure geometry over rectangles in one layer's pixels.
//
//   * icons have priority: they keep their grid cells;
//   * a widget is shown at its saved place when that is free, else where it is
//     shown now (while that stays free), else in the nearest free spot; with no
//     room at all it is hidden until there is;
//   * a dragged widget or icon lands in the nearest free spot to where it is
//     dropped, so nothing else moves.

import { cellKey, type Grid } from "./icon-layout";

export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface Point {
  left: number;
  top: number;
}

export interface Size {
  width: number;
  height: number;
}

/** Rounding slack: boxes that only touch (or overlap by a rounding error) do not collide. */
const EPSILON = 0.5;

export function intersects(a: Rect, b: Rect): boolean {
  return (
    a.left < b.left + b.width - EPSILON &&
    b.left < a.left + a.width - EPSILON &&
    a.top < b.top + b.height - EPSILON &&
    b.top < a.top + a.height - EPSILON
  );
}

/** `rect` grown by `by` pixels on every side (the space it keeps around itself). */
export function inflate(rect: Rect, by: number): Rect {
  return { left: rect.left - by, top: rect.top - by, width: rect.width + by * 2, height: rect.height + by * 2 };
}

export function translate(rect: Rect, dx: number, dy: number): Rect {
  return { ...rect, left: rect.left + dx, top: rect.top + dy };
}

/** True when a `size` box at `at` lies inside `bounds` (origin 0, 0) and touches no obstacle. */
export function isFree(at: Point, size: Size, bounds: Size, obstacles: ReadonlyArray<Rect>): boolean {
  if (at.left < -EPSILON || at.top < -EPSILON) return false;
  if (at.left + size.width > bounds.width + EPSILON || at.top + size.height > bounds.height + EPSILON) return false;
  const box = { ...at, ...size };
  return !obstacles.some((obstacle) => intersects(box, obstacle));
}

/**
 * The free spot for a `size` box nearest to `desired` (straight-line
 * distance), inside `bounds` and clear of every obstacle; null when there is
 * none. The nearest free spot always has each coordinate on the desired line,
 * a bounds edge or an obstacle edge, so only those combinations are tried.
 */
export function nearestFreeSpot(
  size: Size,
  desired: Point,
  bounds: Size,
  obstacles: ReadonlyArray<Rect>,
): Point | null {
  const maxLeft = bounds.width - size.width;
  const maxTop = bounds.height - size.height;
  if (maxLeft < -EPSILON || maxTop < -EPSILON) return null;
  const clampLeft = (value: number): number => Math.max(0, Math.min(Math.max(0, maxLeft), value));
  const clampTop = (value: number): number => Math.max(0, Math.min(Math.max(0, maxTop), value));
  const xs = new Set<number>([clampLeft(desired.left), 0, Math.max(0, maxLeft)]);
  const ys = new Set<number>([clampTop(desired.top), 0, Math.max(0, maxTop)]);
  for (const obstacle of obstacles) {
    for (const x of [obstacle.left - size.width, obstacle.left + obstacle.width]) if (x >= 0 && x <= maxLeft + EPSILON) xs.add(x);
    for (const y of [obstacle.top - size.height, obstacle.top + obstacle.height]) if (y >= 0 && y <= maxTop + EPSILON) ys.add(y);
  }
  const candidates: Array<Point & { distance: number }> = [];
  for (const left of xs) {
    for (const top of ys) {
      candidates.push({ left, top, distance: (left - desired.left) ** 2 + (top - desired.top) ** 2 });
    }
  }
  // Closest first; ties go to the upper, then the left one (stable results).
  candidates.sort((a, b) => a.distance - b.distance || a.top - b.top || a.left - b.left);
  for (const candidate of candidates) {
    if (isFree(candidate, size, bounds, obstacles)) return { left: candidate.left, top: candidate.top };
  }
  return null;
}

export interface WidgetBox {
  id: string;
  size: Size;
  /** Its saved place on this desktop. */
  desired: Point;
  /** Where it is shown now, if anywhere. */
  current?: Point | null;
}

/**
 * Where each widget is shown (null = no room, hidden), in priority order:
 * earlier boxes are placed first and become obstacles for later ones.
 * `gap` is the space kept between two widgets; `obstacles` (icons) already
 * include their own margin.
 */
export function arrangeWidgets(
  boxes: ReadonlyArray<WidgetBox>,
  bounds: Size,
  obstacles: ReadonlyArray<Rect>,
  gap: number,
): Map<string, Point | null> {
  const placed = new Map<string, Point | null>();
  const taken: Rect[] = [...obstacles];
  for (const box of boxes) {
    const spot = isFree(box.desired, box.size, bounds, taken)
      ? box.desired
      : box.current && isFree(box.current, box.size, bounds, taken)
        ? box.current
        : nearestFreeSpot(box.size, box.desired, bounds, taken);
    placed.set(box.id, spot ? { left: spot.left, top: spot.top } : null);
    if (spot) taken.push(inflate({ ...spot, ...box.size }, gap));
  }
  return placed;
}

/** Grid cells (of `cellSize`, from the layer's top left) that any rect covers. */
export function coveredCells(rects: ReadonlyArray<Rect>, cellSize: { w: number; h: number }, grid: Grid): Set<string> {
  const covered = new Set<string>();
  if (cellSize.w <= 0 || cellSize.h <= 0) return covered;
  for (const rect of rects) {
    const firstCol = Math.max(0, Math.floor((rect.left + EPSILON) / cellSize.w));
    const lastCol = Math.min(grid.cols - 1, Math.ceil((rect.left + rect.width - EPSILON) / cellSize.w) - 1);
    const firstRow = Math.max(0, Math.floor((rect.top + EPSILON) / cellSize.h));
    const lastRow = Math.min(grid.rows - 1, Math.ceil((rect.top + rect.height - EPSILON) / cellSize.h) - 1);
    for (let col = firstCol; col <= lastCol; col += 1) {
      for (let row = firstRow; row <= lastRow; row += 1) covered.add(cellKey({ col, row }));
    }
  }
  return covered;
}
