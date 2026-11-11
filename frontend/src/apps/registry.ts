// Application registry: everything the desktop can launch.

import type { AppDefinition } from "../core/app";
import type { Permissions } from "../core/types";
import { applicationsApp } from "./applications";
import { backupApp } from "./backup";
import { controlPanelApp } from "./controlpanel";
import { filesApp } from "./files";
import { logsApp } from "./logs";
import { monitorApp } from "./monitor";

export const APPS: AppDefinition[] = [
  filesApp,
  applicationsApp,
  monitorApp,
  backupApp,
  controlPanelApp,
  logsApp,
];

export function findApp(id: string): AppDefinition | undefined {
  return APPS.find((app) => app.id === id);
}

/** Apps visible to the current role (fallback when permissions are absent). */
export function appsForRole(isAdmin: boolean): AppDefinition[] {
  return APPS.filter((app) => !app.adminOnly || isAdmin);
}

/**
 * Whether a single app is allowed for this account.
 *
 * Uses the effective permissions from /api/auth/me when present; missing app
 * entries (or a missing permissions object entirely) fall back to the role
 * defaults so older servers keep working unchanged.
 */
export function isAppAllowed(appId: string, permissions: Permissions | null, isAdmin: boolean): boolean {
  if (isAdmin) return true;
  const app = findApp(appId);
  if (!app) return false;
  const granted = permissions?.apps?.[appId];
  if (granted && typeof granted.allowed === "boolean") return granted.allowed;
  return !app.adminOnly;
}

/** Apps visible to the current account, driven by effective permissions. */
export function appsForPermissions(permissions: Permissions | null, isAdmin: boolean): AppDefinition[] {
  return APPS.filter((app) => isAppAllowed(app.id, permissions, isAdmin));
}

/** True when the app is read-only for this account (e.g. group `allow` grant). */
export function isAppReadOnly(appId: string, permissions: Permissions | null, isAdmin: boolean): boolean {
  if (isAdmin) return false;
  const granted = permissions?.apps?.[appId];
  return Boolean(granted?.readOnly && granted.allowed);
}

/** Stable signature used to detect effective permission changes. */
export function permissionsSignature(permissions: Permissions | null, isAdmin: boolean): string {
  const ids = appsForPermissions(permissions, isAdmin).map((app) => app.id);
  const flags = ids.map((id) => `${id}:${isAppReadOnly(id, permissions, isAdmin) ? "ro" : "rw"}`);
  return flags.join(",");
}
