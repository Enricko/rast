import assert from "node:assert/strict";
import test from "node:test";

import { backupWarnings } from "../src/apps/backup/warnings";

test("a finished backup lists what it left out", () => {
  assert.deepEqual(
    backupWarnings({ fileName: "a.tar.gz", warnings: ["users/homes/t-bob: skipped", 3, null] }),
    ["users/homes/t-bob: skipped"],
  );
  assert.deepEqual(backupWarnings({ fileName: "a.tar.gz" }), []);
  assert.deepEqual(backupWarnings({ warnings: "not a list" }), []);
  assert.deepEqual(backupWarnings(null), []);
  assert.deepEqual(backupWarnings("done"), []);
});
