// Desktop shell: boot flow, login/setup screens, desktop, taskbar and menus.

import { appsForRole, findApp } from "../apps/registry";
import type { AppContext, AppDefinition } from "../core/app";
import { ApiClient, ApiError } from "../core/api";
import { clear, h } from "../core/dom";
import { formatClock, formatDate } from "../core/format";
import { iconSvg } from "../core/icons";
import { EventStream } from "../core/sse";
import { Store } from "../core/store";
import { ToastCenter, type ToastOptions } from "../core/toast";
import type { Bootstrap, MeResponse, User } from "../core/types";
import { modal, errorMessage, promptDialog } from "../core/ui";
import { WindowManager, type WindowHandle } from "../core/wm";

interface BootDeps {
  api: ApiClient;
  store: Store;
  events: EventStream;
}

export async function boot(root: HTMLElement): Promise<void> {
  const api = new ApiClient();
  const store = new Store();
  const events = new EventStream();

  let bootstrap: Bootstrap;
  try {
    bootstrap = await api.get<Bootstrap>("/api/bootstrap");
  } catch (error) {
    renderFatal(root, "Cannot reach the RAST server", errorMessage(error));
    return;
  }
  store.patch({
    siteName: bootstrap.product,
    version: bootstrap.version,
    maxUploadBytes: bootstrap.maxUploadBytes,
  });
  api.onSessionLost(() => {
    if (document.querySelector(".desktop")) {
      events.stop();
      showLogin(root, { api, store, events }, "Your session expired. Please sign in again.");
    }
  });

  if (bootstrap.setupRequired) {
    renderSetup(root, { api, store, events }, bootstrap);
    return;
  }

  try {
    const me = await api.get<MeResponse>("/api/auth/me", undefined, { allowUnauthorized: true });
    startDesktop(root, { api, store, events }, me);
  } catch (error) {
    if (error instanceof ApiError && (error.status === 401 || error.status === 403)) {
      showLogin(root, { api, store, events });
    } else {
      renderFatal(root, "RAST is not ready", errorMessage(error));
    }
  }
}

function renderFatal(root: HTMLElement, title: string, detail: string): void {
  clear(root);
  root.className = "auth-shell";
  root.appendChild(
    h(
      "div",
      { class: "auth-card" },
      brandBlock(),
      h("div", { class: "error-state" }, h("div", { class: "error-title" }, title), h("div", { class: "error-detail" }, detail)),
      h("div", { class: "auth-footer" }, h("span", {}, "RAST"), h("span", {}, "Resource Administration & Storage Toolkit")),
    ),
  );
}

function brandBlock(): HTMLElement {
  const mark = h("div", { class: "auth-logo-mark" });
  mark.appendChild(iconSvg("logo", 24));
  return h(
    "div",
    { class: "auth-logo" },
    mark,
    h(
      "div",
      {},
      h("div", { class: "auth-logo-title" }, "RAST"),
      h("div", { class: "auth-logo-sub" }, "Resource Administration & Storage Toolkit"),
    ),
  );
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

function showLogin(root: HTMLElement, deps: BootDeps, notice?: string): void {
  clear(root);
  root.className = "auth-shell";

  const username = h("input", {
    class: "input",
    type: "text",
    name: "username",
    autocomplete: "username",
    placeholder: "Username",
  });
  const password = h("input", {
    class: "input",
    type: "password",
    name: "password",
    autocomplete: "current-password",
    placeholder: "Password",
  });
  const error = h("div", { class: "auth-message", style: "display:none" });
  const submit = h("button", { class: "btn btn-primary", type: "submit", style: "width:100%;height:38px;margin-top:6px" }, "Sign in");

  const form = h(
    "form",
    {
      onSubmit: async (event) => {
        event.preventDefault();
        error.style.display = "none";
        submit.disabled = true;
        submit.textContent = "Signing in…";
        try {
          const response = await deps.api.post<MeResponse & { user: User }>("/api/auth/login", {
            username: username.value.trim(),
            password: password.value,
          });
          deps.api.setCsrfToken(response.csrfToken);
          deps.store.patch({
            user: response.user,
            features: response.features,
            csrfToken: response.csrfToken,
            sessionExpiresAt: response.sessionExpiresAt,
          });
          startDesktop(root, deps, response);
        } catch (err) {
          error.textContent = errorMessage(err);
          error.style.display = "block";
          password.value = "";
          password.focus();
        } finally {
          submit.disabled = false;
          submit.textContent = "Sign in";
        }
      },
      class: "col",
    },
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Username"), username),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Password"), password),
    error,
    submit,
  );

  const card = h(
    "div",
    { class: "auth-card" },
    brandBlock(),
    h("div", { class: "auth-title" }, "Welcome back"),
    h("div", { class: "auth-subtitle" }, "Sign in to manage your server"),
    notice ? h("div", { class: "auth-message" }, notice) : null,
    form,
    h(
      "div",
      { class: "auth-footer" },
      h("span", {}, `v${deps.store.get().version || "—"}`),
      h("span", {}, "No public registration"),
    ),
  );
  root.appendChild(card);
  username.focus();
}

// ---------------------------------------------------------------------------
// First-run setup
// ---------------------------------------------------------------------------

function renderSetup(root: HTMLElement, deps: BootDeps, bootstrap: Bootstrap): void {
  clear(root);
  root.className = "auth-shell";

  const token = h("input", {
    class: "input",
    type: "text",
    placeholder: "rast_setup_…",
    autocomplete: "off",
  });
  const username = h("input", { class: "input", type: "text", value: "admin", autocomplete: "username" });
  const displayName = h("input", { class: "input", type: "text", value: "Administrator" });
  const password = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const confirm = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const error = h("div", { class: "auth-message", style: "display:none" });
  const submit = h("button", { class: "btn btn-primary", type: "submit", style: "width:100%;height:38px;margin-top:6px" }, "Create administrator");

  const form = h(
    "form",
    {
      class: "col",
      onSubmit: async (event) => {
        event.preventDefault();
        error.style.display = "none";
        if (password.value !== confirm.value) {
          error.textContent = "Passwords do not match";
          error.style.display = "block";
          return;
        }
        submit.disabled = true;
        submit.textContent = "Creating…";
        try {
          await deps.api.post("/api/setup", {
            token: token.value.trim() || undefined,
            username: username.value.trim(),
            password: password.value,
            displayName: displayName.value.trim() || undefined,
          });
          showLogin(root, deps, "Administrator created. Sign in to continue.");
        } catch (err) {
          error.textContent = errorMessage(err);
          error.style.display = "block";
        } finally {
          submit.disabled = false;
          submit.textContent = "Create administrator";
        }
      },
    },
    h(
      "div",
      { class: "auth-message" },
      "No administrator exists yet. Run ",
      h("code", {}, "sudo -u rast rast setup-token"),
      " on the server and paste the token below.",
    ),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Setup token"), token),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Username"), username),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Display name"), displayName),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Password"), password, h("span", { class: "field-hint" }, "At least 10 characters, mixing letters, digits and symbols.")),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Confirm password"), confirm),
    error,
    submit,
  );

  root.appendChild(
    h(
      "div",
      { class: "auth-card" },
      brandBlock(),
      h("div", { class: "auth-title" }, "First-run setup"),
      h("div", { class: "auth-subtitle" }, `RAST v${bootstrap.version} is ready to be configured`),
      form,
    ),
  );
  token.focus();
}

// ---------------------------------------------------------------------------
// Desktop
// ---------------------------------------------------------------------------

function startDesktop(root: HTMLElement, deps: BootDeps, me: MeResponse): void {
  deps.api.setCsrfToken(me.csrfToken);
  deps.store.patch({
    user: me.user,
    features: me.features,
    csrfToken: me.csrfToken,
    sessionExpiresAt: me.sessionExpiresAt,
  });

  clear(root);
  root.className = "";
  const desktop = h("div", { class: "desktop" });
  const iconLayer = h("div", { class: "desktop-icons" });
  const windowsLayer = h("div", { class: "windows-layer" });
  const toast = new ToastCenter(desktop);
  const taskbar = h("div", { class: "taskbar" });

  let startMenu: HTMLElement | null = null;
  let userMenu: HTMLElement | null = null;

  const wm = new WindowManager(windowsLayer, {
    taskbarHeight: 58,
    onChange: () => renderTaskbar(),
  });

  function toastShow(message: string, options?: ToastOptions): void {
    toast.show(message, options);
  }

  function openApp(id: string, params?: unknown): void {
    const app = findApp(id);
    if (!app) {
      toastShow(`Unknown application: ${id}`, { kind: "error" });
      return;
    }
    if (app.adminOnly && !deps.store.isAdmin()) {
      toastShow(`${app.name} requires an administrator account`, { kind: "warning" });
      return;
    }
    closeMenus();
    const handle = wm.open(
      app,
      params,
      async ({ contentEl, window }) => {
        contentEl.replaceChildren();
        const appHost = h("div", { class: "app" });
        contentEl.appendChild(appHost);
        const busy = h("div", {
          class: "app-busy hidden",
          style:
            "position:absolute;inset:0;background:rgba(6,10,20,.55);backdrop-filter:blur(2px);display:grid;place-items:center;z-index:9",
        });
        busy.appendChild(h("span", { class: "spinner" }));
        contentEl.appendChild(busy);

        const ctx: AppContext = {
          api: deps.api,
          store: deps.store,
          events: deps.events,
          toast,
          window,
          root: appHost,
          params,
          setTitle: (title: string) => window.setTitle(title),
          refreshUser: async () => {
            const fresh = await deps.api.get<MeResponse>("/api/auth/me");
            deps.api.setCsrfToken(fresh.csrfToken);
            deps.store.patch({ user: fresh.user, features: fresh.features, csrfToken: fresh.csrfToken });
          },
          openApp,
          withBusy: async <T,>(promise: Promise<T>, label?: string): Promise<T> => {
            busy.classList.remove("hidden");
            if (label) busy.title = label;
            try {
              return await promise;
            } finally {
              busy.classList.add("hidden");
            }
          },
          onResize: (cb: () => void) => window.onResize(cb),
          onDispose: (cb: () => void) => {
            handle.onResize(cb);
            return () => undefined;
          },
        };
        await app.mount(ctx);
        window.onResize(() => {
          /* apps register their own resize handlers */
        });
      },
      { singleton: app.singleton },
    );
    if (app.singleton) handle.focus();
  }

  function closeMenus(): void {
    startMenu?.remove();
    startMenu = null;
    userMenu?.remove();
    userMenu = null;
  }

  function toggleStartMenu(): void {
    if (startMenu) {
      closeMenus();
      return;
    }
    closeMenus();
    const search = h("input", { class: "input menu-search", type: "text", placeholder: "Search applications…" });
    const grid = h("div", { class: "menu-grid" });
    const renderApps = (query: string) => {
      clear(grid);
      for (const app of appsForRole(deps.store.isAdmin())) {
        if (query && !app.name.toLowerCase().includes(query.toLowerCase())) continue;
        const item = h(
          "button",
          { class: "menu-app", type: "button", onClick: () => openApp(app.id) },
          h("span", { class: "menu-app-glyph" }, iconSvg(app.icon, 22)),
          h("span", {}, app.name),
        );
        grid.appendChild(item);
      }
      if (!grid.children.length) {
        grid.appendChild(h("div", { class: "muted small", style: "grid-column:1/-1;padding:12px" }, "No matching applications"));
      }
    };
    renderApps("");
    search.addEventListener("input", () => renderApps(search.value));
    search.addEventListener("keydown", (event) => {
      if (event.key === "Escape") closeMenus();
      if (event.key === "Enter") {
        const first = grid.querySelector<HTMLButtonElement>(".menu-app");
        first?.click();
      }
    });

    startMenu = h(
      "div",
      { class: "menu-panel menu-panel-start" },
      h("div", { class: "menu-section-title" }, "Applications"),
      search,
      grid,
      h("div", { class: "menu-sep" }),
      menuItem("lock", "Lock screen", () => {
        closeMenus();
        void logout();
      }),
    );
    desktop.appendChild(startMenu);
    search.focus();
  }

  function menuItem(icon: string, label: string, onClick: () => void, danger = false): HTMLElement {
    const btn = h("button", { class: `menu-item${danger ? " menu-item-danger" : ""}`, type: "button" }, iconSvg(icon, 16), h("span", {}, label));
    btn.addEventListener("click", onClick);
    return btn;
  }

  function toggleUserMenu(anchor: HTMLElement): void {
    if (userMenu) {
      closeMenus();
      return;
    }
    closeMenus();
    const user = deps.store.get().user;
    userMenu = h(
      "div",
      { class: "menu-panel menu-panel-user" },
      h(
        "div",
        { class: "row", style: "padding:6px 8px 10px" },
        h("div", { class: "avatar" }, (user?.displayName || user?.username || "?").slice(0, 1).toUpperCase()),
        h(
          "div",
          {},
          h("div", { style: "font-weight:650" }, user?.displayName || user?.username || "User"),
          h("div", { class: "small muted" }, `${user?.role === "admin" ? "Administrator" : "User"} · ${user?.username}`),
        ),
      ),
      h("div", { class: "menu-sep" }),
      menuItem("user", "Account settings", () => openApp("controlpanel", { tab: "profile" })),
      menuItem(deps.store.get().theme === "dark" ? "sun" : "moon", deps.store.get().theme === "dark" ? "Light mode" : "Dark mode", () => {
        deps.store.toggleTheme();
        closeMenus();
      }),
      h("div", { class: "menu-sep" }),
      menuItem("logout", "Sign out", () => {
        closeMenus();
        void logout();
      }, true),
    );
    desktop.appendChild(userMenu);
    const rect = anchor.getBoundingClientRect();
    userMenu.style.bottom = `${window.innerHeight - rect.top + 8}px`;
  }

  async function logout(): Promise<void> {
    try {
      await deps.api.post("/api/auth/logout");
    } catch {
      /* session may already be gone */
    }
    deps.events.stop();
    wm.closeAll();
    deps.store.patch({ user: null, csrfToken: "" });
    showLogin(root, deps, "You have been signed out.");
  }

  // ------------------------------------------------------------- taskbar
  const startButton = h("button", {
    class: "taskbar-start",
    type: "button",
    ariaLabel: "Open application menu",
    onClick: toggleStartMenu,
  }, iconSvg("logo", 22));

  const taskbarApps = h("div", { class: "taskbar-apps" });
  const clock = h("div", { class: "tray-clock" });
  const clockTime = h("div", { class: "tray-clock-time" });
  const clockDate = h("div", { class: "tray-clock-date" });
  clock.append(clockTime, clockDate);

  const themeButton = h("button", { class: "tray-button", type: "button", ariaLabel: "Toggle theme" }, iconSvg("sun", 18));
  themeButton.addEventListener("click", () => {
    deps.store.toggleTheme();
    themeButton.replaceChildren(iconSvg(deps.store.get().theme === "dark" ? "sun" : "moon", 18));
  });
  themeButton.replaceChildren(iconSvg(deps.store.get().theme === "dark" ? "sun" : "moon", 18));

  const user = deps.store.get().user;
  const userChip = h(
    "div",
    { class: "user-chip", role: "button", tabIndex: 0 },
    h("div", { class: "avatar" }, (user?.displayName || user?.username || "?").slice(0, 1).toUpperCase()),
    h(
      "div",
      {},
      h("div", { class: "user-chip-name" }, user?.displayName || user?.username || "User"),
      h("div", { class: "user-chip-role" }, user?.role === "admin" ? "Admin" : "User"),
    ),
  );
  userChip.addEventListener("click", () => toggleUserMenu(userChip));
  userChip.addEventListener("keydown", (event) => {
    if (event.key === "Enter") toggleUserMenu(userChip);
  });

  taskbar.append(startButton, taskbarApps, h("div", { class: "taskbar-tray" }, themeButton, clock, userChip));

  function renderTaskbar(): void {
    clear(taskbarApps);
    for (const info of wm.list()) {
      const app = findApp(info.appId);
      const btn = h(
        "button",
        {
          class: `taskbar-app${info.focused && !info.minimized ? " is-active" : ""}`,
          type: "button",
          title: info.title,
          onClick: () => {
            const record = wm.list().find((candidate) => candidate.id === info.id);
            if (!record) return;
            if (record.minimized) {
              // Restore by focusing; the manager restores minimized windows.
              wm.toggleMinimize(info.id);
            } else if (record.focused) {
              wm.minimize(info.id);
            } else {
              wm.focus(info.id);
            }
          },
        },
        iconSvg(app?.icon ?? "grid", 17),
        h("span", { class: "taskbar-app-label" }, info.title),
      );
      taskbarApps.appendChild(btn);
    }
  }

  // Desktop icons.
  for (const app of appsForRole(deps.store.isAdmin())) {
    const btn = h(
      "button",
      {
        class: "desktop-icon",
        type: "button",
        onClick: () => openApp(app.id),
        onKeyDown: (event) => {
          if (event.key === "Enter") openApp(app.id);
        },
      },
      h("span", { class: "desktop-icon-glyph" }, iconSvg(app.icon, 24)),
      h("span", { class: "desktop-icon-label" }, app.name),
    );
    iconLayer.appendChild(btn);
  }

  desktop.append(iconLayer, windowsLayer, taskbar);
  root.appendChild(desktop);
  desktop.classList.add("is-desktop");

  // Clock.
  const updateClock = () => {
    const now = Date.now();
    clockTime.textContent = formatClock(now);
    clockDate.textContent = formatDate(now);
  };
  updateClock();
  const clockTimer = window.setInterval(updateClock, 1000);
  window.addEventListener("beforeunload", () => window.clearInterval(clockTimer));

  // Menus close on outside click / Escape.
  document.addEventListener("pointerdown", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest(".menu-panel") || target.closest(".taskbar-start") || target.closest(".user-chip")) return;
    closeMenus();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeMenus();
  });

  deps.events.start();
  renderTaskbar();

  // Force a password change when the account requires it.
  if (me.user.mustChangePassword) {
    window.setTimeout(() => promptPasswordChange(deps, toast), 400);
  }

  // Expose a tiny debug handle for integration tests and troubleshooting.
  (window as unknown as { rast?: unknown }).rast = { store: deps.store, wm, api: deps.api, openApp };
}

export function promptPasswordChange(deps: BootDeps, toast: ToastCenter): void {
  const current = h("input", { class: "input", type: "password", autocomplete: "current-password" });
  const next = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const confirm = h("input", { class: "input", type: "password", autocomplete: "new-password" });
  const error = h("div", { class: "field-hint", style: "color:var(--danger)" });
  const body = h(
    "div",
    {},
    h("p", { class: "muted" }, "Your administrator requires you to set a new password before continuing."),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Current password"), current),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "New password"), next),
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Confirm new password"), confirm),
    error,
  );
  modal({
    title: "Change password",
    body,
    actions: [
      {
        label: "Update password",
        variant: "primary",
        keepOpen: true,
        onClick: async () => {
          error.textContent = "";
          if (next.value !== confirm.value) {
            error.textContent = "Passwords do not match";
            return;
          }
          try {
            await deps.api.post("/api/auth/password", {
              currentPassword: current.value,
              newPassword: next.value,
            });
            toast.success("Password updated");
            const me = await deps.api.get<MeResponse>("/api/auth/me");
            deps.store.patch({ user: me.user });
            document.querySelectorAll(".modal-backdrop").forEach((node) => node.remove());
          } catch (err) {
            error.textContent = errorMessage(err);
          }
        },
      },
    ],
  });
}

export { promptDialog };
export type { WindowHandle, AppDefinition };
