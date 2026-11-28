// Package Center apps on the desktop: `GET /api/appfiles` lists the app
// windows this account may open; each becomes an app definition whose
// `mount` loads the package's styles and window module (served by RAST at a
// versioned path) and hands it the window context and the SDK.

import type { ApiClient } from "./api";
import type { AppDefinition, AppInstance } from "./app";
import { setPackageAppIcon } from "./icons";
import { sdk } from "./sdk";

export interface PackageWindow {
  appId: string;
  packageId: string;
  name: string;
  description: string;
  version: string;
  entry: string;
  styles: string[];
  sdk: number;
  singleton?: boolean;
  adminOnly?: boolean;
  defaultSize?: { w: number; h: number } | null;
  minSize?: { w: number; h: number } | null;
  /** The package's icon, shown wherever the app's icon is. */
  icon?: { mediaType: string; data: string } | null;
}

interface WindowModule {
  default?: { mount?: (ctx: unknown, sdk: unknown) => AppInstance | void | Promise<AppInstance | void> };
}

const styleLinks = new Map<string, Promise<void>>();

/** Add a style sheet once; an app's sheets of other versions leave. */
function loadStyle(appId: string, url: string): Promise<void> {
  const known = styleLinks.get(url);
  if (known) return known;
  document.querySelectorAll<HTMLLinkElement>(`link[data-app-style="${CSS.escape(appId)}"]`).forEach((link) => {
    if (link.getAttribute("href") !== url && !link.dataset.keep) {
      styleLinks.delete(link.getAttribute("href") ?? "");
      link.remove();
    }
  });
  const loading = new Promise<void>((resolve, reject) => {
    const link = document.createElement("link");
    link.rel = "stylesheet";
    link.href = url;
    link.dataset.appStyle = appId;
    link.onload = () => resolve();
    link.onerror = () => {
      styleLinks.delete(url);
      link.remove();
      reject(new Error(`could not load ${url}`));
    };
    document.head.appendChild(link);
  });
  styleLinks.set(url, loading);
  return loading;
}

export function packageAppDefinition(window: PackageWindow): AppDefinition {
  setPackageAppIcon(window.appId, window.icon);
  return {
    id: window.appId,
    name: window.name,
    icon: "package",
    description: window.description,
    ...(window.adminOnly ? { adminOnly: true } : {}),
    ...(window.singleton ? { singleton: true } : {}),
    ...(window.defaultSize ? { defaultSize: window.defaultSize } : {}),
    ...(window.minSize ? { minSize: window.minSize } : {}),
    packageVersion: window.version,
    async mount(ctx) {
      // The module reads the SDK as it loads: publish it first.
      const lent = sdk();
      await Promise.all(window.styles.map((url) => loadStyle(window.appId, url)));
      const module = (await import(/* @vite-ignore */ window.entry)) as WindowModule;
      const app = module.default;
      if (!app || typeof app.mount !== "function") throw new Error(`${window.name} has no window to show`);
      return app.mount(ctx, lent);
    },
  };
}

/** The app windows of installed Package Center apps (empty when the list cannot be read). */
export async function fetchPackageWindows(api: ApiClient): Promise<PackageWindow[]> {
  try {
    const body = await api.get<{ apps: PackageWindow[] }>("/api/appfiles");
    return Array.isArray(body.apps) ? body.apps.filter((app) => app.sdk === 1) : [];
  } catch {
    return [];
  }
}
