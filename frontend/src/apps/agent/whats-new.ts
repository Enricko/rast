// What is new in each AI Agent version, newest first. The window shows the
// versions an account has not seen yet, once (POST /api/ai/whats-new). A
// release adds its entry here with the version in src/ai/app.rs (a Rust
// test checks the newest entry matches it).

export interface Release {
  version: string;
  date: string;
  items: string[];
}

export const WHATS_NEW: Release[] = [
  {
    version: "1.3.0",
    date: "2026-11-27",
    items: [
      "Update from here: when a newer AI Agent is ready, administrators get an Update button under the chat list, and the window opens the new version by itself.",
      "This note, shown once per version, says what changed.",
      "Quota has bars for every account. Accounts whose provider reports nothing are measured against the limits you set in RAST, or against about where the provider last said stop.",
      "Limits per account (Quota → Limits): at its 5-hour or weekly limit an account rests and the next account answers.",
      "ChatGPT Codex plans are asked for their limits as Quota opens.",
    ],
  },
  {
    version: "1.2.0",
    date: "2026-11-27",
    items: [
      "Round robin is a switch per provider, with Sticky, and Round robin for all turns it on for every provider.",
      "Quota is a tracker like 9router's: filters, accounts by provider, bars coloured by what is left, Refresh, Edit and Remove.",
    ],
  },
  {
    version: "1.1.0",
    date: "2026-11-27",
    items: ["Several accounts per provider: when one reaches its limit, the next account answers.", "Settings → Quota shows each account's limits and rest."],
  },
  {
    version: "1.0.0",
    date: "2026-11-26",
    items: ["AI Agent installs and updates from App Center, without a RAST update."],
  },
];

/** Compare "1.10.0" and "1.9.2" by their numbers. */
export function compareVersions(a: string, b: string): number {
  const parts = (v: string) => v.split(/[.-]/).map((p) => Number.parseInt(p, 10) || 0);
  const [x, y] = [parts(a), parts(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i += 1) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d) return d < 0 ? -1 : 1;
  }
  return 0;
}

/**
 * The releases to show an account at `current`: those newer than the one it
 * last saw (at most three), or `current`'s alone when it saw none; empty
 * once it saw `current`.
 */
export function whatsNewSince(seen: string | null | undefined, current: string, releases: Release[] = WHATS_NEW): Release[] {
  if (!current || (seen && compareVersions(seen, current) >= 0)) return [];
  const upTo = releases.filter((r) => compareVersions(r.version, current) <= 0);
  const shown = seen ? upTo.filter((r) => compareVersions(r.version, seen) > 0) : upTo.filter((r) => r.version === current);
  return shown.slice(0, 3);
}
