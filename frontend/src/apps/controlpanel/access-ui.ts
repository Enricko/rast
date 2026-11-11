// Shared UI + data helpers for the Users & Groups access policy screens.
//
// Built entirely on the existing components/tokens. Every loader tolerates the
// groups backend being absent (404/501/503) so older servers keep working.

import { apiErrorMessage } from "../../core/api";
import type { AppContext } from "../../core/app";
import { badge } from "../../core/components";
import { h } from "../../core/dom";
import type {
  AccessRuleSource,
  AccessRules,
  AppRule,
  EffectiveAccess,
  FolderRule,
  GroupRef,
  Page,
  Permissions,
  Share,
} from "../../core/types";
import {
  APP_RULE_OPTIONS,
  FOLDER_RULE_OPTIONS,
  RULE_APPS,
  appLabel,
  denyConflict,
  folderLevelLabel,
  folderLevelTone,
  sourceLabel,
} from "./common";

export interface AccessData {
  allGroups: GroupRef[];
  membership: number[];
  rules: AccessRules;
  shares: string[];
  effective: EffectiveAccess | null;
  /** Sections that could not be loaded (endpoint missing/unavailable). */
  errors: string[];
}

function asArray<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : [];
}

/** Load everything the users access editor needs, tolerating partial failures. */
export async function loadUserAccessData(ctx: AppContext, userId: number): Promise<AccessData> {
  const [groupsResult, membershipResult, rulesResult, sharesResult, effectiveResult] = await Promise.allSettled([
    ctx.api.get<Page<GroupRef>>("/api/groups", { page: 1, perPage: 100 }),
    ctx.api.get<{ groupIds?: number[]; groups?: GroupRef[] }>(`/api/users/${userId}/groups`),
    ctx.api.get<AccessRules>(`/api/users/${userId}/access-rules`),
    ctx.api.get<Share[]>("/api/shares"),
    ctx.api.get<EffectiveAccess>(`/api/users/${userId}/effective-access`),
  ]);
  const errors: string[] = [];
  const allGroups =
    groupsResult.status === "fulfilled" ? asArray<GroupRef>(groupsResult.value.items) : [];
  if (groupsResult.status === "rejected") errors.push(`Groups: ${apiErrorMessage(groupsResult.reason)}`);

  let membership: number[] = [];
  if (membershipResult.status === "fulfilled") {
    membership = asArray<number>(membershipResult.value.groupIds).map(Number);
  } else {
    errors.push(`Group membership: ${apiErrorMessage(membershipResult.reason)}`);
  }

  let rules: AccessRules = { folderRules: [], appRules: [] };
  if (rulesResult.status === "fulfilled") {
    rules = {
      folderRules: asArray(rulesResult.value.folderRules),
      appRules: asArray(rulesResult.value.appRules),
    };
  } else {
    errors.push(`Direct rules: ${apiErrorMessage(rulesResult.reason)}`);
  }

  const shares =
    sharesResult.status === "fulfilled" ? asArray<Share>(sharesResult.value).map((share) => share.name) : [];
  if (sharesResult.status === "rejected") errors.push(`Shared folders: ${apiErrorMessage(sharesResult.reason)}`);

  let effective: EffectiveAccess | null = null;
  if (effectiveResult.status === "fulfilled") {
    effective = effectiveResult.value;
  } else {
    errors.push(`Effective access: ${apiErrorMessage(effectiveResult.reason)}`);
  }

  return { allGroups, membership, rules, shares, effective, errors };
}

export interface GroupMembershipEditor {
  el: HTMLElement;
  collect(): number[];
  setSelection(ids: number[]): void;
  setGroups(groups: GroupRef[]): void;
}

/** Search + checkbox list of groups. */
export function renderGroupMembership(options: {
  groups: GroupRef[];
  selected: number[];
  onChange: () => void;
}): GroupMembershipEditor {
  let groups = [...options.groups];
  const selected = new Set<number>(options.selected.map(Number));
  const search = h("input", {
    class: "input input-search",
    type: "search",
    placeholder: "Search groups…",
    ariaLabel: "Search groups",
  });
  const list = h("div", { class: "checklist", role: "group", ariaLabel: "Group membership" });
  const empty = h("div", { class: "field-hint", hidden: true }, "No groups yet — create a group first.");
  const searchWrap = h("div", { class: "search-wrap" }, h("span", { class: "search-icon" }, "⌕"), search);

  const render = (): void => {
    const query = search.value.trim().toLowerCase();
    list.replaceChildren();
    const matches = groups.filter((group) => !query || group.name.toLowerCase().includes(query));
    empty.hidden = groups.length > 0;
    for (const group of matches) {
      const checked = selected.has(Number(group.id));
      const input = h("input", {
        class: "checkbox-input",
        type: "checkbox",
        checked,
        ariaLabel: group.name,
      });
      input.addEventListener("change", () => {
        if (input.checked) selected.add(Number(group.id));
        else selected.delete(Number(group.id));
        options.onChange();
      });
      list.appendChild(
        h(
          "label",
          { class: "checklist-row" },
          input,
          h("span", { class: "checklist-label" }, group.name),
        ),
      );
    }
    if (matches.length === 0 && groups.length > 0) {
      list.appendChild(h("div", { class: "field-hint" }, `No groups match “${search.value.trim()}”.`));
    }
  };
  search.addEventListener("input", render);
  render();
  const el = h("div", { class: "membership-editor" }, searchWrap, list, empty);
  return {
    el,
    collect: () => Array.from(selected),
    setSelection: (ids) => {
      selected.clear();
      for (const id of ids) selected.add(Number(id));
      render();
    },
    setGroups: (next) => {
      groups = [...next];
      render();
    },
  };
}

export interface DirectRulesEditor {
  el: HTMLElement;
  collect(): AccessRules;
  rules(): AccessRules;
}

/** Folder + application rule selects. `inherit` means "remove the direct rule". */
export function renderDirectRulesEditor(options: {
  shareNames: string[];
  rules: AccessRules;
  onChange: () => void;
}): DirectRulesEditor {
  const folderRules = new Map<string, FolderRule>();
  for (const rule of options.rules.folderRules) folderRules.set(rule.shareName, rule.rule);
  const appRules = new Map<string, AppRule>();
  for (const rule of options.rules.appRules) appRules.set(rule.appId, rule.rule);

  // Direct rules may reference shares that are no longer listed; keep them
  // visible so an admin can clear them.
  const folderNames = Array.from(new Set([...options.shareNames, ...folderRules.keys()])).sort((a, b) =>
    a.localeCompare(b),
  );
  const MAX_ROWS = 200;
  const shownFolders = folderNames.slice(0, MAX_ROWS);

  const folderBody = h("tbody");
  for (const shareName of shownFolders) {
    const select = h(
      "select",
      { class: "select select-sm rule-select", ariaLabel: `Direct rule for ${shareName}` },
      ...FOLDER_RULE_OPTIONS.map((option) =>
        h("option", { value: option.value, selected: (folderRules.get(shareName) ?? "inherit") === option.value }, option.label),
      ),
    );
    select.addEventListener("change", () => {
      folderRules.set(shareName, select.value as FolderRule);
      options.onChange();
    });
    folderBody.appendChild(
      h("tr", {}, h("td", { class: "mono small" }, shareName), h("td", { class: "rule-cell" }, select)),
    );
  }
  if (shownFolders.length === 0) {
    folderBody.appendChild(
      h("tr", {}, h("td", { colspan: 2, class: "table-empty" }, "No shared folders available.")),
    );
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
      .filter(([, rule]) => rule !== "inherit")
      .map(([shareName, rule]) => ({ shareName, rule })),
    appRules: Array.from(appRules.entries())
      .filter(([, rule]) => rule !== "inherit")
      .map(([appId, rule]) => ({ appId, rule })),
  });

  const el = h(
    "div",
    { class: "access-rules-editor" },
    h("div", { class: "access-subsection" }, h("div", { class: "access-subsection-title" }, "Shared folders"), folderTable),
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

  return { el, collect, rules: () => collect() };
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
        h("span", { class: "prov-rule" }, source.rule),
      ),
    );
  }
  return wrap;
}

function fallbackLabel(fallback: string | null | undefined): string {
  switch (fallback) {
    case "admin":
      return "Administrator bypass";
    case "legacy-acl":
      return "Legacy share membership";
    case "owner":
      return "Share owner";
    case "explicit":
      return "Explicit rule";
    case "default":
      return "Role default";
    case "none":
      return "No grant";
    default:
      return "";
  }
}

/** Effective Access tables with provenance + explicit-deny explanation. */
export function renderEffectiveAccess(access: EffectiveAccess | null, fallbackError?: string): HTMLElement {
  if (!access) {
    return h(
      "div",
      { class: "inline-availability" },
      h("div", { class: "inline-availability-title" }, "Effective access is not available yet"),
      h(
        "div",
        { class: "small muted" },
        fallbackError || "The server does not expose /api/users/{id}/effective-access yet.",
      ),
    );
  }
  const folderBody = h("tbody");
  for (const folder of access.folders ?? []) {
    const conflict = denyConflict(folder.sources ?? []);
    folderBody.appendChild(
      h(
        "tr",
        {},
        h("td", {}, h("div", { class: "mono small" }, folder.shareName), folder.path ? h("div", { class: "small faint" }, folder.path) : null),
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
  if (!(access.folders ?? []).length) {
    folderBody.appendChild(h("tr", {}, h("td", { colspan: 4, class: "table-empty" }, "No shared folders to evaluate.")));
  }
  const folderTable = h(
    "table",
    { class: "table effective-table" },
    h(
      "thead",
      {},
      h("tr", {}, h("th", {}, "Shared folder"), h("th", {}, "Effective level"), h("th", {}, "Provenance"), h("th", {}, "Notes")),
    ),
    folderBody,
  );

  const appBody = h("tbody");
  for (const app of access.apps ?? []) {
    const constant = RULE_APPS.find((item) => item.id === app.appId);
    const decision = app.allowed ? (app.readOnly ? "Read-only" : "Allowed") : "Denied";
    const conflict = denyConflict(app.sources ?? []);
    appBody.appendChild(
      h(
        "tr",
        {},
        h("td", {}, app.label || constant?.label || appLabel(app.appId)),
        h("td", {}, badge(decision, app.allowed ? (app.readOnly ? "warn" : "ok") : "error")),
        h("td", {}, sourceChips(app.sources ?? [])),
        h(
          "td",
          { class: "small muted" },
          ...[
            fallbackLabel(app.fallback) ? h("span", {}, fallbackLabel(app.fallback)) : null,
            conflict ? h("div", { class: "conflict-note" }, conflict) : null,
          ].filter((node): node is HTMLElement => node !== null),
        ),
      ),
    );
  }
  if (!(access.apps ?? []).length) {
    appBody.appendChild(h("tr", {}, h("td", { colspan: 4, class: "table-empty" }, "No applications to evaluate.")));
  }
  const appTable = h(
    "table",
    { class: "table effective-table" },
    h(
      "thead",
      {},
      h("tr", {}, h("th", {}, "Application"), h("th", {}, "Effective access"), h("th", {}, "Provenance"), h("th", {}, "Notes")),
    ),
    appBody,
  );

  return h(
    "div",
    { class: "effective-access" },
    h(
      "div",
      { class: "small muted" },
      "Explicit No Access always wins over grants. Effective Access shows the decision and where it came from.",
    ),
    h(
      "div",
      { class: "access-subsection" },
      h("div", { class: "access-subsection-title" }, "Shared folders"),
      h("div", { class: "table-wrap table-wrap-compact" }, folderTable),
    ),
    h(
      "div",
      { class: "access-subsection" },
      h("div", { class: "access-subsection-title" }, "Applications"),
      h("div", { class: "table-wrap table-wrap-compact" }, appTable),
    ),
  );
}

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

/** Empty-state text for permissions fallback (older server). */
export function permissionsFallback(permissions: Permissions | null): boolean {
  return !permissions || !permissions.apps;
}
