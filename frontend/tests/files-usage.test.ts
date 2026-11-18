import assert from "node:assert/strict";
import test from "node:test";

import { quotaBackendHint, shareLimitHint, usageChips } from "../src/apps/files/usage";

const base = { quotaBytes: 100, usedBytes: 50, trashBytes: 5 };

test("one-HOME usage is broken down by location, largest first", () => {
  const chips = usageChips({
    ...base,
    byType: { documents: 9 },
    byLocation: { files: 10, apps: 30, caches: 0, trash: 5, shared: 20, evil: 99 },
  });
  assert.deepEqual(chips, [
    ["Apps", 30],
    ["In shared folders", 20],
    ["Files", 10],
  ]);
});

test("legacy usage keeps the per-type chips", () => {
  const chips = usageChips({
    ...base,
    byType: { a: 1, b: 5, c: 3, d: 2, e: 4 },
    byLocation: { files: 50, trash: 5 },
  });
  assert.deepEqual(chips.map(([label]) => label), ["b", "e", "c", "d"]);
  assert.deepEqual(usageChips(base), []);
});

test("quota labels say what is limited", () => {
  assert.match(quotaBackendHint("filesystem"), /every app/);
  assert.match(quotaBackendHint("rast"), /not limited/);
  assert.match(shareLimitHint("filesystem, best effort"), /old group/);
  assert.match(shareLimitHint("RAST only"), /File Station/);
  assert.match(shareLimitHint(undefined), /File Station/);
});
