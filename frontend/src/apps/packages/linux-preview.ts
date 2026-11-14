// Pure helpers for the App Center "Install Linux App" workflow: the server
// preview, the inferred install plan (fresh/register/duplicate), saved user
// data, availability choices, the supported package file types and the install
// job's stage mapping.
//
// (formatBytes is a pure helper too: no DOM is touched here.)
//
// The server inspects a .deb/portable archive (or simulates the package
// transaction for a .deb) and returns a preview before anything is installed.
// These helpers normalize that JSON into the rows the UI shows and are
// unit-tested in `tests/native.test.ts` (no DOM). The server contract has
// evolved twice already (flat fields -> `{app, candidates, deb, ...}` ->
// additive `install`/`profile`/`availability`), so the readers below accept
// every shape and degrade gracefully.

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
  /** Nested metadata on older servers, a plain package name on newer ones. */
  package?: LinuxPreviewPackage | string | null;
  dependencies?: string[];
  /** The helper's flat flag: this package is already installed on the host. */
  preexisting?: boolean;
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

/** Who can see and open an app: every signed-in user, or administrators. */
export type LinuxAppVisibility = "users" | "admins";

/**
 * An existing RAST app entry the uploaded file probably belongs to
 * (`preview.duplicate`; also `preview.install.existingApp`).
 */
export interface LinuxPreviewExistingApp {
  id?: string | null;
  appId?: string | null;
  name?: string | null;
  version?: string | null;
  status?: string | null;
  backend?: string | null;
  canUpdate?: boolean | null;
  matchedBy?: string | null;
  reason?: string | null;
}

/** New (additive) backend fields the App Center uses when present. */
export interface LinuxPreviewProfile {
  exists?: boolean;
  users?: number;
}

export interface LinuxPreviewInstall {
  mode?: string | null;
  hostPackageInstalled?: boolean | null;
  existingApp?: LinuxPreviewExistingApp | null;
}

export interface LinuxPreviewAvailability {
  default?: string | null;
  options?: string[] | null;
}

/**
 * Install/update preview returned by `POST /api/linux-apps/preview`.
 * `app`/`candidates`/`deb` mirror the current backend; the flat fields are the
 * older shape and are still read as a fallback. `profile`/`install`/
 * `availability`/`duplicate` are additive fields a newer server provides; the
 * readers below fall back to the older shape when they are missing.
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
  /** Host (server) details, not the package's own architecture. */
  hostArchitecture?: string | null;
  hostDistribution?: string | null;
  duplicate?: LinuxPreviewExistingApp | null;
  profile?: LinuxPreviewProfile | null;
  install?: LinuxPreviewInstall | null;
  availability?: LinuxPreviewAvailability | null;
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
  return preview.deb?.transaction ?? packageOf(preview)?.transaction ?? preview.transaction ?? null;
}

function packageOf(preview: LinuxInstallPreview): LinuxPreviewPackage | null {
  // `deb.package` can also be a plain package name on newer servers; only an
  // object carries the nested metadata this module reads.
  const debPackage = preview.deb?.package;
  if (debPackage && typeof debPackage === "object") return debPackage;
  return typeof preview.package === "object" && preview.package ? preview.package : null;
}

/** True when the `.deb`'s package is already installed on the host. */
function packagePreexistingOf(preview: LinuxInstallPreview): boolean {
  if (packageOf(preview)?.preexisting === true) return true;
  // Newer previews put the helper's `preexisting` flag on `deb` itself.
  return preview.deb?.preexisting === true;
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
    preexisting: packagePreexistingOf(preview),
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

// -------------------------------------------------- install-workflow readers
//
// These normalize the additive preview fields of the newer server (profile,
// install, availability, duplicate, host details) and the client-side upload
// stage mapping. Like the readers above they never throw on a missing or
// malformed field: an older server simply degrades to the older shape.

function objectOf<T extends object>(value: unknown): T | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as T) : null;
}

function textOrNull(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const text = value.trim();
  return text === "" ? null : text;
}

/** Normalized existing-app entry (`preview.duplicate` / `preview.install.existingApp`). */
export interface PreviewExistingApp {
  id: string | null;
  name: string | null;
  version: string | null;
  status: string | null;
  backend: string | null;
  canUpdate: boolean | null;
  matchedBy: string | null;
  reason: string | null;
}

function existingAppOf(value: unknown): PreviewExistingApp | null {
  const raw = objectOf<LinuxPreviewExistingApp>(value);
  if (!raw) return null;
  const id = textOrNull(raw.appId) ?? textOrNull(raw.id);
  const normalized: PreviewExistingApp = {
    id,
    name: textOrNull(raw.name),
    version: textOrNull(raw.version),
    status: textOrNull(raw.status),
    backend: textOrNull(raw.backend),
    canUpdate: typeof raw.canUpdate === "boolean" ? raw.canUpdate : null,
    matchedBy: textOrNull(raw.matchedBy),
    reason: textOrNull(raw.reason),
  };
  if (!normalized.id && !normalized.name) return null;
  return normalized;
}

/** The existing RAST app entry the file probably belongs to, if the server found one. */
export function previewDuplicate(preview: LinuxInstallPreview): PreviewExistingApp | null {
  return (
    existingAppOf(preview.duplicate) ??
    existingAppOf(objectOf<LinuxPreviewInstall>(preview.install)?.existingApp)
  );
}

/** Saved per-user app data already on the server (`preview.profile`). */
export interface PreviewSavedProfile {
  exists: boolean;
  users: number;
}

export function previewProfile(preview: LinuxInstallPreview): PreviewSavedProfile {
  const raw = objectOf<LinuxPreviewProfile>(preview.profile);
  const users =
    typeof raw?.users === "number" && Number.isFinite(raw.users) && raw.users > 0
      ? Math.floor(raw.users)
      : 0;
  return { exists: raw?.exists === true || users > 0, users };
}

/** "2 user profiles will be preserved" / "Saved app data will be preserved" / "None yet". */
export function previewProfileSummary(profile: PreviewSavedProfile): string {
  if (profile.users > 0) {
    return `${profile.users} user profile${profile.users === 1 ? "" : "s"} will be preserved`;
  }
  return profile.exists ? "Saved app data will be preserved" : "None yet";
}

export type InstallMode = "fresh" | "register" | "duplicate";

/** What installing the file would do, normalized from `preview.install`. */
export interface PreviewInstallPlan {
  mode: InstallMode;
  /** The server's package manager already has this package installed. */
  hostPackageInstalled: boolean;
  /** An existing RAST app entry for the same app, if any. */
  existingApp: PreviewExistingApp | null;
}

function installModeOf(value: unknown): InstallMode | null {
  return value === "fresh" || value === "register" || value === "duplicate" ? value : null;
}

/**
 * The install plan. Uses `preview.install` when the server provides it and
 * falls back to the older `duplicate` + `.deb` transaction shape otherwise.
 */
export function previewInstallPlan(preview: LinuxInstallPreview): PreviewInstallPlan {
  const raw = objectOf<LinuxPreviewInstall>(preview.install);
  const transaction = previewTransaction(preview);
  const existingApp = previewDuplicate(preview);
  const hostPackageInstalled =
    typeof raw?.hostPackageInstalled === "boolean"
      ? raw.hostPackageInstalled
      : transaction.preexisting;
  const mode =
    installModeOf(raw?.mode) ?? (existingApp ? "duplicate" : hostPackageInstalled ? "register" : "fresh");
  return { mode, hostPackageInstalled, existingApp };
}

/** Primary action label: registering a package the host already has is not a fresh install. */
export function previewInstallActionLabel(plan: PreviewInstallPlan): string {
  return plan.hostPackageInstalled || plan.mode === "register"
    ? "Add existing installation to RAST"
    : "Install";
}

/** Application availability choices (`preview.availability`, "users"|"admins"). */
export interface PreviewAvailability {
  defaultVisibility: LinuxAppVisibility;
  options: LinuxAppVisibility[];
}

function visibilityOf(value: unknown): LinuxAppVisibility | null {
  return value === "users" || value === "admins" ? value : null;
}

const DEFAULT_VISIBILITIES: LinuxAppVisibility[] = ["users", "admins"];

export function previewAvailability(preview: LinuxInstallPreview): PreviewAvailability {
  const raw = objectOf<LinuxPreviewAvailability>(preview.availability);
  const options: LinuxAppVisibility[] = [];
  for (const value of Array.isArray(raw?.options) ? raw.options : []) {
    const visibility = visibilityOf(value);
    if (visibility && !options.includes(visibility)) options.push(visibility);
  }
  const defaultVisibility = visibilityOf(raw?.default) ?? "users";
  if (options.length === 0) options.push(...DEFAULT_VISIBILITIES);
  if (!options.includes(defaultVisibility)) options.unshift(defaultVisibility);
  return { defaultVisibility, options };
}

/** Host (server) distribution and architecture, when the preview reports them. */
export interface PreviewHostInfo {
  distribution: string | null;
  architecture: string | null;
}

export function previewHost(preview: LinuxInstallPreview): PreviewHostInfo {
  return {
    distribution: textOrNull(preview.hostDistribution),
    architecture: textOrNull(preview.hostArchitecture),
  };
}

/** Where the file comes from: an https URL, a File Station path or the picked file. */
export interface PreviewSourceInfo {
  kind: "url" | "path" | "file";
  /** Row label, e.g. "Downloaded from" or "File Station path". */
  label: string;
  value: string;
}

export function previewSource(preview: LinuxInstallPreview): PreviewSourceInfo | null {
  const source = objectOf<LinuxPreviewSource>(preview.source);
  const url = textOrNull(source?.url);
  if (url) return { kind: "url", label: "Downloaded from", value: url };
  const path = textOrNull(source?.path);
  if (path) return { kind: "path", label: "File Station path", value: path };
  const fileName = textOrNull(preview.fileName);
  if (fileName) return { kind: "file", label: "File", value: fileName };
  return null;
}

// ------------------------------------------------------- upload file types

/** The file types the dialog offers (AppImage is deliberately not among them). */
export const PACKAGE_FILE_ACCEPT = ".deb,.tar.gz,.tgz,.tar.xz,.txz,.tar.bz2,.tbz2,.tar";

/** Plain-language list of supported file types. */
export const PACKAGE_FILE_HINT = ".deb packages, or a portable archive (.tar.gz, .tar.xz, .tgz, .tbz2)";

export const APPIMAGE_REJECTED_MESSAGE =
  "AppImage is not offered because it is not validated on this server. Use a .deb package or a portable archive (.tar.gz, .tar.xz or .tbz2).";

export const UNSUPPORTED_FILE_MESSAGE =
  "That file type is not supported. Use a .deb package or a portable archive (.tar.gz, .tar.xz or .tbz2).";

const SUPPORTED_PACKAGE_PATTERN = /\.(deb|tar\.gz|tgz|tar\.xz|txz|tar\.bz2|tbz2|tar)$/i;
const APPIMAGE_PATTERN = /\.(appimage|app)$/i;

/**
 * Why a file cannot be installed from the App Center dialog, or null when it
 * is a supported package type. AppImage gets its own explanation.
 */
export function packageFileProblem(fileName: string): string | null {
  const name = (fileName ?? "").trim();
  if (name === "") return UNSUPPORTED_FILE_MESSAGE;
  if (APPIMAGE_PATTERN.test(name)) return APPIMAGE_REJECTED_MESSAGE;
  if (!SUPPORTED_PACKAGE_PATTERN.test(name)) return UNSUPPORTED_FILE_MESSAGE;
  return null;
}

export function isSupportedPackageFileName(fileName: string): boolean {
  return packageFileProblem(fileName) === null;
}

// ------------------------------------------------------- install job stages

/** The visible stages of the install dialog, in order. */
export type InstallStage = "uploading" | "validating" | "installing" | "registering" | "ready";

export const INSTALL_STAGES: readonly InstallStage[] = [
  "uploading",
  "validating",
  "installing",
  "registering",
  "ready",
];

const INSTALL_STAGE_LABELS: Record<InstallStage, string> = {
  uploading: "Uploading",
  validating: "Validating",
  installing: "Installing",
  registering: "Registering",
  ready: "Ready",
};

export function installStageLabel(stage: InstallStage): string {
  return INSTALL_STAGE_LABELS[stage];
}

export function installStageIndex(stage: InstallStage): number {
  const index = INSTALL_STAGES.indexOf(stage);
  return index < 0 ? 0 : index;
}

/** progress/total as a 0..1 fraction; 0 when the server has no total. */
export function installProgressFraction(progress: number, total: number): number {
  if (!Number.isFinite(progress) || !Number.isFinite(total) || total <= 0) return 0;
  return Math.min(1, Math.max(0, progress / total));
}

/**
 * Fraction-only fallback for the stage: the server counts five steps
 * (validate/install/register/verify/ready), so 1/5..2/5 is "Installing",
 * 3/5..4/5 is "Registering" and a full fraction is "Ready". The uploading
 * stage is client-side and never comes from a fraction.
 */
export function installStageFromFraction(progress: number, total: number): InstallStage {
  if (total > 0 && progress >= total) return "ready";
  const fraction = installProgressFraction(progress, total);
  if (fraction >= 0.6) return "registering";
  if (fraction >= 0.2) return "installing";
  return "validating";
}

/**
 * Map a job progress message to a stage. Matches the backend contract
 * ("Validating the package", "Installing the system package", "Registering
 * the application", "Verifying the installation", "Ready") and falls back to
 * the progress fraction when the message is generic.
 */
export function installStageFromMessage(message: string, progress = 0, total = 0): InstallStage {
  const text = (message ?? "").toLowerCase().trim();
  if (text) {
    if (/\bready\b/.test(text)) return "ready";
    if (/upload/.test(text)) return "uploading";
    if (/download/.test(text)) return "uploading";
    if (/validat|inspect|check|prepar|queued|started/.test(text)) return "validating";
    if (/verif/.test(text)) return "registering";
    if (/regist/.test(text)) return "registering";
    if (/install|dpkg|apt\b|system package/.test(text)) return "installing";
  }
  return installStageFromFraction(progress, total);
}
