// Activity Logs: auditable record of user and system actions.
// OWNER: teammate `frontend-shell` (expand UI).

import type { AppDefinition, AppContext } from "../../core/app";
import { h, clear } from "../../core/dom";
import { formatDateTime } from "../../core/format";
import type { LogEntry, Page } from "../../core/types";
import { errorMessage, modal } from "../../core/ui";

export const logsApp: AppDefinition = {
  id: "logs",
  name: "Activity Logs",
  icon: "logs",
  description: "Who did what, when, and with what result",
  singleton: true,
  defaultSize: { w: 1020, h: 640 },
  mount(ctx) {
    return mountLogs(ctx);
  },
};

async function mountLogs(ctx: AppContext): Promise<void> {
  const filterActor = h("input", { class: "input", type: "search", placeholder: "User…", style: "width:130px" });
  const filterAction = h("select", { class: "select", style: "width:170px" });
  filterAction.appendChild(h("option", { value: "" }, "All actions"));
  const filterResult = h(
    "select",
    { class: "select", style: "width:130px" },
    h("option", { value: "" }, "Any result"),
    h("option", { value: "success" }, "Success"),
    h("option", { value: "failure" }, "Failure"),
    h("option", { value: "denied" }, "Denied"),
  );
  const search = h("input", { class: "input", type: "search", placeholder: "Search path or detail…", style: "width:220px" });
  const refresh = h("button", { class: "btn", type: "button" }, "Refresh");
  const exportButton = h("button", { class: "btn", type: "button" }, "Export CSV");
  const pageLabel = h("div", { class: "small muted" });
  const prev = h("button", { class: "btn btn-sm", type: "button" }, "Previous");
  const next = h("button", { class: "btn btn-sm", type: "button" }, "Next");
  const tableBody = h("tbody");
  const toolbar = h(
    "div",
    { class: "app-toolbar" },
    filterActor,
    filterAction,
    filterResult,
    search,
    refresh,
    h("div", { class: "app-toolbar-spacer" }),
    pageLabel,
    prev,
    next,
    exportButton,
  );
  const content = h(
    "div",
    { class: "table-wrap" },
    h(
      "table",
      { class: "table" },
      h(
        "thead",
        {},
        h("tr", {}, h("th", { style: "width:150px" }, "Time"), h("th", {}, "User"), h("th", {}, "Action"), h("th", {}, "Object"), h("th", {}, "Result"), h("th", {}, "Detail"), h("th", {}, "IP")),
      ),
      tableBody,
    ),
  );
  ctx.root.append(toolbar, content);

  let page = 1;
  const perPage = 50;
  let total = 0;

  const loadActions = async () => {
    try {
      const response = await ctx.api.get<{ items: string[] }>("/api/logs/actions");
      for (const action of response.items) {
        filterAction.appendChild(h("option", { value: action }, action));
      }
    } catch {
      /* filter options are optional */
    }
  };

  const load = async () => {
    clear(tableBody);
    tableBody.appendChild(
      h("tr", {}, h("td", { colspan: 7 }, h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading activity…"))),
    );
    try {
      const response = await ctx.api.get<Page<LogEntry>>("/api/logs", {
        page,
        perPage,
        actor: filterActor.value.trim() || undefined,
        action: filterAction.value || undefined,
        result: filterResult.value || undefined,
        q: search.value.trim() || undefined,
      });
      total = response.total;
      clear(tableBody);
      if (!response.items.length) {
        tableBody.appendChild(h("tr", {}, h("td", { colspan: 7 }, h("div", { class: "table-empty" }, "No activity matches these filters"))));
      }
      for (const entry of response.items) {
        const row = h(
          "tr",
          { style: "cursor:pointer", title: "Show details" },
          h("td", { class: "mono small" }, formatDateTime(entry.createdAt)),
          h("td", {}, entry.username || "—"),
          h("td", {}, h("span", { class: "badge info" }, entry.action)),
          h("td", { class: "mono small", title: entry.objectPath }, entry.objectPath || entry.objectId || entry.objectType || "—"),
          h(
            "td",
            {},
            h(
              "span",
              { class: `badge ${entry.result === "success" ? "ok" : entry.result === "denied" ? "warn" : "error"}` },
              entry.result,
            ),
          ),
          h("td", { class: "small muted" }, entry.detail || "—"),
          h("td", { class: "mono small" }, entry.ip || "—"),
        );
        row.addEventListener("click", () => showDetails(entry));
        tableBody.appendChild(row);
      }
      const pages = Math.max(1, Math.ceil(total / perPage));
      pageLabel.textContent = `Page ${page} of ${pages} · ${total} entries`;
      prev.disabled = page <= 1;
      next.disabled = page >= pages;
    } catch (error) {
      clear(tableBody);
      tableBody.appendChild(
        h("tr", {}, h("td", { colspan: 7 }, h("div", { class: "error-state" }, h("div", { class: "error-title" }, "Failed to load activity"), h("div", { class: "error-detail" }, errorMessage(error))))),
      );
    }
  };

  function showDetails(entry: LogEntry): void {
    modal({
      title: `Activity #${entry.id}`,
      wide: true,
      body: h(
        "dl",
        { class: "kv" },
        h("dt", {}, "Timestamp"),
        h("dd", {}, formatDateTime(entry.createdAt)),
        h("dt", {}, "User"),
        h("dd", {}, `${entry.username || "—"}${entry.userId ? ` (#${entry.userId})` : ""}`),
        h("dt", {}, "Action"),
        h("dd", { class: "mono" }, entry.action),
        h("dt", {}, "Object"),
        h("dd", { class: "mono" }, `${entry.objectType || "—"} ${entry.objectId || ""}`),
        h("dt", {}, "Path"),
        h("dd", { class: "mono" }, entry.objectPath || "—"),
        h("dt", {}, "Result"),
        h("dd", {}, entry.result),
        h("dt", {}, "Detail"),
        h("dd", {}, entry.detail || "—"),
        h("dt", {}, "Client IP"),
        h("dd", { class: "mono" }, entry.ip || "—"),
      ),
    });
  }

  refresh.addEventListener("click", () => void load());
  filterAction.addEventListener("change", () => {
    page = 1;
    void load();
  });
  filterResult.addEventListener("change", () => {
    page = 1;
    void load();
  });
  let timer: number | undefined;
  const debouncedSearch = () => {
    if (timer !== undefined) window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      page = 1;
      void load();
    }, 300);
  };
  filterActor.addEventListener("input", debouncedSearch);
  search.addEventListener("input", debouncedSearch);
  prev.addEventListener("click", () => {
    page = Math.max(1, page - 1);
    void load();
  });
  next.addEventListener("click", () => {
    page += 1;
    void load();
  });
  exportButton.addEventListener("click", () => {
    const params = new URLSearchParams();
    if (filterActor.value.trim()) params.set("actor", filterActor.value.trim());
    if (filterAction.value) params.set("action", filterAction.value);
    if (filterResult.value) params.set("result", filterResult.value);
    if (search.value.trim()) params.set("q", search.value.trim());
    const anchor = document.createElement("a");
    anchor.href = `/api/logs/export?${params.toString()}`;
    anchor.click();
  });

  await loadActions();
  await load();
}
