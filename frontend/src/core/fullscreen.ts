// Full screen for the whole RAST desktop: the taskbar button (bottom right),
// Ctrl+Shift+F (⌘⇧F on a Mac) and the desktop and taskbar menus. It uses the
// browser's Fullscreen API (webkit-prefixed on older Safari); where the
// browser offers none (iPhone) the controls stay hidden. Esc also leaves full
// screen (the browser's own key).

import type { Platform } from "./os-key";

type FullscreenDocument = Document & {
  webkitFullscreenElement?: Element | null;
  webkitFullscreenEnabled?: boolean;
  webkitExitFullscreen?: () => Promise<void> | void;
};
type FullscreenElement = HTMLElement & { webkitRequestFullscreen?: () => Promise<void> | void };

interface KeyLike {
  key: string;
  code?: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  repeat?: boolean;
}

/** Whether this browser can show a page full screen. */
export function fullscreenSupported(doc: Document = document): boolean {
  const d = doc as FullscreenDocument;
  return Boolean(d.fullscreenEnabled || d.webkitFullscreenEnabled);
}

export function isFullscreen(doc: Document = document): boolean {
  const d = doc as FullscreenDocument;
  return Boolean(d.fullscreenElement || d.webkitFullscreenElement);
}

/** Enter or leave full screen (call it from a click or key press). */
export async function toggleFullscreen(doc: Document = document): Promise<void> {
  const d = doc as FullscreenDocument;
  if (isFullscreen(doc)) {
    if (d.exitFullscreen) await d.exitFullscreen();
    else await d.webkitExitFullscreen?.();
    return;
  }
  const root = doc.documentElement as FullscreenElement;
  if (root.requestFullscreen) await root.requestFullscreen({ navigationUI: "hide" });
  else await root.webkitRequestFullscreen?.();
}

/** Call `listener` whenever the page enters or leaves full screen. */
export function onFullscreenChange(listener: () => void, doc: Document = document): () => void {
  doc.addEventListener("fullscreenchange", listener);
  doc.addEventListener("webkitfullscreenchange", listener);
  return () => {
    doc.removeEventListener("fullscreenchange", listener);
    doc.removeEventListener("webkitfullscreenchange", listener);
  };
}

/** Ctrl+Shift+F, or ⌘⇧F on a Mac (Alt and held-down repeats left out). */
export function isFullscreenShortcut(event: KeyLike, platform: Platform): boolean {
  if (event.repeat || event.altKey || !event.shiftKey) return false;
  const modifier = platform === "mac" ? event.metaKey || event.ctrlKey : event.ctrlKey && !event.metaKey;
  return Boolean(modifier) && (event.code === "KeyF" || event.key.toLowerCase() === "f");
}

/** The shortcut as printed in hints. */
export function fullscreenShortcutLabel(platform: Platform): string {
  return platform === "mac" ? "⌘⇧F" : "Ctrl+Shift+F";
}
