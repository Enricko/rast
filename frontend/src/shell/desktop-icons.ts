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
  layoutForSave,
  moveIcon,
  neighbour,
  placeIcon,
  readingOrder,
  removeIcon,
  type Cell,
  type FillOrder,
  type Grid,
} from "./icon-layout";

const DRAG_THRESHOLD = 5;
const TOUCH_DRAG_THRESHOLD = 8;
const LONG_PRESS_MS = 550;
/** Clicks right after a drag are the end of that drag, not a new click. */
const CLICK_GUARD_MS = 350;
const DEFAULT_CELL = { w: 104, h: 108 };
/** Space between neighbouring icons (their hover backgrounds never touch). */
const CELL_GAP = 4;
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

  const cellKey = (cell: Cell): string => `${cell.col},${cell.row}`;

  /** Cells occupied by widgets; icons must not be placed on them. */
  function blockedCells(): Set<string> {
    const blocked = new Set<string>();
    const rects = options.blockedRects?.() ?? [];
    if (rects.length === 0) return blocked;
    const overlaps = (a: { x: number; y: number; w: number; h: number }, b: typeof a): boolean =>
      a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;
    for (let col = 0; col < grid.cols; col += 1) {
      for (let row = 0; row < grid.rows; row += 1) {
        const rect = { x: col * cell.w, y: row * cell.h, w: cell.w, h: cell.h };
        if (rects.some((widget) => overlaps(widget, rect))) blocked.add(`${col},${row}`);
      }
    }
    return blocked;
  }

  /** Nearest cell that is neither occupied nor blocked; null when full. */
  function nearestAllowedCell(blocked: Set<string>, occupied: Set<string>, from: Cell): Cell | null {
    let best: Cell | null = null;
    let bestDistance = Number.POSITIVE_INFINITY;
    for (let col = 0; col < grid.cols; col += 1) {
      for (let row = 0; row < grid.rows; row += 1) {
        if (blocked.has(`${col},${row}`) || occupied.has(`${col},${row}`)) continue;
        const distance = (col - from.col) ** 2 + (row - from.row) ** 2;
        if (distance < bestDistance) {
          best = { col, row };
          bestDistance = distance;
        }
      }
    }
    return best;
  }

  function avoidBlocked(cells: DesktopIcon[]): DesktopIcon[] {
    const blocked = blockedCells();
    if (blocked.size === 0) return cells;
    const occupied = new Set(cells.map(cellKey));
    return cells.map((icon) => {
      if (!blocked.has(cellKey(icon))) return icon;
      const free = nearestAllowedCell(blocked, occupied, icon);
      if (!free) return icon;
      occupied.delete(cellKey(icon));
      occupied.add(cellKey(free));
      return { ...icon, ...free };
    });
  }

  function position(el: HTMLElement, at: Cell): void {
    // Positioned on the shared 8px grid (cell sizes are multiples of the grid).
    el.style.left = `${Math.round(at.col * cell.w)}px`;
    el.style.top = `${Math.round(at.row * cell.h)}px`;
    el.style.width = `${Math.round(cell.w - CELL_GAP)}px`;
    el.style.height = `${Math.round(cell.h - CELL_GAP)}px`;
  }

  function cellAt(clientX: number, clientY: number): Cell {
    const rect = layer.getBoundingClientRect();
    const col = Math.floor((clientX - rect.left) / cell.w);
    const row = Math.floor((clientY - rect.top) / cell.h);
    return {
      col: Math.max(0, Math.min(grid.cols - 1, col)),
      row: Math.max(0, Math.min(grid.rows - 1, row)),
    };
  }

  function showMarker(at: Cell): void {
    position(marker, at);
    marker.hidden = false;
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
    shown = avoidBlocked(
      displayLayout(
        savedLayout(),
        apps.map((app) => app.id),
        grid,
        order,
        shown,
      ),
    );
    const nextSignature = JSON.stringify([shown, grid, cell]);
    if (!force && nextSignature === signature) return;
    signature = nextSignature;
    const active = document.activeElement;
    const focusedId = active instanceof HTMLElement && layer.contains(active) ? (active.dataset.app ?? null) : null;
    layer.replaceChildren(marker);
    buttons.clear();
    const ordered = readingOrder(shown, order);
    if (!selectedId || !ordered.some((icon) => icon.id === selectedId)) selectedId = ordered[0]?.id ?? null;
    for (const icon of ordered) {
      const app = apps.find((candidate) => candidate.id === icon.id);
      if (!app) continue;
      const button = iconButton(app);
      position(button, icon);
      buttons.set(app.id, button);
      layer.appendChild(button);
    }
    updateSelection();
    if (focusedId) buttons.get(focusedId)?.focus();
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
  /** Show `next` now and save it; false (and nothing changes) when the saved desktop is unknown. */
  function apply(next: DesktopIcon[], before: DesktopIcon[]): boolean {
    if (!appearance.isKnown) {
      toast.error("Your saved desktop could not be loaded yet, so icons cannot be arranged. Try again in a moment.");
      render(true);
      return false;
    }
    const visible = options.visibleApps().map((app) => app.id);
    const layout = layoutForSave(next, savedLayout(), visible, before);
    shown = next;
    void appearance.saveDesktopIcons(layout).then((ok) => {
      if (ok || disposed) return;
      toast.warning("Could not save the desktop to your account. Showing the saved layout.");
      render(true);
    });
    render(true);
    return true;
  }

  function moveTo(id: string, target: Cell): void {
    const next = moveIcon(shown, id, target);
    if (next === shown) {
      render(true);
      return;
    }
    apply(next, shown);
  }

  function add(id: string, at: Cell | null = null): boolean {
    if (!appById(id)) return false;
    render();
    const next = placeIcon(shown, id, at, grid, order);
    if (!next) {
      toast.warning("The desktop is full. Remove an icon to make room.");
      return false;
    }
    if (next === shown) return true;
    return apply(next, shown);
  }

  /** Remove an icon; `focusNext` (keyboard) moves focus to the icon that followed it. */
  function remove(id: string, focusNext = false): void {
    const icon = shown.find((candidate) => candidate.id === id);
    if (!icon) return;
    const following = neighbour(shown, icon, order === "rows" ? "right" : "down") ?? neighbour(shown, icon, "up");
    if (!apply(removeIcon(shown, id), shown)) return;
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
    // Where in its cell the icon was grabbed, so it does not jump to the pointer.
    const grabX = startX - layerRect.left - icon.col * cell.w;
    const grabY = startY - layerRect.top - icon.row * cell.h;
    let dragging = false;
    let target: Cell = { col: icon.col, row: icon.row };
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
      // The cell under the dragged icon's centre.
      target = cellAt(moveEvent.clientX - grabX + cell.w / 2, moveEvent.clientY - grabY + cell.h / 2);
      showMarker(target);
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
        if (!commit || !endEvent) {
          render(true);
          return;
        }
        if (document.elementFromPoint(endEvent.clientX, endEvent.clientY)?.closest(".taskbar")) {
          render(true);
          options.dropOnTaskbar(app.id);
          return;
        }
        moveTo(app.id, target);
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
        const target = { col: icon.col + dc, row: icon.row + dr };
        if (target.col < 0 || target.row < 0 || target.col >= grid.cols || target.row >= grid.rows) return;
        moveTo(app.id, target);
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
    showMarker(cellAt(event.clientX, event.clientY));
  };
  const onDrop = (event: DragEvent): void => {
    if (!carriesApp(event) || !droppable(event)) return;
    event.preventDefault();
    hideMarker();
    const id = event.dataTransfer?.getData(APP_DRAG_TYPE) ?? "";
    if (id) add(id, cellAt(event.clientX, event.clientY));
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
