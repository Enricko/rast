// Pure helpers for the App Center "install/update preview" of native apps.
//
// (formatBytes is a pure helper too: no DOM is touched here.)
//
// The server inspects a .deb/AppImage/archive (or simulates the package
// transaction for a .deb) and returns a preview before anything is installed.
// These helpers normalize that JSON into the rows the UI shows and are
// unit-tested in `tests/native.test.ts` (no DOM). The server contract has
// evolved once already (flat fields -> `{app, candidates, deb, ...}`), so the
// readers below accept both shapes and degrade gracefully.

import { formatBytes } from "../../core/format";

export interface LinuxPreviewTransactionItem {
  name: string;
  version: string;
  oldVersion?: string | null;
}

export interface LinuxPreviewTransaction {
  installs?: LinuxPreviewTransactionItem[];
  removals?: LinuxPreviewTransactionItem[];
  upgrades?: LinuxPreviewTransactionItem[];
  downloadBytes?: number;
}

export interface LinuxPreviewLauncher {
  name?: string | null;
  argv?: string[];
  desktopEntry?: string | null;
  launchGuess?: boolean;
}

export interface LinuxPreviewApp {
  name?: string;
  version?: string | null;
  package?: string | null;
  description?: string | null;
  icon?: string | null;
  launcher?: LinuxPreviewLauncher | null;
  launchGuess?: boolean;
}

export interface LinuxPreviewPackage {
  name?: string;
  version?: string | null;
  preexisting?: boolean;
  transaction?: LinuxPreviewTransaction | null;
}

/** The server's `.deb` simulation result (shape may grow; read defensively). */
export interface LinuxPreviewDeb {
  transaction?: LinuxPreviewTransaction | null;
  package?: LinuxPreviewPackage | null;
  dependencies?: string[];
  [key: string]: unknown;
}

export interface LinuxPreviewCompatibility {
  ok?: boolean;
  reasons?: string[];
  warnings?: string[];
}

export interface LinuxPreviewSource {
  kind?: string;
  path?: string;
  url?: string;
}

export interface LinuxPreviewUpdate {
  source?: LinuxPreviewSource | null;
  latestVersion?: string | null;
  available?: boolean;
}

/**
 * Install/update preview returned by `POST /api/linux-apps/preview`.
 * `app`/`candidates`/`deb` mirror the current backend; the flat fields are the
 * older shape and are still read as a fallback.
 */
export interface LinuxInstallPreview {
  format?: string;
  fileName?: string;
  sizeBytes?: number | null;
  sha256?: string | null;
  source?: LinuxPreviewSource | null;
  architecture?: string | null;
  app?: LinuxPreviewApp | null;
  candidates?: LinuxPreviewLauncher[];
  needsChoice?: boolean;
  compatibility?: LinuxPreviewCompatibility | null;
  deb?: LinuxPreviewDeb | null;
  update?: LinuxPreviewUpdate | null;
  warnings?: string[];
  // Older shape (kept for compatibility).
  name?: string;
  version?: string | null;
  package?: LinuxPreviewPackage | string | null;
  launcher?: LinuxPreviewLauncher | null;
  dependencies?: string[];
  transaction?: LinuxPreviewTransaction | null;
  updateSource?: LinuxPreviewUpdate["source"] | null;
}

export interface PreviewIdentityRow {
  label: string;
  value: string;
}

export type TransactionKind = "install" | "remove" | "upgrade";

export interface TransactionRow {
  kind: TransactionKind;
  name: string;
  version: string;
  oldVersion: string | null;
}

export interface PreviewTransactionSummary {
  rows: TransactionRow[];
  installs: number;
  removals: number;
  upgrades: number;
  downloadBytes: number;
  /** True when applying the file changes the system's packages. */
  hasChanges: boolean;
  /** The package is already present on the host (upgrade/reinstall). */
  preexisting: boolean;
}

const orDash = (value: string | null | undefined): string => {
  const text = (value ?? "").trim();
  return text || "—";
};

/** Display name of the app the file would install. */
export function previewName(preview: LinuxInstallPreview): string {
  return (
    preview.app?.name?.trim() ||
    preview.name?.trim() ||
    preview.fileName?.trim() ||
    "This app"
  );
}

export function previewVersion(preview: LinuxInstallPreview): string | null {
  return preview.app?.version ?? preview.version ?? null;
}

export function previewFormat(preview: LinuxInstallPreview): string {
  return (preview.format ?? "").toUpperCase();
}

export function previewArchitecture(preview: LinuxInstallPreview): string | null {
  return preview.architecture ?? null;
}

export function previewFileName(preview: LinuxInstallPreview): string | null {
  return preview.fileName ?? null;
}

export function previewSizeBytes(preview: LinuxInstallPreview): number | null {
  return typeof preview.sizeBytes === "number" ? preview.sizeBytes : null;
}

/** The launcher candidate the server picked, if any. */
export function previewLauncher(preview: LinuxInstallPreview): LinuxPreviewLauncher | null {
  return preview.app?.launcher ?? preview.launcher ?? null;
}

/** Every launcher the server found (the admin must pick when `needsChoice`). */
export function previewCandidates(preview: LinuxInstallPreview): LinuxPreviewLauncher[] {
  const candidates = preview.candidates ?? [];
  if (candidates.length > 0) return candidates;
  const launcher = previewLauncher(preview);
  return launcher ? [launcher] : [];
}

/** True when the admin must pick which program starts the app. */
export function previewNeedsChoice(preview: LinuxInstallPreview): boolean {
  if (preview.needsChoice === true || preview.needsChoice === false) return preview.needsChoice;
  return previewCandidates(preview).length > 1 && !previewLauncher(preview);
}

function transactionOf(preview: LinuxInstallPreview): LinuxPreviewTransaction | null {
  return (
    preview.deb?.transaction ??
    preview.deb?.package?.transaction ??
    preview.transaction ??
    (typeof preview.package === "object" && preview.package ? preview.package.transaction ?? null : null)
  );
}

function packageOf(preview: LinuxInstallPreview): LinuxPreviewPackage | null {
  return (
    preview.deb?.package ??
    (typeof preview.package === "object" && preview.package ? preview.package : null) ??
    null
  );
}

/** Name of the system package the file installs (when it is a real package). */
export function previewPackageName(preview: LinuxInstallPreview): string | null {
  return packageOf(preview)?.name ?? preview.app?.package ?? null;
}

export function previewLauncherOwner(launcher: LinuxPreviewLauncher | null | undefined): string {
  if (!launcher) return "—";
  const argv = launcher.argv ?? [];
  if (argv.length > 0) return argv.join(" ");
  if (launcher.desktopEntry) return launcher.desktopEntry;
  if (launcher.name) return launcher.name;
  return "—";
}

/** Identity rows of an install preview (what will be installed and from where). */
export function previewIdentity(preview: LinuxInstallPreview): PreviewIdentityRow[] {
  const rows: PreviewIdentityRow[] = [
    { label: "File", value: orDash(previewFileName(preview) ?? previewName(preview)) },
    { label: "Format", value: orDash(previewFormat(preview)) },
    { label: "Version", value: orDash(previewVersion(preview)) },
    { label: "Architecture", value: orDash(previewArchitecture(preview)) },
  ];
  const size = previewSizeBytes(preview);
  if (size !== null && size > 0) rows.push({ label: "File size", value: formatBytes(size) });
  const source = preview.source;
  if (source?.kind === "url" && source.url) rows.push({ label: "Downloaded from", value: source.url });
  else if (source?.path) rows.push({ label: "File Station path", value: source.path });
  rows.push({ label: "Starts", value: previewLauncherOwner(previewLauncher(preview)) });
  const packageName = previewPackageName(preview);
  if (packageName) {
    const version = packageOf(preview)?.version ?? null;
    rows.push({ label: "Package", value: version ? `${packageName} ${version}` : packageName });
  }
  return rows;
}

/** Normalized package transaction (installs/removals/upgrades + download size). */
export function previewTransaction(preview: LinuxInstallPreview): PreviewTransactionSummary {
  const transaction = transactionOf(preview);
  const rows: TransactionRow[] = [];
  const push = (kind: TransactionKind, items: LinuxPreviewTransactionItem[] | undefined): void => {
    for (const item of items ?? []) {
      rows.push({ kind, name: item.name, version: item.version, oldVersion: item.oldVersion ?? null });
    }
  };
  push("remove", transaction?.removals);
  push("upgrade", transaction?.upgrades);
  push("install", transaction?.installs);
  return {
    rows,
    installs: (transaction?.installs ?? []).length,
    removals: (transaction?.removals ?? []).length,
    upgrades: (transaction?.upgrades ?? []).length,
    downloadBytes: typeof transaction?.downloadBytes === "number" ? transaction.downloadBytes : 0,
    hasChanges: rows.length > 0,
    preexisting: packageOf(preview)?.preexisting === true,
  };
}

/**
 * Compatibility result: blocking reasons fail; warnings are shown but do not
 * block (the server reports them separately).
 */
export function previewCompatibility(preview: LinuxInstallPreview): {
  ok: boolean;
  reasons: string[];
  warnings: string[];
} {
  const compatibility = preview.compatibility;
  const warnings = (compatibility?.warnings ?? [])
    .concat(preview.warnings ?? [])
    .filter((value): value is string => typeof value === "string" && value.trim() !== "");
  const reasons = (compatibility?.reasons ?? []).filter(
    (value): value is string => typeof value === "string" && value.trim() !== "",
  );
  return { ok: compatibility?.ok !== false && reasons.length === 0, reasons, warnings };
}

/** Extra dependencies that would be installed alongside the app. */
export function previewDependencies(preview: LinuxInstallPreview): string[] {
  const dependencies = preview.deb?.dependencies ?? preview.dependencies ?? [];
  return dependencies.filter((value): value is string => typeof value === "string" && value.trim() !== "");
}

/** One-line summary for confirm buttons and toasts. */
export function previewSummary(preview: LinuxInstallPreview): string {
  const name = previewName(preview);
  const version = previewVersion(preview) ? ` ${previewVersion(preview)}` : "";
  const transaction = previewTransaction(preview);
  const changes = transaction.removals + transaction.upgrades + transaction.installs;
  if (changes > 0) {
    return `Install ${name}${version}? This changes ${changes} package${changes === 1 ? "" : "s"}.`;
  }
  return `Install ${name}${version}?`;
}
