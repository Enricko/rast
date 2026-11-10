// Desktop wallpapers: original CSS gradient compositions (no bitmap assets),
// persisted per browser so the desktop feels personal without a server round trip.

import { h } from "../core/dom";

export type WallpaperId = "aurora" | "nebula" | "tide" | "graphite" | "dawn";

export interface Wallpaper {
  id: WallpaperId;
  label: string;
  detail: string;
}

export const WALLPAPERS: ReadonlyArray<Wallpaper> = [
  { id: "aurora", label: "Aurora", detail: "Deep navy with aurora blue and teal light" },
  { id: "nebula", label: "Nebula", detail: "Violet and indigo clouds" },
  { id: "tide", label: "Tide", detail: "Cool ocean gradient with mint glow" },
  { id: "graphite", label: "Graphite", detail: "Low-contrast slate, easy on the eyes" },
  { id: "dawn", label: "Dawn", detail: "Warm sunrise gradient" },
];

const STORAGE_KEY = "rast.wallpaper";

export function isWallpaperId(value: string): value is WallpaperId {
  return WALLPAPERS.some((wallpaper) => wallpaper.id === value);
}

export function readWallpaper(): WallpaperId {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    if (stored && isWallpaperId(stored)) return stored;
  } catch {
    /* storage disabled */
  }
  return "aurora";
}

export function applyWallpaper(desktop: HTMLElement, id: WallpaperId): void {
  desktop.dataset.wallpaper = id;
  try {
    window.localStorage.setItem(STORAGE_KEY, id);
  } catch {
    /* ignore */
  }
}

/** Wallpaper swatches for the "Change wallpaper" dialog. */
export function buildWallpaperPicker(current: WallpaperId, onPick: (id: WallpaperId) => void): HTMLElement {
  const grid = h("div", { class: "wallpaper-grid", role: "listbox", ariaLabel: "Desktop wallpaper" });
  for (const wallpaper of WALLPAPERS) {
    const selected = wallpaper.id === current;
    const btn = h(
      "button",
      {
        class: `wallpaper-swatch${selected ? " is-selected" : ""}`,
        type: "button",
        role: "option",
        ariaSelected: selected,
        onClick: () => onPick(wallpaper.id),
      },
      h("span", { class: `wallpaper-preview wp-${wallpaper.id}`, ariaHidden: true }),
      h(
        "span",
        { class: "wallpaper-meta" },
        h("span", { class: "wallpaper-label" }, wallpaper.label),
        h("span", { class: "wallpaper-detail small muted" }, wallpaper.detail),
      ),
      selected ? h("span", { class: "wallpaper-check", ariaHidden: true }, "✓") : null,
    );
    btn.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
      event.preventDefault();
      const buttons = Array.from(grid.querySelectorAll<HTMLButtonElement>(".wallpaper-swatch"));
      const index = buttons.indexOf(btn);
      const delta = event.key === "ArrowDown" ? 1 : -1;
      buttons[(index + delta + buttons.length) % buttons.length]?.focus();
    });
    grid.appendChild(btn);
  }
  return grid;
}
