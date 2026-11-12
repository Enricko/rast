// Shared UI + data helpers for the Users & Groups access policy screens.
//
// Built on the canonical components/tokens. Every new endpoint (F12/F13) is
// treated as optional: when the server does not implement it yet the UI falls
// back to the previous behaviour (detail payloads / unfiltered effective
// access) and shows an inline availability note instead of failing.

import { ApiError, apiErrorMessage } from "../../core/api";
import type { AppContext } from "../../core/app";
import { badge, pager, type Pager } from "../../core/components";
import { debounce, h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import type {
  AccessRuleSource,
  AccessRules,
  AppRule,
  EffectiveAccess,
  EffectiveAppAccess,
  EffectiveFolderAccess,
  FolderLevel,
  FolderRule,
  GroupImpact,
  GroupImpactUser,
  GroupMember,
  GroupRef,
  Page,
  Share,
} from "../../core/types";
import {
  APP_RULE_OPTIONS,
  FOLDER_RULE_OPTIONS,
  RULE_APPS,
  appLabel,
  folderLevelLabel,
  folderLevelTone,
  sourceLabel,
} from "./common";

const PICKER_PAGE_SIZE = 25;
const MAX_MEMBER_PAGES = 10;
const MEMBERS_PAGE_SIZE = 100;
const EFFECTIVE_PAGE_SIZE = 25;

function asArray<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : [];
}

export function isOptionalEndpointError(error: unknown): boolean {
  if (!(error instanceof ApiError)) return false;
  return (
    error.status === 400 ||
    error.status === 404 ||
    error.status === 405 ||
    error.status === 501 ||
    error.status === 503 ||
    error.code === "not_implemented"
  );
}

// ---------------------------------------------------------------------------
// Entity picker: server search + pagination, selections kept across pages
// ---------------------------------------------------------------------------

export interface PickerOption {
  id: number;
  label: string;
  description?: string;
  /** Deactivated account: listed for review, never offered for adding. */
  inactive?: boolean;
  /** When true the option is shown but cannot be checked. */
  disabled?: boolean;
}

export interface PickerPageResult {
  items: PickerOption[];
  total: number;
}

export interface EntityPickerOptions {
  fetchPage: (query: string, page: number, perPage: number) => Promise<PickerPageResult>;
  /** Initially selected options (labels may come from a detail/members payload). */
  selected: PickerOption[];
  onChange: () => void;
  searchPlaceholder?: string;
  emptyText?: string;
  ariaLabel?: string;
  /** Text shown above the current page list. */
  hint?: string;
}

export interface EntityPicker {
  el: HTMLElement;
  selectedIds(): number[];
  selectedItems(): PickerOption[];
  setSelected(items: PickerOption[]): void;
  reload(): Promise<void>;
  dispose(): void;
}

/** Reusable server-paginated checkbox picker with a persistent selection set. */
export function createEntityPicker(options: EntityPickerOptions): EntityPicker {
  const selected = new Map<number, PickerOption>();
  for (const item of options.selected) selected.set(Number(item.id), item);
  let page = 1;
  let total = 0;
  let pageItems: PickerOption[] = [];
  let loading = false;
  let disposed = false;
  let loadToken = 0;

  const search = h("input", {
    class: "input input-search",
    type: "search",
    placeholder: options.searchPlaceholder ?? "Search…",
    ariaLabel: options.ariaLabel ?? "Search",
  });
  const selectedLabel = h("div", { class: "picker-selected-label small muted" });
  const chips = h("div", { class: "picker-chips", role: "list", ariaLabel: "Selected items" });
  const list = h("div", { class: "checklist", role: "group", ariaLabel: options.ariaLabel ?? "Options" });
  const pagerHost = h("div", { class: "picker-pager" });
  const pagerControl: Pager = pager((nextPage) => {
    page = nextPage;
    void load();
  });
  pagerHost.appendChild(pagerControl.el);
  const hintLabel = h("div", { class: "field-hint" }, options.hint ?? "");
  const empty = h("div", { class: "field-hint", hidden: true }, options.emptyText ?? "Nothing found.");
  const errorHost = h("div", { class: "inline-availability", hidden: true });
  const selectedHost = h("div", { class: "picker-selected" }, selectedLabel, chips);

  const clearSearch = debounce(() => {
    page = 1;
    void load();
  }, 260);
  search.addEventListener("input", clearSearch);

  function renderSelected(): void {
    const items = Array.from(selected.values()).sort((a, b) => a.label.localeCompare(b.label));
    selectedHost.hidden = items.length === 0;
    selectedLabel.textContent = `Selected (${items.length})`;
    chips.replaceChildren();
    for (const item of items) {
      const remove = h(
        "button",
        {
          class: "picker-chip-remove",
          type: "button",
          ariaLabel: `Remove ${item.label}`,
          title: "Remove",
        },
        "×",
      );
      remove.addEventListener("click", () => {
        selected.delete(Number(item.id));
        options.onChange();
        renderSelected();
        renderList();
      });
      chips.appendChild(
        h(
          "span",
          { class: "picker-chip", role: "listitem" },
          h("span", { class: "picker-chip-label" }, item.label),
          item.inactive ? badge("Disabled", "warn") : null,
          remove,
        ),
      );
    }
  }

  function renderList(): void {
    list.replaceChildren();
    empty.hidden = pageItems.length > 0 || loading;
    for (const item of pageItems) {
      const isSelected = selected.has(Number(item.id));
      const cannotAdd = Boolean(item.disabled || item.inactive) && !isSelected;
      const input = h("input", {
        class: "checkbox-input",
        type: "checkbox",
        checked: isSelected,
        disabled: cannotAdd,
        ariaLabel: item.label,
      });
      input.addEventListener("change", () => {
        if (input.checked) selected.set(Number(item.id), item);
        else selected.delete(Number(item.id));
        options.onChange();
        renderSelected();
        renderList();
      });
      list.appendChild(
        h(
          "label",
          { class: `checklist-row${cannotAdd ? " is-disabled" : ""}` },
          input,
          h(
            "span",
            { class: "checklist-label" },
            h("span", { class: "checklist-name" }, item.label),
            item.description ? h("span", { class: "small faint" }, item.description) : null,
          ),
          item.inactive ? badge("Disabled", "warn") : null,
          item.disabled && !item.inactive ? h("span", { class: "small faint" }, "Unavailable") : null,
        ),
      );
    }
  }

  async function load(): Promise<void> {
    if (disposed) return;
    const token = ++loadToken;
    loading = true;
    errorHost.hidden = true;
    list.replaceChildren(h("div", { class: "loading-block compact" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading…")));
    try {
      const result = await options.fetchPage(search.value.trim(), page, PICKER_PAGE_SIZE);
      if (token !== loadToken || disposed) return;
      pageItems = asArray<PickerOption>(result.items);
      total = Number(result.total ?? pageItems.length);
      const pages = Math.max(1, Math.ceil(total / PICKER_PAGE_SIZE));
      if (page > pages) {
        page = pages;
        loading = false;
        void load();
        return;
      }
      pagerControl.update({
        page,
        totalPages: pages,
        totalItems: total,
        label: `${total.toLocaleString()} ${total === 1 ? "match" : "matches"} · page ${page} of ${pages}`,
      });
      renderList();
    } catch (error) {
      if (token !== loadToken || disposed) return;
      pageItems = [];
      list.replaceChildren();
      empty.hidden = true;
      errorHost.hidden = false;
      errorHost.replaceChildren(
        h(
          "div",
          {},
          h("div", { class: "inline-availability-title" }, "Could not load options"),
          h("div", { class: "small muted" }, apiErrorMessage(error)),
          h(
            "button",
            { class: "btn btn-sm", type: "button", onClick: () => void load() },
            "Retry",
          ),
        ),
      );
      pagerControl.update({ page: 1, totalPages: 1, totalItems: 0, label: "—" });
    } finally {
      loading = false;
    }
  }

  renderSelected();
  void load();
  const el = h("div", { class: "entity-picker" }, selectedHost, search, hintLabel, errorHost, list, pagerHost);

  return {
    el,
    selectedIds: () => Array.from(selected.keys()),
    selectedItems: () => Array.from(selected.values()),
    setSelected: (items) => {
      selected.clear();
      for (const item of items) selected.set(Number(item.id), item);
      renderSelected();
      renderList();
    },
    reload: () => load(),
    dispose: () => {
      disposed = true;
      loadToken += 1;
    },
  };
}

// ---------------------------------------------------------------------------
// Data loaders
// ---------------------------------------------------------------------------

export interface ShareRef {
  shareName: string;
  shareId?: string;
}

export interface UserAccessBasics {
  membershipIds: number[];
  membershipGroups: GroupRef[];
  rules: AccessRules;
  shares: ShareRef[];
  errors: string[];
}

/** Load direct rules + memberships for the user editor (tolerant of 404/501). */
export async function loadUserAccessData(ctx: AppContext, userId: number): Promise<UserAccessBasics> {
  const [membershipResult, rulesResult, sharesResult] = await Promise.allSettled([
    ctx.api.get<{ groupIds?: number[]; groups?: GroupRef[] }>(`/api/users/${userId}/groups`),
    ctx.api.get<AccessRules>(`/api/users/${userId}/access-rules`),
    ctx.api.get<Share[]>("/api/shares"),
  ]);
  const errors: string[] = [];
  let membershipIds: number[] = [];
  let membershipGroups: GroupRef[] = [];
  if (membershipResult.status === "fulfilled") {
    membershipIds = asArray<number>(membershipResult.value.groupIds).map(Number);
    membershipGroups = asArray<GroupRef>(membershipResult.value.groups);
  } else {
    errors.push(`Group membership: ${apiErrorMessage(membershipResult.reason)}`);
  }

  let rules: AccessRules = { folderRules: [], appRules: [] };
  if (rulesResult.status === "fulfilled") {
    rules = {
      folderRules: asArray<AccessRules["folderRules"][number]>(rulesResult.value.folderRules),
      appRules: asArray<AccessRules["appRules"][number]>(rulesResult.value.appRules),
    };
  } else {
    errors.push(`Direct rules: ${apiErrorMessage(rulesResult.reason)}`);
  }

  let shares: ShareRef[] = [];
  if (sharesResult.status === "fulfilled") {
    shares = asArray<Share>(sharesResult.value).map((share) => ({ shareName: share.name, shareId: share.id }));
  } else {
    errors.push(`Shared folders: ${apiErrorMessage(sharesResult.reason)}`);
  }

  return { membershipIds, membershipGroups, rules, shares, errors };
}

export interface GroupMembersLoad {
  members: GroupMember[];
  total: number;
  complete: boolean;
  error: string | null;
}

/**
 * Load every member of a group through the paginated members endpoint (bounded),
 * falling back to the detail payload when the endpoint is not available yet.
 */
export async function loadGroupMembers(
  ctx: AppContext,
  groupId: number | null,
  fallbackMembers: GroupMember[],
): Promise<GroupMembersLoad> {
  if (groupId === null) {
    return { members: [], total: 0, complete: true, error: null };
  }
  const collected: GroupMember[] = [];
  const seen = new Set<number>();
  let total = fallbackMembers.length;
  try {
    for (let page = 1; page <= MAX_MEMBER_PAGES; page += 1) {
      const response = await ctx.api.get<Page<GroupMember>>(`/api/groups/${groupId}/members`, {
        page,
        perPage: MEMBERS_PAGE_SIZE,
      });
      const items = asArray<GroupMember>(response.items);
      total = Number(response.total ?? items.length);
      for (const member of items) {
        const id = Number(member.userId);
        if (seen.has(id)) continue;
        seen.add(id);
        collected.push(member);
      }
      if (items.length === 0 || collected.length >= total) break;
    }
    return {
      members: collected,
      total,
      complete: collected.length >= total,
      error: null,
    };
  } catch (error) {
    if (!isOptionalEndpointError(error)) {
      // Non-optional failure: show the fallback and a note, keep editing usable.
      return {
        members: fallbackMembers,
        total: fallbackMembers.length,
        complete: true,
        error: `Group members: ${apiErrorMessage(error)}`,
      };
    }
    return { members: fallbackMembers, total: fallbackMembers.length, complete: true, error: null };
  }
}

// ---------------------------------------------------------------------------
// Direct overrides editor
// ---------------------------------------------------------------------------

export interface DirectRulesEditor {
  el: HTMLElement;
  collect(): AccessRules;
}

/** Folder + application rule selects. `inherit` means "remove the direct rule". */
export function renderDirectRulesEditor(options: {
  shares: ShareRef[];
  rules: AccessRules;
  onChange: () => void;
}): DirectRulesEditor {
  const folderRules = new Map<string, GroupFolderRuleLike>();
  for (const rule of options.rules.folderRules) {
    folderRules.set(rule.shareName, { rule: rule.rule, shareId: rule.shareId });
  }
  const appRules = new Map<string, AppRule>();
  for (const rule of options.rules.appRules) appRules.set(rule.appId, rule.rule);

  const shareIds = new Map<string, string | undefined>();
  for (const share of options.shares) shareIds.set(share.shareName, share.shareId);
  for (const rule of options.rules.folderRules) {
    if (rule.shareId && !shareIds.has(rule.shareName)) shareIds.set(rule.shareName, rule.shareId);
  }

  // Direct rules may reference shares that are no longer listed; keep them
  // visible so an admin can clear them.
  const folderNames = Array.from(new Set([...options.shares.map((share) => share.shareName), ...folderRules.keys()])).sort(
    (a, b) => a.localeCompare(b),
  );
  const MAX_ROWS = 200;
  const shownFolders = folderNames.slice(0, MAX_ROWS);

  const folderBody = h("tbody");
  for (const shareName of shownFolders) {
    const select = h(
      "select",
      { class: "select select-sm rule-select", ariaLabel: `Direct rule for ${shareName}` },
      ...FOLDER_RULE_OPTIONS.map((option) =>
        h("option", { value: option.value, selected: (folderRules.get(shareName)?.rule ?? "inherit") === option.value }, option.label),
      ),
    );
    select.addEventListener("change", () => {
      folderRules.set(shareName, { rule: select.value as FolderRule, shareId: shareIds.get(shareName) });
      options.onChange();
    });
    folderBody.appendChild(
      h("tr", {}, h("td", { class: "mono small" }, shareName), h("td", { class: "rule-cell" }, select)),
    );
  }
  if (shownFolders.length === 0) {
    folderBody.appendChild(h("tr", {}, h("td", { colspan: 2, class: "table-empty" }, "No shared folders available.")));
  }
  const folderTable = h(
    "table",
    { class: "table rules-table" },
    h("thead", {}, h("tr", {}, h("th", {}, "Shared folder"), h("th", { class: "rule-col" }, "Direct rule"))),
    folderBody,
  );

  const appBody = h("tbody");
  for (const app of RULE_APPS) {
    const select = h(
      "select",
      { class: "select select-sm rule-select", ariaLabel: `Direct rule for ${app.label}` },
      ...APP_RULE_OPTIONS.map((option) =>
        h("option", { value: option.value, selected: (appRules.get(app.id) ?? "inherit") === option.value }, option.label),
      ),
    );
    select.addEventListener("change", () => {
      appRules.set(app.id, select.value as AppRule);
      options.onChange();
    });
    appBody.appendChild(h("tr", {}, h("td", {}, app.label), h("td", { class: "rule-cell" }, select)));
  }
  const appTable = h(
    "table",
    { class: "table rules-table" },
    h("thead", {}, h("tr", {}, h("th", {}, "Application"), h("th", { class: "rule-col" }, "Direct rule"))),
    appBody,
  );

  const collect = (): AccessRules => ({
    folderRules: Array.from(folderRules.entries())
      .filter(([, value]) => value.rule !== "inherit")
      .map(([shareName, value]) => ({
        shareName,
        rule: value.rule,
        ...(value.shareId ? { shareId: value.shareId } : {}),
      })),
    appRules: Array.from(appRules.entries())
      .filter(([, rule]) => rule !== "inherit")
      .map(([appId, rule]) => ({ appId, rule })),
  });

  const el = h(
    "div",
    { class: "access-rules-editor" },
    h(
      "div",
      { class: "access-subsection" },
      h("div", { class: "access-subsection-title" }, "Shared folders"),
      h("div", { class: "table-wrap table-wrap-compact" }, folderTable),
    ),
    shownFolders.length < folderNames.length
      ? h("div", { class: "field-hint" }, `Showing the first ${MAX_ROWS} of ${folderNames.length} shared folders.`)
      : null,
    h(
      "div",
      { class: "access-subsection" },
      h("div", { class: "access-subsection-title" }, "Applications"),
      h("div", { class: "table-wrap table-wrap-compact" }, appTable),
    ),
  );

  return { el, collect };
}

interface GroupFolderRuleLike {
  rule: FolderRule;
  shareId?: string;
}

// ---------------------------------------------------------------------------
// Effective access (F13): server search/filter/pagination with client fallback
// ---------------------------------------------------------------------------

export type EffectiveLevelFilter = "all" | "none" | "read" | "write";

export interface EffectiveAccessPanelOptions {
  ctx: AppContext;
  userId: number;
  /** Called with non-fatal availability problems (optional endpoints). */
  onAvailability?: (problems: string[]) => void;
}

export interface EffectiveAccessPanel {
  el: HTMLElement;
  reload: () => Promise<void>;
  dispose: () => void;
}

const LEVEL_OPTIONS: Array<{ value: EffectiveLevelFilter; label: string }> = [
  { value: "all", label: "All levels" },
  { value: "none", label: "No Access" },
  { value: "read", label: "Read Only" },
  { value: "write", label: "Read & Write" },
];

interface EffectivePage {
  access: EffectiveAccess | null;
  folders: EffectiveFolderAccess[];
  apps: EffectiveAppAccess[];
  total: number;
  page: number;
  perPage: number;
  serverPaged: boolean;
  error: string | null;
}

function normalizeEffectiveResponse(input: EffectiveAccess): EffectiveAccess {
  return {
    user: input.user,
    groups: asArray<GroupRef>(input.groups),
    folders: asArray<EffectiveFolderAccess>(input.folders).map((folder) => ({
      ...folder,
      level: folder.level ?? "none",
      sources: asArray<AccessRuleSource>(folder.sources),
      conflict: folder.conflict ?? null,
    })),
    apps: asArray<EffectiveAppAccess>(input.apps).map((app) => ({
      ...app,
      sources: asArray<AccessRuleSource>(app.sources),
    })),
    foldersTotal: typeof input.foldersTotal === "number" ? input.foldersTotal : undefined,
    page: typeof input.page === "number" ? input.page : undefined,
    perPage: typeof input.perPage === "number" ? input.perPage : undefined,
  };
}

function folderMatches(folder: EffectiveFolderAccess, query: string, level: EffectiveLevelFilter): boolean {
  if (level !== "all" && (folder.level ?? "none") !== level) return false;
  if (!query) return true;
  const needle = query.toLowerCase();
  return folder.shareName.toLowerCase().includes(needle) || (folder.path ?? "").toLowerCase().includes(needle);
}

/** Fetch one page; falls back to client-side filter/paging when unsupported. */
async function fetchEffectivePage(
  ctx: AppContext,
  userId: number,
  query: string,
  level: EffectiveLevelFilter,
  page: number,
  perPage: number,
): Promise<EffectivePage> {
  const params: Record<string, unknown> = { q: query || undefined, level, page, perPage };
  try {
    const response = normalizeEffectiveResponse(
      await ctx.api.get<EffectiveAccess>(`/api/users/${userId}/effective-access`, params),
    );
    const serverPaged =
      typeof response.foldersTotal === "number" && typeof response.page === "number" && typeof response.perPage === "number";
    if (serverPaged) {
      return {
        access: response,
        folders: response.folders,
        apps: response.apps,
        total: response.foldersTotal ?? response.folders.length,
        page: response.page ?? page,
        perPage: response.perPage ?? perPage,
        serverPaged: true,
        error: null,
      };
    }
    return clientPaginate(response, query, level, page, perPage, "The server returns unpaginated Effective Access; filtering locally.");
  } catch (error) {
    if (!isOptionalEndpointError(error)) throw error;
    // Endpoint/params unavailable: fall back to the plain endpoint.
    try {
      const response = normalizeEffectiveResponse(await ctx.api.get<EffectiveAccess>(`/api/users/${userId}/effective-access`));
      return clientPaginate(response, query, level, page, perPage, null);
    } catch (fallbackError) {
      return {
        access: null,
        folders: [],
        apps: [],
        total: 0,
        page: 1,
        perPage,
        serverPaged: false,
        error: apiErrorMessage(fallbackError),
      };
    }
  }
}

function clientPaginate(
  access: EffectiveAccess,
  query: string,
  level: EffectiveLevelFilter,
  page: number,
  perPage: number,
  note: string | null,
): EffectivePage {
  const filtered = access.folders.filter((folder) => folderMatches(folder, query, level));
  const total = filtered.length;
  const pages = Math.max(1, Math.ceil(total / perPage));
  const safePage = Math.min(Math.max(1, page), pages);
  const start = (safePage - 1) * perPage;
  return {
    access: { ...access, folders: filtered, apps: access.apps },
    folders: filtered.slice(start, start + perPage),
    apps: access.apps,
    total,
    page: safePage,
    perPage,
    serverPaged: false,
    error: note,
  };
}

function ruleWord(rule: string | boolean | null | undefined): string {
  if (typeof rule === "boolean") return rule ? "Allowed" : "Denied";
  switch (rule) {
    case "none":
      return "No Access";
    case "read":
      return "Read Only";
    case "write":
      return "Read & Write";
    case "admin":
      return "Admin";
    case "inherit":
      return "Inherit";
    case "allow":
      return "Allow";
    default:
      return rule ? String(rule) : "Unknown";
  }
}

function describeSourceList(sources: AccessRuleSource[]): string {
  const parts = sources.map((source) => {
    const type = source.type ?? source.kind ?? "direct";
    if (type === "direct") return "Direct";
    return source.groupName ? `Group “${source.groupName}”` : "Group";
  });
  return Array.from(new Set(parts)).join(" and ");
}

/** Human-readable explicit-deny conflict from the server or raw sources. */
export function folderConflictText(folder: EffectiveFolderAccess): string | null {
  if (folder.conflict && (folder.conflict.deniedBy?.length || folder.conflict.overriddenGrants?.length)) {
    const denied = folder.conflict.deniedBy ?? [];
    const grants = folder.conflict.overriddenGrants ?? [];
    if (denied.length > 0 && grants.length > 0) {
      return `No Access from ${describeSourceList(denied)} overrides ${ruleWord(grants[0]?.rule)} from ${describeSourceList(grants)}.`;
    }
    if (denied.length > 0) return `No Access from ${describeSourceList(denied)} wins for this folder.`;
  }
  const deny = (folder.sources ?? []).filter((source) => source.rule === "none");
  const grant = (folder.sources ?? []).filter(
    (source) => source.rule === "read" || source.rule === "write" || source.rule === "allow",
  );
  if (deny.length > 0 && grant.length > 0) {
    return `No Access from ${describeSourceList(deny)} overrides ${ruleWord(grant[0]?.rule)} from ${describeSourceList(grant)}.`;
  }
  return null;
}

function fallbackLabel(fallback: string | null | undefined): string {
  switch (fallback) {
    case "admin":
      return "Administrator bypass";
    case "legacy-acl":
      return "Legacy share membership";
    case "explicit":
      return "Explicit rule";
    case "default":
      return "Role default";
    case "none":
      return "No grant";
    case "not-installed":
      return "Not installed (Package Center)";
    default:
      return "";
  }
}

function sourceChips(sources: AccessRuleSource[]): Node {
  const wrap = h("div", { class: "prov-chips" });
  if (!sources || sources.length === 0) {
    wrap.appendChild(h("span", { class: "small faint" }, "No rules"));
    return wrap;
  }
  for (const source of sources) {
    const type = source.type ?? source.kind ?? "direct";
    wrap.appendChild(
      h(
        "span",
        { class: `prov-chip prov-${type}` },
        h("span", { class: "prov-who" }, sourceLabel(source)),
        h("span", { class: "prov-rule" }, ruleWord(source.rule)),
      ),
    );
  }
  return wrap;
}

function folderTable(folders: EffectiveFolderAccess[]): HTMLElement {
  const body = h("tbody");
  for (const folder of folders) {
    const conflict = folderConflictText(folder);
    body.appendChild(
      h(
        "tr",
        {},
        h(
          "td",
          {},
          h("div", { class: "mono small" }, folder.shareName),
          folder.path ? h("div", { class: "small faint" }, folder.path) : null,
        ),
        h("td", {}, badge(folderLevelLabel(folder.level ?? "none"), folderLevelTone(folder.level ?? "none"))),
        h("td", {}, sourceChips(folder.sources ?? [])),
        h(
          "td",
          { class: "small muted" },
          ...[
            fallbackLabel(folder.fallback) ? h("span", {}, fallbackLabel(folder.fallback)) : null,
            conflict ? h("div", { class: "conflict-note" }, conflict) : null,
          ].filter((node): node is HTMLElement => node !== null),
        ),
      ),
    );
  }
  if (folders.length === 0) {
    body.appendChild(h("tr", {}, h("td", { colspan: 4, class: "table-empty" }, "No shared folders match this search or filter.")));
  }
  return h(
    "table",
    { class: "table effective-table" },
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", {}, "Shared folder"),
        h("th", {}, "Effective level"),
        h("th", {}, "Provenance"),
        h("th", {}, "Notes"),
      ),
    ),
    body,
  );
}

function appTable(apps: EffectiveAppAccess[]): HTMLElement {
  const body = h("tbody");
  for (const app of apps) {
    const constant = RULE_APPS.find((item) => item.id === app.appId);
    const decision = app.allowed ? (app.readOnly ? "Read-only" : "Allowed") : "Denied";
    body.appendChild(
      h(
        "tr",
        {},
        h("td", {}, app.label || constant?.label || appLabel(app.appId)),
        h("td", {}, badge(decision, app.allowed ? (app.readOnly ? "warn" : "ok") : "error")),
        h("td", {}, sourceChips(app.sources ?? [])),
        h(
          "td",
          { class: "small muted" },
          fallbackLabel(app.fallback) ? h("span", {}, fallbackLabel(app.fallback)) : null,
        ),
      ),
    );
  }
  if (apps.length === 0) {
    body.appendChild(h("tr", {}, h("td", { colspan: 4, class: "table-empty" }, "No applications to evaluate.")));
  }
  return h(
    "table",
    { class: "table effective-table" },
    h(
      "thead",
      {},
      h("tr", {}, h("th", {}, "Application"), h("th", {}, "Effective access"), h("th", {}, "Provenance"), h("th", {}, "Notes")),
    ),
    body,
  );
}

/** Effective Access panel: search + level filter + folder pagination; apps always shown. */
export function createEffectiveAccessPanel(options: EffectiveAccessPanelOptions): EffectiveAccessPanel {
  let query = "";
  let level: EffectiveLevelFilter = "all";
  let page = 1;
  let disposed = false;
  let token = 0;

  const search = h("input", {
    class: "input input-search",
    type: "search",
    placeholder: "Search shared folders…",
    ariaLabel: "Search effective access",
  });
  const levelSelect = h(
    "select",
    { class: "select select-sm", ariaLabel: "Filter by effective level" },
    ...LEVEL_OPTIONS.map((option) => h("option", { value: option.value }, option.label)),
  );
  const toolbar = h(
    "div",
    { class: "effective-toolbar" },
    h("div", { class: "search-wrap" }, h("span", { class: "search-icon" }, iconSvg("search", 15)), search),
    levelSelect,
    h("div", { class: "cp-toolbar-spacer" }),
    h("div", { class: "small muted effective-total" }),
  );
  const folderHost = h("div", { class: "effective-folders" });
  const appsHost = h("div", { class: "effective-apps" });
  const pagerHost = h("div", { class: "effective-pager" });
  const availabilityHost = h("div", { class: "inline-availability", hidden: true });
  const errorHost = h("div", { class: "inline-availability", hidden: true });
  const panel = h(
    "div",
    { class: "effective-access" },
    h(
      "div",
      { class: "small muted" },
      "Final decision computed by the server: explicit No Access always wins over grants. Search and filter the shared-folder list; applications are always shown.",
    ),
    toolbar,
    availabilityHost,
    errorHost,
    h(
      "div",
      { class: "access-subsection" },
      h("div", { class: "access-subsection-title" }, "Shared folders"),
      folderHost,
      pagerHost,
    ),
    h("div", { class: "access-subsection" }, h("div", { class: "access-subsection-title" }, "Applications"), appsHost),
  );

  const pagerControl: Pager = pager((nextPage) => {
    page = nextPage;
    void load();
  });
  pagerHost.appendChild(pagerControl.el);

  const scheduleReload = debounce(() => {
    page = 1;
    void load();
  }, 260);
  search.addEventListener("input", () => {
    query = search.value.trim();
    scheduleReload();
  });
  levelSelect.addEventListener("change", () => {
    level = levelSelect.value as EffectiveLevelFilter;
    page = 1;
    void load();
  });

  function render(pageData: EffectivePage): void {
    const totalEl = toolbar.querySelector<HTMLElement>(".effective-total");
    if (totalEl) {
      totalEl.textContent = `${pageData.total.toLocaleString()} shared folder${pageData.total === 1 ? "" : "s"}`;
    }
    errorHost.hidden = true;
    if (pageData.error) {
      availabilityHost.hidden = false;
      availabilityHost.replaceChildren(
        h("div", { class: "inline-availability-title" }, "Filtering locally"),
        h("div", { class: "small muted" }, pageData.error),
      );
    } else {
      availabilityHost.hidden = true;
    }
    folderHost.replaceChildren(
      h("div", { class: "table-wrap table-wrap-compact" }, folderTable(pageData.folders)),
    );
    appsHost.replaceChildren(
      h("div", { class: "table-wrap table-wrap-compact" }, appTable(pageData.apps)),
    );
    const pages = Math.max(1, Math.ceil(pageData.total / pageData.perPage));
    pagerControl.update({
      page: pageData.page,
      totalPages: pages,
      totalItems: pageData.total,
      label: `${pageData.total.toLocaleString()} folder${pageData.total === 1 ? "" : "s"} · page ${pageData.page} of ${pages}`,
    });
  }

  function renderFailure(message: string): void {
    folderHost.replaceChildren();
    appsHost.replaceChildren();
    pagerControl.update({ page: 1, totalPages: 1, totalItems: 0, label: "—" });
    availabilityHost.hidden = true;
    errorHost.hidden = false;
    errorHost.replaceChildren(
      h(
        "div",
        {},
        h("div", { class: "inline-availability-title" }, "Effective access is unavailable"),
        h("div", { class: "small muted" }, message),
        h("button", { class: "btn btn-sm", type: "button", onClick: () => void load() }, "Retry"),
      ),
    );
  }

  async function load(): Promise<void> {
    if (disposed) return;
    const requestToken = ++token;
    folderHost.replaceChildren(h("div", { class: "loading-block compact" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading effective access…")));
    try {
      const pageData = await fetchEffectivePage(options.ctx, options.userId, query, level, page, EFFECTIVE_PAGE_SIZE);
      if (disposed || requestToken !== token) return;
      if (pageData.error && !pageData.access) {
        renderFailure(pageData.error);
        options.onAvailability?.([pageData.error]);
        return;
      }
      render(pageData);
      if (pageData.error) options.onAvailability?.([pageData.error]);
    } catch (error) {
      if (disposed || requestToken !== token) return;
      renderFailure(apiErrorMessage(error));
      options.onAvailability?.([`Effective access: ${apiErrorMessage(error)}`]);
    }
  }

  void load();
  return {
    el: panel,
    reload: () => load(),
    dispose: () => {
      disposed = true;
      token += 1;
    },
  };
}

// ---------------------------------------------------------------------------
// Group deletion impact (F12)
// ---------------------------------------------------------------------------

export interface ImpactSummary {
  members: number;
  folderRules: number;
  appRules: number;
  userCount: number;
  lines: Array<{ username: string; displayName?: string; changes: string[] }>;
  truncated: boolean;
}

/** Fetch the delete-impact preview; null when the endpoint is unavailable. */
export async function loadGroupImpact(ctx: AppContext, groupId: number): Promise<GroupImpact | null> {
  try {
    return await ctx.api.get<GroupImpact>(`/api/groups/${groupId}/impact`);
  } catch (error) {
    if (isOptionalEndpointError(error)) return null;
    throw error;
  }
}

function impactChangeLine(change: { resource: string; kind: "folder" | "app"; before?: string | boolean | null; after?: string | boolean | null }): string {
  const resource = change.kind === "app" ? appLabel(change.resource) : change.resource;
  return `${resource}: ${ruleWord(change.before)} → ${ruleWord(change.after)}`;
}

function impactUserChanges(user: GroupImpactUser): string[] {
  if (Array.isArray(user.changes) && user.changes.length > 0) {
    return user.changes.map((change) => impactChangeLine(change));
  }
  const lines: string[] = [];
  const beforeFolders = user.before?.folders ?? {};
  const afterFolders = user.after?.folders ?? {};
  for (const [share, before] of Object.entries(beforeFolders)) {
    const after = afterFolders[share] ?? "none";
    if (after !== before) lines.push(`${share}: ${ruleWord(before)} → ${ruleWord(after)}`);
  }
  const beforeApps = user.before?.apps ?? {};
  const afterApps = user.after?.apps ?? {};
  for (const [appId, before] of Object.entries(beforeApps)) {
    const after = afterApps[appId] ?? false;
    if (after !== before) lines.push(`${appLabel(appId)}: ${ruleWord(before)} → ${ruleWord(after)}`);
  }
  return lines;
}

/** Normalise either documented impact shape into display-ready lines. */
export function summarizeImpact(impact: GroupImpact): ImpactSummary {
  const users = asArray<GroupImpactUser>(impact.users);
  const lines = users.slice(0, 10).map((user) => ({
    username: user.username,
    displayName: user.displayName,
    changes: impactUserChanges(user).slice(0, 6),
  }));
  return {
    members: Number(impact.members ?? users.length),
    folderRules: Number(impact.folderRules ?? 0),
    appRules: Number(impact.appRules ?? 0),
    userCount: users.length,
    lines,
    truncated: Boolean(impact.truncated) || users.length > 10,
  };
}

// ---------------------------------------------------------------------------
// Small shared bits
// ---------------------------------------------------------------------------

/** Compact group chips for the users table. */
export function groupChips(groups: GroupRef[] | undefined, limit = 2): HTMLElement {
  const wrap = h("div", { class: "group-chips" });
  const list = groups ?? [];
  if (list.length === 0) {
    wrap.appendChild(h("span", { class: "small faint" }, "—"));
    return wrap;
  }
  for (const group of list.slice(0, limit)) {
    wrap.appendChild(h("span", { class: "chip chip-sm" }, group.name));
  }
  if (list.length > limit) wrap.appendChild(h("span", { class: "chip chip-sm chip-more" }, `+${list.length - limit}`));
  return wrap;
}

/** Banner shown when one or more access endpoints are unavailable. */
export function accessAvailability(problems: string[]): HTMLElement | null {
  if (problems.length === 0) return null;
  return h(
    "div",
    { class: "inline-availability" },
    h("div", { class: "inline-availability-title" }, "Some access information is unavailable"),
    h("div", { class: "small muted" }, problems.join(" · ")),
  );
}

// Keep the type referenced for downstream modules that import it from here.
export type { FolderLevel };
