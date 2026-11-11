import assert from "node:assert/strict";
import test from "node:test";

import { validateGib } from "../src/core/forms";

test("quota fields accept 0 as unlimited", () => {
  assert.equal(validateGib("0", { allowZero: true }), null);
  assert.equal(validateGib("25", { allowZero: true }), null);
  assert.equal(validateGib("1,5", { allowZero: true }), null);
  assert.match(validateGib("", { allowZero: true }) ?? "", /0 = unlimited/);
  assert.notEqual(validateGib("-1", { allowZero: true }), null);
  assert.notEqual(validateGib("abc", { allowZero: true }), null);
  // Fields that require a size still reject zero.
  assert.notEqual(validateGib("0"), null);
});
