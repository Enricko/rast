// Installed App Center app packages as first-class desktop applications.
//
// A package of type "app" becomes an entry in Applications, a desktop icon
// and a taskbar pin exactly like a built-in app: this module publishes an
// AppDefinition for every installed app package, updated from the Package
// Center overview and the realtime `package.changed` event.

import type { ApiClient } from "../core/api";
import type { AppContext, AppDefinition } from "../core/app";
import { h } from "../core/dom";
import { errorMessage } from "../core/ui";

export interface AppPackageInfo {
  id: string;
  name: string;
  description: string;
  version: string;
  app: { kind: string; entry?: string | null } | null;
}

export const PACKAGE_APP_PREFIX = "package:";

export function packageAppId(packageId: string): string {
  return `${PACKAGE_APP_PREFIX}${packageId}`;
}

export function isPackageAppId(appId: string): boolean {
  return appId.startsWith(PACKAGE_APP_PREFIX);
}

let packages: AppPackageInfo[] = [];
const listeners = new Set<() => void>();

function notify(): void {
  for (const listener of [...listeners]) {
    try {
      listener();
    } catch (error) {
      console.error("package app listener failed", error);
    }
  }
}

export function onPackageApps(callback: () => void): () => void {
  listeners.add(callback);
  return () => listeners.delete(callback);
}

export function packageAppDefinitions(): AppDefinition[] {
  return packages.map((info) => ({
    id: packageAppId(info.id),
    name: info.name,
    icon: "package",
    description: info.description || "Installed app package",
    defaultSize: info.app?.kind === "web" ? { w: 900, h: 640 } : { w: 560, h: 440 },
    minSize: { w: 360, h: 280 },
    mount: (ctx: AppContext) => mountPackageApp(ctx, info),
  }));
}

export function setPackageApps(entries: AppPackageInfo[]): void {
  const signature = JSON.stringify(
    entries.map((entry) => [entry.id, entry.name, entry.version, entry.app?.kind ?? ""]),
  );
  const previous = JSON.stringify(
    packages.map((entry) => [entry.id, entry.name, entry.version, entry.app?.kind ?? ""]),
  );
  packages = entries;
  if (signature !== previous) notify();
}

type OverviewPackage = {
  id?: string;
  name?: string;
  description?: string;
  packageType?: string;
  defaultVersion?: string | null;
  app?: { kind?: string; entry?: string | null } | null;
  installed?: Array<{ version?: string }>;
};

/** Update the registry from an `/api/packages` overview payload. */
export function setPackagesFromOverview(entries: OverviewPackage[] | undefined): void {
  const apps = (entries ?? [])
    .filter((entry) => entry.packageType === "app" && (entry.installed?.length ?? 0) > 0)
    .map((entry) => ({
      id: entry.id ?? "",
      name: entry.name ?? entry.id ?? "Application",
      description: entry.description ?? "",
      version: entry.defaultVersion ?? entry.installed?.[entry.installed.length - 1]?.version ?? "",
      app: entry.app
        ? { kind: entry.app.kind ?? "web", entry: entry.app.entry ?? null }
        : { kind: "command", entry: null },
    }))
    .filter((entry) => entry.id.length > 0);
  setPackageApps(apps);
}

/** Fetch the installed app packages (silently ignored when not permitted). */
export async function refreshPackageApps(api: ApiClient): Promise<void> {
  try {
    const overview = await api.get<{ packages?: OverviewPackage[] }>("/api/packages");
    setPackagesFromOverview(overview.packages);
  } catch {
    /* the account may not use the Package Center; no package apps then */
  }
}

function mountPackageApp(ctx: AppContext, info: AppPackageInfo): void {
  if (info.app?.kind === "web") {
    const entry = (info.app.entry || "index.html").replace(/^\/+/, "");
    const frame = document.createElement("iframe");
    frame.className = "package-app-frame";
    frame.title = info.name;
    frame.src = `/api/packages/${encodeURIComponent(info.id)}/app/${entry}`;
    ctx.root.appendChild(frame);
    return;
  }
  mountCommandApp(ctx, info);
}

function mountCommandApp(ctx: AppContext, info: AppPackageInfo): void {
  const input = h("input", {
    class: "input package-app-input",
    type: "text",
    placeholder: "Arguments (optional)",
    ariaLabel: "Arguments",
    spellcheck: false,
  });
  const runButton = h("button", { class: "btn btn-primary btn-sm", type: "button" }, "Run");
  const output = h(
    "pre",
    { class: "package-app-output" },
    "Click Run to start the application.",
  );
  let busy = false;
  const run = async (): Promise<void> => {
    if (busy) return;
    busy = true;
    runButton.disabled = true;
    output.textContent = "Running…";
    const value = input.value.trim();
    try {
      const result = await ctx.api.post<{
        stdout: string;
        stderr: string;
        exitCode: number | null;
      }>(`/api/packages/${encodeURIComponent(info.id)}/run`, {
        args: value ? [value] : [],
      });
      const parts: string[] = [];
      if (result.stdout) parts.push(result.stdout.trimEnd());
      if (result.stderr) parts.push(result.stderr.trimEnd());
      if (result.exitCode !== null && result.exitCode !== 0) {
        parts.push(`exit code ${result.exitCode}`);
      }
      output.textContent = parts.join("\n") || "(no output)";
    } catch (error) {
      output.textContent = errorMessage(error);
    } finally {
      busy = false;
      runButton.disabled = false;
    }
  };
  runButton.addEventListener("click", () => void run());
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") void run();
  });
  ctx.root.appendChild(
    h(
      "div",
      { class: "package-app" },
      h("div", { class: "package-app-row" }, input, runButton),
      output,
    ),
  );
}
