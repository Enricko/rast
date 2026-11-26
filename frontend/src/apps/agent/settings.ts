// AI Agent settings, for administrators: tiles for providers, agents, skills,
// limits, everyone's chats and usage; each opens its own page.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { switchControl } from "../../core/components";
import { relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { confirmDialog, errorMessage, modal } from "../../core/ui";
import {
  formatTokens,
  type AdminAgent,
  type AdminProvider,
  type Conversation,
  type LimitRule,
  type Overview,
  type Skill,
} from "./model";

export interface SettingsView {
  refresh(): void;
  destroy(): void;
}

type Page = "home" | "providers" | "agents" | "skills" | "limits" | "chats" | "usage";

const PAGES: Array<{ id: Exclude<Page, "home">; label: string; icon: string }> = [
  { id: "providers", label: "Providers", icon: "database" },
  { id: "agents", label: "Agents", icon: "user" },
  { id: "skills", label: "Skills", icon: "layers" },
  { id: "limits", label: "Limits", icon: "shield" },
  { id: "chats", label: "Chats", icon: "list" },
  { id: "usage", label: "Usage", icon: "activity" },
];

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
      agents: `${data.agents.length}`,
      skills: `${data.skills.length}`,
      limits: data.limits.length ? `${data.limits.length} rule${data.limits.length === 1 ? "" : "s"}` : "No limits",
      chats: "All accounts",
      usage: `${formatTokens(usedToday)} today`,
    };
    const grid = h("div", { class: "ai-tiles" });
    for (const item of PAGES) {
      grid.appendChild(
        h(
          "button",
          { class: "ai-tile", type: "button", onClick: () => show(item.id) },
          h("span", { class: "ai-tile-icon" }, iconSvg(item.icon, 22)),
          h("span", { class: "ai-tile-label" }, item.label),
          h("span", { class: "ai-tile-value small muted" }, counts[item.id]),
        ),
      );
    }
    body.appendChild(grid);
    if (!data.providers.length) {
      body.appendChild(
        h(
          "div",
          { class: "ai-set-hint" },
          h("p", {}, "Start by connecting a provider: an API key from Anthropic, OpenAI, DeepSeek or OpenRouter, or a local Ollama."),
          h("button", { class: "btn btn-primary btn-sm", type: "button", onClick: () => editProvider(null) }, "Add a provider"),
        ),
      );
    }
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

  function row(opts: { title: string; detail: string; badge?: string; off?: boolean; onClick: () => void; onDelete?: () => void }): HTMLElement {
    return h(
      "div",
      { class: `ai-row${opts.off ? " is-off" : ""}` },
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
    addButton("Add provider", () => editProvider(null));
    rows(
      data.providers.map((p) =>
        row({
          title: p.name,
          badge: p.enabled ? p.kindLabel : "Off",
          off: !p.enabled,
          detail: `${p.models.length} model${p.models.length === 1 ? "" : "s"} · ${p.effectiveBaseUrl}${p.hasKey ? ` · key ${p.keyHint}` : ""}`,
          onClick: () => editProvider(p),
          onDelete: () => void remove("provider", p.name, `/api/ai/providers/${p.id}`),
        }),
      ),
      "No providers yet.",
    );
  }

  function editProvider(existing: AdminProvider | null): void {
    const data = overview;
    if (!data) return;
    const kind = h("select", { class: "select" });
    for (const k of data.kinds) kind.appendChild(h("option", { value: k.id }, k.label));
    kind.value = existing?.kind ?? "anthropic";
    kind.disabled = Boolean(existing);
    const name = h("input", { class: "input", value: existing?.name ?? "", autocomplete: "off", placeholder: "e.g. Claude (work)" });
    const baseUrl = h("input", { class: "input", value: existing?.baseUrl ?? "", autocomplete: "off", spellcheck: false });
    const key = h("input", { class: "input", type: "password", autocomplete: "new-password", spellcheck: false });
    const removeKey = switchControl({ label: "Remove the saved key" });
    const models = h("textarea", { class: "textarea ai-models", rows: 5, spellcheck: false, placeholder: "One model per line" });
    models.value = (existing?.models ?? []).join("\n");
    const enabled = switchControl({ label: "On", checked: existing?.enabled ?? true });
    const status = h("div", { class: "field-error", hidden: true });
    const picks = h("div", { class: "ai-model-picks hidden" });
    const keyField = h("label", { class: "field" }, h("span", { class: "field-label" }, "API key"), key, h("span", { class: "field-hint" }));

    const kindInfo = () => data.kinds.find((k) => k.id === kind.value);
    const sync = () => {
      const info = kindInfo();
      baseUrl.placeholder = info?.defaultBaseUrl || "https://…/v1";
      const hint = keyField.querySelector(".field-hint")!;
      if (existing?.hasKey) hint.textContent = `Saved (${existing.keyHint}). Leave empty to keep it.`;
      else hint.textContent = info?.needsKey ? "Required." : "Optional for local models.";
      if (!existing && (!name.value || data.kinds.some((k) => k.label === name.value))) name.value = info?.label ?? "";
    };
    kind.addEventListener("change", sync);
    sync();

    const loadButton = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("refresh", 13), h("span", {}, "Load from provider"));
    loadButton.addEventListener("click", async () => {
      loadButton.disabled = true;
      status.hidden = true;
      try {
        const result = await ctx.api.post<{ models: string[] }>("/api/ai/providers/models", {
          ...(existing ? { providerId: existing.id } : { kind: kind.value }),
          ...(baseUrl.value.trim() ? { baseUrl: baseUrl.value.trim() } : {}),
          ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
        });
        showPicks(result.models);
      } catch (error) {
        status.textContent = errorMessage(error);
        status.hidden = false;
      } finally {
        loadButton.disabled = false;
      }
    });

    function chosen(): string[] {
      return models.value.split("\n").map((m) => m.trim()).filter(Boolean);
    }

    function showPicks(all: string[]): void {
      picks.classList.remove("hidden");
      const filter = h("input", { class: "input input-sm", type: "search", placeholder: `Filter ${all.length} models`, autocomplete: "off" });
      const listEl = h("div", { class: "ai-model-list" });
      const draw = () => {
        const q = filter.value.trim().toLowerCase();
        const selected = new Set(chosen());
        listEl.replaceChildren(
          ...all
            .filter((m) => !q || m.toLowerCase().includes(q))
            .slice(0, 300)
            .map((model) => {
              const box = h("input", { type: "checkbox", checked: selected.has(model) });
              box.addEventListener("change", () => {
                const current = chosen().filter((m) => m !== model);
                if (box.checked) current.push(model);
                models.value = current.join("\n");
              });
              return h("label", { class: "ai-model-option" }, box, h("span", {}, model));
            }),
        );
        if (!all.length) listEl.appendChild(h("p", { class: "small muted" }, "The provider listed no models."));
      };
      filter.addEventListener("input", draw);
      picks.replaceChildren(filter, listEl);
      draw();
      filter.focus();
    }

    const form = h(
      "div",
      { class: "ai-form" },
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Kind"), kind),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Base URL"), baseUrl, h("span", { class: "field-hint" }, "Leave empty for the usual address.")),
      keyField,
      existing?.hasKey ? removeKey.el : null,
      h(
        "div",
        { class: "field" },
        h("span", { class: "field-label ai-field-row" }, "Models people can pick", loadButton),
        models,
        picks,
      ),
      enabled.el,
      status,
    );
    modal({
      title: existing ? existing.name : "Add provider",
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
            const payload = {
              kind: kind.value,
              name: name.value.trim(),
              baseUrl: baseUrl.value.trim(),
              ...(key.value.trim() ? { apiKey: key.value.trim() } : {}),
              ...(existing && removeKey.isChecked() && !key.value.trim() ? { removeKey: true } : {}),
              models: chosen(),
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
        { class: "ai-form-row" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Temperature"), temperature, h("span", { class: "field-hint" }, "0 (focused) to 2 (creative)")),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Longest answer"), maxTokens, h("span", { class: "field-hint" }, "Tokens")),
      ),
      enabled.el,
      status,
    );
    modal({
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
