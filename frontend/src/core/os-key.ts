// The OS key opens the start menu, like on a real desktop: tap ⌘ Cmd on a Mac,
// the Windows key on Windows (Super on Linux). It is a *tap*: the menu opens
// when the key is released without any other key, click or scroll while it
// was held, so ⌘C, ⌘V, Win+Shift+S, ⌘-click … never open it.
//
// Note: Windows (and many Linux desktops) also open their own menu for that
// key; a web page cannot stop that outside full screen.

export type Platform = "mac" | "windows" | "other";

interface NavigatorLike {
  platform?: string;
  userAgent?: string;
  userAgentData?: { platform?: string };
}

export function detectPlatform(nav: NavigatorLike | undefined = typeof navigator === "undefined" ? undefined : navigator): Platform {
  const name = `${nav?.userAgentData?.platform ?? ""} ${nav?.platform ?? ""} ${nav?.userAgent ?? ""}`;
  if (/mac|iphone|ipad/i.test(name)) return "mac";
  if (/win/i.test(name)) return "windows";
  return "other";
}

/** The OS key's name, as printed on the keyboard (`short` for hints). */
export function osKeyLabel(platform: Platform = detectPlatform(), short = false): string {
  if (platform === "mac") return short ? "⌘" : "⌘ Cmd";
  if (platform === "windows") return short ? "Win" : "Windows key";
  return "Super";
}

/**
 * How the start menu opens from the keyboard, for hints. Linux desktops keep
 * Super for themselves, so there the hint is Ctrl+Space (a tap still works
 * where the desktop lets it through).
 */
export function startMenuHint(platform: Platform = detectPlatform()): { short: string; tip: string } {
  if (platform === "other") return { short: "Ctrl+Space", tip: "Ctrl+Space" };
  return { short: osKeyLabel(platform, true), tip: `${osKeyLabel(platform)} or Ctrl+Space` };
}

/** A tap held longer than this is not a tap (the combination was abandoned). */
export const OS_KEY_TAP_MS = 1200;

interface KeyLike {
  key: string;
  repeat?: boolean;
  timeStamp: number;
  ctrlKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

/** The tap detector (pure, fed with key events; tested on its own). */
export class OsKeyTap {
  private downAt: number | null = null;

  keydown(event: KeyLike): void {
    if (event.key === "Meta" || event.key === "OS") {
      if (event.repeat || this.downAt !== null) return;
      this.downAt = event.ctrlKey || event.altKey || event.shiftKey ? null : event.timeStamp;
      return;
    }
    this.downAt = null; // any other key: it is a combination
  }

  /** True when this release completes a tap. */
  keyup(event: KeyLike): boolean {
    if (event.key !== "Meta" && event.key !== "OS") return false;
    const downAt = this.downAt;
    this.downAt = null;
    if (downAt === null) return false;
    if (event.ctrlKey || event.altKey || event.shiftKey) return false;
    return event.timeStamp - downAt <= OS_KEY_TAP_MS;
  }

  /** A click, scroll or focus change while held: not a tap. */
  cancel(): void {
    this.downAt = null;
  }
}

/**
 * Call `onTap` when the OS key is tapped in this window. Listens in the
 * capture phase so terminals and editors cannot swallow the key. Returns the
 * disposer.
 */
export function watchOsKeyTap(onTap: () => void): () => void {
  const tap = new OsKeyTap();
  const down = (event: KeyboardEvent): void => tap.keydown(event);
  const up = (event: KeyboardEvent): void => {
    if (tap.keyup(event)) onTap();
  };
  const cancel = (): void => tap.cancel();
  window.addEventListener("keydown", down, true);
  window.addEventListener("keyup", up, true);
  window.addEventListener("pointerdown", cancel, true);
  window.addEventListener("wheel", cancel, { capture: true, passive: true });
  window.addEventListener("blur", cancel);
  document.addEventListener("visibilitychange", cancel);
  return () => {
    window.removeEventListener("keydown", down, true);
    window.removeEventListener("keyup", up, true);
    window.removeEventListener("pointerdown", cancel, true);
    window.removeEventListener("wheel", cancel, true);
    window.removeEventListener("blur", cancel);
    document.removeEventListener("visibilitychange", cancel);
  };
}
