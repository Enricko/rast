// Control Panel → Users: paginated account administration with role, status,
// quota, password reset and session management.

import { apiErrorMessage } from "../../core/api";
import { pager, switchControl } from "../../core/components";
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
import type { Page, SessionInfo, SystemInfo, User, UserSessionList } from "../../core/types";
import { confirmDialog, contextMenu, modal } from "../../core/ui";
import { activeBadge, describeUserAgent, lastLoginCell, quotaCell, roleBadge, type TabRenderContext } from "./common";

const PER_PAGE_OPTIONS = [10, 25, 50, 100];
const DEFAULT_PER_PAGE = 25;

export async function renderUsersTab(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  const viewer = ctx.store.get().user;
  if (!viewer) throw new Error("not authenticated");
  const viewerId = viewer.id;
  let page = 1;
  let perPage = DEFAULT_PER_PAGE;
  let query = "";
  let adminCount: number | null = null;
  let loadToken = 0;
  let openEditUserId: number | null = null;
  let editConflictHost: HTMLElement | null = null;

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
            { colspan: 6 },
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
            { colspan: 6 },
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
    const isSelf = user.id === viewerId;
    const isLastAdmin = adminCount === 1 && user.role === "admin" && user.active;
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
    if (isSelf || isLastAdmin) {
      role.disabled = true;
      roleField.hintEl?.remove();
      roleField.el.appendChild(
        h(
          "span",
          { class: "field-hint" },
          isSelf
            ? "You cannot change your own role."
            : "This is the only active administrator; the role is locked.",
        ),
      );
    }
    const activeSwitch = switchControl({ label: "Account is active", checked: user.active });
    if (isSelf || isLastAdmin) {
      activeSwitch.input.disabled = true;
      if (isSelf) activeSwitch.el.title = "You cannot deactivate your own account.";
      if (isLastAdmin) activeSwitch.el.title = "The last active administrator cannot be deactivated.";
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
    const alert = h("div", { class: "banner banner-error", hidden: true, role: "alert" });
    const conflictHost = h(
      "div",
      { class: "banner banner-warn", hidden: true, role: "status" },
      h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
      h(
        "div",
        { class: "banner-body" },
        h("div", { class: "banner-title" }, "This account changed in another session"),
        h(
          "div",
          { class: "banner-message" },
          "Reload the fields to review the latest role, status and quota before saving.",
        ),
        h(
          "button",
          {
            class: "btn btn-sm",
            type: "button",
            onClick: () => void reloadEditedUser(user),
          },
          "Reload account",
        ),
      ),
    );
    const warning = (isSelf || isLastAdmin)
      ? h(
          "div",
          { class: "banner banner-warn" },
          h("span", { class: "banner-icon" }, iconSvg("shieldAlert", 17)),
          h(
            "div",
            { class: "banner-body" },
            h(
              "div",
              { class: "banner-message" },
              isSelf
                ? "You cannot deactivate or demote your own account. Use another administrator account for that."
                : "This is the only active administrator account. RAST will not allow it to be demoted or disabled.",
            ),
          ),
        )
      : null;

    modal({
      title: `Edit ${user.username}`,
      body: h(
        "div",
        { class: "col" },
        conflictHost,
        warning ?? null,
        displayField.el,
        roleField.el,
        activeSwitch.el,
        quotaField.el,
        forceChange.el,
        alert,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Save changes",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            alert.hidden = true;
            displayField.setError(null);
            quotaField.setError(null);
            const displayError = validateRequiredText(displayInput.value, "Display name", 64);
            if (displayError) {
              displayField.setError(displayError);
              return;
            }
            const quotaError = validateGib(quota.value);
            if (quotaError) {
              quotaField.setError(quotaError);
              return;
            }
            const nextRole = role.disabled ? user.role : (role.value as User["role"]);
            const nextActive = activeSwitch.isChecked();
            if (isLastAdmin && (nextRole !== "admin" || !nextActive)) {
              alert.hidden = false;
              alert.textContent = "The last active administrator cannot be demoted or disabled.";
              throw new Error("invalid last admin change");
            }
            try {
              await ctx.api.patch(`/api/users/${user.id}`, {
                displayName: displayInput.value.trim(),
                role: nextRole,
                active: nextActive,
                quotaBytes: gibToBytes(parseGib(quota.value)),
                mustChangePassword: forceChange.isChecked(),
              });
              ctx.toast.success("User updated");
              close();
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
      onClose: () => {
        openEditUserId = null;
        editConflictHost = null;
      },
    });
    openEditUserId = user.id;
    editConflictHost = conflictHost;
    window.setTimeout(() => displayInput.focus(), 40);
  }

  async function reloadEditedUser(user: User): Promise<void> {
    try {
      const response = await ctx.api.get<Page<User>>("/api/users", { page: 1, perPage: 100, q: user.username });
      const fresh = response.items.find((candidate) => candidate.id === user.id);
      if (!fresh) {
        ctx.toast.warning("This account no longer exists.");
        return;
      }
      const dialog = editConflictHost?.closest(".modal");
      const display = dialog?.querySelector<HTMLInputElement>('[data-edit-field="display"]');
      const quotaInput = dialog?.querySelector<HTMLInputElement>('[data-edit-field="quota"]');
      if (display) display.value = fresh.displayName;
      if (quotaInput) quotaInput.value = String(bytesToGib(fresh.quotaBytes));
      const roleSelect = dialog?.querySelector<HTMLSelectElement>(".modal-body select");
      if (roleSelect && !roleSelect.disabled) roleSelect.value = fresh.role;
      const switches = dialog?.querySelectorAll<HTMLInputElement>(".switch-input");
      if (switches && switches[0] && !switches[0].disabled) switches[0].checked = fresh.active;
      if (switches && switches[1]) switches[1].checked = fresh.mustChangePassword;
      if (editConflictHost) editConflictHost.hidden = true;
      ctx.toast.success("Account reloaded from the server");
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
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
    if (data && openEditUserId !== null && data.userId === openEditUserId && editConflictHost) {
      editConflictHost.hidden = false;
    }
  });
  const unsubscribeResync = realtime.onResync(() => reloadUsers.run());

  await refreshAdminCount();
  await load();

  return () => {
    reloadUsers.cancel();
    unsubscribeUsers();
    unsubscribeResync();
    loadToken += 1;
  };
}
