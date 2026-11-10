// Start menu, user menu, connection-status panel and clock popover.

import { appsForRole } from "../apps/registry";
import { segmented } from "../core/components";
import { h } from "../core/dom";
import {
  formatClockSeconds,
  formatDateTime,
  formatWeekdayDate,
  relativeTime,
} from "../core/format";
import { iconSvg } from "../core/icons";
import type { ThemePreference } from "../core/store";
import { ApiError } from "../core/api";
import type { ShellDeps } from "./shared";

export interface PanelContext {
  deps: ShellDeps;
  desktop: HTMLElement;
  /** Milliseconds to add to Date.now() to get server time. */
  serverOffset: number;
  openApp(id: string, params?: unknown): void;
  openAccount(tab: string, section?: string): void;
  requestSignOut(): void;
  showShortcuts(): void;
  showAbout(): void;
  showWallpaper(): void;
  notify(message: string, kind: "info" | "success" | "warning" | "error"): void;
}

const RECENT_KEY = "rast.recentApps";
const MAX_RECENT = 5;

export function readRecentApps(): string[] {
  try {
    const raw = window.localStorage.getItem(RECENT_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((value): value is string => typeof value === "string").slice(0, MAX_RECENT);
  } catch {
    return [];
  }
}

export function pushRecentApp(appId: string): void {
  try {
    const recent = [appId, ...readRecentApps().filter((id) => id !== appId)].slice(0, MAX_RECENT);
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(recent));
  } catch {
    /* ignore */
  }
}

export function menuItem(
  icon: string,
  label: string,
  onClick: () => void,
  options: { danger?: boolean; shortcut?: string; disabled?: boolean } = {},
): HTMLButtonElement {
  const btn = h(
    "button",
    {
      class: `menu-item${options.danger ? " menu-item-danger" : ""}`,
      type: "button",
      onClick,
    },
    iconSvg(icon, 16),
    h("span", { class: "menu-item-label" }, label),
    options.shortcut ? h("span", { class: "menu-item-shortcut" }, options.shortcut) : null,
  );
  if (options.disabled) btn.disabled = true;
  return btn;
}

export function panelHeader(title: string, detail?: string): HTMLElement {
  return h(
    "div",
    { class: "panel-header" },
    h("div", { class: "panel-header-title" }, title),
    detail ? h("div", { class: "panel-header-detail small muted" }, detail) : null,
  );
}

function appTile(
  app: { id: string; name: string; icon: string; description: string },
  open: () => void,
): HTMLButtonElement {
  const tile = h(
    "button",
    { class: "menu-app", type: "button", onClick: open },
    h("span", { class: "menu-app-glyph" }, iconSvg(app.icon, 22)),
    h(
      "span",
      { class: "menu-app-text" },
      h("span", { class: "menu-app-name" }, app.name),
      h("span", { class: "menu-app-desc" }, app.description),
    ),
  );
  return tile;
}

function installGridKeys(container: HTMLElement, tiles: HTMLButtonElement[]): void {
  container.addEventListener("keydown", (event) => {
    const target = event.target as HTMLElement;
    if (!target.classList.contains("menu-app")) return;
    const index = tiles.indexOf(target as HTMLButtonElement);
    if (index < 0) return;
    if (!["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const columns = Math.max(1, getComputedStyle(container).gridTemplateColumns.split(" ").length);
    let next: number;
    if (event.key === "ArrowRight") next = index + 1;
    else if (event.key === "ArrowLeft") next = index - 1;
    else if (event.key === "ArrowDown") next = index + columns;
    else if (event.key === "ArrowUp") next = index - columns;
    else if (event.key === "Home") next = 0;
    else next = tiles.length - 1;
    if (next < 0) next = 0;
    if (next >= tiles.length) next = tiles.length - 1;
    tiles[next]?.focus();
  });
}

export function buildStartMenu(ctx: PanelContext, onClose: () => void): HTMLElement {
  const isAdmin = ctx.deps.store.isAdmin();
  const apps = appsForRole(isAdmin);
  const search = h("input", {
    class: "input menu-search",
    type: "search",
    placeholder: "Search applications…",
    ariaLabel: "Search applications",
  });
  const grid = h("div", { class: "start-grid", role: "listbox", ariaLabel: "Applications" });
  const recentWrap = h("div", { class: "start-recent" });
  const hiddenLabel = h("div", { class: "menu-section-title", hidden: true }, "No matching applications");
  const store = ctx.deps.store;

  const launch = (appId: string) => {
    pushRecentApp(appId);
    ctx.openApp(appId);
    onClose();
  };

  const renderRecent = () => {
    recentWrap.replaceChildren();
    const recent = readRecentApps()
      .map((id) => apps.find((app) => app.id === id))
      .filter((app): app is NonNullable<ReturnType<typeof apps.find>> => Boolean(app));
    if (recent.length === 0) {
      recentWrap.hidden = true;
      return;
    }
    recentWrap.hidden = false;
    recentWrap.appendChild(h("div", { class: "menu-section-title" }, "Recent"));
    const chips = h("div", { class: "recent-chips" });
    for (const app of recent) {
      chips.appendChild(
        h(
          "button",
          { class: "recent-chip", type: "button", onClick: () => launch(app.id), title: app.description },
          iconSvg(app.icon, 14),
          h("span", {}, app.name),
        ),
      );
    }
    recentWrap.appendChild(chips);
  };

  const renderApps = (query: string) => {
    const normalized = query.trim().toLowerCase();
    const matches = normalized
      ? apps.filter(
          (app) =>
            app.name.toLowerCase().includes(normalized) ||
            app.description.toLowerCase().includes(normalized) ||
            app.id.includes(normalized),
        )
      : apps;
    grid.replaceChildren();
    for (const app of matches) {
      grid.appendChild(appTile(app, () => launch(app.id)));
    }
    const tiles = Array.from(grid.querySelectorAll<HTMLButtonElement>(".menu-app"));
    installGridKeys(grid, tiles);
    hiddenLabel.hidden = matches.length > 0;
    hiddenLabel.textContent = normalized ? `No applications match “${query.trim()}”` : "No applications available for your account";
    renderRecent();
    recentWrap.hidden = recentWrap.hidden || Boolean(normalized);
  };
  renderApps("");

  search.addEventListener("input", () => renderApps(search.value));
  search.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      grid.querySelector<HTMLButtonElement>(".menu-app")?.focus();
    } else if (event.key === "Enter") {
      event.preventDefault();
      grid.querySelector<HTMLButtonElement>(".menu-app")?.click();
    }
  });

  const footer = h(
    "div",
    { class: "start-footer" },
    menuItem("user", "Account settings", () => {
      onClose();
      ctx.openAccount("profile");
    }),
    menuItem("wallpaper", "Change wallpaper", () => {
      onClose();
      ctx.showWallpaper();
    }),
    menuItem("grid", "Keyboard shortcuts", () => {
      onClose();
      ctx.showShortcuts();
    }),
    menuItem("info", "About RAST", () => {
      onClose();
      ctx.showAbout();
    }),
    h("div", { class: "menu-sep" }),
    menuItem("logout", "Sign out", () => {
      onClose();
      ctx.requestSignOut();
    }, { danger: true }),
  );

  const panel = h(
    "div",
    {
      class: "menu-panel start-menu",
      role: "dialog",
      ariaLabel: "Start menu",
    },
    h(
      "div",
      { class: "start-head" },
      h(
        "div",
        { class: "start-brand" },
        h("span", { class: "start-logo" }, iconSvg("logo", 22)),
        h(
          "span",
          { class: "start-brand-text" },
          h("span", { class: "start-title" }, store.get().siteName || "RAST"),
          h(
            "span",
            { class: "start-sub small muted" },
            [store.get().version ? `v${store.get().version}` : null, store.get().user ? `signed in as ${store.get().user?.displayName || store.get().user?.username}` : null]
              .filter(Boolean)
              .join(" · "),
          ),
        ),
      ),
      h("span", { class: "start-hint" }, "Ctrl+Space"),
    ),
    search,
    recentWrap,
    h("div", { class: "menu-section-title" }, "Applications"),
    grid,
    hiddenLabel,
    h("div", { class: "menu-sep" }),
    footer,
  );
  window.setTimeout(() => search.focus(), 20);
  return panel;
}

export function buildUserMenu(ctx: PanelContext, onClose: () => void): HTMLElement {
  const store = ctx.deps.store;
  const user = store.get().user;
  const artwork = segmented<ThemePreference>({
    items: [
      { value: "dark", label: "Dark", icon: "moon" },
      { value: "light", label: "Light", icon: "sun" },
      { value: "system", label: "System", icon: "monitor" },
    ],
    value: store.get().themePreference,
    ariaLabel: "Theme preference",
    onChange: (value) => store.setThemePreference(value),
  });
  const themeNote = h("div", { class: "small faint theme-note" }, themeDescription(store.get().themePreference, store.get().theme));

  const panel = h(
    "div",
    { class: "menu-panel user-menu", role: "dialog", ariaLabel: "Account menu" },
    h(
      "div",
      { class: "user-menu-head" },
      h("div", { class: "avatar avatar-lg" }, (user?.displayName || user?.username || "?").slice(0, 1).toUpperCase()),
      h(
        "div",
        { class: "user-menu-id" },
        h("div", { class: "user-menu-name" }, user?.displayName || user?.username || "User"),
        h(
          "div",
          { class: "small muted" },
          [user?.username ? `@${user.username}` : null, user?.role === "admin" ? "Administrator" : "User"]
            .filter(Boolean)
            .join(" · "),
        ),
        h("div", { class: "small faint" }, `Signed in ${relativeTime(store.get().user?.lastLoginAt ?? Date.now())}`),
      ),
    ),
    h("div", { class: "menu-sep" }),
    h("div", { class: "menu-section-title" }, "Appearance"),
    artwork.el,
    themeNote,
    h("div", { class: "menu-sep" }),
    menuItem("user", "Account settings", () => {
      onClose();
      ctx.openAccount("profile");
    }),
    menuItem("sessions", "Active sessions", () => {
      onClose();
      ctx.openAccount("profile", "sessions");
    }),
    menuItem("grid", "Keyboard shortcuts", () => {
      onClose();
      ctx.showShortcuts();
    }),
    menuItem("info", "About RAST", () => {
      onClose();
      ctx.showAbout();
    }),
    h("div", { class: "menu-sep" }),
    menuItem("logout", "Sign out", () => {
      onClose();
      ctx.requestSignOut();
    }, { danger: true }),
  );
  const observer = store.subscribe((state) => {
    artwork.setValue(state.themePreference);
    themeNote.textContent = themeDescription(state.themePreference, state.theme);
  });
  cleanupWhenDetached(panel, observer);
  return panel;
}

function themeDescription(preference: ThemePreference, resolved: string): string {
  if (preference === "system") return `Following the operating system (currently ${resolved})`;
  return `${resolved === "dark" ? "Dark" : "Light"} mode enabled`;
}

function cleanupWhenDetached(el: HTMLElement, cleanup: () => void): void {
  const id = window.setInterval(() => {
    if (!el.isConnected) {
      window.clearInterval(id);
      cleanup();
    }
  }, 1000);
}

export function buildStatusPanel(ctx: PanelContext, onClose: () => void): HTMLElement {
  const deps = ctx.deps;
  const state = deps.events.status;
  const statusTone = state === "connected" ? "ok" : state === "connecting" ? "warn" : "error";
  const statusLabel = state === "connected" ? "Connected" : state === "connecting" ? "Connecting…" : "Disconnected";
  const lastEvent = deps.events.lastEventTimestamp;
  const expires = deps.store.get().sessionExpiresAt;
  const reconnect = h(
    "button",
    { class: "btn btn-sm", type: "button" },
    iconSvg("refresh", 15),
    "Reconnect",
  );
  reconnect.addEventListener("click", () => {
    deps.events.stop();
    deps.events.start();
    ctx.notify("Reconnecting to the live event stream…", "info");
    onClose();
  });
  const serverTime = new Date(Date.now() + ctx.serverOffset);
  return h(
    "div",
    { class: "menu-panel status-panel", role: "dialog", ariaLabel: "Connection status" },
    panelHeader("System status", deps.store.get().siteName || "RAST"),
    h(
      "dl",
      { class: "kv compact" },
      h("dt", {}, "Live connection"),
      h("dd", {}, h("span", { class: `badge ${statusTone}` }, statusLabel)),
      h("dt", {}, "Last event"),
      h("dd", {}, lastEvent ? relativeTime(lastEvent) : "none yet"),
      h("dt", {}, "Session expires"),
      h("dd", {}, expires ? formatDateTime(expires) : "unknown"),
      h("dt", {}, "Server time"),
      h("dd", { class: "mono" }, formatClockSeconds(serverTime.getTime())),
      h("dt", {}, "Version"),
      h("dd", {}, deps.store.get().version || "unknown"),
    ),
    h(
      "div",
      { class: "status-actions" },
      reconnect,
      h(
        "button",
        {
          class: "btn btn-sm",
          type: "button",
          onClick: () => {
            onClose();
            ctx.openAccount("profile", "sessions");
          },
        },
        iconSvg("sessions", 15),
        "Manage sessions",
      ),
    ),
    h("div", { class: "small faint status-note" }, "Live updates use a single shared event stream."),
  );
}

export function buildClockPanel(ctx: PanelContext, onClose: () => void): HTMLElement {
  const timeEl = h("div", { class: "clock-popover-time" });
  const dateEl = h("div", { class: "clock-popover-date muted" });
  const zone = (() => {
    try {
      return Intl.DateTimeFormat().resolvedOptions().timeZone || "local time";
    } catch {
      return "local time";
    }
  })();
  const panel = h(
    "div",
    { class: "menu-panel clock-panel", role: "dialog", ariaLabel: "Clock" },
    panelHeader("Date and time", zone),
    timeEl,
    dateEl,
    h("div", { class: "menu-sep" }),
    h(
      "dl",
      { class: "kv compact" },
      h("dt", {}, "Server time"),
      h("dd", { class: "mono", id: "clock-server-time" }, "—"),
      h("dt", {}, "Session expires"),
      h("dd", {}, formatDateTime(ctx.deps.store.get().sessionExpiresAt)),
      h("dt", {}, "Product"),
      h("dd", {}, ctx.deps.store.get().siteName || "RAST"),
    ),
    h(
      "div",
      { class: "status-actions" },
      h(
        "button",
        { class: "btn btn-sm", type: "button", onClick: () => onClose() },
        "Close",
      ),
    ),
  );
  let stopped = false;
  const tick = () => {
    if (stopped) return;
    if (!panel.isConnected) {
      stopped = true;
      window.clearInterval(interval);
      return;
    }
    const now = Date.now();
    timeEl.textContent = formatClockSeconds(now);
    dateEl.textContent = formatWeekdayDate(now);
    const serverEl = panel.querySelector<HTMLElement>("#clock-server-time");
    if (serverEl) serverEl.textContent = formatClockSeconds(now + ctx.serverOffset);
  };
  tick();
  const interval = window.setInterval(tick, 1000);
  window.setTimeout(() => {
    if (!panel.isConnected) window.clearInterval(interval);
  }, 500);
  return panel;
}

/** Message for a failed optional API call, used by panels/apps. */
export function optionalFeatureMessage(error: unknown, feature: string): string {
  if (error instanceof ApiError) return error.message;
  return `${feature} is not available: ${String(error)}`;
}
