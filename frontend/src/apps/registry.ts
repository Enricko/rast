// Application registry: everything the desktop can launch.

import type { AppDefinition } from "../core/app";
import type { Permissions } from "../core/types";
import { agentApp } from "./agent";
import { applicationsApp } from "./applications";
import { backupApp } from "./backup";
import { controlPanelApp } from "./controlpanel";
import { filesApp } from "./files";
import { launcherApp } from "./launcher";
import { logsApp } from "./logs";
import { monitorApp } from "./monitor";
import { packagesApp } from "./packages";
import { terminalApp } from "./terminal";

export const APPS: AppDefinition[] = [
  launcherApp,
  filesApp,
  applicationsApp,
  monitorApp,
  backupApp,
  packagesApp,
  controlPanelApp,
  logsApp,
  terminalApp,
  agentApp,
];

// Package Center apps (see core/package-apps.ts): they join the built-in
// apps, and one with a built-in app's id takes its place.
let packageApps: AppDefinition[] = [];

/** Set the Package Center apps; true when the list changed. */
export function setPackageApps(apps: AppDefinition[]): boolean {
  const key = (list: AppDefinition[]) => list.map((app) => `${app.id}@${app.packageVersion ?? ""}`).join(",");
  if (key(apps) === key(packageApps)) return false;
  packageApps = apps.slice();
  return true;
}

/** Every app the desktop knows: built-in ones and Package Center ones. */
export function allApps(): AppDefinition[] {
  const replaced = new Set(packageApps.map((app) => app.id));
  return [...APPS.filter((app) => !replaced.has(app.id)), ...packageApps];
}

export function findApp(id: string): AppDefinition | undefined {
  return packageApps.find((app) => app.id === id) ?? APPS.find((app) => app.id === id);
}

/** Apps visible to the current role (fallback when permissions are absent). */
export function appsForRole(isAdmin: boolean): AppDefinition[] {
  return allApps().filter((app) => !app.adminOnly || isAdmin);
}

/**
 * Whether a single app is allowed for this account.
 *
 * Uses the effective permissions from /api/auth/me when present; missing app
 * entries (or a missing permissions object entirely) fall back to the role
 * defaults so older servers keep working unchanged.
 */
export function isAppAllowed(appId: string, permissions: Permissions | null, isAdmin: boolean): boolean {
  const app = findApp(appId);
  if (!app) return false;
  const granted = permissions?.apps?.[appId];
  // Uninstalled in App Center: closed for everyone, administrators included.
  if (granted?.installed === false) return false;
  if (isAdmin) return true;
  if (granted && typeof granted.allowed === "boolean") return granted.allowed;
  return !app.adminOnly;
}

/** Apps visible to the current account, driven by effective permissions. */
export function appsForPermissions(permissions: Permissions | null, isAdmin: boolean): AppDefinition[] {
  return allApps().filter((app) => isAppAllowed(app.id, permissions, isAdmin));
}

/** True when the app is read-only for this account (e.g. group `allow` grant). */
export function isAppReadOnly(appId: string, permissions: Permissions | null, isAdmin: boolean): boolean {
  if (isAdmin) return false;
  const granted = permissions?.apps?.[appId];
  return Boolean(granted?.readOnly && granted.allowed);
}

/** Whether an app is installed (uninstalled apps live only in App Center). */
export function isAppInstalled(appId: string, permissions: Permissions | null): boolean {
  return permissions?.apps?.[appId]?.installed !== false;
}

/** Stable signature used to detect effective permission changes. */
export function permissionsSignature(permissions: Permissions | null, isAdmin: boolean): string {
  const flags = appsForPermissions(permissions, isAdmin).map((app) => {
    const version = app.packageVersion ? `@${app.packageVersion}` : "";
    return `${app.id}${version}:${isAppReadOnly(app.id, permissions, isAdmin) ? "ro" : "rw"}`;
  });
  return flags.join(",");
}
