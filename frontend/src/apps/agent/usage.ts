// AI Agent usage: cards for today, this week and this month, a bar per day
// and a table per model, each with what the tokens are worth at API prices.
// Shared by the administrators' page for an account and everyone's own
// Usage view.

import { h } from "../../core/dom";
import { relativeTime } from "../../core/format";
import { formatTokens, formatUsd, type UsageDetail } from "./model";

/** "≈ $1.20" (an estimate at API prices), "≈ $0" when nothing is known. */
export function formatEstimate(value: number | undefined): string {
  return `≈ ${formatUsd(value ?? 0)}`;
}

export function usageCard(label: string, value: string, detail: string): HTMLElement {
  return h(
    "div",
    { class: "ai-usage-card" },
    h("span", { class: "small muted" }, label),
    h("span", { class: "ai-usage-value" }, value),
    h("span", { class: "small muted" }, detail),
  );
}

/** Today, this week and this month: tokens and their estimated cost. */
export function usageTotals(detail: UsageDetail): HTMLElement {
  const a = detail.allowance;
  const t = detail.totals;
  const card = (label: string, tokens: number, estimate: number | undefined) =>
    usageCard(label, `${formatTokens(tokens)} tokens`, `${formatEstimate(estimate)} estimated`);
  return h(
    "div",
    { class: "ai-usage-cards" },
    card("Today", t?.today.tokens ?? a.usedToday, t?.today.estimate ?? a.costToday),
    card("This week", t?.week.tokens ?? a.usedWeek ?? 0, t?.week.estimate ?? a.costWeek),
    card("This month", t?.month.tokens ?? a.usedMonth, t?.month.estimate ?? a.costMonth),
  );
}

/**
 * Use per model: replies, tokens, the estimated cost and (`counted`, for
 * administrators) what counts toward spending limits.
 */
export function usageModels(models: UsageDetail["models"], counted: boolean): HTMLElement {
  if (!models.length) return h("p", { class: "ai-set-empty muted" }, "Nothing used in these days.");
  const head = ["Model", "Replies", "In", "Out", "Est. cost", ...(counted ? ["Counted"] : []), "Last used"];
  const table = h(
    "table",
    { class: "table ai-usage-table" },
    h("thead", {}, h("tr", {}, ...head.map((label, i) => h("th", { class: i > 0 && i < head.length - 1 ? "num" : "" }, label)))),
    h(
      "tbody",
      {},
      ...models.map((m) =>
        h(
          "tr",
          {},
          h("td", {}, h("span", { class: "ai-usage-model" }, m.model || "—"), h("span", { class: "small muted" }, m.provider ?? "Removed provider")),
          h("td", { class: "num" }, String(m.calls)),
          h("td", { class: "num" }, formatTokens(m.input)),
          h("td", { class: "num" }, formatTokens(m.output)),
          h("td", { class: "num" }, formatEstimate(m.estimate ?? m.cost)),
          counted ? h("td", { class: "num" }, formatUsd(m.cost)) : null,
          h("td", { class: "muted" }, relativeTime(m.lastAt)),
        ),
      ),
    ),
  );
  return h("div", { class: "ai-table-wrap" }, table);
}

/** One bar per day (tokens), its estimated cost and replies on hover or focus. */
export function dailyChart(days: UsageDetail["days"]): HTMLElement {
  const max = Math.max(1, ...days.map((d) => d.tokens));
  const dateLabel = (date: string) => new Date(`${date}T12:00:00`).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  const tip = h("div", { class: "ai-chart-tip hidden", role: "status" });
  const plot = h("div", { class: "ai-chart-plot" });
  const showTip = (slot: HTMLElement, day: UsageDetail["days"][number]) => {
    tip.replaceChildren(
      h("strong", {}, `${day.tokens.toLocaleString()} tokens`),
      h("span", {}, `${formatEstimate(day.estimate ?? day.cost)} · ${day.calls} repl${day.calls === 1 ? "y" : "ies"}`),
      h("span", { class: "muted" }, dateLabel(day.date)),
    );
    tip.classList.remove("hidden");
    const box = plot.getBoundingClientRect();
    const at = slot.getBoundingClientRect();
    const center = at.left - box.left + at.width / 2;
    tip.style.left = `${Math.min(Math.max(center, 70), box.width - 70)}px`;
  };
  const hideTip = () => tip.classList.add("hidden");
  for (const day of days) {
    const slot = h(
      "button",
      {
        class: "ai-chart-slot",
        type: "button",
        ariaLabel: `${dateLabel(day.date)}: ${day.tokens} tokens, ${formatEstimate(day.estimate ?? day.cost)}`,
      },
      h("span", { class: "ai-chart-bar", style: `height:${day.tokens ? Math.max(2, (day.tokens / max) * 100) : 0}%` }),
    );
    slot.addEventListener("pointerenter", () => showTip(slot, day));
    // Keyboard focus only: a dialog focusing its first bar shows nothing.
    slot.addEventListener("focus", () => slot.matches(":focus-visible") && showTip(slot, day));
    slot.addEventListener("pointerleave", hideTip);
    slot.addEventListener("blur", hideTip);
    plot.appendChild(slot);
  }
  const used = days.filter((d) => d.tokens > 0);
  const table = h("details", { class: "ai-chart-table" }, h("summary", { class: "small muted" }, "As a table"));
  table.appendChild(
    used.length
      ? h(
          "table",
          { class: "table ai-usage-table" },
          h("thead", {}, h("tr", {}, h("th", {}, "Day"), h("th", { class: "num" }, "Tokens"), h("th", { class: "num" }, "Est. cost"), h("th", { class: "num" }, "Replies"))),
          h(
            "tbody",
            {},
            ...used
              .slice()
              .reverse()
              .map((d) =>
                h(
                  "tr",
                  {},
                  h("td", {}, dateLabel(d.date)),
                  h("td", { class: "num" }, d.tokens.toLocaleString()),
                  h("td", { class: "num" }, formatEstimate(d.estimate ?? d.cost)),
                  h("td", { class: "num" }, String(d.calls)),
                ),
              ),
          ),
        )
      : h("p", { class: "small muted" }, "No use in these days."),
  );
  return h(
    "div",
    { class: "ai-chart" },
    h("div", { class: "ai-chart-scale small muted" }, h("span", {}, formatTokens(max)), h("span", {}, "0")),
    h("div", { class: "ai-chart-area" }, plot, tip),
    h("div", { class: "ai-chart-axis small muted" }, h("span", {}, days[0] ? dateLabel(days[0].date) : ""), h("span", {}, "Today")),
    table,
  );
}
