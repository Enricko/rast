// Pure helpers for the Package Center (unit-tested in tests/packages.test.ts).

import type { JobHistory, PackageView, SystemInstall, VersionChoice } from "./types";

export type PackageFilter = "all" | "installed" | "updates";

/** Tile order: updates first, then installed, then the rest; by name within each. */
export function sortForTiles(packages: PackageView[]): PackageView[] {
  const rank = (p: PackageView) => (p.updates.length > 0 ? 0 : p.installed.length > 0 || p.system ? 1 : 2);
  return [...packages].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}

/** Packages shown by a sidebar view, filtered by a free-text query. */
export function filterPackages(packages: PackageView[], view: PackageFilter, query = ""): PackageView[] {
  const q = query.trim().toLowerCase();
  return packages.filter((p) => {
    if (view === "installed" && p.installed.length === 0 && !p.system) return false;
    if (view === "updates" && p.updateState === "none") return false;
    if (!q) return true;
    return [p.name, p.id, p.description, p.publisher.name, p.sourceRepo ?? ""].some((v) => v.toLowerCase().includes(q));
  });
}

/** What a package tile offers: one action and one short line. */
export type TileAction = "update" | "install" | "installed" | "unavailable" | "none";

export interface TileState {
  action: TileAction;
  /** One short line under the name, e.g. "8.5.4 → 8.5.12" or "8.5.4 · System". */
  subtitle: string;
  /** Version the action installs (update and install only). */
  version?: string;
  /** Whether the installed version becomes the default (terminal command). */
  makeDefault?: boolean;
}

/** The version a tile shows as installed: the default, else the first, else the system's. */
export function installedLabel(p: PackageView): string | null {
  const shown = p.installed.find((v) => v.isDefault) ?? p.installed[0];
  if (shown) {
    const more = p.installed.length - 1;
    return more > 0 ? `${shown.version} +${more}` : shown.version;
  }
  if (p.system) return `${p.system.version ?? "Unknown version"} · System`;
  return null;
}

export function tileState(p: PackageView): TileState {
  const update = p.updates[0];
  if (update) {
    return { action: "update", subtitle: `${update.from} → ${update.to}`, version: update.to, makeDefault: update.replacesDefault };
  }
  const installed = installedLabel(p);
  if (installed) return { action: "installed", subtitle: installed };
  const next = p.official ? suggestedVersion(p) : null;
  if (next) return { action: "install", subtitle: next.version, version: next.version, makeDefault: true };
  if (p.unavailableReason) return { action: "unavailable", subtitle: "Not available" };
  return { action: "none", subtitle: "Not installed" };
}

/** The coloured status line under a package's name on its page (text, not only colour). */
export function statusLine(p: PackageView): { kind: "update" | "installed" | "available" | "unavailable"; text: string } {
  const update = p.updates[0];
  if (update) return { kind: "update", text: `Update available: ${update.from}${update.fromSystem ? " (system)" : ""} → ${update.to}` };
  if (p.installed.length > 0 || p.system) {
    const where = p.installed.length === 0 ? " (system)" : "";
    return { kind: "installed", text: p.latest ? `Installed${where} · ${upToDateText(p).replace(/^Up/, "up")}` : `Installed${where}` };
  }
  if (p.official && suggestedVersion(p)) return { kind: "available", text: "Not installed" };
  return { kind: "unavailable", text: p.unavailableReason ? "Not available for this server" : "Not installed" };
}

/** "Up to date", naming the release line when a newer line exists. */
export function upToDateText(p: PackageView): string {
  const current = p.installed.find((v) => v.isDefault)?.channel ?? (p.system?.version ? channelOf(p.system.version, p.versions) : null);
  const line = current ? p.versions.find((v) => v.channel === current) : undefined;
  if (!line || !p.latest || p.latest.channel === line.channel) return "Up to date";
  return `Up to date in ${line.channelLabel || line.channel}`;
}

/** The catalog channel a version belongs to (longest matching channel id). */
function channelOf(version: string, versions: VersionChoice[]): string | null {
  let best: string | null = null;
  for (const v of versions) {
    const c = v.channel;
    if ((version === c || version.startsWith(`${c}.`)) && (!best || c.length > best.length)) best = c;
  }
  return best;
}

/** Host part of a URL for compact links ("nodejs.org"). */
export function linkLabel(url: string): string {
  try {
    return new URL(url).host.replace(/^www\./, "");
  } catch {
    return url;
  }
}

/** Short status line for a package card. */
export function updateSummary(p: PackageView): { kind: "ready" | "detected" | "none"; text: string } {
  const ready = p.updates[0];
  if (ready) {
    const more = p.updates.length > 1 ? ` (+${p.updates.length - 1} more)` : "";
    const from = ready.fromSystem ? `${ready.from} (system)` : ready.from;
    return { kind: "ready", text: `Update ready: ${from} → ${ready.to}${more}` };
  }
  const detected = p.detected[0];
  if (p.updateState === "detected" && detected) {
    return {
      kind: "detected",
      text:
        detected.source === "github"
          ? `New release ${detected.version} published — review before installing`
          : `New upstream ${detected.version} detected — ${stateLabel(detected.state)}`,
    };
  }
  return { kind: "none", text: "" };
}

export function stateLabel(state: string): string {
  switch (state) {
    case "pending":
      return "waiting for the catalog workflow";
    case "verifying":
      return "being verified";
    case "building":
      return "being built";
    case "failed":
      return "verification failed";
    case "review":
      return "needs review";
    default:
      return state || "not verified yet";
  }
}

/** Versions grouped by channel, newest channel first (input is newest-first). */
export function groupByChannel(versions: VersionChoice[]): Array<{ channel: string; label: string; versions: VersionChoice[] }> {
  const groups: Array<{ channel: string; label: string; versions: VersionChoice[] }> = [];
  for (const v of versions) {
    let group = groups.find((g) => g.channel === v.channel);
    if (!group) {
      const status = v.channelStatus ? ` · ${v.channelStatus}` : "";
      group = { channel: v.channel, label: `${v.channelLabel || v.channel}${status}`, versions: [] };
      groups.push(group);
    }
    group.versions.push(v);
  }
  return groups;
}

/** Label of a version in the picker. */
export function versionOptionLabel(v: VersionChoice): string {
  const parts = [v.version];
  if (v.revision > 1) parts.push(`rev ${v.revision}`);
  if (v.prerelease) parts.push("pre-release");
  if (v.installedRevision != null) {
    parts.push(v.installedRevision < v.revision ? "installed (older revision)" : "installed");
  }
  if (!v.compatible) parts.push("unavailable");
  return parts.join(" · ");
}

/** Where a system runtime comes from, e.g. "apt · php8.5-cli" or "manual install". */
export function systemSourceLabel(s: Pick<SystemInstall, "source" | "sourcePackage">): string {
  if (s.source === "apt") return s.sourcePackage ? `apt · ${s.sourcePackage}` : "apt";
  if (s.source === "snap") return "snap";
  return "manual install";
}

/** The version a fresh install would pick: newest compatible, not installed, stable. */
export function suggestedVersion(p: PackageView): VersionChoice | null {
  const fresh = p.versions.filter((v) => v.compatible && v.installedRevision == null);
  return fresh.find((v) => !v.prerelease) ?? fresh[0] ?? null;
}

export function jobActionLabel(job: Pick<JobHistory, "action" | "version">): string {
  const verb = job.action === "update" ? "Update to" : job.action === "reinstall" ? "Reinstall" : "Install";
  return `${verb} ${job.version}`;
}

export function jobFraction(progress: number, total: number): number {
  if (!total || total <= 0) return 0;
  return Math.max(0, Math.min(1, progress / total));
}

export function shortSha(sha: string | undefined | null): string {
  return sha ? `${sha.slice(0, 12)}…` : "";
}

/** Stable accent colour for a package monogram. */
export function monogramColor(id: string): string {
  const known: Record<string, string> = {
    python: "#3776AB",
    php: "#777BB4",
    go: "#00ADD8",
    nodejs: "#3C873A",
    lua: "#2C2D72",
  };
  if (known[id]) return known[id];
  let hash = 0;
  for (const ch of id) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return `hsl(${hash % 360} 45% 42%)`;
}

export function monogram(name: string, id = ""): string {
  const known: Record<string, string> = { nodejs: "JS", python: "Py", php: "PHP", go: "Go", lua: "Lua" };
  if (known[id]) return known[id];
  const letters = name.replace(/[^A-Za-z0-9]/g, "");
  if (!letters) return "?";
  return letters.length > 2 && /^[A-Z]{2,}/.test(letters) ? letters.slice(0, 3) : letters.slice(0, 2).replace(/^./, (c) => c.toUpperCase());
}
