// AI Agent settings, for administrators: tiles for providers, agents, skills,
// tools (web search), limits, everyone's chats and usage; each opens its own
// page.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { switchControl } from "../../core/components";
import { relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { confirmDialog, errorMessage, modal } from "../../core/ui";
import { agentAvatar, providerMark } from "./marks";
import {
  formatTokens,
  groupKinds,
  type ProviderKind,
  type AdminAgent,
  type AdminProvider,
  type Conversation,
  type LimitRule,
  type Overview,
  type SearchSettings,
  type Skill,
} from "./model";

export interface SettingsView {
  refresh(): void;
  destroy(): void;
}

type Page = "home" | "providers" | "agents" | "skills" | "tools" | "limits" | "chats" | "usage";

const PAGES: Array<{ id: Exclude<Page, "home">; label: string; icon: string; hue: number }> = [
  { id: "providers", label: "Providers", icon: "database", hue: 212 },
  { id: "agents", label: "Agents", icon: "sparkle", hue: 268 },
  { id: "skills", label: "Skills", icon: "layers", hue: 172 },
  { id: "tools", label: "Tools", icon: "terminal", hue: 32 },
  { id: "limits", label: "Limits", icon: "shield", hue: 145 },
  { id: "chats", label: "Chats", icon: "list", hue: 198 },
  { id: "usage", label: "Usage", icon: "activity", hue: 340 },
];

const SEARCH_LABELS: Record<SearchSettings["kind"], string> = {
  "": "Off",
  brave: "Brave Search",
  tavily: "Tavily",
  searxng: "SearXNG",
};

function numberOrNull(value: string): number | null {
  const trimmed = value.trim().replace(/[,_\s]/g, "");
  if (!trimmed) return null;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) ? parsed : NaN;
}

/** A form inside a modal: shows the server's refusal and stays open. */
function formError(status: HTMLElement, error: unknown): never {
  status.textContent = errorMessage(error);
  status.hidden = false;
  throw error instanceof Error ? error : new Error(String(error));
}

export function mountSettings(options: {
  ctx: AppContext;
  host: HTMLElement;
  onClose: () => void;
  openConversation: (id: string) => void;
}): SettingsView {
  const { ctx, host } = options;
  let page: Page = "home";
  let overview: Overview | null = null;
  let disposed = false;
  const stops: Array<() => void> = [];

  const back = h("button", { class: "btn btn-ghost btn-sm ai-set-back", type: "button" }, iconSvg("arrowLeft", 15), h("span", {}, "Chats"));
  const title = h("h2", { class: "ai-set-title" }, "Settings");
  const actions = h("div", { class: "ai-set-actions" });
  const body = h("div", { class: "ai-set-body" });
  host.append(h("header", { class: "ai-set-head" }, back, title, actions), body);
  back.addEventListener("click", () => {
    if (page === "home") options.onClose();
    else show("home");
  });

  async function load(): Promise<void> {
    try {
      overview = await ctx.api.get<Overview>("/api/ai/admin/overview");
    } catch (error) {
      body.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error)));
      return;
    }
    if (!disposed) render();
  }

  function show(next: Page): void {
    page = next;
    render();
    body.scrollTop = 0;
  }

  function render(): void {
    const label = PAGES.find((p) => p.id === page)?.label;
    title.textContent = label ?? "Settings";
    back.lastElementChild!.textContent = page === "home" ? "Chats" : "Settings";
    actions.replaceChildren();
    body.replaceChildren();
    if (!overview) {
      body.appendChild(h("div", { class: "ai-set-loading muted" }, "Loading…"));
      return;
    }
    switch (page) {
      case "home":
        return renderHome(overview);
      case "providers":
        return renderProviders(overview);
      case "agents":
        return renderAgents(overview);
      case "skills":
        return renderSkills(overview);
      case "tools":
        return renderTools(overview);
      case "limits":
        return renderLimits(overview);
      case "chats":
        return void renderChats(overview);
      case "usage":
        return renderUsage(overview);
    }
  }

  // --------------------------------------------------------------- home
  function renderHome(data: Overview): void {
    const usedToday = data.usage.reduce((sum, u) => sum + u.today, 0);
    const counts: Record<Exclude<Page, "home">, string> = {
      providers: data.providers.length ? `${data.providers.length} connected` : "None yet",
      agents: data.agents.length ? `${data.agents.length}` : "None yet",
      skills: data.skills.length ? `${data.skills.length}` : "None yet",
      tools: data.search.configured ? `Web search: ${SEARCH_LABELS[data.search.kind]}` : "Files, commands, web pages",
      limits: data.limits.length ? `${data.limits.length} rule${data.limits.length === 1 ? "" : "s"}` : "No limits",
      chats: "All accounts",
      usage: `${formatTokens(usedToday)} tokens today`,
    };
    // What each tile shows at a glance.
    const peek: Partial<Record<Exclude<Page, "home">, HTMLElement | null>> = {
      providers: data.providers.length
        ? h("span", { class: "ai-stack" }, ...data.providers.slice(0, 5).map((p) => providerMark(p.kind, 22)))
        : null,
      agents: data.agents.length ? h("span", { class: "ai-stack" }, ...data.agents.slice(0, 5).map((a) => agentAvatar(a.name, 22))) : null,
    };
    const grid = h("div", { class: "ai-tiles" });
    for (const item of PAGES) {
      const icon = h("span", { class: "ai-tile-icon" }, iconSvg(item.icon, 20));
      icon.style.setProperty("--tile-hue", String(item.hue));
      grid.appendChild(
        h(
          "button",
          { class: "ai-tile", type: "button", onClick: () => show(item.id) },
          h("span", { class: "ai-tile-top" }, icon, peek[item.id] ?? null),
          h("span", { class: "ai-tile-label" }, item.label),
          h("span", { class: "ai-tile-value small muted" }, counts[item.id]),
        ),
      );
    }
    if (!data.providers.length) body.appendChild(connectFirst());
    body.appendChild(grid);
  }

  /** The first step, before anything works. */
  function connectFirst(): HTMLElement {
    const kinds = overview?.kinds ?? [];
    return h(
      "div",
      { class: "ai-hero" },
      h("span", { class: "ai-stack ai-stack-lg" }, ...["anthropic", "openai", "gemini", "opencode", "ollama"].filter((k) => kinds.some((x) => x.id === k)).map((k) => providerMark(k, 34))),
      h("div", { class: "ai-hero-text" }, h("h3", {}, "Connect a model provider"), h("p", { class: "muted" }, `${kinds.length} to choose from: model makers, gateways like OpenCode and OpenRouter, or models on your own network.`)),
      h("button", { class: "btn btn-primary", type: "button", onClick: () => pickProvider() }, iconSvg("plus", 14), h("span", {}, "Add a provider")),
    );
  }

  function addButton(label: string, onClick: () => void): void {
    actions.appendChild(h("button", { class: "btn btn-primary btn-sm", type: "button", onClick }, iconSvg("plus", 14), h("span", {}, label)));
  }

  function rows(items: HTMLElement[], empty: string): void {
    if (!items.length) {
      body.appendChild(h("p", { class: "ai-set-empty muted" }, empty));
      return;
    }
    body.appendChild(h("div", { class: "ai-rows" }, ...items));
  }

  function row(opts: {
    title: string;
    detail: string;
    badge?: string;
    off?: boolean;
    lead?: HTMLElement;
    onClick: () => void;
    onDelete?: () => void;
  }): HTMLElement {
    return h(
      "div",
      { class: `ai-row${opts.off ? " is-off" : ""}` },
      opts.lead ? h("span", { class: "ai-row-lead" }, opts.lead) : null,
      h(
        "button",
        { class: "ai-row-main", type: "button", onClick: opts.onClick },
        h("span", { class: "ai-row-title" }, opts.title, opts.badge ? h("span", { class: "badge" }, opts.badge) : null),
        h("span", { class: "ai-row-detail small muted" }, opts.detail),
      ),
      opts.onDelete
        ? h(
            "button",
            { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Delete", ariaLabel: `Delete ${opts.title}`, onClick: opts.onDelete },
            iconSvg("trash", 14),
          )
        : null,
    );
  }

  async function remove(what: string, name: string, path: string): Promise<void> {
    const ok = await confirmDialog({
      within: ctx.root,
      title: `Delete ${what}`,
      message: `Delete "${name}"?`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await ctx.api.del(path);
      await load();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  // ---------------------------------------------------------- providers
  function renderProviders(data: Overview): void {
    addButton("Add provider", () => pickProvider());
    if (!data.providers.length) {
      body.appendChild(connectFirst());
      return;
    }
    const grid = h("div", { class: "ai-cards" });
    for (const p of data.providers) {
      const models = `${p.models.length} model${p.models.length === 1 ? "" : "s"}`;
      grid.appendChild(
        h(
          "div",
          { class: `ai-card${p.enabled ? "" : " is-off"}` },
          h(
            "button",
            { class: "ai-card-main", type: "button", onClick: () => editProvider(p) },
            h(
              "span",
              { class: "ai-card-head" },
              providerMark(p.kind, 34),
              h("span", { class: "ai-card-names" }, h("span", { class: "ai-card-title" }, p.name), h("span", { class: "ai-card-sub small muted" }, p.kindLabel)),
            ),
            h(
              "span",
              { class: "ai-card-foot small" },
              h("span", { class: `ai-pill${p.enabled ? " is-on" : ""}` }, p.enabled ? "On" : "Off"),
              h("span", { class: "muted" }, models),
              p.hasKey ? h("span", { class: "muted ai-card-key" }, iconSvg("key", 12), p.keyHint) : null,
            ),
          ),
          h(
            "button",
            {
              class: "btn btn-ghost btn-icon btn-sm ai-card-delete",
              type: "button",
              title: "Delete",
              ariaLabel: `Delete ${p.name}`,
              onClick: () => void remove("provider", p.name, `/api/ai/providers/${p.id}`),
            },
            iconSvg("trash", 14),
          ),
        ),
      );
    }
    body.appendChild(grid);
  }

  /** Step 1 of adding a provider: which one. */
  function pickProvider(): void {
    const data = overview;
    if (!data) return;
    const search = h("input", {
      class: "input ai-kind-search",
      type: "search",
      placeholder: `Search ${data.kinds.length} providers`,
      autocomplete: "off",
      dataset: { autofocus: "true" },
    });
    const listEl = h("div", { class: "ai-kind-groups" });
    let close: () => void = () => undefined;
    const draw = () => {
      const groups = groupKinds(data.kinds, search.value);
      listEl.replaceChildren(
        ...groups.map((group) =>
          h(
            "section",
            { class: "ai-kind-group" },
            h("h4", {}, group.label),
            h(
              "div",
              { class: "ai-kind-grid" },
              ...group.kinds.map((k) =>
                h(
                  "button",
                  {
                    class: "ai-kind",
                    type: "button",
                    onClick: () => {
                      close();
                      editProvider(null, k);
                    },
                  },
                  providerMark(k.id, 32),
                  h("span", { class: "ai-kind-text" }, h("span", { class: "ai-kind-label" }, k.label), h("span", { class: "ai-kind-about small muted" }, k.about)),
                ),
              ),
            ),
          ),
        ),
      );
      if (!groups.length) listEl.appendChild(h("p", { class: "ai-set-empty muted" }, "No provider matches. Any OpenAI- or Anthropic-compatible endpoint works under Custom."));
    };
    search.addEventListener("input", draw);
    search.addEventListener("keydown", (event) => {
      if (event.key !== "Enter") return;
      event.preventDefault();
      listEl.querySelector<HTMLButtonElement>(".ai-kind")?.click();
    });
    draw();
    close = modal({ within: ctx.root, title: "Add a provider", wide: true, body: h("div", { class: "ai-kind-picker" }, search, listEl) });
  }

  /** Step 2 (or editing): connect it. */
  function editProvider(existing: AdminProvider | null, picked?: ProviderKind): void {
    const data = overview;
    if (!data) return;
    const info = data.kinds.find((k) => k.id === (existing?.kind ?? picked?.id));
    if (!info) return;
    const custom = info.group === "custom";
    const local = info.group === "local";
    const name = h("input", { class: "input", value: existing?.name ?? info.label, autocomplete: "off", placeholder: `e.g. ${info.label} (work)` });
    const baseUrl = h("input", {
      class: "input",
      value: existing?.baseUrl ?? "",
      autocomplete: "off",
      spellcheck: false,
      placeholder: info.defaultBaseUrl || "https://…/v1",
    });
    const key = h("input", {
      class: "input",
      type: "password",
      autocomplete: "new-password",
      spellcheck: false,
      placeholder: existing?.hasKey ? `Saved (${existing.keyHint})` : info.needsKey ? "Paste the key" : "Optional",
    });
    const removeKey = switchControl({ label: "Remove the saved key" });
    const enabled = switchControl({ label: "People can use it", checked: existing?.enabled ?? true });
    const status = h("div", { class: "field-error", hidden: true });
    let selected: string[] = [...(existing?.models ?? [])];
    let offered: string[] | null = null;

    // Models: chosen ones as chips, the provider's list to pick from.
    const chips = h("div", { class: "ai-model-chips" });
    const typed = h("input", { class: "input input-sm ai-model-add", placeholder: "Add a model by name", autocomplete: "off", spellcheck: false });
    const offerEl = h("div", { class: "ai-model-picks hidden" });
    const loadButton = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("refresh", 13), h("span", {}, "Load models"));
    const drawChips = () => {
      chips.replaceChildren(
        ...selected.map((model) =>
          h(
            "span",
            { class: "ai-model-chip" },
            h("span", {}, model),
            h(
              "button",
              {
                type: "button",
                title: `Remove ${model}`,
                ariaLabel: `Remove ${model}`,
                onClick: () => {
                  selected = selected.filter((m) => m !== model);
                  drawChips();
                  drawOffer();
                },
              },
              iconSvg("close", 11),
            ),
          ),
        ),
      );
      if (!selected.length) chips.appendChild(h("span", { class: "small muted" }, "None yet: load them from the provider, or add one by name."));
    };
    const filter = h("input", { class: "input input-sm", type: "search", autocomplete: "off" });
    const offerList = h("div", { class: "ai-model-list" });
    const drawOffer = () => {
      if (!offered) return;
      const q = filter.value.trim().toLowerCase();
      const shown = offered.filter((m) => !q || m.toLowerCase().includes(q));
      offerList.replaceChildren(
        ...shown.slice(0, 400).map((model) => {
          const box = h("input", { type: "checkbox", checked: selected.includes(model) });
          box.addEventListener("change", () => {
            selected = box.checked ? [...selected, model] : selected.filter((m) => m !== model);
            drawChips();
          });
          return h("label", { class: "ai-model-option" }, box, h("span", {}, model));
        }),
      );
      if (!offered.length) offerList.appendChild(h("p", { class: "small muted" }, "The provider listed no models RAST can use."));
    };
    filter.addEventListener("input", drawOffer);
    const addAll = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, "Select all shown");
    addAll.addEventListener("click", () => {
      const q = filter.value.trim().toLowerCase();
      for (const m of offered ?? []) if ((!q || m.toLowerCase().includes(q)) && !selected.includes(m)) selected.push(m);
      drawChips();
      drawOffer();
    });
    offerEl.append(h("div", { class: "ai-model-tools" }, filter, addAll), offerList);
    typed.addEventListener("keydown", (event) => {
      if (event.key !== "Enter") return;
      event.preventDefault();
      const model = typed.value.trim();
      if (model && !selected.includes(model)) selected.push(model);
      typed.value = "";
      drawChips();
      drawOffer();
    });
    const loadModels = async (quiet: boolean) => {
      loadButton.disabled = true;
      if (!quiet) status.hidden = true;
      try {
        const result = await ctx.api.post<{ models: string[] }>("/api/ai/providers/models", {
          ...(existing ? { providerId: existing.id } : { kind: info.id }),
          ...(baseUrl.value.trim() ? { baseUrl: baseUrl.value.trim() } : {}),
          ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
        });
        offered = result.models;
        filter.placeholder = `Filter ${offered.length} models`;
        offerEl.classList.remove("hidden");
        drawOffer();
      } catch (error) {
        if (!quiet) {
          status.textContent = errorMessage(error);
          status.hidden = false;
        }
      } finally {
        loadButton.disabled = false;
      }
    };
    loadButton.addEventListener("click", () => void loadModels(false));
    // A new provider's list loads as soon as it can answer.
    if (!existing) {
      key.addEventListener("change", () => {
        if (key.value.trim() && !offered) void loadModels(false);
      });
      if (local) void loadModels(true);
    }
    drawChips();

    const keyLink = info.keyUrl
      ? h("a", { class: "ai-key-link", href: info.keyUrl, target: "_blank", rel: "noopener noreferrer" }, info.needsKey ? "Get a key" : "Docs", iconSvg("external", 12))
      : null;
    const keyField = h(
      "label",
      { class: "field" },
      h("span", { class: "field-label ai-field-row" }, "API key", keyLink),
      key,
      existing?.hasKey ? h("span", { class: "field-hint" }, "Leave empty to keep the saved key.") : null,
    );
    const urlField = h(
      "label",
      { class: "field" },
      h("span", { class: "field-label" }, custom ? "Base URL" : "Address"),
      baseUrl,
      h("span", { class: "field-hint" }, custom ? "Where its API answers, e.g. https://example.com/v1." : `Leave empty for ${info.defaultBaseUrl}.`),
    );
    const advanced = h("details", { class: "ai-advanced" }, h("summary", {}, "Advanced"));
    if (!custom) advanced.appendChild(urlField);
    if (local) advanced.appendChild(keyField);
    if (existing?.hasKey) advanced.appendChild(removeKey.el);
    advanced.open = Boolean(existing?.baseUrl);

    const form = h(
      "div",
      { class: "ai-form" },
      h(
        "div",
        { class: "ai-connect-head" },
        providerMark(info.id, 40),
        h("span", { class: "ai-connect-text" }, h("span", { class: "ai-connect-title" }, info.label), h("span", { class: "small muted" }, info.about)),
      ),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
      custom ? urlField : null,
      local ? null : keyField,
      h(
        "div",
        { class: "field" },
        h("span", { class: "field-label ai-field-row" }, "Models people can pick", loadButton),
        chips,
        offerEl,
        typed,
      ),
      advanced.childElementCount > 1 ? advanced : null,
      enabled.el,
      status,
    );
    modal({
      within: ctx.root,
      title: existing ? existing.name : `Connect ${info.label}`,
      body: form,
      wide: true,
      actions: [
        existing
          ? { label: "Cancel", variant: "ghost", onClick: () => undefined }
          : { label: "Back", variant: "ghost", onClick: () => pickProvider() },
        {
          label: existing ? "Save" : "Connect",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            const payload = {
              kind: info.id,
              name: name.value.trim(),
              baseUrl: baseUrl.value.trim(),
              ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
              ...(existing && removeKey.isChecked() && !key.value.trim() ? { removeKey: true } : {}),
              models: selected,
              enabled: enabled.isChecked(),
            };
            try {
              if (existing) await ctx.api.put(`/api/ai/providers/${existing.id}`, payload);
              else await ctx.api.post("/api/ai/providers", payload);
            } catch (error) {
              formError(status, error);
            }
            close();
            await load();
          },
        },
      ],
    });
  }

  // ------------------------------------------------------------- agents
  function renderAgents(data: Overview): void {
    addButton("Add agent", () => editAgent(null));
    rows(
      data.agents.map((a) => {
        const provider = data.providers.find((p) => p.id === a.providerId);
        return row({
          title: a.name,
          lead: agentAvatar(a.name, 30),
          ...(a.enabled ? {} : { badge: "Off" }),
          off: !a.enabled,
          detail: provider ? `${provider.name} · ${a.model}` : "No model set",
          onClick: () => editAgent(a),
          onDelete: () => void remove("agent", a.name, `/api/ai/agents/${a.id}`),
        });
      }),
      "No agents yet. Without one, people pick a model and chat with a plain assistant.",
    );
  }

  function editAgent(existing: AdminAgent | null): void {
    const data = overview;
    if (!data) return;
    const name = h("input", { class: "input", value: existing?.name ?? "", autocomplete: "off", placeholder: "e.g. Writer" });
    const description = h("input", { class: "input", value: existing?.description ?? "", autocomplete: "off", placeholder: "Shown to people choosing an agent" });
    const provider = h("select", { class: "select" }, h("option", { value: "" }, "Choose…"));
    for (const p of data.providers) provider.appendChild(h("option", { value: p.id }, p.name));
    provider.value = existing?.providerId ?? data.providers[0]?.id ?? "";
    const model = h("input", { class: "input", value: existing?.model ?? "", autocomplete: "off", spellcheck: false });
    const datalist = h("datalist", { id: `ai-models-${Math.random().toString(36).slice(2)}` });
    model.setAttribute("list", datalist.id);
    const fillModels = () => {
      const p = data.providers.find((x) => x.id === provider.value);
      datalist.replaceChildren(...(p?.models ?? []).map((m) => h("option", { value: m })));
      if (!model.value && p?.models[0]) model.value = p.models[0];
    };
    provider.addEventListener("change", () => {
      model.value = "";
      fillModels();
    });
    fillModels();
    const prompt = h("textarea", {
      class: "textarea ai-prompt",
      rows: 7,
      placeholder: "Who the agent is and how it answers, e.g. \"You are a friendly writing coach…\"",
    });
    prompt.value = existing?.systemPrompt ?? "";
    const skillBoxes = data.skills.map((skill) => {
      const box = h("input", { type: "checkbox", checked: existing?.skills.includes(skill.id) ?? false });
      return { skill, box, el: h("label", { class: "ai-check" }, box, h("span", {}, skill.name)) };
    });
    const toolBoxes = data.tools.map((tool) => {
      const box = h("input", { type: "checkbox", checked: existing?.tools ? existing.tools.includes(tool.name) : true });
      return { tool, box, el: h("label", { class: "ai-check" }, box, h("span", {}, tool.label)) };
    });
    const temperature = h("input", { class: "input", inputMode: "decimal", value: existing?.temperature?.toString() ?? "", placeholder: "Default" });
    const maxTokens = h("input", { class: "input", inputMode: "numeric", value: existing?.maxOutputTokens?.toString() ?? "", placeholder: "Default" });
    const enabled = switchControl({ label: "On", checked: existing?.enabled ?? true });
    const status = h("div", { class: "field-error", hidden: true });
    const form = h(
      "div",
      { class: "ai-form" },
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Description"), description),
      h(
        "div",
        { class: "ai-form-row" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Provider"), provider),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Model"), model, datalist),
      ),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Personality"), prompt),
      skillBoxes.length
        ? h("div", { class: "field" }, h("span", { class: "field-label" }, "Skills"), h("div", { class: "ai-checks" }, ...skillBoxes.map((s) => s.el)))
        : null,
      h(
        "div",
        { class: "field" },
        h("span", { class: "field-label" }, "Tools"),
        h("div", { class: "ai-checks" }, ...toolBoxes.map((t) => t.el)),
        h("span", { class: "field-hint" }, "Only those the person chatting may use; each call asks them first."),
      ),
      h(
        "div",
        { class: "ai-form-row" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Temperature"), temperature, h("span", { class: "field-hint" }, "0 (focused) to 2 (creative)")),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Longest answer"), maxTokens, h("span", { class: "field-hint" }, "Tokens")),
      ),
      enabled.el,
      status,
    );
    modal({
      within: ctx.root,
      title: existing ? existing.name : "Add agent",
      body: form,
      wide: true,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: existing ? "Save" : "Add",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            const temp = numberOrNull(temperature.value);
            const max = numberOrNull(maxTokens.value);
            if (Number.isNaN(temp) || Number.isNaN(max)) formError(status, new Error("Temperature and answer length are numbers."));
            const payload = {
              name: name.value.trim(),
              description: description.value.trim(),
              systemPrompt: prompt.value,
              providerId: provider.value || null,
              model: model.value.trim(),
              skills: skillBoxes.filter((s) => s.box.checked).map((s) => s.skill.id),
              tools: toolBoxes.every((t) => t.box.checked) ? null : toolBoxes.filter((t) => t.box.checked).map((t) => t.tool.name),
              temperature: temp,
              maxOutputTokens: max,
              enabled: enabled.isChecked(),
            };
            try {
              if (existing) await ctx.api.put(`/api/ai/agents/${existing.id}`, payload);
              else await ctx.api.post("/api/ai/agents", payload);
            } catch (error) {
              formError(status, error);
            }
            close();
            await load();
          },
        },
      ],
    });
  }

  // -------------------------------------------------------------- tools
  function renderTools(data: Overview): void {
    const search = data.search;
    const items: Array<[string, string, string]> = [
      ["folder", "Files", "List, read, find and write files in the person's home and the shared folders they can open (accounts that may use File Station)."],
      ["monitor", "Server files", "Administrators: anything RAST's service account can read. Others: read only, never RAST's data, people's files, home folders or secrets (secret values show as [hidden])."],
      ["terminal", "Commands", "Administrators: as root on this server, once Terminal is turned on. Others: on the server read-only, as an account without privileges and without network; nothing they run can change the server."],
      ["external", "Web pages", "Public pages only; addresses on this server or private networks are refused."],
      ["network", "Web search", search.configured ? `Through ${SEARCH_LABELS[search.kind]}.` : "Needs a search service, below."],
    ];
    body.append(
      h(
        "div",
        { class: "ai-tool-info" },
        ...items.map(([icon, label, detail]) =>
          h(
            "div",
            { class: "ai-tool-info-row" },
            h("span", { class: "ai-tile-icon" }, iconSvg(icon, 18)),
            h("span", {}, h("span", { class: "ai-row-title" }, label), h("span", { class: "ai-row-detail small muted" }, detail)),
          ),
        ),
      ),
      h("p", { class: "ai-set-note small muted" }, "Every tool call shows in the chat and runs only once its owner allows it (or lets that kind run). Turn tools off per agent in Agents."),
    );
    const serverAccess = switchControl({
      label: "Let people who are not administrators read the server",
      hint: "Server files and read-only commands, as above. Their own RAST files and the shared folders they may change stay writable through the file tools.",
      checked: data.serverAccess,
      onChange: async (enabled) => {
        serverAccess.input.disabled = true;
        try {
          await ctx.api.put("/api/ai/server-access", { enabled });
          ctx.toast.success(enabled ? "People can read the server, read-only." : "Only administrators can reach the server now.");
        } catch (error) {
          serverAccess.input.checked = !enabled;
          ctx.toast.error(errorMessage(error));
        } finally {
          serverAccess.input.disabled = false;
        }
      },
    });
    body.append(h("div", { class: "ai-set-switch" }, serverAccess.el));
    const detail = search.configured
      ? search.kind === "searxng"
        ? `${SEARCH_LABELS[search.kind]} · ${search.url}`
        : `${SEARCH_LABELS[search.kind]} · key ${search.keyHint}`
      : "Off. Connect Brave Search, Tavily or a SearXNG server.";
    body.appendChild(h("h3", { class: "ai-set-subtitle" }, "Web search"));
    rows([row({ title: search.configured ? SEARCH_LABELS[search.kind] : "Not connected", detail, onClick: () => editSearch(search) })], "");
    if (search.configured) {
      const test = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("search", 13), h("span", {}, "Try a search"));
      test.addEventListener("click", async () => {
        test.disabled = true;
        try {
          const result = await ctx.api.post<{ results: number; first: { title: string } | null }>("/api/ai/search/test");
          ctx.toast.success(result.results ? `Web search works: ${result.results} results.` : "Web search answered, without results.");
        } catch (error) {
          ctx.toast.error(errorMessage(error));
        } finally {
          test.disabled = false;
        }
      });
      body.appendChild(h("div", { class: "ai-set-row-actions" }, test));
    }
  }

  function editSearch(existing: SearchSettings): void {
    const kind = h("select", { class: "select" });
    for (const [value, label] of Object.entries(SEARCH_LABELS)) kind.appendChild(h("option", { value }, value ? label : "None"));
    kind.value = existing.kind;
    const url = h("input", { class: "input", value: existing.url, autocomplete: "off", spellcheck: false, placeholder: "http://127.0.0.1:8888" });
    const key = h("input", { class: "input", type: "password", autocomplete: "new-password", spellcheck: false });
    const urlField = h("label", { class: "field" }, h("span", { class: "field-label" }, "Address"), url, h("span", { class: "field-hint" }, "Its JSON output must be turned on (search.formats: json)."));
    const keyHint = h("span", { class: "field-hint" });
    const keyField = h("label", { class: "field" }, h("span", { class: "field-label" }, "API key"), key, keyHint);
    const status = h("div", { class: "field-error", hidden: true });
    const sync = () => {
      urlField.hidden = kind.value !== "searxng";
      keyField.hidden = kind.value !== "brave" && kind.value !== "tavily";
      keyHint.textContent = existing.hasKey && kind.value === existing.kind ? `Saved (${existing.keyHint}). Leave empty to keep it.` : "Required.";
    };
    kind.addEventListener("change", sync);
    sync();
    modal({
      within: ctx.root,
      title: "Web search",
      body: h("div", { class: "ai-form" }, h("label", { class: "field" }, h("span", { class: "field-label" }, "Service"), kind), urlField, keyField, status),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Save",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            try {
              await ctx.api.put("/api/ai/search", {
                kind: kind.value,
                url: url.value.trim(),
                ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
              });
            } catch (error) {
              formError(status, error);
            }
            close();
            await load();
          },
        },
      ],
    });
  }

  // ------------------------------------------------------------- skills
  function renderSkills(data: Overview): void {
    addButton("Add skill", () => editSkill(null));
    rows(
      data.skills.map((s) => {
        const used = data.agents.filter((a) => a.skills.includes(s.id)).length;
        return row({
          title: s.name,
          detail: s.description || `${used} agent${used === 1 ? "" : "s"}`,
          onClick: () => editSkill(s),
          onDelete: () => void remove("skill", s.name, `/api/ai/skills/${s.id}`),
        });
      }),
      "No skills yet. A skill is a set of instructions agents can carry, e.g. how to write release notes.",
    );
  }

  function editSkill(existing: Skill | null): void {
    const name = h("input", { class: "input", value: existing?.name ?? "", autocomplete: "off" });
    const description = h("input", { class: "input", value: existing?.description ?? "", autocomplete: "off", placeholder: "When to use it" });
    const instructions = h("textarea", { class: "textarea ai-prompt", rows: 10, spellcheck: false });
    instructions.value = existing?.instructions ?? "";
    const status = h("div", { class: "field-error", hidden: true });
    modal({
      within: ctx.root,
      title: existing ? existing.name : "Add skill",
      wide: true,
      body: h(
        "div",
        { class: "ai-form" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Description"), description),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Instructions"), instructions, h("span", { class: "field-hint" }, "Markdown. Added to the agent's instructions.")),
        status,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: existing ? "Save" : "Add",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            const payload = { name: name.value.trim(), description: description.value.trim(), instructions: instructions.value };
            try {
              if (existing) await ctx.api.put(`/api/ai/skills/${existing.id}`, payload);
              else await ctx.api.post("/api/ai/skills", payload);
            } catch (error) {
              formError(status, error);
            }
            close();
            await load();
          },
        },
      ],
    });
  }

  // ------------------------------------------------------------- limits
  function subjectLabel(data: Overview, subject: string): string {
    if (subject === "default") return "Everyone";
    const [kind, raw] = subject.split(":");
    const id = Number(raw);
    if (kind === "group") return `Group ${data.groups.find((g) => g.id === id)?.name ?? `#${id}`}`;
    const user = data.users.find((u) => u.id === id);
    return user ? user.displayName || user.username : `Account #${id}`;
  }

  function ruleSummary(data: Overview, rule: LimitRule): string {
    const providers =
      rule.providers === null
        ? "All providers"
        : rule.providers.length
          ? rule.providers.map((id) => data.providers.find((p) => p.id === id)?.name ?? "?").join(", ")
          : "No providers";
    const day = rule.dailyTokens === null ? "no daily limit" : `${formatTokens(rule.dailyTokens)}/day`;
    const month = rule.monthlyTokens === null ? "no monthly limit" : `${formatTokens(rule.monthlyTokens)}/month`;
    return `${providers} · ${day} · ${month}`;
  }

  function renderLimits(data: Overview): void {
    addButton("Add rule", () => editLimit(null));
    body.appendChild(
      h(
        "p",
        { class: "ai-set-note small muted" },
        "An account's own rule wins; otherwise its groups' rules (the most generous); otherwise Everyone. Administrators are not limited.",
      ),
    );
    const order = (s: string) => (s === "default" ? 0 : s.startsWith("group:") ? 1 : 2);
    rows(
      [...data.limits]
        .sort((a, b) => order(a.subject) - order(b.subject) || subjectLabel(data, a.subject).localeCompare(subjectLabel(data, b.subject)))
        .map((rule) =>
          row({
            title: subjectLabel(data, rule.subject),
            detail: ruleSummary(data, rule),
            onClick: () => editLimit(rule),
            onDelete: () => void remove("rule", subjectLabel(data, rule.subject), `/api/ai/limits/${encodeURIComponent(rule.subject)}`),
          }),
        ),
      "No limits: everyone with access may use every provider without a token limit.",
    );
  }

  function editLimit(existing: LimitRule | null): void {
    const data = overview;
    if (!data) return;
    const taken = new Set(data.limits.map((l) => l.subject));
    const who = h("select", { class: "select" });
    const addOption = (value: string, label: string) => {
      if (existing ? value === existing.subject : !taken.has(value)) who.appendChild(h("option", { value }, label));
    };
    addOption("default", "Everyone");
    for (const g of data.groups) addOption(`group:${g.id}`, `Group: ${g.name}`);
    for (const u of data.users.filter((x) => !x.isAdmin)) addOption(`user:${u.id}`, `Account: ${u.displayName || u.username}`);
    who.disabled = Boolean(existing);
    if (!who.options.length) {
      ctx.toast.info("Every account and group already has a rule.");
      return;
    }
    const all = switchControl({ label: "All providers", checked: existing ? existing.providers === null : true });
    const boxes = data.providers.map((p) => {
      const box = h("input", { type: "checkbox", checked: existing?.providers?.includes(p.id) ?? false });
      return { p, box, el: h("label", { class: "ai-check" }, box, h("span", {}, p.name)) };
    });
    const checks = h("div", { class: "ai-checks" }, ...boxes.map((b) => b.el));
    const syncAll = () => checks.classList.toggle("hidden", all.isChecked());
    all.input.addEventListener("change", syncAll);
    syncAll();
    const daily = h("input", { class: "input", inputMode: "numeric", value: existing?.dailyTokens?.toString() ?? "", placeholder: "No limit" });
    const monthly = h("input", { class: "input", inputMode: "numeric", value: existing?.monthlyTokens?.toString() ?? "", placeholder: "No limit" });
    const status = h("div", { class: "field-error", hidden: true });
    modal({
      within: ctx.root,
      title: existing ? subjectLabel(data, existing.subject) : "Add rule",
      body: h(
        "div",
        { class: "ai-form" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "For"), who),
        h("div", { class: "field" }, h("span", { class: "field-label" }, "Providers"), all.el, checks),
        h(
          "div",
          { class: "ai-form-row" },
          h("label", { class: "field" }, h("span", { class: "field-label" }, "Tokens per day"), daily),
          h("label", { class: "field" }, h("span", { class: "field-label" }, "Tokens per month"), monthly),
        ),
        status,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Save",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            const day = numberOrNull(daily.value);
            const month = numberOrNull(monthly.value);
            if (Number.isNaN(day) || Number.isNaN(month)) formError(status, new Error("Token limits are whole numbers."));
            try {
              await ctx.api.put(`/api/ai/limits/${encodeURIComponent(who.value)}`, {
                providers: all.isChecked() ? null : boxes.filter((b) => b.box.checked).map((b) => b.p.id),
                dailyTokens: day === null ? null : Math.round(day),
                monthlyTokens: month === null ? null : Math.round(month),
              });
            } catch (error) {
              formError(status, error);
            }
            close();
            await load();
          },
        },
      ],
    });
  }

  // -------------------------------------------------------------- chats
  let chatUser = "";
  let chatQuery = "";

  async function renderChats(data: Overview): Promise<void> {
    const userSelect = h("select", { class: "select select-sm", ariaLabel: "Account" }, h("option", { value: "" }, "All accounts"));
    for (const u of data.users) userSelect.appendChild(h("option", { value: String(u.id) }, u.displayName || u.username));
    userSelect.value = chatUser;
    const searchInput = h("input", { class: "input input-sm", type: "search", placeholder: "Search", value: chatQuery, autocomplete: "off" });
    const listEl = h("div", { class: "ai-rows" });
    body.append(h("div", { class: "ai-set-filters" }, userSelect, searchInput), listEl);
    let timer = 0;
    const fetchList = async () => {
      try {
        const result = await ctx.api.get<{ items: Conversation[] }>("/api/ai/admin/conversations", {
          ...(chatUser ? { userId: chatUser } : {}),
          ...(chatQuery ? { q: chatQuery } : {}),
        });
        if (page !== "chats") return;
        listEl.replaceChildren(
          ...result.items.map((c) =>
            row({
              title: c.title || "New chat",
              detail: `${c.owner ?? "?"} · ${c.messageCount ?? 0} messages · ${relativeTime(c.updatedAt)}`,
              ...(c.running ? { badge: "Writing" } : {}),
              onClick: () => options.openConversation(c.id),
            }),
          ),
        );
        if (!result.items.length) listEl.appendChild(h("p", { class: "ai-set-empty muted" }, "No chats."));
      } catch (error) {
        listEl.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error)));
      }
    };
    userSelect.addEventListener("change", () => {
      chatUser = userSelect.value;
      void fetchList();
    });
    searchInput.addEventListener("input", () => {
      chatQuery = searchInput.value.trim();
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void fetchList(), 250);
    });
    body.appendChild(h("p", { class: "ai-set-note small muted" }, "Opening someone's chat is recorded in the activity log."));
    await fetchList();
  }

  // -------------------------------------------------------------- usage
  function renderUsage(data: Overview): void {
    const table = h("table", { class: "table ai-usage-table" });
    table.appendChild(h("thead", {}, h("tr", {}, h("th", {}, "Account"), h("th", {}, "Today"), h("th", {}, "This month"), h("th", {}, "Rule"))));
    const tbody = h("tbody", {});
    const rowsData = data.users
      .map((u) => ({ u, used: data.usage.find((x) => x.userId === u.id) ?? { today: 0, month: 0 } }))
      .sort((a, b) => b.used.month - a.used.month || a.u.username.localeCompare(b.u.username));
    for (const { u, used } of rowsData) {
      const own = data.limits.find((l) => l.subject === `user:${u.id}`);
      const rule = u.isAdmin ? "Not limited" : own ? "Own rule" : data.limits.some((l) => l.subject.startsWith("group:")) ? "Group or Everyone" : data.limits.some((l) => l.subject === "default") ? "Everyone" : "Not limited";
      tbody.appendChild(
        h(
          "tr",
          {},
          h("td", {}, u.displayName || u.username),
          h("td", {}, formatTokens(used.today)),
          h("td", {}, formatTokens(used.month)),
          h("td", { class: "muted" }, rule),
        ),
      );
    }
    table.appendChild(tbody);
    body.append(h("div", { class: "ai-table-wrap" }, table), h("p", { class: "ai-set-note small muted" }, "Tokens as the providers report them (estimated when they do not)."));
  }

  void load();
  return {
    refresh: () => void load(),
    destroy: () => {
      disposed = true;
      for (const stop of stops.splice(0)) stop();
    },
  };
}
