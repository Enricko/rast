// Pure helpers of the storage usage views (ADR 0005, section 6): the usage
// breakdown of File Station and the quota labels of the Control Panel.

import type { UsageStats } from "./types";

const LOCATION_LABELS: Record<string, string> = {
  files: "Files",
  apps: "Apps",
  caches: "Caches",
  trash: "Recycle bin",
  shared: "In shared folders",
};

/** The chips under the usage bar: by location when apps, caches or shared
 * folders use space (one-HOME accounts), else by type. At most four. */
export function usageChips(usage: UsageStats): Array<[string, number]> {
  const locations = Object.entries(usage.byLocation ?? {}).filter(
    ([key, bytes]) => bytes > 0 && key !== "trash" && key in LOCATION_LABELS,
  );
  if (locations.some(([key]) => key !== "files")) {
    return locations
      .sort((a, b) => b[1] - a[1])
      .slice(0, 4)
      .map(([key, bytes]): [string, number] => [LOCATION_LABELS[key] ?? key, bytes]);
  }
  return Object.entries(usage.byType ?? {})
    .sort((a, b) => b[1] - a[1])
    .slice(0, 4);
}

/** What a quota backend limits, in one sentence. */
export function quotaBackendHint(backend: "filesystem" | "rast"): string {
  return backend === "filesystem"
    ? "Enforced by the filesystem: File Station, Terminal and every app."
    : "Enforced by RAST: File Station and App Center only; apps and Terminal are not limited.";
}

/** What a shared folder's "enforced by" label means. */
export function shareLimitHint(enforcedBy: string | undefined): string {
  return enforcedBy === "filesystem, best effort"
    ? "Group quota on the shared folder's read-write group, plus RAST's check. Files moved in with their old group are not counted."
    : "Checked by RAST for File Station uploads, copies and moves. Apps and Terminal are not limited.";
}
