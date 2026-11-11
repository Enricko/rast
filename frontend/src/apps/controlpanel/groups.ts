// Control Panel → Users & Groups → Groups.
//
// NAS-style group management: search/paginated table, create/edit modal with
// General / Members / Shared Folders / Applications and one Save action, and a
// guarded delete. Works against docs/GROUPS.md; if the groups API is not
// available yet the view shows an explicit "not available" state.

import { apiErrorMessage, ApiError } from "../../core/api";
import { pager } from "../../core/components";
import { debounce, h } from "../../core/dom";
import { relativeTime } from "../../core/format";
import { formField, validateRequiredText } from "../../core/forms";
import { iconSvg } from "../../core/icons";
import { debounceTask, getRealtime } from "../../core/realtime";
import type { GroupDetail, GroupInput, GroupSummary, Page, Share, User } from "../../core/types";
import { confirmDialog, contextMenu, modal } from "../../core/ui";
import {
  APP_RULE_OPTIONS,
  FOLDER_RULE_OPTIONS,
  RULE_APPS,
  createSaveWorkflow,
  pageIntro,
  type TabRenderContext,
} from "./common";

const PER_PAGE = 25;
const MAX_MEMBERS_SHOWN = 200;

function asArray<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : [];
}

function normalizeDetail(detail: Partial<GroupDetail> | null | undefined): GroupDetail {
  const members = asArray<GroupDetail["members"][number]>(detail?.members);
  const folderRules = asArray<GroupDetail["folderRules"][number]>(detail?.folderRules);
  const appRules = asArray<GroupDetail["appRules"][number]>(detail?.appRules);
  return {
    id: Number(detail?.id ?? 0),
    name: String(detail?.name ?? ""),
    description: String(detail?.description ?? ""),
    memberCount: Number(detail?.memberCount ?? members.length ?? 0),
    folderRuleCount: Number(detail?.folderRuleCount ?? folderRules.length ?? 0),
    appRuleCount: Number(detail?.appRuleCount ?? appRules.length ?? 0),
    createdAt: Number(detail?.createdAt ?? 0),
    updatedAt: Number(detail?.updatedAt ?? 0),
    members,
    folderRules,
    appRules,
  };
}

export async function renderGroupsView(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  let page = 1;
  let query = "";
  let total = 0;
  let loadToken = 0;
  let unavailable = false;
  let openEditor: GroupEditorHandle | null = null;

  const intro = pageIntro(
    "Groups",
    "Groups grant shared-folder and application access to their members. Explicit No Access always wins over a grant; administrator role checks still apply.",
  );
  const search = h("input", {
    class: "input users-search",
    type: "search",
    placeholder: "Search groups…",
    ariaLabel: "Search groups",
  });
  const countLabel = h("div", { class: "small muted" });
  const toolbar = h(
    "div",
    { class: "cp-toolbar" },
    h("div", { class: "search-wrap" }, h("span", { class: "search-icon" }, iconSvg("search", 16)), search),
    h("div", { class: "cp-toolbar-spacer" }),
    countLabel,
  );
  const tableBody = h("tbody");
  const table = h(
    "table",
    { class: "table groups-table" },
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", {}, "Group"),
        h("th", { class: "hide-sm" }, "Description"),
        h("th", { class: "num" }, "Members"),
        h("th", { class: "num hide-md" }, "Folder rules"),
        h("th", { class: "num hide-md" }, "App rules"),
        h("th", { class: "hide-sm" }, "Updated"),
        h("th", { class: "actions-col" }, h("span", { class: "sr-only" }, "Actions")),
      ),
    ),
    tableBody,
  );
  const tableWrap = h("div", { class: "table-wrap cp-table-wrap" }, table);
  const pagerControl = pager((nextPage) => {
    page = nextPage;
    void load();
  });
  const footer = h("div", { class: "cp-table-foot" }, pagerControl.el);
  const bodyHost = h("div", { class: "groups-body" });
  host.append(intro, toolbar, bodyHost);

  const createButton = h(
    "button",
    { class: "btn btn-primary", type: "button", onClick: () => void openGroupEditor(null) },
    iconSvg("plus", 16),
    "Create group",
  );
  actions.appendChild(createButton);

  const debouncedSearch = debounce(() => {
    query = search.value.trim();
    page = 1;
    void load();
  }, 280);
  search.addEventListener("input", debouncedSearch);

  function renderUnavailable(error: unknown): void {
    unavailable = true;
    createButton.disabled = true;
    let title = "Groups are not available";
    let detail = apiErrorMessage(error);
    let hint = "The server may not expose the groups API yet.";
    if (error instanceof ApiError) {
      if (error.isNotImplemented || error.isNotFound) {
        title = "Groups are not available yet";
        hint = "This server build does not expose /api/groups yet. The page will populate once the groups backend is enabled.";
      } else if (error.isUnavailable) {
        title = "Groups are temporarily unavailable";
        hint = "The server reported that the access subsystem is unavailable. Other Control Panel sections still work.";
      } else if (error.isForbidden) {
        title = "Administrator access required";
        hint = "Group management is restricted to administrator accounts.";
      }
    }
    bodyHost.replaceChildren(
      h(
        "div",
        { class: "not-available-state" },
        h("div", { class: "not-available-icon" }, iconSvg("users", 30)),
        h("div", { class: "not-available-title" }, title),
        h("div", { class: "not-available-detail muted" }, detail),
        h("div", { class: "not-available-hint small faint" }, hint),
        h(
          "div",
          { class: "row" },
          h("button", { class: "btn", type: "button", onClick: () => void load() }, iconSvg("refresh", 15), "Retry"),
        ),
      ),
    );
  }

  function renderGroups(groups: GroupSummary[]): void {
    bodyHost.replaceChildren(tableWrap, footer);
    tableBody.replaceChildren();
    if (groups.length === 0) {
      tableBody.appendChild(
        h(
          "tr",
          {},
          h(
            "td",
            { colspan: 7 },
            h(
              "div",
              { class: "table-empty" },
              query ? `No groups match “${query}”.` : "No groups yet. Create one to grant shared-folder or application access.",
            ),
          ),
        ),
      );
      return;
    }
    for (const group of groups) {
      const actionsButton = h(
        "button",
        {
          class: "btn btn-icon btn-subtle btn-sm",
          type: "button",
          ariaLabel: `Actions for ${group.name}`,
          title: "Group actions",
        },
        iconSvg("more", 16),
      );
      actionsButton.addEventListener("click", (event) => {
        event.stopPropagation();
        const rect = actionsButton.getBoundingClientRect();
        contextMenu(
          rect.right - 12,
          rect.bottom + 4,
          [
            { label: "Edit group", icon: "edit", onClick: () => void openGroupEditor(group.id) },
            { label: "Delete group", icon: "trash", danger: true, onClick: () => void deleteGroup(group) },
          ],
          { ariaLabel: `${group.name} actions` },
        );
      });
      const row = h(
        "tr",
        { class: "is-clickable", tabIndex: 0, role: "button", ariaLabel: `Edit group ${group.name}` },
        h(
          "td",
          {},
          h(
            "div",
            { class: "user-cell" },
            h("span", { class: "group-glyph" }, iconSvg("users", 16)),
            h("span", { class: "user-cell-text" }, h("span", { class: "user-cell-name" }, group.name)),
          ),
        ),
        h("td", { class: "hide-sm small muted group-description" }, group.description || "—"),
        h("td", { class: "num" }, String(group.memberCount ?? 0)),
        h("td", { class: "num hide-md" }, String(group.folderRuleCount ?? 0)),
        h("td", { class: "num hide-md" }, String(group.appRuleCount ?? 0)),
        h("td", { class: "hide-sm small muted" }, group.updatedAt ? relativeTime(group.updatedAt) : "—"),
        h("td", { class: "actions-col" }, actionsButton),
      );
      row.addEventListener("click", () => void openGroupEditor(group.id));
      row.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          void openGroupEditor(group.id);
        }
      });
      tableBody.appendChild(row);
    }
  }

  async function load(): Promise<void> {
    const token = ++loadToken;
    if (!unavailable) {
      bodyHost.replaceChildren(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading groups…")));
    }
    try {
      const response = await ctx.api.get<Page<GroupSummary>>("/api/groups", {
        page,
        perPage: PER_PAGE,
        q: query || undefined,
      });
      if (token !== loadToken) return;
      unavailable = false;
      createButton.disabled = false;
      const items = asArray<GroupSummary>(response.items);
      total = Number(response.total ?? items.length);
      const pages = Math.max(1, Math.ceil(total / PER_PAGE));
      if (page > pages && total > 0) {
        page = pages;
        void load();
        return;
      }
      countLabel.textContent = total === 0 ? "No groups" : `${total} group${total === 1 ? "" : "s"}`;
      pagerControl.update({ page: response.page ?? page, totalPages: pages, totalItems: total });
      renderGroups(items);
    } catch (error) {
      if (token !== loadToken) return;
      renderUnavailable(error);
      countLabel.textContent = "";
      pagerControl.update({ page: 1, totalPages: 1, totalItems: 0, label: "—" });
    }
  }

  async function deleteGroup(group: GroupSummary): Promise<void> {
    let detail: GroupDetail | null = null;
    try {
      detail = normalizeDetail(await ctx.api.get<GroupDetail>(`/api/groups/${group.id}`));
    } catch {
      detail = null;
    }
    const members = detail?.memberCount ?? group.memberCount ?? 0;
    const folderRules = detail?.folderRuleCount ?? group.folderRuleCount ?? 0;
    const appRules = detail?.appRuleCount ?? group.appRuleCount ?? 0;
    const ok = await confirmDialog({
      title: "Delete group",
      message:
        `Delete “${group.name}”?\n\n` +
        `Affected: ${members} user${members === 1 ? "" : "s"}, ${folderRules} shared-folder rule${folderRules === 1 ? "" : "s"}, ${appRules} application rule${appRules === 1 ? "" : "s"}.\n\n` +
        "Users, shared folders and files are retained. Users lose the access this group granted; explicit direct rules stay in place.",
      confirmLabel: "Delete group",
      danger: true,
      requireTyped: group.name,
    });
    if (!ok) return;
    try {
      const response = await ctx.api.del<{
        deleted?: boolean;
        affectedUsers?: number;
        affectedFolderRules?: number;
        affectedAppRules?: number;
      }>(`/api/groups/${group.id}`);
      const affected = response?.affectedUsers ?? members;
      ctx.toast.success(
        `Group “${group.name}” deleted${affected ? ` — ${affected} user${affected === 1 ? "" : "s"} affected` : ""}`,
      );
      if (openEditor?.groupId === group.id) openEditor.close();
      await load();
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  async function openGroupEditor(groupId: number | null): Promise<void> {
    let detail: GroupDetail | null = null;
    let users: User[] = [];
    let shares: Share[] = [];
    const problems: string[] = [];
    const loads = await Promise.allSettled([
      groupId === null ? Promise.resolve(null) : ctx.api.get<GroupDetail>(`/api/groups/${groupId}`),
      ctx.api.get<Page<User>>("/api/users", { page: 1, perPage: 100 }),
      ctx.api.get<Share[]>("/api/shares"),
    ]);
    if (loads[0].status === "fulfilled" && loads[0].value) detail = normalizeDetail(loads[0].value);
    else if (groupId !== null && loads[0].status === "rejected") problems.push(`Group: ${apiErrorMessage(loads[0].reason)}`);
    if (loads[1].status === "fulfilled") {
      // Deactivated accounts cannot be added to groups; existing memberships
      // are preserved (their ids stay selected) but are not offered again.
      users = asArray<User>(loads[1].value.items).filter((user) => user.active !== false);
    }
    else problems.push(`Users: ${apiErrorMessage(loads[1].reason)}`);
    if (loads[2].status === "fulfilled") shares = asArray<Share>(loads[2].value);
    else problems.push(`Shared folders: ${apiErrorMessage(loads[2].reason)}`);
    if (groupId !== null && !detail) {
      ctx.toast.error("Could not load this group");
      return;
    }

    const initial = detail ?? {
      ...normalizeDetail(null),
      name: "",
      description: "",
    };
    const folderState = new Map<string, string>();
    for (const rule of initial.folderRules) folderState.set(rule.shareName, rule.rule);
    const appState = new Map<string, string>();
    for (const rule of initial.appRules) appState.set(rule.appId, rule.rule);
    const selectedMembers = new Set<number>(initial.members.map((member) => Number(member.userId)));

    const nameInput = h("input", { class: "input", type: "text", value: initial.name, autocomplete: "off", spellcheck: false });
    const nameField = formField({ label: "Group name", control: nameInput, required: true, hint: "Stable id: renaming keeps the group and its rules." });
    const descriptionInput = h("input", {
      class: "input",
      type: "text",
      value: initial.description,
      placeholder: "Optional description",
      autocomplete: "off",
    });
    const descriptionField = formField({ label: "Description (optional)", control: descriptionInput });

    // ---- members
    const memberSearch = h("input", {
      class: "input input-search",
      type: "search",
      placeholder: "Search users…",
      ariaLabel: "Search users",
    });
    const memberList = h("div", { class: "checklist", role: "group", ariaLabel: "Group members" });
    const memberCount = h("span", { class: "small muted" });
    const renderMembers = (): void => {
      const query = memberSearch.value.trim().toLowerCase();
      memberList.replaceChildren();
      const shown = users
        .filter((user) => !query || user.username.toLowerCase().includes(query) || user.displayName.toLowerCase().includes(query))
        .slice(0, MAX_MEMBERS_SHOWN);
      for (const user of shown) {
        const input = h("input", { class: "checkbox-input", type: "checkbox", checked: selectedMembers.has(user.id), ariaLabel: user.username });
        input.addEventListener("change", () => {
          if (input.checked) selectedMembers.add(user.id);
          else selectedMembers.delete(user.id);
          memberCount.textContent = `${selectedMembers.size} selected`;
          markDirty();
        });
        memberList.appendChild(
          h(
            "label",
            { class: "checklist-row" },
            input,
            h(
              "span",
              { class: "checklist-label" },
              h("span", { class: "user-cell-name" }, user.displayName || user.username),
              h("span", { class: "small faint" }, `@${user.username}`),
            ),
          ),
        );
      }
      if (shown.length === 0) {
        memberList.appendChild(h("div", { class: "field-hint" }, users.length === 0 ? "No user accounts available." : "No users match this search."));
      }
      memberCount.textContent = `${selectedMembers.size} selected`;
    };
    memberSearch.addEventListener("input", renderMembers);
    renderMembers();

    // ---- folder rules
    const folderNames = Array.from(new Set([...shares.map((share) => share.name), ...folderState.keys()])).sort((a, b) => a.localeCompare(b));
    const folderBody = h("tbody");
    for (const shareName of folderNames) {
      const select = h(
        "select",
        { class: "select select-sm rule-select", ariaLabel: `Rule for shared folder ${shareName}` },
        ...FOLDER_RULE_OPTIONS.map((option) =>
          h("option", { value: option.value, selected: (folderState.get(shareName) ?? "inherit") === option.value }, option.label),
        ),
      );
      select.addEventListener("change", () => {
        folderState.set(shareName, select.value);
        markDirty();
      });
      folderBody.appendChild(h("tr", {}, h("td", { class: "mono small" }, shareName), h("td", { class: "rule-cell" }, select)));
    }
    if (folderNames.length === 0) {
      folderBody.appendChild(h("tr", {}, h("td", { colspan: 2, class: "table-empty" }, "No shared folders available.")));
    }
    const folderTable = h(
      "table",
      { class: "table rules-table" },
      h("thead", {}, h("tr", {}, h("th", {}, "Shared folder"), h("th", { class: "rule-col" }, "Rule"))),
      folderBody,
    );

    // ---- app rules
    const appBody = h("tbody");
    for (const app of RULE_APPS) {
      const select = h(
        "select",
        { class: "select select-sm rule-select", ariaLabel: `Rule for ${app.label}` },
        ...APP_RULE_OPTIONS.map((option) =>
          h("option", { value: option.value, selected: (appState.get(app.id) ?? "inherit") === option.value }, option.label),
        ),
      );
      select.addEventListener("change", () => {
        appState.set(app.id, select.value);
        markDirty();
      });
      appBody.appendChild(h("tr", {}, h("td", {}, app.label), h("td", { class: "rule-cell" }, select)));
    }
    const appTable = h(
      "table",
      { class: "table rules-table" },
      h("thead", {}, h("tr", {}, h("th", {}, "Application"), h("th", { class: "rule-col" }, "Rule"))),
      appBody,
    );

    const conflictHost = h("div", { class: "cp-conflict", hidden: true });
    const availabilityHost = problems.length
      ? h(
          "div",
          { class: "inline-availability" },
          h("div", { class: "inline-availability-title" }, "Some sections could not be loaded"),
          h("div", { class: "small muted" }, problems.join(" · ")),
        )
      : null;

    let closeModal: (() => void) | null = null;
    let saving = false;
    const workflow = createSaveWorkflow<GroupDetail>({
      save: async () => {
        nameField.setError(null);
        const nameError = validateRequiredText(nameInput.value, "Group name", 64);
        if (nameError) {
          nameField.setError(nameError);
          throw new Error(nameError);
        }
        if (descriptionInput.value.length > 500) {
          descriptionField.setError("Description must be at most 500 characters");
          throw new Error("Description must be at most 500 characters");
        }
        const body: GroupInput = {
          name: nameInput.value.trim(),
          description: descriptionInput.value.trim(),
          members: Array.from(selectedMembers),
          folderRules: Array.from(folderState.entries())
            .filter(([, rule]) => rule !== "inherit")
            .map(([shareName, rule]) => ({ shareName, rule: rule as GroupDetail["folderRules"][number]["rule"] })),
          appRules: Array.from(appState.entries())
            .filter(([, rule]) => rule !== "inherit")
            .map(([appId, rule]) => ({ appId, rule: rule as GroupDetail["appRules"][number]["rule"] })),
        };
        saving = true;
        try {
          const saved =
            groupId === null
              ? await ctx.api.post<GroupDetail>("/api/groups", body)
              : await ctx.api.put<GroupDetail>(`/api/groups/${groupId}`, body);
          return normalizeDetail(saved);
        } finally {
          saving = false;
        }
      },
      onSaved: (saved) => {
        ctx.toast.success(groupId === null ? `Group “${saved.name}” created` : `Group “${saved.name}” saved`);
        closeAfterSave();
      },
      // Deferred: the workflow renders synchronously during construction.
      onStateChange: () => {
        queueMicrotask(() => refreshDirty());
      },
    });

    function currentSignature(): string {
      const folder = Array.from(folderState.entries()).sort(([a], [b]) => a.localeCompare(b));
      const apps = Array.from(appState.entries()).sort(([a], [b]) => a.localeCompare(b));
      return JSON.stringify({
        name: nameInput.value.trim(),
        description: descriptionInput.value.trim(),
        members: Array.from(selectedMembers).sort((a, b) => a - b),
        folder,
        apps,
      });
    }
    const initialSignature = currentSignature();

    function refreshDirty(): void {
      if (saving) return;
      const dirty = currentSignature() !== initialSignature;
      const nameError = validateRequiredText(nameInput.value, "Group name", 64);
      if (dirty && nameError) {
        workflow.setValidationError(nameError);
      } else {
        workflow.setDirty(dirty);
      }
    }

    function markDirty(): void {
      refreshDirty();
    }

    nameInput.addEventListener("input", markDirty);
    descriptionInput.addEventListener("input", markDirty);

    const bodyHost = h(
      "div",
      { class: "col group-editor" },
      availabilityHost,
      conflictHost,
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "General")),
        nameField.el,
        descriptionField.el,
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Members"), memberCount),
        memberSearch,
        memberList,
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Shared Folders")),
        folderTable,
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Applications")),
        appTable,
      ),
      workflow.el,
    );

    const deleteButton =
      groupId === null
        ? null
        : h(
            "button",
            { class: "btn btn-danger btn-sm", type: "button" },
            iconSvg("trash", 15),
            "Delete group",
          );
    const cancelButton = h("button", { class: "btn btn-ghost", type: "button" }, "Cancel");
    deleteButton?.addEventListener("click", () => {
      closeModal?.();
      void deleteGroup(detail ?? { ...initial, id: groupId!, memberCount: initial.members.length });
    });
    cancelButton.addEventListener("click", () => closeModal?.());
    workflow.el.querySelector(".appearance-actions-leading")?.append(deleteButton ?? "", cancelButton);

    const close = modal({
      title: groupId === null ? "Create group" : `Edit group — ${initial.name}`,
      body: bodyHost,
      wide: true,
      onClose: () => {
        openEditor = null;
        workflow.dispose();
      },
    });
    closeModal = close;
    openEditor = {
      groupId,
      isDirty: () => workflow.isDirty(),
      busy: () => workflow.state === "saving" || workflow.state === "saved",
      notifyExternalChange: () => {
        // Our own save also emits group.changed; never flash a conflict for it.
        if (workflow.state === "saving" || workflow.state === "saved") return;
        showGroupConflict();
      },
      close,
    };

    function closeAfterSave(): void {
      window.setTimeout(() => {
        close();
        void load();
      }, 600);
    }

    function showGroupConflict(): void {
      conflictHost.hidden = false;
      conflictHost.replaceChildren(
        h(
          "div",
          { class: "banner banner-warn", role: "status" },
          h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
          h(
            "div",
            { class: "banner-body" },
            h("div", { class: "banner-title" }, "Group changed in another session"),
            h(
              "div",
              { class: "banner-message" },
              "Your unsaved edits were kept. Reload to review the latest saved group; reloading discards this draft.",
            ),
            h(
              "button",
              {
                class: "btn btn-sm",
                type: "button",
                onClick: () => {
                  close();
                  void load();
                  window.setTimeout(() => void openGroupEditor(groupId), 120);
                },
              },
              "Reload group",
            ),
          ),
        ),
      );
    }

    workflow.setDirty(false);
    refreshDirty();
    nameInput.focus();
    window.setTimeout(() => nameInput.select(), 40);
  }

  interface GroupEditorHandle {
    groupId: number | null;
    isDirty(): boolean;
    busy(): boolean;
    notifyExternalChange(): void;
    close(): void;
  }

  // ------------------------------------------------------------- realtime
  const realtime = getRealtime(ctx.events);
  const refreshGroups = debounceTask(() => {
    void load();
  }, 350);
  const unsubscribeGroup = realtime.on<{ reason?: string; groupId?: number }>("group.changed", () => {
    if (openEditor?.busy()) return;
    if (openEditor) {
      if (openEditor.isDirty()) openEditor.notifyExternalChange();
      else {
        // Clean editor: refresh it (and the list) from the server.
        const current = openEditor;
        current.close();
        refreshGroups.run();
        window.setTimeout(() => void openGroupEditor(current.groupId), 150);
      }
      return;
    }
    refreshGroups.run();
  });
  const unsubscribeUsers = realtime.on("user.changed", () => refreshGroups.run());
  const unsubscribeResync = realtime.onResync(() => refreshGroups.run());

  await load();

  return () => {
    refreshGroups.cancel();
    unsubscribeGroup();
    unsubscribeUsers();
    unsubscribeResync();
    openEditor?.close();
    openEditor = null;
    loadToken += 1;
  };
}
