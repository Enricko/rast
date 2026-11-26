// What a window keeps inside it: limits, and what is read back.
import { test } from "node:test";
import assert from "node:assert/strict";
import { MAX_FIELD_CHARS, MAX_VIEW_CHARS, boundedView, readableView } from "../src/shell/window-view";

test("a view is kept within its limits, dropping the longest texts first", () => {
  assert.equal(boundedView({ scroll: [], fields: [] }), undefined);
  const small = { field: "input|Search|0", value: "draft" };
  assert.deepEqual(boundedView({ fields: [small] }), { fields: [small] });
  // One field longer than a field may be: left out, the rest kept.
  const huge = { field: "textarea|Note|0", value: "x".repeat(MAX_FIELD_CHARS + 1) };
  assert.deepEqual(boundedView({ fields: [huge, small] }), { fields: [small] });
  // Several long ones: the longest go until the whole view fits.
  const long = (n: number, size: number) => ({ field: `f${n}`, value: "y".repeat(size) });
  const view = boundedView({ fields: [long(1, 15_000), long(2, 15_500), long(3, 15_900), long(4, 14_000), small] });
  assert.ok(view?.fields);
  assert.ok(JSON.stringify(view).length <= MAX_VIEW_CHARS);
  assert.deepEqual(
    view.fields.map((f) => f.field),
    ["f1", "f2", "f4", "input|Search|0"],
  );
  const scroll = { path: "div.app>div.list", x: 0, y: 900, ratio: 0.5, width: 800 };
  assert.deepEqual(boundedView({ scroll: [scroll] }), { scroll: [scroll] });
});

test("saved views from elsewhere are read defensively", () => {
  assert.deepEqual(readableView(undefined), { scroll: [], fields: [] });
  assert.deepEqual(readableView({ scroll: "x", fields: 3 }), { scroll: [], fields: [] });
  const good = { path: "div.app", x: 0, y: 10, ratio: 0.1, width: 300 };
  assert.deepEqual(
    readableView({
      scroll: [good, { path: "div.app", x: 0, y: Number.NaN, ratio: 0, width: 1 }, null],
      fields: [{ field: "input|Name|0", value: "Ann" }, { field: "f", value: 7 }, { value: "v" }],
    }),
    { scroll: [good], fields: [{ field: "input|Name|0", value: "Ann" }] },
  );
});
