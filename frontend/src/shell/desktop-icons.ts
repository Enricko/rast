// Desktop icons: each account arranges its own app shortcuts on a snap grid.
//
//   * drag an icon (mouse, pen or touch) to any cell; dropping it on another
//     icon swaps the two, dropping it on the taskbar pins the app;
//   * "Remove from desktop" (menu or Delete) hides the shortcut, with Undo;
//   * arrows move the selection, Alt+arrow moves the selected icon one cell;
//   * an app dragged out of the Applications folder lands on the cell under
//     the pointer.
//
// The layout is saved with the account's appearance (`desktopIcons`). Until
// the account arranges anything it is null: every app it may open, in order.

import { APP_DRAG_TYPE, type AppDefinition } from "../core/app";
import { MAX_DESKTOP_CELL, type AppearanceService, type DesktopIcon } from "../core/appearance";
import { h } from "../core/dom";
import { appIconSvg } from "../core/icons";
import type { ToastCenter } from "../core/toast";
import { contextMenu, type ContextMenuEntry } from "../core/ui";
import {
  displayLayout,
  neighbour,
  readingOrder,
  removeIcon,
  type Cell,
  type FillOrder,
  type Grid,
} from "./icon-layout";
import { GRID, nearestFreeRect, rectsIntersect, snap, type Bounds, type Rect } from "./snap-grid";

const DRAG_THRESHOLD = 5;
const TOUCH_DRAG_THRESHOLD = 8;
const LONG_PRESS_MS = 550;
/** Clicks right after a drag are the end of that drag, not a new click. */
const CLICK_GUARD_MS = 350;
const DEFAULT_CELL = { w: 104, h: 112 };
/** Fixed shortcut box, including its label and interaction area (CSS px). */
const ICON_W = 104;
const ICON_H = 112;
/** Space between neighbouring icons (their hover backgrounds never touch). */
const NOT_DROPPABLE = ".window, .taskbar, .menu-panel, .context-menu, .modal-backdrop, .desktop-widget, .toast";

export interface DesktopIconsOptions {
  /** The icon layer (`.desktop-icons`), sized to the free desktop area. */
  layer: HTMLElement;
  /** The whole desktop: receives drops from the Applications folder. */
  desktop: HTMLElement;
  appearance: AppearanceService;
  toast: ToastCenter;
  /** Apps this account may open now, in default order. */
  visibleApps(): AppDefinition[];
  openApp(appId: string): void;
  /** The icon's context menu (Open, pin, remove…). */
  menuItems(app: AppDefinition): ContextMenuEntry[];
  /** An icon was dropped on the taskbar. */
  dropOnTaskbar(appId: string): void;
  /** Rectangles (same layer coordinates) occupied by desktop widgets. */
  blockedRects?: () => Array<{ x: number; y: number; w: number; h: number }>;
}

export interface DesktopIcons {
  /** Show the current apps and saved layout (cheap when nothing changed). */
  refresh(): void;
  has(appId: string): boolean;
  /** Put an app on the desktop (at `cell` or the first free cell); false when it could not. */
  add(appId: string, cell?: Cell | null): boolean;
  remove(appId: string): void;
  /** Back to the default layout: every app, in order. */
  reset(): void;
  /** True once the account arranged its own desktop. */
  isCustomized(): boolean;
  dispose(): void;
}

interface Press {
  end(commit: boolean, event?: PointerEvent): void;
}

export function createDesktopIcons(options: DesktopIconsOptions): DesktopIcons {
  const { layer, desktop, appearance, toast } = options;
  const marker = h("div", { class: "desktop-icon-drop", ariaHidden: true, hidden: true });
  const gridOverlay = h("div", { class: "desktop-grid-overlay", ariaHidden: true, hidden: true });
  const buttons = new Map<string, HTMLButtonElement>();
  const disposers: Array<() => void> = [];
  let shown: DesktopIcon[] = [];
  let grid: Grid = { cols: 1, rows: 1 };
  let cell = { ...DEFAULT_CELL };
  let order: FillOrder = "columns";
  let selectedId: string | null = null;
  let highlighted = false;
  let signature = "";
  let press: Press | null = null;
  let arranging = false;
  let clickGuardUntil = 0;
  let disposed = false;
  let displayRects = new Map<string, Rect>();
  let migrating = false;

  /** The layout being saved, else the saved one; null = default layout. */
  const savedLayout = (): DesktopIcon[] | null =>
    appearance.isKnown ? (appearance.pendingDesktopIcons ?? appearance.value.desktopIcons) : null;

  const appById = (id: string): AppDefinition | undefined => options.visibleApps().find((app) => app.id === id);
  const guarded = (): boolean => Date.now() < clickGuardUntil;

  function measure(): void {
    const style = getComputedStyle(layer);
    const width = layer.clientWidth;
    const height = layer.clientHeight;
    const columns = Number.parseInt(style.getPropertyValue("--icon-columns"), 10);
    const cellH = Number.parseFloat(style.getPropertyValue("--icon-cell-h")) || DEFAULT_CELL.h;
    let cellW = Number.parseFloat(style.getPropertyValue("--icon-cell-w")) || DEFAULT_CELL.w;
    let cols: number;
    if (Number.isInteger(columns) && columns > 0) {
      // Phones: a fixed number of columns across the screen, filled row by row.
      cols = columns;
      if (width > 0) cellW = width / columns;
      order = "rows";
    } else {
      cols = Math.floor(width / cellW);
      order = "columns";
    }
    const limit = MAX_DESKTOP_CELL + 1;
    grid = {
      cols: Math.min(limit, Math.max(1, cols)),
      rows: Math.min(limit, Math.max(1, Math.floor(height / cellH))),
    };
    cell = { w: cellW, h: cellH };
  }

  const layerBounds = (): Bounds => ({ x: 0, y: 0, w: layer.clientWidth, h: layer.clientHeight });

  function iconRect(icon: DesktopIcon): Rect {
    if (icon.gridVersion === 2 && typeof icon.pxX === "number" && typeof icon.pxY === "number") {
      return { x: snap(icon.pxX), y: snap(icon.pxY), w: ICON_W, h: ICON_H };
    }
    return { x: icon.col * cell.w, y: icon.row * cell.h, w: ICON_W, h: ICON_H };
  }

  function clampIconRect(rect: Rect, area: Bounds): Rect {
    return {
      x: Math.max(area.x, Math.min(area.x + area.w - ICON_W, snap(rect.x))),
      y: Math.max(area.y, Math.min(area.y + area.h - ICON_H, snap(rect.y))),
      w: ICON_W,
      h: ICON_H,
    };
  }

  function widgetRects(): Rect[] {
    return options.blockedRects?.() ?? [];
  }

  function cellFor(rect: Rect): { col: number; row: number } {
    return {
      col: Math.max(0, Math.min(MAX_DESKTOP_CELL, Math.round(rect.x / Math.max(1, cell.w)))),
      row: Math.max(0, Math.min(MAX_DESKTOP_CELL, Math.round(rect.y / Math.max(1, cell.h)))),
    };
  }

  function toV2(icon: DesktopIcon, rect: Rect): DesktopIcon {
    return { ...icon, ...cellFor(rect), gridVersion: 2, pxX: snap(rect.x), pxY: snap(rect.y) };
  }

  /** Resolve preferred fine-grid rects against widgets and each other. */
  function resolveRects(icons: DesktopIcon[]): Map<string, Rect> {
    const area = layerBounds();
    const obstacles: Rect[] = [...widgetRects()];
    const mapped = new Map<string, Rect>();
    for (const icon of icons) {
      let rect = clampIconRect(iconRect(icon), area);
      if (obstacles.some((other) => rectsIntersect(other, rect))) {
        rect = nearestFreeRect(rect, { w: ICON_W, h: ICON_H }, area, obstacles) ?? rect;
      }
      obstacles.push(rect);
      mapped.set(icon.id, rect);
    }
    return mapped;
  }





  function showMarkerRect(rect: Rect, valid = true): void {
    marker.style.left = `${Math.round(rect.x)}px`;
    marker.style.top = `${Math.round(rect.y)}px`;
    marker.style.width = `${ICON_W - 4}px`;
    marker.style.height = `${ICON_H - 4}px`;
    marker.classList.toggle("is-blocked", !valid);
    marker.hidden = false;
  }

  function showGrid(): void {
    const area = layerBounds();
    gridOverlay.style.left = `${area.x}px`;
    gridOverlay.style.top = `${area.y}px`;
    gridOverlay.style.width = `${area.w}px`;
    gridOverlay.style.height = `${area.h}px`;
    gridOverlay.hidden = false;
  }

  function hideGrid(): void {
    gridOverlay.hidden = true;
  }

  function hideMarker(): void {
    marker.hidden = true;
  }

  function updateSelection(): void {
    for (const [id, button] of buttons) {
      const selected = id === selectedId;
      button.tabIndex = selected ? 0 : -1;
      button.classList.toggle("is-selected", selected && highlighted);
      button.setAttribute("aria-selected", String(selected));
    }
  }

  /** Make `id` the selected icon; `highlight` shows it (a click or keyboard focus). */
  function select(id: string, focus = true, highlight = true): void {
    const button = buttons.get(id);
    if (!button) return;
    selectedId = id;
    if (highlight) highlighted = true;
    updateSelection();
    if (focus) button.focus();
  }

  // ---------------------------------------------------------------- render
  function render(force = false): void {
    if (disposed || arranging) return;
    measure();
    const apps = options.visibleApps();
    const visible = new Set(apps.map((app) => app.id));
    const saved = savedLayout();
    let icons: DesktopIcon[];
    if (saved && saved.some((icon) => icon.gridVersion === 2)) {
      icons = saved.filter((icon) => visible.has(icon.id));
    } else if (saved) {
      // One-time migration of legacy row/column shortcuts to fine pixels,
      // preserving the visual position (cells were fixed CSS pixels).
      icons = saved.map((icon) => toV2(icon, iconRect(icon)));
      if (!migrating) {
        migrating = true;
        void appearance.saveDesktopIcons(icons).finally(() => {
          migrating = false;
        });
      }
    } else {
      // Tidy automatic arrangement for the initial layout, stored in memory
      // as fine-grid pixels until the account customizes it.
      icons = displayLayout(null, apps.map((app) => app.id), grid, order).map((icon) =>
        toV2(icon, iconRect(icon)),
      );
    }
    shown = icons;
    const rects = resolveRects(shown);
    displayRects = rects;
    const nextSignature = JSON.stringify(
      shown.map((icon) => [icon.id, rects.get(icon.id)]),
    );
    if (!force && nextSignature === signature) {
      updateSelection();
      return;
    }
    signature = nextSignature;
    const active = document.activeElement;
    const focusedId = active instanceof HTMLElement && layer.contains(active) ? (active.dataset.app ?? null) : null;
    layer.replaceChildren(marker, gridOverlay);
    buttons.clear();
    const ordered = shown.filter((icon) => apps.some((app) => app.id === icon.id));
    if (!selectedId || !ordered.some((icon) => icon.id === selectedId)) selectedId = ordered[0]?.id ?? null;
    for (const icon of ordered) {
      const app = apps.find((candidate) => candidate.id === icon.id);
      if (!app) continue;
      const button = iconButton(app);
      place(button, rects.get(icon.id) ?? iconRect(icon));
      buttons.set(app.id, button);
      layer.appendChild(button);
    }
    updateSelection();
    if (focusedId) buttons.get(focusedId)?.focus();
  }

  function place(el: HTMLElement, rect: Rect): void {
    el.style.left = `${Math.round(rect.x)}px`;
    el.style.top = `${Math.round(rect.y)}px`;
    el.style.width = `${ICON_W - 4}px`;
    el.style.height = `${ICON_H - 4}px`;
  }

  function iconButton(app: AppDefinition): HTMLButtonElement {
    const button = h(
      "button",
      {
        class: "desktop-icon",
        type: "button",
        role: "option",
        ariaLabel: `${app.name} — ${app.description}`,
        title: app.name,
        tabIndex: -1,
        dataset: { app: app.id },
      },
      h("span", { class: "desktop-icon-glyph" }, appIconSvg(app.id, app.icon, 30)),
      h("span", { class: "desktop-icon-label" }, app.name),
    );
    button.addEventListener("click", () => {
      if (!guarded()) select(app.id, false);
    });
    button.addEventListener("dblclick", () => {
      if (!guarded()) options.openApp(app.id);
    });
    // Focus that is not from the keyboard (e.g. after signing in) selects quietly.
    button.addEventListener("focus", () => select(app.id, false, button.matches(":focus-visible")));
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      // A long press on Android opens this menu: the press is over.
      press?.end(false);
      if (guarded()) return;
      select(app.id, false);
      openMenu(app, event.clientX, event.clientY);
    });
    button.addEventListener("keydown", (event) => onKey(event, app, button));
    button.addEventListener("pointerdown", (event) => startPress(event, app, button));
    return button;
  }

  function openMenu(app: AppDefinition, x: number, y: number): void {
    contextMenu(x, y, options.menuItems(app), { ariaLabel: app.name });
  }

  // ---------------------------------------------------------- arranging
  /** Persist the complete fine-grid layout, keeping hidden apps' shortcuts. */
  function apply(next: DesktopIcon[]): boolean {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet, so icons cannot be arranged. Try again in a moment.");
      render(true);
      return false;
    }
    const visible = new Set(options.visibleApps().map((app) => app.id));
    const previous = savedLayout() ?? [];
    const hidden = previous.filter(
      (icon) => !visible.has(icon.id) && !next.some((known) => known.id === icon.id),
    );
    const layout = [...next, ...hidden].map((icon) => {
      const rect = displayRects.get(icon.id) ?? iconRect(icon);
      return toV2(icon, rect);
    });
    shown = next;
    void appearance.saveDesktopIcons(layout).then((ok) => {
      if (ok || disposed) return;
      toast.warning("Could not save the desktop to your account. Showing the saved layout.");
      render(true);
    });
    render(true);
    return true;
  }

  function occupiedShortcutRects(except?: string): Rect[] {
    const out: Rect[] = [];
    for (const icon of shown) {
      if (icon.id === except) continue;
      const rect = displayRects.get(icon.id);
      if (rect) out.push(rect);
    }
    return out;
  }

  /** Nearest valid fine-grid rect for a shortcut at the desired position. */
  function freeRectFor(desired: Rect, except?: string): Rect | null {
    const area = layerBounds();
    const candidate = {
      x: Math.max(area.x, Math.min(area.x + area.w - ICON_W, snap(desired.x))),
      y: Math.max(area.y, Math.min(area.y + area.h - ICON_H, snap(desired.y))),
      w: ICON_W,
      h: ICON_H,
    };
    const obstacles = [...widgetRects(), ...occupiedShortcutRects(except)];
    if (!obstacles.some((other) => rectsIntersect(other, candidate))) return candidate;
    return nearestFreeRect(candidate, { w: ICON_W, h: ICON_H }, area, obstacles);
  }

  function moveToRect(id: string, rect: Rect): void {
    const next = shown.map((icon) => (icon.id === id ? toV2(icon, rect) : icon));
    apply(next);
  }

  function addAt(id: string, point?: { x: number; y: number }, cellHint?: Cell | null): boolean {
    if (!appById(id)) return false;
    render();
    const area = layerBounds();
    let desired: Rect;
    if (point) {
      desired = { x: point.x - ICON_W / 2, y: point.y - ICON_H / 2, w: ICON_W, h: ICON_H };
    } else if (cellHint) {
      desired = { x: cellHint.col * cell.w, y: cellHint.row * cell.h, w: ICON_W, h: ICON_H };
    } else {
      desired = { x: area.x, y: area.y, w: ICON_W, h: ICON_H };
    }
    const free = freeRectFor(desired);
    if (!free) {
      toast.warning("The desktop is full. Remove an icon to make room.");
      return false;
    }
    const next = [...shown, toV2({ id, col: 0, row: 0 }, free)];
    return apply(next);
  }

  function add(id: string, at: Cell | null = null): boolean {
    return addAt(id, undefined, at);
  }

  /** Remove an icon; `focusNext` (keyboard) moves focus to the icon that followed it. */
  function remove(id: string, focusNext = false): void {
    const icon = shown.find((candidate) => candidate.id === id);
    if (!icon) return;
    const following = neighbour(shown, icon, order === "rows" ? "right" : "down") ?? neighbour(shown, icon, "up");
    if (!apply(removeIcon(shown, id))) return;
    if (focusNext && following) select(following.id);
    const name = appById(id)?.name ?? "The app";
    toast.info(`${name} was removed from the desktop`, {
      action: { label: "Undo", onClick: () => void add(id, { col: icon.col, row: icon.row }) },
    });
  }

  function reset(): void {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet. Try again in a moment.");
      return;
    }
    void appearance.resetDesktopIcons().then((ok) => {
      if (disposed) return;
      if (!ok) toast.warning("Could not reset the desktop icons.");
      render(true);
    });
  }

  // ------------------------------------------------------- pointer drags
  function startPress(event: PointerEvent, app: AppDefinition, button: HTMLButtonElement): void {
    if (press || disposed) return;
    if (event.pointerType === "mouse" && event.button !== 0) return;
    const icon = shown.find((candidate) => candidate.id === app.id);
    if (!icon) return;
    const touch = event.pointerType === "touch";
    const threshold = touch ? TOUCH_DRAG_THRESHOLD : DRAG_THRESHOLD;
    const startX = event.clientX;
    const startY = event.clientY;
    const layerRect = layer.getBoundingClientRect();
    const startRect = displayRects.get(app.id) ?? iconRect(icon);
    // Pointer grab offset inside the shortcut, so it never jumps on drag start.
    const grabX = startX - layerRect.left - startRect.x;
    const grabY = startY - layerRect.top - startRect.y;
    let dragging = false;
    let targetRect: Rect | null = startRect;
    let longPress: number | undefined;
    try {
      button.setPointerCapture(event.pointerId);
    } catch {
      /* the pointer may already be gone */
    }

    const onMove = (moveEvent: PointerEvent): void => {
      if (moveEvent.pointerId !== event.pointerId) return;
      const dx = moveEvent.clientX - startX;
      const dy = moveEvent.clientY - startY;
      if (!dragging) {
        if (Math.hypot(dx, dy) < threshold) return;
        window.clearTimeout(longPress);
        dragging = true;
        arranging = true;
        layer.classList.add("is-arranging");
        button.classList.add("is-dragging");
        showGrid();
        select(app.id, false);
      }
      button.style.transform = `translate(${Math.round(dx)}px, ${Math.round(dy)}px)`;
      const overTaskbar = Boolean(
        document.elementFromPoint(moveEvent.clientX, moveEvent.clientY)?.closest(".taskbar"),
      );
      if (overTaskbar) {
        hideMarker();
        return;
      }
      const desired: Rect = {
        x: moveEvent.clientX - layerRect.left - grabX,
        y: moveEvent.clientY - layerRect.top - grabY,
        w: ICON_W,
        h: ICON_H,
      };
      const free = freeRectFor(desired, app.id);
      targetRect = free;
      showMarkerRect(free ?? desired, free !== null);
    };
    const onUp = (upEvent: PointerEvent): void => {
      if (upEvent.pointerId === event.pointerId) current.end(true, upEvent);
    };
    const onCancel = (cancelEvent: PointerEvent): void => {
      if (cancelEvent.pointerId === event.pointerId) current.end(false);
    };
    const onEscape = (keyEvent: KeyboardEvent): void => {
      if (keyEvent.key !== "Escape" || !dragging) return;
      keyEvent.preventDefault();
      keyEvent.stopPropagation();
      current.end(false);
    };

    const current: Press = {
      end(commit, endEvent) {
        if (press !== current) return;
        press = null;
        window.clearTimeout(longPress);
        button.removeEventListener("pointermove", onMove);
        button.removeEventListener("pointerup", onUp);
        button.removeEventListener("pointercancel", onCancel);
        document.removeEventListener("keydown", onEscape, true);
        if (!dragging) {
          // A short tap opens the app on touch screens (no double tap there).
          if (commit && touch && endEvent?.type === "pointerup") {
            clickGuardUntil = Date.now() + CLICK_GUARD_MS * 2;
            options.openApp(app.id);
          }
          return;
        }
        clickGuardUntil = Date.now() + CLICK_GUARD_MS;
        arranging = false;
        layer.classList.remove("is-arranging");
        button.classList.remove("is-dragging");
        button.style.transform = "";
        hideMarker();
        hideGrid();
        if (!commit || !endEvent) {
          render(true);
          return;
        }
        if (document.elementFromPoint(endEvent.clientX, endEvent.clientY)?.closest(".taskbar")) {
          render(true);
          options.dropOnTaskbar(app.id);
          return;
        }
        if (targetRect) {
          moveToRect(app.id, targetRect);
        } else {
          render(true);
        }
      },
    };
    press = current;
    if (touch) {
      // Hold without moving: the icon's menu (browsers without a long-press
      // `contextmenu`, such as Safari on iOS).
      longPress = window.setTimeout(() => {
        if (press !== current || dragging) return;
        current.end(false);
        clickGuardUntil = Date.now() + CLICK_GUARD_MS;
        select(app.id, false);
        openMenu(app, startX, startY);
      }, LONG_PRESS_MS);
    }
    button.addEventListener("pointermove", onMove);
    button.addEventListener("pointerup", onUp);
    button.addEventListener("pointercancel", onCancel);
    document.addEventListener("keydown", onEscape, true);
  }

  // ------------------------------------------------------------ keyboard
  function onKey(event: KeyboardEvent, app: AppDefinition, button: HTMLButtonElement): void {
    const icon = shown.find((candidate) => candidate.id === app.id);
    if (!icon) return;
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      options.openApp(app.id);
      return;
    }
    if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
      event.preventDefault();
      const rect = button.getBoundingClientRect();
      openMenu(app, rect.left, rect.bottom);
      return;
    }
    if (event.key === "Delete") {
      event.preventDefault();
      remove(app.id, true);
      return;
    }
    const directions: Record<string, ["up" | "down" | "left" | "right", number, number]> = {
      ArrowUp: ["up", 0, -1],
      ArrowDown: ["down", 0, 1],
      ArrowLeft: ["left", -1, 0],
      ArrowRight: ["right", 1, 0],
    };
    const direction = directions[event.key];
    if (direction) {
      event.preventDefault();
      const [name, dc, dr] = direction;
      if (event.altKey) {
        const current = displayRects.get(app.id) ?? iconRect(icon);
        const step = event.shiftKey ? GRID * 8 : GRID;
        const desired = {
          x: current.x + dc * step,
          y: current.y + dr * step,
          w: ICON_W,
          h: ICON_H,
        };
        const free = freeRectFor(desired, app.id);
        if (free) moveToRect(app.id, free);
        return;
      }
      const next = neighbour(shown, icon, name);
      if (next) select(next.id);
      return;
    }
    if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const ordered = readingOrder(shown, order);
      const next = event.key === "Home" ? ordered[0] : ordered[ordered.length - 1];
      if (next) select(next.id);
    }
  }

  // ------------------------------------------- drops from the Applications folder
  const carriesApp = (event: DragEvent): boolean => Boolean(event.dataTransfer?.types.includes(APP_DRAG_TYPE));
  const droppable = (event: DragEvent): boolean =>
    !(event.target instanceof Element && event.target.closest(NOT_DROPPABLE));
  const onDragOver = (event: DragEvent): void => {
    if (!carriesApp(event)) return;
    if (!droppable(event)) {
      hideMarker();
      return;
    }
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    const layerRect = layer.getBoundingClientRect();
    const desired: Rect = {
      x: event.clientX - layerRect.left - ICON_W / 2,
      y: event.clientY - layerRect.top - ICON_H / 2,
      w: ICON_W,
      h: ICON_H,
    };
    const free = freeRectFor(desired);
    showMarkerRect(free ?? desired, free !== null);
  };
  const onDrop = (event: DragEvent): void => {
    if (!carriesApp(event) || !droppable(event)) return;
    event.preventDefault();
    hideMarker();
    const id = event.dataTransfer?.getData(APP_DRAG_TYPE) ?? "";
    if (id) {
      const layerRect = layer.getBoundingClientRect();
      addAt(id, {
        x: event.clientX - layerRect.left,
        y: event.clientY - layerRect.top,
      });
    }
  };
  const onDragLeave = (event: DragEvent): void => {
    const next = event.relatedTarget;
    if (!(next instanceof Node) || !desktop.contains(next)) hideMarker();
  };
  desktop.addEventListener("dragover", onDragOver);
  desktop.addEventListener("drop", onDrop);
  desktop.addEventListener("dragleave", onDragLeave);
  document.addEventListener("dragend", hideMarker);
  disposers.push(() => {
    desktop.removeEventListener("dragover", onDragOver);
    desktop.removeEventListener("drop", onDrop);
    desktop.removeEventListener("dragleave", onDragLeave);
    document.removeEventListener("dragend", hideMarker);
  });

  // Clicking anywhere else drops the selection highlight (focus order stays).
  const onDesktopPointerDown = (event: PointerEvent): void => {
    if (event.target instanceof Element && event.target.closest(".desktop-icon, .context-menu")) return;
    if (!highlighted) return;
    highlighted = false;
    updateSelection();
  };
  desktop.addEventListener("pointerdown", onDesktopPointerDown);
  disposers.push(() => desktop.removeEventListener("pointerdown", onDesktopPointerDown));

  if (typeof ResizeObserver === "function") {
    const observer = new ResizeObserver(() => render());
    observer.observe(layer);
    disposers.push(() => observer.disconnect());
  } else {
    const onResize = (): void => render();
    window.addEventListener("resize", onResize);
    disposers.push(() => window.removeEventListener("resize", onResize));
  }

  render(true);

  return {
    refresh: () => render(),
    has: (appId) => shown.some((icon) => icon.id === appId),
    add: (appId, at) => add(appId, at ?? null),
    remove: (appId) => remove(appId),
    reset,
    isCustomized: () => savedLayout() !== null,
    dispose() {
      disposed = true;
      press?.end(false);
      for (const dispose of disposers.splice(0)) dispose();
    },
  };
}
