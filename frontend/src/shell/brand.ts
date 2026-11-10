// Document branding: title and favicon derived from the live semantic tokens
// (accent preset + theme). No external assets are loaded.

import type { Theme } from "../core/store";

type ThemeLike = Theme | "system";

function tokenColor(name: string, fallback: string): string {
  try {
    const value = window.getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    if (/^#[0-9a-fA-F]{3,8}$/.test(value)) return value;
    if (value.startsWith("rgb")) return value;
  } catch {
    /* styling not ready yet */
  }
  return fallback;
}

export function setDocumentTitle(siteName: string): void {
  const name = siteName.trim() || "RAST";
  document.title = name === "RAST" ? "RAST · Server Administration" : `${name} · Server Administration`;
}

/** Build the geometric hex logomark as an inline SVG data URI. */
export function buildFaviconUrl(theme?: ThemeLike): string {
  const accent = tokenColor("--accent", theme === "dark" ? "#7cb3f4" : "#2563eb");
  const surface = tokenColor("--surface", theme === "dark" ? "#20262e" : "#ffffff");
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<rect width="64" height="64" rx="15" fill="${surface}"/>
<path d="M32 8 52 19.6v24.8L32 56 12 44.4V19.6z" fill="none" stroke="${accent}" stroke-width="3.4" stroke-linejoin="round"/>
<path d="M32 22.4 40.4 27.4v10L32 42.4 23.6 37.4v-10z" fill="${accent}"/>
</svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

let currentFaviconKey = "";

export function setFavicon(theme?: ThemeLike): void {
  const accent = tokenColor("--accent", "");
  const surface = tokenColor("--surface", "");
  const key = `${theme ?? ""}|${accent}|${surface}`;
  if (currentFaviconKey === key) return;
  currentFaviconKey = key;
  const url = buildFaviconUrl(theme);
  for (const selector of ['link[rel="icon"]', 'link[rel="shortcut icon"]'] as const) {
    let link = document.querySelector<HTMLLinkElement>(selector);
    if (!link) {
      link = document.createElement("link");
      link.rel = "icon";
      link.type = "image/svg+xml";
      document.head.appendChild(link);
    }
    link.href = url;
  }
  const chrome = tokenColor("--chrome", theme === "dark" ? "#232a32" : "#303740");
  document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]').forEach((meta) => {
    meta.content = chrome;
  });
}

export function applyDocumentBrand(siteName: string, theme: Theme): void {
  setDocumentTitle(siteName);
  setFavicon(theme);
}
