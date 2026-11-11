// Control Panel → Users: paginated account administration with role, status,
// quota, password reset and session management.

import { ApiError, apiErrorMessage } from "../../core/api";
import { pager, segmented, switchControl } from "../../core/components";
import { debounce, h } from "../../core/dom";
import {
  bytesToGib,
  formField,
  gibToBytes,
  parseGib,
  passwordField,
  validateGib,
  validatePassword,
  validateRequiredText,
  validateUsername,
} from "../../core/forms";
import { formatDateTime, relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { debounceTask, getRealtime } from "../../core/realtime";
import type {
  AccessRules,
  GroupRef,
  Page,
  SessionInfo,
  SystemInfo,
  User,
  UserSessionList,
} from "../../core/types";
import { confirmDialog, contextMenu, errorMessage, modal } from "../../core/ui";
import {
  activeBadge,
  createSaveWorkflow,
  describeUserAgent,
  lastLoginCell,
  quotaCell,
  roleBadge,
  type SaveWorkflow,
  type TabRenderContext,
} from "./common";
import {
  accessAvailability,
  createEffectiveAccessPanel,
  createEntityPicker,
  groupChips,
  loadUserAccessData,
  renderDirectRulesEditor,
  type EffectiveAccessPanel,
  type EntityPicker,
  type PickerOption,
} from "./access-ui";
import { renderGroupsView } from "./groups";

const PER_PAGE_OPTIONS = [10, 25, 50, 100];
const DEFAULT_PER_PAGE = 25;

function asArray<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : [];
}

/**
 * Control Panel → Users & Groups: inner Users / Groups tabs sharing the same
 * content area and header action slot.
 */
export async function renderUsersTab(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();
  const requested = (ctx.params ?? {}) as { tab?: string; inner?: string; section?: string };
  let active: "users" | "groups" = requested.inner === "groups" || tab.section === "groups" ? "groups" : "users";
  let cleanup: (() => void) | void;
  let switched = false;

  const content = h("div", { class: "users-groups-content" });
  const control = segmented<"users" | "groups">({
    items: [
      { value: "users", label: "Users", icon: "users" },
      { value: "groups", label: "Groups", icon: "share" },
    ],
    value: active,
    ariaLabel: "Users and groups sections",
    onChange: (value) => {
      active = value;
      void switchView();
    },
  });
  const switchHint = h(
    "div",
    { class: "cp-subtabs-hint small muted" },
    "Users: accounts, groups, direct overrides and effective access. Groups: shared-folder and application rules for their members.",
  );
  host.append(
    h(
      "div",
      { class: "cp-subtabs" },
      h("div", { class: "cp-subtabs-row" }, control.el),
      switchHint,
    ),
    content,
  );

  async function switchView(): Promise<void> {
    if (cleanup) {
      cleanup();
      cleanup = undefined;
    }
    content.replaceChildren();
    actions.replaceChildren();
    const innerTab: TabRenderContext = {
      ...tab,
      host: content,
      actions,
      section: switched ? undefined : tab.section,
    };
    switched = true;
    try {
      cleanup = active === "users" ? await renderUsersView(innerTab) : await renderGroupsView(innerTab);
    } catch (error) {
      content.replaceChildren(
        h(
          "div",
          { class: "error-state" },
          h("div", { class: "error-title" }, active === "users" ? "Could not load users" : "Could not load groups"),
          h("div", { class: "error-detail" }, errorMessage(error)),
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void switchView() }, "Retry"),
        ),
      );
    }
  }

  await switchView();
  return () => {
    cleanup?.();
    cleanup = undefined;
  };
}

async function renderUsersView(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  const viewer = ctx.store.get().user;
  if (!viewer) throw new Error("not authenticated");
  const viewerId = viewer.id;
  let page = 1;
  let perPage = DEFAULT_PER_PAGE;
  let query = "";
  let adminCount: number | null = null;
  let loadToken = 0;
  interface UserAccessEditorHandle {
    userId: number;
    isDirty(): boolean;
    busy(): boolean;
    notifyExternalChange(message?: string): void;
    refreshAccess(): Promise<void>;
    reload(): Promise<void>;
  }
  let openAccessEditor: UserAccessEditorHandle | null = null;

  host.replaceChildren();
  actions.replaceChildren();

  const intro = h(
    "div",
    { class: "cp-intro" },
    h("h2", { class: "cp-intro-title" }, "User accounts"),
    h(
      "p",
      { class: "cp-intro-detail muted" },
      "Create accounts, assign roles and quotas, reset passwords and revoke sessions. Disabling an account preserves its files.",
    ),
  );

  const singleAdminBanner = h("div", { class: "banner banner-warn", hidden: true, role: "status" });
  singleAdminBanner.append(
    h("span", { class: "banner-icon" }, iconSvg("shieldAlert", 17)),
    h(
      "div",
      { class: "banner-body" },
      h("div", { class: "banner-title" }, "Only one administrator"),
      h("div", { class: "banner-message" }, "RAST will not allow the last active administrator to be demoted or disabled."),
    ),
  );

  const search = h("input", {
    class: "input users-search",
    type: "search",
    placeholder: "Search username or display name…",
    ariaLabel: "Search users",
  });
  const countLabel = h("div", { class: "small muted" });
  const perPageSelect = h(
    "select",
    { class: "select select-sm", ariaLabel: "Rows per page" },
    ...PER_PAGE_OPTIONS.map((value) => h("option", { value: String(value), selected: value === perPage }, `${value} / page`)),
  );
  perPageSelect.addEventListener("change", () => {
    perPage = Number(perPageSelect.value) || DEFAULT_PER_PAGE;
    page = 1;
    void load();
  });

  const toolbar = h(
    "div",
    { class: "cp-toolbar" },
    h("div", { class: "search-wrap" }, h("span", { class: "search-icon" }, iconSvg("search", 16)), search),
    h("div", { class: "cp-toolbar-spacer" }),
    countLabel,
    perPageSelect,
  );

  const tableBody = h("tbody");
  const table = h(
    "table",
    { class: "table users-table" },
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", {}, "User"),
        h("th", {}, "Role"),
        h("th", { class: "hide-sm" }, "Groups"),
        h("th", {}, "Status"),
        h("th", { class: "quota-col" }, "Quota"),
        h("th", { class: "hide-sm" }, "Last sign-in"),
        h("th", { class: "actions-col" }, h("span", { class: "sr-only" }, "Actions")),
      ),
    ),
    tableBody,
  );
  const pagerControl = pager((nextPage) => {
    page = nextPage;
    void load();
  });
  const footer = h("div", { class: "cp-table-foot" }, pagerControl.el);

  const addButton = h(
    "button",
    { class: "btn btn-primary", type: "button", onClick: () => openCreateModal() },
    iconSvg("plus", 16),
    "Add user",
  );
  actions.appendChild(addButton);

  host.append(intro, singleAdminBanner, toolbar, h("div", { class: "table-wrap cp-table-wrap" }, table), footer);

  const debouncedSearch = debounce(() => {
    query = search.value.trim();
    page = 1;
    void load();
  }, 280);
  search.addEventListener("input", debouncedSearch);

  // ------------------------------------------------------------------ data
  async function refreshAdminCount(): Promise<void> {
    try {
      const info = await ctx.api.get<SystemInfo>("/api/system/info");
      adminCount = info.counts.admins;
      singleAdminBanner.hidden = adminCount > 1;
    } catch {
      adminCount = null;
      singleAdminBanner.hidden = true;
    }
  }

  async function load(): Promise<void> {
    const token = ++loadToken;
    renderLoading();
    try {
      const response = await ctx.api.get<Page<User>>("/api/users", {
        page,
        perPage,
        q: query || undefined,
      });
      if (token !== loadToken) return;
      const pageCount = Math.max(1, Math.ceil(response.total / perPage));
      if (page > pageCount && response.total > 0) {
        page = pageCount;
        void load();
        return;
      }
      renderRows(response.items);
      const start = response.total === 0 ? 0 : (response.page - 1) * response.perPage + 1;
      const end = Math.min(response.total, response.page * response.perPage);
      countLabel.textContent = response.total === 0 ? "No accounts" : `${start}–${end} of ${response.total}`;
      pagerControl.update({ page: response.page, totalPages: pageCount, totalItems: response.total });
    } catch (error) {
      if (token !== loadToken) return;
      tableBody.replaceChildren(
        h(
          "tr",
          {},
          h(
            "td",
            { colspan: 7 },
            h(
              "div",
              { class: "error-state inline-state" },
              h("div", { class: "error-title" }, "Could not load users"),
              h("div", { class: "error-detail" }, apiErrorMessage(error)),
              h("button", { class: "btn btn-sm", type: "button", onClick: () => void load() }, "Retry"),
            ),
          ),
        ),
      );
      countLabel.textContent = "";
    }
  }

  function renderLoading(): void {
    tableBody.replaceChildren(
      ...Array.from({ length: 5 }, () =>
        h(
          "tr",
          { class: "is-loading" },
          ...Array.from({ length: 6 }, () => h("td", {}, h("div", { class: "skeleton-line", ariaHidden: true }))),
        ),
      ),
    );
  }

  function renderRows(users: User[]): void {
    tableBody.replaceChildren();
    if (users.length === 0) {
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
              query ? `No accounts match “${query}”.` : "No user accounts exist yet.",
            ),
          ),
        ),
      );
      return;
    }
    for (const user of users) {
      const isSelf = user.id === viewerId;
      const actionButton = h(
        "button",
        {
          class: "btn btn-icon btn-subtle btn-sm",
          type: "button",
          ariaLabel: `Actions for ${user.username}`,
          title: "Account actions",
        },
        iconSvg("more", 16),
      );
      actionButton.addEventListener("click", () => {
        const rect = actionButton.getBoundingClientRect();
        const canDeactivate = !isSelf && !(user.role === "admin" && user.active && adminCount === 1);
        contextMenu(
          rect.right - 12,
          rect.bottom + 4,
          [
            { label: "Edit user", icon: "edit", onClick: () => openEditModal(user) },
            { label: "Reset password", icon: "key", onClick: () => openResetPasswordModal(user) },
            { label: "View sessions", icon: "sessions", onClick: () => openSessionsModal(user) },
            "separator",
            user.active
              ? {
                  label: "Disable account",
                  icon: "power",
                  danger: true,
                  disabled: !canDeactivate,
                  onClick: () => void deactivateUser(user),
                }
              : { label: "Enable account", icon: "check", onClick: () => void enableUser(user) },
          ],
          { ariaLabel: `${user.username} actions` },
        );
      });
      const row = h(
        "tr",
        {},
        h(
          "td",
          {},
          h(
            "div",
            { class: "user-cell" },
            h("span", { class: "avatar avatar-sm" }, (user.displayName || user.username).slice(0, 1).toUpperCase()),
            h(
              "span",
              { class: "user-cell-text" },
              h("span", { class: "user-cell-name" }, user.displayName || user.username, isSelf ? h("span", { class: "badge badge-you" }, "you") : null),
              h("span", { class: "user-cell-id small faint" }, `@${user.username}`),
            ),
          ),
        ),
        h("td", {}, roleBadge(user.role)),
        h("td", { class: "hide-sm" }, groupChips(user.groups, 2)),
        h("td", {}, activeBadge(user.active)),
        h("td", { class: "quota-col" }, quotaCell(user)),
        h("td", { class: "hide-sm" }, lastLoginCell(user)),
        h("td", { class: "actions-col" }, actionButton),
      );
      row.addEventListener("dblclick", () => openEditModal(user));
      tableBody.appendChild(row);
    }
  }

  // ---------------------------------------------------------------- actions
  function openCreateModal(): void {
    const username = h("input", { class: "input", type: "text", autocomplete: "off", spellcheck: false });
    const usernameField = formField({
      label: "Username",
      control: username,
      required: true,
      hint: "Letters, digits, dot, dash and underscore.",
    });
    const displayInput = h("input", { class: "input", type: "text", autocomplete: "off" });
    const displayField = formField({ label: "Display name (optional)", control: displayInput });
    const role = h(
      "select",
      { class: "select" },
      h("option", { value: "user", selected: true }, "User"),
      h("option", { value: "admin" }, "Administrator"),
    );
    const roleField = formField({ label: "Role", control: role, hint: "Administrators can manage users, storage and system settings." });
    const quota = h("input", { class: "input", type: "number", min: "0", step: "1", value: "0" });
    const quotaField = formField({
      label: "Storage quota (GiB)",
      control: quota,
      hint: "0 means unlimited. Applies to new uploads and file operations.",
    });
    const password = passwordField({
      label: "Initial password",
      autocomplete: "new-password",
      required: true,
      showStrength: true,
    });
    const alert = h("div", { class: "banner banner-error", hidden: true, role: "alert" });

    const body = h(
      "div",
      { class: "col" },
      usernameField.el,
      displayField.el,
      roleField.el,
      quotaField.el,
      password.el,
      alert,
    );

    modal({
      title: "Add user",
      body,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Create user",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            alert.hidden = true;
            usernameField.setError(null);
            displayField.setError(null);
            password.setError(null);
            let invalid = false;
            const usernameError = validateUsername(username.value);
            if (usernameError) {
              usernameField.setError(usernameError);
              invalid = true;
            }
            const displayError = displayInput.value.trim()
              ? validateRequiredText(displayInput.value, "Display name", 64)
              : null;
            if (displayError) {
              displayField.setError(displayError);
              invalid = true;
            }
            const passwordError = validatePassword(password.value());
            if (passwordError) {
              password.setError(passwordError);
              invalid = true;
            }
            const quotaError = validateGib(quota.value);
            if (quotaError) {
              quotaField.setError(quotaError);
              invalid = true;
            }
            if (invalid) return;
            try {
              await ctx.api.post("/api/users", {
                username: username.value.trim(),
                displayName: displayInput.value.trim() || undefined,
                role: role.value,
                password: password.value(),
                quotaBytes: gibToBytes(parseGib(quota.value)),
              });
              ctx.toast.success(`User ${username.value.trim()} created`);
              close();
              page = 1;
              await refreshAdminCount();
              void load();
            } catch (error) {
              alert.hidden = false;
              alert.textContent = apiErrorMessage(error);
              throw error;
            }
          },
        },
      ],
    });
    window.setTimeout(() => username.focus(), 40);
  }

  function openEditModal(user: User): void {
    void openEditModalAsync(user);
  }

  function optionalAccessError(error: unknown): boolean {
    return (
      error instanceof ApiError &&
      (error.isNotFound || error.isNotImplemented || error.isUnavailable || error.status === 501)
    );
  }

  function normalizeRuleSignature(rules: AccessRules): string {
    return JSON.stringify({
      folders: [...(rules.folderRules ?? [])].sort((a, b) => a.shareName.localeCompare(b.shareName)),
      apps: [...(rules.appRules ?? [])].sort((a, b) => a.appId.localeCompare(b.appId)),
    });
  }

  async function openEditModalAsync(user: User): Promise<void> {
    if (openAccessEditor) return;
    const isSelf = user.id === viewerId;
    const isLastAdminRole = adminCount === 1 && user.role === "admin" && user.active;

    let data: Awaited<ReturnType<typeof loadUserAccessData>>;
    try {
      data = await loadUserAccessData(ctx, user.id);
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
      return;
    }
    const accessData = data;

    // ---------------------------------------------------------------- profile fields
    const displayInput = h("input", {
      class: "input",
      type: "text",
      value: user.displayName,
      autocomplete: "off",
      dataset: { editField: "display" },
    });
    const displayField = formField({ label: "Display name", control: displayInput, required: true });
    const role = h(
      "select",
      { class: "select" },
      h("option", { value: "user", selected: user.role === "user" }, "User"),
      h("option", { value: "admin", selected: user.role === "admin" }, "Administrator"),
    );
    const roleField = formField({ label: "Role", control: role });
    if (isSelf || isLastAdminRole) {
      role.disabled = true;
      roleField.hintEl?.remove();
      roleField.el.appendChild(
        h(
          "span",
          { class: "field-hint" },
          isSelf ? "You cannot change your own role." : "This is the only active administrator; the role is locked.",
        ),
      );
    }
    const activeSwitch = switchControl({ label: "Account is active", checked: user.active });
    if (isSelf || isLastAdminRole) {
      activeSwitch.input.disabled = true;
      if (isSelf) activeSwitch.el.title = "You cannot deactivate your own account.";
      if (isLastAdminRole) activeSwitch.el.title = "The last active administrator cannot be deactivated.";
    }
    const quota = h("input", {
      class: "input",
      type: "number",
      min: "0",
      step: "1",
      value: String(bytesToGib(user.quotaBytes)),
      dataset: { editField: "quota" },
    });
    const quotaField = formField({ label: "Storage quota (GiB)", control: quota, hint: "0 means unlimited." });
    const forceChange = switchControl({
      label: "Require password change at next sign-in",
      checked: user.mustChangePassword,
    });

    // ------------------------------------------------------- group membership (F12)
    const membershipIds =
      accessData.membershipIds.length > 0 ? accessData.membershipIds : (user.groupIds ?? []);
    const membershipNames = new Map<number, string>();
    for (const group of accessData.membershipGroups) membershipNames.set(Number(group.id), group.name);
    for (const group of user.groups ?? []) membershipNames.set(Number(group.id), group.name);
    const initialMembershipOptions: PickerOption[] = membershipIds.map((id) => ({
      id,
      label: membershipNames.get(Number(id)) ?? `Group #${id}`,
    }));
    let membershipGroupCount = h(
      "span",
      { class: "small muted" },
      `${initialMembershipOptions.length} group${initialMembershipOptions.length === 1 ? "" : "s"} selected`,
    );
    let groupPicker: EntityPicker | null = null;
    async function fetchGroupsPage(query: string, pageNumber: number, perPage: number): Promise<{ items: PickerOption[]; total: number }> {
      const response = await ctx.api.get<Page<GroupRef>>("/api/groups", {
        q: query || undefined,
        page: pageNumber,
        perPage,
      });
      return {
        items: asArray<GroupRef>(response.items).map((group) => ({ id: Number(group.id), label: group.name })),
        total: Number(response.total ?? 0),
      };
    }
    groupPicker = createEntityPicker({
      fetchPage: fetchGroupsPage,
      selected: initialMembershipOptions,
      onChange: () => {
        if (groupPicker) membershipGroupCount.textContent = `${groupPicker.selectedIds().length} group${groupPicker.selectedIds().length === 1 ? "" : "s"} selected`;
        markDirty();
      },
      searchPlaceholder: "Search groups by name…",
      emptyText: "No groups match this search.",
      ariaLabel: "Group membership",
    });

    // -------------------------------------------------------------- direct overrides
    let baselineRules = normalizeRuleSignature(accessData.rules);
    let rulesEditor = renderDirectRulesEditor({
      shares: accessData.shares,
      rules: accessData.rules,
      onChange: () => markDirty(),
    });
    const rulesHost = h("div", { class: "rules-host" }, rulesEditor.el);
    const rulesHint = h(
      "div",
      { class: "section-note muted" },
      "Direct rules apply to this account only and override what the groups alone would grant. Inherit removes the direct rule for that resource; explicit No Access always wins.",
    );

    // ---------------------------------------------------- effective access (F13)
    const effectivePanel: EffectiveAccessPanel = createEffectiveAccessPanel({
      ctx,
      userId: user.id,
    });
    const effectiveHost = h("div", { class: "effective-host" }, effectivePanel.el);

    // ----------------------------------------------------------------- dirty state
    let baselineMembership = JSON.stringify(
      (groupPicker ? groupPicker.selectedIds() : []).slice().sort((a, b) => a - b),
    );
    const readProfile = () => ({
      displayName: displayInput.value.trim(),
      role: role.disabled ? user.role : (role.value as User["role"]),
      active: activeSwitch.isChecked(),
      quotaBytes: gibToBytes(parseGib(quota.value)),
      mustChangePassword: forceChange.isChecked(),
    });
    const signature = (): string =>
      JSON.stringify({
        profile: readProfile(),
        membership: (groupPicker ? groupPicker.selectedIds() : []).slice().sort((a, b) => a - b),
        rules: normalizeRuleSignature(rulesEditor.collect()),
      });
    let savedSignature = signature();

    function validationMessage(): string | null {
      const displayError = validateRequiredText(displayInput.value, "Display name", 64);
      if (displayError) return displayError;
      const quotaError = validateGib(quota.value);
      if (quotaError) return quotaError;
      const profile = readProfile();
      if (isLastAdminRole && (profile.role !== "admin" || !profile.active)) {
        return "The last active administrator cannot be demoted or disabled.";
      }
      return null;
    }

    let workflow: SaveWorkflow | null = null;
    let closing = false;
    function markDirty(): void {
      if (!workflow || closing) return;
      workflow.markEdited();
      const dirty = signature() !== savedSignature;
      const invalid = validationMessage();
      if (dirty && invalid) workflow.setValidationError(invalid);
      else workflow.setDirty(dirty);
    }
    displayInput.addEventListener("input", markDirty);
    quota.addEventListener("input", markDirty);
    role.addEventListener("change", markDirty);
    activeSwitch.input.addEventListener("change", markDirty);
    forceChange.input.addEventListener("change", markDirty);

    // ---------------------------------------------------------------- conflict host
    const conflictHost = h("div", { class: "cp-conflict", hidden: true });
    const availability = accessAvailability(accessData.errors);

    function showConflict(message: string): void {
      conflictHost.hidden = false;
      conflictHost.replaceChildren(
        h(
          "div",
          { class: "banner banner-warn", role: "status" },
          h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
          h(
            "div",
            { class: "banner-body" },
            h("div", { class: "banner-title" }, message),
            h("div", { class: "banner-message" }, "Your unsaved edits were kept. Reload to review the latest values (the draft is discarded)."),
            h(
              "div",
              { class: "row wrap" },
              h(
                "button",
                {
                  class: "btn btn-sm",
                  type: "button",
                  onClick: () => {
                    close();
                    void reloadUserAndReopen();
                  },
                },
                "Reload user",
              ),
              h(
                "button",
                { class: "btn btn-sm btn-subtle", type: "button", onClick: () => (conflictHost.hidden = true) },
                "Keep editing",
              ),
            ),
          ),
        ),
      );
    }

    async function reloadUserAndReopen(): Promise<void> {
      try {
        const page = await ctx.api.get<Page<User>>("/api/users", { page: 1, perPage: 100, q: user.username });
        const fresh = page.items.find((candidate) => candidate.id === user.id);
        if (!fresh) {
          ctx.toast.warning("This account is no longer available.");
          return;
        }
        await load();
        openEditModal(fresh);
      } catch (error) {
        ctx.toast.error(apiErrorMessage(error));
      }
    }

    async function refreshAccess(): Promise<void> {
      const fresh = await loadUserAccessData(ctx, user.id);
      if (!workflow?.isDirty()) {
        const freshNames = new Map<number, string>();
        for (const group of fresh.membershipGroups) freshNames.set(Number(group.id), group.name);
        for (const group of user.groups ?? []) freshNames.set(Number(group.id), group.name);
        const selected = fresh.membershipIds.map((id) => ({ id, label: freshNames.get(Number(id)) ?? `Group #${id}` }));
        groupPicker?.setSelected(selected);
        if (groupPicker) {
          membershipGroupCount.textContent = `${groupPicker.selectedIds().length} group${groupPicker.selectedIds().length === 1 ? "" : "s"} selected`;
        }
        baselineMembership = JSON.stringify([...fresh.membershipIds].sort((a, b) => a - b));
        baselineRules = normalizeRuleSignature(fresh.rules);
        rulesEditor = renderDirectRulesEditor({ shares: fresh.shares, rules: fresh.rules, onChange: () => markDirty() });
        rulesHost.replaceChildren(rulesEditor.el);
        savedSignature = signature();
      }
      await effectivePanel.reload();
    }

    // ------------------------------------------------------------------- save FSM
    workflow = createSaveWorkflow<Awaited<ReturnType<typeof saveUser>>>(
      {
        save: async () => saveUser(),
        onSaved: () => {
          savedSignature = signature();
          baselineMembership = JSON.stringify((groupPicker ? groupPicker.selectedIds() : []).sort((a, b) => a - b));
          baselineRules = normalizeRuleSignature(rulesEditor.collect());
          void refreshAdminCount();
          void load();
          window.setTimeout(() => close(), 700);
        },
        onStateChange: () => {
          queueMicrotask(() => markDirty());
        },
      },
    );
    const activeWorkflow = workflow;

    async function saveUser(): Promise<User> {
      const invalid = validationMessage();
      if (invalid) throw new Error(invalid);
      const profile = readProfile();
      const warnings: string[] = [];
      await ctx.api.patch<User>(`/api/users/${user.id}`, {
        displayName: profile.displayName,
        role: profile.role,
        active: profile.active,
        quotaBytes: profile.quotaBytes,
        mustChangePassword: profile.mustChangePassword,
      });
      const membershipIdsNow = groupPicker ? groupPicker.selectedIds() : [];
      const membershipNow = JSON.stringify(membershipIdsNow.slice().sort((a, b) => a - b));
      if (membershipNow !== baselineMembership) {
        try {
          await ctx.api.put(`/api/users/${user.id}/groups`, { groupIds: membershipIdsNow });
          baselineMembership = membershipNow;
        } catch (error) {
          if (!optionalAccessError(error)) throw error;
          warnings.push(`Group membership was not saved: ${apiErrorMessage(error)}`);
        }
      }
      const rulesNow = normalizeRuleSignature(rulesEditor.collect());
      if (rulesNow !== baselineRules) {
        try {
          await ctx.api.put<AccessRules>(`/api/users/${user.id}/access-rules`, rulesEditor.collect());
          baselineRules = rulesNow;
        } catch (error) {
          if (!optionalAccessError(error)) throw error;
          warnings.push(`Direct rules were not saved: ${apiErrorMessage(error)}`);
        }
      }
      if (warnings.length > 0) ctx.toast.warning(warnings.join(" · "));
      if (isSelf) {
        try {
          await ctx.refreshUser();
        } catch {
          /* the live access.changed event also refreshes the shell */
        }
      }
      return { ...user, ...profile } as User;
    }

    // ------------------------------------------------------------------ layout
    const identity = h(
      "div",
      { class: "identity identity-compact" },
      h("span", { class: "avatar avatar-sm" }, (user.displayName || user.username).slice(0, 1).toUpperCase()),
      h(
        "div",
        { class: "identity-text" },
        h("div", { class: "identity-name" }, user.displayName || user.username),
        h(
          "div",
          { class: "identity-meta small muted" },
          `@${user.username} · created ${formatDateTime(user.createdAt)} · last sign-in ${relativeTime(user.lastLoginAt)}`,
        ),
      ),
    );

    const bodyHost = h(
      "div",
      { class: "col user-editor" },
      availability,
      conflictHost,
      identity,
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Account")),
        h("div", { class: "form-grid-2" }, displayField.el, roleField.el),
        h("div", { class: "form-grid-2" }, quotaField.el, h("div", { class: "field" }, h("span", { class: "field-label" }, "Options"), activeSwitch.el, forceChange.el)),
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h(
          "div",
          { class: "appearance-section-head" },
          h("span", { class: "appearance-section-title" }, "Group Membership"),
          membershipGroupCount,
        ),
        h(
          "p",
          { class: "section-note muted" },
          "Groups grant shared-folder and application access to their members. Changing membership affects everything the groups below allow.",
        ),
        groupPicker?.el ?? null,
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Direct Overrides")),
        rulesHint,
        h("div", { class: "rules-host" }, rulesHost),
      ),
      h(
        "div",
        { class: "appearance-section group-section" },
        h("div", { class: "appearance-section-head" }, h("span", { class: "appearance-section-title" }, "Effective Access")),
        h(
          "p",
          { class: "section-note muted" },
          "Final result computed by the server, combining group rules, direct overrides, legacy membership and role defaults. Read-only here.",
        ),
        effectiveHost,
      ),
      activeWorkflow.el,
    );

    const cancelButton = h("button", { class: "btn btn-ghost", type: "button" }, "Cancel");
    cancelButton.addEventListener("click", () => close());
    activeWorkflow.el.querySelector(".appearance-actions-leading")?.append(cancelButton);

    const close = modal({
      title: `Edit ${user.username}`,
      body: bodyHost,
      wide: true,
      onClose: () => {
        closing = true;
        openAccessEditor = null;
        activeWorkflow.dispose();
        groupPicker?.dispose();
        effectivePanel.dispose();
      },
    });

    openAccessEditor = {
      userId: user.id,
      isDirty: () => activeWorkflow.isDirty(),
      busy: () => activeWorkflow.state === "saving" || activeWorkflow.state === "saved",
      notifyExternalChange: (message?: string) => {
        if (activeWorkflow.state === "saving" || activeWorkflow.state === "saved") return;
        showConflict(message ?? "This account changed in another session.");
      },
      refreshAccess,
      reload: reloadUserAndReopen,
    };
    activeWorkflow.setDirty(false);
    window.setTimeout(() => displayInput.focus(), 40);
  }

  function openResetPasswordModal(user: User): void {
    const password = passwordField({
      label: `New password for ${user.username}`,
      autocomplete: "new-password",
      required: true,
      showStrength: true,
    });
    const forceChange = switchControl({ label: "Require password change at next sign-in", checked: true });
    const alert = h("div", { class: "banner banner-error", hidden: true, role: "alert" });
    modal({
      title: `Reset password — ${user.username}`,
      body: h(
        "div",
        { class: "col" },
        h("p", { class: "muted small" }, "The new password is not shown again. Share it through a trusted channel."),
        password.el,
        forceChange.el,
        alert,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Reset password",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            alert.hidden = true;
            const policyError = validatePassword(password.value());
            if (policyError) {
              password.setError(policyError);
              return;
            }
            try {
              await ctx.api.post(`/api/users/${user.id}/password`, {
                password: password.value(),
                mustChangePassword: forceChange.isChecked(),
              });
              ctx.toast.success(`Password reset for ${user.username}`);
              close();
            } catch (error) {
              alert.hidden = false;
              alert.textContent = apiErrorMessage(error);
              throw error;
            }
          },
        },
      ],
    });
  }

  function openSessionsModal(user: User): void {
    const hostEl = h("div", { class: "modal-loading" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading sessions…"));
    const refresh = async (): Promise<void> => {
      hostEl.replaceChildren(h("div", { class: "modal-loading" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading sessions…")));
      try {
        const response = await ctx.api.get<UserSessionList | SessionInfo[]>(`/api/users/${user.id}/sessions`);
        const sessions = Array.isArray(response) ? response : response.items;
        hostEl.replaceChildren();
        if (sessions.length === 0) {
          hostEl.appendChild(h("div", { class: "table-empty" }, "No active sessions for this account."));
          return;
        }
        const list = h("div", { class: "session-list" });
        for (const session of sessions) {
          list.appendChild(
            h(
              "div",
              { class: "session-row" },
              h("span", { class: "session-icon" }, iconSvg("monitor", 18)),
              h(
                "div",
                { class: "session-main" },
                h("div", { class: "session-title" }, describeUserAgent(session.userAgent)),
                h(
                  "div",
                  { class: "session-meta small muted" },
                  `${session.ip || "unknown IP"} · last active ${relativeTime(session.lastSeenAt)} · expires ${formatDateTime(session.expiresAt)}`,
                ),
              ),
            ),
          );
        }
        hostEl.appendChild(list);
      } catch (error) {
        hostEl.replaceChildren(
          h(
            "div",
            { class: "error-state inline-state" },
            h("div", { class: "error-title" }, "Could not load sessions"),
            h("div", { class: "error-detail" }, apiErrorMessage(error)),
            h("button", { class: "btn btn-sm", type: "button", onClick: () => void refresh() }, "Retry"),
          ),
        );
      }
    };
    void refresh();
    modal({
      title: `Active sessions — ${user.username}`,
      body: hostEl,
      actions: [
        {
          label: "Revoke all sessions",
          variant: "danger",
          keepOpen: true,
          onClick: async () => {
            const ok = await confirmDialog({
              title: "Revoke all sessions",
              message: `Sign out every active session for ${user.username}? They will need to sign in again.`,
              confirmLabel: "Revoke all",
              danger: true,
            });
            if (!ok) return;
            try {
              await ctx.api.del(`/api/users/${user.id}/sessions`);
              ctx.toast.success("Sessions revoked");
              await refresh();
            } catch (error) {
              ctx.toast.error(apiErrorMessage(error));
            }
          },
        },
        { label: "Close", variant: "ghost", onClick: () => {} },
      ],
    });
  }

  async function deactivateUser(user: User): Promise<void> {
    const ok = await confirmDialog({
      title: "Disable account",
      message: `Disable ${user.username}? Their files are preserved, their sessions are revoked and they can no longer sign in. You can re-enable the account later.`,
      confirmLabel: "Disable account",
      danger: true,
    });
    if (!ok) return;
    try {
      await ctx.api.del(`/api/users/${user.id}`);
      ctx.toast.success(`${user.username} disabled`);
      await refreshAdminCount();
      void load();
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  async function enableUser(user: User): Promise<void> {
    try {
      await ctx.api.patch(`/api/users/${user.id}`, { active: true });
      ctx.toast.success(`${user.username} enabled`);
      void load();
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  const realtime = getRealtime(ctx.events);
  const reloadUsers = debounceTask(() => {
    void refreshAdminCount();
    void load();
  }, 350);
  const unsubscribeUsers = realtime.on<{ userId: number; event: string }>("user.changed", (data) => {
    reloadUsers.run();
    if (data && openAccessEditor && !openAccessEditor.busy() && data.userId === openAccessEditor.userId) {
      openAccessEditor.notifyExternalChange("This account was changed in another session.");
    }
  });
  const unsubscribeGroups = realtime.on<{ groupId?: number }>("group.changed", () => {
    reloadUsers.run();
    if (!openAccessEditor || openAccessEditor.busy()) return;
    if (openAccessEditor.isDirty()) {
      openAccessEditor.notifyExternalChange("A group or access rule changed in another session.");
    } else {
      void openAccessEditor.refreshAccess();
    }
  });
  const unsubscribeResync = realtime.onResync(() => {
    reloadUsers.run();
    if (openAccessEditor && !openAccessEditor.busy() && !openAccessEditor.isDirty()) void openAccessEditor.refreshAccess();
  });

  await refreshAdminCount();
  await load();

  return () => {
    reloadUsers.cancel();
    unsubscribeUsers();
    unsubscribeGroups();
    unsubscribeResync();
    openAccessEditor = null;
    loadToken += 1;
  };
}
