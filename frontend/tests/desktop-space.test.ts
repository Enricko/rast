import assert from "node:assert/strict";
import test from "node:test";

import {
  arrangeWidgets,
  coveredCells,
  inflate,
  intersects,
  isFree,
  nearestFreeSpot,
  type Rect,
} from "../src/shell/desktop-space";
import { dropTarget, displayLayout, placeIcon } from "../src/shell/icon-layout";

const box = (left: number, top: number, width: number, height: number): Rect => ({ left, top, width, height });

test("touching boxes do not collide, overlapping ones do", () => {
  assert.equal(intersects(box(0, 0, 10, 10), box(10, 0, 10, 10)), false);
  assert.equal(intersects(box(0, 0, 10, 10), box(9, 9, 10, 10)), true);
  assert.deepEqual(inflate(box(10, 10, 20, 20), 5), box(5, 5, 30, 30));
});

test("a free spot stays where it is; a taken one moves to the nearest free spot", () => {
  const bounds = { width: 1000, height: 600 };
  const size = { width: 260, height: 200 };
  assert.deepEqual(nearestFreeSpot(size, { left: 700, top: 0 }, bounds, []), { left: 700, top: 0 });
  // Another widget at the top right: the nearest free spot is right below it.
  const other = inflate(box(740, 0, 260, 180), 12);
  const spot = nearestFreeSpot(size, { left: 740, top: 0 }, bounds, [other]);
  assert.deepEqual(spot, { left: 740, top: 192 });
  assert.ok(spot && isFree(spot, size, bounds, [other]));
});

test("a spot is never outside the desktop or over an obstacle, and null when nothing fits", () => {
  const bounds = { width: 600, height: 400 };
  const size = { width: 260, height: 200 };
  // Desired far outside: clamped into the desktop.
  assert.deepEqual(nearestFreeSpot(size, { left: 900, top: -50 }, bounds, []), { left: 340, top: 0 });
  // Icons fill the left column: the widget goes beside them.
  const icons = [box(0, 0, 110, 400)];
  assert.deepEqual(nearestFreeSpot(size, { left: 0, top: 0 }, bounds, icons), { left: 110, top: 0 });
  // A desktop too small for the widget, or completely taken.
  assert.equal(nearestFreeSpot(size, { left: 0, top: 0 }, { width: 200, height: 400 }, []), null);
  assert.equal(nearestFreeSpot(size, { left: 0, top: 0 }, bounds, [box(0, 0, 600, 400)]), null);
});

test("widgets are arranged without covering icons or each other; the rest are hidden", () => {
  const bounds = { width: 700, height: 480 };
  const icons = [inflate(box(0, 0, 104, 432), 6)];
  const size = { width: 260, height: 220 };
  const placed = arrangeWidgets(
    [
      { id: "network", size, desired: { left: 440, top: 0 } },
      { id: "health", size, desired: { left: 440, top: 0 } },
      { id: "cpu", size, desired: { left: 440, top: 260 } },
      { id: "storage", size, desired: { left: 440, top: 0 } },
      { id: "extra", size, desired: { left: 440, top: 0 } },
    ],
    bounds,
    icons,
    12,
  );
  const shown = [...placed.entries()].filter(([, at]) => at !== null);
  // Room for a 2 x 2 block of widgets right of the icons; the fifth has none.
  assert.equal(shown.length, 4);
  assert.equal(placed.get("extra"), null);
  assert.deepEqual(placed.get("network"), { left: 440, top: 0 }, "first in priority keeps its place");
  const rects = shown.map(([, at]) => box(at!.left, at!.top, size.width, size.height));
  for (const [i, a] of rects.entries()) {
    assert.ok(!intersects(a, icons[0]!), "never over the icons");
    assert.ok(a.left >= 0 && a.top >= 0 && a.left + a.width <= bounds.width && a.top + a.height <= bounds.height);
    for (const b of rects.slice(i + 1)) assert.ok(!intersects(inflate(a, 12), b), "never over (or touching) another widget");
  }
});

test("a widget whose saved place is taken stays where it is shown while that is free", () => {
  const bounds = { width: 1000, height: 600 };
  const size = { width: 260, height: 200 };
  const placed = arrangeWidgets(
    [
      { id: "a", size, desired: { left: 740, top: 0 } },
      { id: "b", size, desired: { left: 740, top: 0 }, current: { left: 100, top: 300 } },
    ],
    bounds,
    [],
    12,
  );
  assert.deepEqual(placed.get("b"), { left: 100, top: 300 });
  // Once its own place is free again it goes back there.
  const alone = arrangeWidgets([{ id: "b", size, desired: { left: 740, top: 0 }, current: { left: 100, top: 300 } }], bounds, [], 12);
  assert.deepEqual(alone.get("b"), { left: 740, top: 0 });
});

test("cells under a widget are blocked for icons", () => {
  const cell = { w: 100, h: 100 };
  const grid = { cols: 6, rows: 4 };
  const covered = coveredCells([box(310, 0, 260, 150)], cell, grid);
  assert.deepEqual([...covered].sort(), ["3,0", "3,1", "4,0", "4,1", "5,0", "5,1"]);
  // Touching a cell's edge does not cover it.
  assert.deepEqual([...coveredCells([box(100, 100, 100, 100)], cell, grid)], ["1,1"]);
});

test("icons dropped on a widget land in the nearest free cell; other drops are unchanged", () => {
  const grid = { cols: 4, rows: 3 };
  const blocked = new Set(["2,0", "3,0", "2,1", "3,1"]);
  const layout = [
    { id: "files", col: 0, row: 0 },
    { id: "monitor", col: 1, row: 0 },
  ];
  assert.deepEqual(dropTarget(layout, "files", { col: 3, row: 0 }, grid, blocked), { col: 3, row: 2 });
  assert.deepEqual(dropTarget(layout, "files", { col: 1, row: 0 }, grid, blocked), { col: 1, row: 0 }, "onto an icon: swap");
  assert.deepEqual(dropTarget(layout, "files", { col: 2, row: 0 }, grid, blocked), { col: 1, row: 1 });
  assert.equal(dropTarget(layout, "files", { col: 9, row: 0 }, grid, blocked), null);
  // New apps avoid widgets too, unless only covered cells are left.
  assert.deepEqual(placeIcon(layout, "logs", { col: 3, row: 1 }, grid, "columns", blocked)?.at(-1), { id: "logs", col: 3, row: 2 });
  assert.deepEqual(placeIcon(layout, "logs", null, grid, "columns", blocked)?.at(-1), { id: "logs", col: 0, row: 1 });
  const nearlyFull = new Set(["0,1", "0,2", "1,1", "1,2", "2,0", "2,1", "2,2", "3,0", "3,1", "3,2"]);
  assert.deepEqual(placeIcon(layout, "logs", null, grid, "columns", nearlyFull)?.at(-1), { id: "logs", col: 0, row: 1 });
  // An icon whose saved cell is off this (smaller) screen skips covered cells.
  const shown = displayLayout([{ id: "files", col: 9, row: 0 }], ["files"], grid, "columns", [], new Set(["0,0", "0,1"]));
  assert.deepEqual(shown, [{ id: "files", col: 0, row: 2 }]);
});
