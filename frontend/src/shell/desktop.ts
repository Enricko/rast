// Desktop shell: wallpaper, application icon grid, window layer, taskbar, tray,
// panels (start menu / account / status / clock), context menus and shortcuts.

import { appsForPermissions, findApp, isAppAllowed, isAppInstalled, permissionsSignature } from "../apps/registry";
import { APP_DRAG_TYPE, type AppContext, type AppDefinition, type DesktopActions } from "../core/app";
import { rectToStage, stageSize } from "../core/display";
import { h } from "../core/dom";
import { formatClock } from "../core/format";
import { fullscreenShortcutLabel, fullscreenSupported, isFullscreen, isFullscreenShortcut, onFullscreenChange, toggleFullscreen } from "../core/fullscreen";
import { detectPlatform, startMenuHint, watchOsKeyTap } from "../core/os-key";
import { appIconSvg, iconSvg } from "../core/icons";
import type { MeResponse, Settings } from "../core/types";
import {
  DEFAULT_APPEARANCE,
  DEFAULT_TASKBAR_PINS,
  applyAppearance,
  getAppearanceService,
  getAppearanceSync,
  isAppearancePreviewActive,
  isStaleAppearanceError,
  type Appearance,
  type ThemeSetting,
} from "../core/appearance";
import { ToastCenter } from "../core/toast";
import { confirmDialog, contextMenu, dialogOpen, errorMessage, type ContextMenuEntry } from "../core/ui";
import { getRealtime } from "../core/realtime";
import { WindowManager, type WindowInfo, type WindowPlacement } from "../core/wm";
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
import { openDisplaySettings } from "./display-settings";
import { openPersonalizeDialog } from "./personalize";
import { taskbarSlots } from "./taskbar-order";
import { createDesktopIcons } from "./desktop-icons";
import { createWidgetLayer, type WidgetLayer } from "./widgets";
import {
  MAX_SESSION_WINDOWS,
  createSessionSaver,
  loadSession,
  newWindowKey,
  placementOf,
  storableParams,
  type SavedSession,
  type SavedWindow,
  type SessionUpdate,
} from "./window-session";
import { ViewTracker, type WindowView } from "./window-view";
import { WindowDialogs, type SavedDialog } from "./window-dialogs";

const MAX_PINNED = 8;
const MAX_TASKBAR_WINDOWS = 10;
const SESSION_WARNING_MS = 10 * 60_000;

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
  // Resource widgets sit above the icons and below windows; nothing else is
  // shown on an empty desktop (wallpaper, icons, taskbar and widgets only).
  const widgetLayer = h("div", { class: "widget-layer", role: "region", ariaLabel: "Desktop widgets" });
  const windowsLayer = h("div", { class: "windows-layer" });
  const taskbar = h("div", { class: "taskbar", role: "toolbar", ariaLabel: "Taskbar" });
  const toast = new ToastCenter(desktop);
  desktop.append(wallpaperLayer, iconLayer, widgetLayer, windowsLayer, taskbar);
  deps.root.appendChild(desktop);

  // The account's pins (saved with its appearance); defaults until loaded.
  let pinned: string[] = [...DEFAULT_TASKBAR_PINS];
  let panel: HTMLElement | null = null;
  let panelKind: string | null = null;
  let disposed = false;
  /** Where each window's app is (launch parameters, then `ctx.saveState`). */
  const windowParams = new Map<string, unknown>();
  /** Stable key of each window in the saved desktop session. */
  const windowKeys = new Map<string, string>();
  /** Saving starts once the saved session was opened (or there was none). */
  let sessionReady = false;
  /** Singleton windows that show new parameters when opened again. */
  const reopenListeners = new Map<string, (params: unknown) => void>();
  /** Apps that follow another page moving them elsewhere. */
  const placeListeners = new Map<string, (params: unknown) => void>();
  /**
   * Windows opened or moved from a saved session, with the placement saved
   * and the one shown: a smaller screen moves windows to fit, and as long as
   * the window stays there the session keeps the saved placement (so this
   * screen does not move the windows on a larger one).
   */
  const sessionPlaces = new Map<string, { shown: WindowPlacement; saved: WindowPlacement }>();
  /** Scroll positions and typed text inside each window. */
  const windowViews = new Map<string, ViewTracker>();
  /** Dialogs each window keeps (AppContext.dialog). */
  const windowDialogs = new Map<string, WindowDialogs>();
  const disposers: Array<() => void> = [];

  const wm = new WindowManager(windowsLayer, {
    taskbarHeight: 58,
    onChange: () => {
      renderTaskbar();
      if (sessionReady) sessionSaver.schedule();
    },
  });
  disposers.push(() => wm.destroy());
  // Open windows follow the account: a reload, another tab or another device
  // shows the same windows (see window-session.ts).
  const sessionSaver = createSessionSaver(deps.api, captureSession, {
    apply: applySession,
    busy: () => wm.isInteracting(),
  });
  // Dialogs of the desktop itself stay too ("Sign out?"): after a reload,
  // in another tab or on another device, and live.
  const desktopDialogs = new WindowDialogs(() => {
    if (sessionReady) sessionSaver.schedule();
  });
  desktopDialogs.register("sign-out", () => void requestSignOut());
  desktopDialogs.mounted();
  disposers.push(() => desktopDialogs.dispose());

  // ------------------------------------------------------ per-user appearance
  const appearance = getAppearanceService(deps.api);
  const appearanceSync = getAppearanceSync(deps.api, deps.events);
  // Desktop icons (arranged per account): created before the appearance
  // subscription below, which refreshes them. Icons and widgets share the
  // desktop's free space (shell/desktop-space.ts): icons keep their cells and
  // widgets make room; neither is ever dropped on the other.
  let widgetLayerRef: WidgetLayer | null = null;
  const desktopIcons = createDesktopIcons({
    layer: iconLayer,
    desktop,
    appearance,
    toast,
    visibleApps,
    openApp: (appId) => openApp(appId),
    menuItems: iconContextItems,
    dropOnTaskbar: (appId) => pinApp(appId),
    widgetRects: () => widgetLayerRef?.rects() ?? [],
    onLayout: () => widgetLayerRef?.arrange(),
  });
  disposers.push(() => desktopIcons.dispose());
  const widgets = createWidgetLayer({
    layer: widgetLayer,
    api: deps.api,
    realtime: getRealtime(deps.events),
    appearance,
    toast,
    isMonitorAllowed: () => isAppAllowed("monitor", store.get().permissions, store.isAdmin()),
    openMonitor: () => openApp("monitor"),
    iconRects: () => desktopIcons.occupiedRects(),
  });
  widgetLayerRef = widgets;
  disposers.push(() => widgets.dispose());
  /** Show the pins being saved, else the account's saved pins. */
  function showSavedPins(): void {
    if (!appearance.isKnown) return;
    const next = appearance.pendingTaskbarPins ?? appearance.value.taskbarPins;
    if (JSON.stringify(next) === JSON.stringify(pinned)) return;
    pinned = [...next];
    renderTaskbar();
  }
  // The taskbar is built further down; its first render uses these pins.
  if (appearance.isKnown) pinned = [...(appearance.pendingTaskbarPins ?? appearance.value.taskbarPins)];
  let appliedSignature = JSON.stringify(appearance.value);
  applyAppearance(appearance.value);
  /** Show the account's saved theme (never while the user's own choice is still being saved). */
  const adoptSavedTheme = (theme: ThemeSetting): void => {
    if (!appearance.isKnown || appearance.savingTheme) return;
    if (store.get().themePreference !== theme) store.setThemePreference(theme);
  };
  disposers.push(
    appearance.subscribe((state) => {
      // Widgets are not part of any editor preview: always show the layout
      // being saved (newest first) or the saved one.
      if (appearance.isKnown) {
        widgets.showSaved();
        showSavedPins();
        desktopIcons.refresh();
      }
      // An open editor owns the document while it previews a draft.
      if (isAppearancePreviewActive()) return;
      appliedSignature = JSON.stringify(state);
      applyAppearance(state);
      setFavicon(state.theme);
      adoptSavedTheme(state.theme);
    }),
  );
  disposers.push(
    appearanceSync.onAdopt((state) => {
      const signature = JSON.stringify(state);
      const changed = signature !== appliedSignature;
      appliedSignature = signature;
      // A reconnect/resync can "adopt" an identical snapshot: only announce a
      // visible change, never toast on every reconnect.
      if (changed && !isAppearancePreviewActive()) {
        applyAppearance(state);
        setFavicon(state.theme);
        adoptSavedTheme(state.theme);
        toast.info("Appearance updated from another session");
      }
    }),
  );
  // This account's paint cache applies synchronously (through the
  // subscription above), then the server copy. Nothing here ever saves:
  // only explicit user actions (theme controls, editors) write appearance.
  void appearance.load(me.user.id).catch((error) => {
    if (isStaleAppearanceError(error)) return;
    console.warn("appearance settings unavailable", error);
  });

  /** The tray toggle and account menu: show the choice now, save only the theme. */
  function setShellTheme(preference: ThemeSetting): void {
    store.setThemePreference(preference);
    void appearance.saveTheme(preference).then((ok) => {
      if (!ok && !disposed) toast.warning("Could not save the theme to your account. It will apply in this browser until you reload.");
    });
  }

  // ------------------------------------------------------------ app launch
  function openApp(
    appId: string,
    params?: unknown,
    restore?: {
      key: string;
      placement: WindowPlacement;
      view?: WindowView | undefined;
      dialogs?: SavedDialog[] | undefined;
    },
  ): void {
    const app: AppDefinition | undefined = findApp(appId);
    if (!app) {
      toast.error(`Unknown application: ${appId}`);
      return;
    }
    if (!isAppAllowed(app.id, store.get().permissions, store.isAdmin())) {
      toast.warning(`${app.name} is not available for your account`);
      return;
    }
    if (!restore) {
      closePanel();
      pushRecentApp(app.id);
    }
    const openBefore = new Set(wm.list().map((win) => win.id));
    const handle = wm.open(
      app,
      params,
      async (mount) => {
        const windowId = mount.window.id;
        mount.onDispose(() => {
          windowParams.delete(windowId);
          windowKeys.delete(windowId);
          placeListeners.delete(windowId);
          sessionPlaces.delete(windowId);
        });
        mount.contentEl.replaceChildren();
        const view = new ViewTracker(mount.contentEl, () => {
          if (sessionReady) sessionSaver.schedule();
        });
        windowViews.set(windowId, view);
        mount.onDispose(() => {
          view.dispose();
          windowViews.delete(windowId);
        });
        // Scrolled to and typed as saved, as soon as the app shows it.
        if (restore?.view) view.apply(restore.view, false);
        const dialogs = new WindowDialogs(() => {
          if (sessionReady) sessionSaver.schedule();
        }, mount.contentEl);
        windowDialogs.set(windowId, dialogs);
        mount.onDispose(() => {
          dialogs.dispose();
          windowDialogs.delete(windowId);
        });
        if (restore?.dialogs) dialogs.apply(restore.dialogs, false);
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
          desktop: desktopActions,
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
          onReopen: (callback: (next: unknown) => void) => {
            reopenListeners.set(windowId, callback);
            mount.onDispose(() => reopenListeners.delete(windowId));
          },
          saveState: (state: unknown) => {
            if (mount.window.isClosed()) return;
            windowParams.set(windowId, storableParams(state));
            if (sessionReady) sessionSaver.schedule();
          },
          onPlace: (callback: (next: unknown) => void) => {
            placeListeners.set(windowId, callback);
          },
          dialog: (name: string, dialogParams?: unknown) => dialogs.session(name, dialogParams),
          onDialog: (name: string, reopen: (next: unknown) => void) => dialogs.register(name, reopen),
        };
        await app.mount(ctx);
        // Dialogs the window had open open again now that the app runs.
        dialogs.mounted();
      },
      { singleton: app.singleton, ...(restore ? { placement: restore.placement } : {}) },
    );
    if (!openBefore.has(handle.id)) {
      windowKeys.set(handle.id, restore?.key ?? newWindowKey());
      windowParams.set(handle.id, storableParams(params));
      if (restore) rememberPlace(handle.id, restore.placement);
    } else if (params !== undefined) {
      // A singleton shown again with a new place to show.
      windowParams.set(handle.id, storableParams(params));
      reopenListeners.get(handle.id)?.(params);
    }
    if (sessionReady) sessionSaver.schedule();
  }

  // ------------------------------------------------------- desktop session
  function samePlace(a: WindowPlacement, b: WindowPlacement): boolean {
    if (a.state !== b.state || a.minimized !== b.minimized) return false;
    if (!a.rect || !b.rect) return a.rect === b.rect;
    return (
      Math.round(a.rect.x) === Math.round(b.rect.x) &&
      Math.round(a.rect.y) === Math.round(b.rect.y) &&
      Math.round(a.rect.w) === Math.round(b.rect.w) &&
      Math.round(a.rect.h) === Math.round(b.rect.h)
    );
  }

  function rememberPlace(id: string, saved: WindowPlacement): void {
    const shown = wm.placement(id);
    if (shown) sessionPlaces.set(id, { shown, saved: { ...saved, rect: saved.rect ? { ...saved.rect } : null } });
  }

  /** Where a window is, for the session (see `sessionPlaces`). */
  function sessionPlacement(id: string): WindowPlacement | null {
    const placement = wm.placement(id);
    if (!placement) return null;
    const kept = sessionPlaces.get(id);
    if (kept) {
      const shown = { ...kept.shown, minimized: placement.minimized };
      if (samePlace(shown, placement)) return { ...kept.saved, minimized: placement.minimized };
      sessionPlaces.delete(id);
    }
    return placement;
  }

  function captureSession(): SavedSession {
    const infos = new Map(wm.list().map((win) => [win.id, win]));
    const windows: SavedWindow[] = [];
    let focused: string | null = null;
    for (const id of wm.stackOrder().slice(-MAX_SESSION_WINDOWS)) {
      const info = infos.get(id);
      const placement = sessionPlacement(id);
      const key = windowKeys.get(id);
      if (!info || !placement || !key) continue;
      const rect = placement.rect;
      const entry: SavedWindow = {
        key,
        appId: info.appId,
        rect:
          rect && rect.w > 0 && rect.h > 0
            ? { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.w), h: Math.round(rect.h) }
            : null,
        state: placement.state,
        minimized: placement.minimized,
      };
      const params = windowParams.get(id);
      if (params !== undefined) entry.params = params;
      const view = windowViews.get(id)?.capture();
      if (view) entry.view = view;
      const dialogs = windowDialogs.get(id)?.capture();
      if (dialogs) entry.dialogs = dialogs;
      windows.push(entry);
      if (info.focused && !placement.minimized) focused = key;
    }
    const stage = stageSize();
    const session: SavedSession = {
      version: 1,
      windows,
      focused,
      stage: { w: Math.max(1, Math.round(stage.width)), h: Math.max(1, Math.round(stage.height)) },
    };
    const dialogs = desktopDialogs.capture();
    if (dialogs) session.dialogs = dialogs;
    return session;
  }

  function restoreSession(session: SavedSession): void {
    desktopDialogs.apply(session.dialogs ?? [], false);
    for (const entry of session.windows.slice(-MAX_SESSION_WINDOWS)) {
      if (!entry || typeof entry.appId !== "string" || typeof entry.key !== "string") continue;
      if (!isAppAllowed(entry.appId, store.get().permissions, store.isAdmin())) continue;
      openApp(entry.appId, entry.params, {
        key: entry.key,
        placement: placementOf(entry),
        view: entry.view,
        dialogs: entry.dialogs,
      });
    }
    if (session.focused) {
      for (const [id, key] of windowKeys) {
        if (key !== session.focused) continue;
        const win = wm.list().find((candidate) => candidate.id === id);
        if (win && !win.minimized) wm.focus(id);
      }
    }
  }

  /**
   * Show the session another page saved: open, move, restack and close
   * windows until this page shows the same desktop. Windows stay mounted
   * when they only moved; their apps follow a new place (`ctx.onPlace`).
   */
  function applySession(session: SavedSession): void {
    const allowed = (entry: SavedWindow | null | undefined): entry is SavedWindow =>
      Boolean(entry) &&
      typeof entry?.appId === "string" &&
      typeof entry.key === "string" &&
      isAppAllowed(entry.appId, store.get().permissions, store.isAdmin());
    desktopDialogs.apply(session.dialogs ?? [], true);
    const entries = session.windows.slice(-MAX_SESSION_WINDOWS).filter(allowed);
    const wanted = new Set(entries.map((entry) => entry.key));
    const appOf = new Map(wm.list().map((win) => [win.id, win.appId]));
    const idOf = new Map<string, string>();
    for (const [id, key] of windowKeys) if (wanted.has(key)) idOf.set(key, id);
    // A window this page opened while the other page opened the same app
    // has another key: keep it (mounted) as that window.
    const spare = wm.stackOrder().filter((id) => !wanted.has(windowKeys.get(id) ?? ""));
    for (const entry of entries) {
      if (idOf.has(entry.key)) continue;
      const index = spare.findIndex((id) => appOf.get(id) === entry.appId);
      if (index < 0) continue;
      const [id] = spare.splice(index, 1);
      if (!id) continue;
      windowKeys.set(id, entry.key);
      idOf.set(entry.key, id);
    }
    for (const id of spare) wm.close(id);

    for (const entry of entries) {
      const target = placementOf(entry);
      const id = idOf.get(entry.key);
      if (!id) {
        openApp(entry.appId, entry.params, {
          key: entry.key,
          placement: target,
          view: entry.view,
          dialogs: entry.dialogs,
        });
        continue;
      }
      const current = sessionPlacement(id);
      if (!current || !samePlace(current, target)) {
        wm.arrange(id, target);
        rememberPlace(id, target);
      }
      const params = storableParams(entry.params);
      if (params !== undefined && JSON.stringify(params) !== JSON.stringify(windowParams.get(id))) {
        windowParams.set(id, params);
        try {
          placeListeners.get(id)?.(params);
        } catch (error) {
          console.error(`${entry.appId} could not show its new place`, error);
        }
      }
      windowViews.get(id)?.apply(entry.view, true);
      windowDialogs.get(id)?.apply(entry.dialogs ?? [], true);
    }

    const order: string[] = [];
    let focusedId: string | null = null;
    for (const [id, key] of windowKeys) {
      if (key === session.focused) focusedId = id;
    }
    for (const entry of entries) {
      for (const [id, key] of windowKeys) if (key === entry.key) order.push(id);
    }
    const focusedNow = wm.list().find((win) => win.focused)?.id ?? null;
    const stackNow = wm.stackOrder();
    if (focusedNow !== focusedId || order.join() !== stackNow.join()) wm.setStack(order, focusedId);
  }

  async function startSession(fromMemory: SessionSnapshot | null): Promise<void> {
    // A failed load must not let the next change replace the saved windows
    // with whatever this page has open: try a few times first (once when
    // this page's own windows are at hand).
    let loaded: SessionUpdate | null = null;
    for (const wait of fromMemory ? [0] : [0, 2_000, 5_000]) {
      if (wait) await new Promise((resolve) => window.setTimeout(resolve, wait));
      if (disposed) return;
      try {
        loaded = await loadSession(deps.api);
        break;
      } catch {
        loaded = null;
      }
    }
    if (disposed) return;
    const newer = loaded?.session && (loaded.updatedAt ?? 0) > (fromMemory?.version ?? 0) ? loaded.session : null;
    if (fromMemory && !newer) {
      // Signed in again in this page: its windows, saved if they changed.
      restoreSession(fromMemory.session);
      sessionReady = true;
      sessionSaver.started(fromMemory.version, false);
      sessionSaver.schedule();
      return;
    }
    if (loaded?.session) restoreSession(loaded.session);
    sessionReady = true;
    sessionSaver.started(loaded?.updatedAt ?? null, loaded !== null);
    if (!loaded) sessionSaver.schedule();
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
    setTheme: (preference) => setShellTheme(preference),
    notify: (message, kind) => {
      if (kind === "error") toast.error(message);
      else if (kind === "warning") toast.warning(message);
      else if (kind === "success") toast.success(message);
      else toast.info(message);
    },
  };

  // ------------------------------------------------------------ desk icons
  function visibleApps(): AppDefinition[] {
    return appsForPermissions(store.get().permissions, store.isAdmin());
  }

  function isPinned(appId: string): boolean {
    return pinned.includes(appId);
  }

  function savePins(next: string[]): boolean {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet, so the taskbar cannot be changed. Try again in a moment.");
      return false;
    }
    pinned = next;
    renderTaskbar();
    void appearance.saveTaskbarPins(next).then((ok) => {
      if (ok || disposed) return;
      toast.warning("Could not save the taskbar to your account. Showing the saved pins.");
      showSavedPins();
    });
    return true;
  }

  function pinApp(appId: string): void {
    if (isPinned(appId)) return;
    if (pinned.length >= MAX_PINNED) {
      toast.warning(`The taskbar holds ${MAX_PINNED} pinned apps. Unpin one first.`);
      return;
    }
    if (savePins([...pinned, appId])) toast.success(`Pinned ${findApp(appId)?.name ?? appId} to the taskbar`);
  }

  function unpinApp(appId: string): void {
    if (!isPinned(appId)) return;
    savePins(pinned.filter((id) => id !== appId));
  }

  function iconContextItems(app: AppDefinition): ContextMenuEntry[] {
    return [
      { label: "Open", icon: "arrowRight", onClick: () => openApp(app.id) },
      isPinned(app.id)
        ? { label: "Unpin from taskbar", icon: "unpin", onClick: () => unpinApp(app.id) }
        : { label: "Pin to taskbar", icon: "pin", onClick: () => pinApp(app.id) },
      { label: "Remove from desktop", icon: "close", onClick: () => desktopIcons.remove(app.id) },
      "separator",
      { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
    ];
  }

  /** What apps (the Applications folder) may change on the desktop and taskbar. */
  const desktopActions: DesktopActions = {
    apps: visibleApps,
    isOnDesktop: (appId) => desktopIcons.has(appId),
    addToDesktop: (appId) => {
      if (desktopIcons.add(appId)) toast.success(`Added ${findApp(appId)?.name ?? appId} to the desktop`);
    },
    removeFromDesktop: (appId) => desktopIcons.remove(appId),
    isPinned,
    pin: pinApp,
    unpin: unpinApp,
  };

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
    if (!appearance.isKnown) {
      // Never open an editor seeded with defaults: saving it would replace
      // the account's real settings.
      void appearance
        .load(me.user.id)
        .then(() => {
          if (!disposed && appearance.isKnown) showPersonalize();
        })
        .catch((error) => {
          if (isStaleAppearanceError(error) || disposed) return;
          toast.error(`Could not load your saved appearance: ${errorMessage(error)}. Try again in a moment.`);
        });
      return;
    }
    openPersonalizeDialog({
      api: deps.api,
      events: deps.events,
      userId: me.user.id,
      toast,
      restoreTheme: () => {
        if (appearance.isKnown) store.setThemePreference(appearance.value.theme);
      },
      onApplied: (saved: Appearance) => {
        store.setThemePreference(saved.theme);
        applyAppearance(saved);
      },
    });
  }

  async function requestSignOut(): Promise<void> {
    closePanel();
    // Kept like a window's dialogs: confirming signs out the device it is
    // confirmed on, and closes it (as cancelled) everywhere else.
    const ok = await confirmDialog({
      title: "Sign out",
      message: "Sign out of RAST? Your windows, and what you typed in them, come back the next time you sign in.",
      confirmLabel: "Sign out",
      danger: true,
      session: desktopDialogs.session("sign-out"),
    });
    if (!ok) return;
    if (sessionReady) await sessionSaver.flush();
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
      { label: "Applications", icon: "apps", onClick: () => openApp("launcher") },
      { label: "Reset icon layout", icon: "grid", onClick: () => desktopIcons.reset(), disabled: !desktopIcons.isCustomized() },
      "separator",
      { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
      { label: "Display settings…", icon: "monitor", onClick: () => openDisplaySettings() },
      { label: "Widgets…", icon: "activity", onClick: () => widgets.openCatalog() },
      { label: "Show desktop", icon: "monitor", onClick: () => wm.minimizeAll(), disabled: wm.list().length === 0 },
      ...fullscreenMenuItem(),
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
      ariaExpanded: false,
      dataset: { tip: `Start menu (${startMenuHint().tip})`, panelTrigger: "start" },
      onClick: () => toggleStartPanel(startButton),
    },
    iconSvg("logo", 23),
  );

  const taskbarApps = h("div", { class: "taskbar-apps" });

  const connectionButton = h("button", {
    class: "tray-button tray-connection",
    type: "button",
    ariaLabel: "Connection status",
    ariaExpanded: false,
    dataset: { panelTrigger: "status", tip: "Live connection status" },
    onClick: () => toggleStatusPanel(connectionButton),
  });

  const themeButton = h("button", {
    class: "tray-button",
    type: "button",
    ariaLabel: "Switch theme",
    dataset: { tip: "Switch dark / light mode" },
    onClick: () => setShellTheme(store.get().theme === "dark" ? "light" : "dark"),
  });

  // Full screen for the whole desktop (also Ctrl+Shift+F and the menus).
  const platform = detectPlatform();
  const fullscreenButton = h("button", {
    class: "tray-button tray-fullscreen",
    type: "button",
    onClick: () => switchFullscreen(),
  });
  fullscreenButton.hidden = !fullscreenSupported();
  function showFullscreenState(): void {
    const on = isFullscreen();
    const label = on ? "Exit full screen" : "Full screen";
    const tip = `${label} (${fullscreenShortcutLabel(platform)}${on ? " or Esc" : ""})`;
    fullscreenButton.replaceChildren(iconSvg(on ? "fullscreen-exit" : "fullscreen", 18));
    fullscreenButton.setAttribute("aria-label", label);
    fullscreenButton.setAttribute("aria-pressed", String(on));
    fullscreenButton.dataset.tip = tip;
  }
  function switchFullscreen(): void {
    toggleFullscreen().catch((error) => toast.error(`Full screen is unavailable here: ${errorMessage(error)}`));
  }
  function fullscreenMenuItem(): ContextMenuEntry[] {
    if (!fullscreenSupported()) return [];
    const on = isFullscreen();
    return [
      {
        label: on ? "Exit full screen" : "Full screen",
        icon: on ? "fullscreen-exit" : "fullscreen",
        shortcut: fullscreenShortcutLabel(platform),
        onClick: () => switchFullscreen(),
      },
    ];
  }
  showFullscreenState();
  disposers.push(onFullscreenChange(showFullscreenState));

  const clockTime = h("span", { class: "tray-clock-time" });
  const clockDate = h("span", { class: "tray-clock-date" });
  const clockButton = h(
    "button",
    {
      class: "tray-clock",
      type: "button",
      ariaLabel: "Date and time",
      ariaExpanded: false,
      dataset: { panelTrigger: "clock", tip: "Date, time and server time" },
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
      ariaExpanded: false,
      dataset: { panelTrigger: "user", tip: "Account menu" },
      onClick: () => toggleUserPanel(userButton),
    },
    userAvatar,
    h("span", { class: "user-chip-text" }, userName, userRole),
  );

  taskbar.append(
    startButton,
    taskbarApps,
    h("div", { class: "taskbar-tray" }, connectionButton, themeButton, fullscreenButton, clockButton, userButton),
  );

  // ------------------------------------------------------ taskbar tooltips
  // One themed tooltip for every taskbar control (`data-tip`), shown on hover
  // after a short delay and immediately on keyboard focus, always kept inside
  // the viewport (above the control, or below if there is no room).
  const taskbarTooltip = h("div", { class: "taskbar-tooltip", role: "tooltip", ariaHidden: true });
  document.body.appendChild(taskbarTooltip);
  disposers.push(() => taskbarTooltip.remove());
  let tooltipAnchor: HTMLElement | null = null;
  let tooltipTimer: number | undefined;
  const TOOLTIP_DELAY_MS = 450;
  const TOOLTIP_MARGIN = 8;

  function placeTaskbarTooltip(anchor: HTMLElement): void {
    const text = anchor.dataset.tip;
    if (!text || !anchor.isConnected) {
      hideTaskbarTooltip();
      return;
    }
    taskbarTooltip.textContent = text;
    taskbarTooltip.style.left = "0px";
    taskbarTooltip.style.top = "0px";
    // Measured on screen, placed in desktop pixels (see core/display.ts).
    const anchorRect = rectToStage(anchor.getBoundingClientRect());
    const tipRect = rectToStage(taskbarTooltip.getBoundingClientRect());
    const viewportWidth = stageSize().width;
    const maxLeft = Math.max(TOOLTIP_MARGIN, viewportWidth - tipRect.width - TOOLTIP_MARGIN);
    const left = Math.min(Math.max(anchorRect.left + anchorRect.width / 2 - tipRect.width / 2, TOOLTIP_MARGIN), maxLeft);
    let top = anchorRect.top - tipRect.height - TOOLTIP_MARGIN;
    if (top < TOOLTIP_MARGIN) top = anchorRect.bottom + TOOLTIP_MARGIN;
    taskbarTooltip.style.left = `${Math.round(left)}px`;
    taskbarTooltip.style.top = `${Math.round(top)}px`;
    taskbarTooltip.classList.add("is-visible");
  }

  function showTaskbarTooltip(anchor: HTMLElement, delay: number): void {
    window.clearTimeout(tooltipTimer);
    tooltipAnchor = anchor;
    // Moving between controls while a tooltip is open switches immediately.
    const wait = taskbarTooltip.classList.contains("is-visible") ? 0 : delay;
    tooltipTimer = window.setTimeout(() => {
      if (tooltipAnchor === anchor && !disposed) placeTaskbarTooltip(anchor);
    }, wait);
  }

  function hideTaskbarTooltip(): void {
    window.clearTimeout(tooltipTimer);
    tooltipTimer = undefined;
    tooltipAnchor = null;
    taskbarTooltip.classList.remove("is-visible");
  }

  const tipTarget = (target: EventTarget | null): HTMLElement | null =>
    target instanceof Element ? target.closest<HTMLElement>("[data-tip]") : null;
  taskbar.addEventListener("pointerover", (event) => {
    if (event.pointerType === "touch") return;
    const anchor = tipTarget(event.target);
    if (anchor && anchor !== tooltipAnchor) showTaskbarTooltip(anchor, TOOLTIP_DELAY_MS);
  });
  taskbar.addEventListener("pointerout", (event) => {
    const next = tipTarget(event.relatedTarget);
    if (next && next === tooltipAnchor) return;
    if (!next) hideTaskbarTooltip();
  });
  taskbar.addEventListener("focusin", (event) => {
    const anchor = tipTarget(event.target);
    if (anchor && anchor.matches(":focus-visible")) showTaskbarTooltip(anchor, 0);
  });
  taskbar.addEventListener("focusout", () => hideTaskbarTooltip());
  taskbar.addEventListener("pointerdown", () => hideTaskbarTooltip());
  const onTooltipKey = (event: KeyboardEvent): void => {
    if (event.key === "Escape" && tooltipAnchor) hideTaskbarTooltip();
  };
  document.addEventListener("keydown", onTooltipKey);
  disposers.push(() => document.removeEventListener("keydown", onTooltipKey));

  // ------------------------------------------------------- taskbar density
  // When the app strip runs out of room the buttons become icon-only
  // (`is-compact`); if that is still not enough the tray drops its secondary
  // text (`is-tight`). Names stay available as tooltips and accessible names.
  let densityFrame = 0;
  function updateTaskbarDensity(): void {
    densityFrame = 0;
    if (disposed) return;
    const overflowing = (): boolean => taskbarApps.scrollWidth > taskbarApps.clientWidth + 1;
    taskbar.classList.remove("is-compact", "is-tight");
    if (!overflowing()) return;
    taskbar.classList.add("is-compact");
    if (!overflowing()) return;
    taskbar.classList.add("is-tight");
  }
  function scheduleTaskbarDensity(): void {
    if (densityFrame || disposed) return;
    densityFrame = window.requestAnimationFrame(updateTaskbarDensity);
  }
  if (typeof ResizeObserver === "function") {
    const densityObserver = new ResizeObserver(() => scheduleTaskbarDensity());
    densityObserver.observe(taskbar);
    disposers.push(() => densityObserver.disconnect());
  } else {
    window.addEventListener("resize", scheduleTaskbarDensity);
    disposers.push(() => window.removeEventListener("resize", scheduleTaskbarDensity));
  }
  disposers.push(() => {
    if (densityFrame) window.cancelAnimationFrame(densityFrame);
  });

  function taskbarWindowButton(win: WindowInfo): HTMLButtonElement {
    const app = findApp(win.appId);
    const button = h(
      "button",
      {
        class: `taskbar-app${win.focused && !win.minimized ? " is-active" : ""}${win.minimized ? " is-minimized" : ""}`,
        type: "button",
        ariaLabel: `${app?.name ?? win.title}${win.minimized ? " (minimized)" : ""}`,
        dataset: { tip: win.minimized ? `${win.title} (minimized)` : win.title },
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
      // The application's name only; the window title (e.g. "Control Panel —
      // Profile") stays in the tooltip to tell several windows apart.
      h("span", { class: "taskbar-app-label" }, app?.name || win.title || "Window"),
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
        dataset: { tip: `${app.name} — ${app.description}` },
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
    const slots = taskbarSlots(pinned, windows, (appId) =>
      Boolean(findApp(appId)) && isAppAllowed(appId, store.get().permissions, store.isAdmin()),
    );
    let shownWindows = 0;
    for (const [index, slot] of slots.entries()) {
      if (slot.kind === "launcher") {
        taskbarApps.appendChild(pinnedButton(findApp(slot.appId)!));
        continue;
      }
      if (shownWindows >= MAX_TASKBAR_WINDOWS) {
        const remaining = slots
          .slice(index)
          .flatMap((entry) => (entry.kind === "window" ? [entry.win] : []));
        const more = h(
          "button",
          {
            class: "taskbar-app is-more",
            type: "button",
            ariaLabel: `${remaining.length} more windows`,
            dataset: { tip: `${remaining.length} more windows` },
          },
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
      taskbarApps.appendChild(taskbarWindowButton(slot.win));
      shownWindows += 1;
    }
    // The anchor of an open tooltip may have been replaced by this render.
    if (tooltipAnchor && !tooltipAnchor.isConnected) hideTaskbarTooltip();
    updateTaskbarDensity();
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
    connectionButton.dataset.tip = `Live connection: ${label} — click for status and last update`;
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
    themeButton.dataset.tip = theme === "dark" ? "Switch to light mode" : "Switch to dark mode";
    updateConnectionButton();
    setFavicon(theme);
    renderTaskbar();
  }

  // ------------------------------------------------------- clock & timers
  const pad = (value: number): string => String(value).padStart(2, "0");
  function updateClock(): void {
    const now = Date.now();
    clockTime.textContent = formatClock(now);
    const date = new Date(now);
    clockDate.textContent = `${date.toLocaleDateString(undefined, { weekday: "short" })}, ${pad(date.getDate())} ${date.toLocaleDateString(undefined, { month: "short" })}`;
    clockButton.dataset.tip = `${date.toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" })} · ${formatClock(now)}`;
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
  // The account's other pages saved their windows: show them here too.
  disposers.push(realtime.on<SessionUpdate>("desktop.session", (data) => sessionSaver.receive(data)));
  disposers.push(
    realtime.onResync(() => {
      // Events may have been missed (asleep, offline): catch up.
      if (!sessionReady || disposed) return;
      loadSession(deps.api).then(
        (update) => sessionSaver.receive({ ...update, origin: null }),
        () => undefined,
      );
    }),
  );
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
      "package.install": "Package install",
      "package.update": "Package update",
      "app.deploy": "App deployment",
      "docker.pull": "Image download",
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
  // App installs/uninstalls reach every account: they update the desktop
  // quietly (a closed window still says why).
  const QUIET_ACCESS_REASONS = new Set(["app_installed", "app_uninstalled"]);
  let announceAccess = false;
  const refreshAccess = async (): Promise<void> => {
    const announce = announceAccess;
    announceAccess = false;
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
      if (announce) toast.info("Your access was updated");
    } catch (error) {
      console.warn("could not refresh effective access", error);
    }
  };
  disposers.push(
    realtime.on<{ reason?: string; userId?: number; changedBy?: string }>("access.changed", (data) => {
      if (disposed) return;
      if (!QUIET_ACCESS_REASONS.has(data?.reason ?? "")) announceAccess = true;
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
    if (dialogOpen()) return;
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
    if (isFullscreenShortcut(event, platform) && fullscreenSupported()) {
      event.preventDefault();
      switchFullscreen();
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
  // Tap ⌘ Cmd (Mac) / the Windows key: open or close the start menu.
  disposers.push(
    watchOsKeyTap(() => {
      if (dialogOpen()) return;
      toggleStartPanel(startButton);
    }),
  );

  const onTaskbarContext = (event: MouseEvent): void => {
    const targetEl = event.target as HTMLElement;
    if (targetEl.closest(".taskbar-app, .taskbar-start, .tray-button, .tray-clock, .user-chip")) return;
    event.preventDefault();
    contextMenu(
      event.clientX,
      event.clientY,
      [
        { label: "Open start menu", icon: "logo", shortcut: startMenuHint().short, onClick: () => toggleStartPanel(startButton) },
        { label: "Show desktop", icon: "monitor", onClick: () => wm.minimizeAll(), disabled: wm.list().length === 0 },
        ...fullscreenMenuItem(),
        "separator",
        { label: "Keyboard shortcuts", icon: "grid", onClick: () => showShortcuts() },
        { label: "About RAST", icon: "info", onClick: () => showAbout() },
      ],
      { ariaLabel: "Taskbar menu" },
    );
  };
  taskbar.addEventListener("contextmenu", onTaskbarContext);

  // Apps dragged from the Applications folder (or a desktop icon) pin here.
  const carriesApp = (event: DragEvent): boolean => Boolean(event.dataTransfer?.types.includes(APP_DRAG_TYPE));
  taskbar.addEventListener("dragover", (event) => {
    if (!carriesApp(event)) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    taskbar.classList.add("is-drop-target");
  });
  taskbar.addEventListener("dragleave", (event) => {
    if (!(event.relatedTarget instanceof Node) || !taskbar.contains(event.relatedTarget)) taskbar.classList.remove("is-drop-target");
  });
  taskbar.addEventListener("drop", (event) => {
    if (!carriesApp(event)) return;
    event.preventDefault();
    taskbar.classList.remove("is-drop-target");
    const appId = event.dataTransfer?.getData(APP_DRAG_TYPE) ?? "";
    if (appId && findApp(appId)) pinApp(appId);
  });
  const clearTaskbarDrop = (): void => taskbar.classList.remove("is-drop-target");
  document.addEventListener("dragend", clearTaskbarDrop);
  disposers.push(() => document.removeEventListener("dragend", clearTaskbarDrop));

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
    desktopIcons.refresh();
    for (const win of wm.list()) {
      if (isAppAllowed(win.appId, store.get().permissions, store.isAdmin())) continue;
      const app = findApp(win.appId);
      wm.close(win.id);
      if (!isAppInstalled(win.appId, store.get().permissions)) {
        toast.warning(`${app?.name ?? "The application"} was uninstalled — its window has been closed.`);
      } else {
        toast.warning(`${app?.name ?? "Application"} access was revoked — the window has been closed.`);
      }
    }
    renderTaskbar();
    widgets.permissionsChanged();
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

  // ------------------------------------------- reopen the account's windows
  // After an expired session, the windows of this page (newer than the last
  // save); otherwise the session saved on the server.
  const inMemory = options.restore && options.restore.userId === me.user.id ? options.restore : null;
  void startSession(inMemory);

  renderTaskbar();
  updateTray();

  // Tiny debug handle used by integration checks and troubleshooting.
  (window as unknown as { rast?: unknown }).rast = { store, wm, api: deps.api, openApp, toast };

  function snapshot(): SessionSnapshot {
    // Signing in again after the session was lost: the windows come back,
    // a "Sign out?" that was open does not.
    const session = captureSession();
    delete session.dialogs;
    return { userId: me.user.id, session, version: sessionSaver.version };
  }

  function dispose(): void {
    if (disposed) return;
    disposed = true;
    // Closing the desktop closes its windows; the saved session keeps them.
    sessionSaver.stop();
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
