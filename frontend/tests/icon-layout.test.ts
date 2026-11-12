import assert from "node:assert/strict";
import test from "node:test";

import {
  defaultLayout,
  displayLayout,
  firstFreeCell,
  layoutForSave,
  moveIcon,
  nearestFreeCell,
  neighbour,
  placeIcon,
  readingOrder,
  removeIcon,
} from "../src/shell/icon-layout";

const grid = { cols: 4, rows: 3 };
const ids = ["launcher", "files", "monitor", "packages", "controlpanel"];

test("the default layout fills each column top to bottom", () => {
  assert.deepEqual(defaultLayout(ids, grid), [
    { id: "launcher", col: 0, row: 0 },
    { id: "files", col: 0, row: 1 },
    { id: "monitor", col: 0, row: 2 },
    { id: "packages", col: 1, row: 0 },
    { id: "controlpanel", col: 1, row: 1 },
  ]);
});

test("phones fill row by row", () => {
  assert.deepEqual(defaultLayout(ids.slice(0, 3), { cols: 2, rows: 5 }, "rows"), [
    { id: "launcher", col: 0, row: 0 },
    { id: "files", col: 1, row: 0 },
    { id: "monitor", col: 0, row: 1 },
  ]);
  assert.deepEqual(firstFreeCell([{ id: "a", col: 0, row: 0 }], { cols: 2, rows: 2 }, "rows"), { col: 1, row: 0 });
  assert.deepEqual(displayLayout([{ id: "files", col: 7, row: 0 }, { id: "logs", col: 0, row: 0 }], ["files", "logs"], { cols: 2, rows: 5 }, "rows"), [
    { id: "logs", col: 0, row: 0 },
    { id: "files", col: 1, row: 0 },
  ]);
});

test("the saved layout is shown for visible apps, off-screen icons find a free cell", () => {
  const saved = [
    { id: "files", col: 0, row: 0 },
    { id: "backup", col: 1, row: 0 },
    { id: "logs", col: 9, row: 9 },
  ];
  assert.deepEqual(displayLayout(saved, ["files", "logs"], grid), [
    { id: "files", col: 0, row: 0 },
    { id: "logs", col: 0, row: 1 },
  ]);
  assert.deepEqual(displayLayout(null, ["files"], grid), [{ id: "files", col: 0, row: 0 }]);
  assert.deepEqual(displayLayout([], ids, grid), [], "a cleared desktop stays clear");
  // A shifted icon stays where it is shown while that cell is free.
  const previous = [{ id: "logs", col: 2, row: 2 }];
  assert.deepEqual(displayLayout(saved, ["files", "logs"], grid, "columns", previous).at(-1), { id: "logs", col: 2, row: 2 });
  assert.deepEqual(
    displayLayout(saved, ["files", "logs"], grid, "columns", [{ id: "logs", col: 0, row: 0 }]).at(-1),
    { id: "logs", col: 0, row: 1 },
    "its old cell is taken now",
  );
});

test("moving onto another icon swaps the two", () => {
  const layout = defaultLayout(ids.slice(0, 3), grid);
  assert.deepEqual(moveIcon(layout, "launcher", { col: 2, row: 1 }), [
    { id: "launcher", col: 2, row: 1 },
    { id: "files", col: 0, row: 1 },
    { id: "monitor", col: 0, row: 2 },
  ]);
  assert.deepEqual(moveIcon(layout, "launcher", { col: 0, row: 2 }), [
    { id: "launcher", col: 0, row: 2 },
    { id: "files", col: 0, row: 1 },
    { id: "monitor", col: 0, row: 0 },
  ]);
  assert.equal(moveIcon(layout, "launcher", { col: 0, row: 0 }), layout, "same cell: unchanged");
});

test("placing finds the requested or nearest free cell, and refuses a full grid", () => {
  const layout = [{ id: "files", col: 1, row: 1 }];
  assert.deepEqual(placeIcon(layout, "logs", { col: 1, row: 1 }, grid)?.at(-1), { id: "logs", col: 0, row: 1 });
  assert.deepEqual(placeIcon(layout, "logs", null, grid)?.at(-1), { id: "logs", col: 0, row: 0 });
  assert.deepEqual(placeIcon(layout, "files", { col: 3, row: 2 }, grid), [{ id: "files", col: 3, row: 2 }], "already there: moves");
  const full = defaultLayout(["a", "b", "c", "d"], { cols: 2, rows: 2 });
  assert.equal(placeIcon(full, "e", null, { cols: 2, rows: 2 }), null);
  assert.equal(firstFreeCell(full, { cols: 2, rows: 2 }), null);
  assert.deepEqual(nearestFreeCell(layout, { col: 1, row: 1 }, { cols: 2, rows: 2 }), { col: 0, row: 1 });
  assert.deepEqual(removeIcon(full, "b").map((i) => i.id), ["a", "c", "d"]);
});

test("saving keeps hidden apps' icons unless their cell was reused", () => {
  const saved = [
    { id: "files", col: 0, row: 0 },
    { id: "backup", col: 1, row: 0 },
    { id: "applications", col: 2, row: 0 },
  ];
  const shown = [
    { id: "files", col: 1, row: 0 },
    { id: "logs", col: 0, row: 0 },
  ];
  assert.deepEqual(layoutForSave(shown, saved, ["files", "logs"]), [
    { id: "files", col: 1, row: 0 },
    { id: "logs", col: 0, row: 0 },
    { id: "applications", col: 2, row: 0 },
  ]);
  assert.deepEqual(layoutForSave(shown, null, ["files", "logs"]), shown);
});

test("an icon only shifted by a smaller screen keeps its saved cell", () => {
  const small = { cols: 2, rows: 2 };
  const saved = [
    { id: "files", col: 0, row: 0 },
    { id: "logs", col: 6, row: 1 },
    { id: "monitor", col: 1, row: 1 },
  ];
  const before = displayLayout(saved, ["files", "logs", "monitor"], small);
  assert.deepEqual(before.find((i) => i.id === "logs"), { id: "logs", col: 0, row: 1 });
  // Moving File Station keeps Activity Logs where the big screen has it.
  const next = moveIcon(before, "files", { col: 1, row: 0 });
  assert.deepEqual(layoutForSave(next, saved, ["files", "logs", "monitor"], before), [
    { id: "files", col: 1, row: 0 },
    { id: "monitor", col: 1, row: 1 },
    { id: "logs", col: 6, row: 1 },
  ]);
  // Swapping with the shifted icon moves it for real.
  const swapped = moveIcon(before, "files", { col: 0, row: 1 });
  assert.deepEqual(layoutForSave(swapped, saved, ["files", "logs", "monitor"], before), [
    { id: "files", col: 0, row: 1 },
    { id: "monitor", col: 1, row: 1 },
    { id: "logs", col: 0, row: 0 },
  ]);
});

test("arrow keys go along the same line first, then to the closest icon", () => {
  const layout = [
    { id: "a", col: 0, row: 0 },
    { id: "b", col: 0, row: 2 },
    { id: "c", col: 2, row: 1 },
    { id: "d", col: 3, row: 0 },
  ];
  assert.equal(neighbour(layout, { col: 0, row: 0 }, "down")?.id, "b");
  assert.equal(neighbour(layout, { col: 0, row: 0 }, "right")?.id, "d", "same row first");
  assert.equal(neighbour(layout, { col: 0, row: 2 }, "right")?.id, "c");
  assert.equal(neighbour(layout, { col: 2, row: 1 }, "right")?.id, "d");
  assert.equal(neighbour(layout, { col: 0, row: 0 }, "up"), null);
  assert.deepEqual(readingOrder(layout).map((i) => i.id), ["a", "b", "c", "d"]);
  assert.deepEqual(readingOrder(layout, "rows").map((i) => i.id), ["a", "d", "c", "b"]);
});
