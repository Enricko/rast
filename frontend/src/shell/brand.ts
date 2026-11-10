// Document branding: title and favicon derived from the server-provided
// product name (and theme accent). No external assets are loaded.

import type { Theme } from "../core/store";

const FAVICON: Record<Theme, { outer: string; inner: string }> = {
  dark: { outer: "#4f8cff", inner: "#0b1222" },
  light: { outer: "#2563eb", inner: "#ffffff" },
};

export function setDocumentTitle(siteName: string): void {
  const name = siteName.trim() || "RAST";
  document.title = name === "RAST" ? "RAST · Server Administration" : `${name} · Server Administration`;
}

/** Build the geometric hex logomark as an inline SVG data URI. */
export function buildFaviconUrl(theme: Theme): string {
  const colors = FAVICON[theme];
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<rect width="64" height="64" rx="15" fill="${colors.inner}"/>
<path d="M32 8 52 19.6v24.8L32 56 12 44.4V19.6z" fill="none" stroke="${colors.outer}" stroke-width="3.4" stroke-linejoin="round"/>
<path d="M32 22.4 40.4 27.4v10L32 42.4 23.6 37.4v-10z" fill="${colors.outer}"/>
</svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

let currentFaviconTheme: Theme | null = null;

export function setFavicon(theme: Theme): void {
  if (currentFaviconTheme === theme) return;
  currentFaviconTheme = theme;
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
  document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]').forEach((meta) => {
    meta.content = theme === "dark" ? "#070c18" : "#eef2fa";
  });
}

export function applyDocumentBrand(siteName: string, theme: Theme): void {
  setDocumentTitle(siteName);
  setFavicon(theme);
}
