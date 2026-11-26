// AI Agent: chat with the agents and models an administrator sets up
// (src/ai). Each account has its own conversations; replies are written on
// the server and stream to every open page (`ai.delta`), so a reload or
// another device picks a reply up where it is. Administrators also get the
// settings (providers, agents, skills, limits) and can read everyone's
// conversations (settings.ts).

import type { AppContext, AppDefinition, AppInstance } from "../../core/app";
import { ApiError } from "../../core/api";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { debounceTask, getRealtime } from "../../core/realtime";
import { confirmDialog, contextMenu, copyText, errorMessage, promptDialog } from "../../core/ui";
import { renderMarkdown } from "./markdown";
import {
  allowanceText,
  allowanceUsed,
  applyDelta,
  formatTokens,
  groupByDay,
  messageReasoning,
  messageText,
  upsertMessage,
  type Conversation,
  type ConversationDetail,
  type ConversationEvent,
  type DeltaEvent,
  type Message,
  type MessageEvent,
  type Options,
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

function readChoice(): string {
  try {
    return window.localStorage.getItem(CHOICE_KEY) ?? "";
  } catch {
    return "";
  }
}

function writeChoice(value: string): void {
  try {
    window.localStorage.setItem(CHOICE_KEY, value);
  } catch {
    /* per-browser convenience only */
  }
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
    { class: "btn btn-primary ai-new", type: "button", onClick: () => openNew() },
    iconSvg("plus", 15),
    h("span", {}, "New chat"),
  );
  const search = h("input", {
    class: "input ai-search",
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
  const side = h("aside", { class: "ai-side" }, newButton, search, list, h("div", { class: "ai-side-foot" }, allowanceEl, settingsButton));

  const titleEl = h("h2", { class: "ai-title" }, "New chat");
  const ownerEl = h("span", { class: "ai-owner hidden" });
  const picker = h("select", { class: "select ai-picker", ariaLabel: "Agent or model" });
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
  const head = h("header", { class: "ai-head" }, sideToggle, h("div", { class: "ai-head-text" }, titleEl, ownerEl), picker, menuButton);
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
  const composerNote = h("div", { class: "ai-composer-note small muted" });
  const composer = h("div", { class: "ai-composer" }, h("div", { class: "ai-input-wrap" }, input, sendButton), composerNote);
  const main = h("section", { class: "ai-main" }, head, scroller, composer);
  const settingsHost = h("section", { class: "ai-settings hidden" });
  ctx.root.append(side, main, settingsHost);

  sideToggle.addEventListener("click", () => ctx.root.classList.toggle("ai-side-open"));
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
  picker.addEventListener("change", () => void choose(picker.value));
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
        const created = await ctx.api.post<{ conversation: Conversation }>("/api/ai/conversations", choiceBody(choice));
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
    const lines = current.messages.map((m) => `${m.role === "user" ? "You" : "Assistant"}:\n${messageText(m)}`);
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

  function updateTitle(): void {
    const conversation = current?.conversation;
    titleEl.textContent = conversation ? conversation.title || "New chat" : "New chat";
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
          item.running ? h("span", { class: "ai-dot", title: "Writing a reply" }) : null,
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
    picker.classList.toggle("hidden", !values.length);
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
    const allowance = options?.allowance;
    composerNote.textContent =
      allowance && allowanceUsed(allowance) === 1 ? "You have used your AI allowance for now." : "";
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
      thread.appendChild(h("div", { class: "ai-welcome" }, h("p", { class: "ai-welcome-title" }, "What can I help with?")));
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
    return h(
      "div",
      { class: "ai-welcome" },
      h("p", { class: "ai-welcome-title" }, "What can I help with?"),
      agents.length > 1
        ? h(
            "div",
            { class: "ai-agent-chips" },
            ...agents.map((agent) =>
              h(
                "button",
                {
                  class: `ai-chip${picker.value === `agent:${agent.id}` ? " is-active" : ""}`,
                  type: "button",
                  title: agent.description || agent.name,
                  onClick: () => {
                    picker.value = `agent:${agent.id}`;
                    writeChoice(picker.value);
                    renderThread();
                    input.focus();
                  },
                },
                agent.name,
              ),
            ),
          )
        : null,
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
    const wrap = h("div", { class: "ai-msg ai-msg-assistant", dataset: { id: String(message.id) } }, body);
    bodies.set(message.id, { el: body, frame: 0 });
    fillReply(body, message);
    return wrap;
  }

  function fillReply(body: HTMLElement, message: Message): void {
    const text = messageText(message);
    const reasoning = messageReasoning(message);
    const streaming = message.status === "streaming";
    const parts: HTMLElement[] = [];
    if (reasoning) {
      const previous = body.querySelector("details");
      const details = h("details", { class: "ai-thinking" });
      details.open = previous ? (previous as HTMLDetailsElement).open : streaming && !text;
      const summary = h("summary", {}, streaming && !text ? "Thinking…" : "Thought");
      const inner = h("div", { class: "ai-thinking-text" }, reasoning);
      details.append(summary, inner);
      parts.push(details);
    }
    const md = h("div", { class: "ai-md" });
    renderMarkdown(md, text, {
      onCopy: (code) => void copyText(code).then((ok) => ok && ctx.toast.success("Code copied.")),
    });
    if (streaming) caretInto(md);
    parts.push(md);
    if (message.status === "error") {
      parts.push(h("div", { class: "ai-error", role: "alert" }, iconSvg("warning", 14), h("span", {}, message.error || "The reply failed.")));
    }
    if (!streaming) {
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
      if (listed && listed.running !== event.running) {
        listed.running = event.running;
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
