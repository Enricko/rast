// AI Agent: chat with the agents and models an administrator sets up
// (src/ai). Each account has its own conversations; replies are written on
// the server and stream to every open page (`ai.delta`), so a reload or
// another device picks a reply up where it is. Administrators also get the
// settings (providers, agents, skills, limits) and can read everyone's
// conversations (settings.ts). Agents may ask to use tools (files, commands,
// the web); each call shows as a card the chat's owner allows or denies.

import type { AppContext, AppDefinition, AppInstance } from "../../core/app";
import { ApiError } from "../../core/api";
import { h } from "../../core/dom";
import { appIconSvg, iconSvg } from "../../core/icons";
import { debounceTask, getRealtime } from "../../core/realtime";
import { confirmDialog, contextMenu, copyText, errorMessage, promptDialog } from "../../core/ui";
import { agentAvatar, providerMark } from "./marks";
import { renderMarkdown } from "./markdown";
import {
  allowanceText,
  allowanceUsed,
  applyDelta,
  EFFORTS,
  effortLabel,
  formatTokens,
  groupByDay,
  maySkipAsking,
  messageText,
  PERMISSION_MODES,
  permissionMode,
  TOOL_INFO,
  toolArgument,
  toolBlocks,
  upsertMessage,
  type ContentBlock,
  type Conversation,
  type ConversationDetail,
  type ConversationEvent,
  type DeltaEvent,
  type Effort,
  type Message,
  type MessageEvent,
  type Options,
  type ToolBlock,
} from "./model";
import { mountSettings, type SettingsView } from "./settings";

export const agentApp: AppDefinition = {
  id: "agent",
  name: "AI Agent",
  icon: "activity",
  description: "Chat with AI agents on the models your administrator sets up",
  singleton: true,
  defaultSize: { w: 1000, h: 680 },
  minSize: { w: 420, h: 360 },
  mount(ctx) {
    return mountAgentApp(ctx);
  },
};

interface Params {
  conversationId?: string;
  settings?: boolean;
}

const CHOICE_KEY = "rast.agent.choice";

/** Per-browser conveniences (last choice, effort, hidden chat list). */
function readLocal(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeLocal(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* per-browser convenience only */
  }
}

function readChoice(): string {
  return readLocal(CHOICE_KEY) ?? "";
}

function writeChoice(value: string): void {
  writeLocal(CHOICE_KEY, value);
}

/** Picker values: `agent:<id>` or `model:<providerId>|<model>`. */
function choiceOf(conversation: Pick<Conversation, "agentId" | "providerId" | "model">): string {
  if (conversation.agentId) return `agent:${conversation.agentId}`;
  if (conversation.providerId) return `model:${conversation.providerId}|${conversation.model}`;
  return "";
}

function choiceBody(choice: string): { agentId?: string; providerId?: string; model?: string } {
  if (choice.startsWith("agent:")) return { agentId: choice.slice(6) };
  if (choice.startsWith("model:")) {
    const rest = choice.slice(6);
    const bar = rest.indexOf("|");
    return { providerId: rest.slice(0, bar), model: rest.slice(bar + 1) };
  }
  return {};
}

function asParams(value: unknown): Params {
  if (!value || typeof value !== "object") return {};
  const raw = value as Record<string, unknown>;
  return {
    ...(typeof raw.conversationId === "string" ? { conversationId: raw.conversationId } : {}),
    ...(raw.settings === true ? { settings: true } : {}),
  };
}

function mountAgentApp(ctx: AppContext): AppInstance {
  ctx.root.classList.add("app-agent");
  const realtime = getRealtime(ctx.events);
  const me = ctx.store.get().user?.id ?? -1;
  const isAdmin = ctx.store.isAdmin();
  let disposed = false;
  let options: Options | null = null;
  let conversations: Conversation[] = [];
  let current: ConversationDetail | null = null;
  let loadToken = 0;
  let settings: SettingsView | null = null;
  const unsubscribers: Array<() => void> = [];

  // ------------------------------------------------------------- layout
  const newButton = h(
    "button",
    { class: "ai-new", type: "button", onClick: () => openNew() },
    iconSvg("compose", 16),
    h("span", {}, "New chat"),
  );
  const search = h("input", {
    class: "ai-search",
    type: "search",
    placeholder: "Search chats",
    ariaLabel: "Search chats",
    autocomplete: "off",
  });
  const list = h("nav", { class: "ai-list", ariaLabel: "Chats" });
  const allowanceEl = h("div", { class: "ai-allowance hidden" });
  const settingsButton = h(
    "button",
    { class: "btn btn-ghost ai-settings-btn", type: "button", onClick: () => openSettings() },
    iconSvg("settings", 15),
    h("span", {}, "Settings"),
  );
  if (!isAdmin) settingsButton.classList.add("hidden");
  const sideClose = h(
    "button",
    { class: "btn btn-ghost btn-icon btn-sm ai-side-close", type: "button", title: "Hide chats", ariaLabel: "Hide chats" },
    iconSvg("menu", 16),
  );
  const side = h(
    "aside",
    { class: "ai-side" },
    h(
      "div",
      { class: "ai-brand" },
      h("span", { class: "ai-brand-icon" }, appIconSvg("agent", "activity", 22)),
      h("span", { class: "ai-brand-name" }, "AI Agent"),
      sideClose,
    ),
    newButton,
    h("label", { class: "ai-search-wrap" }, iconSvg("search", 14), search),
    list,
    h("div", { class: "ai-side-foot" }, allowanceEl, settingsButton),
  );

  const titleEl = h("h2", { class: "ai-title" }, "New chat");
  const subEl = h("span", { class: "ai-sub" });
  const ownerEl = h("span", { class: "ai-owner hidden" });
  const headAvatar = h("span", { class: "ai-head-avatar" });
  const picker = h("select", { class: "ai-picker", ariaLabel: "Agent or model" });
  const pickFace = h("span", { class: "ai-pick-face" });
  const pickWrap = h("label", { class: "ai-pick", title: "Agent or model" }, pickFace, iconSvg("chevronDown", 13), picker);
  const menuButton = h(
    "button",
    { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "More", ariaLabel: "More" },
    iconSvg("more", 16),
  );
  const sideToggle = h(
    "button",
    { class: "btn btn-ghost btn-icon btn-sm ai-side-toggle", type: "button", title: "Chats", ariaLabel: "Show chats" },
    iconSvg("menu", 16),
  );
  const head = h(
    "header",
    { class: "ai-head" },
    sideToggle,
    headAvatar,
    h("div", { class: "ai-head-text" }, titleEl, h("span", { class: "ai-head-sub" }, subEl, ownerEl)),
    menuButton,
  );
  const thread = h("div", { class: "ai-thread" });
  const scroller = h("div", { class: "ai-scroll" }, thread);
  const input = h("textarea", {
    class: "ai-input",
    rows: 1,
    placeholder: "Message",
    ariaLabel: "Message",
    spellcheck: true,
  });
  const sendButton = h(
    "button",
    { class: "btn btn-primary btn-icon ai-send", type: "button", title: "Send (Enter)", ariaLabel: "Send" },
    iconSvg("arrowUp", 17),
  );
  // What may run without asking, and how hard the model thinks.
  const guardLabel = h("span", { class: "ai-chip-label" });
  const guard = h(
    "button",
    { class: "ai-chip ai-guard", type: "button", title: "What the agent may do without asking", ariaLabel: "Permissions" },
    iconSvg("shield", 13),
    guardLabel,
    iconSvg("chevronDown", 12),
  );
  const effortText = h("span", { class: "ai-chip-label" });
  const effortButton = h(
    "button",
    { class: "ai-chip ai-effort", type: "button", title: "Effort: how hard the model thinks", ariaLabel: "Effort" },
    iconSvg("gauge", 13),
    effortText,
    iconSvg("chevronDown", 12),
  );
  guard.setAttribute("aria-haspopup", "menu");
  effortButton.setAttribute("aria-haspopup", "menu");
  const composerNote = h("div", { class: "ai-composer-note small muted" });
  const composer = h(
    "div",
    { class: "ai-composer" },
    composerNote,
    h("div", { class: "ai-input-wrap" }, input, h("div", { class: "ai-composer-bar" }, pickWrap, effortButton, guard, sendButton)),
  );
  const main = h("section", { class: "ai-main" }, head, scroller, composer);
  const settingsHost = h("section", { class: "ai-settings hidden" });
  const scrim = h("div", { class: "ai-scrim" });
  ctx.root.append(side, main, settingsHost, scrim);

  // The chat list: a column that hides in a wide window, a drawer in a
  // narrow one (where it is positioned over the chat).
  const SIDE_KEY = "rast.agent.sideHidden";
  if (readLocal(SIDE_KEY) === "1") ctx.root.classList.add("ai-side-hidden");
  const isDrawer = () => getComputedStyle(side).position === "absolute";
  function setDrawer(open: boolean): void {
    ctx.root.classList.toggle("ai-side-open", open);
    if (open) window.setTimeout(() => newButton.focus({ preventScroll: true }), 30);
  }
  function toggleSide(): void {
    if (isDrawer()) {
      setDrawer(!ctx.root.classList.contains("ai-side-open"));
      return;
    }
    const hidden = ctx.root.classList.toggle("ai-side-hidden");
    writeLocal(SIDE_KEY, hidden ? "1" : "0");
  }
  sideToggle.addEventListener("click", toggleSide);
  sideClose.addEventListener("click", () => {
    toggleSide();
    sideToggle.focus({ preventScroll: true });
  });
  scrim.addEventListener("click", () => setDrawer(false));
  ctx.root.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && ctx.root.classList.contains("ai-side-open") && isDrawer()) {
      event.preventDefault();
      event.stopPropagation();
      setDrawer(false);
      sideToggle.focus({ preventScroll: true });
    }
  });

  // New chats start with these (the effort last chosen; asking first).
  const EFFORT_KEY = "rast.agent.effort";
  const storedEffort = (): Effort => {
    const value = readLocal(EFFORT_KEY) ?? "";
    return EFFORTS.some((e) => e.id === value) ? (value as Effort) : "";
  };
  let draftEffort: Effort = storedEffort();
  let draftAllowed: string[] = [];
  guard.addEventListener("click", () => {
    const allowed = current ? current.conversation.allowedTools ?? [] : draftAllowed;
    const mode = permissionMode(allowed);
    const icons: Record<string, string> = { ask: "shield", read: "eye", edit: "edit", act: "network" };
    contextMenu(
      0,
      0,
      [
        ...PERMISSION_MODES.map((m) => ({
          label: m.label,
          hint: m.hint,
          icon: icons[m.id],
          checked: mode?.id === m.id,
          onClick: () => void setPermissions([...m.tools]),
        })),
        ...(mode
          ? []
          : [
              {
                label: "Allowed one by one",
                hint: allowed.map((name) => TOOL_INFO[name]?.label ?? name).join(", "),
                icon: "check",
                checked: true,
                onClick: () => undefined,
              },
            ]),
        "separator" as const,
        { label: "Commands always ask first", icon: "terminal", disabled: true, onClick: () => undefined },
      ],
      { anchor: guard, ariaLabel: "Permissions" },
    );
  });
  effortButton.addEventListener("click", () => {
    const effort = current ? current.conversation.effort ?? "" : draftEffort;
    contextMenu(
      0,
      0,
      EFFORTS.map((e) => ({ label: e.label, hint: e.hint, checked: e.id === effort, onClick: () => void setEffort(e.id) })),
      { anchor: effortButton, ariaLabel: "Effort" },
    );
  });
  search.addEventListener("input", () => renderList());
  input.addEventListener("input", () => {
    autoGrow();
    updateComposer();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void send();
    }
  });
  sendButton.addEventListener("click", () => {
    if (current?.conversation.running) void stop();
    else void send();
  });
  picker.addEventListener("change", () => {
    drawPickFace();
    updateTitle();
    if (!current) renderThread();
    void choose(picker.value);
  });
  menuButton.addEventListener("click", (event) => {
    if (!current || current.readOnly) return;
    const conversation = current.conversation;
    contextMenu(event.clientX, event.clientY, [
      { label: "Rename", icon: "rename", onClick: () => void rename(conversation) },
      { label: "Copy chat", icon: "copy", onClick: () => void copyChat() },
      "separator",
      { label: "Delete", icon: "trash", danger: true, onClick: () => void remove(conversation) },
    ]);
  });

  function autoGrow(): void {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 220)}px`;
  }

  // -------------------------------------------------------------- data
  async function loadOptions(): Promise<void> {
    try {
      options = await ctx.api.get<Options>("/api/ai/options");
    } catch (error) {
      options = null;
      ctx.toast.error(errorMessage(error));
    }
    renderPicker();
    renderAllowance();
    updateComposer();
    if (!current) renderThread();
  }

  async function loadList(): Promise<void> {
    try {
      const body = await ctx.api.get<{ items: Conversation[] }>("/api/ai/conversations");
      conversations = body.items;
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
    renderList();
  }

  async function openConversation(id: string, options_: { quiet?: boolean } = {}): Promise<void> {
    closeSettings();
    const token = ++loadToken;
    try {
      const detail = await ctx.api.get<ConversationDetail>(`/api/ai/conversations/${encodeURIComponent(id)}`);
      if (token !== loadToken || disposed) return;
      const switching = current?.conversation.id !== id;
      current = detail;
      if (!detail.readOnly) ctx.saveState({ conversationId: id });
      renderAll(switching);
      if (switching && !detail.readOnly) input.focus();
    } catch (error) {
      if (token !== loadToken) return;
      if (error instanceof ApiError && error.isNotFound) {
        if (!options_.quiet) ctx.toast.warning("That chat no longer exists.");
        openNew();
        return;
      }
      ctx.toast.error(errorMessage(error));
    }
  }

  function openNew(): void {
    closeSettings();
    loadToken += 1;
    current = null;
    draftAllowed = [];
    draftEffort = storedEffort();
    ctx.saveState({});
    renderAll(true);
    input.focus();
  }

  function openSettings(): void {
    if (!isAdmin) return;
    ctx.saveState({ settings: true });
    main.classList.add("hidden");
    settingsHost.classList.remove("hidden");
    ctx.root.classList.remove("ai-side-open");
    if (!settings) {
      settings = mountSettings({
        ctx,
        host: settingsHost,
        onClose: () => {
          closeSettings();
          if (current) ctx.saveState({ conversationId: current.conversation.id });
          else ctx.saveState({});
        },
        openConversation: (id) => void openConversation(id),
      });
    } else {
      settings.refresh();
    }
    ctx.setTitle("AI Agent — Settings");
  }

  function closeSettings(): void {
    if (settingsHost.classList.contains("hidden")) return;
    settingsHost.classList.add("hidden");
    main.classList.remove("hidden");
    updateTitle();
  }

  // ----------------------------------------------------------- actions
  async function send(): Promise<void> {
    const text = input.value.trim();
    if (!text || current?.readOnly || current?.conversation.running) return;
    const choice = picker.value;
    if (!current && !choice) {
      ctx.toast.warning("Pick an agent or a model first.");
      return;
    }
    sendButton.disabled = true;
    try {
      let id = current?.conversation.id;
      if (!id) {
        const created = await ctx.api.post<{ conversation: Conversation }>("/api/ai/conversations", {
          ...choiceBody(choice),
          effort: draftEffort,
          allowedTools: draftAllowed,
        });
        id = created.conversation.id;
        conversations = [created.conversation, ...conversations.filter((c) => c.id !== id)];
        current = { conversation: created.conversation, messages: [], seq: 0, readOnly: false };
        ctx.saveState({ conversationId: id });
      }
      const sent = await ctx.api.post<{ conversation: Conversation; message: Message; reply: Message }>(
        `/api/ai/conversations/${encodeURIComponent(id)}/messages`,
        { text },
      );
      input.value = "";
      autoGrow();
      if (current?.conversation.id === id) {
        current.conversation = sent.conversation;
        upsertMessage(current.messages, sent.message);
        upsertMessage(current.messages, sent.reply);
      }
      upsertConversation(sent.conversation);
      renderAll(false);
      scrollToEnd(true);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      if (error instanceof ApiError && (error.isRateLimited || error.isForbidden)) void loadOptions();
    } finally {
      sendButton.disabled = false;
      updateComposer();
    }
  }

  async function stop(): Promise<void> {
    if (!current) return;
    try {
      await ctx.api.post(`/api/ai/conversations/${encodeURIComponent(current.conversation.id)}/stop`);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  /** The owner's answer to a tool request. */
  async function decide(call: ToolBlock, decision: "allow" | "deny", always = false): Promise<void> {
    if (!current || current.readOnly) return;
    const id = current.conversation.id;
    try {
      const body = await ctx.api.post<{ message: Message; conversation: Conversation }>(
        `/api/ai/conversations/${encodeURIComponent(id)}/tools/${encodeURIComponent(call.id)}`,
        { decision, always },
      );
      if (current?.conversation.id !== id) return;
      upsertMessage(current.messages, body.message);
      current.conversation = { ...current.conversation, ...body.conversation };
      if (body.conversation.running) current.seq = 0;
      upsertConversation(body.conversation);
      refreshMessage(body.message);
      updateComposer();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      if (error instanceof ApiError && error.isConflict) void openConversation(id, { quiet: true });
      else refreshCurrent();
    }
  }

  /** What runs without asking: this chat's, or the next new chat's. */
  async function setPermissions(tools: string[]): Promise<void> {
    if (!current) {
      draftAllowed = tools;
      updateComposer();
      return;
    }
    await patchConversation({ allowedTools: tools });
  }

  /** How hard the model thinks: this chat's, and new chats' from now on. */
  async function setEffort(effort: Effort): Promise<void> {
    writeLocal(EFFORT_KEY, effort);
    draftEffort = effort;
    if (!current) {
      updateComposer();
      return;
    }
    await patchConversation({ effort });
  }

  async function patchConversation(change: { allowedTools?: string[]; effort?: Effort }): Promise<void> {
    if (!current || current.readOnly) return;
    const id = current.conversation.id;
    try {
      const updated = await ctx.api.patch<{ conversation: Conversation }>(`/api/ai/conversations/${encodeURIComponent(id)}`, change);
      upsertConversation(updated.conversation);
      if (current?.conversation.id === id) current.conversation = { ...current.conversation, ...updated.conversation };
      updateComposer();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  function refreshCurrent(): void {
    const reply = current?.messages[current.messages.length - 1];
    if (reply) refreshMessage(reply);
  }

  async function choose(choice: string): Promise<void> {
    writeChoice(choice);
    if (!current || current.readOnly) return;
    try {
      const body = choiceBody(choice);
      const patch = body.agentId ? { agentId: body.agentId } : { agentId: "", providerId: body.providerId, model: body.model };
      const updated = await ctx.api.patch<{ conversation: Conversation }>(
        `/api/ai/conversations/${encodeURIComponent(current.conversation.id)}`,
        patch,
      );
      current.conversation = updated.conversation;
      upsertConversation(updated.conversation);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      renderPicker();
    }
  }

  async function rename(conversation: Conversation): Promise<void> {
    const title = await promptDialog({
      title: "Rename chat",
      label: "Name",
      value: conversation.title,
      confirmLabel: "Rename",
      session: ctx.dialog("rename", { id: conversation.id }),
    });
    if (title === null) return;
    try {
      const updated = await ctx.api.patch<{ conversation: Conversation }>(
        `/api/ai/conversations/${encodeURIComponent(conversation.id)}`,
        { title: title.trim() },
      );
      upsertConversation(updated.conversation);
      if (current?.conversation.id === conversation.id) current.conversation = updated.conversation;
      renderList();
      updateTitle();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  async function remove(conversation: Conversation): Promise<void> {
    const ok = await confirmDialog({
      title: "Delete chat",
      message: `Delete "${conversation.title || "New chat"}"? Its messages cannot be brought back.`,
      confirmLabel: "Delete",
      danger: true,
      session: ctx.dialog("delete", { id: conversation.id }),
    });
    if (!ok) return;
    try {
      await ctx.api.del(`/api/ai/conversations/${encodeURIComponent(conversation.id)}`);
      conversations = conversations.filter((c) => c.id !== conversation.id);
      if (current?.conversation.id === conversation.id) openNew();
      else renderList();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  async function copyChat(): Promise<void> {
    if (!current) return;
    const lines = current.messages.map((m) => {
      const calls = toolBlocks(m).map((call) => `[${TOOL_INFO[call.name]?.label ?? call.name}: ${toolArgument(call)} (${call.status})]`);
      return `${m.role === "user" ? "You" : "Assistant"}:\n${[messageText(m), ...calls].filter(Boolean).join("\n")}`;
    });
    if (await copyText(lines.join("\n\n"))) ctx.toast.success("Chat copied.");
  }

  ctx.onDialog("rename", (params) => {
    const id = (params as { id?: string } | null)?.id;
    const conversation = conversations.find((c) => c.id === id);
    if (conversation) void rename(conversation);
  });
  ctx.onDialog("delete", (params) => {
    const id = (params as { id?: string } | null)?.id;
    const conversation = conversations.find((c) => c.id === id);
    if (conversation) void remove(conversation);
  });

  function upsertConversation(conversation: Conversation): void {
    if (conversation.userId !== me) return;
    const index = conversations.findIndex((c) => c.id === conversation.id);
    if (index >= 0) conversations[index] = { ...conversations[index], ...conversation };
    else conversations.unshift(conversation);
    conversations.sort((a, b) => b.updatedAt - a.updatedAt);
    renderList();
  }

  // --------------------------------------------------------- rendering
  function renderAll(resetScroll: boolean): void {
    renderList();
    renderPicker();
    renderThread();
    updateTitle();
    updateComposer();
    if (resetScroll) scrollToEnd(true);
  }

  /** Who answers for a picker value: name, the model behind it, picture. */
  function describe(choice: string): { name: string; detail: string; picture: (size: number) => HTMLElement } | null {
    const body = choiceBody(choice);
    if (body.agentId) {
      const agent = options?.agents.find((a) => a.id === body.agentId);
      const name = agent?.name ?? "Agent";
      return { name, detail: agent?.model ?? "", picture: (size) => agentAvatar(name, size) };
    }
    if (body.providerId) {
      const provider = options?.providers.find((p) => p.id === body.providerId);
      const kind = provider?.kind ?? "custom";
      return { name: body.model ?? "", detail: provider?.name ?? "", picture: (size) => providerMark(kind, size) };
    }
    return null;
  }

  function updateTitle(): void {
    const conversation = current?.conversation;
    titleEl.textContent = conversation ? conversation.title || "New chat" : "New chat";
    const who = describe(conversation ? choiceOf(conversation) : picker.value);
    headAvatar.replaceChildren(who ? who.picture(26) : agentAvatar("AI", 26));
    subEl.textContent = who ? [who.name, who.detail].filter(Boolean).join(" · ") : "";
    ownerEl.classList.toggle("hidden", !current?.readOnly);
    ownerEl.textContent = current?.readOnly ? `${conversation?.owner ?? "?"}'s chat · read only` : "";
    menuButton.classList.toggle("hidden", !current || current.readOnly);
    if (settingsHost.classList.contains("hidden")) ctx.setTitle(conversation?.title ? `AI Agent — ${conversation.title}` : "AI Agent");
  }

  function renderList(): void {
    const query = search.value.trim().toLowerCase();
    const items = conversations.filter((c) => !query || c.title.toLowerCase().includes(query));
    list.replaceChildren();
    if (!items.length) {
      list.appendChild(h("p", { class: "ai-list-empty small muted" }, query ? "No chats match." : "No chats yet."));
      return;
    }
    for (const group of groupByDay(items)) {
      list.appendChild(h("div", { class: "ai-list-group" }, group.label));
      for (const item of group.items) {
        const active = current?.conversation.id === item.id;
        const row = h(
          "button",
          {
            class: `ai-list-item${active ? " is-active" : ""}`,
            type: "button",
            title: item.title || "New chat",
            onClick: () => {
              ctx.root.classList.remove("ai-side-open");
              void openConversation(item.id);
            },
            onContextMenu: (event) => {
              event.preventDefault();
              contextMenu(event.clientX, event.clientY, [
                { label: "Rename", icon: "rename", onClick: () => void rename(item) },
                { label: "Delete", icon: "trash", danger: true, onClick: () => void remove(item) },
              ]);
            },
          },
          h("span", { class: "ai-list-title" }, item.title || "New chat"),
          item.running
            ? h("span", { class: "ai-dot", title: "Writing a reply" })
            : item.waiting
              ? h("span", { class: "ai-dot is-waiting", title: "Waiting for your answer" })
              : null,
        );
        list.appendChild(row);
      }
    }
  }

  function renderPicker(): void {
    picker.replaceChildren();
    const agents = options?.agents ?? [];
    const providers = (options?.providers ?? []).filter((p) => p.models.length);
    const wanted = current ? choiceOf(current.conversation) : readChoice();
    const values: string[] = [];
    if (agents.length) {
      const group = h("optgroup", {});
      group.label = "Agents";
      for (const agent of agents) {
        const value = `agent:${agent.id}`;
        values.push(value);
        group.appendChild(h("option", { value }, agent.name));
      }
      picker.appendChild(group);
    }
    for (const provider of providers) {
      const group = h("optgroup", {});
      group.label = provider.name;
      for (const model of provider.models) {
        const value = `model:${provider.id}|${model}`;
        values.push(value);
        group.appendChild(h("option", { value }, model));
      }
      picker.appendChild(group);
    }
    // A conversation's own choice that is no longer offered stays visible.
    if (wanted && !values.includes(wanted) && current) {
      const label = current.conversation.agentId ? "Agent no longer available" : current.conversation.model || "Unknown model";
      picker.prepend(h("option", { value: wanted }, label));
      values.unshift(wanted);
    }
    picker.value = values.includes(wanted) ? wanted : (values[0] ?? "");
    picker.disabled = !values.length || Boolean(current?.readOnly);
    pickWrap.classList.toggle("hidden", !values.length);
    drawPickFace();
  }

  function drawPickFace(): void {
    const who = describe(picker.value);
    pickFace.replaceChildren(...(who ? [who.picture(18), h("span", { class: "ai-pick-name" }, who.name)] : [h("span", {}, "Choose")]));
  }

  function renderAllowance(): void {
    const allowance = options?.allowance;
    const textValue = allowance ? allowanceText(allowance) : null;
    allowanceEl.classList.toggle("hidden", !textValue);
    if (!allowance || !textValue) return;
    const used = allowanceUsed(allowance) ?? 0;
    allowanceEl.replaceChildren(
      h("div", { class: "ai-allowance-bar" }, h("span", { style: `width:${Math.round(used * 100)}%` })),
      h("span", { class: "small muted" }, textValue),
    );
    allowanceEl.classList.toggle("is-full", used >= 1);
  }

  function updateComposer(): void {
    const running = Boolean(current?.conversation.running);
    const readOnly = Boolean(current?.readOnly);
    composer.classList.toggle("hidden", readOnly || !options?.configured);
    sendButton.replaceChildren(iconSvg(running ? "stop" : "arrowUp", 17));
    sendButton.title = running ? "Stop" : "Send (Enter)";
    sendButton.setAttribute("aria-label", running ? "Stop" : "Send");
    sendButton.classList.toggle("is-stop", running);
    sendButton.disabled = !running && (!input.value.trim() || !picker.value);
    const allowed = current ? current.conversation.allowedTools ?? [] : draftAllowed;
    const mode = permissionMode(allowed);
    guardLabel.textContent = mode?.short ?? "Some allowed";
    guard.classList.toggle("is-on", mode?.id !== "ask");
    effortText.textContent = effortLabel(current ? current.conversation.effort : draftEffort);
    const allowance = options?.allowance;
    const waiting = !running && current?.messages[current.messages.length - 1]?.status === "waiting";
    input.placeholder = waiting ? "Or write something else" : "Message";
    composerNote.textContent =
      allowance && allowanceUsed(allowance) === 1
        ? "You have used your AI allowance for now."
        : waiting
          ? "The agent is waiting for your answer above. Writing a message instead skips its request."
          : "";
  }

  const bodies = new Map<number, { el: HTMLElement; frame: number }>();
  const allowanceRefresh = debounceTask(() => void loadOptions(), 400);

  function renderThread(): void {
    thread.replaceChildren();
    bodies.clear();
    if (!options) return;
    if (!current) {
      thread.appendChild(welcome());
      return;
    }
    if (!current.messages.length) {
      thread.appendChild(welcome());
    }
    for (const message of current.messages) thread.appendChild(messageEl(message));
  }

  function welcome(): HTMLElement {
    if (!options?.configured) {
      return h(
        "div",
        { class: "ai-welcome" },
        h("div", { class: "ai-welcome-icon" }, iconSvg("activity", 28)),
        h("p", { class: "ai-welcome-title" }, "No AI models yet"),
        h(
          "p",
          { class: "small muted" },
          isAdmin ? "Add a model provider to start chatting." : "An administrator has not set up AI models yet.",
        ),
        isAdmin
          ? h("button", { class: "btn btn-primary btn-sm", type: "button", onClick: () => openSettings() }, "Add a provider")
          : null,
      );
    }
    if (!picker.value) {
      return h(
        "div",
        { class: "ai-welcome" },
        h("p", { class: "ai-welcome-title" }, "No models for this account"),
        h("p", { class: "small muted" }, "Ask an administrator to let your account use an AI model."),
      );
    }
    const agents = options.agents;
    const who = describe(picker.value);
    const user = ctx.store.get().user;
    const first = (user?.displayName || user?.username || "").trim().split(/\s+/)[0] ?? "";
    const hour = new Date().getHours();
    const greeting = hour < 5 ? "Working late" : hour < 12 ? "Good morning" : hour < 18 ? "Good afternoon" : "Good evening";
    const agent = agents.find((a) => `agent:${a.id}` === picker.value);
    const suggestions: Array<[string, string, string]> = [
      ["folder", "What's in my home folder?", "List what is in my home folder and tell me what takes the most space."],
      ["edit", "Write a note", "Write a short note to notes.md with a checklist for today."],
      isAdmin
        ? ["terminal", "Check the server", "How is the server doing? Check disk space, memory and uptime."]
        : ["search", "Find a file", "Find the files in my folders whose name mentions \"invoice\"."],
      ["external", "Read a web page", "Summarize this page for me: https://"],
    ];
    const orb = h("div", { class: "ai-orb" }, who ? who.picture(60) : agentAvatar("AI", 60));
    return h(
      "div",
      { class: "ai-welcome" },
      orb,
      h("p", { class: "ai-welcome-title" }, first ? `${greeting}, ${first}` : greeting),
      h(
        "p",
        { class: "ai-welcome-sub muted" },
        agent?.description || (who ? `${who.name} is ready. It asks you before it touches your files, runs a command or opens the web.` : ""),
      ),
      agents.length > 1
        ? h(
            "div",
            { class: "ai-agent-chips" },
            ...agents.map((a) =>
              h(
                "button",
                {
                  class: `ai-chip${picker.value === `agent:${a.id}` ? " is-active" : ""}`,
                  type: "button",
                  title: a.description || a.name,
                  onClick: () => {
                    picker.value = `agent:${a.id}`;
                    writeChoice(picker.value);
                    drawPickFace();
                    updateTitle();
                    renderThread();
                    input.focus();
                  },
                },
                agentAvatar(a.name, 18),
                h("span", {}, a.name),
              ),
            ),
          )
        : null,
      h(
        "div",
        { class: "ai-ideas" },
        ...suggestions.map(([icon, label, prompt]) =>
          h(
            "button",
            {
              class: "ai-idea",
              type: "button",
              onClick: () => {
                input.value = prompt;
                autoGrow();
                updateComposer();
                input.focus();
                input.setSelectionRange(prompt.length, prompt.length);
              },
            },
            h("span", { class: "ai-idea-icon" }, iconSvg(icon, 15)),
            h("span", { class: "ai-idea-label" }, label),
          ),
        ),
      ),
    );
  }

  function messageEl(message: Message): HTMLElement {
    if (message.role === "user") {
      const text = messageText(message);
      return h(
        "div",
        { class: "ai-msg ai-msg-user", dataset: { id: String(message.id) } },
        h("div", { class: "ai-bubble" }, text),
        h(
          "div",
          { class: "ai-msg-tools" },
          h(
            "button",
            {
              class: "btn btn-ghost btn-icon btn-sm",
              type: "button",
              title: "Copy",
              ariaLabel: "Copy",
              onClick: () => void copyText(text).then((ok) => ok && ctx.toast.success("Copied.")),
            },
            iconSvg("copy", 13),
          ),
        ),
      );
    }
    const body = h("div", { class: "ai-reply" });
    const who = current ? describe(choiceOf(current.conversation)) : null;
    const name = who?.name || message.model || "Assistant";
    const wrap = h(
      "div",
      { class: "ai-msg ai-msg-assistant", dataset: { id: String(message.id) } },
      h("div", { class: "ai-who" }, who ? who.picture(22) : agentAvatar(name, 22), h("span", { class: "ai-who-name" }, name)),
      body,
    );
    bodies.set(message.id, { el: body, frame: 0 });
    fillReply(body, message);
    return wrap;
  }

  function fillReply(body: HTMLElement, message: Message): void {
    const text = messageText(message);
    const streaming = message.status === "streaming";
    // Opened sections stay open while the reply changes.
    const opened = new Map<string, boolean>();
    for (const details of body.querySelectorAll<HTMLDetailsElement>("details[data-key]")) {
      opened.set(details.dataset.key ?? "", details.open);
    }
    const parts: HTMLElement[] = [];
    const blocks: ContentBlock[] = message.content;
    const last = blocks.length - 1;
    blocks.forEach((block, index) => {
      const live = streaming && index === last;
      if (block.type === "reasoning") {
        if (!block.text) return;
        const key = `reasoning:${index}`;
        const details = h("details", { class: "ai-thinking", dataset: { key } });
        details.open = opened.get(key) ?? live;
        details.classList.toggle("is-live", live);
        details.append(
          h("summary", {}, iconSvg("sparkle", 13), h("span", {}, live ? "Thinking…" : "Thought it through")),
          h("div", { class: "ai-thinking-text" }, block.text),
        );
        parts.push(details);
      } else if (block.type === "text") {
        if (!block.text && !live) return;
        const md = h("div", { class: "ai-md" });
        renderMarkdown(md, block.text, {
          onCopy: (code) => void copyText(code).then((ok) => ok && ctx.toast.success("Code copied.")),
        });
        if (live) caretInto(md);
        parts.push(md);
      } else if (block.type === "tool") {
        parts.push(toolCard(message, block, opened));
      }
    });
    // Between steps: still working.
    const tail = blocks[last];
    const toolBusy = tail?.type === "tool" && (tail.status === "running" || tail.status === "allowed");
    if (streaming && tail?.type !== "text" && !toolBusy && !(tail?.type === "reasoning")) {
      const md = h("div", { class: "ai-md" });
      caretInto(md);
      parts.push(md);
    }
    if (message.status === "error") {
      parts.push(h("div", { class: "ai-error", role: "alert" }, iconSvg("warning", 14), h("span", {}, message.error || "The reply failed.")));
    }
    if (!streaming && message.status !== "waiting") {
      const meta: string[] = [];
      if (message.model) meta.push(message.model);
      if (message.status === "stopped") meta.push("stopped");
      if (message.inputTokens || message.outputTokens) meta.push(`${formatTokens(message.inputTokens + message.outputTokens)} tokens`);
      parts.push(
        h(
          "div",
          { class: "ai-msg-tools" },
          text
            ? h(
                "button",
                {
                  class: "btn btn-ghost btn-icon btn-sm",
                  type: "button",
                  title: "Copy",
                  ariaLabel: "Copy",
                  onClick: () => void copyText(text).then((ok) => ok && ctx.toast.success("Copied.")),
                },
                iconSvg("copy", 13),
              )
            : null,
          h("span", { class: "ai-meta small faint" }, meta.join(" · ")),
        ),
      );
    }
    body.replaceChildren(...parts);
  }

  function toolCard(message: Message, block: ToolBlock, opened: Map<string, boolean>): HTMLElement {
    const info = TOOL_INFO[block.name];
    const arg = toolArgument(block);
    const waiting = message.status === "waiting" && block.status === "pending";
    const canAnswer = waiting && !current?.readOnly;
    const state: Record<ToolBlock["status"], [string, string]> = {
      pending: ["clock", "Asks first"],
      allowed: ["clock", "Allowed"],
      running: ["refresh", "Running"],
      done: ["check", "Done"],
      error: ["warning", "Failed"],
      denied: ["close", "Not allowed"],
      skipped: ["close", "Not run"],
    };
    const [icon, label] = state[block.status] ?? ["info", block.status];
    const card = h(
      "div",
      { class: `ai-tool is-${block.status}`, dataset: { call: block.id } },
      h(
        "div",
        { class: "ai-tool-head" },
        h("span", { class: "ai-tool-icon" }, iconSvg(info?.icon ?? "apps", 14)),
        h("span", { class: "ai-tool-label" }, info?.label ?? block.name),
        h("span", { class: "ai-tool-arg", title: arg }, arg),
        h("span", { class: "ai-tool-state small" }, iconSvg(icon, 12), h("span", {}, label)),
      ),
    );
    const input = typeof block.input === "object" && block.input ? block.input : {};
    // What exactly would run or be written.
    if (block.name === "run_command" && (waiting || arg.length > 60 || arg.includes("\n"))) {
      card.appendChild(h("pre", { class: "ai-tool-code" }, arg));
    }
    if (block.name === "write_file" && typeof input.content === "string") {
      const key = `content:${block.id}`;
      const details = h("details", { class: "ai-tool-more", dataset: { key } });
      details.open = opened.get(key) ?? false;
      const size = new TextEncoder().encode(input.content).length;
      details.append(h("summary", {}, `Content · ${size.toLocaleString()} bytes`), h("pre", { class: "ai-tool-code" }, input.content));
      card.appendChild(details);
    }
    if (canAnswer) {
      const buttons: HTMLButtonElement[] = [];
      const act = (decision: "allow" | "deny", always: boolean) => () => {
        for (const button of buttons) button.disabled = true;
        void decide(block, decision, always);
      };
      buttons.push(h("button", { class: "btn btn-primary btn-sm", type: "button", onClick: act("allow", false) }, "Allow"));
      if (maySkipAsking(block.name)) {
        buttons.push(
          h(
            "button",
            { class: "btn btn-sm", type: "button", title: `Run "${info?.label ?? block.name}" without asking for the rest of this chat`, onClick: act("allow", true) },
            "Allow for this chat",
          ),
        );
      }
      buttons.push(h("button", { class: "btn btn-ghost btn-sm", type: "button", onClick: act("deny", false) }, "Deny"));
      card.appendChild(h("div", { class: "ai-tool-actions" }, ...buttons));
    } else if (waiting) {
      card.appendChild(h("p", { class: "ai-tool-note small muted" }, `Waiting for ${current?.conversation.owner ?? "the owner"} to answer.`));
    }
    if (block.status === "error" && block.error) {
      card.appendChild(h("p", { class: "ai-tool-error small" }, block.error));
    }
    if (block.status === "done" && block.output) {
      const key = `output:${block.id}`;
      const details = h("details", { class: "ai-tool-more", dataset: { key } });
      details.open = opened.get(key) ?? false;
      details.append(h("summary", {}, "Result"), h("pre", { class: "ai-tool-code" }, block.output));
      card.appendChild(details);
    }
    return card;
  }

  /** The typing caret goes at the end of the last line of text. */
  function caretInto(md: HTMLElement): void {
    const caret = h("span", { class: "ai-caret", ariaHidden: true });
    let target: Element = md;
    for (;;) {
      const last: Element | null = target.lastElementChild;
      if (!last || !/^(P|LI|UL|OL|H[1-6]|BLOCKQUOTE)$/.test(last.tagName)) break;
      target = last;
    }
    target.appendChild(caret);
  }

  function refreshMessage(message: Message): void {
    const entry = bodies.get(message.id);
    if (!entry) {
      renderThread();
      return;
    }
    if (entry.frame) return;
    entry.frame = window.requestAnimationFrame(() => {
      entry.frame = 0;
      const stick = nearBottom();
      const latest = current?.messages.find((m) => m.id === message.id);
      if (latest) fillReply(entry.el, latest);
      if (stick) scrollToEnd(false);
    });
  }

  function nearBottom(): boolean {
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  }

  function scrollToEnd(force: boolean): void {
    if (force || nearBottom()) {
      window.requestAnimationFrame(() => {
        scroller.scrollTop = scroller.scrollHeight;
      });
    }
  }

  // ---------------------------------------------------------- realtime
  unsubscribers.push(
    realtime.on<DeltaEvent>("ai.delta", (delta) => {
      if (!current || current.conversation.id !== delta.conversationId) return;
      const result = applyDelta(current, delta);
      if (result === "stale") void openConversation(current.conversation.id, { quiet: true });
      else if (result === "applied") {
        const message = current.messages.find((m) => m.id === delta.messageId);
        if (message) refreshMessage(message);
      }
    }),
    realtime.on<MessageEvent>("ai.message", (event) => {
      const listed = conversations.find((c) => c.id === event.conversationId);
      const waiting = event.message.role === "assistant" ? event.message.status === "waiting" : listed?.waiting;
      if (listed && (listed.running !== event.running || listed.waiting !== waiting)) {
        listed.running = event.running;
        listed.waiting = Boolean(waiting);
        renderList();
      }
      if (listed && !event.running && event.message.role === "assistant" && options?.allowance && allowanceText(options.allowance)) {
        allowanceRefresh.run();
      }
      if (!current || current.conversation.id !== event.conversationId) return;
      const known = current.messages.some((m) => m.id === event.message.id);
      upsertMessage(current.messages, event.message);
      current.conversation.running = event.running;
      if (!event.running) current.seq = 0;
      else if (typeof event.seq === "number" && event.message.status === "streaming") current.seq = event.seq;
      if (event.message.role === "assistant") current.conversation.waiting = event.message.status === "waiting";
      if (known) refreshMessage(event.message);
      else {
        const stick = nearBottom();
        renderThread();
        if (stick) scrollToEnd(true);
      }
      updateComposer();
    }),
    realtime.on<ConversationEvent>("ai.conversation", (event) => {
      if (event.deleted) {
        conversations = conversations.filter((c) => c.id !== event.conversation.id);
        renderList();
        if (current?.conversation.id === event.conversation.id) openNew();
        return;
      }
      upsertConversation(event.conversation);
      if (current?.conversation.id === event.conversation.id) {
        const running = current.conversation.running;
        current.conversation = { ...current.conversation, ...event.conversation, running: running || event.conversation.running };
        renderPicker();
        updateTitle();
        updateComposer();
      }
    }),
    realtime.on("ai.config", () => {
      void loadOptions();
      settings?.refresh();
    }),
    realtime.onResync(() => {
      void loadOptions();
      void loadList();
      if (current) void openConversation(current.conversation.id, { quiet: true });
    }),
  );

  ctx.onPlace((value) => {
    const params = asParams(value);
    if (params.settings) openSettings();
    else if (params.conversationId) void openConversation(params.conversationId, { quiet: true });
    else openNew();
  });
  ctx.onReopen((value) => {
    const params = asParams(value);
    if (params.settings) openSettings();
    else if (params.conversationId) void openConversation(params.conversationId);
  });
  ctx.onDispose(() => {
    disposed = true;
    for (const unsubscribe of unsubscribers.splice(0)) unsubscribe();
    for (const entry of bodies.values()) if (entry.frame) window.cancelAnimationFrame(entry.frame);
    allowanceRefresh.cancel();
    settings?.destroy();
  });

  // ------------------------------------------------------------- start
  const start = asParams(ctx.params);
  renderAll(false);
  void (async () => {
    await Promise.all([loadOptions(), loadList()]);
    if (disposed) return;
    if (start.settings && isAdmin) openSettings();
    else if (start.conversationId) await openConversation(start.conversationId, { quiet: true });
    else renderThread();
  })();
  return {};
}
