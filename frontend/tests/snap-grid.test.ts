import { test } from "node:test";
import assert from "node:assert/strict";
import {
  GRID,
  SAFE_INSET,
  WIDGET_GAP,
  defaultWidgetGroup,
  fitRect,
  nearestFreeRect,
  rectsIntersect,
  snap,
  snapUp,
  usableBounds,
} from "../src/shell/snap-grid.ts";

test("usable desktop excludes the safe inset and taskbar", () => {
  const bounds = usableBounds(1600, 900, 58);
  assert.deepEqual(bounds, { x: 16, y: 16, w: 1568, h: 810 });
  assert.equal(bounds.x % GRID, 0);
  assert.equal(bounds.h % GRID, 2); // height may be odd; positions are still snapped
});

test("snap rounds positions and sizes to the 8px grid", () => {
  assert.equal(snap(0), 0);
  assert.equal(snap(3), 0);
  assert.equal(snap(5), 8);
  assert.equal(snap(18), 16);
  assert.equal(snapUp(201), 208);
  assert.equal(snapUp(200), 200);
});

test("fitRect keeps a rectangle inside the bounds and on the grid", () => {
  const bounds = usableBounds(1000, 700, 58);
  const inside = fitRect({ x: 20, y: 24, w: 201, h: 300 }, bounds);
  assert.deepEqual(inside, { x: 24, y: 24, w: 208, h: 304 });
  const bottomRight = fitRect({ x: 5000, y: 5000, w: 208, h: 304 }, bounds);
  assert.ok(bottomRight.x + bottomRight.w <= bounds.x + bounds.w);
  assert.equal((bounds.x + bounds.w - (bottomRight.x + bottomRight.w)) % GRID, (bounds.w - bottomRight.w) % GRID);
  assert.ok(bottomRight.y + bottomRight.h <= bounds.y + bounds.h);
  assert.equal(bottomRight.x % GRID, 0);
  assert.equal(bottomRight.y % GRID, 0);
  const tooSmall = fitRect({ x: 0, y: 0, w: 20, h: 20 }, bounds);
  assert.equal(tooSmall.w, 200);
  assert.equal(tooSmall.h, 200);
});

test("nearestFreeRect avoids occupied cells and stays on the grid", () => {
  const bounds = usableBounds(800, 600, 58);
  const size = { w: 208, h: 208 };
  const blocked = { x: 16, y: 16, w: 208, h: 208 };
  const free = nearestFreeRect({ x: 16, y: 16, w: 208, h: 208 }, size, bounds, [blocked]);
  assert.ok(free);
  assert.equal(rectsIntersect(free!, blocked), false);
  assert.equal(free!.x % GRID, 0);
  assert.equal(free!.y % GRID, 0);
});

test("default widget group is right aligned with 16px gaps, two columns when possible", () => {
  const bounds = usableBounds(1600, 900, 58);
  const sizes = Array.from({ length: 4 }, () => ({ w: 208, h: 328 }));
  const rects = defaultWidgetGroup(sizes, bounds);
  assert.equal(rects.length, 4);
  const rightColumn = rects.filter((rect) => rect.x === bounds.x + bounds.w - 208);
  const leftColumn = rects.filter((rect) => rect.x === bounds.x + bounds.w - 208 * 2 - WIDGET_GAP);
  assert.equal(rightColumn.length, 2);
  assert.equal(leftColumn.length, 2);
  for (const column of [rightColumn, leftColumn]) {
    column.sort((a, b) => a.y - b.y);
    assert.equal(column[1].y - (column[0].y + column[0].h), WIDGET_GAP);
  }
  // one column on a narrow desktop
  const narrow = usableBounds(400, 900, 58);
  const single = defaultWidgetGroup(sizes, narrow);
  assert.ok(single.every((rect) => rect.x === narrow.x + narrow.w - 208));
});

test("default widget group keeps each widget's own preferred size", () => {
  const bounds = usableBounds(1600, 900, 58);
  const sizes = [
    { w: 232, h: 312 },
    { w: 232, h: 256 },
    { w: 224, h: 232 },
    { w: 224, h: 232 },
  ];
  const rects = defaultWidgetGroup(sizes, bounds);
  const heights = rects.map((rect) => rect.h);
  assert.deepEqual(heights, [312, 256, 232, 232]);
  assert.equal(rects[0].w, 232);
  assert.equal(rects[2].w, 224);
  // Smaller viewport: no widget adopts the tallest height.
  const narrow = usableBounds(500, 900, 58);
  const single = defaultWidgetGroup(sizes, narrow);
  assert.deepEqual(single.map((rect) => rect.h), [312, 256, 232, 232]);
});
