import assert from "node:assert/strict";
import test from "node:test";

import { HIDDEN_KEY, hiddenParam, readHiddenPreference } from "../src/apps/files/types";

function storage(values: Record<string, string>): Pick<Storage, "getItem"> {
  return { getItem: (key: string) => values[key] ?? null };
}

test("dot files stay hidden unless the person turned them on", () => {
  assert.equal(readHiddenPreference(storage({})), false);
  assert.equal(readHiddenPreference(storage({ [HIDDEN_KEY]: "0" })), false);
  assert.equal(readHiddenPreference(storage({ [HIDDEN_KEY]: "1" })), true);
  assert.equal(readHiddenPreference(null), false);
  const broken = {
    getItem: (): string | null => {
      throw new Error("storage disabled");
    },
  };
  assert.equal(readHiddenPreference(broken), false);
});

test("listings ask the server to leave dot files out by default", () => {
  assert.equal(hiddenParam(false), false);
  assert.equal(hiddenParam(true), undefined);
});
