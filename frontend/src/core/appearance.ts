// Per-user appearance (wallpaper + theme), persisted on the server.
//
// The server (SQLite) is the source of truth. localStorage is only a small paint
// cache for the same user so the desktop appears instantly on reload; it is
// keyed by user id and removed on sign-out so wallpapers never leak between
// accounts on a shared browser.

import type { ApiClient } from "./api";
import { ApiError } from "./api";
import { getRealtime } from "./realtime";
import type { EventStream } from "./sse";

export type ThemeSetting = "light" | "dark" | "system";
export type AccentId =
  | "ocean"
  | "forest"
  | "teal"
  | "sage"
  | "amber"
  | "terracotta"
  | "rose"
  | "plum"
  | "indigo"
  | "graphite";
export type ResolvedTheme = "light" | "dark";
export type WallpaperKind = "builtin" | "upload" | "color";
export type WallpaperFit = "cover" | "contain" | "center";
export type WallpaperPosition =
  | "center"
  | "top"
  | "bottom"
  | "left"
  | "right"
  | "top-left"
  | "top-right"
  | "bottom-left"
  | "bottom-right";

export interface WallpaperSettings {
  kind: WallpaperKind;
  builtinId: string;
  uploadId: string | null;
  color: string;
  fit: WallpaperFit;
  position: WallpaperPosition;
  /** Darkening overlay percentage, 0-60. */
  overlay: number;
}

export interface AppearanceUpload {
  id: string;
  width: number;
  height: number;
  sizeBytes: number;
  mime: string;
  createdAt: number;
  url: string;
}

export interface Appearance {
  theme: ThemeSetting;
  accent: AccentId;
  wallpaper: WallpaperSettings;
  uploads: AppearanceUpload[];
  limits?: { maxBytes: number; maxUploads: number };
}

/** Payload of the `appearance.changed` realtime event. */
export interface AppearanceChangedEvent {
  reason: string;
  originSessionId?: string | null;
  appearance?: Appearance | null;
}

/** Payload of the SSE `hello` event. */
export interface RealtimeHello {
  seq?: number;
  ts?: number;
  userId?: number;
  admin?: boolean;
  sessionId?: string;
  version?: string;
}

let previewDepth = 0;

/**
 * While an appearance editor previews a draft, other appearance updates (own
 * upload response, external sessions) must not overwrite the preview.
 */
export function beginAppearancePreview(): void {
  previewDepth += 1;
}

export function endAppearancePreview(): void {
  previewDepth = Math.max(0, previewDepth - 1);
}

export function isAppearancePreviewActive(): boolean {
  return previewDepth > 0;
}

export interface BuiltinWallpaper {
  id: string;
  label: string;
  detail: string;
  file: string;
}

/** Bundled nature wallpapers (see docs/WALLPAPERS.md for licence). */
export const BUILTIN_WALLPAPERS: ReadonlyArray<BuiltinWallpaper> = [
  {
    id: "alpine-dawn",
    label: "Alpine Dawn",
    detail: "Calm lake below snow-capped peaks at sunrise",
    file: "/wallpapers/alpine-dawn.jpg",
  },
  {
    id: "forest-mist",
    label: "Forest Mist",
    detail: "Pine forest with soft morning light rays",
    file: "/wallpapers/forest-mist.jpg",
  },
  {
    id: "coastal-calm",
    label: "Coastal Calm",
    detail: "Turquoise water, smooth rocks and a pale sky",
    file: "/wallpapers/coastal-calm.jpg",
  },
  {
    id: "desert-dusk",
    label: "Desert Dusk",
    detail: "Warm dunes under a violet evening sky",
    file: "/wallpapers/desert-dusk.jpg",
  },
];

export interface AccentPreset {
  id: AccentId;
  label: string;
  /** Accent used in light mode. */
  light: string;
  /** Accent used in dark mode (lighter for contrast). */
  dark: string;
}

/** Exactly ten accent presets; ids are stable API values. */
export const ACCENT_PRESETS: ReadonlyArray<AccentPreset> = [
  { id: "ocean", label: "Ocean Blue", light: "#2563EB", dark: "#7CB3F4" },
  { id: "forest", label: "Forest Green", light: "#15803D", dark: "#79BC8B" },
  { id: "teal", label: "Teal", light: "#0F766E", dark: "#6CCABD" },
  { id: "sage", label: "Sage", light: "#4D7C0F", dark: "#BECCA5" },
  { id: "amber", label: "Amber", light: "#B45309", dark: "#E9B560" },
  { id: "terracotta", label: "Terracotta", light: "#B84D34", dark: "#E99A80" },
  { id: "rose", label: "Rose", light: "#BE3455", dark: "#EE93A9" },
  { id: "plum", label: "Plum", light: "#7E3F98", dark: "#CCA1DB" },
  { id: "indigo", label: "Indigo", light: "#4F46B8", dark: "#ABA7EF" },
  { id: "graphite", label: "Graphite", light: "#475569", dark: "#BBC7D4" },
];

export const DEFAULT_ACCENT: AccentId = "ocean";

export function accentPreset(id: AccentId): AccentPreset {
  return ACCENT_PRESETS.find((preset) => preset.id === id) ?? ACCENT_PRESETS[0]!;
}

export function isAccentId(value: string): value is AccentId {
  return ACCENT_PRESETS.some((preset) => preset.id === value);
}

export const DEFAULT_APPEARANCE: Appearance = {
  theme: "light",
  accent: DEFAULT_ACCENT,
  wallpaper: {
    kind: "builtin",
    builtinId: "alpine-dawn",
    uploadId: null,
    color: "#1B2430",
    fit: "cover",
    position: "center",
    overlay: 20,
  },
  uploads: [],
  limits: { maxBytes: 10 * 1024 * 1024, maxUploads: 20 },
};

export const WALLPAPER_POSITIONS: ReadonlyArray<{ id: WallpaperPosition; label: string }> = [
  { id: "center", label: "Center" },
  { id: "top", label: "Top" },
  { id: "bottom", label: "Bottom" },
  { id: "left", label: "Left" },
  { id: "right", label: "Right" },
  { id: "top-left", label: "Top left" },
  { id: "top-right", label: "Top right" },
  { id: "bottom-left", label: "Bottom left" },
  { id: "bottom-right", label: "Bottom right" },
];

export function builtinWallpaper(id: string): BuiltinWallpaper | undefined {
  return BUILTIN_WALLPAPERS.find((wallpaper) => wallpaper.id === id);
}

export function builtinUrl(id: string): string {
  return builtinWallpaper(id)?.file ?? BUILTIN_WALLPAPERS[0]?.file ?? "/wallpapers/alpine-dawn.jpg";
}

/** CSS `background-position` for a logical position. */
export function positionCss(position: WallpaperPosition): string {
  const left = position.includes("left");
  const right = position.includes("right");
  const top = position.includes("top");
  const bottom = position.includes("bottom");
  if (left || right) {
    return `${right ? "right" : "left"}${top ? " top" : bottom ? " bottom" : ""}`;
  }
  if (top) return "top";
  if (bottom) return "bottom";
  return "center";
}

/** CSS `background-size` for a fit mode (center keeps natural size). */
export function fitCss(fit: WallpaperFit): string {
  if (fit === "cover") return "cover";
  if (fit === "contain") return "contain";
  return "auto";
}

export function resolveTheme(theme: ThemeSetting): ResolvedTheme {
  if (theme === "dark" || theme === "light") return theme;
  try {
    return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  } catch {
    return "light";
  }
}

const CACHE_PREFIX = "rast.appearance.v1.";

function cacheKey(userId: number): string {
  return `${CACHE_PREFIX}${userId}`;
}

export function readCachedAppearance(userId: number): Appearance | null {
  try {
    const raw = window.localStorage.getItem(cacheKey(userId));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Appearance;
    if (!parsed || typeof parsed !== "object" || !parsed.wallpaper) return null;
    return normalizeAppearance(parsed);
  } catch {
    return null;
  }
}

export function writeCachedAppearance(userId: number, appearance: Appearance): void {
  try {
    window.localStorage.setItem(cacheKey(userId), JSON.stringify(appearance));
  } catch {
    /* storage disabled or full: the server copy still applies */
  }
}

/** Remove this user's cache (also used on sign-out so accounts never share it). */
export function clearCachedAppearance(userId?: number | null): void {
  try {
    if (typeof userId === "number") {
      window.localStorage.removeItem(cacheKey(userId));
      return;
    }
    for (const key of Object.keys(window.localStorage)) {
      if (key.startsWith(CACHE_PREFIX)) window.localStorage.removeItem(key);
    }
  } catch {
    /* ignore */
  }
}

/** Fill in missing fields defensively (older cache entries, partial responses). */
export function normalizeAppearance(input: Partial<Appearance> | null | undefined): Appearance {
  const wallpaper = { ...DEFAULT_APPEARANCE.wallpaper, ...(input?.wallpaper ?? {}) };
  const overlay = Math.max(0, Math.min(60, Math.round(Number(wallpaper.overlay) || 0)));
  return {
    theme: (input?.theme as ThemeSetting) ?? DEFAULT_APPEARANCE.theme,
    accent: isAccentId(String(input?.accent ?? "")) ? (input!.accent as AccentId) : DEFAULT_ACCENT,
    wallpaper: {
      kind: (wallpaper.kind as WallpaperKind) ?? "builtin",
      builtinId: wallpaper.builtinId || DEFAULT_APPEARANCE.wallpaper.builtinId,
      uploadId: wallpaper.uploadId ?? null,
      color: /^#[0-9a-fA-F]{6}$/.test(wallpaper.color) ? wallpaper.color : DEFAULT_APPEARANCE.wallpaper.color,
      fit: (wallpaper.fit as WallpaperFit) ?? "cover",
      position: (wallpaper.position as WallpaperPosition) ?? "center",
      overlay,
    },
    uploads: Array.isArray(input?.uploads) ? input!.uploads! : [],
    limits: input?.limits ?? DEFAULT_APPEARANCE.limits,
  };
}

/** URL used as the CSS background for an upload; requires the session cookie. */
export function uploadUrl(uploadId: string): string {
  return `/api/appearance/wallpapers/${encodeURIComponent(uploadId)}`;
}

/**
 * Apply appearance to the document. `preview` renders without touching the
 * cache or the theme chrome (used by the Personalize dialog while dragging
 * sliders / trying wallpapers).
 */
export function applyAppearance(appearance: Appearance, options: { preview?: boolean } = {}): void {
  const root = document.documentElement;
  const wallpaper = appearance.wallpaper;
  const url =
    wallpaper.kind === "upload" && wallpaper.uploadId
      ? uploadUrl(wallpaper.uploadId)
      : wallpaper.kind === "builtin"
        ? builtinUrl(wallpaper.builtinId)
        : "";

  if (url) {
    root.style.setProperty("--wallpaper-image", `url("${url}")`);
  } else {
    root.style.removeProperty("--wallpaper-image");
  }
  root.style.setProperty("--wallpaper-color", wallpaper.color || DEFAULT_APPEARANCE.wallpaper.color);
  root.style.setProperty("--wallpaper-size", fitCss(wallpaper.fit));
  root.style.setProperty("--wallpaper-position", positionCss(wallpaper.position));
  root.style.setProperty("--wallpaper-overlay", String(wallpaper.overlay / 100));
  root.dataset.accent = appearance.accent;
  root.dataset.wallpaperKind = wallpaper.kind;
  if (wallpaper.kind !== "color" && !url) {
    root.dataset.wallpaperKind = "color";
  }
  if (!options.preview) {
    root.dataset.wallpaperApplied = "true";
  }
}

/** Revert document CSS variables to a previously captured state. */
export function captureWallpaper(): Partial<CSSStyleDeclaration> | null {
  const root = document.documentElement;
  const props = ["--wallpaper-image", "--wallpaper-color", "--wallpaper-size", "--wallpaper-position", "--wallpaper-overlay"];
  const snapshot: Record<string, string> = {};
  for (const prop of props) {
    const value = root.style.getPropertyValue(prop);
    if (value) snapshot[prop] = value;
  }
  return snapshot as unknown as Partial<CSSStyleDeclaration>;
}

export function restoreWallpaper(snapshot: Partial<CSSStyleDeclaration> | null): void {
  const root = document.documentElement;
  const props = ["--wallpaper-image", "--wallpaper-color", "--wallpaper-size", "--wallpaper-position", "--wallpaper-overlay"];
  for (const prop of props) {
    const value = snapshot ? (snapshot as Record<string, string>)[prop] : "";
    if (value) root.style.setProperty(prop, value);
    else root.style.removeProperty(prop);
  }
}

/** API client for /api/appearance. */
export class AppearanceService {
  private current: Appearance = { ...DEFAULT_APPEARANCE };
  private userId: number | null = null;
  private listeners = new Set<(appearance: Appearance) => void>();

  constructor(private readonly api: ApiClient) {}

  get value(): Appearance {
    return this.current;
  }

  get currentUserId(): number | null {
    return this.userId;
  }

  subscribe(listener: (appearance: Appearance) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(): void {
    for (const listener of this.listeners) {
      try {
        listener(this.current);
      } catch {
        /* ignore listener errors */
      }
    }
  }

  /** Load the signed-in user's appearance (cache-first, then server). */
  async load(userId: number): Promise<Appearance> {
    this.userId = userId;
    const cached = readCachedAppearance(userId);
    if (cached) {
      this.current = cached;
      this.emit();
    }
    try {
      const fresh = normalizeAppearance(await this.api.get<Appearance>("/api/appearance"));
      this.current = fresh;
      writeCachedAppearance(userId, fresh);
      this.emit();
      return fresh;
    } catch (error) {
      if (cached) return cached;
      throw error;
    }
  }

  /**
   * Adopt an appearance that came from the server (another session, or a
   * refreshed snapshot) without making a network request.
   */
  adopt(input: Appearance): Appearance {
    const next = normalizeAppearance(input);
    this.current = next;
    if (this.userId !== null) writeCachedAppearance(this.userId, next);
    this.emit();
    return next;
  }

  /** Re-read the signed-in user's appearance from the server. */
  async reload(): Promise<Appearance> {
    const fresh = normalizeAppearance(await this.api.get<Appearance>("/api/appearance"));
    this.current = fresh;
    if (this.userId !== null) writeCachedAppearance(this.userId, fresh);
    this.emit();
    return fresh;
  }

  /** Save wallpaper/theme changes to the server. */
  async save(patch: {
    theme?: ThemeSetting;
    accent?: AccentId;
    wallpaper?: Partial<WallpaperSettings>;
  }): Promise<Appearance> {
    const wallpaper = { ...this.current.wallpaper, ...(patch.wallpaper ?? {}) };
    const payload = {
      theme: patch.theme ?? this.current.theme,
      accent: patch.accent ?? this.current.accent,
      wallpaper: {
        kind: wallpaper.kind,
        builtinId: wallpaper.builtinId,
        uploadId: wallpaper.uploadId,
        color: wallpaper.color,
        fit: wallpaper.fit,
        position: wallpaper.position,
        overlay: wallpaper.overlay,
      },
    };
    const updated = normalizeAppearance(await this.api.put<Appearance>("/api/appearance", payload));
    this.current = updated;
    if (this.userId !== null) writeCachedAppearance(this.userId, updated);
    this.emit();
    return updated;
  }

  /** Upload a wallpaper file (raw body). Returns the created upload. */
  async upload(file: File): Promise<AppearanceUpload> {
    const response = await fetch("/api/appearance/wallpapers", {
      method: "POST",
      headers: {
        "Content-Type": file.type,
        "X-CSRF-Token": this.api.getCsrfToken(),
      },
      credentials: "same-origin",
      body: file,
    });
    if (!response.ok) {
      let message = `Upload failed (${response.status})`;
      let code = "upload_failed";
      try {
        const payload = (await response.json()) as { error?: { code?: string; message?: string } };
        message = payload.error?.message ?? message;
        code = payload.error?.code ?? code;
      } catch {
        /* keep default message */
      }
      throw new ApiError(response.status, code, message);
    }
    const upload = (await response.json()) as AppearanceUpload;
    this.current = {
      ...this.current,
      uploads: [upload, ...this.current.uploads.filter((item) => item.id !== upload.id)],
    };
    this.emit();
    return upload;
  }

  async removeUpload(uploadId: string): Promise<Appearance> {
    const result = await this.api.del<{ ok: boolean; appearance?: Appearance }>(
      `/api/appearance/wallpapers/${encodeURIComponent(uploadId)}`,
    );
    const updated = normalizeAppearance(result.appearance ?? this.current);
    this.current = updated;
    if (this.userId !== null) writeCachedAppearance(this.userId, updated);
    this.emit();
    return updated;
  }

  async reset(): Promise<Appearance> {
    const updated = normalizeAppearance(await this.api.post<Appearance>("/api/appearance/reset"));
    this.current = updated;
    if (this.userId !== null) writeCachedAppearance(this.userId, updated);
    this.emit();
    return updated;
  }

  /** Update the accent through the server. */
  async saveAccent(accent: AccentId): Promise<void> {
    if (this.current.accent === accent) return;
    try {
      await this.save({ accent });
    } catch (error) {
      console.warn("could not persist accent preference", error);
    }
  }

  /** Update the theme through the server (debounced by the caller). */
  async saveTheme(theme: ThemeSetting): Promise<void> {
    if (this.current.theme === theme) return;
    try {
      await this.save({ theme });
    } catch (error) {
      console.warn("could not persist theme preference", error);
    }
  }

  clear(): void {
    if (this.userId !== null) clearCachedAppearance(this.userId);
    else clearCachedAppearance();
    this.userId = null;
    this.current = { ...DEFAULT_APPEARANCE };
  }
}

// ---------------------------------------------------------------------------
// Cross-session appearance synchronization.
//
// The server publishes `appearance.changed` to the owner's other sessions with
// the originating session id. This coordinator:
//   * remembers our own session id from the SSE `hello` frame,
//   * ignores the echo of our own save (guarded by a short grace window),
//   * adopts external changes when no editor has a dirty draft,
//   * reports a conflict to open editors when the user has unsaved changes.
// ---------------------------------------------------------------------------

export type AppearanceConflictListener = (appearance: Appearance, event: AppearanceChangedEvent) => void;
export type AppearanceAdoptListener = (appearance: Appearance) => void;

const LOCAL_SAVE_GRACE_MS = 4_000;

export class AppearanceSync {
  private readonly realtime;
  private readonly unsubscribers: Array<() => void> = [];
  private readonly dirtySources = new Set<() => boolean>();
  private readonly conflictListeners = new Set<AppearanceConflictListener>();
  private readonly adoptListeners = new Set<AppearanceAdoptListener>();
  private sessionId: string | null = null;
  private lastLocalSaveAt = 0;
  private disposed = false;

  constructor(
    private readonly service: AppearanceService,
    events: EventStream,
  ) {
    this.realtime = getRealtime(events);
    this.unsubscribers.push(
      this.realtime.on<RealtimeHello>("hello", (data) => {
        if (data && typeof data.sessionId === "string") this.sessionId = data.sessionId;
      }),
    );
    this.unsubscribers.push(
      this.realtime.on<AppearanceChangedEvent>("appearance.changed", (data) => this.handleChanged(data)),
    );
    this.unsubscribers.push(
      this.realtime.onResync(() => {
        void this.resync();
      }),
    );
  }

  get session(): string | null {
    return this.sessionId;
  }

  /** Called right after a successful save so the echo event is ignored. */
  markLocalSave(): void {
    this.lastLocalSaveAt = Date.now();
  }

  /** Register a predicate telling the coordinator whether a draft is dirty. */
  registerDirtySource(source: () => boolean): () => void {
    this.dirtySources.add(source);
    return () => this.dirtySources.delete(source);
  }

  hasDirtyDraft(): boolean {
    for (const source of this.dirtySources) {
      try {
        if (source()) return true;
      } catch {
        /* ignore a broken predicate */
      }
    }
    return false;
  }

  onConflict(listener: AppearanceConflictListener): () => void {
    this.conflictListeners.add(listener);
    return () => this.conflictListeners.delete(listener);
  }

  onAdopt(listener: AppearanceAdoptListener): () => void {
    this.adoptListeners.add(listener);
    return () => this.adoptListeners.delete(listener);
  }

  /** Adopt an appearance (e.g. resolved "Use updated" conflict). */
  adopt(input: Appearance): Appearance {
    const next = this.service.adopt(input);
    for (const listener of this.adoptListeners) {
      try {
        listener(next);
      } catch (error) {
        console.error("appearance adopt listener failed", error);
      }
    }
    return next;
  }

  /** True when the event was triggered by this tab's own recent save. */
  isOwnEcho(event: AppearanceChangedEvent): boolean {
    if (!event.originSessionId) return false;
    if (event.originSessionId !== this.sessionId) return false;
    return Date.now() - this.lastLocalSaveAt < LOCAL_SAVE_GRACE_MS;
  }

  private handleChanged(event: AppearanceChangedEvent): void {
    if (this.disposed || !event) return;
    if (!event.appearance) {
      // Some events (e.g. a deletion from elsewhere) only carry the reason.
      void this.resync();
      return;
    }
    if (this.isOwnEcho(event)) return;
    const appearance = normalizeAppearance(event.appearance);
    // The event may echo a save whose response we already applied (different
    // tab of the same session, or a race before the grace window was marked).
    // Identical state needs no adoption and must never look like a conflict.
    if (JSON.stringify(appearance) === JSON.stringify(normalizeAppearance(this.service.value))) return;
    if (this.hasDirtyDraft()) {
      for (const listener of this.conflictListeners) {
        try {
          listener(appearance, event);
        } catch (error) {
          console.error("appearance conflict listener failed", error);
        }
      }
      return;
    }
    this.adopt(appearance);
  }

  private async resync(): Promise<void> {
    if (this.disposed) return;
    try {
      const fresh = await this.service.reload();
      if (this.hasDirtyDraft()) {
        for (const listener of this.conflictListeners) {
          try {
            listener(fresh, { reason: "resync", originSessionId: null, appearance: fresh });
          } catch (error) {
            console.error("appearance conflict listener failed", error);
          }
        }
        return;
      }
      for (const listener of this.adoptListeners) {
        try {
          listener(fresh);
        } catch (error) {
          console.error("appearance adopt listener failed", error);
        }
      }
    } catch {
      /* the next reconnect will retry */
    }
  }

  dispose(): void {
    this.disposed = true;
    for (const unsubscribe of this.unsubscribers.splice(0)) {
      try {
        unsubscribe();
      } catch {
        /* ignore */
      }
    }
    this.dirtySources.clear();
    this.conflictListeners.clear();
    this.adoptListeners.clear();
  }
}

const SYNC_MAP = new WeakMap<ApiClient, AppearanceSync>();

/** Shared cross-session appearance coordinator for this tab. */
export function getAppearanceSync(api: ApiClient, events: EventStream): AppearanceSync {
  let sync = SYNC_MAP.get(api);
  if (!sync) {
    sync = new AppearanceSync(getAppearanceService(api), events);
    SYNC_MAP.set(api, sync);
  }
  return sync;
}

// ---------------------------------------------------------------------------
// Shared instance: every part of the UI (desktop, Control Panel, Personalize)
// talks to the same service so there is exactly one appearance state.
// ---------------------------------------------------------------------------

const SERVICE_MAP = new WeakMap<ApiClient, AppearanceService>();

export function getAppearanceService(api: ApiClient): AppearanceService {
  let service = SERVICE_MAP.get(api);
  if (!service) {
    service = new AppearanceService(api);
    SERVICE_MAP.set(api, service);
  }
  return service;
}
