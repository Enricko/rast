// Application state shared by the shell and every app window.

import type { Features, User } from "./types";

/** Concrete theme that is currently rendered. */
export type Theme = "dark" | "light";
/** User preference; "system" follows the operating system. */
export type ThemePreference = Theme | "system";

export interface AppState {
  user: User | null;
  features: Features;
  csrfToken: string;
  siteName: string;
  version: string;
  loginMessage: string;
  maxUploadBytes: number;
  theme: Theme;
  themePreference: ThemePreference;
  sessionExpiresAt: number;
}

const DEFAULT_FEATURES: Features = {
  docker: false,
  apps: false,
  backup: false,
  monitor: true,
  activityLogs: true,
  fileStation: true,
};

const THEME_KEY = "rast.theme";

export type StoreListener = (state: AppState) => void;

export class Store {
  private state: AppState;
  private readonly listeners = new Set<StoreListener>();
  private mediaQuery: MediaQueryList | null = null;
  private mediaHandler: ((event: MediaQueryListEvent) => void) | null = null;

  constructor(initial?: Partial<AppState>) {
    const preference = readThemePreference();
    this.state = {
      user: null,
      features: { ...DEFAULT_FEATURES },
      csrfToken: "",
      siteName: "RAST",
      version: "",
      loginMessage: "",
      maxUploadBytes: 10 * 1024 * 1024 * 1024,
      theme: resolveTheme(preference),
      themePreference: preference,
      sessionExpiresAt: 0,
      ...initial,
    };
    if (initial?.theme && !initial.themePreference) {
      this.state.themePreference = initial.theme;
    }
    this.applyTheme();
    this.watchSystemTheme();
  }

  get(): AppState {
    return this.state;
  }

  patch(partial: Partial<AppState>): void {
    const previousTheme = this.state.theme;
    this.state = { ...this.state, ...partial };
    if (partial.themePreference !== undefined) {
      this.state.theme = resolveTheme(partial.themePreference);
      writeThemePreference(partial.themePreference);
    } else if (partial.theme !== undefined) {
      this.state.themePreference = partial.theme;
      writeThemePreference(partial.theme);
    }
    if (previousTheme !== this.state.theme || partial.themePreference !== undefined) {
      this.applyTheme();
    }
    this.emit();
  }

  setUser(user: User | null): void {
    this.patch({ user });
  }

  isAdmin(): boolean {
    return this.state.user?.role === "admin";
  }

  subscribe(listener: StoreListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  toggleTheme(): void {
    this.setThemePreference(this.state.theme === "dark" ? "light" : "dark");
  }

  setThemePreference(preference: ThemePreference): void {
    this.patch({ themePreference: preference });
  }

  private emit(): void {
    for (const listener of this.listeners) {
      try {
        listener(this.state);
      } catch (error) {
        console.error("store listener failed", error);
      }
    }
  }

  private applyTheme(): void {
    const root = document.documentElement;
    root.dataset.theme = this.state.theme;
    root.dataset.themePreference = this.state.themePreference;
    root.style.colorScheme = this.state.theme;
  }

  private watchSystemTheme(): void {
    if (typeof window.matchMedia !== "function") return;
    this.mediaQuery = window.matchMedia("(prefers-color-scheme: light)");
    this.mediaHandler = () => {
      if (this.state.themePreference !== "system") return;
      const theme = resolveTheme("system");
      if (theme === this.state.theme) return;
      this.state = { ...this.state, theme };
      this.applyTheme();
      this.emit();
    };
    this.mediaQuery.addEventListener("change", this.mediaHandler);
  }
}

export function resolveTheme(preference: ThemePreference): Theme {
  if (preference === "dark" || preference === "light") return preference;
  try {
    return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
  } catch {
    return "dark";
  }
}

function readThemePreference(): ThemePreference {
  try {
    const stored = window.localStorage.getItem(THEME_KEY);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    /* storage disabled */
  }
  return "system";
}

function writeThemePreference(preference: ThemePreference): void {
  try {
    window.localStorage.setItem(THEME_KEY, preference);
  } catch {
    /* ignore */
  }
}
