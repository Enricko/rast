// Application registry: everything the desktop can launch.

import type { AppDefinition } from "../core/app";
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

/** Apps visible to the current role. */
export function appsForRole(isAdmin: boolean): AppDefinition[] {
  return APPS.filter((app) => !app.adminOnly || isAdmin);
}
