import { test } from "node:test";
import assert from "node:assert/strict";
import {
  autoScrollVelocity,
  boxesIntersect,
  dedupeDescendants,
  validMoveTarget,
  clampGroupDelta,
  groupCollides,
  marqueeBox,
  updateSelection,
} from "../src/shell/selection.ts";

test("marquee rectangle works in every drag direction", () => {
  assert.deepEqual(marqueeBox(100, 100, 40, 30), { x: 40, y: 30, w: 60, h: 70 });
  assert.deepEqual(marqueeBox(10, 20, 50, 60), { x: 10, y: 20, w: 40, h: 40 });
});

test("selection modifiers replace, add and toggle", () => {
  const current = new Set(["a", "b"]);
  assert.deepEqual([...updateSelection(current, ["c"], "replace")], ["c"]);
  assert.deepEqual([...updateSelection(current, ["c"], "add")].sort(), ["a", "b", "c"]);
  assert.deepEqual([...updateSelection(current, ["a", "c"], "toggle")].sort(), ["b", "c"]);
});

test("group delta is snapped and clamped to the usable desktop", () => {
  const bounds = { x: 16, y: 16, w: 1000, h: 600 };
  const rects = [
    { x: 100, y: 100, w: 104, h: 112 },
    { x: 300, y: 300, w: 208, h: 208 },
  ];
  assert.deepEqual(clampGroupDelta(rects, 13, 19, bounds), { dx: 16, dy: 16 });
  assert.deepEqual(clampGroupDelta(rects, 5000, 5000, bounds), { dx: 508, dy: 108 });
  assert.deepEqual(clampGroupDelta(rects, -5000, -5000, bounds), { dx: -84, dy: -84 });
});

test("collision helper detects overlapping obstacles only", () => {
  const a = { x: 0, y: 0, w: 10, h: 10 };
  assert.equal(boxesIntersect(a, { x: 5, y: 5, w: 10, h: 10 }), true);
  assert.equal(boxesIntersect(a, { x: 10, y: 0, w: 10, h: 10 }), false);
  assert.equal(groupCollides([a], [{ x: 5, y: 5, w: 5, h: 5 }]), true);
  assert.equal(groupCollides([a], [{ x: 20, y: 20, w: 5, h: 5 }]), false);
});

test("auto-scroll velocity grows near the pane edges", () => {
  assert.equal(autoScrollVelocity(150, 100, 200), 0);
  assert.ok(autoScrollVelocity(105, 100, 200) < 0);
  assert.ok(autoScrollVelocity(295, 100, 200) > 0);
});

test("bulk move helpers drop descendants and reject moving into itself", () => {
  assert.deepEqual(dedupeDescendants(["/a/b/c", "/a/b", "/x"]), ["/x", "/a/b"]);
  assert.equal(validMoveTarget(["/a/b", "/x"], "/dest"), true);
  assert.equal(validMoveTarget(["/a/b"], "/a/b"), false);
  assert.equal(validMoveTarget(["/a/b"], "/a/b/c"), false);
  assert.equal(validMoveTarget(["/a/b"], "/a"), true);
});
