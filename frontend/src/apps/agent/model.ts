// AI Agent data shapes (`/api/ai/*`, see src/api/ai.rs) and the pure parts of
// the chat: applying streamed deltas, the text of a message, allowance text.

export interface WordsBlock {
  type: "text" | "reasoning";
  text: string;
}

export type ToolStatus = "pending" | "allowed" | "running" | "done" | "error" | "denied" | "skipped";

/** A call the agent asked for (see src/ai/tools.rs). */
export interface ToolBlock {
  type: "tool";
  id: string;
  name: string;
  input: Record<string, unknown> | string;
  status: ToolStatus;
  output?: string;
  error?: string;
}

export type ContentBlock = WordsBlock | ToolBlock;

export interface Message {
  id: number;
  conversationId: string;
  role: "user" | "assistant";
  content: ContentBlock[];
  providerId: string | null;
  model: string;
  inputTokens: number;
  outputTokens: number;
  status: "streaming" | "waiting" | "done" | "stopped" | "error";
  error: string;
  createdAt: number;
  finishedAt: number | null;
}

export interface Conversation {
  id: string;
  userId: number;
  agentId: string | null;
  providerId: string | null;
  model: string;
  title: string;
  /** Tools that run without asking in this chat. */
  allowedTools: string[];
  createdAt: number;
  updatedAt: number;
  running: boolean;
  /** Its last reply waits for the owner to answer a tool request. */
  waiting: boolean;
  owner?: string;
  messageCount?: number;
}

export interface ConversationDetail {
  conversation: Conversation;
  messages: Message[];
  /** Deltas of the running reply already included in `messages`. */
  seq: number;
  readOnly: boolean;
}

export interface PublicProvider {
  id: string;
  kind: string;
  kindLabel: string;
  name: string;
  models: string[];
}

export interface PublicAgent {
  id: string;
  name: string;
  description: string;
  providerId: string | null;
  model: string;
}

export interface Allowance {
  usedToday: number;
  usedMonth: number;
  dailyTokens: number | null;
  monthlyTokens: number | null;
  dayEndsAt: number;
  monthEndsAt: number;
  source: string;
}

export interface Options {
  isAdmin: boolean;
  configured: boolean;
  providers: PublicProvider[];
  agents: PublicAgent[];
  allowance: Allowance;
}

export interface DeltaEvent {
  conversationId: string;
  messageId: number;
  seq: number;
  text: string;
  reasoning: string;
}

export interface MessageEvent {
  conversationId: string;
  message: Message;
  running: boolean;
  /** Deltas of the running reply already in `message`. */
  seq?: number;
}

export interface ConversationEvent {
  conversation: Conversation;
  deleted?: boolean;
}

// ------------------------------------------------------------ admin shapes

export interface ProviderKind {
  id: string;
  label: string;
  defaultBaseUrl: string;
  needsKey: boolean;
  /** How the picker groups it: maker, gateway, fast, local or custom. */
  group: string;
  /** Where to get a key ("" none). */
  keyUrl: string;
  about: string;
}

/** The provider picker's groups, in order. */
export const KIND_GROUPS: Array<{ id: string; label: string }> = [
  { id: "maker", label: "Model makers" },
  { id: "gateway", label: "Gateways and subscriptions" },
  { id: "fast", label: "Fast open models" },
  { id: "local", label: "On your network" },
  { id: "custom", label: "Any other endpoint" },
];

/** Kinds matching a search, by group (empty groups left out). */
export function groupKinds(kinds: ProviderKind[], query: string): Array<{ id: string; label: string; kinds: ProviderKind[] }> {
  const q = query.trim().toLowerCase();
  const match = (k: ProviderKind) => !q || `${k.label} ${k.about} ${k.id}`.toLowerCase().includes(q);
  return KIND_GROUPS.map((g) => ({ ...g, kinds: kinds.filter((k) => k.group === g.id && match(k)) })).filter((g) => g.kinds.length);
}

export interface AdminProvider extends PublicProvider {
  baseUrl: string;
  effectiveBaseUrl: string;
  hasKey: boolean;
  keyHint: string;
  enabled: boolean;
  createdAt: number;
  updatedAt: number;
}

export interface AdminAgent {
  id: string;
  name: string;
  description: string;
  systemPrompt: string;
  providerId: string | null;
  model: string;
  skills: string[];
  /** Tools it may use (null: all). */
  tools: string[] | null;
  temperature: number | null;
  maxOutputTokens: number | null;
  enabled: boolean;
  createdAt: number;
  updatedAt: number;
}

export interface Skill {
  id: string;
  name: string;
  description: string;
  instructions: string;
  createdAt: number;
  updatedAt: number;
}

export interface LimitRule {
  subject: string;
  providers: string[] | null;
  dailyTokens: number | null;
  monthlyTokens: number | null;
}

export interface Overview {
  kinds: ProviderKind[];
  providers: AdminProvider[];
  agents: AdminAgent[];
  skills: Skill[];
  limits: LimitRule[];
  users: Array<{ id: number; username: string; displayName: string; isAdmin: boolean; active: boolean }>;
  groups: Array<{ id: number; name: string }>;
  usage: Array<{ userId: number; today: number; month: number }>;
  tools: Array<{ name: string; label: string }>;
  search: SearchSettings;
}

export interface SearchSettings {
  kind: "" | "brave" | "tavily" | "searxng";
  url: string;
  hasKey: boolean;
  keyHint: string;
  configured: boolean;
}

// ------------------------------------------------------------- pure logic

/** The visible text of a message (reasoning and tools left out). */
export function messageText(message: Pick<Message, "content">): string {
  return message.content
    .filter((block): block is WordsBlock => block.type === "text")
    .map((block) => block.text)
    .join("");
}

export function messageReasoning(message: Pick<Message, "content">): string {
  return message.content
    .filter((block): block is WordsBlock => block.type === "reasoning")
    .map((block) => block.text)
    .join("");
}

export function toolBlocks(message: Pick<Message, "content">): ToolBlock[] {
  return message.content.filter((block): block is ToolBlock => block.type === "tool");
}

/**
 * Add streamed text to a reply's current step (after its last tool block),
 * exactly as the server does (src/ai/mod.rs `apply_delta`): reasoning into
 * the step's reasoning block (made at the step's start), text into the
 * text block that ends it (or a new one).
 */
export function addToStep(blocks: ContentBlock[], text: string, reasoning: string): void {
  let start = 0;
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i]?.type === "tool") {
      start = i + 1;
      break;
    }
  }
  if (reasoning) {
    const at = blocks.slice(start).findIndex((b) => b.type === "reasoning");
    if (at >= 0) (blocks[start + at] as WordsBlock).text += reasoning;
    else blocks.splice(start, 0, { type: "reasoning", text: reasoning });
  }
  if (text) {
    const last = blocks[blocks.length - 1];
    if (blocks.length > start && last?.type === "text") last.text += text;
    else blocks.push({ type: "text", text });
  }
}

/**
 * Apply a streamed delta to a conversation's messages. Returns `stale` when
 * deltas were missed (the page should load the conversation again), `ignored`
 * when the delta is one the page already has, `applied` otherwise.
 */
export function applyDelta(
  detail: Pick<ConversationDetail, "messages" | "seq">,
  delta: DeltaEvent,
): "applied" | "ignored" | "stale" {
  const message = detail.messages.find((m) => m.id === delta.messageId);
  if (!message || message.status !== "streaming") return message ? "ignored" : "stale";
  if (delta.seq <= detail.seq) return "ignored";
  if (delta.seq !== detail.seq + 1) return "stale";
  addToStep(message.content, delta.text, delta.reasoning);
  detail.seq = delta.seq;
  return "applied";
}

/** Tools by name: label and the argument a card shows. */
export const TOOL_INFO: Record<string, { label: string; doing: string; arg: string; icon: string }> = {
  list_files: { label: "List a folder", doing: "Listing", arg: "path", icon: "folder" },
  read_file: { label: "Read a file", doing: "Reading", arg: "path", icon: "file" },
  search_files: { label: "Find files", doing: "Finding", arg: "query", icon: "search" },
  write_file: { label: "Write a file", doing: "Writing", arg: "path", icon: "edit" },
  create_folder: { label: "Create a folder", doing: "Creating", arg: "path", icon: "folder" },
  run_command: { label: "Run a command", doing: "Running", arg: "command", icon: "terminal" },
  web_search: { label: "Search the web", doing: "Searching", arg: "query", icon: "network" },
  fetch_url: { label: "Open a web page", doing: "Opening", arg: "url", icon: "external" },
};

/** The argument a tool card shows (its path, command, query or address). */
export function toolArgument(block: Pick<ToolBlock, "name" | "input">): string {
  const key = TOOL_INFO[block.name]?.arg ?? "path";
  if (typeof block.input === "string") return block.input;
  const value = block.input?.[key];
  return typeof value === "string" ? value : JSON.stringify(block.input ?? {});
}

/** Whether "Allow for this chat" is offered (never for the shell). */
export function maySkipAsking(name: string): boolean {
  return name in TOOL_INFO && name !== "run_command";
}

/** Insert or replace a message, keeping id order. */
export function upsertMessage(messages: Message[], message: Message): void {
  const index = messages.findIndex((m) => m.id === message.id);
  if (index >= 0) {
    messages[index] = message;
    return;
  }
  const after = messages.findIndex((m) => m.id > message.id);
  if (after < 0) messages.push(message);
  else messages.splice(after, 0, message);
}

export function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

/** "12k of 50k today" style summary, or null when unlimited. */
export function allowanceText(allowance: Allowance): string | null {
  const parts: string[] = [];
  if (allowance.dailyTokens !== null) {
    parts.push(`${formatTokens(allowance.usedToday)} of ${formatTokens(allowance.dailyTokens)} tokens today`);
  }
  if (allowance.monthlyTokens !== null) {
    parts.push(`${formatTokens(allowance.usedMonth)} of ${formatTokens(allowance.monthlyTokens)} this month`);
  }
  return parts.length ? parts.join(" · ") : null;
}

/** Share of the tightest allowance used (0–1), or null when unlimited. */
export function allowanceUsed(allowance: Allowance): number | null {
  const shares: number[] = [];
  if (allowance.dailyTokens !== null) shares.push(allowance.dailyTokens > 0 ? allowance.usedToday / allowance.dailyTokens : 1);
  if (allowance.monthlyTokens !== null) shares.push(allowance.monthlyTokens > 0 ? allowance.usedMonth / allowance.monthlyTokens : 1);
  return shares.length ? Math.min(1, Math.max(...shares)) : null;
}

/** Group conversations by when they were last used. */
export function groupByDay(items: Conversation[], now = Date.now()): Array<{ label: string; items: Conversation[] }> {
  const start = new Date(now);
  start.setHours(0, 0, 0, 0);
  const today = start.getTime();
  const yesterday = today - 86_400_000;
  const week = today - 6 * 86_400_000;
  const groups = new Map<string, Conversation[]>();
  for (const item of items) {
    const label = item.updatedAt >= today ? "Today" : item.updatedAt >= yesterday ? "Yesterday" : item.updatedAt >= week ? "This week" : "Older";
    const list = groups.get(label) ?? [];
    list.push(item);
    groups.set(label, list);
  }
  return ["Today", "Yesterday", "This week", "Older"]
    .filter((label) => groups.has(label))
    .map((label) => ({ label, items: groups.get(label) ?? [] }));
}
