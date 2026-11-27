// AI Agent settings, for administrators: tiles for providers, agents, skills,
// tools (web search), limits, everyone's chats, model prices and usage; each
// opens its own page.

import type { AppContext } from "../../core/app";
import { appendChildren, h } from "../../core/dom";
import { switchControl } from "../../core/components";
import { relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { confirmDialog, copyText, errorMessage, modal } from "../../core/ui";
import { agentAvatar, providerMark } from "./marks";
import { dailyChart, formatEstimate, usageCard, usageModels, usageTotals } from "./usage";
import {
  allowanceLimits,
  formatTokens,
  formatPrice,
  formatUsd,
  priceRules,
  planLabel,
  groupKinds,
  type Price,
  type PriceListing,
  type PricedModel,
  type Prices,
  type UsageDetail,
  type UsageRow,
  type ProviderKind,
  type SignInDone,
  type SignInFlow,
  type SignInPoll,
  type SignInStarted,
  type AdminAgent,
  type AdminProvider,
  type ProviderAccount,
  type QuotaAccount,
  type QuotaOverview,
  type QuotaWindow,
  type Rotation,
  accountState,
  leftTone,
  resetIn,
  windowLeft,
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

type Page = "home" | "providers" | "quota" | "agents" | "skills" | "tools" | "limits" | "chats" | "prices" | "usage";

const PAGES: Array<{ id: Exclude<Page, "home">; label: string; icon: string; hue: number }> = [
  { id: "providers", label: "Providers", icon: "database", hue: 212 },
  { id: "quota", label: "Quota", icon: "gauge", hue: 20 },
  { id: "agents", label: "Agents", icon: "sparkle", hue: 268 },
  { id: "skills", label: "Skills", icon: "layers", hue: 172 },
  { id: "tools", label: "Tools", icon: "terminal", hue: 32 },
  { id: "limits", label: "Limits", icon: "shield", hue: 145 },
  { id: "chats", label: "Chats", icon: "list", hue: 198 },
  { id: "prices", label: "Prices", icon: "tag", hue: 48 },
  { id: "usage", label: "Usage", icon: "activity", hue: 340 },
];

const SEARCH_LABELS: Record<SearchSettings["kind"], string> = {
  "": "Off",
  brave: "Brave Search",
  tavily: "Tavily",
  searxng: "SearXNG",
};

/**
 * Codex's sign-in copy, used only when the server does not send the plan's
 * own (`site`/`callbackHint`/`risk` in GET /api/ai/admin/overview and in the
 * start response). It keeps the dialog worded exactly as it was before other
 * plans existed; the server's values win as soon as it has them.
 */
const CODEX_SITE = "ChatGPT";
const CODEX_CALLBACK_HINT = "http://localhost:1455/auth/callback?code=…";
const CODEX_RISK =
  "A ChatGPT plan is one person's: only you can chat with it here, and replies count toward your plan's Codex limits. Using a plan outside OpenAI's own apps is not officially supported; OpenAI may restrict the account.";

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
  /** Usage: the account whose use is open (null: everyone's). */
  let usageUser: number | null = null;
  let overview: Overview | null = null;
  let disposed = false;
  /** Quota: its automatic refresh (0: off). */
  let quotaTimer = 0;
  let quotaAuto = false;
  /** Quota's filter: a provider kind, or every provider. */
  let quotaKind = "all";
  const stopQuotaTimer = (): void => {
    if (quotaTimer) window.clearInterval(quotaTimer);
    quotaTimer = 0;
  };
  const stops: Array<() => void> = [];

  const back = h("button", { class: "btn btn-ghost btn-sm ai-set-back", type: "button" }, iconSvg("arrowLeft", 15), h("span", {}, "Chats"));
  const title = h("h2", { class: "ai-set-title" }, "Settings");
  const actions = h("div", { class: "ai-set-actions" });
  const body = h("div", { class: "ai-set-body" });
  host.append(h("header", { class: "ai-set-head" }, back, title, actions), body);
  back.addEventListener("click", () => {
    if (page === "home") options.onClose();
    else if (page === "usage" && usageUser !== null) {
      usageUser = null;
      render();
    } else show("home");
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
    usageUser = null;
    render();
    body.scrollTop = 0;
  }

  function render(): void {
    const label = PAGES.find((p) => p.id === page)?.label;
    title.textContent = label ?? "Settings";
    back.lastElementChild!.textContent = page === "home" ? "Chats" : page === "usage" && usageUser !== null ? "Usage" : "Settings";
    actions.replaceChildren();
    body.replaceChildren();
    if (page !== "quota") stopQuotaTimer();
    if (!overview) {
      body.appendChild(h("div", { class: "ai-set-loading muted" }, "Loading…"));
      return;
    }
    switch (page) {
      case "home":
        return renderHome(overview);
      case "providers":
        return renderProviders(overview);
      case "quota":
        return void renderQuota();
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
      case "prices":
        return void renderPrices();
      case "usage":
        return usageUser === null ? renderUsage(overview) : void renderUsageDetail(overview, usageUser);
    }
  }

  // --------------------------------------------------------------- home
  function renderHome(data: Overview): void {
    const usedToday = data.usage.reduce((sum, u) => sum + u.today, 0);
    const spentToday = data.usage.reduce((sum, u) => sum + u.costToday, 0);
    const counts: Record<Exclude<Page, "home">, string> = {
      providers: data.providers.length ? `${data.providers.length} connected` : "None yet",
      quota: quotaSummary(data.providers),
      agents: data.agents.length ? `${data.agents.length}` : "None yet",
      skills: data.skills.length ? `${data.skills.length}` : "None yet",
      tools: data.search.configured ? `Web search: ${SEARCH_LABELS[data.search.kind]}` : "Files, commands, web pages",
      limits: data.limits.length ? `${data.limits.length} rule${data.limits.length === 1 ? "" : "s"}` : "No limits",
      chats: "All accounts",
      prices: "Per model",
      usage: spentToday > 0 ? `${formatTokens(usedToday)} tokens · ${formatUsd(spentToday)} today` : `${formatTokens(usedToday)} tokens today`,
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
    if (data.providers.some((p) => (p.accounts?.length ?? 0) > 1) || data.rotation?.mode === "round") {
      actions.appendChild(rotationControl(data.rotation ?? { mode: "fill", sticky: 1 }, "Round robin for all", (next) => saveRotation(null, next)));
    }
    addButton("Add provider", () => pickProvider());
    if (!data.providers.length) {
      body.appendChild(connectFirst());
      return;
    }
    const grid = h("div", { class: "ai-cards" });
    for (const p of data.providers) {
      const models = `${p.models.length} model${p.models.length === 1 ? "" : "s"}`;
      const accounts = p.accounts ?? [];
      const keyText = (icon: string, text: string, warn = false) =>
        h("span", { class: `ai-card-key${warn ? " is-warn" : " muted"}`, title: text }, iconSvg(icon, 12), h("span", { class: "ai-card-key-text" }, text));
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
              h("span", { class: "muted ai-card-count" }, models),
              accounts.length > 1
                ? keyText("users", `${accounts.length} accounts`)
                : p.signsIn
                  ? keyText("user", p.signedIn ? p.signedIn.email || "Signed in" : "Not signed in", !p.signedIn)
                  : p.hasKey
                    ? keyText("key", p.keyHint)
                    : null,
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
    const plan = info.group === "plan";
    if (plan && !existing) {
      signIn(null, info);
      return;
    }
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
    // Several accounts (the provider's own key or sign-in is the first).
    const accounts = existing && !local ? accountsField(existing, info) : null;
    const status = h("div", { class: "field-error", hidden: true });
    let selected: string[] = [...(existing?.models ?? [])];
    let offered: string[] | null = null;
    // What the provider's list says its models cost, kept with the
    // provider when it is saved (prices are changed under Prices).
    let listPrices: Prices | null = null;

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
    // Shown when the list is RAST's own because the provider's could not be read.
    const listNote = h("p", { class: "field-hint ai-model-note", hidden: true });
    const loadModels = async (quiet: boolean) => {
      loadButton.disabled = true;
      if (!quiet) status.hidden = true;
      try {
        const result = await ctx.api.post<{ models: string[]; prices?: Prices; source?: "live" | "built-in"; note?: string | null }>("/api/ai/providers/models", {
          ...(existing ? { providerId: existing.id } : { kind: info.id }),
          ...(baseUrl.value.trim() ? { baseUrl: baseUrl.value.trim() } : {}),
          ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
        });
        offered = result.models;
        listPrices = result.prices ?? {};
        listNote.hidden = result.source !== "built-in";
        listNote.textContent =
          result.source === "built-in" ? `These are RAST's built-in models for ${info.label}. Its own list: ${result.note || "unavailable"}` : "";
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
    if (!custom && !plan) advanced.appendChild(urlField);
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
      plan ? null : local ? null : keyField,
      accounts,
      h(
        "div",
        { class: "field" },
        h("span", { class: "field-label ai-field-row" }, "Models people can pick", loadButton),
        chips,
        offerEl,
        listNote,
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
              ...(listPrices ? { listPrices } : {}),
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

  /** After a sign-in succeeded: say who, reload, and open the provider when
   * its plan listed no models (so they can be added by hand). */
  async function connected(done: SignInDone, close: () => void, onDone?: (provider: AdminProvider) => void): Promise<void> {
    const who = done.provider.signedIn;
    ctx.toast.success(who?.email ? `Signed in as ${who.email}` : "Signed in");
    close();
    await load();
    // Added to a provider (or signed in again from its accounts): its
    // dialog shows the account; the provider is set up already.
    if (onDone) {
      onDone(done.provider);
      return;
    }
    if (!done.provider.models.length) {
      const fresh = overview?.providers.find((p) => p.id === done.provider.id);
      if (fresh) editProvider(fresh);
    }
  }

  /**
   * Sign in with a plan. A browser flow opens the provider's page and takes
   * the address it sends the browser to (a localhost page that does not
   * open). A device flow shows a code to enter on the provider's own page and
   * asks the server whether it was entered yet, every `interval` seconds.
   * A new provider is added for this account; `existing` signs in again to
   * that one. Claude's dialog also offers the Claude Code login already on
   * the server.
   */
  function signIn(existing: AdminProvider | null, info: ProviderKind, more: { addTo?: AdminProvider; again?: ProviderAccount; onDone?: (provider: AdminProvider) => void } = {}): void {
    const { addTo, again, onDone } = more;
    const name = h("input", { class: "input", value: existing?.name ?? info.label, autocomplete: "off", placeholder: `e.g. ${info.label} (work)` });
    const status = h("div", { class: "field-error", hidden: true });
    let flow: SignInFlow = info.signInFlow || "browser";
    let started: SignInStarted | null = null;
    let pollTimer = 0;
    let closed = false;
    let close: () => void = () => undefined;

    // What the plan calls its page and the note it shows: the server sends
    // these (overview and start response); the Codex copies keep the old
    // dialog for a server that does not send them yet.
    const site = (): string => started?.site || info.site || (info.id === "codex" ? CODEX_SITE : info.label);
    const callbackHint = (): string => started?.callbackHint || info.callbackHint || (info.id === "codex" ? CODEX_CALLBACK_HINT : "Paste the address here");
    const riskNote = (): string => started?.risk ?? info.risk ?? (info.id === "codex" ? CODEX_RISK : "");

    const clearError = (): void => {
      status.textContent = "";
      status.hidden = true;
    };
    const showError = (error: unknown): void => {
      status.textContent = errorMessage(error);
      status.hidden = false;
    };
    const stopPolling = (): void => {
      if (pollTimer) window.clearTimeout(pollTimer);
      pollTimer = 0;
    };

    // Browser flow: the provider's page, then the address it ends on.
    const openLinkLabel = h("span", {}, "");
    const openLink = h("a", { class: "btn btn-primary ai-sign-in-open is-waiting", target: "_blank", rel: "noopener noreferrer" }, openLinkLabel, iconSvg("external", 13));
    openLink.setAttribute("aria-disabled", "true");
    const pasted = h("input", { class: "input", autocomplete: "off", spellcheck: false });
    openLink.addEventListener("click", (event) => {
      if (openLink.classList.contains("is-waiting")) event.preventDefault();
      else window.setTimeout(() => pasted.focus(), 300);
    });

    // Device flow: the code to enter on the provider's page, then polls.
    const codeEl = h("code", { class: "ai-device-code" }, "…");
    const copyCode = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("copy", 13), h("span", {}, "Copy"));
    copyCode.addEventListener("click", () => {
      const code = started?.userCode ?? "";
      if (!code) return;
      void copyText(code).then((copied) => {
        if (copied) ctx.toast.success("Code copied");
        else ctx.toast.error("Could not copy the code; select it and copy it by hand.");
      });
    });
    const openPageLabel = h("span", {}, "");
    const openPage = h("a", { class: "btn btn-primary ai-sign-in-open is-waiting", target: "_blank", rel: "noopener noreferrer" }, openPageLabel, iconSvg("external", 13));
    openPage.setAttribute("aria-disabled", "true");
    openPage.addEventListener("click", (event) => {
      if (openPage.classList.contains("is-waiting")) event.preventDefault();
    });
    const waitingLabel = h("span", {}, "Waiting…");
    const waiting = h("div", { class: "ai-sign-in-waiting", role: "status" }, h("span", { class: "spinner" }), waitingLabel);
    waiting.hidden = true;

    const riskBanner = (): HTMLElement | null => {
      const note = riskNote();
      return note ? h("div", { class: "banner banner-warning ai-sign-in-risk" }, iconSvg("warning", 15), h("span", {}, note)) : null;
    };
    const browserSteps = (): Array<Node | null> => [
      h("div", { class: "field ai-sign-in-step" }, h("span", { class: "field-label" }, `1. Allow RAST in your ${site()} account`), openLink),
      h(
        "label",
        { class: "field ai-sign-in-step" },
        h("span", { class: "field-label" }, "2. Paste the address you land on"),
        pasted,
        h("span", { class: "field-hint" }, "The page after you allow it will not open (it points at localhost). Copy its whole address from the address bar."),
      ),
      riskBanner(),
    ];
    const deviceSteps = (): Array<Node | null> => [
      h(
        "div",
        { class: "field ai-sign-in-step" },
        h("span", { class: "field-label" }, `1. Open ${site()} and enter this code`),
        h("span", { class: "ai-device-row" }, codeEl, copyCode),
        openPage,
      ),
      waiting,
      riskBanner(),
    ];
    const step = h("div", { class: "ai-sign-in-flow" });
    const paint = (): void => {
      openLinkLabel.textContent = `Continue with ${site()}`;
      pasted.placeholder = callbackHint();
      openPageLabel.textContent = `Open ${site()}`;
      waitingLabel.textContent = `Waiting for you to allow it in ${site()}…`;
      step.replaceChildren();
      appendChildren(step, flow === "device" ? deviceSteps() : browserSteps());
    };
    paint();

    const enableBrowser = (result: SignInStarted): void => {
      if (!result.url) return;
      openLink.setAttribute("href", result.url);
      openLink.classList.remove("is-waiting");
      openLink.removeAttribute("aria-disabled");
    };
    const startDevice = (result: SignInStarted): void => {
      if (result.userCode) codeEl.textContent = result.userCode;
      if (result.verificationUri) {
        openPage.setAttribute("href", result.verificationUri);
        openPage.classList.remove("is-waiting");
        openPage.removeAttribute("aria-disabled");
      }
      waiting.hidden = false;
      pollDevice(result);
    };
    const pollDevice = (result: SignInStarted): void => {
      const interval = Math.max(2, Math.round(result.interval || 5));
      const deadline = Date.now() + Math.max(30, Math.round(result.expiresIn || 900)) * 1000;
      const poll = async (): Promise<void> => {
        pollTimer = 0;
        if (closed) return;
        if (Date.now() >= deadline) {
          waiting.hidden = true;
          showError("That code expired before it was used. Close this and sign in again for a fresh one.");
          return;
        }
        let answer: SignInPoll;
        try {
          answer = await ctx.api.post<SignInPoll>("/api/ai/providers/sign-in/poll", { state: result.state });
        } catch (error) {
          if (closed) return;
          waiting.hidden = true;
          showError(error);
          return;
        }
        if (closed) return;
        if (answer.status === "done") {
          waiting.hidden = true;
          await connected(answer, close, onDone);
          return;
        }
        pollTimer = window.setTimeout(() => void poll(), interval * 1000);
      };
      pollTimer = window.setTimeout(() => void poll(), interval * 1000);
    };

    const connect = async (): Promise<void> => {
      clearError();
      const value = pasted.value.trim();
      if (!value) {
        status.textContent = `Paste the address of the page ${site()} sent you to.`;
        status.hidden = false;
        pasted.focus();
        return;
      }
      try {
        const done = await ctx.api.post<SignInDone>("/api/ai/providers/sign-in/finish", {
          callback: value,
          ...(existing || again ? {} : addTo ? { name: addTo.name } : { name: name.value.trim() }),
        });
        if (closed) return;
        await connected(done, close, onDone);
      } catch (error) {
        if (closed) return;
        // A used or failed sign-in cannot be finished: ask for a new page.
        openLink.classList.add("is-waiting");
        openLink.setAttribute("aria-disabled", "true");
        void startIt();
        showError(error);
      }
    };

    const startIt = async (): Promise<void> => {
      started = null;
      if (flow === "device") stopPolling();
      try {
        const result = await ctx.api.post<SignInStarted>("/api/ai/providers/sign-in", {
          kind: info.id,
          ...(again ? { providerId: again.id } : existing ? { providerId: existing.id } : {}),
          ...(addTo ? { addTo: addTo.id, name: addTo.name } : {}),
        });
        if (closed) return;
        started = result;
        // The kind list says how a kind signs in; follow the server if it
        // knows better (a server from before the plans sends no flow).
        if (result.flow && result.flow !== flow) {
          flow = result.flow;
          // The browser flow's Connect action has nothing to do here.
          if (flow === "device") step.closest(".modal")?.querySelector<HTMLButtonElement>(".modal-foot .btn-primary")?.remove();
        }
        paint();
        if (flow === "device") startDevice(result);
        else enableBrowser(result);
      } catch (error) {
        if (!closed) showError(error);
      }
    };

    // Claude: the login Claude Code already has on this server works too.
    let importField: HTMLElement | null = null;
    if (info.id === "claude" && !addTo && !again) {
      const importButton = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("download", 13), h("span", {}, "Import from Claude Code"));
      importButton.addEventListener("click", async () => {
        clearError();
        importButton.disabled = true;
        try {
          const done = await ctx.api.post<SignInDone>("/api/ai/providers/import/claude-code", {
            ...(existing ? { providerId: existing.id } : { name: name.value.trim() }),
          });
          if (closed) return;
          await connected(done, close, onDone);
        } catch (error) {
          if (!closed) showError(error);
        } finally {
          importButton.disabled = false;
        }
      });
      importField = h(
        "div",
        { class: "field ai-sign-in-alt" },
        h("span", { class: "field-label" }, "Already signed in to Claude Code on this server?"),
        importButton,
        h("span", { class: "field-hint" }, "Uses the login Claude Code keeps on the server (~/.claude/.credentials.json) instead of the browser round trip."),
      );
    }

    const form = h(
      "div",
      { class: "ai-form" },
      h(
        "div",
        { class: "ai-connect-head" },
        providerMark(info.id, 40),
        h("span", { class: "ai-connect-text" }, h("span", { class: "ai-connect-title" }, info.label), h("span", { class: "small muted" }, info.about)),
      ),
      existing || addTo || again ? null : h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
      addTo
        ? h("p", { class: "small muted ai-sign-in-add" }, `Sign in with another ${site()} account. ${addTo.name} then takes turns between its accounts.`)
        : null,
      step,
      importField,
      status,
    );
    const back = {
      label: existing || addTo || again ? "Cancel" : "Back",
      variant: "ghost" as const,
      onClick: existing || addTo || again ? () => undefined : () => pickProvider(),
    };
    const actions = flow === "device" ? [back] : [back, { label: "Connect", variant: "primary" as const, keepOpen: true, onClick: () => connect() }];
    close = modal({
      within: ctx.root,
      title: addTo ? `Add an account to ${addTo.name}` : again ? `Sign in again: ${again.label}` : existing ? `Sign in to ${existing.name}` : `Sign in to ${info.label}`,
      body: form,
      wide: true,
      onClose: () => {
        closed = true;
        stopPolling();
      },
      actions,
    });
    void startIt();
  }

  // ----------------------------------------------------------- accounts

  /**
   * Round robin's switch and Sticky count, as 9router shows them. On: each
   * reply takes the next account (Sticky replies each), so the accounts
   * reach their limits together. Off: the first account answers until it
   * reaches a limit, then the next. Changes apply at once through `save`.
   */
  function rotationControl(rotation: Rotation, label: string, save: (next: Rotation) => Promise<boolean>): HTMLElement {
    let current: Rotation = { mode: rotation.mode, sticky: rotation.sticky || 1 };
    const sticky = h("input", {
      class: "input input-sm ai-sticky",
      type: "number",
      min: "1",
      max: "100",
      value: String(current.sticky),
      ariaLabel: `${label}: replies in a row per account`,
    });
    const stickyRow = h("label", { class: "ai-sticky-row small", title: "Replies in a row on one account before the next takes its turn" }, h("span", {}, "Sticky"), sticky);
    const paint = (): void => {
      stickyRow.hidden = current.mode !== "round";
    };
    const toggle = switchControl({
      label,
      checked: current.mode === "round",
      onChange: (on) =>
        void (async () => {
          const next: Rotation = { ...current, mode: on ? "round" : "fill" };
          toggle.input.disabled = true;
          if (await save(next)) current = next;
          else toggle.setChecked(current.mode === "round");
          toggle.input.disabled = false;
          paint();
        })(),
    });
    toggle.el.classList.add("ai-rotation-switch");
    sticky.addEventListener("change", async () => {
      const value = Math.round(Number(sticky.value));
      const next: Rotation = { ...current, sticky: Number.isFinite(value) ? Math.min(100, Math.max(1, value)) : 1 };
      sticky.value = String(next.sticky);
      if (next.sticky === current.sticky) return;
      if (await save(next)) current = next;
      else sticky.value = String(current.sticky);
    });
    paint();
    return h("div", { class: "ai-rotation" }, toggle.el, stickyRow);
  }

  /** Store a turn order: one provider's, or every provider's (null). */
  async function saveRotation(providerId: string | null, next: Rotation): Promise<boolean> {
    try {
      if (providerId) await ctx.api.put(`/api/ai/providers/${providerId}/rotation`, next);
      else await ctx.api.put("/api/ai/admin/rotation", next);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      return false;
    }
    if (!providerId) ctx.toast.success(next.mode === "round" ? "Round robin is on for every provider" : "Every provider fills its first account first");
    void load();
    return true;
  }

  /** The provider editor's accounts: one row each (its state and an on/off
   * switch; the row opens the account), Add account, and Round robin.
   * Everything here applies at once. */
  function accountsField(existing: AdminProvider, info: ProviderKind): HTMLElement {
    let provider = existing;
    const field = h("div", { class: "field ai-accounts" });
    const list = h("div", { class: "ai-account-rows" });
    const add = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("plus", 13), h("span", {}, "Add account"));
    add.addEventListener("click", () => {
      if (provider.signsIn) signIn(null, info, { addTo: provider, onDone: refreshed });
      else addKeyAccount(provider, refreshed);
    });

    function refreshed(next: AdminProvider): void {
      provider = { ...provider, ...next };
      draw();
    }

    function draw(): void {
      const accounts = provider.accounts ?? [];
      const now = Date.now();
      list.replaceChildren(
        ...accounts.map((account, index) => {
          const state = accountState(account, now, provider.signsIn);
          const toggle = switchControl({
            label: `${account.label} answers`,
            checked: !account.paused,
            onChange: (on) => void setAccount(provider, account, { paused: !on }).then((next) => (next ? refreshed(next) : draw())),
          });
          toggle.el.classList.add("ai-account-switch");
          return h(
            "div",
            { class: `ai-account-row${account.paused ? " is-off" : ""}` },
            h(
              "button",
              { class: "ai-account-main", type: "button", onClick: () => openAccount(provider, info, account.id, refreshed) },
              h("span", { class: "ai-account-order small muted" }, `#${index + 1}`),
              h(
                "span",
                { class: "ai-account-text" },
                h("span", { class: "ai-account-label", title: account.label }, account.label),
                h("span", { class: `ai-account-state small is-${state.tone}` }, state.label),
              ),
              iconSvg("chevronRight", 14),
            ),
            toggle.el,
          );
        }),
      );
      field.replaceChildren(
        h("span", { class: "field-label ai-field-row" }, accounts.length > 1 ? `Accounts (${accounts.length})` : "Account", add),
        list,
        accounts.length > 1
          ? rotationControl(provider.rotation ?? { mode: "fill", sticky: 1 }, "Round robin", async (next) => {
              const ok = await saveRotation(provider.id, next);
              if (ok) provider = { ...provider, rotation: next };
              return ok;
            })
          : h("span", { class: "field-hint" }, "Add another account and replies switch to it when this one reaches its limit."),
      );
    }

    draw();
    return field;
  }

  /** Change one account (set aside, wake, move); returns its provider. */
  async function setAccount(provider: { id: string }, account: { id: string }, body: Record<string, unknown>): Promise<AdminProvider | null> {
    try {
      const result = await ctx.api.put<{ provider: AdminProvider }>(`/api/ai/providers/${provider.id}/accounts/${account.id}`, body);
      void load();
      return result.provider;
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      return null;
    }
  }

  /** Remove one account after asking; returns its provider. */
  async function removeAccount(provider: { id: string; name: string }, account: { id: string; label: string }): Promise<AdminProvider | null> {
    const ok = await confirmDialog({
      within: ctx.root,
      title: "Remove account",
      message: `Remove ${account.label} from ${provider.name}? Its use so far stays in Usage.`,
      confirmLabel: "Remove",
      danger: true,
    });
    if (!ok) return null;
    try {
      const result = await ctx.api.del<{ provider: AdminProvider }>(`/api/ai/providers/${provider.id}/accounts/${account.id}`);
      ctx.toast.success(`Removed ${account.label}`);
      void load();
      return result.provider;
    } catch (error) {
      ctx.toast.error(errorMessage(error));
      return null;
    }
  }

  /** One account: who, its state and limits, and what can be done. */
  function openAccount(provider: AdminProvider, info: ProviderKind, accountId: string, onChanged: (provider: AdminProvider) => void): void {
    const accounts = provider.accounts ?? [];
    const index = accounts.findIndex((a) => a.id === accountId);
    const account = accounts[index];
    if (!account) return;
    const count = accounts.length;
    const now = Date.now();
    const state = accountState(account, now, provider.signsIn);
    let close: () => void = () => undefined;
    const act = (label: string, icon: string, run: () => Promise<unknown> | void, danger = false): HTMLButtonElement => {
      const button = h("button", { class: `btn btn-sm${danger ? " ai-account-danger" : ""}`, type: "button" }, iconSvg(icon, 13), h("span", {}, label));
      button.addEventListener("click", async () => {
        button.disabled = true;
        try {
          await run();
        } finally {
          button.disabled = false;
        }
      });
      return button;
    };
    const apply = async (body: Record<string, unknown>): Promise<void> => {
      const next = await setAccount(provider, account, body);
      if (next) {
        onChanged(next);
        close();
      }
    };
    const buttons: HTMLElement[] = [];
    if (account.resting && account.resting.until > now) buttons.push(act("Use now", "play", () => apply({ wake: true })));
    if (provider.signsIn) {
      buttons.push(
        act(account.signedIn ? "Sign in again" : "Sign in", "refresh", () => {
          close();
          signIn(provider, info, { again: account, onDone: onChanged });
        }),
      );
      if (account.signedIn) {
        buttons.push(
          act("Sign out", "logout", async () => {
            const ok = await confirmDialog({
              within: ctx.root,
              title: "Sign out",
              message:
                count > 1
                  ? `Sign out of ${account.label}? ${provider.name}'s other accounts go on answering.`
                  : `Sign out of "${provider.name}"? Its models stay; it answers again after someone signs in.`,
              confirmLabel: "Sign out",
              danger: true,
            });
            if (!ok) return;
            try {
              await ctx.api.post(`/api/ai/providers/${account.id}/sign-out`);
              ctx.toast.success(`Signed out of ${account.label}`);
              await load();
              const fresh = overview?.providers.find((p) => p.id === provider.id);
              if (fresh) onChanged(fresh);
              close();
            } catch (error) {
              ctx.toast.error(errorMessage(error));
            }
          }),
        );
      }
    }
    if (index > 0) buttons.push(act("Move up", "arrowUp", () => apply({ move: "up" })));
    if (index + 1 < count) buttons.push(act("Move down", "arrowDown", () => apply({ move: "down" })));
    if (count > 1) {
      buttons.push(
        act(
          "Remove",
          "trash",
          async () => {
            const next = await removeAccount(provider, account);
            if (next) {
              onChanged(next);
              close();
            }
          },
          true,
        ),
      );
    }
    const windows = account.quota.windows ?? [];
    close = modal({
      within: ctx.root,
      title: account.label,
      body: h(
        "div",
        { class: "ai-form" },
        h(
          "div",
          { class: "ai-connect-head" },
          providerMark(provider.kind, 40),
          h(
            "span",
            { class: "ai-connect-text" },
            h("span", { class: "ai-connect-title ai-account-label" }, account.label),
            h("span", { class: "small muted" }, `${provider.name} · account #${index + 1}`),
          ),
        ),
        h(
          "div",
          { class: "ai-account-facts" },
          h("span", { class: `ai-account-state is-${state.tone}` }, state.label),
          account.resting && account.resting.until > now ? h("span", { class: "small muted" }, account.resting.reason) : null,
          account.signedIn?.plan ? h("span", { class: "ai-pill" }, planLabel(account.signedIn.plan)) : null,
        ),
        windows.length
          ? h("div", { class: "ai-quota-rows" }, ...windows.map((w) => quotaRow(w, now)))
          : h("p", { class: "small muted" }, "Its limits show here once the provider reports them. Quota has what RAST counted."),
        h("div", { class: "ai-account-actions" }, ...buttons),
      ),
      actions: [{ label: "Close", variant: "ghost", onClick: () => undefined }],
    });
  }

  /** Another API key for a provider. */
  function addKeyAccount(provider: AdminProvider, onDone: (provider: AdminProvider) => void): void {
    const key = h("input", { class: "input", type: "password", autocomplete: "new-password", spellcheck: false, placeholder: "Paste the key", dataset: { autofocus: "true" } });
    const status = h("div", { class: "field-error", hidden: true });
    modal({
      within: ctx.root,
      title: `Add an account to ${provider.name}`,
      body: h(
        "div",
        { class: "ai-form" },
        h("p", { class: "small muted" }, `Another ${provider.kindLabel} key. ${provider.name} then takes turns between its keys.`),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "API key"), key),
        status,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Add",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            let result: { provider: AdminProvider };
            try {
              result = await ctx.api.post<{ provider: AdminProvider }>(`/api/ai/providers/${provider.id}/accounts`, { apiKey: key.value.trim() });
            } catch (error) {
              formError(status, error);
            }
            close();
            ctx.toast.success("Account added");
            onDone(result.provider);
            void load();
          },
        },
      ],
    });
  }

  /** One limit window, as 9router shows it: its name and what is left, a
   * bar coloured by what is left, the counts and when it resets. */
  function quotaRow(w: QuotaWindow, now: number): HTMLElement {
    const left = windowLeft(w);
    const fill = h("span", { class: "ai-quota-fill" });
    fill.style.width = `${left === null ? 0 : left}%`;
    const counts = w.limit !== undefined ? `${formatTokens(w.used ?? 0)} / ${formatTokens(w.limit)}` : left === null ? "" : `${Math.round(100 - left)}% used`;
    const reset = w.resetsAt ? (w.resetsAt > now ? `Resets ${resetIn(w.resetsAt - now)}` : "Reset") : "";
    return h(
      "div",
      { class: `ai-quota-row is-${leftTone(left) || "none"}` },
      h("span", { class: "ai-quota-name", title: w.name }, w.name),
      h("span", { class: "ai-quota-left small" }, left === null ? "—" : `${Math.round(left)}% left`),
      h("span", { class: "ai-quota-bar", role: "meter", ariaLabel: `${w.name}: ${left === null ? "unknown" : `${Math.round(left)}% left`}` }, fill),
      h("span", { class: "ai-quota-used small muted" }, counts),
      h("span", { class: "ai-quota-reset small muted" }, reset),
    );
  }

  /** The Quota tile's line: accounts, and how many rest. */
  function quotaSummary(providers: AdminProvider[]): string {
    const all = providers.flatMap((p) => p.accounts ?? []);
    if (!all.length) return "No accounts yet";
    const now = Date.now();
    const resting = all.filter((a) => a.resting && a.resting.until > now).length;
    const accounts = `${all.length} account${all.length === 1 ? "" : "s"}`;
    return resting ? `${accounts} · ${resting} resting` : accounts;
  }

  // -------------------------------------------------------------- quota

  /**
   * The quota tracker (9router's): filters by provider, Round robin for all,
   * then each provider with its accounts as cards: limits the provider
   * reports (ChatGPT's Codex is asked on Refresh), the account's rest, and
   * what RAST counted per model.
   */
  async function renderQuota(data?: QuotaOverview, ask = false): Promise<void> {
    if (!data) {
      if (!body.childElementCount) body.appendChild(h("div", { class: "ai-set-loading muted" }, "Loading…"));
      try {
        data = ask ? await ctx.api.post<QuotaOverview>("/api/ai/admin/quota/refresh", {}) : await ctx.api.get<QuotaOverview>("/api/ai/admin/quota");
      } catch (error) {
        if (page === "quota") body.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error)));
        return;
      }
      if (disposed || page !== "quota") return;
    }
    const shown = data;
    const scroll = body.scrollTop;
    actions.replaceChildren();
    body.replaceChildren();
    const refresh = h("button", { class: "btn btn-sm", type: "button", title: "Ask the providers for their limits now" }, iconSvg("refresh", 13), h("span", {}, "Refresh all"));
    refresh.addEventListener("click", () => {
      refresh.disabled = true;
      void renderQuota(undefined, true);
    });
    const auto = switchControl({
      label: "Auto-refresh",
      checked: quotaAuto,
      onChange: (on) => {
        quotaAuto = on;
        stopQuotaTimer();
        if (on) startQuotaTimer();
      },
    });
    auto.el.classList.add("ai-quota-auto");
    actions.append(auto.el, refresh);
    if (quotaAuto && !quotaTimer) startQuotaTimer();

    const now = Date.now();
    const kinds = new Map<string, { label: string; count: number }>();
    for (const p of shown.providers) {
      const known = kinds.get(p.kind);
      kinds.set(p.kind, { label: p.kindLabel, count: (known?.count ?? 0) + p.accounts.length });
    }
    if (quotaKind !== "all" && !kinds.has(quotaKind)) quotaKind = "all";
    const total = shown.providers.reduce((sum, p) => sum + p.accounts.length, 0);
    const chip = (id: string, label: string, count: number, mark: HTMLElement | null) =>
      h(
        "button",
        {
          class: `ai-quota-chip${quotaKind === id ? " is-on" : ""}`,
          type: "button",
          ariaPressed: quotaKind === id,
          onClick: () => {
            quotaKind = id;
            void renderQuota(shown);
          },
        },
        mark,
        h("span", {}, label),
        h("span", { class: "ai-quota-chip-count" }, String(count)),
      );
    const toolbar = h(
      "div",
      { class: "ai-quota-toolbar" },
      h(
        "div",
        { class: "ai-quota-chips", role: "group", ariaLabel: "Show providers" },
        chip("all", "All", total, null),
        ...[...kinds.entries()].map(([kind, k]) => chip(kind, k.label, k.count, providerMark(kind, 16))),
      ),
      rotationControl(shown.rotation, "Round robin for all", (next) => saveRotation(null, next)),
    );
    body.appendChild(toolbar);
    if (!total) {
      body.appendChild(h("p", { class: "ai-set-empty muted" }, "Connect a provider under Providers; its accounts show here with their limits."));
      return;
    }
    for (const provider of shown.providers) {
      if (quotaKind !== "all" && provider.kind !== quotaKind) continue;
      const head = h(
        "header",
        { class: "ai-quota-group-head" },
        providerMark(provider.kind, 22),
        h("span", { class: "ai-quota-group-name" }, provider.name),
        h("span", { class: "small muted" }, `${provider.accounts.length} account${provider.accounts.length === 1 ? "" : "s"}${provider.enabled ? "" : " · off"}`),
        provider.accounts.length > 1 ? rotationControl(provider.rotation, "Round robin", (next) => saveRotation(provider.id, next)) : null,
      );
      body.appendChild(
        h(
          "section",
          { class: "ai-quota-group" },
          head,
          h("div", { class: "ai-quota-cards" }, ...provider.accounts.map((account, index) => quotaCard(provider, account, index, now))),
        ),
      );
    }
    body.scrollTop = scroll;
  }

  function startQuotaTimer(): void {
    quotaTimer = window.setInterval(() => {
      if (disposed || page !== "quota") return stopQuotaTimer();
      void renderQuota(undefined, true);
    }, 60_000);
  }

  function quotaCard(provider: QuotaOverview["providers"][number], account: QuotaAccount, index: number, now: number): HTMLElement {
    const state = accountState(account, now, provider.signsIn);
    const count = provider.accounts.length;
    const icon = (label: string, name: string, run: () => void, danger = false) =>
      h("button", { class: `btn btn-ghost btn-icon btn-sm${danger ? " ai-account-danger" : ""}`, type: "button", title: label, ariaLabel: `${label}: ${account.label}`, onClick: run }, iconSvg(name, 14));
    const toggle = switchControl({
      label: `${account.label} answers`,
      checked: !account.paused,
      onChange: (on) =>
        void setAccount(provider, account, { paused: !on }).then((next) => {
          if (next) void renderQuota();
          else toggle.setChecked(!on);
        }),
    });
    toggle.el.classList.add("ai-account-switch");
    const edit = (): void => {
      const full = overview?.providers.find((p) => p.id === provider.id);
      const info = overview?.kinds.find((k) => k.id === provider.kind);
      if (full && info) openAccount(full, info, account.id, () => void renderQuota());
    };
    const tools = h(
      "span",
      { class: "ai-quota-tools" },
      account.canCheck
        ? icon("Refresh", "refresh", () =>
            void ctx.api
              .post<QuotaOverview>("/api/ai/admin/quota/refresh", { accountId: account.id })
              .then((next) => {
                if (!disposed && page === "quota") void renderQuota(next);
              })
              .catch((error) => ctx.toast.error(errorMessage(error))),
          )
        : null,
      icon("Edit", "edit", edit),
      count > 1 ? icon("Remove", "trash", () => void removeAccount(provider, account).then((next) => next && void renderQuota()), true) : null,
      toggle.el,
    );
    const windows = account.quota.windows ?? [];
    const resting = account.resting && account.resting.until > now ? account.resting : null;
    const status = h(
      "div",
      { class: "ai-quota-state small" },
      h("span", { class: `ai-account-state is-${state.tone}` }, state.label),
      resting ? h("span", { class: "muted" }, resting.reason) : null,
      resting
        ? h(
            "button",
            { class: "btn btn-sm", type: "button", onClick: () => void setAccount(provider, account, { wake: true }).then((next) => next && void renderQuota()) },
            "Use now",
          )
        : null,
    );
    // What RAST counted per model (the last 5 hours), when the provider
    // reports no limits of its own.
    const models = windows.length
      ? null
      : account.models.length
        ? h(
            "div",
            { class: "ai-quota-models" },
            h("span", { class: "ai-quota-models-head small muted" }, h("span", {}, "Model"), h("span", {}, "5 h"), h("span", {}, "7 days")),
            ...account.models.slice(0, 5).map((m) =>
              h(
                "div",
                { class: "ai-quota-model small" },
                h("span", { class: "ai-quota-model-name", title: m.model }, m.model),
                h("span", { title: `${m.hours5.calls} call${m.hours5.calls === 1 ? "" : "s"}` }, formatTokens(m.hours5.tokens)),
                h("span", { class: "muted", title: `${m.week.calls} call${m.week.calls === 1 ? "" : "s"}` }, formatTokens(m.week.tokens)),
              ),
            ),
          )
        : h("p", { class: "ai-quota-quiet small muted" }, "Unused in the last 7 days");
    const totals = account.counted;
    const foot = h(
      "div",
      { class: "ai-quota-foot small muted" },
      h("span", {}, `${formatTokens(totals.hours5.tokens)} tokens in 5 h · ${formatTokens(totals.week.tokens)} in 7 days`),
      account.checkError
        ? h("span", { class: "ai-quota-error", title: account.checkError }, iconSvg("warning", 12), account.checkError)
        : account.quota.checkedAt
          ? h("span", {}, `Checked ${relativeTime(account.quota.checkedAt)}`)
          : null,
    );
    return h(
      "article",
      { class: `ai-quota-card${account.paused || !provider.enabled ? " is-off" : ""}` },
      h(
        "header",
        { class: "ai-quota-head" },
        providerMark(provider.kind, 30),
        h(
          "span",
          { class: "ai-card-names" },
          h("span", { class: "ai-card-title", title: account.label }, account.label),
          h(
            "span",
            { class: "ai-quota-sub small muted" },
            account.signedIn?.plan ? h("span", { class: "ai-pill" }, planLabel(account.signedIn.plan)) : null,
            h("span", {}, count > 1 ? `#${index + 1}` : provider.kindLabel),
          ),
        ),
        tools,
      ),
      status,
      windows.length ? h("div", { class: "ai-quota-rows" }, ...windows.map((w) => quotaRow(w, now))) : null,
      models,
      foot,
    );
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
      [
        "browser",
        "Browser",
        data.browser?.chrome
          ? "Chrome on this server for pages that need clicks and forms, with a screenshot of each step. Public web only; it runs boxed in as an account without privileges, once Terminal is turned on."
          : "Opens pages, clicks and fills in forms with a screenshot of each step, once Google Chrome is installed on this server (Linux 64-bit .deb) and Terminal is turned on.",
      ],
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
    const caps: string[] = [];
    const per = { day: "day", week: "week", month: "month" } as const;
    const tokens = (value: number | null, when: keyof typeof per) => value !== null && caps.push(`${formatTokens(value)} tokens/${per[when]}`);
    const money = (value: number | null, when: keyof typeof per) => value !== null && caps.push(`${formatUsd(value)}/${per[when]}`);
    tokens(rule.dailyTokens, "day");
    tokens(rule.weeklyTokens ?? null, "week");
    tokens(rule.monthlyTokens, "month");
    money(rule.dailyCost ?? null, "day");
    money(rule.weeklyCost ?? null, "week");
    money(rule.monthlyCost ?? null, "month");
    return `${providers} · ${caps.length ? caps.join(" · ") : "no limits on use"}`;
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
    const field = (value: number | null | undefined, mode: "numeric" | "decimal") =>
      h("input", { class: "input", inputMode: mode, value: value === null || value === undefined ? "" : String(value), placeholder: "No limit" });
    const daily = field(existing?.dailyTokens, "numeric");
    const weekly = field(existing?.weeklyTokens, "numeric");
    const monthly = field(existing?.monthlyTokens, "numeric");
    const dailyCost = field(existing?.dailyCost, "decimal");
    const weeklyCost = field(existing?.weeklyCost, "decimal");
    const monthlyCost = field(existing?.monthlyCost, "decimal");
    const cell = (label: string, input: HTMLElement) => h("label", { class: "field" }, h("span", { class: "field-label" }, label), input);
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
          "fieldset",
          { class: "ai-limit-set" },
          h("legend", { class: "field-label" }, "Tokens"),
          h("div", { class: "ai-form-row ai-form-row-3" }, cell("Per day", daily), cell("Per week", weekly), cell("Per month", monthly)),
        ),
        h(
          "fieldset",
          { class: "ai-limit-set" },
          h("legend", { class: "field-label" }, "Spending (USD)"),
          h("div", { class: "ai-form-row ai-form-row-3" }, cell("Per day", dailyCost), cell("Per week", weeklyCost), cell("Per month", monthlyCost)),
          h("span", { class: "field-hint" }, "Counted from each model's price under Prices. Weeks start on Monday."),
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
            const tokens = [daily, weekly, monthly].map((el) => numberOrNull(el.value));
            const money = [dailyCost, weeklyCost, monthlyCost].map((el) => numberOrNull(el.value.replace("$", "")));
            if (tokens.some((v) => Number.isNaN(v))) formError(status, new Error("Token limits are whole numbers."));
            if (money.some((v) => Number.isNaN(v))) formError(status, new Error("Spending limits are amounts in dollars, such as 2.50."));
            const whole = (v: number | null | undefined) => (v === null || v === undefined ? null : Math.round(v));
            try {
              await ctx.api.put(`/api/ai/limits/${encodeURIComponent(who.value)}`, {
                providers: all.isChecked() ? null : boxes.filter((b) => b.box.checked).map((b) => b.p.id),
                dailyTokens: whole(tokens[0]),
                weeklyTokens: whole(tokens[1]),
                monthlyTokens: whole(tokens[2]),
                dailyCost: money[0] ?? null,
                weeklyCost: money[1] ?? null,
                monthlyCost: money[2] ?? null,
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
  /** Everyone together (cards, a bar per day, each model), then each
   * account, which opens its own page. */
  function renderUsage(data: Overview): void {
    const none = { today: 0, week: 0, month: 0, costToday: 0, costWeek: 0, costMonth: 0, lastAt: 0 };
    const rowsData = data.users
      .map((u) => ({ u, used: data.usage.find((x) => x.userId === u.id) ?? { ...none, userId: u.id } }))
      .sort(
        (a, b) =>
          (b.used.estimateMonth ?? 0) - (a.used.estimateMonth ?? 0) || b.used.month - a.used.month || a.u.username.localeCompare(b.u.username),
      );
    const sum = (pick: (u: UsageRow) => number | undefined) => data.usage.reduce((total, u) => total + (pick(u) ?? 0), 0);
    const card = (label: string, tokens: number, estimate: number, counted: number) =>
      usageCard(label, `${formatTokens(tokens)} tokens`, `${formatEstimate(estimate)} estimated${counted ? ` · ${formatUsd(counted)} counted` : ""}`);
    body.appendChild(h("h3", { class: "ai-set-subtitle ai-usage-first" }, "Everyone"));
    body.appendChild(
      h(
        "div",
        { class: "ai-usage-cards" },
        card("Today", sum((u) => u.today), sum((u) => u.estimateToday), sum((u) => u.costToday)),
        card("This week", sum((u) => u.week), sum((u) => u.estimateWeek), sum((u) => u.costWeek)),
        card("This month", sum((u) => u.month), sum((u) => u.estimateMonth), sum((u) => u.costMonth)),
        usageCard("Accounts that used it", String(data.usage.filter((u) => u.month > 0).length), `of ${data.users.length} this month`),
      ),
    );

    // Each account: its use and cost; a row opens its days and models.
    body.appendChild(h("h3", { class: "ai-set-subtitle" }, "Each account"));
    const table = h("table", { class: "table ai-usage-table" });
    table.appendChild(
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", {}, "Account"),
          h("th", { class: "num" }, "Today"),
          h("th", { class: "num" }, "This week"),
          h("th", { class: "num" }, "This month"),
          h("th", { class: "num" }, "Est. cost, month"),
          h("th", { class: "num" }, "Counted, month"),
          h("th", {}, "Last used"),
        ),
      ),
    );
    const tbody = h("tbody", {});
    for (const { u, used } of rowsData) {
      const open = () => {
        usageUser = u.id;
        render();
        body.scrollTop = 0;
      };
      tbody.appendChild(
        h(
          "tr",
          { class: "ai-usage-row" },
          h("td", {}, h("button", { class: "ai-usage-open", type: "button", onClick: open }, u.displayName || u.username, iconSvg("chevronRight", 12))),
          h("td", { class: "num" }, formatTokens(used.today)),
          h("td", { class: "num" }, formatTokens(used.week)),
          h("td", { class: "num" }, formatTokens(used.month)),
          h("td", { class: "num" }, formatEstimate(used.estimateMonth)),
          h("td", { class: "num muted" }, formatUsd(used.costMonth)),
          h("td", { class: "muted" }, used.lastAt ? relativeTime(used.lastAt) : "—"),
        ),
      );
    }
    table.appendChild(tbody);
    body.appendChild(h("div", { class: "ai-table-wrap" }, table));

    // Everyone's days and models, once loaded.
    const later = h("div", { class: "ai-usage-later" }, h("p", { class: "ai-set-loading muted" }, "Loading…"));
    body.append(
      later,
      h(
        "p",
        { class: "ai-set-note small muted" },
        "Est. cost is the tokens at each model's API price, signed-in plans included. Counted is what spending limits count (a signed-in plan counts nothing). Open an account for its own days and models.",
      ),
    );
    void ctx.api
      .get<UsageDetail>("/api/ai/admin/usage", { days: "30" })
      .then((detail) => {
        if (page !== "usage" || usageUser !== null) return;
        later.replaceChildren(
          h("h3", { class: "ai-set-subtitle" }, "Tokens per day, everyone, last 30 days"),
          dailyChart(detail.days),
          h("h3", { class: "ai-set-subtitle" }, "By model, everyone, last 30 days"),
          usageModels(detail.models, true),
        );
      })
      .catch((error: unknown) => later.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error))));
  }

  // ------------------------------------------------------------- prices
  const SOURCE_TEXT: Record<PricedModel["source"], string> = {
    set: "Set here",
    list: "Provider's list",
    "built-in": "Built-in",
    local: "Free (local)",
    plan: "In your plan",
    none: "No price",
  };

  /** Input, output, cached and cache-write cells for a price. */
  function priceCells(price: Price | null): HTMLElement[] {
    if (!price) return [0, 1, 2, 3].map(() => h("td", { class: "num muted" }, "—"));
    return [price.input, price.output, price.cached ?? price.input, price.cacheWrite ?? price.input].map((value, i) =>
      h("td", { class: `num${i > 1 && (i === 2 ? price.cached : price.cacheWrite) === undefined ? " muted" : ""}` }, formatPrice(value)),
    );
  }

  function priceHead(first: string, last: string | null): HTMLElement {
    return h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", {}, first),
        h("th", { class: "num" }, "Input"),
        h("th", { class: "num" }, "Output"),
        h("th", { class: "num" }, "Cached"),
        h("th", { class: "num" }, "Cache write"),
        last === null ? null : h("th", {}, last),
      ),
    );
  }

  async function renderPrices(listing?: PriceListing): Promise<void> {
    if (!listing) {
      body.appendChild(h("div", { class: "ai-set-loading muted" }, "Loading…"));
      try {
        listing = await ctx.api.get<PriceListing>("/api/ai/prices");
      } catch (error) {
        if (page === "prices") body.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error)));
        return;
      }
      if (disposed || page !== "prices") return;
    }
    const data = listing;
    actions.replaceChildren();
    body.replaceChildren();
    const refresh = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("refresh", 13), h("span", {}, "Update prices"));
    refresh.title = "Ask each provider's model list for its prices now";
    refresh.addEventListener("click", async () => {
      refresh.disabled = true;
      try {
        const next = await ctx.api.post<PriceListing>("/api/ai/prices/refresh", {});
        if (disposed || page !== "prices") return;
        await renderPrices(next);
        const failed = (next.results ?? []).filter((r) => r.error);
        if (failed.length) {
          body.prepend(h("div", { class: "banner banner-error" }, failed.map((r) => `${r.name}: ${r.error}`).join(" · ")));
        }
      } catch (error) {
        refresh.disabled = false;
        body.prepend(h("div", { class: "banner banner-error" }, errorMessage(error)));
      }
    });
    actions.appendChild(refresh);
    body.appendChild(
      h(
        "p",
        { class: "ai-set-note ai-price-note small muted" },
        "USD per million tokens. A price set here comes first, then the provider's own list (checked daily), then the built-in list. Cached input is what the provider reads back from its prompt cache; reasoning counts as output.",
      ),
    );
    if (!data.providers.length) body.appendChild(h("p", { class: "ai-set-empty muted" }, "Connect a provider to see its models' prices."));
    for (const provider of data.providers) {
      const checked = provider.local ? "Runs locally: free" : provider.checkedAt ? `List checked ${relativeTime(provider.checkedAt)}` : "List not checked yet";
      const table = h("table", { class: "table ai-usage-table ai-price-table" }, priceHead("Model", "From"));
      const tbody = h("tbody", {});
      for (const item of provider.models) {
        const from = h("span", { class: `ai-price-from is-${item.source}` }, SOURCE_TEXT[item.source]);
        if (item.match) from.title = `Built-in: ${item.match}`;
        tbody.appendChild(
          h(
            "tr",
            { class: "ai-usage-row" },
            h(
              "td",
              {},
              h("button", { class: "ai-usage-open", type: "button", onClick: () => editPrice(provider, item) }, item.model, iconSvg("edit", 12)),
              ...(item.price ? priceRules(item.price) : []).map((rule) => h("span", { class: "ai-price-rule small muted" }, rule)),
            ),
            ...priceCells(item.price),
            h("td", {}, from),
          ),
        );
      }
      if (!provider.models.length) tbody.appendChild(h("tr", {}, h("td", { class: "muted", colspan: 6 }, "No models chosen.")));
      table.appendChild(tbody);
      body.appendChild(
        h(
          "section",
          { class: `ai-price-provider${provider.enabled ? "" : " is-off"}` },
          h(
            "div",
            { class: "ai-price-head" },
            providerMark(provider.kind, 26),
            h("span", { class: "ai-card-names" }, h("span", { class: "ai-card-title" }, provider.name), h("span", { class: "ai-card-sub small muted" }, `${provider.kindLabel} · ${checked}`)),
          ),
          h("div", { class: "ai-table-wrap" }, table),
        ),
      );
    }
    body.appendChild(builtInList(data));
  }

  /** The built-in list, searchable. */
  function builtInList(data: PriceListing): HTMLElement {
    const all = [...data.builtIn.models.map((p) => ({ ...p, key: p.name ?? "", isPattern: false })), ...data.builtIn.patterns.map((p) => ({ ...p, key: p.pattern ?? "", isPattern: true }))];
    const search = h("input", { class: "input input-sm ai-price-search", type: "search", placeholder: `Search ${all.length} names and patterns`, ariaLabel: "Search the built-in prices", spellcheck: false });
    const tbody = h("tbody", {});
    const draw = () => {
      const q = search.value.trim().toLowerCase();
      const shown = all.filter((p) => !q || p.key.toLowerCase().includes(q));
      tbody.replaceChildren(
        ...shown.map((p) =>
          h(
            "tr",
            {},
            h("td", {}, h("span", { class: p.isPattern ? "ai-price-pattern" : "" }, p.key)),
            ...priceCells({ input: p.input, output: p.output, cached: p.cached, cacheWrite: p.cacheWrite }),
          ),
        ),
      );
      if (!shown.length) tbody.appendChild(h("tr", {}, h("td", { class: "muted", colspan: 5 }, "Nothing matches.")));
    };
    search.addEventListener("input", draw);
    draw();
    return h(
      "details",
      { class: "ai-advanced ai-price-builtin" },
      h("summary", {}, `Built-in list · ${data.builtIn.models.length} models, ${data.builtIn.patterns.length} patterns`),
      h("p", { class: "small muted" }, "Used for models the provider's list does not price. Names are matched without case, a vendor prefix, a date or an effort; patterns (* matches anything) after, first match wins. From 9router's price table, with Anthropic's published prices for Claude."),
      search,
      h("div", { class: "ai-table-wrap" }, h("table", { class: "table ai-usage-table ai-price-table" }, priceHead("Model or pattern", null), tbody)),
    );
  }

  function editPrice(provider: PriceListing["providers"][number], item: PricedModel): void {
    const base = item.source === "set" ? item.fallback : { price: item.price, source: item.source, match: item.match };
    const current = item.source === "set" ? item.price : null;
    const status = h("div", { class: "field-error", hidden: true });
    const field = (label: string, value: number | undefined, fallback: number | undefined) => {
      const input = h("input", {
        class: "input",
        inputMode: "decimal",
        placeholder: fallback === undefined ? "" : String(fallback),
        value: value === undefined ? "" : String(value),
        ariaLabel: `${label}, USD per million tokens`,
      });
      return { input, el: h("label", { class: "field" }, h("span", { class: "field-label" }, label), h("span", { class: "ai-price-input" }, h("span", { class: "muted" }, "$"), input)) };
    };
    const bp = base?.price ?? null;
    const input = field("Input", current?.input, bp?.input);
    const output = field("Output", current?.output, bp?.output);
    const cached = field("Cached input", current?.cached, bp?.cached ?? bp?.input);
    const write = field("Cache write", current?.cacheWrite, bp?.cacheWrite ?? bp?.input);
    // Peak hours and long-prompt prices stay as they are.
    const rules = (current ?? bp) ? priceRules((current ?? bp)!) : [];
    const otherwise =
      base && base.source !== "none"
        ? `Without a price here: ${SOURCE_TEXT[base.source].toLowerCase()}${base.match ? ` (${base.match})` : ""}${bp ? `, ${formatPrice(bp.input)} in, ${formatPrice(bp.output)} out` : ""}.`
        : "Without a price here it counts as free.";
    const save = async (price: Price | null, close: () => void) => {
      try {
        const next = await ctx.api.put<PriceListing>(`/api/ai/providers/${provider.id}/prices`, { model: item.model, price });
        close();
        if (!disposed && page === "prices") await renderPrices(next);
      } catch (error) {
        formError(status, error);
      }
    };
    modal({
      within: ctx.root,
      title: item.model,
      body: h(
        "div",
        { class: "ai-form" },
        h("p", { class: "small muted" }, `${provider.name}. USD per million tokens; empty fields take the value shown.`),
        h("div", { class: "ai-form-row" }, input.el, output.el),
        h("div", { class: "ai-form-row" }, cached.el, write.el),
        rules.length ? h("p", { class: "small muted" }, `Kept as they are: ${rules.join("; ")}.`) : null,
        h("p", { class: "small muted" }, otherwise),
        status,
      ),
      actions: [
        ...(item.source === "set"
          ? [{ label: "Remove this price", variant: "ghost" as const, keepOpen: true, onClick: (close: () => void) => save(null, close) }]
          : []),
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Save",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            status.hidden = true;
            const read = (f: { input: HTMLInputElement }, fallback: number | undefined) => {
              const value = numberOrNull(f.input.value);
              if (Number.isNaN(value) || (value !== null && value < 0)) formError(status, new Error("Prices are a number of dollars, 0 or more."));
              return value ?? fallback;
            };
            const keep = current ?? bp;
            const price: Price = {
              input: read(input, bp?.input) ?? 0,
              output: read(output, bp?.output) ?? 0,
              ...(keep?.tiers?.length ? { tiers: keep.tiers } : {}),
              ...(keep?.multipliers?.length ? { multipliers: keep.multipliers } : {}),
            };
            const c = read(cached, bp?.cached);
            const w = read(write, bp?.cacheWrite);
            if (c !== undefined) price.cached = c;
            if (w !== undefined) price.cacheWrite = w;
            await save(price, close);
          },
        },
      ],
    });
  }

  async function renderUsageDetail(data: Overview, userId: number): Promise<void> {
    const account = data.users.find((u) => u.id === userId);
    title.textContent = account ? account.displayName || account.username : "Usage";
    body.appendChild(h("div", { class: "ai-set-loading muted" }, "Loading…"));
    let detail: UsageDetail;
    try {
      detail = await ctx.api.get<UsageDetail>(`/api/ai/admin/usage/${userId}`, { days: "30" });
    } catch (error) {
      body.replaceChildren(h("div", { class: "banner banner-error" }, errorMessage(error)));
      return;
    }
    if (page !== "usage" || usageUser !== userId) return;
    body.replaceChildren();
    const a = detail.allowance;
    body.appendChild(usageTotals(detail));
    // Its limits, each with how much is used.
    const limits = allowanceLimits(a);
    const source: Record<string, string> = { admin: "Administrator: not limited", user: "Its own rule", group: "Its groups' rules", default: "The rule for everyone", none: "No rule: not limited" };
    body.appendChild(h("h3", { class: "ai-set-subtitle" }, "Limits"));
    body.appendChild(
      h(
        "div",
        { class: "ai-usage-limits" },
        h("p", { class: "small muted" }, source[a.source] ?? a.source),
        ...limits.map((limit) =>
          h(
            "div",
            { class: `ai-usage-limit${limit.share >= 1 ? " is-full" : ""}` },
            h("span", { class: "small" }, limit.text),
            h("span", { class: "ai-allowance-bar" }, h("span", { style: `width:${Math.round(Math.min(1, limit.share) * 100)}%` })),
          ),
        ),
      ),
    );
    body.appendChild(h("h3", { class: "ai-set-subtitle" }, "Tokens per day, last 30 days"));
    body.appendChild(dailyChart(detail.days));
    body.appendChild(h("h3", { class: "ai-set-subtitle" }, "By model, last 30 days"));
    body.appendChild(usageModels(detail.models, true));
    body.appendChild(h("p", { class: "ai-set-note small muted" }, "Est. cost is the tokens at API prices (signed-in plans included); Counted is what spending limits count."));
  }

  void load();
  return {
    refresh: () => void load(),
    destroy: () => {
      disposed = true;
      stopQuotaTimer();
      for (const stop of stops.splice(0)) stop();
    },
  };
}
