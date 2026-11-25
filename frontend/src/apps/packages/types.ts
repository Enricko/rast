// App Center API shapes (see src/packages/service.rs).

export interface ExecutablePath {
  name: string;
  path: string;
}

export interface Provenance {
  kind?: "upstream" | "built" | string;
  source?: string;
  sourceSha256?: string | null;
  verification?: string | null;
  buildUrl?: string | null;
  attestationUrl?: string | null;
}

export interface Requirements {
  glibc?: string | null;
  notes?: string | null;
}

export interface ArtifactView {
  target: string;
  url: string;
  size: number;
  sha256: string;
  requires: Requirements;
  provenance: Provenance;
}

export interface Bundled {
  name: string;
  version: string;
}

export interface InstalledView {
  version: string;
  revision: number;
  channel: string;
  target: string;
  path: string;
  executables: ExecutablePath[];
  isDefault: boolean;
  status: "ok" | "missing" | string;
  installedAt: number;
  installedBy: string;
  sizeBytes: number;
  checkOutput: string;
  releaseTag?: string | null;
  artifact: ArtifactView;
  bundled: Bundled[];
  pinnedBy: string[];
}

export interface VersionChoice {
  version: string;
  revision: number;
  channel: string;
  channelLabel?: string | null;
  channelStatus?: string | null;
  prerelease: boolean;
  releaseDate?: string | null;
  compatible: boolean;
  reason?: string | null;
  installedRevision?: number | null;
  targets: string[];
  artifact?: ArtifactView | null;
  bundled: Bundled[];
  extensions: string[];
  executables: ExecutablePath[];
  notes?: string | null;
}

export interface UpdateView {
  channel: string;
  from: string;
  to: string;
  revision: number;
  replacesDefault: boolean;
  /** `from` is the system's own runtime; the update installs next to it. */
  fromSystem: boolean;
}

/** A runtime the operating system already has (apt, manual install, snap). */
export interface SystemInstall {
  command: string;
  path: string;
  realPath: string;
  version?: string | null;
  versionCommand: string;
  output: string;
  source: "apt" | "snap" | "manual" | string;
  sourcePackage?: string | null;
  onPath: boolean;
  /** Moved aside (`.rast-displaced.<command>`) while App Center provides the command system-wide. */
  displaced?: boolean;
  detectedAt: number;
}

/** What a login shell runs for the package's main command. */
export interface TerminalView {
  command: string;
  source: "package-center" | "system" | string;
  version?: string | null;
  path: string;
}

/** The defaults linked into /usr/local/bin for sudo, services and scripts (`packages_system_wide`). */
export interface SystemLinks {
  enabled: boolean;
  linked: string[];
  pending: string[];
  displaced: string[];
  directory: string;
  /** The root watcher (systemd path unit) is installed. */
  watcher: boolean;
}

/** App Center on the login-shell PATH (/etc/profile.d). */
export interface ShellPath {
  enabled: boolean;
  installed: boolean;
  active: boolean;
  profile: string;
  binDir: string;
  syncCommand: string;
  system?: SystemLinks;
}

export interface DetectedView {
  version: string;
  channel: string;
  state: string;
  detail: string;
  detectedAt: string;
  source: "catalog" | "github" | string;
  releaseUrl?: string | null;
}

export interface JobHistory {
  jobId: string;
  packageId: string;
  version: string;
  action: string;
  status: string;
  progress: number;
  total: number;
  message: string;
  error?: string | null;
  username: string;
  createdAt: number;
  finishedAt?: number | null;
}

export interface ConsumerView {
  id: number;
  name: string;
  description: string;
  packageId: string;
  version?: string | null;
  followsDefault: boolean;
  effectiveVersion?: string | null;
  executables: ExecutablePath[];
  createdBy: string;
  updatedAt: number;
}

export interface Publisher {
  id: string;
  name: string;
  url?: string | null;
}

export interface PackageView {
  id: string;
  packageType: string;
  name: string;
  description: string;
  homepage?: string | null;
  license?: string | null;
  publisher: Publisher;
  source: "official" | "github" | string;
  sourceRepo?: string | null;
  official: boolean;
  verification: { kind: string; label: string; detail: string };
  inCatalog: boolean;
  installed: InstalledView[];
  defaultVersion?: string | null;
  defaultExecutables: ExecutablePath[];
  latest?: VersionChoice | null;
  versions: VersionChoice[];
  unsupported: Array<{ version: string; reason: string }>;
  updates: UpdateView[];
  detected: DetectedView[];
  updateState: "ready" | "detected" | "none" | string;
  activeJob?: JobHistory | null;
  consumers: ConsumerView[];
  unavailableReason?: string | null;
  system?: SystemInstall | null;
  terminal?: TerminalView | null;
}

export interface CatalogStatus {
  url: string;
  official: boolean;
  name?: string | null;
  keyId?: string | null;
  officialKey: boolean;
  sequence?: number | null;
  generatedAt?: string | null;
  expiresAt?: string | null;
  checkedAt?: number | null;
  lastAttemptAt?: number | null;
  lastError?: string | null;
  intervalMinutes: number;
  nextCheckAt?: number | null;
  stale: boolean;
  expired: boolean;
  checking: boolean;
}

export interface HostInfo {
  os: string;
  arch: string;
  target: string;
  libc: string;
  glibcVersion?: string | null;
  kernel?: string | null;
  rastVersion: string;
}

/** One of RAST's own apps: built-in ones are always installed. */
export interface AppPackage {
  id: string;
  name: string;
  description: string;
  installed: boolean;
  removable: boolean;
}

export interface Overview {
  /** RAST's own apps (absent on older servers). */
  apps?: AppPackage[];
  host: HostInfo;
  catalog: CatalogStatus;
  shell?: ShellPath | null;
  packages: PackageView[];
  counts: Record<string, number>;
  canManage: boolean;
}

export interface PackageDetail {
  package: PackageView;
  jobs: JobHistory[];
  host: HostInfo;
}

export interface Check {
  label: string;
  status: "ok" | "warning" | "failed" | string;
  detail: string;
}

export interface ReleaseAsset {
  name: string;
  size: number;
  digest?: string | null;
  downloadUrl?: string;
}

export interface Release {
  id: number;
  tag: string;
  name?: string | null;
  htmlUrl: string;
  publishedAt?: string | null;
  prerelease: boolean;
  assets: ReleaseAsset[];
}

export interface ManifestView {
  id: string;
  type: string;
  name: string;
  description: string;
  homepage?: string | null;
  license?: string | null;
  publisher: Publisher;
  version: string;
  revision: number;
  minRastVersion?: string | null;
  artifacts: ArtifactView[];
  runtime: { executables: ExecutablePath[]; bundled?: Bundled[]; extensions?: string[] };
}

export interface Preview {
  inspectionId: string;
  repo: string;
  repoUrl: string;
  release: Release;
  manifest: ManifestView;
  manifestSha256: string;
  checks: Check[];
  official: boolean;
  sourceApproved: boolean;
  sourceApprovedBy?: string | null;
  hostTarget: string;
  compatible: boolean;
  reason?: string | null;
  artifact?: ArtifactView | null;
  alreadyInstalled: boolean;
  expiresAt: number;
}

/** `package.changed` / `package.catalog` payloads. */
export interface PackageEvent {
  packageId?: string;
  version?: string;
  reason?: string;
  jobId?: string;
  error?: string;
  changed?: boolean;
}

/** `job.*` payload subset. */
export interface JobEvent {
  id?: string;
  kind?: string;
  progress?: number;
  total?: number;
  message?: string;
  error?: string;
}
