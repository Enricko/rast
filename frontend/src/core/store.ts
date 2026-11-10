// Application state shared by the shell and every app window.

import type { Features, User } from "./types";

export type Theme = "dark" | "light";

export interface AppState {
  user: User | null;
  features: Features;
  csrfToken: string;
  siteName: string;
  version: string;
  maxUploadBytes: number;
  theme: Theme;
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

  constructor(initial?: Partial<AppState>) {
    this.state = {
      user: null,
      features: { ...DEFAULT_FEATURES },
      csrfToken: "",
      siteName: "RAST",
      version: "",
      maxUploadBytes: 10 * 1024 * 1024 * 1024,
      theme: readTheme(),
      sessionExpiresAt: 0,
      ...initial,
    };
    this.applyTheme();
  }

  get(): AppState {
    return this.state;
  }

  patch(partial: Partial<AppState>): void {
    this.state = { ...this.state, ...partial };
    if (partial.theme) {
      this.applyTheme();
      writeTheme(partial.theme);
    }
    for (const listener of this.listeners) {
      try {
        listener(this.state);
      } catch (error) {
        console.error("store listener failed", error);
      }
    }
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
    this.patch({ theme: this.state.theme === "dark" ? "light" : "dark" });
  }

  private applyTheme(): void {
    const root = document.documentElement;
    root.dataset.theme = this.state.theme;
    root.style.colorScheme = this.state.theme;
  }
}

function readTheme(): Theme {
  try {
    const stored = window.localStorage.getItem(THEME_KEY);
    if (stored === "light" || stored === "dark") return stored;
  } catch {
    /* storage disabled */
  }
  const prefersLight = window.matchMedia?.("(prefers-color-scheme: light)").matches ?? false;
  return prefersLight ? "light" : "dark";
}

function writeTheme(theme: Theme): void {
  try {
    window.localStorage.setItem(THEME_KEY, theme);
  } catch {
    /* ignore */
  }
}
