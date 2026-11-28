// "Ask AI" hands AI Agent text: code and output go in a fence that their own
// backticks cannot close, and long output keeps its end.
import { test } from "node:test";
import assert from "node:assert/strict";
import { askAi, canAskAi, fenced, tailOf } from "../src/core/ask-ai";
import type { AppContext } from "../src/core/app";

test("fenced text keeps its own backticks inside", () => {
  assert.equal(fenced("ls -la\n"), "```\nls -la\n```");
  assert.equal(fenced("a ``` b", "md"), "````md\na ``` b\n````");
});

test("tailOf keeps the end, from a line start when it can", () => {
  assert.equal(tailOf("short", 10), "short");
  assert.equal(tailOf("first line\nsecond\nthird", 14), "second\nthird");
  assert.equal(tailOf("x".repeat(30), 10), "x".repeat(10));
});

test("AI Agent opens with the text, for accounts that may open it", () => {
  const opened: Array<[string, unknown]> = [];
  const ctx = (apps: string[]) =>
    ({
      desktop: { apps: () => apps.map((id) => ({ id })) },
      openApp: (id: string, params: unknown) => opened.push([id, params]),
    }) as unknown as AppContext;
  assert.equal(canAskAi(ctx(["files"])), false);
  assert.equal(canAskAi(ctx(["files", "agent"])), true);
  askAi(ctx(["agent"]), "About my file /a.txt: ");
  assert.deepEqual(opened, [["agent", { ask: "About my file /a.txt: " }]]);
});
