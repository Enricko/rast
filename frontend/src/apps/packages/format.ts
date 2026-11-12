// Pure helpers for the Package Center (unit-tested in tests/packages.test.ts).

import type { JobHistory, PackageView, SystemInstall, VersionChoice } from "./types";

export type PackageFilter = "all" | "installed" | "updates";

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
