// Desktop icon layout: pure functions over grid cells (column, row from the
// top left). The desktop renders `displayLayout`, edits it with `moveIcon`,
// `placeIcon` and `removeIcon`, and saves the result of `layoutForSave`.

import type { DesktopIcon } from "../core/appearance";

export interface Grid {
  cols: number;
  rows: number;
}

export interface Cell {
  col: number;
  row: number;
}

/** How icons fill the grid: down each column (desktop) or along each row (phone). */
export type FillOrder = "columns" | "rows";

const inGrid = (cell: Cell, grid: Grid): boolean =>
  cell.col >= 0 && cell.row >= 0 && cell.col < grid.cols && cell.row < grid.rows;

const taken = (layout: DesktopIcon[], cell: Cell, except?: string): boolean =>
  layout.some((icon) => icon.id !== except && icon.col === cell.col && icon.row === cell.row);

/** Every app in order: top to bottom, then the next column (or row by row on a phone). */
export function defaultLayout(ids: string[], grid: Grid, order: FillOrder = "columns"): DesktopIcon[] {
  if (order === "rows") {
    const cols = Math.max(1, grid.cols);
    return ids.map((id, index) => ({ id, col: index % cols, row: Math.floor(index / cols) }));
  }
  const rows = Math.max(1, grid.rows);
  return ids.map((id, index) => ({ id, col: Math.floor(index / rows), row: index % rows }));
}

/** The first free cell in fill order; null when the grid is full. */
export function firstFreeCell(layout: DesktopIcon[], grid: Grid, order: FillOrder = "columns"): Cell | null {
  const outer = order === "rows" ? grid.rows : grid.cols;
  const inner = order === "rows" ? grid.cols : grid.rows;
  for (let a = 0; a < outer; a += 1) {
    for (let b = 0; b < inner; b += 1) {
      const cell = order === "rows" ? { col: b, row: a } : { col: a, row: b };
      if (!taken(layout, cell)) return cell;
    }
  }
  return null;
}

/** The free cell closest to `cell` (itself when free); null when the grid is full. */
export function nearestFreeCell(layout: DesktopIcon[], cell: Cell, grid: Grid): Cell | null {
  let best: Cell | null = null;
  let bestDistance = Infinity;
  for (let col = 0; col < grid.cols; col += 1) {
    for (let row = 0; row < grid.rows; row += 1) {
      if (taken(layout, { col, row })) continue;
      const distance = (col - cell.col) ** 2 + (row - cell.row) ** 2;
      if (distance < bestDistance) {
        best = { col, row };
        bestDistance = distance;
      }
    }
  }
  return best;
}

/**
 * What the desktop shows: the saved layout (or the default one) for the apps
 * the account may open now. An icon whose cell is off screen (a smaller
 * window than when it was placed) is shown in a free cell instead: the one it
 * is shown in now (`previous`) while that stays free, so it does not jump
 * around while other icons move.
 */
export function displayLayout(
  saved: DesktopIcon[] | null,
  visible: string[],
  grid: Grid,
  order: FillOrder = "columns",
  previous: DesktopIcon[] = [],
): DesktopIcon[] {
  if (saved === null) return defaultLayout(visible, grid, order);
  const shown = saved.filter((icon) => visible.includes(icon.id));
  const placed = shown.filter((icon) => inGrid(icon, grid));
  for (const icon of shown) {
    if (inGrid(icon, grid)) continue;
    const hint = previous.find((p) => p.id === icon.id);
    const cell =
      hint && inGrid(hint, grid) && !taken(placed, hint) ? { col: hint.col, row: hint.row } : firstFreeCell(placed, grid, order);
    // No room at all: keep it where it was (the browser clips it).
    placed.push(cell ? { id: icon.id, ...cell } : icon);
  }
  return placed;
}

/** Move an icon to `cell`; an icon already there takes the moved icon's old cell. */
export function moveIcon(layout: DesktopIcon[], id: string, cell: Cell): DesktopIcon[] {
  const moving = layout.find((icon) => icon.id === id);
  if (!moving || (moving.col === cell.col && moving.row === cell.row)) return layout;
  return layout.map((icon) => {
    if (icon.id === id) return { id, col: cell.col, row: cell.row };
    if (icon.col === cell.col && icon.row === cell.row) return { id: icon.id, col: moving.col, row: moving.row };
    return icon;
  });
}

/**
 * Put an app on the desktop: at `cell` (or the free cell nearest to it), or
 * in the first free cell. An app already on the desktop moves there. Returns
 * null when there is no room.
 */
export function placeIcon(
  layout: DesktopIcon[],
  id: string,
  cell: Cell | null,
  grid: Grid,
  order: FillOrder = "columns",
): DesktopIcon[] | null {
  if (layout.some((icon) => icon.id === id)) {
    return cell && inGrid(cell, grid) ? moveIcon(layout, id, cell) : layout;
  }
  const target = cell && inGrid(cell, grid) ? nearestFreeCell(layout, cell, grid) : firstFreeCell(layout, grid, order);
  if (!target) return null;
  return [...layout, { id, ...target }];
}

export function removeIcon(layout: DesktopIcon[], id: string): DesktopIcon[] {
  return layout.filter((icon) => icon.id !== id);
}

/**
 * The layout to save after an edit turned `before` (what the desktop showed)
 * into `next`:
 *   * icons the edit did not move keep their saved cell, so an icon that was
 *     only shown elsewhere because this screen is smaller keeps its place on
 *     the bigger one;
 *   * saved icons of apps the account cannot open right now are kept (they
 *     come back when access returns), unless their cell is now used.
 */
export function layoutForSave(
  next: DesktopIcon[],
  saved: DesktopIcon[] | null,
  visible: string[],
  before?: DesktopIcon[],
): DesktopIcon[] {
  const savedCell = (id: string): DesktopIcon | undefined => saved?.find((icon) => icon.id === id);
  const out: DesktopIcon[] = [];
  const kept: DesktopIcon[] = [];
  for (const icon of next) {
    const was = before?.find((b) => b.id === icon.id);
    const stored = savedCell(icon.id);
    const untouched = was !== undefined && was.col === icon.col && was.row === icon.row;
    if (untouched && stored && (stored.col !== icon.col || stored.row !== icon.row)) kept.push({ ...stored });
    else out.push({ ...icon });
  }
  for (const icon of kept) {
    // Never two icons in one cell: a clash keeps the cell it was shown in.
    out.push(taken(out, icon) ? { ...next.find((n) => n.id === icon.id)! } : icon);
  }
  for (const icon of saved ?? []) {
    if (visible.includes(icon.id) || out.some((o) => o.id === icon.id)) continue;
    if (taken(out, icon)) continue;
    out.push({ ...icon });
  }
  return out;
}

/** The icon next to `from` in a direction (arrow-key navigation): the same line first, then the closest; or null. */
export function neighbour(layout: DesktopIcon[], from: Cell, direction: "up" | "down" | "left" | "right"): DesktopIcon | null {
  let best: DesktopIcon | null = null;
  let bestScore = Infinity;
  for (const icon of layout) {
    const dc = icon.col - from.col;
    const dr = icon.row - from.row;
    let primary: number;
    let secondary: number;
    if (direction === "down") [primary, secondary] = [dr, dc];
    else if (direction === "up") [primary, secondary] = [-dr, dc];
    else if (direction === "right") [primary, secondary] = [dc, dr];
    else [primary, secondary] = [-dc, dr];
    if (primary <= 0) continue;
    const score = Math.abs(secondary) * 100 + primary;
    if (score < bestScore) {
      best = icon;
      bestScore = score;
    }
  }
  return best;
}

/** Icons in reading order for screen readers and Home/End: the fill order. */
export function readingOrder(layout: DesktopIcon[], order: FillOrder = "columns"): DesktopIcon[] {
  return [...layout].sort((a, b) => (order === "rows" ? a.row - b.row || a.col - b.col : a.col - b.col || a.row - b.row));
}
