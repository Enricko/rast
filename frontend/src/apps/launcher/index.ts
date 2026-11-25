// Applications: the folder with every app this account may open. Click a tile
// to open it; drag it to the desktop or the taskbar, or use its menu to add or
// remove the desktop shortcut and the taskbar pin. Apps that are not
// installed live in App Center ("Get more apps").

import { APP_DRAG_TYPE, type AppContext, type AppDefinition } from "../../core/app";
import { h } from "../../core/dom";
import { appIconSvg, iconSvg } from "../../core/icons";
import { contextMenu, type ContextMenuEntry } from "../../core/ui";

export const LAUNCHER_ID = "launcher";

export const launcherApp: AppDefinition = {
  id: LAUNCHER_ID,
  name: "Applications",
  icon: "apps",
  description: "Every app you can open",
  singleton: true,
  defaultSize: { w: 720, h: 500 },
  minSize: { w: 320, h: 280 },
  mount(ctx) {
    mountLauncher(ctx);
  },
};

function mountLauncher(ctx: AppContext): void {
  ctx.setTitle("Applications");
  let query = "";
  let signature = "";

  const search = h("input", {
    class: "input input-search launcher-search",
    type: "search",
    placeholder: "Search apps",
    ariaLabel: "Search apps",
  });
  const moreButton = h(
    "button",
    { class: "btn btn-ghost btn-sm", type: "button", title: "Install more apps in App Center" },
    iconSvg("package", 14),
    h("span", {}, "Get more apps"),
  );
  moreButton.addEventListener("click", () => ctx.openApp("packages", { view: "all" }));
  const grid = h("div", { class: "launcher-grid", role: "list", ariaLabel: "Applications" });
  const empty = h("div", { class: "launcher-empty", hidden: true });
  const hint = h("div", { class: "launcher-hint" }, "Drag an app to the desktop or the taskbar.");

  ctx.root.classList.add("launcher-app");
  ctx.root.append(
    h("div", { class: "app-toolbar launcher-toolbar", role: "toolbar", ariaLabel: "Applications" }, search, h("div", { class: "app-toolbar-spacer" }), moreButton),
    h("div", { class: "launcher-body" }, grid, empty, hint),
  );

  const apps = (): AppDefinition[] => ctx.desktop.apps().filter((app) => app.id !== LAUNCHER_ID);

  function matches(app: AppDefinition): boolean {
    const needle = query.trim().toLowerCase();
    if (!needle) return true;
    return `${app.name} ${app.description}`.toLowerCase().includes(needle);
  }

  function menuItems(app: AppDefinition): ContextMenuEntry[] {
    const desktop = ctx.desktop;
    const items: ContextMenuEntry[] = [
      { label: "Open", icon: "arrowRight", onClick: () => ctx.openApp(app.id) },
      "separator",
      desktop.isOnDesktop(app.id)
        ? { label: "Remove from desktop", icon: "close", onClick: () => desktop.removeFromDesktop(app.id) }
        : { label: "Add to desktop", icon: "plus", onClick: () => desktop.addToDesktop(app.id) },
      desktop.isPinned(app.id)
        ? { label: "Unpin from taskbar", icon: "unpin", onClick: () => desktop.unpin(app.id) }
        : { label: "Pin to taskbar", icon: "pin", onClick: () => desktop.pin(app.id) },
    ];
    if (app.id !== "packages" && canOpenPackageCenter()) {
      items.push("separator", {
        label: "Show in App Center",
        icon: "package",
        onClick: () => ctx.openApp("packages", { app: app.id }),
      });
    }
    return items;
  }

  function canOpenPackageCenter(): boolean {
    return ctx.desktop.apps().some((app) => app.id === "packages");
  }

  function tile(app: AppDefinition): HTMLElement {
    const button = h(
      "button",
      {
        class: "launcher-tile",
        type: "button",
        title: app.description,
        ariaLabel: `${app.name} — ${app.description}`,
        dataset: { app: app.id },
      },
      h("span", { class: "launcher-tile-glyph" }, appIconSvg(app.id, app.icon, 32)),
      h("span", { class: "launcher-tile-name" }, app.name),
    );
    button.draggable = true;
    button.addEventListener("click", () => ctx.openApp(app.id));
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      contextMenu(event.clientX, event.clientY, menuItems(app), { ariaLabel: app.name });
    });
    button.addEventListener("keydown", (event) => {
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
        event.preventDefault();
        const rect = button.getBoundingClientRect();
        contextMenu(rect.left, rect.bottom, menuItems(app), { ariaLabel: app.name });
        return;
      }
      const tiles = [...grid.querySelectorAll<HTMLButtonElement>(".launcher-tile")];
      const index = tiles.indexOf(button);
      const columns = Math.max(1, getComputedStyle(grid).gridTemplateColumns.split(" ").length);
      const moves: Record<string, number> = { ArrowRight: 1, ArrowLeft: -1, ArrowDown: columns, ArrowUp: -columns };
      let next: number | null = null;
      if (event.key in moves) next = index + moves[event.key]!;
      else if (event.key === "Home") next = 0;
      else if (event.key === "End") next = tiles.length - 1;
      if (next === null) return;
      event.preventDefault();
      tiles[Math.max(0, Math.min(tiles.length - 1, next))]?.focus();
    });
    button.addEventListener("dragstart", (event) => {
      if (!event.dataTransfer) return;
      event.dataTransfer.setData(APP_DRAG_TYPE, app.id);
      event.dataTransfer.setData("text/plain", app.name);
      event.dataTransfer.effectAllowed = "copy";
      const glyph = button.querySelector<HTMLElement>(".launcher-tile-glyph");
      if (glyph) event.dataTransfer.setDragImage(glyph, glyph.offsetWidth / 2, glyph.offsetHeight / 2);
      button.classList.add("is-dragging");
    });
    button.addEventListener("dragend", () => button.classList.remove("is-dragging"));
    return h("div", { role: "listitem", class: "launcher-item" }, button);
  }

  function render(force = false): void {
    const all = apps();
    const shown = all.filter(matches);
    const nextSignature = `${query}|${shown.map((app) => app.id).join(",")}|${canOpenPackageCenter()}`;
    if (!force && nextSignature === signature) return;
    signature = nextSignature;
    const focused = (document.activeElement as HTMLElement | null)?.dataset?.app;
    grid.replaceChildren(...shown.map(tile));
    moreButton.hidden = !canOpenPackageCenter();
    empty.hidden = shown.length > 0;
    empty.textContent = all.length === 0 ? "No apps are available for your account." : `No apps match “${query.trim()}”.`;
    hint.hidden = shown.length === 0;
    if (focused) grid.querySelector<HTMLElement>(`[data-app="${focused}"]`)?.focus();
  }

  search.addEventListener("input", () => {
    query = search.value;
    render();
  });
  search.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      const first = apps().find(matches);
      if (first) ctx.openApp(first.id);
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      grid.querySelector<HTMLElement>(".launcher-tile")?.focus();
    }
  });

  // Installs, uninstalls and access changes show up right away.
  ctx.onDispose(ctx.store.subscribe(() => render()));
  render(true);
  window.setTimeout(() => search.focus(), 0);
}
