// Per-user appearance (wallpaper + theme), persisted on the server.
//
// The server (SQLite) is the source of truth. localStorage is only a small paint
// cache for the same user so the desktop appears instantly on reload; it is
// keyed by user id and removed on sign-out so wallpapers never leak between
// accounts on a shared browser.

import type { ApiClient } from "./api";
import { ApiError } from "./api";

export type ThemeSetting = "light" | "dark" | "system";
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
  wallpaper: WallpaperSettings;
  uploads: AppearanceUpload[];
  limits?: { maxBytes: number; maxUploads: number };
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

export const DEFAULT_APPEARANCE: Appearance = {
  theme: "light",
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

  /** Save wallpaper/theme changes to the server. */
  async save(patch: { theme?: ThemeSetting; wallpaper?: Partial<WallpaperSettings> }): Promise<Appearance> {
    const wallpaper = { ...this.current.wallpaper, ...(patch.wallpaper ?? {}) };
    const payload = {
      theme: patch.theme ?? this.current.theme,
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
