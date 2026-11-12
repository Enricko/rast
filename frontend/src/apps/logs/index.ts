// Activity Logs: the server's audit trail with server-side filters, pagination,
// row details and CSV export — updated live from `audit.created` events.
//
// Live behaviour:
//   * page 1 + matching filters + scrolled to the top  -> prepend the new row
//   * page 1 + matching filters + scrolled down        -> "new activity" pill
//   * other page / filters exclude the entry           -> one-click jump pill
// The routine Refresh button and auto-refresh toggle are gone; `resync` after a
// reconnect refetches the current page.

import type { AppContext, AppDefinition } from "../../core/app";
import { apiErrorMessage } from "../../core/api";
import { badge, pager, type Pager } from "../../core/components";
import { clear, debounce, h } from "../../core/dom";
import { endOfDay, formatDateTime, formatDateTimeSec, formatNumberCompact, formatTime, relativeTime, startOfDay } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { getRealtime } from "../../core/realtime";
import type { LogEntry, Page } from "../../core/types";
import { copyText, modal } from "../../core/ui";

const PER_PAGE_OPTIONS = [25, 50, 100];
const DEFAULT_PER_PAGE = 50;
const MAX_ACTIONS = 200;
const MAX_PENDING = 100;

interface Filters {
  actor: string;
  action: string;
  result: string;
  q: string;
  from: string;
  to: string;
}

const EMPTY_FILTERS: Filters = { actor: "", action: "", result: "", q: "", from: "", to: "" };

type NewActivityReason = "scroll" | "page" | "filters";

export const logsApp: AppDefinition = {
  id: "logs",
  name: "Activity Logs",
  icon: "logs",
  description: "Audit trail of sign-ins, file operations and administrative actions",
  singleton: true,
  defaultSize: { w: 1080, h: 680 },
  minSize: { w: 420, h: 360 },
  mount(ctx) {
    void mountLogs(ctx).catch((error) => {
      console.error("Activity Logs failed to start", error);
    });
  },
};

function humanizeAction(action: string): string {
  const known: Record<string, string> = {
    "auth.login": "Sign in",
    "auth.logout": "Sign out",
    "auth.password_change": "Password change",
    "auth.session_revoke": "Session revoked",
    "auth.session_revoke_all": "All sessions revoked",
    "user.create": "User created",
    "user.update": "User updated",
    "user.deactivate": "User disabled",
    "user.password_reset": "Password reset",
    "user.sessions_revoke": "User sessions revoked",
    "settings.update": "Settings updated",
    setup: "First-run setup",
    "file.upload": "File uploaded",
    "file.download": "File downloaded",
    "file.delete": "File deleted",
    "file.rename": "File renamed",
    "file.move": "File moved",
    "file.copy": "File copied",
    "file.mkdir": "Folder created",
    "file.restore": "File restored",
    "file.purge": "Recycle bin emptied",
    "backup.create": "Backup started",
    "backup.delete": "Backup deleted",
    "app.deploy": "App deployed",
    "app.start": "App started",
    "app.stop": "App stopped",
    "app.delete": "App deleted",
    "docker.container.start": "Container started",
    "docker.container.stop": "Container stopped",
    "docker.container.restart": "Container restarted",
    "docker.container.remove": "Container deleted",
    "docker.container.exec": "Container terminal opened",
    "docker.image.pull": "Image pulled",
    "docker.image.remove": "Image deleted",
    "docker.image.prune": "Unused images removed",
    "docker.volume.remove": "Volume deleted",
    "share.create": "Shared folder created",
    "share.update": "Shared folder updated",
    "share.delete": "Shared folder deleted",
    "appearance.update": "Appearance updated",
    "appearance.wallpaper_upload": "Wallpaper uploaded",
    "appearance.wallpaper_delete": "Wallpaper deleted",
  };
  if (known[action]) return known[action];
  return action
    .split(".")
    .map((part) => (part ? part.charAt(0).toUpperCase() + part.slice(1) : part))
    .join(" · ");
}

function resultBadge(result: string): HTMLElement {
  if (result === "success") return badge("success", "ok");
  if (result === "denied") return badge("denied", "warn");
  if (result === "failure") return badge("failure", "error");
  return badge(result || "—", "neutral");
}

async function mountLogs(ctx: AppContext): Promise<() => void> {
  const isAdmin = ctx.store.isAdmin();
  const viewerId = ctx.store.get().user?.id ?? null;
  const filters: Filters = { ...EMPTY_FILTERS };
  let page = 1;
  let perPage = DEFAULT_PER_PAGE;
  let total = 0;
  let loading = false;
  let loadToken = 0;
  let disposed = false;
  let entries: LogEntry[] = [];
  let pending: LogEntry[] = [];
  const seenIds = new Set<number>();
  let actionOptions: string[] = [];
  ctx.setTitle("Activity Logs");
  ctx.root.classList.add("app-logs");

  // ------------------------------------------------------------------ filter UI
  const searchInput = h("input", {
    class: "input",
    type: "search",
    placeholder: "Search path or detail…",
    ariaLabel: "Search activity",
  });
  const actorInput = h("input", {
    class: "input",
    type: "search",
    placeholder: "User…",
    ariaLabel: "Filter by user",
  });
  const actionSelect = h(
    "select",
    { class: "select", ariaLabel: "Filter by action" },
    h("option", { value: "" }, "All actions"),
  );
  const resultSelect = h(
    "select",
    { class: "select", ariaLabel: "Filter by result" },
    h("option", { value: "" }, "Any result"),
    h("option", { value: "success" }, "Success"),
    h("option", { value: "failure" }, "Failure"),
    h("option", { value: "denied" }, "Denied"),
  );
  const fromInput = h("input", { class: "input input-date", type: "date", ariaLabel: "From date" });
  const toInput = h("input", { class: "input input-date", type: "date", ariaLabel: "To date" });
  const resetButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, "Reset filters");
  const filtersRow = h(
    "div",
    { class: "logs-filters" },
    h("div", { class: "logs-search" }, h("span", { class: "search-icon" }, iconSvg("search", 16)), searchInput),
    isAdmin ? actorInput : null,
    actionSelect,
    resultSelect,
    h(
      "label",
      { class: "date-range" },
      h("span", { class: "date-label small muted" }, "From"),
      fromInput,
      h("span", { class: "date-label small muted" }, "To"),
      toInput,
    ),
    resetButton,
  );

  const countLabel = h("div", { class: "small muted logs-count" });
  const liveLabel = h("div", { class: "small faint logs-refreshed" });
  const exportButton = h("button", { class: "btn btn-sm", type: "button" }, iconSvg("download", 15), "Export CSV");
  const perPageSelect = h(
    "select",
    { class: "select select-sm", ariaLabel: "Entries per page" },
    ...PER_PAGE_OPTIONS.map((value) => h("option", { value: String(value), selected: value === perPage }, `${value} per page`)),
  );
  const actionsRow = h(
    "div",
    { class: "logs-actions-bar" },
    h("div", { class: "row" }, isAdmin ? null : h("span", { class: "badge info" }, "Your activity only")),
    countLabel,
    h("div", { class: "cp-toolbar-spacer" }),
    liveLabel,
    perPageSelect,
    exportButton,
  );

  // ---------------------------------------------------------- new activity pill
  const newActivityButton = h("button", { class: "logs-new-indicator", type: "button", hidden: true });
  let newActivityReason: NewActivityReason = "scroll";
  newActivityButton.addEventListener("click", () => {
    hideNewActivity();
    if (newActivityReason === "page") {
      page = 1;
      void load();
      return;
    }
    if (newActivityReason === "filters") {
      applyFilterInputs(EMPTY_FILTERS);
      page = 1;
      void load();
      return;
    }
    flushPending();
  });

  // ------------------------------------------------------------------ table
  const tableBody = h("tbody");
  const table = h(
    "table",
    { class: "table logs-table" },
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", { class: "col-time" }, "Time"),
        h("th", {}, "User"),
        h("th", {}, "Action"),
        h("th", { class: "col-object" }, "Object"),
        h("th", {}, "Result"),
        h("th", { class: "col-detail hide-md" }, "Detail"),
        h("th", { class: "col-ip hide-lg" }, "IP"),
      ),
    ),
    tableBody,
  );
  const tableWrap = h("div", { class: "table-wrap logs-table-wrap" }, table);
  const pagerControl: Pager = pager((nextPage) => {
    page = nextPage;
    hideNewActivity();
    void load();
  });
  const footer = h("div", { class: "logs-footer" }, pagerControl.el);
  const errorHost = h("div", { class: "logs-error", hidden: true });

  const indicatorRow = h("div", { class: "logs-live-row" }, newActivityButton);
  ctx.root.append(filtersRow, actionsRow, errorHost, indicatorRow, tableWrap, footer);

  // ------------------------------------------------------------------ data
  async function loadActions(): Promise<void> {
    try {
      const response = await ctx.api.get<{ items: string[] }>("/api/logs/actions");
      actionOptions = response.items.slice(0, MAX_ACTIONS);
      const current = actionSelect.value;
      clear(actionSelect);
      actionSelect.appendChild(h("option", { value: "" }, `All actions (${actionOptions.length})`));
      for (const action of actionOptions) {
        actionSelect.appendChild(h("option", { value: action }, `${humanizeAction(action)} — ${action}`));
      }
      actionSelect.value = current;
    } catch {
      actionSelect.appendChild(h("option", { value: "", disabled: true }, "Action list unavailable"));
    }
  }

  function activeFilterCount(): number {
    return [filters.actor, filters.action, filters.result, filters.q, filters.from, filters.to].filter(Boolean).length;
  }

  function buildQuery(): Record<string, unknown> {
    const query: Record<string, unknown> = {
      page,
      perPage,
      actor: filters.actor || undefined,
      action: filters.action || undefined,
      result: filters.result || undefined,
      q: filters.q || undefined,
    };
    if (filters.from) {
      const fromTs = startOfDay(new Date(`${filters.from}T00:00:00`).getTime());
      if (Number.isFinite(fromTs)) query.from = fromTs;
    }
    if (filters.to) {
      const toTs = endOfDay(new Date(`${filters.to}T00:00:00`).getTime());
      if (Number.isFinite(toTs)) query.to = toTs;
    }
    return query;
  }

  function validateDateRange(): string | null {
    if (!filters.from || !filters.to) return null;
    const fromTs = new Date(`${filters.from}T00:00:00`).getTime();
    const toTs = new Date(`${filters.to}T00:00:00`).getTime();
    if (Number.isFinite(fromTs) && Number.isFinite(toTs) && fromTs > toTs) {
      return "The start date must be before the end date.";
    }
    return null;
  }

  async function load(silent = false): Promise<void> {
    if (disposed || loading) return;
    const dateError = validateDateRange();
    errorHost.hidden = true;
    hideNewActivity();
    if (dateError) {
      errorHost.hidden = false;
      errorHost.replaceChildren(
        h("div", { class: "banner banner-warn" }, h("span", { class: "banner-icon" }, iconSvg("warning", 16)), h("div", { class: "banner-message" }, dateError)),
      );
      return;
    }
    loading = true;
    const token = ++loadToken;
    if (!silent) renderLoading();
    try {
      const response = await ctx.api.get<Page<LogEntry>>("/api/logs", buildQuery());
      if (token !== loadToken || disposed) return;
      const pageCount = Math.max(1, Math.ceil(response.total / perPage));
      if (page > pageCount && response.total > 0) {
        page = pageCount;
        loading = false;
        void load(silent);
        return;
      }
      total = response.total;
      entries = response.items.slice(0, perPage);
      seenIds.clear();
      let newestId = 0;
      for (const entry of entries) {
        seenIds.add(entry.id);
        if (entry.id > newestId) newestId = entry.id;
      }
      pending = pending.filter((entry) => entry.id > newestId && !seenIds.has(entry.id));
      renderRows(entries);
      if (pending.length > 0) showNewActivity(tableWrap.scrollTop > 6 ? "scroll" : "scroll", pending.length);
      else hideNewActivity();
      pagerControl.update({ page: response.page, totalPages: pageCount, totalItems: response.total });
      countLabel.textContent =
        response.total === 0
          ? "No matching entries"
          : `${formatNumberCompact(response.total)} ${response.total === 1 ? "entry" : "entries"}${activeFilterCount() > 0 ? " matching filters" : ""}`;
    } catch (error) {
      if (token !== loadToken || disposed) return;
      renderError(error);
    } finally {
      loading = false;
    }
  }

  function renderLoading(): void {
    tableBody.replaceChildren(
      ...Array.from({ length: 8 }, () =>
        h(
          "tr",
          { class: "is-loading" },
          ...Array.from({ length: 7 }, () => h("td", {}, h("div", { class: "skeleton-line", ariaHidden: true }))),
        ),
      ),
    );
    pagerControl.update({ page, totalPages: 1, totalItems: 0, label: "Loading…" });
  }

  function renderError(error: unknown): void {
    tableBody.replaceChildren();
    errorHost.hidden = false;
    errorHost.replaceChildren(
      h(
        "div",
        { class: "banner banner-error" },
        h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
        h(
          "div",
          { class: "banner-body" },
          h("div", { class: "banner-title" }, "Could not load activity"),
          h("div", { class: "banner-message" }, apiErrorMessage(error)),
        ),
        h("button", { class: "btn btn-sm", type: "button", onClick: () => void load() }, "Retry"),
      ),
    );
    countLabel.textContent = "";
    pagerControl.update({ page: 1, totalPages: 1, totalItems: 0, label: "—" });
  }

  function buildLogRow(entry: LogEntry): HTMLTableRowElement {
    const objectLabel = entry.objectPath || entry.objectId || entry.objectType || "—";
    const row = h(
      "tr",
      {
        class: "log-row",
        tabIndex: 0,
        role: "button",
        ariaLabel: `${humanizeAction(entry.action)} by ${entry.username || "anonymous"} — ${entry.result} — open details`,
      },
      h("td", { class: "col-time mono small", title: formatDateTimeSec(entry.createdAt) }, formatDateTime(entry.createdAt)),
      h("td", { class: "col-user" }, entry.username || h("span", { class: "muted" }, "anonymous")),
      h("td", { title: entry.action }, h("span", { class: "log-action" }, humanizeAction(entry.action))),
      h(
        "td",
        { class: "col-object" },
        h("span", { class: "log-object mono small", title: objectLabel }, objectLabel),
        entry.objectType ? h("span", { class: "log-object-type small faint" }, entry.objectType) : null,
      ),
      h("td", {}, resultBadge(entry.result)),
      h("td", { class: "col-detail hide-md small muted", title: entry.detail }, entry.detail || "—"),
      h("td", { class: "col-ip hide-lg mono small" }, entry.ip || "—"),
    );
    row.addEventListener("click", () => showDetails(entry));
    row.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        showDetails(entry);
      }
    });
    return row;
  }

  function renderRows(list: LogEntry[]): void {
    tableBody.replaceChildren();
    if (list.length === 0) {
      tableBody.appendChild(
        h(
          "tr",
          {},
          h(
            "td",
            { colspan: 7 },
            h(
              "div",
              { class: "empty-state logs-empty" },
              h("div", { class: "empty-title" }, activeFilterCount() > 0 ? "No activity matches these filters" : "No activity recorded yet"),
              h(
                "div",
                { class: "empty-detail" },
                activeFilterCount() > 0
                  ? "Try clearing the filters or widening the date range. New matching activity appears here automatically."
                  : "Actions such as sign-ins, file operations and settings changes will appear here.",
              ),
              activeFilterCount() > 0
                ? h("button", { class: "btn btn-sm", type: "button", onClick: () => resetFilters() }, "Reset filters")
                : null,
            ),
          ),
        ),
      );
      return;
    }
    const fragment = document.createDocumentFragment();
    for (const entry of list) fragment.appendChild(buildLogRow(entry));
    tableBody.appendChild(fragment);
  }

  // -------------------------------------------------------------- live updates
  function matchesFilters(entry: LogEntry): boolean {
    if (!isAdmin && viewerId !== null && entry.userId !== null && entry.userId !== viewerId) return false;
    if (filters.action && entry.action !== filters.action) return false;
    if (filters.result && entry.result !== filters.result) return false;
    if (filters.actor && !entry.username.toLowerCase().includes(filters.actor.toLowerCase())) return false;
    if (filters.q) {
      const q = filters.q.toLowerCase();
      if (!`${entry.objectPath} ${entry.detail}`.toLowerCase().includes(q)) return false;
    }
    if (filters.from) {
      const fromTs = startOfDay(new Date(`${filters.from}T00:00:00`).getTime());
      if (Number.isFinite(fromTs) && entry.createdAt < fromTs) return false;
    }
    if (filters.to) {
      const toTs = endOfDay(new Date(`${filters.to}T00:00:00`).getTime());
      if (Number.isFinite(toTs) && entry.createdAt > toTs) return false;
    }
    return true;
  }

  function showNewActivity(reason: NewActivityReason, count: number): void {
    newActivityReason = reason;
    newActivityButton.hidden = false;
    const label =
      reason === "page"
        ? "New activity — jump to page 1"
        : reason === "filters"
          ? "New activity outside these filters — show latest"
          : `New activity${count > 1 ? ` (${count})` : ""} — show`;
    newActivityButton.replaceChildren(
      h("span", { class: "logs-new-dot", ariaHidden: true }),
      h("span", {}, label),
    );
  }

  function hideNewActivity(): void {
    newActivityButton.hidden = true;
    newActivityButton.replaceChildren();
  }

  function flushPending(): void {
    if (pending.length === 0) return;
    const buffered = pending.splice(0);
    for (const entry of buffered) {
      entries.unshift(entry);
      seenIds.add(entry.id);
      total += 1;
      tableBody.insertBefore(buildLogRow(entry), tableBody.firstChild);
    }
    while (entries.length > perPage) {
      entries.pop();
      tableBody.lastElementChild?.remove();
    }
    tableWrap.scrollTop = 0;
    const pageCount = Math.max(1, Math.ceil(total / perPage));
    pagerControl.update({ page: 1, totalPages: pageCount, totalItems: total });
    countLabel.textContent = `${formatNumberCompact(total)} ${total === 1 ? "entry" : "entries"}${activeFilterCount() > 0 ? " matching filters" : ""}`;
  }

  function onAuditEntry(entry: LogEntry | null | undefined): void {
    if (disposed || !entry || typeof entry.id !== "number") return;
    if (seenIds.has(entry.id) || pending.some((candidate) => candidate.id === entry.id)) return;
    if (!matchesFilters(entry)) {
      showNewActivity("filters", 1);
      return;
    }
    if (page !== 1) {
      showNewActivity("page", 1);
      return;
    }
    if (loading) {
      // A refetch is in flight; keep the entry and reconcile after it settles
      // so we never show a stale "new activity" pill for data already loaded.
      pending.push(entry);
      if (pending.length > MAX_PENDING) pending.shift();
      return;
    }
    if (tableWrap.scrollTop > 6) {
      pending.push(entry);
      if (pending.length > MAX_PENDING) pending.shift();
      showNewActivity("scroll", pending.length);
      return;
    }
    // At the top: prepend immediately without a refetch and keep focus/scroll.
    entries.unshift(entry);
    seenIds.add(entry.id);
    total += 1;
    tableBody.insertBefore(buildLogRow(entry), tableBody.firstChild);
    while (entries.length > perPage) {
      entries.pop();
      tableBody.lastElementChild?.remove();
    }
    const pageCount = Math.max(1, Math.ceil(total / perPage));
    pagerControl.update({ page: 1, totalPages: pageCount, totalItems: total });
    countLabel.textContent = `${formatNumberCompact(total)} ${total === 1 ? "entry" : "entries"}${activeFilterCount() > 0 ? " matching filters" : ""}`;
  }

  function showDetails(entry: LogEntry): void {
    const copyTargets: Array<{ label: string; value: string }> = [];
    const rows: HTMLElement[] = [];
    const addRow = (term: string, value: string | Node | null, copyValue?: string): void => {
      const dd = h("dd", {});
      if (value === null) dd.textContent = "—";
      else if (typeof value === "string") dd.textContent = value;
      else dd.appendChild(value);
      if (copyValue) {
        const copy = h(
          "button",
          { class: "btn btn-subtle btn-sm copy-inline", type: "button", ariaLabel: `Copy ${term}`, title: "Copy" },
          iconSvg("copy", 13),
        );
        copy.addEventListener("click", () => {
          void copyText(copyValue).then((ok) => {
            copy.replaceChildren(iconSvg(ok ? "check" : "warning", 13));
            window.setTimeout(() => copy.replaceChildren(iconSvg("copy", 13)), 1400);
          });
        });
        dd.appendChild(copy);
        copyTargets.push({ label: term, value: copyValue });
      }
      rows.push(h("dt", {}, term), dd);
    };
    addRow("Timestamp", formatDateTimeSec(entry.createdAt));
    addRow("User", `${entry.username || "anonymous"}${entry.userId ? ` (#${entry.userId})` : ""}`);
    addRow("Action", humanizeAction(entry.action), entry.action);
    addRow("Raw action", entry.action, entry.action);
    addRow("Object type", entry.objectType || null);
    addRow("Object ID", entry.objectId || null);
    addRow("Object path", entry.objectPath || null, entry.objectPath || undefined);
    addRow("Result", resultBadge(entry.result));
    addRow("Detail", entry.detail || null, entry.detail || undefined);
    addRow("Client IP", entry.ip || null);
    addRow("Activity ID", String(entry.id));
    const body = h(
      "div",
      { class: "col" },
      h("dl", { class: "kv log-detail" }, ...rows),
      h(
        "div",
        { class: "row log-detail-actions" },
        h(
          "button",
          {
            class: "btn btn-sm",
            type: "button",
            onClick: () => {
              const text = copyTargets.map((target) => `${target.label}: ${target.value}`).join("\n");
              void copyText(`${text}\nTimestamp: ${formatDateTimeSec(entry.createdAt)}`).then((ok) => {
                if (ok) ctx.toast.success("Activity details copied");
                else ctx.toast.error("Could not copy to the clipboard");
              });
            },
          },
          iconSvg("copy", 14),
          "Copy details",
        ),
      ),
    );
    modal({ title: `Activity #${entry.id}`, body, wide: true });
  }

  function applyFilterInputs(next: Filters): void {
    filters.actor = next.actor;
    filters.action = next.action;
    filters.result = next.result;
    filters.q = next.q;
    filters.from = next.from;
    filters.to = next.to;
    actorInput.value = next.actor;
    actionSelect.value = next.action;
    resultSelect.value = next.result;
    searchInput.value = next.q;
    fromInput.value = next.from;
    toInput.value = next.to;
  }

  function resetFilters(): void {
    applyFilterInputs(EMPTY_FILTERS);
    page = 1;
    void load();
  }

  const reloadFromInputs = () => {
    filters.actor = actorInput.value.trim();
    filters.action = actionSelect.value;
    filters.result = resultSelect.value;
    filters.q = searchInput.value.trim();
    filters.from = fromInput.value;
    filters.to = toInput.value;
    page = 1;
    void load();
  };
  const debouncedReload = debounce(reloadFromInputs, 280);
  searchInput.addEventListener("input", debouncedReload);
  actorInput.addEventListener("input", debouncedReload);
  actionSelect.addEventListener("change", reloadFromInputs);
  resultSelect.addEventListener("change", reloadFromInputs);
  fromInput.addEventListener("change", reloadFromInputs);
  toInput.addEventListener("change", reloadFromInputs);
  resetButton.addEventListener("click", resetFilters);
  perPageSelect.addEventListener("change", () => {
    perPage = Number(perPageSelect.value) || DEFAULT_PER_PAGE;
    page = 1;
    void load();
  });
  exportButton.addEventListener("click", () => {
    const dateError = validateDateRange();
    if (dateError) {
      ctx.toast.warning(dateError);
      return;
    }
    const query = buildQuery();
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (key === "page" || key === "perPage") continue;
      if (value === undefined || value === null || value === "") continue;
      params.set(key, String(value));
    }
    const anchor = document.createElement("a");
    anchor.href = `/api/logs/export?${params.toString()}`;
    anchor.download = "rast-activity-logs.csv";
    anchor.rel = "noopener";
    anchor.click();
    ctx.toast.info("Export started — up to 10,000 matching entries");
  });

  // ------------------------------------------------------------------ realtime
  const realtime = getRealtime(ctx.events);
  const updateLiveLabel = (): void => {
    if (disposed) return;
    const state = realtime.status;
    const last = realtime.lastUpdate;
    if (state === "connected") {
      liveLabel.textContent = last ? `Live · updated ${formatTime(last)}` : "Live updates on";
    } else if (state === "offline") {
      liveLabel.textContent = "Live updates offline";
    } else {
      liveLabel.textContent = "Reconnecting…";
    }
  };
  updateLiveLabel();
  const unsubscribeStatus = realtime.onStatus(updateLiveLabel);
  const unsubscribeAudit = realtime.on<LogEntry>("audit.created", (entry) => onAuditEntry(entry));
  const unsubscribeResync = realtime.onResync(() => {
    if (disposed) return;
    void load(true);
  });
  const liveTimer = window.setInterval(() => {
    if (disposed) return;
    const last = realtime.lastUpdate;
    if (last === 0) return;
    liveLabel.textContent = `${realtime.status === "connected" ? "Live" : "Offline"} · updated ${relativeTime(last)}`;
  }, 30_000);
  tableWrap.addEventListener("scroll", () => {
    if (!newActivityButton.hidden && newActivityReason === "scroll" && tableWrap.scrollTop <= 6) {
      hideNewActivity();
      flushPending();
    }
  });

  await loadActions();
  await load();
  window.setTimeout(() => searchInput.focus(), 60);

  const cleanup = (): void => {
    disposed = true;
    window.clearInterval(liveTimer);
    unsubscribeStatus();
    unsubscribeAudit();
    unsubscribeResync();
    loadToken += 1;
  };
  ctx.onDispose(cleanup);
  return cleanup;
}
