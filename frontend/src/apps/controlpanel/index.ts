// Control Panel: profile, users, storage and component status.
// OWNER: teammate `frontend-shell` (expand into full control panel).

import type { AppDefinition, AppContext } from "../../core/app";
import { h, clear } from "../../core/dom";
import { formatBytes, formatDateTime, relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { ComponentHealth, MeResponse, Settings, SystemInfo, User } from "../../core/types";
import { confirmDialog, errorMessage, modal, promptDialog } from "../../core/ui";

const TABS = [
  { id: "profile", label: "Profile" },
  { id: "users", label: "Users" },
  { id: "storage", label: "Storage" },
  { id: "components", label: "Components" },
] as const;

type TabId = (typeof TABS)[number]["id"];

export const controlPanelApp: AppDefinition = {
  id: "controlpanel",
  name: "Control Panel",
  icon: "control",
  description: "Profile, users, quotas, storage and system health",
  singleton: true,
  defaultSize: { w: 980, h: 640 },
  mount(ctx) {
    return mountControlPanel(ctx);
  },
};

async function mountControlPanel(ctx: AppContext) {
  const params = (ctx.params ?? {}) as { tab?: TabId };
  const isAdmin = ctx.store.isAdmin();
  const tabs = TABS.filter((tab) => tab.id === "profile" || tab.id === "components" || isAdmin);
  let active: TabId = params.tab ?? "profile";

  const tabBar = h("div", { class: "tabs" });
  const body = h("div", { class: "app-content", style: "padding:16px" });
  ctx.root.append(tabBar, body);

  const renderTabs = () => {
    clear(tabBar);
    for (const tab of tabs) {
      const btn = h("button", { class: `tab${active === tab.id ? " is-active" : ""}`, type: "button" }, tab.label);
      btn.addEventListener("click", () => {
        active = tab.id;
        renderTabs();
        void renderBody();
      });
      tabBar.appendChild(btn);
    }
  };

  const renderBody = async () => {
    body.replaceChildren(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading…"));
    try {
      if (active === "profile") await renderProfile(ctx, body);
      else if (active === "users") await renderUsers(ctx, body);
      else if (active === "storage") await renderStorage(ctx, body);
      else await renderComponents(ctx, body);
    } catch (error) {
      body.replaceChildren(
        h("div", { class: "error-state" }, h("div", { class: "error-title" }, "Failed to load"), h("div", { class: "error-detail" }, errorMessage(error))),
      );
    }
  };

  renderTabs();
  await renderBody();
}

async function renderProfile(ctx: AppContext, body: HTMLElement): Promise<void> {
  const me = await ctx.api.get<MeResponse>("/api/auth/me");
  const user = me.user;
  const displayName = h("input", { class: "input", type: "text", value: user.displayName });
  const current = h("input", { class: "input", type: "password", autocomplete: "current-password" });
  const next = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const confirm = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const profileMsg = h("div", { class: "small muted" });

  const saveProfile = h("button", { class: "btn btn-primary", type: "button" }, "Save profile");
  saveProfile.addEventListener("click", async () => {
    saveProfile.disabled = true;
    try {
      await ctx.api.patch(`/api/users/${user.id}`, { displayName: displayName.value.trim() });
      profileMsg.textContent = "Profile updated";
      await ctx.refreshUser();
      ctx.toast.success("Profile updated");
    } catch (error) {
      profileMsg.textContent = errorMessage(error);
      ctx.toast.error(errorMessage(error));
    } finally {
      saveProfile.disabled = false;
    }
  });

  const changePassword = h("button", { class: "btn btn-primary", type: "button" }, "Change password");
  const pwMsg = h("div", { class: "field-hint" });
  changePassword.addEventListener("click", async () => {
    pwMsg.textContent = "";
    if (next.value !== confirm.value) {
      pwMsg.textContent = "Passwords do not match";
      return;
    }
    changePassword.disabled = true;
    try {
      await ctx.api.post("/api/auth/password", { currentPassword: current.value, newPassword: next.value });
      ctx.toast.success("Password changed; other sessions were signed out");
      current.value = next.value = confirm.value = "";
      pwMsg.textContent = "Password updated";
    } catch (error) {
      pwMsg.textContent = errorMessage(error);
    } finally {
      changePassword.disabled = false;
    }
  });

  body.replaceChildren(
    section(
      "Account",
      h(
        "dl",
        { class: "kv" },
        h("dt", {}, "Username"),
        h("dd", {}, user.username),
        h("dt", {}, "Role"),
        h("dd", {}, user.role === "admin" ? "Administrator" : "User"),
        h("dt", {}, "Storage quota"),
        h("dd", {}, user.quotaBytes === 0 ? "Unlimited" : formatBytes(user.quotaBytes)),
        h("dt", {}, "Used"),
        h("dd", {}, formatBytes(user.usedBytes)),
        h("dt", {}, "Last sign-in"),
        h("dd", {}, relativeTime(user.lastLoginAt)),
      ),
    ),
    section(
      "Display name",
      h("label", { class: "field" }, displayName),
      h("div", { class: "row" }, saveProfile, profileMsg),
    ),
    section(
      "Change password",
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Current password"), current),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "New password"), next),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Confirm new password"), confirm),
      h("div", { class: "row" }, changePassword, pwMsg),
    ),
    section(
      "Appearance",
      h(
        "div",
        { class: "row" },
        h("button", { class: "btn", type: "button", onClick: () => ctx.store.toggleTheme() }, "Toggle dark / light mode"),
      ),
    ),
  );
}

async function renderUsers(ctx: AppContext, body: HTMLElement): Promise<void> {
  const page = await ctx.api.get<{ items: User[]; total: number }>("/api/users", { page: 1, perPage: 100 });
  const rows = page.items.map((user) =>
    h(
      "tr",
      {},
      h("td", {}, h("div", { class: "row" }, h("div", { class: "avatar", style: "width:24px;height:24px;font-size:10px" }, user.username.slice(0, 1).toUpperCase()), h("span", {}, user.username))),
      h("td", {}, user.displayName),
      h("td", {}, h("span", { class: `badge ${user.role === "admin" ? "info" : ""}` }, user.role)),
      h("td", {}, user.active ? h("span", { class: "badge ok" }, "active") : h("span", { class: "badge" }, "disabled")),
      h("td", { class: "num" }, user.quotaBytes === 0 ? "∞" : formatBytes(user.quotaBytes)),
      h("td", { class: "num" }, formatBytes(user.usedBytes)),
      h("td", {}, relativeTime(user.lastLoginAt)),
      h(
        "td",
        {},
        h(
          "div",
          { class: "row" },
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void editUser(ctx, user) }, "Edit"),
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void resetUserPassword(ctx, user) }, "Password"),
          user.active
            ? h("button", { class: "btn btn-sm btn-danger", type: "button", onClick: () => void deactivateUser(ctx, user) }, "Disable")
            : null,
        ),
      ),
    ),
  );

  const addButton = h("button", { class: "btn btn-primary", type: "button" }, "Add user");
  addButton.addEventListener("click", () => void createUser(ctx));

  body.replaceChildren(
    h(
      "div",
      { class: "spread", style: "margin-bottom:12px" },
      h("div", {}, h("h3", { style: "margin:0" }, "Users"), h("div", { class: "small muted" }, `${page.total} account(s)`)),
      h("div", { class: "row" }, addButton),
    ),
    h(
      "div",
      { class: "table-wrap" },
      h(
        "table",
        { class: "table" },
        h("thead", {}, h("tr", {}, h("th", {}, "Username"), h("th", {}, "Display name"), h("th", {}, "Role"), h("th", {}, "Status"), h("th", { class: "num" }, "Quota"), h("th", { class: "num" }, "Used"), h("th", {}, "Last sign-in"), h("th", {}, "Actions"))),
        h("tbody", {}, ...rows),
      ),
    ),
  );
}

async function createUser(ctx: AppContext): Promise<void> {
  const username = h("input", { class: "input", type: "text" });
  const displayName = h("input", { class: "input", type: "text" });
  const password = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const role = h("select", { class: "select" }, h("option", { value: "user", selected: true }, "User"), h("option", { value: "admin" }, "Administrator"));
  const quotaGb = h("input", { class: "input", type: "number", value: "20", min: "0", step: "1" });
  const error = h("div", { class: "field-hint", style: "color:var(--danger)" });

  const close = modal({
    title: "Add user",
    body: h(
      "div",
      {},
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Username"), username),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Display name"), displayName),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Password"), password, h("span", { class: "field-hint" }, "At least 10 characters with mixed character classes.")),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Role"), role),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Quota (GiB, 0 = unlimited)"), quotaGb),
      error,
    ),
    actions: [
      { label: "Cancel", variant: "ghost", onClick: () => {} },
      {
        label: "Create user",
        variant: "primary",
        onClick: async (done) => {
          error.textContent = "";
          try {
            await ctx.api.post("/api/users", {
              username: username.value.trim(),
              displayName: displayName.value.trim() || undefined,
              password: password.value,
              role: role.value,
              quotaBytes: Math.round(Number(quotaGb.value || 0) * 1024 * 1024 * 1024),
            });
            ctx.toast.success("User created");
            done();
            const tab = document.querySelector<HTMLButtonElement>(".tab.is-active");
            tab?.click();
          } catch (err) {
            error.textContent = errorMessage(err);
            throw err;
          }
        },
      },
    ],
  });
  void close;
  username.focus();
}

async function editUser(ctx: AppContext, user: User): Promise<void> {
  const displayName = h("input", { class: "input", type: "text", value: user.displayName });
  const role = h(
    "select",
    { class: "select" },
    h("option", { value: "user", selected: user.role === "user" }, "User"),
    h("option", { value: "admin", selected: user.role === "admin" }, "Administrator"),
  );
  const active = h("input", { type: "checkbox", checked: user.active });
  const quotaGb = h("input", { class: "input", type: "number", value: String(Math.round((user.quotaBytes / (1024 * 1024 * 1024)) * 10) / 10), min: "0", step: "1" });
  const error = h("div", { class: "field-hint", style: "color:var(--danger)" });

  modal({
    title: `Edit ${user.username}`,
    body: h(
      "div",
      {},
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Display name"), displayName),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Role"), role),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Quota (GiB, 0 = unlimited)"), quotaGb),
      h("label", { class: "checkbox", style: "margin-top:8px" }, active, h("span", {}, "Account active")),
      error,
    ),
    actions: [
      { label: "Cancel", variant: "ghost", onClick: () => {} },
      {
        label: "Save",
        variant: "primary",
        onClick: async (done) => {
          try {
            await ctx.api.patch(`/api/users/${user.id}`, {
              displayName: displayName.value.trim(),
              role: role.value,
              active: active.checked,
              quotaBytes: Math.round(Number(quotaGb.value || 0) * 1024 * 1024 * 1024),
            });
            ctx.toast.success("User updated");
            done();
            const tab = document.querySelector<HTMLButtonElement>(".tab.is-active");
            tab?.click();
          } catch (err) {
            error.textContent = errorMessage(err);
            throw err;
          }
        },
      },
    ],
  });
}

async function resetUserPassword(ctx: AppContext, user: User): Promise<void> {
  const password = await promptDialog({
    title: `Reset password for ${user.username}`,
    label: "New password",
    type: "password",
  });
  if (!password) return;
  try {
    await ctx.api.post(`/api/users/${user.id}/password`, { password, mustChangePassword: true });
    ctx.toast.success("Password reset; user must change it at next sign-in");
  } catch (error) {
    ctx.toast.error(errorMessage(error));
  }
}

async function deactivateUser(ctx: AppContext, user: User): Promise<void> {
  const ok = await confirmDialog({
    title: "Disable account",
    message: `Disable ${user.username}? Their files are preserved and they will be signed out immediately.`,
    confirmLabel: "Disable account",
    danger: true,
  });
  if (!ok) return;
  try {
    await ctx.api.del(`/api/users/${user.id}`);
    ctx.toast.success("Account disabled");
    const tab = document.querySelector<HTMLButtonElement>(".tab.is-active");
    tab?.click();
  } catch (error) {
    ctx.toast.error(errorMessage(error));
  }
}

async function renderStorage(ctx: AppContext, body: HTMLElement): Promise<void> {
  const [info, storage] = await Promise.all([
    ctx.api.get<SystemInfo>("/api/system/info"),
    ctx.api.get<Record<string, unknown>>("/api/settings/storage"),
  ]);
  const disk = info.storage;
  const usedPercent = disk.available && disk.totalBytes ? Math.round(((disk.usedBytes ?? 0) / disk.totalBytes) * 100) : 0;
  body.replaceChildren(
    section(
      "Storage locations",
      h(
        "dl",
        { class: "kv" },
        h("dt", {}, "Storage directory"),
        h("dd", { class: "mono" }, String(storage.storageDir ?? info.server.storageDir ?? "—")),
        h("dt", {}, "User homes"),
        h("dd", { class: "mono" }, String(storage.homesDir ?? "—")),
        h("dt", {}, "Shared folders"),
        h("dd", { class: "mono" }, String(storage.sharedDir ?? "—")),
        h("dt", {}, "Recycle bin"),
        h("dd", { class: "mono" }, String(storage.trashDir ?? "—")),
      ),
    ),
    section(
      "Filesystem",
      disk.available
        ? h(
            "div",
            { class: "col" },
            h("div", { class: "progress" }, h("div", { class: "progress-bar", style: `width:${usedPercent}%` })),
            h("div", { class: "small muted" }, `${formatBytes(disk.usedBytes)} of ${formatBytes(disk.totalBytes)} used · ${formatBytes(disk.availableBytes)} free · ${disk.mountPoint ?? ""}`),
          )
        : h("div", { class: "muted" }, `Unavailable: ${disk.reason ?? "unknown"}`),
      h("div", { class: "small muted", style: "margin-top:8px" }, `Files stored across all homes: ${formatBytes(Number(storage.usedBytes ?? 0))}`),
    ),
    section(
      "Quotas",
      h("div", { class: "muted" }, "Per-user quotas are enforced by RAST when uploading, copying and restoring files."),
    ),
  );
}

async function renderComponents(ctx: AppContext, body: HTMLElement): Promise<void> {
  const [components, settings, info] = await Promise.all([
    ctx.api.get<{ components: ComponentHealth[] }>("/api/system/components"),
    ctx.store.isAdmin() ? ctx.api.get<Settings>("/api/settings").catch(() => null) : Promise.resolve(null),
    ctx.api.get<SystemInfo>("/api/system/info"),
  ]);
  const sections: Array<Node | null> = [
    section(
      "System",
      h(
        "dl",
        { class: "kv" },
        h("dt", {}, "Hostname"),
        h("dd", {}, info.host.hostname),
        h("dt", {}, "Operating system"),
        h("dd", {}, info.host.os),
        h("dt", {}, "Kernel"),
        h("dd", {}, info.host.kernel),
        h("dt", {}, "Architecture"),
        h("dd", {}, info.host.arch),
        h("dt", {}, "CPU cores"),
        h("dd", {}, String(info.host.cpuCount)),
        h("dt", {}, "RAST version"),
        h("dd", {}, info.version),
        h("dt", {}, "Uptime"),
        h("dd", {}, new Date(Date.now() - info.host.uptimeSecs * 1000).toLocaleString()),
      ),
    ),
    section(
      "Components",
      h(
        "div",
        { class: "col" },
        ...components.components.map((component) =>
          h(
            "div",
            { class: "spread", style: "padding:8px 0;border-bottom:1px solid var(--border)" },
            h("div", {}, h("div", { style: "font-weight:600" }, component.name), h("div", { class: "small muted" }, component.detail)),
            h("span", { class: `badge ${component.status === "ok" ? "ok" : component.status === "warning" ? "warn" : component.status === "disabled" ? "" : "error"}` }, component.status),
          ),
        ),
      ),
    ),
    settings
      ? section(
          "Settings",
          h(
            "dl",
            { class: "kv" },
            h("dt", {}, "Site name"),
            h("dd", {}, settings.siteName),
            h("dt", {}, "Audit retention"),
            h("dd", {}, `${settings.auditRetentionDays} days`),
            h("dt", {}, "Trash retention"),
            h("dd", {}, `${settings.trashRetentionDays} days`),
            h("dt", {}, "Default quota"),
            h("dd", {}, settings.defaultQuotaBytes === 0 ? "Unlimited" : formatBytes(settings.defaultQuotaBytes)),
            h("dt", {}, "Listen address"),
            h("dd", { class: "mono" }, settings.config.bind),
            h("dt", {}, "Public URL"),
            h("dd", { class: "mono" }, settings.config.publicUrl ?? "not configured"),
          ),
        )
      : null,
  ];
  body.replaceChildren(...sections.filter((node): node is Node => node !== null));
}

function section(title: string, ...children: Array<Node | null>): HTMLElement {
  return h(
    "section",
    { style: "margin-bottom:18px;padding:14px 16px;border:1px solid var(--border);border-radius:var(--radius);background:var(--surface-2)" },
    h("h3", { style: "font-size:13.5px;text-transform:uppercase;letter-spacing:.06em;color:var(--text-dim)" }, title),
    ...children.filter((child): child is Node => child !== null),
  );
}

export { iconSvg, formatDateTime };
