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
  /** The browser's screenshot of the page after the step (a file of the chat). */
  image?: Omit<FileBlock, "type">;
}

/** A file sent with a message (see src/ai/files.rs). */
export interface FileBlock {
  type: "file";
  id: string;
  name: string;
  mime: string;
  kind: "image" | "pdf" | "text";
  size: number;
}

export type ContentBlock = WordsBlock | ToolBlock | FileBlock;

/** The largest file and how many files go with one message. */
export const MAX_FILE_BYTES = 10 * 1024 * 1024;
export const MAX_FILES = 10;

export function messageFiles(message: Pick<Message, "content">): FileBlock[] {
  return message.content.filter((block): block is FileBlock => block.type === "file");
}

export function fileUrl(id: string): string {
  return `/api/ai/files/${encodeURIComponent(id)}`;
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
  /** What the reply is worth at API prices, USD (0: unknown or none). */
  estimatedCost?: number;
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
  /** How hard the model thinks ("" = the provider's default). */
  effort?: Effort;
  /** When its owner pinned it to the top of the list (null: not pinned). */
  pinnedAt?: number | null;
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
  usedWeek?: number;
  usedMonth: number;
  /** Money spent (USD). */
  costToday?: number;
  costWeek?: number;
  costMonth?: number;
  dailyTokens: number | null;
  weeklyTokens?: number | null;
  monthlyTokens: number | null;
  /** Spending limits (USD). */
  dailyCost?: number | null;
  weeklyCost?: number | null;
  monthlyCost?: number | null;
  dayEndsAt: number;
  weekEndsAt?: number;
  monthEndsAt: number;
  source: string;
}

/** A model's price in USD per million tokens. Cached input (read back from
 * the provider's prompt cache) and input written to the cache cost the input
 * price unless given. */
export interface Price {
  input: number;
  output: number;
  cached?: number;
  cacheWrite?: number;
  /** Other prices for prompts of more than `above` input tokens. */
  tiers?: PriceTier[];
  /** Factors at certain times (UTC), e.g. peak hours. */
  multipliers?: PriceMultiplier[];
}

export interface PriceTier {
  above: number;
  input: number;
  output: number;
  cached?: number;
  cacheWrite?: number;
}

export interface PriceMultiplier {
  factor: number;
  /** "mon" … "sun"; none: every day. */
  days?: string[];
  /** ["01:00", "04:00"]; none: all day. */
  hours?: Array<[string, string]>;
  /** YYYY-MM-DD, from (inclusive) and until (exclusive). */
  from?: string;
  until?: string;
}

const WEEK = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

function dayNames(days: string[]): string {
  const idx = days.map((d) => WEEK.indexOf(d.toLowerCase())).filter((i) => i >= 0).sort((a, b) => a - b);
  const name = (i: number) => WEEK[i]![0]!.toUpperCase() + WEEK[i]!.slice(1);
  const consecutive = idx.length > 2 && idx.every((d, i) => i === 0 || d === idx[i - 1]! + 1);
  return consecutive ? `${name(idx[0]!)}–${name(idx[idx.length - 1]!)}` : idx.map(name).join(", ");
}

function shortDate(date: string): string {
  const d = new Date(`${date}T00:00:00Z`);
  return Number.isNaN(d.getTime()) ? date : d.toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" });
}

/** A price's time and length rules in words: "×2 Mon–Fri 01:00–04:00
 * UTC", "Over 200k tokens: $4 in, $12 out". */
export function priceRules(price: Price): string[] {
  const out: string[] = [];
  for (const m of price.multipliers ?? []) {
    const parts = [`×${Number(m.factor.toFixed(2))}`];
    if (m.days?.length) parts.push(dayNames(m.days));
    if (m.hours?.length) parts.push(`${m.hours.map(([a, b]) => `${a}–${b}`).join(", ")} UTC`);
    if (m.from) parts.push(`from ${shortDate(m.from)}`);
    if (m.until) parts.push(`until ${shortDate(m.until)}`);
    out.push(parts.join(" "));
  }
  for (const t of price.tiers ?? []) {
    out.push(`Over ${formatTokens(t.above)} tokens: ${formatPrice(t.input)} in, ${formatPrice(t.output)} out`);
  }
  return out;
}

export type Prices = Record<string, Price>;

/** Where a price comes from: set by an administrator, the provider's own
 * model list, the built-in list, free because the provider runs locally or
 * because a signed-in plan pays for it, or unknown (free). */
export type PriceSource = "set" | "list" | "built-in" | "local" | "plan" | "none";

export interface PricedModel {
  model: string;
  price: Price | null;
  source: PriceSource;
  /** The built-in entry that matched (a name or a pattern). */
  match: string | null;
  /** With a price set: what the model costs without it. */
  fallback?: { price: Price | null; source: PriceSource; match: string | null };
}

export interface BuiltInPrice {
  name?: string;
  pattern?: string;
  input: number;
  output: number;
  cached: number;
  cacheWrite: number;
}

/** GET /api/ai/prices. */
export interface PriceListing {
  providers: Array<{
    id: string;
    name: string;
    kind: string;
    kindLabel: string;
    enabled: boolean;
    local: boolean;
    checkedAt: number | null;
    models: PricedModel[];
  }>;
  builtIn: { models: BuiltInPrice[]; patterns: BuiltInPrice[] };
  results?: Array<{ id: string; name: string; priced?: number; error?: string }>;
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
  /** How the picker groups it: maker, plan, gateway, fast, local or custom. */
  group: string;
  /** Where to get a key ("" none). */
  keyUrl: string;
  about: string;
  /** Added only by signing in to a plan: no API key (group "plan"). */
  signsIn: boolean;
  /** How signing in to a plan works ("" for the API-key kinds). */
  signInFlow: SignInFlow;
  /** Plan kinds, browser flows: whose page asks to allow RAST ("OpenAI",
   * "Claude", "Google", …). */
  site: string;
  /** Plan kinds, browser flows: what the address to paste looks like. */
  callbackHint: string;
  /** Plan kinds: what using a plan outside its own app may cost ("" none). */
  risk: string;
}

/** How a plan is signed in to: a page to allow in a browser, with the
 * address it ends on pasted back, or a code entered on the provider's page
 * while this page waits. */
export type SignInFlow = "browser" | "device" | "";

/** POST /api/ai/providers/sign-in: a page to open, or a code to enter. */
export interface SignInStarted {
  flow: "browser" | "device";
  /** Ties the finish (or the polls) of this attempt to its pending sign-in. */
  state: string;
  /** Browser flow: the provider's page, and where it sends the browser. */
  url?: string;
  redirectUri?: string;
  callbackHint?: string;
  /** Device flow: what to enter on the provider's page, and how often to ask
   * whether it was entered (`interval`, seconds; `expiresIn`, seconds). */
  userCode?: string;
  verificationUri?: string;
  interval?: number;
  expiresIn?: number;
  /** What the plan calls itself, whose page it is, and any risk note. */
  label?: string;
  site?: string;
  risk?: string;
}

/** A sign-in that added (or signed in again to) a provider. */
export interface SignInDone {
  provider: AdminProvider;
  /** The models the plan lists that RAST can use (null: none could be read). */
  offered: string[] | null;
}

/** One poll of a device flow: still waiting, or done and stored. */
export type SignInPoll = { status: "waiting" } | ({ status: "done" } & SignInDone);

/** A plan's name as people know it ("plus" → "Plus"). */
export function planLabel(plan: string): string {
  const known: Record<string, string> = { plus: "Plus", pro: "Pro", team: "Team", business: "Business", enterprise: "Enterprise", edu: "Edu", free: "Free", go: "Go" };
  const p = plan.trim().toLowerCase();
  return known[p] ?? (p ? p[0]!.toUpperCase() + p.slice(1) : "");
}

/** The provider picker's groups, in order. */
export const KIND_GROUPS: Array<{ id: string; label: string }> = [
  { id: "maker", label: "Model makers" },
  { id: "plan", label: "Sign in with your plan" },
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

/** Who signed in to a plan (never its tokens). */
export interface SignedIn {
  email: string;
  /** "plus", "pro", "team", … ("" unknown). */
  plan: string;
  expiresAt: number;
}

/** One limit window a provider reported for an account. */
export interface QuotaWindow {
  /** "5 hours", "Weekly", "Requests", "Tokens", … */
  name: string;
  /** Share used, 0–100. */
  usedPercent?: number;
  used?: number;
  limit?: number;
  /** When it starts again (ms). */
  resetsAt?: number;
}

export interface AccountQuota {
  windows?: QuotaWindow[];
  /** When an answer last reported them (ms; 0 never). */
  checkedAt?: number;
}

/** How a provider's accounts take turns. */
export interface Rotation {
  /** fill: the first that can answer; round: in turn. */
  mode: "fill" | "round";
  /** Replies in a row per account (round robin). */
  sticky: number;
}

/** One account of a provider (the provider itself is the first). */
export interface ProviderAccount {
  id: string;
  /** Who signed in, or the key's name. */
  label: string;
  name: string;
  /** The provider's own row. */
  first: boolean;
  signedIn: SignedIn | null;
  hasKey: boolean;
  keyHint: string;
  /** Signed in or keyed. */
  ready: boolean;
  /** Set aside by an administrator. */
  paused: boolean;
  /** Reached a limit: skipped until then. */
  resting: { until: number; reason: string } | null;
  quota: AccountQuota;
  position: number;
}

/** What RAST counted for an account in a time window. */
export interface Counted {
  calls: number;
  tokens: number;
}

export interface QuotaAccount extends ProviderAccount {
  counted: { hours5: Counted; day: Counted; week: Counted };
  /** What RAST counted per model (most used first). */
  models: Array<{ model: string; hours5: Counted; week: Counted }>;
  /** RAST can ask the provider for its limits now (ChatGPT's Codex plans). */
  canCheck: boolean;
  /** Why the last ask failed. */
  checkError?: string;
}

/** GET /api/ai/admin/quota */
export interface QuotaOverview {
  now: number;
  /** The turn order for all providers (and new ones). */
  rotation: Rotation;
  providers: Array<{
    id: string;
    name: string;
    kind: string;
    kindLabel: string;
    signsIn: boolean;
    enabled: boolean;
    rotation: Rotation;
    accounts: QuotaAccount[];
  }>;
}

export interface AdminProvider extends PublicProvider {
  prices: Prices;
  baseUrl: string;
  effectiveBaseUrl: string;
  hasKey: boolean;
  keyHint: string;
  /** Added by signing in with a plan: only its owner may chat with it. */
  signsIn: boolean;
  ownerId: number | null;
  signedIn: SignedIn | null;
  enabled: boolean;
  /** Its accounts, the provider first (a server before accounts sends none). */
  accounts?: ProviderAccount[];
  rotation?: Rotation;
  createdAt: number;
  updatedAt: number;
}

/** "in 3 h 12 min", "in 40 s" (ms from now). */
export function resetIn(ms: number): string {
  const secs = Math.max(1, Math.round(ms / 1000));
  if (secs < 60) return `in ${secs} s`;
  if (secs < 3600) return `in ${Math.ceil(secs / 60)} min`;
  if (secs < 86_400) {
    const hours = Math.floor(secs / 3600);
    const minutes = Math.floor((secs % 3600) / 60);
    return minutes ? `in ${hours} h ${minutes} min` : `in ${hours} h`;
  }
  const days = Math.floor(secs / 86_400);
  const hours = Math.floor((secs % 86_400) / 3600);
  return hours ? `in ${days} d ${hours} h` : `in ${days} d`;
}

/** An account's state in a few words, and its tone. */
export function accountState(account: ProviderAccount, now: number, signsIn: boolean): { label: string; tone: "ok" | "warn" | "off" } {
  if (account.paused) return { label: "Set aside", tone: "off" };
  if (!account.ready) return { label: signsIn ? "Sign in needed" : "Key needed", tone: "warn" };
  if (account.resting && account.resting.until > now) return { label: `Resting · back ${resetIn(account.resting.until - now)}`, tone: "warn" };
  return { label: "Ready", tone: "ok" };
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
  weeklyTokens: number | null;
  monthlyTokens: number | null;
  /** Spending limits (USD). */
  dailyCost: number | null;
  weeklyCost: number | null;
  monthlyCost: number | null;
}

/** One account's use today, this week and this month (money in USD). */
export interface UsageRow {
  userId: number;
  today: number;
  week: number;
  month: number;
  costToday: number;
  costWeek: number;
  costMonth: number;
  /** The same tokens at API prices (also for signed-in plans). */
  estimateToday?: number;
  estimateWeek?: number;
  estimateMonth?: number;
  lastAt: number;
}

/** GET /api/ai/admin/usage[/{userId}]: use over some days. `cost` counts toward spending limits; `estimate` is
 * the same tokens at API prices (also for signed-in plans). */
export interface UsageDetail {
  user?: { id: number; username: string; displayName: string; isAdmin: boolean };
  allowance: Allowance;
  providers?: string[] | null;
  days: Array<{ date: string; input: number; output: number; tokens: number; cost: number; estimate?: number; calls: number }>;
  models: Array<{ providerId: string | null; provider: string | null; model: string; input: number; output: number; tokens: number; cost: number; estimate?: number; calls: number; lastAt: number }>;
  totals?: Record<"today" | "week" | "month", { tokens: number; estimate: number }>;
}

/** A chat's tokens and what its replies are worth at API prices. */
export function chatTotals(messages: Array<Pick<Message, "inputTokens" | "outputTokens" | "estimatedCost">>): { tokens: number; estimate: number } {
  let tokens = 0;
  let estimate = 0;
  for (const m of messages) {
    tokens += (m.inputTokens || 0) + (m.outputTokens || 0);
    estimate += m.estimatedCost || 0;
  }
  return { tokens, estimate };
}

export interface Overview {
  kinds: ProviderKind[];
  providers: AdminProvider[];
  agents: AdminAgent[];
  skills: Skill[];
  limits: LimitRule[];
  users: Array<{ id: number; username: string; displayName: string; isAdmin: boolean; active: boolean }>;
  groups: Array<{ id: number; name: string }>;
  usage: UsageRow[];
  tools: Array<{ name: string; label: string }>;
  search: SearchSettings;
  /** Accounts that are not administrators may read the server (read-only). */
  serverAccess: boolean;
  /** Whether Chrome is installed for the agents' browser. */
  browser?: { chrome: boolean };
  /** The turn order for all providers (and new ones). */
  rotation?: Rotation;
}

/** How much of a limit window is left (0–100), when it says. */
export function windowLeft(w: QuotaWindow): number | null {
  const used = w.usedPercent ?? (w.limit ? ((w.used ?? 0) * 100) / w.limit : null);
  return used === null ? null : Math.max(0, Math.min(100, 100 - used));
}

/** A bar's tone by what is left: plenty, getting low, nearly out. */
export function leftTone(left: number | null): "ok" | "mid" | "low" | "" {
  if (left === null) return "";
  return left >= 50 ? "ok" : left >= 20 ? "mid" : "low";
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
  list_server_files: { label: "List a server folder", doing: "Listing", arg: "path", icon: "monitor" },
  read_server_file: { label: "Read a server file", doing: "Reading", arg: "path", icon: "monitor" },
  run_command: { label: "Run a command", doing: "Running", arg: "command", icon: "terminal" },
  web_search: { label: "Search the web", doing: "Searching", arg: "query", icon: "network" },
  fetch_url: { label: "Open a web page", doing: "Opening", arg: "url", icon: "external" },
  browser: { label: "Use the browser", doing: "Browsing", arg: "action", icon: "browser" },
  artifact: { label: "Artifact", doing: "Writing", arg: "title", icon: "compose" },
};

// ------------------------------------------------------------- artifacts

export type ArtifactKind = "html" | "svg" | "markdown" | "code";

/** One version of an artifact the model showed (the `artifact` tool). */
export interface ArtifactVersion {
  callId: string;
  /** The same across versions (the call's id, as a slug). */
  key: string;
  title: string;
  kind: ArtifactKind;
  language: string;
  content: string;
  /** 1 for the first version of `key` in the chat. */
  version: number;
}

export const ARTIFACT_KINDS: Record<ArtifactKind, { label: string; icon: string }> = {
  html: { label: "Web page", icon: "browser" },
  svg: { label: "Picture", icon: "image" },
  markdown: { label: "Document", icon: "file" },
  code: { label: "Code", icon: "terminal" },
};

/** Lowercase letters, digits and dashes (as src/ai/tools.rs `slug`). */
export function artifactSlug(text: string): string {
  let out = "";
  for (const c of text.toLowerCase()) {
    if (/^[a-z0-9]$/.test(c)) out += c;
    else if (out && !out.endsWith("-")) out += "-";
    if (out.length >= 64) break;
  }
  out = out.replace(/-+$/, "");
  return out || "artifact";
}

/** The artifact a tool block shows, once it is done (else null). */
export function artifactOf(block: ContentBlock): Omit<ArtifactVersion, "version"> | null {
  if (block.type !== "tool" || block.name !== "artifact" || block.status !== "done") return null;
  const input = typeof block.input === "object" && block.input ? block.input : {};
  const text = (key: string) => (typeof input[key] === "string" ? (input[key] as string) : "");
  const kind = text("kind").trim().toLowerCase();
  if (!(kind in ARTIFACT_KINDS) || !text("content")) return null;
  const title = text("title").trim().slice(0, 120) || "Untitled";
  return {
    callId: block.id,
    key: artifactSlug(text("id").trim() || title),
    title,
    kind: kind as ArtifactKind,
    language: text("language").trim(),
    content: text("content"),
  };
}

/** Every artifact of a chat, oldest first, numbered per key. */
export function chatArtifacts(messages: readonly Message[]): ArtifactVersion[] {
  const counts = new Map<string, number>();
  const out: ArtifactVersion[] = [];
  for (const message of messages) {
    for (const block of message.content ?? []) {
      const artifact = artifactOf(block);
      if (!artifact) continue;
      const version = (counts.get(artifact.key) ?? 0) + 1;
      counts.set(artifact.key, version);
      out.push({ ...artifact, version });
    }
  }
  return out;
}

const CODE_EXTENSIONS: Record<string, string> = {
  javascript: "js", js: "js", typescript: "ts", ts: "ts", tsx: "tsx", jsx: "jsx", python: "py", py: "py",
  rust: "rs", go: "go", java: "java", kotlin: "kt", swift: "swift", c: "c", cpp: "cpp", "c++": "cpp",
  csharp: "cs", "c#": "cs", ruby: "rb", php: "php", bash: "sh", shell: "sh", sh: "sh", zsh: "sh",
  powershell: "ps1", sql: "sql", json: "json", yaml: "yaml", yml: "yaml", toml: "toml", css: "css",
  html: "html", xml: "xml", lua: "lua", dart: "dart", r: "r", scala: "scala", dockerfile: "Dockerfile",
};

/** The file name a download of the artifact gets. */
export function artifactFileName(artifact: Pick<ArtifactVersion, "title" | "kind" | "language">): string {
  const base = artifactSlug(artifact.title);
  if (artifact.kind === "html") return `${base}.html`;
  if (artifact.kind === "svg") return `${base}.svg`;
  if (artifact.kind === "markdown") return `${base}.md`;
  const ext = CODE_EXTENSIONS[artifact.language.toLowerCase()] ?? "txt";
  return ext === "Dockerfile" ? "Dockerfile" : `${base}.${ext}`;
}

export function artifactMime(kind: ArtifactKind): string {
  return kind === "html" ? "text/html" : kind === "svg" ? "image/svg+xml" : kind === "markdown" ? "text/markdown" : "text/plain";
}

/** A browser step in a few words: "open https://…", "click [12]". */
export function browserStep(input: Record<string, unknown>): string {
  const text = (key: string) => (typeof input[key] === "string" ? (input[key] as string) : "");
  const ref = typeof input.ref === "number" || typeof input.ref === "string" ? `[${String(input.ref).replace(/[[\]]/g, "")}]` : "";
  switch (input.action) {
    case "open":
      return `open ${text("url")}`;
    case "click":
      return `click ${ref}`;
    case "type":
      return `type "${text("text")}" into ${ref}${input.submit === true ? " + Enter" : ""}`;
    case "scroll":
      return `scroll ${text("direction") || "down"}`;
    default:
      return text("action");
  }
}

/** The argument a tool card shows (its path, command, query or address). */
export function toolArgument(block: Pick<ToolBlock, "name" | "input">): string {
  const key = TOOL_INFO[block.name]?.arg ?? "path";
  if (typeof block.input === "string") return block.input;
  if (block.name === "browser" && block.input) return browserStep(block.input);
  const value = block.input?.[key];
  return typeof value === "string" ? value : JSON.stringify(block.input ?? {});
}

/** How hard the model thinks; "" leaves it to the provider. */
export type Effort = "" | "low" | "medium" | "high" | "max";

export const EFFORTS: ReadonlyArray<{ id: Effort; label: string; hint: string }> = [
  { id: "", label: "Auto", hint: "The model's own default" },
  { id: "low", label: "Low", hint: "Quick answers, fewer tokens" },
  { id: "medium", label: "Medium", hint: "A balance of speed and depth" },
  { id: "high", label: "High", hint: "Thinks longer on hard tasks" },
  { id: "max", label: "Max", hint: "Thinks the longest; models without it use High" },
];

export function effortLabel(effort: string | undefined): string {
  return EFFORTS.find((e) => e.id === (effort ?? ""))?.label ?? "Auto";
}

/** What may run without asking: presets of tools. */
export interface PermissionMode {
  id: "ask" | "read" | "edit" | "act" | "all";
  label: string;
  short: string;
  hint: string;
  tools: readonly string[];
}

const READ_TOOLS = ["list_files", "read_file", "search_files", "list_server_files", "read_server_file"] as const;
const EDIT_TOOLS = [...READ_TOOLS, "write_file", "create_folder"] as const;

export const PERMISSION_MODES: readonly PermissionMode[] = [
  { id: "ask", label: "Ask every time", short: "Asks first", hint: "Every step on your files or the web waits for you", tools: [] },
  { id: "read", label: "Allow reading files", short: "Reads freely", hint: "Lists, reads and searches your files and the server's without asking", tools: READ_TOOLS },
  { id: "edit", label: "Allow file changes", short: "Edits freely", hint: "Also writes files and makes folders without asking", tools: EDIT_TOOLS },
  { id: "act", label: "Allow files and the web", short: "Acts freely", hint: "Also searches the web, opens pages and uses the browser without asking", tools: [...EDIT_TOOLS, "web_search", "fetch_url", "browser"] },
  { id: "all", label: "Allow everything", short: "Runs freely", hint: "Also runs commands without asking", tools: [...EDIT_TOOLS, "web_search", "fetch_url", "browser", "run_command"] },
];

/** The preset a chat's allowed tools match, or null (allowed one by one). */
export function permissionMode(allowed: readonly string[]): PermissionMode | null {
  const have = new Set(allowed);
  return PERMISSION_MODES.find((mode) => mode.tools.length === have.size && mode.tools.every((t) => have.has(t))) ?? null;
}

/** Whether "Allow for this chat" is offered: for every tool. */
export function maySkipAsking(name: string): boolean {
  return name in TOOL_INFO;
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

/** How long something took or has been going: "4s", "1m 05s", "1h 02m". */
export function formatElapsed(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/**
 * What a reply that is still being written is doing, for the line under it
 * ("Thinking", "Writing", …), or null while something else shows it (a
 * tool card that runs, thinking that streams).
 */
export function replyActivity(message: Pick<Message, "status" | "content">): string | null {
  if (message.status !== "streaming") return null;
  const tail = message.content[message.content.length - 1];
  if (!tail) return "Thinking";
  if (tail.type === "reasoning") return tail.text ? null : "Thinking";
  if (tail.type === "text") return tail.text ? "Writing" : "Thinking";
  if (tail.type === "tool") return tail.status === "running" || tail.status === "allowed" ? null : "Reading the results";
  return "Working";
}

export function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

/** A price per million tokens: "$3", "$0.30", "$0.625", "$0.0028". */
export function formatPrice(value: number): string {
  if (value === 0) return "$0";
  const text = String(Number(value.toPrecision(4)));
  return "$" + (/\.\d$/.test(text) ? text + "0" : text);
}

/** "$1.23", "$0.0042", "$0". */
export function formatUsd(value: number): string {
  if (value === 0) return "$0";
  if (value >= 100) return "$" + Math.round(value).toLocaleString("en-US");
  if (value >= 0.01) return "$" + value.toFixed(2);
  return "$" + String(Number(value.toPrecision(2)));
}

/** Every limit the account has, each with what was used of it. */
export function allowanceLimits(a: Allowance): Array<{ text: string; share: number }> {
  const out: Array<{ text: string; share: number }> = [];
  const add = (used: number, limit: number | null | undefined, kind: "tokens" | "money", when: string) => {
    if (limit === null || limit === undefined) return;
    const text =
      kind === "tokens"
        ? `${formatTokens(used)} of ${formatTokens(limit)} tokens ${when}`
        : `${formatUsd(used)} of ${formatUsd(limit)} spent ${when}`;
    out.push({ text, share: limit > 0 ? used / limit : 1 });
  };
  add(a.usedToday, a.dailyTokens, "tokens", "today");
  add(a.costToday ?? 0, a.dailyCost, "money", "today");
  add(a.usedWeek ?? 0, a.weeklyTokens, "tokens", "this week");
  add(a.costWeek ?? 0, a.weeklyCost, "money", "this week");
  add(a.usedMonth, a.monthlyTokens, "tokens", "this month");
  add(a.costMonth ?? 0, a.monthlyCost, "money", "this month");
  return out;
}

/** The tightest limit as "12k of 50k tokens today", or null when unlimited. */
export function allowanceText(allowance: Allowance): string | null {
  const limits = allowanceLimits(allowance);
  if (!limits.length) return null;
  return limits.reduce((a, b) => (b.share > a.share ? b : a)).text;
}

/** Share of the tightest allowance used (0–1), or null when unlimited. */
export function allowanceUsed(allowance: Allowance): number | null {
  const limits = allowanceLimits(allowance);
  return limits.length ? Math.min(1, Math.max(...limits.map((l) => l.share))) : null;
}

/** Group conversations by when they were last used. */
/**
 * Chats by when they were last used, newest first: Pinned on top, then
 * Today, Yesterday, Previous 7 days, Previous 30 days, then each earlier
 * month of this year ("October"), then each earlier year ("2025"). Local
 * time.
 */
export function groupByDay(items: Conversation[], now = Date.now()): Array<{ label: string; items: Conversation[] }> {
  const start = new Date(now);
  start.setHours(0, 0, 0, 0);
  const today = start.getTime();
  const day = (n: number) => {
    const d = new Date(today);
    d.setDate(d.getDate() - n);
    return d.getTime();
  };
  const [yesterday, week, month] = [day(1), day(7), day(30)];
  const year = start.getFullYear();
  const monthName = new Intl.DateTimeFormat("en-US", { month: "long" });
  const label = (at: number): string => {
    if (at >= today) return "Today";
    if (at >= yesterday) return "Yesterday";
    if (at >= week) return "Previous 7 days";
    if (at >= month) return "Previous 30 days";
    const when = new Date(at);
    return when.getFullYear() === year ? monthName.format(when) : String(when.getFullYear());
  };
  const groups: Array<{ label: string; items: Conversation[]; newest: number }> = [];
  for (const item of items) {
    const name = item.pinnedAt ? "Pinned" : label(item.updatedAt);
    let group = groups.find((g) => g.label === name);
    if (!group) {
      group = { label: name, items: [], newest: item.updatedAt };
      groups.push(group);
    }
    group.items.push(item);
    group.newest = Math.max(group.newest, item.updatedAt);
  }
  const pinned = (g: { label: string }) => (g.label === "Pinned" ? 1 : 0);
  return groups
    .sort((a, b) => pinned(b) - pinned(a) || b.newest - a.newest)
    .map(({ label, items }) => ({ label, items: [...items].sort((a, b) => b.updatedAt - a.updatedAt) }));
}
