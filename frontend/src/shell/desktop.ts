// Desktop shell: wallpaper, application icon grid, window layer, taskbar, tray,
// panels (start menu / account / status / clock), context menus and shortcuts.

import { appsForPermissions, findApp, isAppAllowed, permissionsSignature } from "../apps/registry";
import type { AppContext, AppDefinition } from "../core/app";
import { h } from "../core/dom";
import { formatClock } from "../core/format";
import { appIconSvg, iconSvg } from "../core/icons";
import type { MeResponse, Permissions, Settings } from "../core/types";
import {
  DEFAULT_APPEARANCE,
  applyAppearance,
  getAppearanceService,
  getAppearanceSync,
  isAppearancePreviewActive,
  type Appearance,
} from "../core/appearance";
import { ToastCenter } from "../core/toast";
import { confirmDialog, contextMenu, type ContextMenuEntry } from "../core/ui";
import { getRealtime } from "../core/realtime";
import { WindowManager, type WindowInfo } from "../core/wm";
import { setFavicon } from "./brand";
import { forcePasswordChange, showAboutDialog, showShortcutsDialog } from "./dialogs";
import {
  buildClockPanel,
  buildStartMenu,
  buildStatusPanel,
  buildUserMenu,
  pushRecentApp,
  type PanelContext,
} from "./panels";
import type { DesktopController, DesktopOptions, SessionSnapshot, ShellDeps } from "./shared";
import { openPersonalizeDialog } from "./personalize";

const PINNED_KEY = "rast.pinnedApps";
const DEFAULT_PINNED = ["files", "monitor", "controlpanel"];
const MAX_PINNED = 8;
const MAX_WINDOWS = 8;
const MAX_TASKBAR_WINDOWS = 10;
const SESSION_WARNING_MS = 10 * 60_000;

function readPinnedApps(permissions: Permissions | null, isAdmin: boolean): string[] {
  const available = new Set(appsForPermissions(permissions, isAdmin).map((app) => app.id));
  let stored: string[] | null = null;
  try {
    const raw = window.localStorage.getItem(PINNED_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) stored = parsed.filter((value): value is string => typeof value === "string");
    }
  } catch {
    stored = null;
  }
  const source = stored ?? DEFAULT_PINNED;
  return source.filter((id) => available.has(id)).slice(0, MAX_PINNED);
}

function writePinnedApps(ids: string[]): void {
  try {
    window.localStorage.setItem(PINNED_KEY, JSON.stringify(ids.slice(0, MAX_PINNED)));
  } catch {
    /* ignore */
  }
}

export function startDesktop(deps: ShellDeps, me: MeResponse, options: DesktopOptions): DesktopController {
  deps.api.setCsrfToken(me.csrfToken);
  deps.store.patch({
    user: me.user,
    features: me.features,
    permissions: me.permissions ?? null,
    csrfToken: me.csrfToken,
    sessionExpiresAt: me.sessionExpiresAt,
  });
  const serverOffset = (me.serverTime ?? Date.now()) - Date.now();
  const store = deps.store;
  const isAdmin = store.isAdmin();

  deps.root.replaceChildren();
  deps.root.className = "shell";

  const desktop = h("div", { class: "desktop" });
  const wallpaperLayer = h(
    "div",
    { class: "wallpaper-layer", ariaHidden: true },
    h("div", { class: "wallpaper-media" }),
    h("div", { class: "wallpaper-overlay" }),
  );
  const iconLayer = h("div", { class: "desktop-icons", role: "listbox", ariaLabel: "Desktop applications" });
  const hintLayer = h("div", { class: "desktop-hint-layer" });
  const windowsLayer = h("div", { class: "windows-layer" });
  const taskbar = h("div", { class: "taskbar", role: "toolbar", ariaLabel: "Taskbar" });
  const toast = new ToastCenter(desktop);
  desktop.append(wallpaperLayer, iconLayer, hintLayer, windowsLayer, taskbar);
  deps.root.appendChild(desktop);

  let pinned = readPinnedApps(me.permissions ?? null, isAdmin);
  let panel: HTMLElement | null = null;
  let panelKind: string | null = null;
  let disposed = false;
  let lastWindowCount = -1;
  const windowParams = new Map<string, unknown>();
  const disposers: Array<() => void> = [];

  const wm = new WindowManager(windowsLayer, {
    taskbarHeight: 58,
    onChange: () => renderTaskbar(),
  });
  disposers.push(() => wm.destroy());

  // ------------------------------------------------------ per-user appearance
  const appearance = getAppearanceService(deps.api);
  const appearanceSync = getAppearanceSync(deps.api, deps.events);
  let appliedSignature = JSON.stringify(appearance.value);
  applyAppearance(appearance.value);
  disposers.push(
    appearance.subscribe((state) => {
      // An open editor owns the document while it previews a draft.
      if (isAppearancePreviewActive()) return;
      appliedSignature = JSON.stringify(state);
      applyAppearance(state);
      setFavicon(state.theme);
    }),
  );
  disposers.push(
    appearanceSync.onAdopt((state) => {
      const signature = JSON.stringify(state);
      const changed = signature !== appliedSignature;
      appliedSignature = signature;
      persistedTheme = state.theme;
      store.setThemePreference(state.theme);
      // A reconnect/resync can "adopt" an identical snapshot: only announce a
      // visible change, never toast on every reconnect.
      if (changed && !isAppearancePreviewActive()) {
        applyAppearance(state);
        setFavicon(state.theme);
        toast.info("Appearance updated from another session");
      }
    }),
  );
  let persistedTheme = store.get().themePreference;
  void appearance
    .load(me.user.id)
    .then((loaded) => {
      persistedTheme = loaded.theme;
      store.setThemePreference(loaded.theme);
      applyAppearance(loaded);
    })
    .catch((error) => {
      console.warn("appearance settings unavailable", error);
    });
  disposers.push(
    store.subscribe((state) => {
      if (!state.user) return;
      if (state.themePreference === persistedTheme) return;
      persistedTheme = state.themePreference;
      void appearance.saveTheme(state.themePreference);
    }),
  );

  // ------------------------------------------------------------ app launch
  function openApp(appId: string, params?: unknown): void {
    const app: AppDefinition | undefined = findApp(appId);
    if (!app) {
      toast.error(`Unknown application: ${appId}`);
      return;
    }
    if (!isAppAllowed(app.id, store.get().permissions, store.isAdmin())) {
      toast.warning(`${app.name} is not available for your account`);
      return;
    }
    closePanel();
    pushRecentApp(app.id);
    const handle = wm.open(
      app,
      params,
      async (mount) => {
        mount.contentEl.replaceChildren();
        const appHost = h("div", { class: "app" });
        mount.contentEl.appendChild(appHost);
        const busyLabel = h("span", { class: "busy-label" }, "Working…");
        const busy = h(
          "div",
          { class: "app-busy hidden", role: "status", ariaLive: "polite" },
          h("div", { class: "app-busy-inner" }, h("span", { class: "spinner" }), busyLabel),
        );
        mount.contentEl.appendChild(busy);
        let busyDepth = 0;

        const ctx: AppContext = {
          api: deps.api,
          store: deps.store,
          events: deps.events,
          toast,
          window: mount.window,
          root: appHost,
          params,
          setTitle: (title: string) => mount.window.setTitle(title),
          refreshUser: async () => {
            const fresh = await deps.api.get<MeResponse>("/api/auth/me");
            deps.api.setCsrfToken(fresh.csrfToken);
            store.patch({
              user: fresh.user,
              features: fresh.features,
              permissions: fresh.permissions ?? null,
              csrfToken: fresh.csrfToken,
              sessionExpiresAt: fresh.sessionExpiresAt,
            });
          },
          openApp,
          withBusy: async <T,>(promise: Promise<T>, label?: string): Promise<T> => {
            busyDepth += 1;
            if (label) busyLabel.textContent = label;
            busy.classList.remove("hidden");
            try {
              return await promise;
            } finally {
              busyDepth = Math.max(0, busyDepth - 1);
              if (busyDepth === 0) busy.classList.add("hidden");
            }
          },
          onResize: (callback: () => void) => mount.onResize(callback),
          onDispose: (callback: () => void) => mount.onDispose(callback),
        };
        await app.mount(ctx);
      },
      { singleton: app.singleton },
    );
    windowParams.set(handle.id, params);
  }

  function openAccount(tab: string, section?: string): void {
    openApp("controlpanel", section ? { tab, section } : { tab });
  }

  const panelContext: PanelContext = {
    deps,
    desktop,
    serverOffset,
    openApp,
    openAccount,
    requestSignOut: () => void requestSignOut(),
    showShortcuts: () => showShortcuts(),
    showAbout: () => showAbout(),
    showPersonalize: () => showPersonalize(),
    notify: (message, kind) => {
      if (kind === "error") toast.error(message);
      else if (kind === "warning") toast.warning(message);
      else if (kind === "success") toast.success(message);
      else toast.info(message);
    },
  };

  // ------------------------------------------------------------ desk icons
  const iconButtons: HTMLButtonElement[] = [];
  let selectedIcon = 0;

  function visibleApps(): AppDefinition[] {
    return appsForPermissions(store.get().permissions, store.isAdmin());
  }

  function setIconTabStops(): void {
    for (const [index, button] of iconButtons.entries()) {
      const selected = index === selectedIcon;
      button.tabIndex = selected ? 0 : -1;
      button.classList.toggle("is-selected", selected);
      button.setAttribute("aria-selected", String(selected));
    }
  }

  function selectIcon(index: number, focus = true): void {
    if (iconButtons.length === 0) return;
    selectedIcon = Math.max(0, Math.min(iconButtons.length - 1, index));
    setIconTabStops();
    if (focus) iconButtons[selectedIcon]?.focus();
  }

  function isPinned(appId: string): boolean {
    return pinned.includes(appId);
  }

  function pinApp(appId: string): void {
    if (isPinned(appId) || pinned.length >= MAX_PINNED) return;
    pinned = [...pinned, appId];
    writePinnedApps(pinned);
    renderTaskbar();
    toast.success(`Pinned ${findApp(appId)?.name ?? appId} to the taskbar`);
  }

  function unpinApp(appId: string): void {
    if (!isPinned(appId)) return;
    pinned = pinned.filter((id) => id !== appId);
    writePinnedApps(pinned);
    renderTaskbar();
  }

  function iconContextItems(app: AppDefinition): ContextMenuEntry[] {
    return [
      { label: "Open", icon: "arrowRight", onClick: () => openApp(app.id) },
      isPinned(app.id)
        ? { label: "Unpin from taskbar", icon: "unpin", onClick: () => unpinApp(app.id) }
        : { label: "Pin to taskbar", icon: "pin", onClick: () => pinApp(app.id) },
      "separator",
      { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
    ];
  }

  function renderDesktopIcons(): void {
    iconLayer.replaceChildren();
    iconButtons.length = 0;
    selectedIcon = 0;
    for (const [index, app] of visibleApps().entries()) {
      const button = h(
        "button",
      {
        class: "desktop-icon",
        type: "button",
        role: "option",
        ariaLabel: `${app.name} — ${app.description}`,
        title: app.name,
        tabIndex: index === 0 ? 0 : -1,
        onClick: () => selectIcon(index),
      },
      h("span", { class: "desktop-icon-glyph" }, appIconSvg(app.id, app.icon, 30)),
      h("span", { class: "desktop-icon-label" }, app.name),
    );
    button.addEventListener("dblclick", () => openApp(app.id));
    button.addEventListener("pointerup", (event) => {
      if (event.pointerType === "touch") openApp(app.id);
    });
    button.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        openApp(app.id);
        return;
      }
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
        event.preventDefault();
        const rect = button.getBoundingClientRect();
        contextMenu(rect.left, rect.bottom, iconContextItems(app), { ariaLabel: app.name });
        return;
      }
      const columns = Math.max(1, getComputedStyle(iconLayer).gridTemplateColumns.split(" ").length);
      if (event.key === "ArrowDown") selectIcon(index + columns);
      else if (event.key === "ArrowUp") selectIcon(index - columns);
      else if (event.key === "ArrowRight") selectIcon(index + 1);
      else if (event.key === "ArrowLeft") selectIcon(index - 1);
      else if (event.key === "Home") selectIcon(0);
      else if (event.key === "End") selectIcon(iconButtons.length - 1);
      else return;
      event.preventDefault();
    });
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      selectIcon(index, false);
      contextMenu(event.clientX, event.clientY, iconContextItems(app), { ariaLabel: app.name });
    });
    button.addEventListener("focus", () => selectIcon(index, false));
    iconButtons.push(button);
    iconLayer.appendChild(button);
  }
    setIconTabStops();
  }
  renderDesktopIcons();

  // --------------------------------------------------------------- panels
  function closePanel(): void {
    if (panel) {
      panel.remove();
      panel = null;
      panelKind = null;
    }
    for (const trigger of desktop.querySelectorAll<HTMLElement>("[data-panel-trigger]")) {
      trigger.setAttribute("aria-expanded", "false");
    }
  }

  function showPanel(kind: string, builder: () => HTMLElement, anchor: HTMLElement): void {
    if (panelKind === kind) {
      closePanel();
      return;
    }
    closePanel();
    panel = builder();
    panel.dataset.panelKind = kind;
    panel.classList.add("panel-open");
    desktop.appendChild(panel);
    panelKind = kind;
    anchor.setAttribute("aria-expanded", "true");
  }

  function toggleStartPanel(anchor: HTMLElement): void {
    showPanel("start", () => buildStartMenu(panelContext, closePanel), anchor);
  }

  function toggleUserPanel(anchor: HTMLElement): void {
    showPanel("user", () => buildUserMenu(panelContext, closePanel), anchor);
  }

  function toggleStatusPanel(anchor: HTMLElement): void {
    showPanel("status", () => buildStatusPanel(panelContext, closePanel), anchor);
  }

  function toggleClockPanel(anchor: HTMLElement): void {
    showPanel("clock", () => buildClockPanel(panelContext, closePanel), anchor);
  }

  function showShortcuts(): void {
    showShortcutsDialog();
  }

  function showAbout(): void {
    showAboutDialog(store.get().version, store.get().siteName);
  }

  function showPersonalize(): void {
    closePanel();
    openPersonalizeDialog({
      api: deps.api,
      events: deps.events,
      userId: me.user.id,
      toast,
      restoreTheme: () => store.setThemePreference(appearance.value.theme),
      onApplied: (saved: Appearance) => {
        persistedTheme = saved.theme;
        store.setThemePreference(saved.theme);
        applyAppearance(saved);
      },
    });
  }

  async function requestSignOut(): Promise<void> {
    closePanel();
    const ok = await confirmDialog({
      title: "Sign out",
      message: "Sign out of RAST? Open applications will close and any unsaved form state will be lost.",
      confirmLabel: "Sign out",
      danger: true,
    });
    if (!ok) return;
    try {
      await deps.api.post("/api/auth/logout");
    } catch {
      /* the session may already be gone */
    }
    appearance.clear();
    applyAppearance(DEFAULT_APPEARANCE);
    dispose();
    store.patch({ user: null, csrfToken: "" });
    options.onSignedOut("You have been signed out.");
  }

  // ------------------------------------------------------------- wallpaper
  desktop.addEventListener("contextmenu", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest(".window, .taskbar, .menu-panel, .desktop-icon, .toast, .context-menu, .modal-backdrop")) return;
    event.preventDefault();
    const items: ContextMenuEntry[] = [
      { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
      { label: "Show desktop", icon: "monitor", onClick: () => wm.minimizeAll(), disabled: wm.list().length === 0 },
      "separator",
      { label: "Open Control Panel", icon: "control", onClick: () => openAccount("profile"), disabled: !isAdmin },
      { label: "Open Activity Logs", icon: "logs", onClick: () => openApp("logs") },
      "separator",
      { label: "Keyboard shortcuts", icon: "grid", shortcut: "?", onClick: () => showShortcuts() },
      { label: "About RAST", icon: "info", onClick: () => showAbout() },
      "separator",
      { label: "Sign out", icon: "logout", danger: true, onClick: () => void requestSignOut() },
    ];
    contextMenu(event.clientX, event.clientY, items, { ariaLabel: "Desktop menu" });
  });

  // -------------------------------------------------------------- taskbar
  const startButton = h(
    "button",
    {
      class: "taskbar-start",
      type: "button",
      ariaLabel: "Open start menu",
      title: "Start menu (Ctrl+Space)",
      ariaExpanded: false,
      dataset: { panelTrigger: "start" },
      onClick: () => toggleStartPanel(startButton),
    },
    iconSvg("logo", 23),
  );

  const taskbarApps = h("div", { class: "taskbar-apps" });

  const connectionButton = h("button", {
    class: "tray-button tray-connection",
    type: "button",
    ariaLabel: "Connection status",
    title: "Live connection status",
    ariaExpanded: false,
    dataset: { panelTrigger: "status" },
    onClick: () => toggleStatusPanel(connectionButton),
  });

  const themeButton = h("button", {
    class: "tray-button",
    type: "button",
    ariaLabel: "Switch theme",
    title: "Switch dark / light mode",
    onClick: () => store.toggleTheme(),
  });

  const clockTime = h("span", { class: "tray-clock-time" });
  const clockDate = h("span", { class: "tray-clock-date" });
  const clockButton = h(
    "button",
    {
      class: "tray-clock",
      type: "button",
      ariaLabel: "Date and time",
      title: "Date, time and server time",
      ariaExpanded: false,
      dataset: { panelTrigger: "clock" },
      onClick: () => toggleClockPanel(clockButton),
    },
    clockTime,
    clockDate,
  );

  const userAvatar = h("span", { class: "avatar" });
  const userName = h("span", { class: "user-chip-name" });
  const userRole = h("span", { class: "user-chip-role" });
  const userButton = h(
    "button",
    {
      class: "user-chip",
      type: "button",
      ariaLabel: "Account menu",
      title: "Account menu",
      ariaExpanded: false,
      dataset: { panelTrigger: "user" },
      onClick: () => toggleUserPanel(userButton),
    },
    userAvatar,
    h("span", { class: "user-chip-text" }, userName, userRole),
  );

  taskbar.append(
    startButton,
    taskbarApps,
    h("div", { class: "taskbar-tray" }, connectionButton, themeButton, clockButton, userButton),
  );

  function taskbarWindowButton(win: WindowInfo): HTMLButtonElement {
    const app = findApp(win.appId);
    const button = h(
      "button",
      {
        class: `taskbar-app${win.focused && !win.minimized ? " is-active" : ""}${win.minimized ? " is-minimized" : ""}`,
        type: "button",
        ariaLabel: `${app?.name ?? win.title}${win.minimized ? " (minimized)" : ""}`,
        title: win.title,
        onClick: () => {
          if (win.minimized) wm.toggleMinimize(win.id);
          else if (win.focused) wm.minimize(win.id);
          else wm.focus(win.id);
        },
      },
      h(
        "span",
        { class: "taskbar-app-icon", dataset: { app: win.appId } },
        appIconSvg(win.appId, win.icon || app?.icon || "grid", 17),
      ),
      h("span", { class: "taskbar-app-label" }, win.title || app?.name || "Window"),
      h("span", { class: "taskbar-app-indicator", ariaHidden: true }),
    );
    button.addEventListener("auxclick", (event) => {
      if (event.button === 1) wm.close(win.id);
    });
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      const items: ContextMenuEntry[] = [
        { label: win.minimized ? "Restore" : "Focus", icon: "arrowRight", onClick: () => (win.minimized ? wm.toggleMinimize(win.id) : wm.focus(win.id)) },
        { label: "Minimize", icon: "minimize", onClick: () => wm.minimize(win.id), disabled: win.minimized },
        win.maximized
          ? { label: "Restore size", icon: "restore", onClick: () => wm.toggleMaximize(win.id) }
          : { label: "Maximize", icon: "maximize", onClick: () => wm.toggleMaximize(win.id) },
        "separator",
        isPinned(win.appId)
          ? { label: "Unpin from taskbar", icon: "unpin", onClick: () => unpinApp(win.appId) }
          : { label: "Pin to taskbar", icon: "pin", onClick: () => pinApp(win.appId) },
        "separator",
        { label: "Close window", icon: "close", danger: true, onClick: () => wm.close(win.id) },
      ];
      contextMenu(event.clientX, event.clientY, items, { ariaLabel: `${app?.name ?? "Window"} menu` });
    });
    return button;
  }

  function pinnedButton(app: AppDefinition): HTMLButtonElement {
    const button = h(
      "button",
      {
        class: "taskbar-app is-pinned",
        type: "button",
        ariaLabel: `Open ${app.name}`,
        title: `${app.name} — ${app.description}`,
        onClick: () => openApp(app.id),
      },
      h(
        "span",
        { class: "taskbar-app-icon", dataset: { app: app.id } },
        appIconSvg(app.id, app.icon, 17),
      ),
      h("span", { class: "taskbar-app-label" }, app.name),
      h("span", { class: "taskbar-app-indicator", ariaHidden: true }),
    );
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      contextMenu(
        event.clientX,
        event.clientY,
        [
          { label: `Open ${app.name}`, icon: "arrowRight", onClick: () => openApp(app.id) },
          { label: "Unpin from taskbar", icon: "unpin", onClick: () => unpinApp(app.id) },
          "separator",
          { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
        ],
        { ariaLabel: app.name },
      );
    });
    return button;
  }

  function renderTaskbar(): void {
    if (disposed) return;
    taskbarApps.replaceChildren();
    const windows = wm.list();
    const openApps = new Set(windows.map((win) => win.appId));
    let rendered = 0;
    for (const appId of pinned) {
      const app = findApp(appId);
      if (!app || openApps.has(appId) || !isAppAllowed(appId, store.get().permissions, store.isAdmin())) continue;
      if (rendered >= MAX_TASKBAR_WINDOWS + MAX_PINNED) break;
      taskbarApps.appendChild(pinnedButton(app));
      rendered += 1;
    }
    for (const [index, win] of windows.entries()) {
      if (index >= MAX_TASKBAR_WINDOWS) {
        const remaining = windows.slice(MAX_TASKBAR_WINDOWS);
        const more = h(
          "button",
          { class: "taskbar-app is-more", type: "button", ariaLabel: `${remaining.length} more windows` },
          iconSvg("more", 18),
          h("span", { class: "taskbar-app-label" }, `+${remaining.length}`),
        );
        more.addEventListener("click", () => {
          const rect = more.getBoundingClientRect();
          contextMenu(
            rect.left,
            rect.top - 8 - Math.min(remaining.length * 34, 300),
            remaining.map<ContextMenuEntry>((entry) => ({
              label: entry.title || findApp(entry.appId)?.name || "Window",
              icon: entry.icon || "grid",
              onClick: () => wm.focus(entry.id),
            })),
            { ariaLabel: "More windows" },
          );
        });
        taskbarApps.appendChild(more);
        break;
      }
      taskbarApps.appendChild(taskbarWindowButton(win));
      rendered += 1;
    }
    renderDesktopHint();
  }

  function updateConnectionButton(): void {
    const state = deps.events.status;
    const label = state === "connected" ? "Live" : state === "offline" ? "Offline" : "Reconnecting";
    connectionButton.replaceChildren(iconSvg(state === "connected" ? "wifi" : "wifiOff", 18));
    connectionButton.dataset.state = state;
    connectionButton.classList.toggle("is-connected", state === "connected");
    connectionButton.classList.toggle("is-connecting", state === "connecting" || state === "reconnecting");
    connectionButton.classList.toggle("is-disconnected", state === "offline");
    connectionButton.setAttribute("aria-label", `Live connection: ${label}`);
    connectionButton.title = `Live connection: ${label} — click for status and last update`;
  }

  function updateTray(): void {
    if (disposed) return;
    const user = store.get().user;
    const name = user?.displayName || user?.username || "User";
    userAvatar.textContent = name.slice(0, 1).toUpperCase();
    userName.textContent = name;
    userRole.textContent = user?.role === "admin" ? "Admin" : "User";
    userButton.setAttribute("aria-label", `Account menu for ${name}`);
    const theme = store.get().theme;
    themeButton.replaceChildren(iconSvg(theme === "dark" ? "sun" : "moon", 18));
    themeButton.setAttribute("aria-label", theme === "dark" ? "Switch to light mode" : "Switch to dark mode");
    themeButton.title = theme === "dark" ? "Switch to light mode" : "Switch to dark mode";
    updateConnectionButton();
    setFavicon(theme);
    renderTaskbar();
  }

  // ----------------------------------------------------------- empty hint
  function renderDesktopHint(): void {
    if (disposed) return;
    const count = wm.list().length;
    if (count > 0) {
      if (lastWindowCount !== count) {
        hintLayer.replaceChildren();
        hintLayer.hidden = true;
        lastWindowCount = count;
      }
      return;
    }
    if (lastWindowCount === 0) return;
    lastWindowCount = 0;
    hintLayer.hidden = false;
    const quick = pinned
      .filter((id) => isAppAllowed(id, store.get().permissions, store.isAdmin()))
      .map((id) => findApp(id))
      .filter((app): app is AppDefinition => Boolean(app))
      .slice(0, 3);
    const quickRow = h("div", { class: "desktop-hint-quick" });
    for (const app of quick) {
      quickRow.appendChild(
        h(
          "button",
          { class: "quick-launch", type: "button", onClick: () => openApp(app.id), title: app.description },
          appIconSvg(app.id, app.icon, 16),
          h("span", {}, app.name),
        ),
      );
    }
    hintLayer.replaceChildren(
      h(
        "div",
        { class: "desktop-hint" },
        h("div", { class: "desktop-hint-mark" }, iconSvg("logo", 20)),
        h("div", { class: "desktop-hint-title" }, "No applications open"),
        h(
          "div",
          { class: "desktop-hint-detail muted" },
          "Open an application from the desktop, or press Ctrl+Space to search. Right-click the desktop to personalize the wallpaper and theme.",
        ),
        quickRow,
        h("div", { class: "desktop-hint-tip small faint" }, "Tip: double-click a desktop icon to open it."),
      ),
    );
  }

  // ------------------------------------------------------- clock & timers
  const pad = (value: number): string => String(value).padStart(2, "0");
  function updateClock(): void {
    const now = Date.now();
    clockTime.textContent = formatClock(now);
    const date = new Date(now);
    clockDate.textContent = `${date.toLocaleDateString(undefined, { weekday: "short" })}, ${pad(date.getDate())} ${date.toLocaleDateString(undefined, { month: "short" })}`;
    clockButton.title = `${date.toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" })} · ${formatClock(now)}`;
  }
  updateClock();
  const clockTimer = window.setInterval(updateClock, 1000);
  disposers.push(() => window.clearInterval(clockTimer));

  let expiryWarned = false;
  const expiryTimer = window.setInterval(() => {
    const expires = store.get().sessionExpiresAt;
    if (!expires || expiryWarned) return;
    const remaining = expires - Date.now();
    if (remaining > 0 && remaining <= SESSION_WARNING_MS) {
      expiryWarned = true;
      toast.warning("Your session expires in under 10 minutes.", {
        action: { label: "Sessions", onClick: () => openAccount("profile", "sessions") },
      });
    }
  }, 30_000);
  disposers.push(() => window.clearInterval(expiryTimer));

  // --------------------------------------------------------- realtime stream
  const realtime = getRealtime(deps.events);
  let everConnected = deps.events.status === "connected";
  let warnedDisconnected = false;
  let disconnectTimer: number | undefined;
  updateConnectionButton();
  deps.events.start();
  disposers.push(
    realtime.onStatus((state) => {
      updateConnectionButton();
      if (state === "connected") {
        if (disconnectTimer !== undefined) {
          window.clearTimeout(disconnectTimer);
          disconnectTimer = undefined;
        }
        if (warnedDisconnected) {
          warnedDisconnected = false;
          toast.success("Live updates restored");
        }
        everConnected = true;
        return;
      }
      if (state !== "offline" || !everConnected || warnedDisconnected) return;
      if (disconnectTimer !== undefined) return;
      disconnectTimer = window.setTimeout(() => {
        disconnectTimer = undefined;
        if (deps.events.status === "connected") return;
        warnedDisconnected = true;
        toast.warning("Live updates are offline — reconnecting…");
      }, 6000);
    }),
  );
  disposers.push(() => {
    if (disconnectTimer !== undefined) window.clearTimeout(disconnectTimer);
  });

  // The server ends revoked/expired sessions with a dedicated event and then
  // closes the stream: show the login screen with a clear explanation.
  disposers.push(
    realtime.on<{ reason?: string }>("session.revoked", (data) => {
      if (disposed) return;
      options.onSessionRevoked?.(typeof data?.reason === "string" ? data.reason : "");
    }),
  );

  // Background job notifications for the signed-in user, coalesced so a burst
  // of finished jobs is one toast and progress events never toast.
  interface JobNoticePayload {
    id?: string;
    kind?: string;
    userId?: number | null;
    message?: string;
    error?: string | null;
  }
  const jobQueue: Array<{ kind: string; failed: boolean; error: string }> = [];
  let jobTimer: number | undefined;
  const jobLabel = (kind: string): string => {
    const labels: Record<string, string> = {
      upload: "File upload",
      copy: "File copy",
      move: "File move",
      delete: "File delete",
      trash: "Recycle bin cleanup",
      backup: "Backup",
      deploy: "App deployment",
      import: "App import",
      restore: "Restore",
    };
    return labels[kind] ?? (kind ? kind.charAt(0).toUpperCase() + kind.slice(1) : "Background job");
  };
  const flushJobs = (): void => {
    jobTimer = undefined;
    if (disposed || jobQueue.length === 0) return;
    const jobs = jobQueue.splice(0);
    if (jobs.length === 1) {
      const job = jobs[0]!;
      if (job.failed) toast.error(`${jobLabel(job.kind)} failed${job.error ? `: ${job.error}` : ""}`);
      else toast.success(`${jobLabel(job.kind)} finished`);
      return;
    }
    const failed = jobs.filter((job) => job.failed).length;
    if (failed > 0) toast.warning(`${jobs.length - failed} jobs finished, ${failed} failed`);
    else toast.success(`${jobs.length} background jobs finished`);
  };
  const queueJob = (failed: boolean, data: JobNoticePayload): void => {
    if (disposed) return;
    const currentUserId = store.get().user?.id;
    if (typeof data?.userId === "number" && data.userId !== currentUserId) return;
    jobQueue.push({ kind: data?.kind ?? "", failed, error: data?.error ?? "" });
    if (jobQueue.length > 20) jobQueue.shift();
    if (jobTimer !== undefined) window.clearTimeout(jobTimer);
    jobTimer = window.setTimeout(flushJobs, 650);
  };
  disposers.push(realtime.on<JobNoticePayload>("job.done", (data) => queueJob(false, data)));
  disposers.push(realtime.on<JobNoticePayload>("job.failed", (data) => queueJob(true, data)));
  disposers.push(() => {
    if (jobTimer !== undefined) window.clearTimeout(jobTimer);
  });

  // ------------------------------------------------------ access updates
  // Group/rule changes are pushed per affected user. We refetch the effective
  // permissions and apply them in place (launcher, taskbar, open windows).
  let accessRefreshTimer: number | undefined;
  const refreshAccess = async (): Promise<void> => {
    try {
      const fresh = await deps.api.get<MeResponse>("/api/auth/me");
      if (disposed) return;
      deps.api.setCsrfToken(fresh.csrfToken);
      store.patch({
        user: fresh.user,
        features: fresh.features,
        permissions: fresh.permissions ?? null,
        csrfToken: fresh.csrfToken,
        sessionExpiresAt: fresh.sessionExpiresAt,
      });
      toast.info("Your access was updated");
    } catch (error) {
      console.warn("could not refresh effective access", error);
    }
  };
  disposers.push(
    realtime.on<{ reason?: string; userId?: number; changedBy?: string }>("access.changed", () => {
      if (disposed) return;
      if (accessRefreshTimer !== undefined) window.clearTimeout(accessRefreshTimer);
      accessRefreshTimer = window.setTimeout(() => {
        accessRefreshTimer = undefined;
        void refreshAccess();
      }, 250);
    }),
  );
  disposers.push(() => {
    if (accessRefreshTimer !== undefined) window.clearTimeout(accessRefreshTimer);
  });

  // ------------------------------------------------------ global bindings
  const onPointerDown = (event: PointerEvent): void => {
    if (!panel) return;
    const targetEl = event.target as HTMLElement;
    if (panel.contains(targetEl)) return;
    if (targetEl.closest("[data-panel-trigger]")) return;
    closePanel();
  };
  document.addEventListener("pointerdown", onPointerDown, true);
  disposers.push(() => document.removeEventListener("pointerdown", onPointerDown, true));

  const isEditable = (target: EventTarget | null): boolean => {
    const el = target as HTMLElement | null;
    if (!el) return false;
    return Boolean(el.closest("input, textarea, select, [contenteditable='true']"));
  };

  const onKeyDown = (event: KeyboardEvent): void => {
    if (document.querySelector(".modal-backdrop")) return;
    if (event.key === "Escape") {
      if (panel) {
        closePanel();
        event.preventDefault();
      }
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.code === "Space") {
      event.preventDefault();
      toggleStartPanel(startButton);
      return;
    }
    if (event.key === "Meta" && !event.repeat) {
      event.preventDefault();
      toggleStartPanel(startButton);
      return;
    }
    if (isEditable(event.target)) return;
    if (event.key === "?" || ((event.ctrlKey || event.metaKey) && event.key === "/")) {
      event.preventDefault();
      showShortcuts();
    }
  };
  document.addEventListener("keydown", onKeyDown);
  disposers.push(() => document.removeEventListener("keydown", onKeyDown));

  const onTaskbarContext = (event: MouseEvent): void => {
    const targetEl = event.target as HTMLElement;
    if (targetEl.closest(".taskbar-app, .taskbar-start, .tray-button, .tray-clock, .user-chip")) return;
    event.preventDefault();
    contextMenu(
      event.clientX,
      event.clientY,
      [
        { label: "Open start menu", icon: "logo", shortcut: "Ctrl+Space", onClick: () => toggleStartPanel(startButton) },
        { label: "Show desktop", icon: "monitor", onClick: () => wm.minimizeAll(), disabled: wm.list().length === 0 },
        "separator",
        { label: "Keyboard shortcuts", icon: "grid", onClick: () => showShortcuts() },
        { label: "About RAST", icon: "info", onClick: () => showAbout() },
      ],
      { ariaLabel: "Taskbar menu" },
    );
  };
  taskbar.addEventListener("contextmenu", onTaskbarContext);

  // ------------------------------------------------------ store reactivity
  // Permission changes are applied in place: rebuild launcher/taskbar, close
  // windows that lost access and keep every other window exactly where it is.
  let accessSignature = permissionsSignature(store.get().permissions, store.isAdmin());

  function enforceEffectiveAccess(): void {
    if (disposed) return;
    const signature = permissionsSignature(store.get().permissions, store.isAdmin());
    if (signature === accessSignature) return;
    accessSignature = signature;
    closePanel();
    renderDesktopIcons();
    for (const win of wm.list()) {
      if (isAppAllowed(win.appId, store.get().permissions, store.isAdmin())) continue;
      const app = findApp(win.appId);
      wm.close(win.id);
      toast.warning(`${app?.name ?? "Application"} access was revoked — the window has been closed.`);
    }
    renderTaskbar();
    renderDesktopHint();
  }

  disposers.push(
    store.subscribe(() => {
      enforceEffectiveAccess();
      updateTray();
    }),
  );
  updateTray();

  // --------------------------------------------------- session-expiry flow
  if (me.user.mustChangePassword) {
    window.setTimeout(() => {
      if (disposed) return;
      forcePasswordChange(deps, toast);
    }, 420);
  }

  // Admin-only settings enrichment (site name shown in title/taskbar).
  if (isAdmin) {
    void deps.api
      .get<Settings>("/api/settings")
      .then((settings) => {
        if (disposed) return;
        if (settings.siteName && settings.siteName !== store.get().siteName) {
          store.patch({ siteName: settings.siteName });
        }
        if (settings.loginMessage) {
          toast.info(`Server notice: ${settings.loginMessage}`, { timeoutMs: 8000 });
        }
      })
      .catch(() => undefined);
  }

  // --------------------------------------------------- restore after login
  if (options.restore) {
    const toRestore = options.restore.apps.slice(0, MAX_WINDOWS);
    for (const entry of toRestore) {
      const app = findApp(entry.appId);
      if (!app || !isAppAllowed(app.id, store.get().permissions, store.isAdmin())) continue;
      openApp(entry.appId, entry.params);
    }
    const focusedAppId = options.restore.focusedAppId;
    if (focusedAppId) {
      const focused = wm.list().find((win) => win.appId === focusedAppId);
      if (focused) wm.focus(focused.id);
    }
  }

  renderTaskbar();
  renderDesktopHint();
  updateTray();

  // Tiny debug handle used by integration checks and troubleshooting.
  (window as unknown as { rast?: unknown }).rast = { store, wm, api: deps.api, openApp, toast };

  function snapshot(): SessionSnapshot {
    const snapshotApps: SessionSnapshot["apps"] = [];
    for (const win of wm.list().slice(0, MAX_WINDOWS)) {
      const entry: { appId: string; params?: unknown } = { appId: win.appId };
      const params = windowParams.get(win.id);
      if (params !== undefined) entry.params = params;
      snapshotApps.push(entry);
    }
    return { apps: snapshotApps, focusedAppId: wm.focusedAppId() };
  }

  function dispose(): void {
    if (disposed) return;
    disposed = true;
    closePanel();
    for (const disposer of disposers.splice(0)) {
      try {
        disposer();
      } catch {
        /* ignore */
      }
    }
    toast.clear();
    desktop.remove();
  }

  return { snapshot, dispose };
}
