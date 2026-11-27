import assert from "node:assert/strict";
import test from "node:test";

import { agentHues } from "../src/apps/agent/marks";
import { parseInline, parseMarkdown, safeHref } from "../src/apps/agent/markdown";
import {
  accountState,
  resetIn,
  type ProviderAccount,
  addToStep,
  groupKinds,
  allowanceText,
  allowanceUsed,
  formatUsd,
  formatPrice,
  priceRules,
  planLabel,
  formatElapsed,
  replyActivity,
  applyDelta,
  groupByDay,
  maySkipAsking,
  messageText,
  toolArgument,
  chatArtifacts,
  artifactFileName,
  artifactSlug,
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
  assert.equal(toolArgument({ name: "browser", input: { action: "open", url: "https://x.io" } }), "open https://x.io");
  assert.equal(toolArgument({ name: "browser", input: { action: "click", ref: 12 } }), "click [12]");
  assert.equal(toolArgument({ name: "browser", input: { action: "type", ref: "3", text: "cats", submit: true } }), 'type "cats" into [3] + Enter');
  assert.equal(toolArgument({ name: "browser", input: { action: "scroll" } }), "scroll down");
  assert.equal(toolArgument({ name: "read_file", input: "not json" }), "not json");
  assert.equal(toolArgument({ name: "unknown", input: { a: 1 } }), '{"a":1}');
  assert.equal(maySkipAsking("write_file"), true);
  assert.equal(maySkipAsking("run_command"), true);
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
  assert.equal(formatPrice(3), "$3");
  assert.equal(formatPrice(0.3), "$0.30");
  assert.equal(formatPrice(0.625), "$0.625");
  assert.equal(formatPrice(0.0028), "$0.0028");
  assert.equal(formatPrice(0), "$0");
  assert.equal(formatPrice(12.5), "$12.50");
  assert.deepEqual(
    priceRules({
      input: 2,
      output: 6,
      multipliers: [
        { factor: 2, days: ["mon", "tue", "wed", "thu", "fri"], hours: [["01:00", "04:00"], ["06:00", "10:00"]] },
        { factor: 2, from: "2026-12-01" },
      ],
      tiers: [{ above: 200_000, input: 4, output: 12 }],
    }),
    ["×2 Mon–Fri 01:00–04:00, 06:00–10:00 UTC", "×2 from Dec 1, 2026", "Over 200k tokens: $4 in, $12 out"],
  );
});

test("chats group by day, week, month and year", () => {
  const now = new Date(2026, 10, 26, 15).getTime();
  const at = (days: number) => ({ id: String(days), userId: 1, agentId: null, providerId: null, model: "", title: "", createdAt: 0, updatedAt: now - days * 86_400_000, running: false });
  const groups = groupByDay([at(800), at(3), at(0), at(40), at(1), at(10), at(70), at(400), at(45)] as never, now);
  assert.deepEqual(
    groups.map((g) => [g.label, g.items.map((c) => c.id)]),
    [
      ["Today", ["0"]],
      ["Yesterday", ["1"]],
      ["Previous 7 days", ["3"]],
      ["Previous 30 days", ["10"]],
      ["October", ["40", "45"]],
      ["September", ["70"]],
      ["2025", ["400"]],
      ["2024", ["800"]],
    ],
  );
});

test("pinned chats come first, whatever their age", () => {
  const now = new Date(2026, 10, 26, 15).getTime();
  const at = (id: string, days: number, pinnedAt: number | null = null) =>
    ({ id, userId: 1, agentId: null, providerId: null, model: "", title: "", createdAt: 0, updatedAt: now - days * 86_400_000, pinnedAt, running: false }) as never;
  const groups = groupByDay([at("a", 0), at("old", 400, now), at("b", 2), at("week", 3, now - 5)], now);
  assert.deepEqual(
    groups.map((g: { label: string; items: Array<{ id: string }> }) => [g.label, g.items.map((c) => c.id)]),
    [
      ["Pinned", ["week", "old"]],
      ["Today", ["a"]],
      ["Previous 7 days", ["b"]],
    ],
  );
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
    kind("codex", "plan", "ChatGPT Codex", "Sign in with your ChatGPT plan"),
    kind("custom", "custom", "OpenAI-compatible"),
  ];
  assert.deepEqual(
    groupKinds(kinds, "").map((g) => [g.id, g.kinds.map((k) => k.id)]),
    [
      ["maker", ["anthropic"]],
      ["plan", ["codex"]],
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
  assert.deepEqual(
    groupKinds(kinds, "chatgpt").map((g) => g.id),
    ["plan"],
  );
  assert.equal(planLabel("plus"), "Plus");
  assert.equal(planLabel("PRO"), "Pro");
  assert.equal(planLabel("k12"), "K12");
  assert.equal(planLabel(""), "");
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
  // Only "Allow everything" runs commands without asking.
  assert.deepEqual(PERMISSION_MODES.filter((m) => m.tools.includes("run_command")).map((m) => m.id), ["all"]);
  assert.equal(permissionMode(PERMISSION_MODES[4]!.tools)?.id, "all");
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

test("a reply being written says what it does and for how long", () => {
  assert.equal(formatElapsed(-5), "0s");
  assert.equal(formatElapsed(4_900), "4s");
  assert.equal(formatElapsed(65_000), "1m 05s");
  assert.equal(formatElapsed(3_725_000), "1h 02m");
  const reply = (status: Message["status"], content: ContentBlock[]) => replyActivity({ status, content });
  assert.equal(reply("streaming", []), "Thinking");
  assert.equal(reply("streaming", [{ type: "text", text: "" }]), "Thinking");
  assert.equal(reply("streaming", [{ type: "text", text: "Hel" }]), "Writing");
  // Thinking that streams shows itself.
  assert.equal(reply("streaming", [{ type: "reasoning", text: "Plan" }]), null);
  const tool = (status: string) => ({ type: "tool", id: "c1", name: "list_files", input: {}, status }) as unknown as ContentBlock;
  assert.equal(reply("streaming", [tool("running")]), null);
  assert.equal(reply("streaming", [tool("done")]), "Reading the results");
  assert.equal(reply("done", []), null);
  assert.equal(reply("waiting", [tool("pending")]), null);
});

test("artifacts: versions per id, file names, slugs like the server", () => {
  const tool = (id: string, input: Record<string, unknown>, status = "done") => ({ type: "tool", id, name: "artifact", input, status }) as never;
  const messages = [
    { id: 1, content: [tool("c1", { id: "calc", title: "Calculator", kind: "html", content: "<p>1</p>" })] },
    { id: 2, content: [{ type: "text", text: "Here" }, tool("c2", { id: "notes", title: "Notes", kind: "markdown", content: "# N" })] },
    { id: 3, content: [tool("c3", { id: "calc", title: "Calculator", kind: "html", content: "<p>2</p>" }), tool("c4", { kind: "html", content: "x" }, "pending")] },
    { id: 4, content: [tool("c5", { title: "Sales report 2026!", kind: "code", language: "Python", content: "print(1)" })] },
  ] as never;
  const list = chatArtifacts(messages);
  assert.deepEqual(
    list.map((a) => [a.callId, a.key, a.version]),
    [
      ["c1", "calc", 1],
      ["c2", "notes", 1],
      ["c3", "calc", 2],
      ["c5", "sales-report-2026", 1],
    ],
  );
  assert.equal(artifactFileName(list[0]!), "calculator.html");
  assert.equal(artifactFileName(list[1]!), "notes.md");
  assert.equal(artifactFileName(list[3]!), "sales-report-2026.py");
  assert.equal(artifactFileName({ title: "x", kind: "code", language: "cobol" }), "x.txt");
  assert.equal(artifactSlug("Budget Calc"), "budget-calc");
  assert.equal(artifactSlug("--"), "artifact");
});

test("reset times read in hours and minutes", () => {
  assert.equal(resetIn(40_000), "in 40 s");
  assert.equal(resetIn(600_000), "in 10 min");
  assert.equal(resetIn(11_520_000), "in 3 h 12 min");
  assert.equal(resetIn(7_200_000), "in 2 h");
  assert.equal(resetIn(2 * 86_400_000 + 3_600_000), "in 2 d 1 h");
});

test("an account's state says whether it answers", () => {
  const account: ProviderAccount = {
    id: "a",
    label: "me@example.com",
    name: "Antigravity",
    first: true,
    signedIn: { email: "me@example.com", plan: "", expiresAt: 0 },
    hasKey: false,
    keyHint: "",
    ready: true,
    paused: false,
    resting: null,
    quota: {},
    position: 0,
  };
  assert.deepEqual(accountState(account, 1000, true), { label: "Ready", tone: "ok" });
  assert.deepEqual(accountState({ ...account, resting: { until: 1000 + 600_000, reason: "Usage limit reached" } }, 1000, true), {
    label: "Resting · back in 10 min",
    tone: "warn",
  });
  // A rest that is over: ready again.
  assert.equal(accountState({ ...account, resting: { until: 500, reason: "" } }, 1000, true).label, "Ready");
  assert.equal(accountState({ ...account, ready: false, signedIn: null }, 1000, true).label, "Sign in needed");
  assert.equal(accountState({ ...account, ready: false }, 1000, false).label, "Key needed");
  assert.deepEqual(accountState({ ...account, paused: true }, 1000, true), { label: "Set aside", tone: "off" });
});
