import assert from "node:assert/strict";
import test from "node:test";

import { relativeTime } from "../src/core/format";

test("relativeTime never says 0 min ago", () => {
  const now = Date.now();
  assert.equal(relativeTime(now - 10_000), "just now");
  assert.equal(relativeTime(now - 50_000), "1 min ago");
  assert.equal(relativeTime(now - 61_000), "1 min ago");
  assert.equal(relativeTime(now - 5 * 60_000 - 1_000), "5 min ago");
  assert.equal(relativeTime(now - 3 * 3_600_000), "3 h ago");
  assert.equal(relativeTime(null), "never");
});
