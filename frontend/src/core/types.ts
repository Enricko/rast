// API DTO types shared by every RAST app. Keep in sync with docs/API.md.

export type Role = "admin" | "user";

export interface User {
  id: number;
  username: string;
  displayName: string;
  role: Role;
  active: boolean;
  quotaBytes: number;
  usedBytes: number;
  mustChangePassword: boolean;
  createdAt: number;
  updatedAt: number;
  lastLoginAt: number | null;
  /** Groups the account belongs to (present once the groups backend lands). */
  groups?: GroupRef[];
  groupIds?: number[];
}

export interface Features {
  docker: boolean;
  apps: boolean;
  backup: boolean;
  monitor: boolean;
  activityLogs: boolean;
  fileStation: boolean;
}

export interface MeResponse {
  user: User;
  csrfToken: string;
  sessionExpiresAt: number;
  features: Features;
  serverTime: number;
  /** Present when the groups backend is enabled; absent on older servers. */
  permissions?: Permissions;
}

// ---------------------------------------------------------------------------
// Users & Groups / access policy (docs/GROUPS.md)
// ---------------------------------------------------------------------------

export type FolderRule = "inherit" | "none" | "read" | "write";
export type AppRule = "inherit" | "none" | "allow";

export interface GroupRef {
  id: number;
  name: string;
}

export interface GroupMember {
  userId: number;
  username: string;
  displayName: string;
  /** Present on /api/groups/{id}/members and new /api/users payloads. */
  active?: boolean;
}

export interface GroupFolderRule {
  shareName: string;
  /** Stable share id when the server exposes one (rule writes keep the name). */
  shareId?: string;
  rule: FolderRule;
}

export interface GroupAppRule {
  appId: string;
  rule: AppRule;
}

export interface GroupSummary {
  id: number;
  name: string;
  description: string;
  memberCount: number;
  folderRuleCount: number;
  appRuleCount: number;
  createdAt: number;
  updatedAt: number;
}

export interface GroupDetail extends GroupSummary {
  members: GroupMember[];
  folderRules: GroupFolderRule[];
  appRules: GroupAppRule[];
}

export interface GroupInput {
  name: string;
  description?: string;
  members?: number[];
  folderRules?: GroupFolderRule[];
  appRules?: GroupAppRule[];
}

export interface UserGroupsResponse {
  groupIds: number[];
  groups: GroupRef[];
}

export interface AccessRules {
  folderRules: GroupFolderRule[];
  appRules: GroupAppRule[];
}

export interface AccessRuleSource {
  type: "direct" | "group";
  groupId?: number | null;
  groupName?: string | null;
  rule: string;
  /** Some backends use `kind` instead of `type`. */
  kind?: "direct" | "group";
}

export type FolderLevel = "none" | "read" | "write" | "admin" | null;

export interface EffectiveFolderAccess {
  shareId?: string;
  shareName: string;
  path?: string;
  /** null/absent means No Access. */
  level: FolderLevel;
  sources: AccessRuleSource[];
  fallback?: string | null;
  /** Server-computed explicit-deny conflict (F13). */
  conflict?: {
    deniedBy: AccessRuleSource[];
    overriddenGrants: AccessRuleSource[];
  } | null;
}

export interface EffectiveAppAccess {
  appId: string;
  label?: string;
  allowed: boolean;
  readOnly: boolean;
  sources: AccessRuleSource[];
  fallback?: string | null;
}

export interface EffectiveAccess {
  user?: User;
  groups?: GroupRef[];
  folders: EffectiveFolderAccess[];
  apps: EffectiveAppAccess[];
  /** Pagination/filter metadata when the server supports F13 query params. */
  foldersTotal?: number;
  page?: number;
  perPage?: number;
}

/** GET /api/groups/{id}/impact — effect of deleting the group (F12). */
export interface GroupImpactChange {
  resource: string;
  kind: "folder" | "app";
  before?: string | boolean | null;
  after?: string | boolean | null;
}

export interface GroupImpactUser {
  userId: number;
  username: string;
  displayName?: string;
  changes?: GroupImpactChange[];
  /** Alternate shape: before/after maps of folder and app decisions. */
  before?: { folders?: Record<string, string>; apps?: Record<string, boolean | null> };
  after?: { folders?: Record<string, string>; apps?: Record<string, boolean | null> };
}

export interface GroupImpact {
  members: number;
  folderRules: number;
  appRules: number;
  users: GroupImpactUser[];
  truncated?: boolean;
}

export interface AppPermission {
  allowed: boolean;
  readOnly: boolean;
  /** False when the app was uninstalled in App Center (closed for everyone). */
  installed?: boolean;
}

export interface Permissions {
  /** Keys are the fixed app ids; missing keys fall back to role defaults. */
  apps: Record<string, AppPermission>;
  groups?: GroupRef[];
}

export interface Bootstrap {
  product: string;
  description: string;
  version: string;
  setupRequired: boolean;
  setupTokenRequired: boolean;
  registrationEnabled: boolean;
  authenticated?: boolean;
  maxUploadBytes: number;
  sessionTtlHours: number;
  serverTime: number;
}

export interface SetupStatus {
  setupRequired: boolean;
  tokenRequired: boolean;
  version: string;
  product: string;
}

export interface Page<T> {
  items: T[];
  page: number;
  perPage: number;
  total: number;
}

export interface Quota {
  quotaBytes: number;
  usedBytes: number;
  freeBytes: number;
  trashBytes?: number;
}

export interface FileEntry {
  name: string;
  path: string;
  kind: "file" | "dir";
  size: number;
  modifiedAt: number;
  mime: string;
  readable: boolean;
  writable: boolean;
}

export interface Breadcrumb {
  name: string;
  path: string;
}

export interface FileListing extends Page<FileEntry> {
  path: string;
  breadcrumbs: Breadcrumb[];
  quota: Quota;
}

export interface FileRoot {
  name: string;
  path: string;
  kind: "home" | "share";
  perms?: "read" | "write" | "admin";
}

export interface TrashEntry {
  id: string;
  originalPath: string;
  sizeBytes: number;
  kind: "file" | "dir";
  deletedAt: number;
}

export interface ShareMember {
  userId: number;
  username: string;
  perms: "read" | "write" | "admin";
}

export interface Share {
  id: string;
  name: string;
  path: string;
  description: string;
  members: ShareMember[];
  createdAt: number;
}

export interface Job {
  id: string;
  kind: string;
  status: "pending" | "running" | "done" | "failed" | "cancelled";
  progress: number;
  total: number;
  message: string;
  userId: number | null;
  username: string;
  createdAt: number;
  startedAt: number | null;
  finishedAt: number | null;
  result?: unknown;
  error?: string | null;
}

export interface MonitorHost {
  cpuPercent: number;
  cpuCores: number;
  loadAvg: number[];
  memoryUsed: number;
  memoryTotal: number;
  swapUsed: number;
  swapTotal: number;
  uptimeSecs: number;
}

export interface MonitorDisk {
  mount: string;
  filesystem?: string;
  totalBytes: number;
  usedBytes: number;
}

export interface MonitorNetwork {
  rxBytesPerSec: number;
  txBytesPerSec: number;
  rxTotal: number;
  txTotal: number;
}

export interface ContainerMetric {
  name: string;
  cpuPercent: number;
  memoryBytes: number;
}

export interface ComponentHealth {
  id: string;
  name: string;
  status: "ok" | "warning" | "error" | "disabled";
  detail: string;
}

export interface MonitorSnapshot {
  ts: number;
  unavailable?: boolean;
  reason?: string;
  host?: MonitorHost;
  disks?: MonitorDisk[];
  network?: MonitorNetwork;
  containers?: ContainerMetric[];
  components?: ComponentHealth[];
  intervalSecs?: number;
}

export interface SessionInfo {
  id: string;
  ip: string;
  userAgent: string;
  createdAt: number;
  lastSeenAt: number;
  expiresAt: number;
  current?: boolean;
}

export interface SystemInfo {
  product: string;
  description: string;
  version: string;
  host: {
    hostname: string;
    os: string;
    kernel: string;
    arch: string;
    cpuCount: number;
    memoryTotalBytes: number;
    memoryAvailableBytes: number;
    uptimeSecs: number;
  };
  server: Record<string, unknown>;
  counts: { users: number; admins: number; managedApps: number };
  database: { mainBytes: number; walBytes: number; totalBytes: number };
  storage: DiskUsage;
  dataDisk: DiskUsage;
  serverTime: number;
}

export interface DiskUsage {
  available: boolean;
  reason?: string;
  mountPoint?: string;
  filesystem?: string;
  totalBytes?: number;
  usedBytes?: number;
  availableBytes?: number;
}

export interface Settings {
  siteName: string;
  loginMessage: string;
  auditRetentionDays: number;
  trashRetentionDays: number;
  defaultQuotaBytes: number;
  config: {
    bind: string;
    publicUrl: string | null;
    dataDir: string;
    storageDir: string;
    sessionTtlHours: number;
    monitorIntervalSecs: number;
    maxUploadBytes: number;
    dockerEnabled: boolean;
    secureCookies: boolean;
    registrationEnabled: boolean;
    readOnly: string[];
  };
}

export interface LogEntry {
  id: number;
  createdAt: number;
  userId: number | null;
  username: string;
  action: string;
  objectType: string;
  objectId: string;
  objectPath: string;
  result: "success" | "failure" | "denied" | string;
  detail: string;
  ip: string;
}

export interface AppEnvVar {
  key: string;
  value: string;
  secret?: boolean;
}

export interface ManagedApp {
  id: string;
  name: string;
  slug: string;
  status: "running" | "stopped" | "partial" | "error" | "deploying" | "unknown";
  lastError: string;
  createdAt: number;
  updatedAt: number;
  containers?: Array<{
    name: string;
    state: string;
    image: string;
    ports?: string[];
  }>;
  ports?: string[];
  volumes?: string[];
  env?: AppEnvVar[];
}

export interface DockerStatus {
  enabled: boolean;
  available: boolean;
  version?: string;
  reason?: string;
  /** RAST version of the Docker helper (null before 0.9.1). */
  workerVersion?: string | null;
  /** The helper still runs another RAST binary and needs a restart. */
  workerOutdated?: boolean;
}

export interface ContainerInfo {
  id: string;
  name: string;
  image: string;
  state: string;
  managed: boolean;
}

export interface Backup {
  id: string;
  fileName: string;
  sizeBytes: number;
  sha256: string;
  scope: string[];
  status: "running" | "done" | "failed" | string;
  note: string;
  createdAt: number;
  finishedAt: number | null;
  error: string;
}

export interface BackupSchedule {
  enabled: boolean;
  frequency: "hourly" | "daily" | "weekly";
  timeOfDay: string;
  weekday: number;
  retentionCount: number;
  scopes: string[];
  lastRunAt: number | null;
  nextRunAt: number;
}

/** POST /api/auth/login response (note: features arrive via /api/auth/me). */
export interface LoginResponse {
  user: User;
  csrfToken: string;
  expiresAt: number;
}

/** GET /api/system/components */
export interface ComponentStatusResponse {
  components: ComponentHealth[];
  jobsRunning: number;
  serverTime: number;
}

/** GET /api/settings/storage */
export interface StorageSettings {
  dataDir: string;
  storageDir: string;
  homesDir: string;
  sharedDir: string;
  trashDir: string;
  homes: string[];
  sharedFolders: string[];
  usedBytes: number;
  filesystem: DiskUsage;
}

/** GET /api/users/{id}/sessions */
export interface UserSessionList {
  items: SessionInfo[];
}

/** POST/PATCH /api/shares body. */
export interface ShareInput {
  name?: string;
  description?: string;
  members?: Array<{ userId: number; perms: ShareMember["perms"] }>;
}
