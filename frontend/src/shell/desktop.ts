// Desktop shell: wallpaper, application icon grid, window layer, taskbar, tray,
// panels (start menu / account / status / clock), context menus and shortcuts.

import { appsForPermissions, findApp, isAppAllowed, isAppInstalled, permissionsSignature } from "../apps/registry";
import { LINUX_PREFIX, loadLinuxApps as fetchLinuxApps, onLinuxApps } from "../apps/linuxapp/catalog";
import { onPackageApps, packageAppId, refreshPackageApps } from "../apps/package-apps";
import { APP_DRAG_TYPE, type AppContext, type AppDefinition, type DesktopActions } from "../core/app";
import { h } from "../core/dom";
import { formatClock } from "../core/format";
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
import { confirmDialog, contextMenu, errorMessage, type ContextMenuEntry } from "../core/ui";
import { getRealtime } from "../core/realtime";
import { WindowManager, type WindowInfo, type WindowRect } from "../core/wm";
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
import { taskbarSlots } from "./taskbar-order";
import { createDesktopIcons } from "./desktop-icons";
import { createWidgetLayer, type WidgetLayer } from "./widgets";
import {
  boxesIntersect,
  clampGroupDelta,
  groupCollides,
  marqueeBox,
  summarizeSelection,
  type Box,
} from "./selection";

const MAX_PINNED = 8;
const MAX_WINDOWS = 8;
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
  const windowParams = new Map<string, unknown>();
  /** Singleton windows that show new parameters when opened again. */
  const reopenListeners = new Map<string, (params: unknown) => void>();
  const disposers: Array<() => void> = [];

  const wm = new WindowManager(windowsLayer, {
    taskbarHeight: 58,
    onChange: () => renderTaskbar(),
  });
  disposers.push(() => wm.destroy());

  // ------------------------------------------------------ per-user appearance
  const appearance = getAppearanceService(deps.api);
  const appearanceSync = getAppearanceSync(deps.api, deps.events);
  // Desktop icons (arranged per account): created before the appearance
  // subscription below, which refreshes them.
  let widgetLayerApi: WidgetLayer | null = null;
  const desktopIcons = createDesktopIcons({
    layer: iconLayer,
    desktop,
    appearance,
    toast,
    visibleApps,
    openApp: (appId) => openApp(appId),
    menuItems: iconContextItems,
    dropOnTaskbar: (appId) => pinApp(appId),
    blockedRects: () => widgetLayerApi?.occupiedRects() ?? [],
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
    onGeometryChange: () => desktopIcons.refresh(),
    obstacles: () => {
      // App icons in widget-layer coordinates, plus the desktop grid cell.
      const style = getComputedStyle(iconLayer);
      const cellW = Number.parseFloat(style.getPropertyValue("--icon-cell-w")) || 104;
      const cellH = Number.parseFloat(style.getPropertyValue("--icon-cell-h")) || 108;
      const layerRect = widgetLayer.getBoundingClientRect();
      const rects = [...iconLayer.querySelectorAll<HTMLElement>(".desktop-icon")].map((icon) => {
        const rect = icon.getBoundingClientRect();
        return {
          x: rect.left - layerRect.left,
          y: rect.top - layerRect.top,
          w: rect.width,
          h: rect.height,
        };
      });
      return { rects, cellW, cellH };
    },
  });
  widgetLayerApi = widgets;
  disposers.push(() => widgets.dispose());

  // ------------------------------------------------ desktop multi-selection
  const marqueeEl = h("div", { class: "desktop-marquee", ariaHidden: true, hidden: true });
  desktop.appendChild(marqueeEl);
  disposers.push(() => marqueeEl.remove());
  const selectedKeys = new Set<string>();
  let marquee: { startX: number; startY: number; additive: boolean; active: boolean } | null = null;

  function shortcutRects(): Map<string, Box> {
    const mapped = new Map<string, Box>();
    for (const [id, rect] of desktopIcons.rects()) mapped.set(`icon:${id}`, rect);
    for (const entry of widgets.rectEntries()) mapped.set(`widget:${entry.id}`, entry.rect);
    return mapped;
  }

  function renderSelection(): void {
    const iconIds = new Set<string>();
    for (const key of selectedKeys) if (key.startsWith("icon:")) iconIds.add(key.slice(5));
    desktopIcons.setSelected(iconIds);
    for (const entry of widgets.rectEntries()) {
      const element = widgetLayer.querySelector<HTMLElement>(`.desktop-widget[data-widget="${entry.id}"]`);
      element?.classList.toggle("is-multi-selected", selectedKeys.has(`widget:${entry.id}`));
    }
  }

  function clearSelection(): void {
    selectedKeys.clear();
    renderSelection();
  }

  function selectItem(key: string, event: PointerEvent | MouseEvent): void {
    const toggle = event.ctrlKey || event.metaKey;
    const add = event.shiftKey;
    if (toggle) {
      if (selectedKeys.has(key)) selectedKeys.delete(key);
      else selectedKeys.add(key);
    } else if (add) {
      selectedKeys.add(key);
    } else {
      selectedKeys.clear();
      selectedKeys.add(key);
    }
    renderSelection();
  }

  function startsMarquee(target: EventTarget | null): boolean {
    if (!(target instanceof Element)) return true;
    if (target.closest(".window, .taskbar, .menu-panel, .context-menu, .modal-backdrop, button, input, select, textarea, .widget-resize, .desktop-icon, .desktop-widget")) return false;
    return Boolean(target.closest(".desktop, .desktop-icons, .widget-layer")) || target === desktop;
  }

  function onDesktopPointerDown(event: PointerEvent): void {
    if (event.button !== 0 || disposed) return;
    const target = event.target as Element;
    const iconEl = target.closest<HTMLElement>(".desktop-icon[data-app]");
    const widgetEl = target.closest<HTMLElement>(".desktop-widget[data-widget]");
    if (iconEl?.dataset.app) {
      selectItem(`icon:${iconEl.dataset.app}`, event);
      return;
    }
    if (widgetEl?.dataset.widget) {
      selectItem(`widget:${widgetEl.dataset.widget}`, event);
      return;
    }
    if (!startsMarquee(event.target)) return;
    if (!event.ctrlKey && !event.metaKey && !event.shiftKey) clearSelection();
    const rect = desktop.getBoundingClientRect();
    marquee = {
      startX: event.clientX - rect.left,
      startY: event.clientY - rect.top,
      additive: event.shiftKey,
      active: false,
    };
    event.preventDefault();
  }

  function onDesktopPointerMove(event: PointerEvent): void {
    if (!marquee || disposed) return;
    const rect = desktop.getBoundingClientRect();
    const currentX = event.clientX - rect.left;
    const currentY = event.clientY - rect.top;
    if (!marquee.active) {
      if (Math.hypot(currentX - marquee.startX, currentY - marquee.startY) < 4) return;
      marquee.active = true;
      marqueeEl.hidden = false;
      const root = document.documentElement;
      root.classList.add("is-selecting");
    }
    const box = marqueeBox(marquee.startX, marquee.startY, currentX, currentY);
    marqueeEl.style.left = `${box.x}px`;
    marqueeEl.style.top = `${box.y}px`;
    marqueeEl.style.width = `${box.w}px`;
    marqueeEl.style.height = `${box.h}px`;
    const hits: string[] = [];
    for (const [key, item] of shortcutRects()) {
      if (boxesIntersect(box, item)) hits.push(key);
    }
    selectedKeys.clear();
    if (marquee.additive) {
      // Shift adds to the selection that existed before this gesture.
      for (const key of previousSelection) selectedKeys.add(key);
    }
    for (const key of hits) selectedKeys.add(key);
    renderSelection();
  }

  function finishMarquee(): void {
    if (!marquee) return;
    marquee = null;
    marqueeEl.hidden = true;
    document.documentElement.classList.remove("is-selecting");
  }

  let previousSelection = new Set<string>();
  const onSelectionPointerDown = (event: PointerEvent): void => {
    previousSelection = new Set(selectedKeys);
    onDesktopPointerDown(event);
  };
  desktop.addEventListener("pointerdown", onSelectionPointerDown);
  window.addEventListener("pointermove", onDesktopPointerMove);
  window.addEventListener("pointerup", finishMarquee);
  window.addEventListener("pointercancel", finishMarquee);
  const onSelectionKey = (event: KeyboardEvent): void => {
    if (disposed) return;
    const active = document.activeElement;
    const desktopFocused =
      active === null ||
      active === document.body ||
      active === desktop ||
      (desktop.contains(active) && !(active instanceof Element && active.closest(".window")));
    if ((event.ctrlKey || event.metaKey) && (event.key === "a" || event.key === "A") && desktopFocused) {
      event.preventDefault();
      selectedKeys.clear();
      for (const key of shortcutRects().keys()) selectedKeys.add(key);
      renderSelection();
      return;
    }
    if (event.key === "Delete" || event.key === "Backspace") {
      const editing =
        active instanceof HTMLElement &&
        (active.closest("input, textarea, select, [contenteditable='true']") !== null ||
          active.isContentEditable);
      if (desktopFocused && !editing && selectedKeys.size > 0) {
        event.preventDefault();
        void removeSelection();
      }
      return;
    }
    if (event.key === "Escape") {
      if (marquee) finishMarquee();
      if (desktopFocused && selectedKeys.size > 0) {
        event.preventDefault();
        clearSelection();
      }
    }
  };
  document.addEventListener("keydown", onSelectionKey);
  disposers.push(() => {
    desktop.removeEventListener("pointerdown", onSelectionPointerDown);
    window.removeEventListener("pointermove", onDesktopPointerMove);
    window.removeEventListener("pointerup", finishMarquee);
    window.removeEventListener("pointercancel", finishMarquee);
    document.removeEventListener("keydown", onSelectionKey);
  });
  widgetLayerApi = widgets;

  // ------------------------------------------------ desktop group dragging
  const groupPreview = h("div", { class: "desktop-group-preview", ariaHidden: true, hidden: true });
  desktop.appendChild(groupPreview);
  disposers.push(() => groupPreview.remove());
  interface GroupDragState {
    startX: number;
    startY: number;
    keys: string[];
    rects: Map<string, Box>;
    obstacles: Box[];
    delta: { dx: number; dy: number };
    valid: boolean;
  }
  let groupDrag: GroupDragState | null = null;
  let suppressDesktopClickUntil = 0;

  const elementForSelection = (key: string): HTMLElement | null => {
    if (key.startsWith("icon:")) {
      return desktop.querySelector<HTMLElement>(`.desktop-icon[data-app="${key.slice(5)}"]`);
    }
    if (key.startsWith("widget:")) {
      return widgetLayer.querySelector<HTMLElement>(`.desktop-widget[data-widget="${key.slice(7)}"]`);
    }
    return null;
  };

  function setGroupTransform(delta: { dx: number; dy: number }): void {
    if (!groupDrag) return;
    for (const key of groupDrag.keys) {
      const element = elementForSelection(key);
      if (element) element.style.transform = `translate(${delta.dx}px, ${delta.dy}px)`;
    }
  }

  function resetGroupTransform(): void {
    if (groupDrag) {
      for (const key of groupDrag.keys) {
        const element = elementForSelection(key);
        if (element) element.style.transform = "";
      }
    }
    groupPreview.hidden = true;
  }

  function finishGroupDrag(apply: boolean): void {
    const state = groupDrag;
    if (!state) return;
    groupDrag = null;
    window.removeEventListener("pointermove", onGroupMove);
    window.removeEventListener("pointerup", onGroupUp);
    window.removeEventListener("pointercancel", onGroupCancel);
    document.removeEventListener("keydown", onGroupKey, true);
    document.documentElement.classList.remove("is-selecting");
    suppressDesktopClickUntil = Date.now() + 450;
    if (!apply || !state.valid || (state.delta.dx === 0 && state.delta.dy === 0)) {
      resetGroupTransform();
      return;
    }
    void (async () => {
      const iconMap = new Map(desktopIcons.rects());
      const widgetMap = new Map(widgets.rectEntries().map((entry) => [entry.id, entry.rect]));
      const nextIcons = desktopIcons.snapshot().map((icon) => {
        const rect = iconMap.get(icon.id);
        if (!state.keys.includes(`icon:${icon.id}`) || !rect) return icon;
        return { ...icon, gridVersion: 2 as const, pxX: rect.x + state.delta.dx, pxY: rect.y + state.delta.dy };
      });
      const nextWidgets = widgets.snapshot().map((widget) => {
        const rect = widgetMap.get(widget.id);
        if (!state.keys.includes(`widget:${widget.id}`) || !rect) return widget;
        return {
          ...widget,
          gridVersion: 2 as const,
          pxX: rect.x + state.delta.dx,
          pxY: rect.y + state.delta.dy,
        };
      });
      try {
        await appearance.saveDesktopLayout(nextWidgets, nextIcons);
        resetGroupTransform();
        renderSelection();
      } catch (error) {
        resetGroupTransform();
        toast.error(`Could not move the selection: ${errorMessage(error)}`);
      }
    })();
  }

  function onGroupMove(event: PointerEvent): void {
    if (!groupDrag) return;
    const startRects = [...groupDrag.rects.values()];
    const raw = clampGroupDelta(
      startRects,
      event.clientX - groupDrag.startX,
      event.clientY - groupDrag.startY,
      { x: 0, y: 0, w: iconLayer.clientWidth, h: iconLayer.clientHeight },
    );
    groupDrag.delta = raw;
    const moved = startRects.map((rect) => ({
      ...rect,
      x: rect.x + raw.dx,
      y: rect.y + raw.dy,
    }));
    groupDrag.valid = !groupCollides(moved, groupDrag.obstacles);
    setGroupTransform(raw);
    const left = Math.min(...moved.map((rect) => rect.x));
    const top = Math.min(...moved.map((rect) => rect.y));
    const right = Math.max(...moved.map((rect) => rect.x + rect.w));
    const bottom = Math.max(...moved.map((rect) => rect.y + rect.h));
    const layerOffsetX = iconLayer.offsetLeft;
    const layerOffsetY = iconLayer.offsetTop;
    groupPreview.style.left = `${layerOffsetX + left}px`;
    groupPreview.style.top = `${layerOffsetY + top}px`;
    groupPreview.style.width = `${right - left}px`;
    groupPreview.style.height = `${bottom - top}px`;
    groupPreview.classList.toggle("is-blocked", !groupDrag.valid);
    groupPreview.hidden = false;
  }

  function onGroupUp(): void {
    finishGroupDrag(true);
  }

  function onGroupCancel(): void {
    finishGroupDrag(false);
  }

  function onGroupKey(event: KeyboardEvent): void {
    if (event.key !== "Escape" || !groupDrag) return;
    event.preventDefault();
    event.stopPropagation();
    finishGroupDrag(false);
  }

  desktop.addEventListener(
    "pointerdown",
    (event) => {
      if (event.button !== 0 || disposed || selectedKeys.size <= 1) return;
      const target = event.target as Element;
      if (target.closest("button, input, select, textarea, .widget-resize, .context-menu")) return;
      const iconEl = target.closest<HTMLElement>(".desktop-icon[data-app]");
      const widgetEl = target.closest<HTMLElement>(".desktop-widget[data-widget]");
      const key = iconEl?.dataset.app
        ? `icon:${iconEl.dataset.app}`
        : widgetEl?.dataset.widget
          ? `widget:${widgetEl.dataset.widget}`
          : null;
      if (!key || !selectedKeys.has(key)) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      const iconRects = desktopIcons.rects();
      const widgetRects = new Map(widgets.rectEntries().map((entry) => [entry.id, entry.rect]));
      const rects = new Map<string, Box>();
      for (const selected of selectedKeys) {
        const rect = selected.startsWith("icon:")
          ? iconRects.get(selected.slice(5))
          : widgetRects.get(selected.slice(7) as never);
        if (rect) rects.set(selected, { ...rect });
      }
      const obstacles: Box[] = [];
      for (const [id, rect] of iconRects) if (!selectedKeys.has(`icon:${id}`)) obstacles.push(rect);
      for (const [id, rect] of widgetRects) if (!selectedKeys.has(`widget:${id}`)) obstacles.push(rect);
      groupDrag = {
        startX: event.clientX,
        startY: event.clientY,
        keys: [...selectedKeys],
        rects,
        obstacles,
        delta: { dx: 0, dy: 0 },
        valid: true,
      };
      document.documentElement.classList.add("is-selecting");
      window.addEventListener("pointermove", onGroupMove);
      window.addEventListener("pointerup", onGroupUp);
      window.addEventListener("pointercancel", onGroupCancel);
      document.addEventListener("keydown", onGroupKey, true);
    },
    true,
  );
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
    restore?: { rect?: WindowRect; maximized?: boolean; minimized?: boolean },
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
    closePanel();
    pushRecentApp(app.id);
    const openBefore = new Set(wm.list().map((win) => win.id));
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
            const windowId = mount.window.id;
            reopenListeners.set(windowId, callback);
            mount.onDispose(() => reopenListeners.delete(windowId));
          },
        };
        await app.mount(ctx);
      },
      { singleton: app.singleton, ...(restore ?? {}) },
    );
    if (openBefore.has(handle.id) && params !== undefined) reopenListeners.get(handle.id)?.(params);
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
  /** Open an installed app package through the normal app window path. */
  function openPackageApp(packageId: string): void {
    openApp(packageAppId(packageId));
  }

  const desktopActions: DesktopActions = {
    apps: visibleApps,
    openPackageApp,
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
      { label: "Applications", icon: "apps", onClick: () => openApp("launcher") },
      { label: "Reset icon layout", icon: "grid", onClick: () => desktopIcons.reset(), disabled: !desktopIcons.isCustomized() },
      "separator",
      { label: "Personalize…", icon: "wallpaper", onClick: () => showPersonalize() },
      { label: "Widgets…", icon: "activity", onClick: () => widgets.openCatalog() },
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
      ariaExpanded: false,
      dataset: { tip: "Start menu (Ctrl+Space)", panelTrigger: "start" },
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
    h("div", { class: "taskbar-tray" }, connectionButton, themeButton, clockButton, userButton),
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
    const anchorRect = anchor.getBoundingClientRect();
    const tipRect = taskbarTooltip.getBoundingClientRect();
    const viewportWidth = document.documentElement.clientWidth || window.innerWidth;
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
      "linuxapps.install": "Linux app install",
      "linuxapps.update": "Linux app update",
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
      if (win.appId.startsWith(LINUX_PREFIX) || !isAppInstalled(win.appId, store.get().permissions)) {
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

  // ----------------------------------------------------------- Linux apps
  // Linux apps shared with this account join the app list once loaded (the
  // server sends only the ones it may see). Windows are not restored after
  // sign-in: each one would start the app on the server.
  {
    let linuxTimer: number | undefined;
    const loadLinuxApps = async (): Promise<void> => {
      try {
        await fetchLinuxApps(deps.api);
      } catch {
        /* an older server, or Docker is off: no Linux apps */
      }
    };
    const scheduleLinuxApps = (): void => {
      if (linuxTimer !== undefined) window.clearTimeout(linuxTimer);
      linuxTimer = window.setTimeout(() => {
        linuxTimer = undefined;
        if (!disposed) void loadLinuxApps();
      }, 300);
    };
    disposers.push(
      onLinuxApps(() => {
        if (disposed) return;
        enforceEffectiveAccess();
        // Same apps, new names or icons: repaint anyway.
        desktopIcons.refresh();
        renderTaskbar();
      }),
    );
    disposers.push(realtime.on("linuxapps.changed", scheduleLinuxApps));
    disposers.push(realtime.onResync(scheduleLinuxApps));
    disposers.push(() => {
      if (linuxTimer !== undefined) window.clearTimeout(linuxTimer);
    });
    void loadLinuxApps();
  }

  // ------------------------------------------- selection-aware actions
  const selectionSummary = (): ReturnType<typeof summarizeSelection> =>
    summarizeSelection(selectedKeys);

  async function saveDesktopLayout(
    icons: ReturnType<typeof desktopIcons.snapshot>,
    widgetsState: ReturnType<typeof widgets.snapshot>,
  ): Promise<void> {
    await appearance.saveDesktopLayout(widgetsState, icons);
  }

  async function removeSelection(): Promise<void> {
    if (selectedKeys.size === 0) return;
    const iconsBefore = desktopIcons.snapshot();
    const widgetsBefore = widgets.snapshot();
    const iconIds = new Set(
      [...selectedKeys].filter((key) => key.startsWith("icon:")).map((key) => key.slice(5)),
    );
    const widgetIds = new Set(
      [...selectedKeys].filter((key) => key.startsWith("widget:")).map((key) => key.slice(7)),
    );
    const nextIcons = iconsBefore.filter((icon) => !iconIds.has(icon.id));
    const nextWidgets = widgetsBefore.filter((widget) => !widgetIds.has(widget.id as never));
    if (nextIcons.length === iconsBefore.length && nextWidgets.length === widgetsBefore.length) {
      selectedKeys.clear();
      renderSelection();
      return;
    }
    const count = selectedKeys.size;
    try {
      await saveDesktopLayout(nextIcons, nextWidgets);
      selectedKeys.clear();
      renderSelection();
      toast.success(`Removed ${count} item${count === 1 ? "" : "s"} from the desktop`, {
        action: {
          label: "Undo",
          onClick: () => {
            void saveDesktopLayout(iconsBefore, widgetsBefore).catch((error) => {
              toast.error(`Could not restore the desktop: ${errorMessage(error)}`);
            });
          },
        },
      });
    } catch (error) {
      toast.error(`Could not remove the selection: ${errorMessage(error)}`);
    }
  }

  function openSelectionMenu(x: number, y: number): void {
    const summary = selectionSummary();
    const pinnedAll = [...selectedKeys]
      .filter((key) => key.startsWith("icon:"))
      .every((key) => isPinned(key.slice(5)));
    const entries: ContextMenuEntry[] = [];
    if (summary.single && summary.shortcuts === 1) {
      const appId = [...selectedKeys][0]!.slice(5);
      entries.push(
        { label: "Open", icon: "arrowRight", onClick: () => openApp(appId) },
        isPinned(appId)
          ? { label: "Unpin from taskbar", icon: "unpin", onClick: () => unpinApp(appId) }
          : { label: "Pin to taskbar", icon: "pin", onClick: () => pinApp(appId) },
      );
    } else if (summary.allShortcuts && summary.shortcuts > 1) {
      entries.push({
        label: pinnedAll ? "Unpin all from taskbar" : "Pin all to taskbar",
        icon: pinnedAll ? "unpin" : "pin",
        onClick: () => {
          for (const key of selectedKeys) {
            const appId = key.slice(5);
            if (pinnedAll) unpinApp(appId);
            else pinApp(appId);
          }
        },
      });
    }
    if (summary.total > 0) {
      entries.push("separator", {
        label:
          summary.total === 1
            ? summary.widgets === 1
              ? "Remove widget from desktop"
              : "Remove from desktop"
            : `Remove ${summary.total} items from desktop`,
        icon: "close",
        onClick: () => void removeSelection(),
      });
    }
    entries.push("separator", {
      label: "Personalize…",
      icon: "wallpaper",
      onClick: () => showPersonalize(),
    });
    contextMenu(x, y, entries, { ariaLabel: "Desktop selection" });
  }

  desktop.addEventListener(
    "contextmenu",
    (event) => {
      if (disposed) return;
      const target = event.target as Element;
      const iconEl = target.closest<HTMLElement>(".desktop-icon[data-app]");
      const widgetEl = target.closest<HTMLElement>(".desktop-widget[data-widget]");
      const key = iconEl?.dataset.app
        ? `icon:${iconEl.dataset.app}`
        : widgetEl?.dataset.widget
          ? `widget:${widgetEl.dataset.widget}`
          : null;
      if (!key) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      if (!selectedKeys.has(key)) {
        selectedKeys.clear();
        selectedKeys.add(key);
        if (event.shiftKey) {
          /* Shift keeps the previous multi-selection and adds the item. */
          for (const previous of previousSelection) selectedKeys.add(previous);
        }
        renderSelection();
      }
      openSelectionMenu(event.clientX, event.clientY);
    },
    true,
  );

  desktop.addEventListener(
    "click",
    (event) => {
      if (Date.now() >= suppressDesktopClickUntil) return;
      const target = event.target as Element;
      if (target.closest(".desktop-icon, .desktop-widget")) {
        event.preventDefault();
        event.stopImmediatePropagation();
      }
    },
    true,
  );

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
  // Windows reopened after a refresh or re-login. Linux apps (Chrome) and
  // package apps are not in the registry yet at this point, so keep them
  // pending and retry when their catalogs load.
  const pendingRestore = new Map<
    string,
    { params?: unknown; rect?: WindowRect; maximized?: boolean; minimized?: boolean }
  >();
  const restoreFocusId = options.restore?.focusedAppId ?? null;
  if (options.restore) {
    for (const entry of options.restore.apps.slice(0, MAX_WINDOWS)) {
      pendingRestore.set(entry.appId, {
        params: entry.params,
        rect: entry.rect,
        maximized: entry.maximized,
        minimized: entry.minimized,
      });
    }
    disposers.push(onLinuxApps(() => attemptRestore()));
    disposers.push(onPackageApps(() => attemptRestore()));
    attemptRestore();
    window.setTimeout(() => attemptRestore(), 1_500);
  }

  function attemptRestore(): void {
    if (disposed || pendingRestore.size === 0) return;
    for (const [appId, state] of [...pendingRestore]) {
      const app = findApp(appId);
      if (!app) continue;
      pendingRestore.delete(appId);
      if (!isAppAllowed(appId, store.get().permissions, store.isAdmin())) continue;
      openApp(appId, state.params, {
        rect: state.rect,
        maximized: state.maximized,
        minimized: state.minimized,
      });
    }
    if (pendingRestore.size === 0 && restoreFocusId) {
      const focused = wm.list().find((win) => win.appId === restoreFocusId);
      if (focused) wm.focus(focused.id);
    }
  }

  renderTaskbar();
  updateTray();

  // Package Center app packages are first-class apps: load the installed ones
  // and keep Applications, desktop icons and the taskbar in sync.
  void refreshPackageApps(deps.api);
  const realtimeStore = getRealtime(deps.events);
  const offPackages = onPackageApps(() => {
    desktopIcons.refresh();
    renderTaskbar();
  });
  disposers.push(offPackages);
  disposers.push(
    realtimeStore.on("package.changed", () => {
      void refreshPackageApps(deps.api);
    }),
  );

  // Tiny debug handle used by integration checks and troubleshooting.
  (window as unknown as { rast?: unknown }).rast = { store, wm, api: deps.api, openApp, toast };

  function snapshot(): SessionSnapshot {
    const snapshotApps: SessionSnapshot["apps"] = [];
    for (const win of wm.list().slice(0, MAX_WINDOWS)) {
      const entry: SessionSnapshot["apps"][number] = { appId: win.appId, rect: win.rect };
      const params = windowParams.get(win.id);
      if (params !== undefined) entry.params = params;
      if (win.maximized) entry.maximized = true;
      if (win.minimized) entry.minimized = true;
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
