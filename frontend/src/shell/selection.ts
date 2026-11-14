// Shared selection geometry for desktop marquees and File Station.
// Pure helpers only: no DOM, no pointer state.

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

export type SelectionMode = "replace" | "toggle" | "add";

/** Rectangle between two pointer positions, in any drag direction. */
export function marqueeBox(ax: number, ay: number, bx: number, by: number): Box {
  return {
    x: Math.min(ax, bx),
    y: Math.min(ay, by),
    w: Math.abs(bx - ax),
    h: Math.abs(by - ay),
  };
}

export function boxesIntersect(a: Box, b: Box): boolean {
  return a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;
}

/** Apply a marquee/click hit list to the current selection. */
export function updateSelection(
  current: ReadonlySet<string>,
  hits: readonly string[],
  mode: SelectionMode,
): Set<string> {
  const next = new Set(mode === "replace" ? [] : current);
  for (const id of hits) {
    if (mode === "toggle") {
      if (next.has(id)) next.delete(id);
      else next.add(id);
    } else {
      next.add(id);
    }
  }
  return next;
}

/** Clamp a group delta to the safe bounds and keep it on the 8px grid. */
export function clampGroupDelta(
  rects: readonly Box[],
  dx: number,
  dy: number,
  bounds: Box,
  grid = 8,
): { dx: number; dy: number } {
  if (rects.length === 0) return { dx: 0, dy: 0 };
  const left = Math.min(...rects.map((rect) => rect.x));
  const top = Math.min(...rects.map((rect) => rect.y));
  const right = Math.max(...rects.map((rect) => rect.x + rect.w));
  const bottom = Math.max(...rects.map((rect) => rect.y + rect.h));
  const snappedX = Math.round(dx / grid) * grid;
  const snappedY = Math.round(dy / grid) * grid;
  return {
    dx: Math.max(bounds.x - left, Math.min(bounds.x + bounds.w - right, snappedX)),
    dy: Math.max(bounds.y - top, Math.min(bounds.y + bounds.h - bottom, snappedY)),
  };
}

/** True when a moved group intersects any unselected obstacle. */
export function groupCollides(rects: readonly Box[], others: readonly Box[]): boolean {
  return rects.some((rect) => others.some((other) => boxesIntersect(rect, other)));
}

/** Unit direction to auto-scroll a pane when the pointer is near an edge. */
export function edgeScroll(distance: number, edge: number): number {
  if (distance < 0) return 0;
  if (distance < edge) return -1;
  return 0;
}

/** Vertical/horizontal auto-scroll velocity for a pointer inside a viewport. */
export function autoScrollVelocity(pointer: number, start: number, size: number, threshold = 24): number {
  if (pointer < start + threshold) return -Math.ceil((start + threshold - pointer) / 4);
  if (pointer > start + size - threshold) return Math.ceil((pointer - (start + size - threshold)) / 4);
  return 0;
}

/** Drop descendants when their parent folder is also selected. */
export function dedupeDescendants(paths: readonly string[]): string[] {
  const sorted = [...new Set(paths)].sort((a, b) => a.length - b.length);
  const out: string[] = [];
  for (const path of sorted) {
    if (out.some((parent) => path.startsWith(parent.endsWith("/") ? parent : `${parent}/`))) continue;
    out.push(path);
  }
  return out;
}

/** A folder cannot be moved into itself or one of its descendants. */
export function validMoveTarget(paths: readonly string[], destDir: string): boolean {
  const normalized = destDir.endsWith("/") ? destDir : `${destDir}/`;
  return !paths.some((path) => {
    const folder = path.endsWith("/") ? path : `${path}/`;
    return normalized === folder || normalized.startsWith(folder);
  });
}

export interface SelectionSummary {
  total: number;
  shortcuts: number;
  widgets: number;
  allShortcuts: boolean;
  single: boolean;
}

/** Count the type-qualified desktop selection keys (`icon:` / `widget:`). */
export function summarizeSelection(keys: Iterable<string>): SelectionSummary {
  let shortcuts = 0;
  let widgets = 0;
  for (const key of keys) {
    if (key.startsWith("icon:")) shortcuts += 1;
    else if (key.startsWith("widget:")) widgets += 1;
  }
  const total = shortcuts + widgets;
  return { total, shortcuts, widgets, allShortcuts: total > 0 && widgets === 0, single: total === 1 };
}
