/**
 * The `warnings` of a finished backup job's result: what the backup left
 * out, for example HOMEs in one-HOME mode, which the service cannot read
 * until Backup Center reads HOMEs through the helper.
 */
export function backupWarnings(result: unknown): string[] {
  if (!result || typeof result !== "object") return [];
  const warnings = (result as { warnings?: unknown }).warnings;
  return Array.isArray(warnings) ? warnings.filter((item): item is string => typeof item === "string") : [];
}
