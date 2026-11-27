import assert from "node:assert/strict";
import test from "node:test";

import { agentHues } from "../src/apps/agent/marks";
import { parseInline, parseMarkdown, safeHref } from "../src/apps/agent/markdown";
import {
  addToStep,
  groupKinds,
  allowanceText,
  allowanceUsed,
  formatUsd,
  applyDelta,
  groupByDay,
  maySkipAsking,
  messageText,
  toolArgument,
  upsertMessage,
  type ContentBlock,
  type Message,
  EFFORTS,
  effortLabel,
  PERMISSION_MODES,
  permissionMode,
} from "../src/apps/agent/model";

test("markdown blocks", () => {
  const blocks = parseMarkdown(
    [
      "# Title",
      "",
      "Some *text*",
      "on two lines.",
      "",
      "- one",
      "- two",
      "  - nested",
      "",
      "1. first",
      "2. second",
      "",
      "> quoted",
      "",
      "```ts",
      "const x = 1;",
      "```",
      "",
      "| a | b |",
      "|---|:-:|",
      "| 1 | 2 |",
      "",
      "---",
    ].join("\n"),
  );
  assert.deepEqual(
    blocks.map((b) => b.t),
    ["h", "p", "list", "list", "quote", "code", "table", "hr"],
  );
  const list = blocks[2];
  assert.ok(list?.t === "list" && !list.ordered && list.items.length === 2);
  assert.equal(list.items[1]?.children[0]?.t, "list");
  const ordered = blocks[3];
  assert.ok(ordered?.t === "list" && ordered.ordered && ordered.items.length === 2);
  const code = blocks[5];
  assert.deepEqual(code, { t: "code", lang: "ts", v: "const x = 1;", open: false });
  const table = blocks[6];
  assert.ok(table?.t === "table");
  assert.deepEqual(table.head, ["a", "b"]);
  assert.deepEqual(table.align, ["", "center"]);
  assert.deepEqual(table.rows, [["1", "2"]]);
});

test("an unclosed fence is open (a reply being written)", () => {
  const blocks = parseMarkdown("Here:\n```py\nprint(1)");
  assert.deepEqual(blocks[1], { t: "code", lang: "py", v: "print(1)", open: true });
});

test("markdown inline", () => {
  assert.deepEqual(parseInline("a **b** *c* `d` ~~e~~"), [
    { t: "text", v: "a " },
    { t: "strong", c: [{ t: "text", v: "b" }] },
    { t: "text", v: " " },
    { t: "em", c: [{ t: "text", v: "c" }] },
    { t: "text", v: " " },
    { t: "code", v: "d" },
    { t: "text", v: " " },
    { t: "del", c: [{ t: "text", v: "e" }] },
  ]);
  assert.deepEqual(parseInline("[docs](https://example.com/a)"), [
    { t: "link", href: "https://example.com/a", c: [{ t: "text", v: "docs" }] },
  ]);
  // Unsafe links keep their text only.
  assert.deepEqual(parseInline("[x](javascript:alert(1))"), [{ t: "text", v: "x" }]);
  assert.deepEqual(parseInline("see https://a.io/x."), [
    { t: "text", v: "see " },
    { t: "link", href: "https://a.io/x", c: [{ t: "text", v: "https://a.io/x" }] },
    { t: "text", v: "." },
  ]);
  assert.deepEqual(parseInline("snake_case_name and 2 * 3 * 4"), [{ t: "text", v: "snake_case_name and 2 * 3 * 4" }]);
  assert.deepEqual(parseInline("<b>hi</b>"), [{ t: "text", v: "<b>hi</b>" }]);
  assert.equal(safeHref("mailto:a@b.c"), "mailto:a@b.c");
  assert.equal(safeHref("data:text/html,x"), null);
});

function message(id: number, status: Message["status"], text = ""): Message {
  return {
    id,
    conversationId: "c",
    role: "assistant",
    content: [{ type: "text", text }],
    providerId: null,
    model: "m",
    inputTokens: 0,
    outputTokens: 0,
    status,
    error: "",
    createdAt: 0,
    finishedAt: null,
  };
}

test("deltas apply in order and report gaps", () => {
  const detail = { messages: [message(5, "streaming", "He")], seq: 1 };
  assert.equal(applyDelta(detail, { conversationId: "c", messageId: 5, seq: 1, text: "He", reasoning: "" }), "ignored");
  assert.equal(applyDelta(detail, { conversationId: "c", messageId: 5, seq: 2, text: "llo", reasoning: "hmm" }), "applied");
  assert.equal(messageText(detail.messages[0]!), "Hello");
  assert.equal(detail.messages[0]?.content[0]?.type, "reasoning");
  assert.equal(applyDelta(detail, { conversationId: "c", messageId: 5, seq: 4, text: "!", reasoning: "" }), "stale");
  assert.equal(applyDelta(detail, { conversationId: "c", messageId: 9, seq: 3, text: "!", reasoning: "" }), "stale");
  detail.messages[0]!.status = "done";
  assert.equal(applyDelta(detail, { conversationId: "c", messageId: 5, seq: 3, text: "!", reasoning: "" }), "ignored");
});

test("deltas fill the current step, as on the server", () => {
  // Same cases as src/ai/mod.rs deltas_fill_the_current_step.
  const blocks: ContentBlock[] = [];
  addToStep(blocks, "", "think");
  addToStep(blocks, "Hel", "");
  addToStep(blocks, "lo", " more");
  assert.deepEqual(blocks, [
    { type: "reasoning", text: "think more" },
    { type: "text", text: "Hello" },
  ]);
  blocks.push({ type: "tool", id: "c1", name: "list_files", input: { path: "/" }, status: "pending" });
  addToStep(blocks, "Found", "");
  addToStep(blocks, "", "again");
  assert.deepEqual(blocks.slice(2), [
    { type: "tool", id: "c1", name: "list_files", input: { path: "/" }, status: "pending" },
    { type: "reasoning", text: "again" },
    { type: "text", text: "Found" },
  ]);
  assert.equal(messageText({ content: blocks }), "HelloFound");
});

test("tool cards show their main argument", () => {
  assert.equal(toolArgument({ name: "run_command", input: { command: "ls -la" } }), "ls -la");
  assert.equal(toolArgument({ name: "fetch_url", input: { url: "https://x.io" } }), "https://x.io");
  assert.equal(toolArgument({ name: "read_file", input: "not json" }), "not json");
  assert.equal(toolArgument({ name: "unknown", input: { a: 1 } }), '{"a":1}');
  assert.equal(maySkipAsking("write_file"), true);
  assert.equal(maySkipAsking("run_command"), false);
  assert.equal(maySkipAsking("format_disk"), false);
});

test("messages stay in id order", () => {
  const list = [message(1, "done"), message(3, "done")];
  upsertMessage(list, message(2, "done"));
  upsertMessage(list, message(3, "stopped"));
  assert.deepEqual(list.map((m) => [m.id, m.status]), [[1, "done"], [2, "done"], [3, "stopped"]]);
});

test("allowance summary", () => {
  const base = { usedToday: 1500, usedMonth: 40_000, dayEndsAt: 0, monthEndsAt: 0, source: "user" };
  assert.equal(allowanceText({ ...base, dailyTokens: null, monthlyTokens: null }), null);
  assert.equal(allowanceUsed({ ...base, dailyTokens: null, monthlyTokens: null }), null);
  assert.equal(allowanceText({ ...base, dailyTokens: 10_000, monthlyTokens: null }), "1.5k of 10k tokens today");
  assert.equal(allowanceUsed({ ...base, dailyTokens: 10_000, monthlyTokens: 50_000 }), 0.8);
  assert.equal(allowanceUsed({ ...base, dailyTokens: 0, monthlyTokens: null }), 1);
  // The tightest limit is the one shown, money included.
  const week = { ...base, dailyTokens: 10_000, monthlyTokens: null, usedWeek: 9000, weeklyTokens: 10_000, costMonth: 4.75, monthlyCost: 5 };
  assert.equal(allowanceText(week), "$4.75 of $5.00 spent this month");
  assert.equal(allowanceUsed(week), 0.95);
  assert.equal(allowanceText({ ...week, monthlyCost: null }), "9.0k of 10k tokens this week");
  assert.equal(formatUsd(0), "$0");
  assert.equal(formatUsd(0.0042), "$0.0042");
  assert.equal(formatUsd(1234.5), "$1,235");
});

test("chats group by day", () => {
  const now = new Date(2026, 10, 26, 15).getTime();
  const at = (days: number) => ({ id: String(days), userId: 1, agentId: null, providerId: null, model: "", title: "", createdAt: 0, updatedAt: now - days * 86_400_000, running: false });
  const groups = groupByDay([at(0), at(1), at(3), at(30)], now);
  assert.deepEqual(groups.map((g) => g.label), ["Today", "Yesterday", "This week", "Older"]);
});

test("the provider picker groups kinds and finds them by name or what they offer", () => {
  const kind = (id: string, group: string, label: string, about = "") => ({
    id,
    label,
    group,
    about,
    defaultBaseUrl: "",
    needsKey: true,
    keyUrl: "",
  });
  const kinds = [
    kind("anthropic", "maker", "Anthropic", "Claude models"),
    kind("opencode", "gateway", "OpenCode Zen", "Claude, GPT and more"),
    kind("ollama", "local", "Ollama"),
    kind("custom", "custom", "OpenAI-compatible"),
  ];
  assert.deepEqual(
    groupKinds(kinds, "").map((g) => [g.id, g.kinds.map((k) => k.id)]),
    [
      ["maker", ["anthropic"]],
      ["gateway", ["opencode"]],
      ["local", ["ollama"]],
      ["custom", ["custom"]],
    ],
  );
  assert.deepEqual(
    groupKinds(kinds, "claude").map((g) => g.kinds.map((k) => k.id)),
    [["anthropic"], ["opencode"]],
  );
  assert.deepEqual(groupKinds(kinds, "nothing like it"), []);
});

test("an agent keeps its colours", () => {
  assert.deepEqual(agentHues("Writer"), agentHues(" writer "));
  assert.notDeepEqual(agentHues("Writer"), agentHues("Captain"));
  for (const hue of agentHues("Captain")) assert.ok(hue >= 0 && hue < 360);
});

test("a chat's allowed tools match a permission preset in any order", () => {
  assert.equal(permissionMode([])?.id, "ask");
  const reads = ["read_server_file", "search_files", "list_files", "read_file", "list_server_files"];
  assert.equal(permissionMode(reads)?.id, "read");
  assert.equal(permissionMode([...reads, "write_file", "create_folder"])?.id, "edit");
  // An older chat that allowed only the RAST file reads.
  assert.equal(permissionMode(["search_files", "list_files", "read_file"]), null);
  assert.equal(permissionMode(PERMISSION_MODES[3]!.tools)?.id, "act");
  // Allowed one by one ("Allow for this chat" on a single tool).
  assert.equal(permissionMode(["read_file"]), null);
  assert.ok(PERMISSION_MODES.every((m) => !m.tools.includes("run_command")));
});

test("effort levels are labelled, Auto for none", () => {
  assert.equal(effortLabel(""), "Auto");
  assert.equal(effortLabel(undefined), "Auto");
  assert.equal(effortLabel("max"), "Max");
  assert.deepEqual(
    EFFORTS.map((e) => e.id),
    ["", "low", "medium", "high", "max"],
  );
});
