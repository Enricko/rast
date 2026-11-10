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
}

export interface Bootstrap {
  product: string;
  description: string;
  version: string;
  setupRequired: boolean;
  setupTokenRequired: boolean;
  registrationEnabled: boolean;
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
