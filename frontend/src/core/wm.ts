// Desktop window manager: move, resize, minimize, maximize, snap, close, focus.
//
// Windows are positioned absolutely inside the desktop layer. On small screens
// they fill the available space (the shell behaves like a mobile app switcher).
// Dragging a titlebar near the top/left/right edge previews a snap zone; the
// drop tiles the window or maximizes it.

import { h } from "./dom";
import { appIconSvg, iconSvg } from "./icons";

export interface WindowRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface WindowInfo {
  id: string;
  appId: string;
  title: string;
  icon: string;
  minimized: boolean;
  maximized: boolean;
  tiled: "left" | "right" | null;
  focused: boolean;
}

export interface WindowHandle {
  id: string;
  appId: string;
  contentEl: HTMLElement;
  setTitle(title: string): void;
  setIcon(name: string): void;
  close(): void;
  minimize(): void;
  toggleMaximize(): void;
  focus(): void;
  isMinimized(): boolean;
  isMaximized(): boolean;
  isClosed(): boolean;
  onResize(callback: () => void): () => void;
}

interface WindowRecord {
  id: string;
  appId: string;
  appIcon: string;
  el: HTMLElement;
  titlebar: HTMLElement;
  titleEl: HTMLElement;
  iconEl: HTMLElement;
  contentEl: HTMLElement;
  minimized: boolean;
  maximized: boolean;
  tiled: "left" | "right" | null;
  prevRect: WindowRect | null;
  resizeCallbacks: Set<() => void>;
  closeCallbacks: Set<() => void>;
  cleanup: Array<() => void>;
}

export interface WindowManagerOptions {
  /** Height reserved at the bottom (taskbar) when the host spans the viewport. */
  taskbarHeight?: number;
  onChange?: (windows: WindowInfo[]) => void;
}

const MOBILE_BREAKPOINT = 820;
const DEFAULT_SIZE = { w: 940, h: 620 };
const MIN_SIZE = { w: 360, h: 260 };
const SNAP_THRESHOLD = 18;

let windowSeq = 0;

export class WindowManager {
  private readonly host: HTMLElement;
  private readonly records = new Map<string, WindowRecord>();
  private zCounter = 10;
  private focusedId: string | null = null;
  private readonly options: WindowManagerOptions;
  private mobileQuery: MediaQueryList | null = null;
  private readonly mobileHandler: () => void;
  private readonly resizeHandler: () => void;
  private resizeFrame: number | undefined;
  private cascade = 0;
  private snapPreview: HTMLElement | null = null;
  private snapZone: "left" | "right" | "top" | null = null;

  constructor(host: HTMLElement, options: WindowManagerOptions = {}) {
    this.host = host;
    this.options = options;
    this.mobileHandler = () => this.layoutAll();
    this.resizeHandler = () => {
      if (this.resizeFrame !== undefined) return;
      this.resizeFrame = window.requestAnimationFrame(() => {
        this.resizeFrame = undefined;
        this.layoutAll();
      });
    };
    if (typeof window.matchMedia === "function") {
      this.mobileQuery = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT}px)`);
      this.mobileQuery.addEventListener("change", this.mobileHandler);
    }
    window.addEventListener("resize", this.resizeHandler);
  }

  get isMobile(): boolean {
    return this.mobileQuery?.matches ?? window.innerWidth <= MOBILE_BREAKPOINT;
  }

  open(
    app: {
      id: string;
      name: string;
      icon: string;
      defaultSize?: { w: number; h: number };
      minSize?: { w: number; h: number };
    },
    params: unknown,
    mount: (ctx: {
      contentEl: HTMLElement;
      window: WindowHandle;
      onResize: (cb: () => void) => () => void;
      onDispose: (cb: () => void) => () => void;
    }) => void | Promise<void>,
    options: { singleton?: boolean } = {},
  ): WindowHandle {
    if (options.singleton) {
      const existing = this.findByApp(app.id);
      if (existing) {
        if (existing.minimized) this.restore(existing.id);
        this.focus(existing.id);
        return this.handle(existing);
      }
    }

    void params;
    const id = `win-${++windowSeq}`;
    const el = h("section", { class: "window entering", role: "dialog", ariaLabel: app.name });
    el.dataset.windowId = id;
    el.dataset.appId = app.id;
    el.tabIndex = -1;

    const iconEl = h("span", { class: "win-icon", dataset: { app: app.id } });
    iconEl.appendChild(appIconSvg(app.id, app.icon, 16));
    const titleEl = h("span", { class: "win-title" }, app.name);
    const titlebar = h(
      "header",
      { class: "win-titlebar" },
      iconEl,
      titleEl,
      h(
        "div",
        { class: "win-controls" },
        this.controlButton("minimize", "Minimize", () => this.minimize(id)),
        this.controlButton("maximize", "Maximize", () => this.toggleMaximize(id)),
        this.controlButton("close", "Close", () => this.close(id)),
      ),
    );

    const contentEl = h("div", { class: "win-body" });
    el.appendChild(titlebar);
    el.appendChild(contentEl);
    for (const dir of ["n", "s", "e", "w", "ne", "nw", "se", "sw"]) {
      el.appendChild(h("div", { class: `win-resize win-resize-${dir}`, dataset: { dir } }));
    }

    const record: WindowRecord = {
      id,
      appId: app.id,
      appIcon: app.icon,
      el,
      titlebar,
      titleEl,
      iconEl,
      contentEl,
      minimized: false,
      maximized: false,
      tiled: null,
      prevRect: null,
      resizeCallbacks: new Set(),
      closeCallbacks: new Set(),
      cleanup: [],
    };
    this.records.set(id, record);
    this.host.appendChild(el);
    this.applyDefaultRect(record, app.defaultSize ?? DEFAULT_SIZE);

    // Drag and resize.
    record.cleanup.push(this.attachDrag(record));
    record.cleanup.push(this.attachResize(record, app));
    titlebar.addEventListener("dblclick", (event) => {
      if ((event.target as HTMLElement).closest(".win-control")) return;
      this.toggleMaximize(id);
    });
    el.addEventListener("pointerdown", () => this.focus(id), { capture: true });

    const notifyResize = () => {
      for (const cb of record.resizeCallbacks) {
        try {
          cb();
        } catch (error) {
          console.error("resize callback failed", error);
        }
      }
    };
    const resizeObserver = new ResizeObserver(() => notifyResize());
    resizeObserver.observe(contentEl);
    record.cleanup.push(() => resizeObserver.disconnect());

    const handle = this.handle(record);
    const onResize = (cb: () => void) => {
      record.resizeCallbacks.add(cb);
      return () => record.resizeCallbacks.delete(cb);
    };
    const onDispose = (cb: () => void) => {
      record.closeCallbacks.add(cb);
      return () => record.closeCallbacks.delete(cb);
    };

    this.focus(id);
    this.emitChange();
    requestAnimationFrame(() => el.classList.remove("entering"));

    // Mount the application asynchronously; errors are shown inside the window.
    Promise.resolve(mount({ contentEl, window: handle, onResize, onDispose })).catch((error) => {
      console.error(`app ${app.id} failed to mount`, error);
      contentEl.replaceChildren(
        h(
          "div",
          { class: "error-state" },
          h("div", { class: "error-title" }, "Application failed to start"),
          h("div", { class: "error-detail" }, String(error instanceof Error ? error.message : error)),
        ),
      );
    });

    // Re-apply mobile layout when the window was opened on a small screen.
    this.layout(record);
    return handle;
  }

  destroy(): void {
    for (const record of this.records.values()) {
      for (const cb of record.closeCallbacks) {
        try {
          cb();
        } catch (error) {
          console.error("dispose callback failed", error);
        }
      }
      for (const cleanup of record.cleanup) {
        try {
          cleanup();
        } catch {
          /* ignore */
        }
      }
      record.el.remove();
    }
    this.records.clear();
    this.snapPreview?.remove();
    this.snapPreview = null;
    window.removeEventListener("resize", this.resizeHandler);
    this.mobileQuery?.removeEventListener("change", this.mobileHandler);
    this.emitChange();
  }

  private controlButton(
    kind: "minimize" | "maximize" | "close",
    label: string,
    action: () => void,
  ): HTMLElement {
    const btn = h("button", { class: `win-control win-control-${kind}`, type: "button", ariaLabel: label });
    btn.appendChild(iconSvg(kind === "maximize" ? "maximize" : kind, 14));
    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      action();
    });
    btn.addEventListener("pointerdown", (event) => event.stopPropagation());
    return btn;
  }

  private handle(record: WindowRecord): WindowHandle {
    const manager = this;
    return {
      id: record.id,
      appId: record.appId,
      contentEl: record.contentEl,
      setTitle(title: string) {
        record.titleEl.textContent = title;
        record.el.setAttribute("aria-label", title);
        manager.emitChange();
      },
      setIcon(name: string) {
        record.iconEl.replaceChildren(iconSvg(name, 18));
        record.appIcon = name;
        manager.emitChange();
      },
      close() {
        manager.close(record.id);
      },
      minimize() {
        manager.minimize(record.id);
      },
      toggleMaximize() {
        manager.toggleMaximize(record.id);
      },
      focus() {
        manager.focus(record.id);
      },
      isMinimized() {
        return record.minimized;
      },
      isMaximized() {
        return record.maximized;
      },
      isClosed() {
        return !manager.records.has(record.id);
      },
      onResize(cb: () => void) {
        record.resizeCallbacks.add(cb);
        return () => record.resizeCallbacks.delete(cb);
      },
    };
  }

  private findByApp(appId: string): WindowRecord | undefined {
    for (const record of this.records.values()) {
      if (record.appId === appId) return record;
    }
    return undefined;
  }

  findWindow(appId: string): WindowHandle | null {
    const record = this.findByApp(appId);
    return record ? this.handle(record) : null;
  }

  focusedAppId(): string | null {
    if (!this.focusedId) return null;
    return this.records.get(this.focusedId)?.appId ?? null;
  }

  focus(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    this.focusedId = id;
    this.zCounter += 1;
    record.el.style.zIndex = String(this.zCounter);
    for (const other of this.records.values()) {
      other.el.classList.toggle("is-focused", other.id === id);
    }
    this.emitChange();
  }

  private restore(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    record.minimized = false;
    record.el.classList.remove("is-minimized");
    this.emitChange();
  }

  minimize(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    if (record.minimized) return;
    record.minimized = true;
    record.el.classList.add("is-minimized");
    if (this.focusedId === id) {
      this.focusedId = null;
      this.focusTopmost();
    }
    this.emitChange();
  }

  minimizeAll(): void {
    for (const record of this.records.values()) {
      record.minimized = true;
      record.el.classList.add("is-minimized");
    }
    this.focusedId = null;
    this.emitChange();
  }

  toggleMinimize(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    if (record.minimized) {
      this.restore(id);
      this.focus(id);
    } else if (this.focusedId === id) {
      this.minimize(id);
    } else {
      this.focus(id);
    }
  }

  toggleMaximize(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    if (this.isMobile) {
      // On mobile windows are always fullscreen; toggling minimizes instead.
      this.minimize(id);
      return;
    }
    if (record.maximized) {
      record.maximized = false;
      record.el.classList.remove("is-maximized");
      record.tiled = null;
      record.el.classList.remove("is-tiled");
      if (record.prevRect) this.setRect(record, record.prevRect);
      record.prevRect = null;
    } else {
      record.prevRect = this.rectOf(record);
      record.maximized = true;
      record.tiled = null;
      record.el.classList.remove("is-tiled");
      record.el.classList.add("is-maximized");
      const bounds = this.bounds();
      this.setRect(record, { x: 0, y: 0, w: bounds.w, h: bounds.h });
    }
    record.resizeCallbacks.forEach((cb) => cb());
    this.emitChange();
  }

  close(id: string): void {
    const record = this.records.get(id);
    if (!record) return;
    for (const cb of record.closeCallbacks) {
      try {
        cb();
      } catch (error) {
        console.error("dispose callback failed", error);
      }
    }
    for (const cleanup of record.cleanup) {
      try {
        cleanup();
      } catch {
        /* ignore */
      }
    }
    this.records.delete(id);
    record.el.classList.add("closing");
    const el = record.el;
    window.setTimeout(() => el.remove(), 130);
    if (this.focusedId === id) {
      this.focusedId = null;
      this.focusTopmost();
    }
    this.emitChange();
  }

  closeAll(): void {
    for (const id of Array.from(this.records.keys())) this.close(id);
  }

  private focusTopmost(): void {
    let top: WindowRecord | null = null;
    for (const other of this.records.values()) {
      if (other.minimized) continue;
      if (!top || Number(other.el.style.zIndex) > Number(top.el.style.zIndex)) top = other;
    }
    if (top) this.focus(top.id);
  }

  list(): WindowInfo[] {
    return Array.from(this.records.values()).map((record) => ({
      id: record.id,
      appId: record.appId,
      title: record.titleEl.textContent ?? "",
      icon: record.appIcon,
      minimized: record.minimized,
      maximized: record.maximized,
      tiled: record.tiled,
      focused: this.focusedId === record.id,
    }));
  }

  /** Bring all windows back into the viewport after a viewport change. */
  layoutAll(): void {
    for (const record of this.records.values()) this.layout(record);
  }

  private layout(record: WindowRecord): void {
    if (this.isMobile) {
      if (!record.maximized) {
        record.prevRect = this.rectOf(record);
        record.maximized = true;
        record.tiled = null;
        record.el.classList.remove("is-tiled");
      }
      const bounds = this.bounds();
      record.el.classList.add("is-mobile", "is-maximized");
      this.setRect(record, { x: 0, y: 0, w: bounds.w, h: bounds.h });
      return;
    }
    if (record.el.classList.contains("is-mobile")) {
      record.el.classList.remove("is-mobile");
      if (record.maximized) {
        record.maximized = false;
        record.el.classList.remove("is-maximized");
        if (record.prevRect) this.setRect(record, record.prevRect);
        record.prevRect = null;
      }
    }
    if (record.maximized) {
      const bounds = this.bounds();
      this.setRect(record, { x: 0, y: 0, w: bounds.w, h: bounds.h });
      return;
    }
    // Keep windows reachable after a viewport resize.
    const bounds = this.bounds();
    const rect = this.rectOf(record);
    const w = Math.min(rect.w, bounds.w - 16);
    const h = Math.min(rect.h, bounds.h - 16);
    const x = Math.min(Math.max(rect.x, -w + 140), Math.max(8, bounds.w - w - 8));
    const y = Math.min(Math.max(rect.y, 0), Math.max(8, bounds.h - h - 8));
    if (x !== rect.x || y !== rect.y || w !== rect.w || h !== rect.h) {
      this.setRect(record, { x, y, w, h });
    }
  }

  private bounds(): { w: number; h: number } {
    const hostW = this.host.clientWidth;
    const hostH = this.host.clientHeight;
    if (hostW > 0 && hostH > 0) {
      return { w: Math.max(320, hostW), h: Math.max(240, hostH) };
    }
    const taskbar = this.options.taskbarHeight ?? 58;
    return {
      w: Math.max(320, window.innerWidth),
      h: Math.max(240, window.innerHeight - taskbar),
    };
  }

  private applyDefaultRect(record: WindowRecord, size: { w: number; h: number }): void {
    const bounds = this.bounds();
    const w = Math.min(size.w, Math.max(MIN_SIZE.w, bounds.w - 16));
    const h = Math.min(size.h, Math.max(MIN_SIZE.h, bounds.h - 16));
    const offset = (this.cascade % 7) * 26;
    this.cascade += 1;
    const x = Math.min(Math.max(8 + offset, 8), Math.max(8, bounds.w - w - 8));
    const y = Math.min(Math.max(8 + offset, 8), Math.max(8, bounds.h - h - 8));
    this.setRect(record, { x, y, w, h });
  }

  private rectOf(record: WindowRecord): WindowRect {
    return {
      x: parseFloat(record.el.style.left || "0"),
      y: parseFloat(record.el.style.top || "0"),
      w: parseFloat(record.el.style.width || "0"),
      h: parseFloat(record.el.style.height || "0"),
    };
  }

  private setRect(record: WindowRecord, rect: WindowRect): void {
    record.el.style.left = `${Math.round(rect.x)}px`;
    record.el.style.top = `${Math.round(rect.y)}px`;
    record.el.style.width = `${Math.round(rect.w)}px`;
    record.el.style.height = `${Math.round(rect.h)}px`;
  }

  // --------------------------------------------------------------- snapping

  private ensureSnapPreview(): HTMLElement {
    if (!this.snapPreview) {
      this.snapPreview = h("div", { class: "snap-preview", hidden: true });
      this.snapPreview.setAttribute("aria-hidden", "true");
      this.host.appendChild(this.snapPreview);
    }
    return this.snapPreview;
  }

  private updateSnap(clientX: number, clientY: number): void {
    const hostRect = this.host.getBoundingClientRect();
    let zone: "left" | "right" | "top" | null = null;
    if (clientY <= hostRect.top + SNAP_THRESHOLD) {
      zone = "top";
    } else if (clientX <= hostRect.left + SNAP_THRESHOLD) {
      zone = "left";
    } else if (clientX >= hostRect.right - SNAP_THRESHOLD) {
      zone = "right";
    }
    this.snapZone = zone;
    const preview = this.ensureSnapPreview();
    if (!zone) {
      preview.hidden = true;
      return;
    }
    const bounds = this.bounds();
    const rect: WindowRect =
      zone === "top"
        ? { x: 0, y: 0, w: bounds.w, h: bounds.h }
        : { x: zone === "left" ? 0 : Math.round(bounds.w / 2), y: 0, w: Math.round(bounds.w / 2), h: bounds.h };
    preview.hidden = false;
    preview.dataset.zone = zone;
    preview.style.left = `${rect.x + 6}px`;
    preview.style.top = `${rect.y + 6}px`;
    preview.style.width = `${Math.max(80, rect.w - 12)}px`;
    preview.style.height = `${Math.max(80, rect.h - 12)}px`;
  }

  private hideSnapPreview(): void {
    this.snapZone = null;
    if (this.snapPreview) this.snapPreview.hidden = true;
  }

  private applySnap(record: WindowRecord): void {
    const zone = this.snapZone;
    this.hideSnapPreview();
    if (!zone) return;
    const bounds = this.bounds();
    if (zone === "top") {
      if (!record.maximized) {
        record.prevRect = this.rectOf(record);
        record.maximized = true;
      }
      record.tiled = null;
      record.el.classList.remove("is-tiled");
      record.el.classList.add("is-maximized");
      this.setRect(record, { x: 0, y: 0, w: bounds.w, h: bounds.h });
    } else {
      record.maximized = false;
      record.el.classList.remove("is-maximized");
      record.tiled = zone;
      record.el.classList.add("is-tiled");
      record.prevRect = null;
      this.setRect(record, {
        x: zone === "left" ? 0 : Math.round(bounds.w / 2),
        y: 0,
        w: Math.round(bounds.w / 2),
        h: bounds.h,
      });
    }
    record.resizeCallbacks.forEach((cb) => cb());
    this.emitChange();
  }

  // ---------------------------------------------------------------- pointer

  private attachDrag(record: WindowRecord): () => void {
    let dragging = false;
    let moved = false;
    let startX = 0;
    let startY = 0;
    let originX = 0;
    let originY = 0;

    const onPointerDown = (event: PointerEvent) => {
      if (this.isMobile || record.maximized) return;
      if ((event.target as HTMLElement).closest(".win-control")) return;
      if (event.button !== 0) return;
      dragging = true;
      moved = false;
      startX = event.clientX;
      startY = event.clientY;
      const rect = this.rectOf(record);
      originX = rect.x;
      originY = rect.y;
      record.tiled = null;
      record.el.classList.remove("is-tiled");
      record.titlebar.setPointerCapture(event.pointerId);
      record.el.classList.add("is-dragging");
      event.preventDefault();
        };

    const onPointerMove = (event: PointerEvent) => {
      if (!dragging) return;
      if (!moved && Math.abs(event.clientX - startX) + Math.abs(event.clientY - startY) < 4) return;
      moved = true;
      this.updateSnap(event.clientX, event.clientY);
      if (this.snapZone) return;
      const bounds = this.bounds();
      const rect = this.rectOf(record);
      const maxX = bounds.w - 80;
      const maxY = bounds.h - 60;
      const x = Math.min(Math.max(originX + (event.clientX - startX), -rect.w + 120), maxX);
      const y = Math.min(Math.max(originY + (event.clientY - startY), 0), maxY);
      record.el.style.left = `${Math.round(x)}px`;
      record.el.style.top = `${Math.round(y)}px`;
    };

    const onPointerUp = (event: PointerEvent) => {
      if (!dragging) return;
      dragging = false;
      record.el.classList.remove("is-dragging");
      if (record.titlebar.hasPointerCapture(event.pointerId)) {
        record.titlebar.releasePointerCapture(event.pointerId);
      }
      if (moved) this.applySnap(record);
      this.hideSnapPreview();
    };

    const onKeyDown = (event: KeyboardEvent) => {
      if (!dragging) return;
      if (event.key === "Escape") {
        dragging = false;
        moved = false;
        this.hideSnapPreview();
        record.el.classList.remove("is-dragging");
      }
    };

    record.titlebar.addEventListener("pointerdown", onPointerDown);
    record.titlebar.addEventListener("pointermove", onPointerMove);
    record.titlebar.addEventListener("pointerup", onPointerUp);
    record.titlebar.addEventListener("pointercancel", onPointerUp);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      record.titlebar.removeEventListener("pointerdown", onPointerDown);
      record.titlebar.removeEventListener("pointermove", onPointerMove);
      record.titlebar.removeEventListener("pointerup", onPointerUp);
      record.titlebar.removeEventListener("pointercancel", onPointerUp);
      document.removeEventListener("keydown", onKeyDown);
    };
  }

  private attachResize(record: WindowRecord, app: { minSize?: { w: number; h: number } }): () => void {
    let active: string | null = null;
    let startX = 0;
    let startY = 0;
    let origin: WindowRect | null = null;
    const minW = app.minSize?.w ?? MIN_SIZE.w;
    const minH = app.minSize?.h ?? MIN_SIZE.h;

    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as HTMLElement;
      if (!target.classList.contains("win-resize")) return;
      if (this.isMobile || record.maximized || event.button !== 0) return;
      active = target.dataset.dir ?? null;
      if (!active) return;
      startX = event.clientX;
      startY = event.clientY;
      origin = this.rectOf(record);
      record.tiled = null;
      record.el.classList.remove("is-tiled");
      target.setPointerCapture(event.pointerId);
      record.el.classList.add("is-resizing");
      event.preventDefault();
      event.stopPropagation();
    };
    const onPointerMove = (event: PointerEvent) => {
      if (!active || !origin) return;
      const dx = event.clientX - startX;
      const dy = event.clientY - startY;
      let { x, y, w, h } = origin;
      if (active.includes("e")) w = Math.max(minW, origin.w + dx);
      if (active.includes("s")) h = Math.max(minH, origin.h + dy);
      if (active.includes("w")) {
        w = Math.max(minW, origin.w - dx);
        x = origin.x + (origin.w - w);
      }
      if (active.includes("n")) {
        h = Math.max(minH, origin.h - dy);
        y = origin.y + (origin.h - h);
      }
      this.setRect(record, { x, y, w, h });
    };
    const finish = (event: PointerEvent) => {
      if (!active) return;
      const target = event.target as HTMLElement;
      if (target.hasPointerCapture?.(event.pointerId)) target.releasePointerCapture(event.pointerId);
      active = null;
      origin = null;
      record.el.classList.remove("is-resizing");
      record.resizeCallbacks.forEach((cb) => cb());
    };

    record.el.addEventListener("pointerdown", onPointerDown);
    record.el.addEventListener("pointermove", onPointerMove);
    record.el.addEventListener("pointerup", finish);
    record.el.addEventListener("pointercancel", finish);
    return () => {
      record.el.removeEventListener("pointerdown", onPointerDown);
      record.el.removeEventListener("pointermove", onPointerMove);
      record.el.removeEventListener("pointerup", finish);
      record.el.removeEventListener("pointercancel", finish);
    };
  }

  private emitChange(): void {
    this.options.onChange?.(this.list());
  }
}
