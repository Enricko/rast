// AI Agent data shapes (`/api/ai/*`, see src/api/ai.rs) and the pure parts of
// the chat: applying streamed deltas, the text of a message, allowance text.

export interface ContentBlock {
  type: "text" | "reasoning";
  text: string;
}

export interface Message {
  id: number;
  conversationId: string;
  role: "user" | "assistant";
  content: ContentBlock[];
  providerId: string | null;
  model: string;
  inputTokens: number;
  outputTokens: number;
  status: "streaming" | "done" | "stopped" | "error";
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
  createdAt: number;
  updatedAt: number;
  running: boolean;
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
}

// ------------------------------------------------------------- pure logic

/** The visible text of a message (reasoning left out). */
export function messageText(message: Pick<Message, "content">): string {
  return message.content
    .filter((block) => block.type === "text")
    .map((block) => block.text)
    .join("");
}

export function messageReasoning(message: Pick<Message, "content">): string {
  return message.content
    .filter((block) => block.type === "reasoning")
    .map((block) => block.text)
    .join("");
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
  const reasoning = messageReasoning(message) + delta.reasoning;
  const text = messageText(message) + delta.text;
  message.content = [
    ...(reasoning ? [{ type: "reasoning" as const, text: reasoning }] : []),
    { type: "text" as const, text },
  ];
  detail.seq = delta.seq;
  return "applied";
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
