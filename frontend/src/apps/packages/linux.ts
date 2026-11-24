// App Center: Linux apps (administrators only). A .deb package or a portable
// archive from this device, File Station or an https link becomes an app with
// its own window; each person's copy runs on the server and stops when the
// window closes.
//
// Installing goes through one dialog: choose a source, the server previews it
// (identity, compatibility, package transaction, existing app data), the
// administrator confirms availability, and the job's stages are shown until
// the app is registered and ready. Tiles have one action (Open, Update or
// progress); the app's page has the rest: updates from the official source,
// memory and CPU limits, uninstall.

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { formatBytes, formatDate, relativeTime } from "../../core/format";
import { appIconSvg, iconSvg } from "../../core/icons";
import type { RealtimeStore } from "../../core/realtime";
import { contextMenu, errorMessage, modal, type ContextMenuEntry } from "../../core/ui";
import type { AppLifecycle } from "../../components/lifecycle";
import { watchJob, type JobOutcome } from "../../components/jobs";
import { banner, emptyBlock, progressBar, type ProgressHandle } from "../../components/states";
import { pickDestination } from "../files/picker";
import { UploadManager } from "../files/upload";
import { NEWER_BUILD, discoveredApps, isNativeApp, isOpenable, LINUX_PREFIX, linuxAppById, linuxApps, linuxAppsInstallAvailable, linuxAppsOffered, linuxNativeSession, linuxOverview, loadDiscoveredApps, loadLinuxApps, nativeAppsEnabled, nativeRuntimeSummary, validateDiscoveredImport, type DiscoveredApp, type DiscoveredImportProblems, type LinuxApp, type NativeAppInfo } from "../linuxapp/catalog";
import {
  APPIMAGE_REJECTED_MESSAGE,
  INSTALL_STAGES,
  installProgressFraction,
  installStageFromMessage,
  installStageIndex,
  installStageLabel,
  PACKAGE_FILE_ACCEPT,
  PACKAGE_FILE_HINT,
  packageFileProblem,
  previewArchitecture,
  previewAvailability,
  previewCandidates,
  previewCompatibility,
  previewDependencies,
  previewDuplicate,
  previewFileName,
  previewFormat,
  previewHost,
  previewIdentity,
  previewInstallActionLabel,
  previewInstallPlan,
  previewLauncherOwner,
  previewName,
  previewNeedsChoice,
  previewPackageName,
  previewProfile,
  previewProfileSummary,
  previewSizeBytes,
  previewSource,
  previewSummary,
  previewTransaction,
  previewVersion,
  type InstallStage,
  type LinuxAppVisibility,
  type LinuxInstallPreview,
  type LinuxPreviewLauncher,
  type PreviewAvailability,
  type PreviewInstallPlan,
} from "./linux-preview";
import type { PackageFilter } from "./format";
import type { JobEvent } from "./types";

export interface LinuxHost {
  ctx: AppContext;
  lc: AppLifecycle;
  realtime: RealtimeStore;
  readOnly(): boolean;
  /** Show this app's page in App Center. */
  openPage(id: string): void;
}

interface Progress {
  progress: number;
  total: number;
  message: string;
}

type PreviewResult =
  | { preview: LinuxInstallPreview }
  | { unsupported: true }
  | { error: string };

/** Where the dialog will get the package from. */
type InstallSource = { kind: "path"; path: string } | { kind: "url"; url: string };

interface InstallResponse {
  jobId?: string;
  installing?: boolean;
  app?: LinuxApp;
}

interface InstallStageView {
  el: HTMLElement;
  update(stage: InstallStage, message: string, progress: number, total: number): void;
}

interface InstallPreviewUi {
  el: HTMLElement;
  /** The availability the administrator picked (`users` by default). */
  visibility(): LinuxAppVisibility;
  /** The launcher picked when the archive contains several programs. */
  launcher(): LinuxPreviewLauncher | undefined;
}

/** One phase of the install dialog. */
type InstallPhase = "pick" | "preparing" | "validating" | "preview" | "installing" | "ready" | "error";

interface InstallDialogState {
  tab: "upload" | "url";
  phase: InstallPhase;
  source: InstallSource | null;
  /** How the source was chosen, for the source row of the preview. */
  origin: "upload" | "station" | "url" | null;
  /** Human-readable source shown on the error view. */
  sourceLabel: string;
  /** URL the person typed, kept while switching tabs. */
  urlDraft: string;
  fileError: string;
  preview: LinuxInstallPreview | null;
  plan: PreviewInstallPlan | null;
  visibility: LinuxAppVisibility;
  /** The server has no preview endpoint (older/Docker-only server). */
  unsupported: boolean;
  error: string;
  installing: boolean;
  app: LinuxApp | null;
  closed: boolean;
}

const FORMAT_LABEL: Record<string, string> = {
  deb: ".deb package",
  appimage: "AppImage",
  tarball: "Portable archive",
};

const DISCOVERED_STATUS: Record<string, string> = {
  discovered: "Not ready",
  pending_approval: "Waiting for approval",
  ready: "Ready",
  unavailable: "No longer installed",
  failed: "Failed",
};

const DISCOVERED_SCOPE: Record<string, string> = {
  user: "Your account",
  system: "System-wide",
  "managed-profile": "Inside an app profile",
};

const DISCOVERED_SOURCE: Record<string, string> = {
  xdg: "Desktop entry",
  appimage: "AppImage",
  portable: "Portable program",
  manual: "Imported by an administrator",
  "external-installer": "Installed by another app",
};

const SOURCE_LABEL: Record<string, string> = {
  apt: "Its official package repository",
  github: "GitHub releases",
  zsync: "The AppImage's update server",
};

/** Where a dropped file is saved (the person's home). */
const DROP_FOLDER = "/Downloads";

const MEMORY_CHOICES_MB = [1024, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768];

/** Ids/names of availability radio groups; one dialog at a time. */
let nextInstallDialogId = 1;

export class LinuxApps {
  private readonly host: LinuxHost;
  /** Running install and update jobs, by job id. */
  private readonly progress = new Map<string, Progress>();
  /** Jobs already watched to the end; a list reload must not follow them again. */
  private readonly watched = new Set<string>();
  private readonly checking = new Set<string>();

  constructor(host: LinuxHost) {
    this.host = host;
    for (const kind of ["job.start", "job.progress"]) {
      host.lc.add(
        host.realtime.on<JobEvent>(kind, (data) => {
          if (!data?.id || !String(data.kind ?? "").startsWith("linuxapps.")) return;
          this.progress.set(data.id, { progress: data.progress ?? 0, total: data.total ?? 0, message: data.message ?? "" });
          this.paintProgress(data.id);
        }),
      );
    }
    for (const kind of ["job.done", "job.failed", "job.cancelled"]) {
      host.lc.add(
        host.realtime.on<JobEvent>(kind, (data) => {
          if (!data?.id) return;
          this.watched.add(data.id);
          if (this.progress.delete(data.id)) void this.reload();
        }),
      );
    }
  }

  /** Administrators manage Linux apps; everyone else never sees them. */
  get enabled(): boolean {
    return this.host.ctx.store.isAdmin();
  }

  get dockerEnabled(): boolean {
    return linuxOverview()?.dockerEnabled ?? true;
  }

  /**
   * Is the Linux apps workflow offered on this server at all? The whole
   * App Center section and the install button disappear when the operator
   * quarantined the feature.
   */
  get offered(): boolean {
    return linuxAppsOffered(linuxOverview());
  }

  /**
   * Can this server install Linux apps at all? The toolbar button and the
   * empty-state tile hide when the feature is off or neither the native
   * backend nor Docker exists.
   */
  get available(): boolean {
    if (!this.enabled) return false;
    return linuxAppsInstallAvailable(linuxOverview());
  }

  reload(): Promise<void> {
    if (!this.enabled) return Promise.resolve();
    return loadLinuxApps(this.host.ctx.api)
      .then(() => this.reattachJobs())
      .catch(() => undefined);
  }

  /** Reload the discovered (externally installed) application inventory. */
  reloadDiscovered(): Promise<void> {
    if (!this.offered) return Promise.resolve();
    return loadDiscoveredApps(this.host.ctx.api).catch(() => undefined);
  }

  /** Is the discovered-application section relevant for this account? */
  discoveredSection(): HTMLElement | null {
    if (!this.offered) return null;
    const apps = discoveredApps();
    // Ordinary accounts see the section only when they actually have entries
    // they may launch; administrators always see it (it is where they approve
    // and rescan).
    if (!this.enabled && apps.length === 0) return null;
    const rows = apps
      .filter((app) => this.enabled || app.canLaunch)
      .map((app) => this.discoveredRow(app));
    return h(
      "section",
      { class: "pkg-group", ariaLabel: "Discovered applications" },
      h(
        "div",
        { class: "pkg-group-head row" },
        h("h3", { class: "pkg-group-title" }, "Discovered applications"),
        h(
          "div",
          { class: "row" },
          this.enabled && !this.host.readOnly()
            ? button("Import existing application", "secondary", () => this.openImportDiscovered())
            : null,
          button("Rescan applications", "secondary", () => void this.rescanDiscovered()),
        ),
      ),
      h(
        "p",
        { class: "small muted pkg-discovered-note" },
        "Applications installed outside App Center (their own .desktop entries, AppImages, portable programs, or an external installer inside an app profile). Administrators approve system-wide entries; owners can launch theirs.",
      ),
      rows.length ? h("div", { class: "pkg-discovered" }, ...rows) : emptyBlock("No applications discovered yet", "Use Rescan applications to look again."),
    );
  }

  private discoveredRow(app: DiscoveredApp): HTMLElement {
    const status = DISCOVERED_STATUS[app.status] ?? app.status;
    const scope = DISCOVERED_SCOPE[app.scope] ?? app.scope;
    const source = DISCOVERED_SOURCE[app.source] ?? app.source;
    const actions: HTMLElement[] = [];
    if (app.canLaunch) {
      actions.push(button("Launch", "primary", () => this.host.ctx.openApp(`${LINUX_PREFIX}${app.id}`)));
    }
    if (this.enabled && !this.host.readOnly()) {
      if (app.scope === "system") {
        actions.push(
          button(app.approved ? "Unapprove" : "Approve", "secondary", () => void this.patchDiscovered(app, { approved: !app.approved })),
        );
      }
      actions.push(button(app.hidden ? "Show" : "Hide", "secondary", () => void this.patchDiscovered(app, { hidden: !app.hidden })));
      actions.push(button("Forget", "secondary", () => void this.forgetDiscovered(app)));
    }
    return h(
      "div",
      { class: "pkg-info-item pkg-discovered-row" },
      h(
        "div",
        { class: "row pkg-discovered-main" },
        h(
          "span",
          { class: "pkg-discovered-icon", ariaHidden: true },
          appIconSvg(`${LINUX_PREFIX}${app.id}`, "box", 24),
        ),
        h(
          "div",
          { class: "pkg-discovered-body" },
          h("div", { class: "pkg-discovered-name" }, app.name, h("span", { class: "pkg-discovered-status" }, status)),
          h("div", { class: "small muted" }, `${source} · ${scope}${app.version ? ` · v${app.version}` : ""}${app.ownerUserId == null ? "" : ` · owner #${app.ownerUserId}`}`),
          h("div", { class: "small muted mono pkg-discovered-exec" }, app.exec.join(" ")),
        ),
      ),
      actions.length ? h("div", { class: "pkg-hero-actions" }, ...actions) : null,
    );
  }

  /**
   * Administrator: register an application that is already installed on the
   * server (no installer runs). The server validates the argv/working
   * directory/icon and shows up in the list right away.
   */
  private openImportDiscovered(): void {
    const { ctx } = this.host;
    const textField = (
      label: string,
      input: HTMLInputElement,
      hint?: string,
    ): { el: HTMLElement; input: HTMLInputElement; error: HTMLElement } => {
      const error = h("div", { class: "field-error", hidden: true });
      const el = h(
        "label",
        { class: "field" },
        h("span", { class: "field-label" }, label),
        input,
        hint ? h("span", { class: "field-hint" }, hint) : null,
        error,
      );
      return { el, input, error };
    };
    const name = textField(
      "Application name",
      h("input", { class: "input", type: "text", placeholder: "For example WordCraft" }),
    );
    const path = textField(
      "Executable path",
      h("input", {
        class: "input mono",
        type: "text",
        placeholder: "/opt/app/app or /usr/bin/app",
        dataset: { autofocus: "true" },
      }),
      "An existing executable on this server. RAST starts it with this exact argv — no shell is involved.",
    );
    const args = textField(
      "Arguments (optional)",
      h("input", { class: "input mono", type: "text", placeholder: '--flag "value with spaces"' }),
      "Extra arguments passed after the program; quotes keep spaces together.",
    );
    const workingDir = textField(
      "Working directory (optional)",
      h("input", { class: "input mono", type: "text", placeholder: "/home/you/Applications" }),
      "Only applied when it is inside the managed application trees; otherwise the app profile home is used.",
    );
    const icon = textField(
      "Icon (optional)",
      h("input", { class: "input", type: "text", placeholder: "icon-theme name or /path/icon.png" }),
      "A PNG or SVG the server can read; otherwise the generic icon is used.",
    );
    const error = h("div", { class: "form-error", role: "alert" });
    const body = h(
      "div",
      { class: "col" },
      h(
        "p",
        { class: "muted small" },
        "Register an application that is already installed on the server. It appears in Discovered applications and can be launched in your shared session; nothing is installed, updated or uninstalled.",
      ),
      name.el,
      path.el,
      args.el,
      workingDir.el,
      icon.el,
      error,
    );
    const fields = [
      ["name", name],
      ["path", path],
      ["args", args],
      ["workingDir", workingDir],
      ["icon", icon],
    ] as const;
    modal({
      title: "Import existing application",
      body,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Import",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            for (const [, field] of fields) {
              field.error.hidden = true;
              field.input.classList.remove("has-error");
              field.input.removeAttribute("aria-invalid");
            }
            error.textContent = "";
            const check = validateDiscoveredImport({
              name: name.input.value,
              path: path.input.value,
              args: args.input.value,
              workingDir: workingDir.input.value,
              icon: icon.input.value,
            });
            let invalid = false;
            for (const [key, field] of fields) {
              const message: string | undefined = check.problems[key as keyof DiscoveredImportProblems];
              if (!message) continue;
              invalid = true;
              field.error.textContent = message;
              field.error.hidden = false;
              field.input.classList.add("has-error");
              field.input.setAttribute("aria-invalid", "true");
            }
            if (invalid || !check.body) {
              error.textContent = "Check the highlighted fields.";
              throw new Error("the import form is incomplete");
            }
            try {
              await ctx.api.post<DiscoveredApp>("/api/linux-apps/discovered/import", check.body);
              close();
              ctx.toast.success(`${check.body.name} was added to Discovered applications`);
              await this.reloadDiscovered();
            } catch (err) {
              error.textContent = errorMessage(err);
              throw err;
            }
          },
        },
      ],
    });
  }

  private async rescanDiscovered(): Promise<void> {
    const { ctx } = this.host;
    try {
      const result = await ctx.api.post<{ system?: number; user?: number; userError?: string }>("/api/linux-apps/discovered/scan", {});
      await this.reloadDiscovered();
      const found = (result.system ?? 0) + (result.user ?? 0);
      if (result.userError) {
        ctx.toast.warning(`Scanned system applications, but this account's own directories could not be scanned: ${result.userError}`);
      } else {
        ctx.toast.info(`Scan finished: ${found} ${found === 1 ? "application" : "applications"} in the inventory.`);
      }
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  private async patchDiscovered(app: DiscoveredApp, patch: { approved?: boolean; hidden?: boolean }): Promise<void> {
    try {
      await this.host.ctx.api.patch(`/api/linux-apps/discovered/${encodeURIComponent(app.id)}`, patch);
      await this.reloadDiscovered();
    } catch (error) {
      this.host.ctx.toast.error(errorMessage(error));
    }
  }

  private async forgetDiscovered(app: DiscoveredApp): Promise<void> {
    try {
      await this.host.ctx.api.del(`/api/linux-apps/discovered/${encodeURIComponent(app.id)}`);
      await this.reloadDiscovered();
      this.host.ctx.toast.info(`${app.name} was removed from the discovered list (its files were not touched).`);
    } catch (error) {
      this.host.ctx.toast.error(errorMessage(error));
    }
  }

  /**
   * Reattach to install/update jobs that were already running when App Center
   * was (re)opened, so the progress bars keep moving instead of freezing.
   */
  private reattachJobs(): void {
    for (const app of linuxApps()) {
      if (app.status !== "installing" && app.status !== "updating") continue;
      const jobId = app.jobId;
      if (!jobId || this.progress.has(jobId) || this.watched.has(jobId)) continue;
      void this.follow(jobId);
    }
  }

  updateCount(): number {
    return this.enabled ? linuxApps().filter((app) => app.status === "ready" && app.update.available).length : 0;
  }

  /** Apps for a list view: Updates shows only apps with a newer version. */
  filtered(view: PackageFilter, query: string): LinuxApp[] {
    if (!this.enabled) return [];
    const q = query.trim().toLowerCase();
    return linuxApps()
      .filter((app) => view !== "updates" || (app.status === "ready" && app.update.available))
      .filter((app) => !q || app.name.toLowerCase().includes(q) || app.id.includes(q) || (app.package ?? "").includes(q))
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  // ------------------------------------------------------------- drag & drop

  /**
   * Dropping a supported package on App Center opens the install dialog with
   * that file; nothing is installed before the server preview and a click.
   */
  attachDrop(root: HTMLElement): void {
    if (!this.enabled) return;
    const overlay = h(
      "div",
      { class: "fs-drop-overlay hidden" },
      iconSvg("upload", 34),
      h("div", { class: "fs-drop-title" }, "Drop to install an app"),
      h("div", { class: "small muted" }, `${PACKAGE_FILE_HINT}. The file is saved in your Downloads folder.`),
    );
    root.append(overlay);
    let depth = 0;
    const active = (event: DragEvent): boolean => !this.host.readOnly() && this.available && hasFileDrag(event);
    root.addEventListener("dragenter", (event) => {
      if (!active(event)) return;
      event.preventDefault();
      depth += 1;
      overlay.classList.remove("hidden");
    });
    root.addEventListener("dragover", (event) => {
      if (!active(event)) return;
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    });
    root.addEventListener("dragleave", (event) => {
      if (!active(event)) return;
      depth = Math.max(0, depth - 1);
      if (depth === 0) overlay.classList.add("hidden");
    });
    root.addEventListener("drop", (event) => {
      if (!active(event)) return;
      event.preventDefault();
      depth = 0;
      overlay.classList.add("hidden");
      this.dropped(Array.from(event.dataTransfer?.files ?? []));
    });
  }

  private dropped(files: File[]): void {
    const { ctx } = this.host;
    if (files.length === 0) return;
    if (!this.available) {
      ctx.toast.warning("Installing Linux apps is not available on this server.");
      return;
    }
    const usable = files.filter((file) => packageFileProblem(file.name) === null);
    if (usable.length === 0) {
      const problem = packageFileProblem(files[0]!.name);
      ctx.toast.warning(problem === APPIMAGE_REJECTED_MESSAGE ? problem : `That file is not supported. Use ${PACKAGE_FILE_HINT}.`);
      return;
    }
    if (usable.length < files.length || files.length > 1) ctx.toast.info("Only one package is installed at a time.");
    this.openAdd([usable[0]!]);
  }

  // ------------------------------------------------------------------ tiles

  tile(app: LinuxApp): HTMLElement {
    const name = h("button", { class: "pkg-tile-name", type: "button", title: `About ${app.name}` }, app.name);
    name.addEventListener("click", (event) => {
      event.stopPropagation();
      this.host.openPage(app.id);
    });
    const tile = h(
      "article",
      { class: `pkg-tile is-linux is-${tileKind(app)}`, dataset: { linuxId: app.id } },
      this.icon(app, "tile"),
      h(
        "div",
        { class: "pkg-tile-body" },
        name,
        h("div", { class: "pkg-tile-sub" }, tileSubtitle(app), this.backendBadge(app)),
        h("div", { class: "pkg-tile-foot" }, this.tileAction(app), this.progressEl(app, "tile")),
      ),
    );
    tile.addEventListener("click", (event) => {
      if ((event.target as HTMLElement).closest("button, a")) return;
      this.host.openPage(app.id);
    });
    return tile;
  }

  /** A tile that starts the install (shown when there are no Linux apps yet). */
  addTile(): HTMLElement {
    const tile = h(
      "button",
      { class: "pkg-tile is-linux-add", type: "button", title: "Install a Linux application on this server" },
      h("div", { class: "pkg-icon pkg-icon-tile is-add", ariaHidden: true }, iconSvg("plus", 26)),
      h(
        "div",
        { class: "pkg-tile-body" },
        h("span", { class: "pkg-tile-name" }, "Install Linux App"),
        h("div", { class: "pkg-tile-sub" }, PACKAGE_FILE_HINT),
      ),
    );
    tile.addEventListener("click", () => this.openAdd());
    return tile;
  }

  private tileAction(app: LinuxApp): HTMLElement {
    if (app.status === "installing" || app.status === "updating") {
      return h("span", { class: "pkg-pill is-static hidden" }, "");
    }
    if (app.status === "failed") return h("span", { class: "pkg-pill is-static is-failed" }, "Not installed");
    if (app.update.available && !this.host.readOnly()) {
      return pill("Update", "is-strong", `Update ${app.name}`, () => this.update(app));
    }
    return pill("Open", "is-quiet", `Open ${app.name}`, () => this.host.ctx.openApp(app.desktopId));
  }

  icon(app: LinuxApp, size: "tile" | "hero"): HTMLElement {
    if (app.iconUrl) {
      const px = size === "hero" ? 64 : 38;
      const img = document.createElement("img");
      img.src = app.iconUrl;
      img.alt = "";
      img.width = px;
      img.height = px;
      img.draggable = false;
      return h("div", { class: `pkg-icon pkg-icon-${size} is-app is-picture`, ariaHidden: true }, img);
    }
    // No icon (yet, while installing): a plain box rather than letters.
    return h("div", { class: `pkg-icon pkg-icon-${size} is-app`, ariaHidden: true }, iconSvg("box", size === "hero" ? 48 : 28));
  }

  private progressEl(app: LinuxApp, size: "tile" | "hero"): HTMLElement {
    const busy = app.status === "installing" || app.status === "updating";
    const el = h(
      "div",
      { class: `pkg-progress is-${size}${busy ? "" : " hidden"}`, role: "status", ariaLive: "polite", dataset: { linuxProgress: app.jobId ?? "" } },
      h("div", { class: "pkg-progress-row" }, h("div", { class: "progress" }, h("div", { class: "progress-bar" }))),
      h("div", { class: "pkg-progress-label" }, app.status === "updating" ? "Updating…" : "Installing…"),
    );
    if (busy && app.jobId) paintBar(el, this.progress.get(app.jobId));
    return el;
  }

  private paintProgress(jobId: string): void {
    const progress = this.progress.get(jobId);
    for (const el of document.querySelectorAll<HTMLElement>(`[data-linux-progress="${CSS.escape(jobId)}"]`)) paintBar(el, progress);
  }

  // ------------------------------------------------------------------- page

  page(id: string): HTMLElement[] {
    const app = linuxAppById(id);
    if (!app) {
      return [emptyBlock(linuxOverview() ? "This app is not installed" : "Loading…", linuxOverview() ? "Go back to the list." : undefined)];
    }
    const parts: Array<HTMLElement | null> = [
      this.hero(app),
      app.description ? h("p", { class: "pkg-about" }, app.description) : null,
      this.problem(app),
      this.info(app),
      this.nativeSection(app),
      this.migrationSection(app),
      this.advanced(app),
    ];
    return parts.filter((part): part is HTMLElement => part !== null);
  }

  private hero(app: LinuxApp): HTMLElement {
    const status = heroStatus(app);
    return h(
      "section",
      { class: "pkg-hero" },
      this.icon(app, "hero"),
      h(
        "div",
        { class: "pkg-hero-body" },
        h("div", { class: "pkg-hero-publisher" }, iconSvg("box", 13), `${FORMAT_LABEL[app.format] ?? app.format} · Linux app`, this.backendBadge(app)),
        h("h2", { class: "pkg-hero-name" }, app.name),
        h("div", { class: `pkg-hero-status is-${status.kind}` }, status.text),
        this.heroActions(app),
        this.progressEl(app, "hero"),
      ),
    );
  }

  private heroActions(app: LinuxApp): HTMLElement | null {
    if (app.status === "installing" || app.status === "updating") return null;
    const readOnly = this.host.readOnly();
    const buttons: HTMLElement[] = [];
    if (app.status === "ready" && app.update.available && !readOnly) {
      buttons.push(button(`Update to ${latestLabel(app)}`, "primary", () => this.update(app)));
    }
    if (isOpenable(app)) {
      buttons.push(button("Open", buttons.length ? "secondary" : "primary", () => this.host.ctx.openApp(app.desktopId)));
    }
    if (!readOnly) {
      const items = this.menuItems(app);
      if (items.length) {
        const more = h("button", { class: "btn btn-sm btn-secondary btn-icon", type: "button", title: "More actions", ariaLabel: "More actions" }, iconSvg("more", 15));
        more.addEventListener("click", () => {
          const rect = more.getBoundingClientRect();
          contextMenu(rect.left, rect.bottom + 4, items, { ariaLabel: `${app.name} actions` });
        });
        buttons.push(more);
      }
    }
    return buttons.length ? h("div", { class: "pkg-hero-actions" }, ...buttons) : null;
  }

  private menuItems(app: LinuxApp): ContextMenuEntry[] {
    const items: ContextMenuEntry[] = [];
    if (isNativeApp(app)) {
      if (app.status === "ready") {
        items.push({ label: "Update from file…", icon: "upload", onClick: () => this.openUpdateFromFile(app) });
        items.push({ label: "Update from URL…", icon: "external", onClick: () => this.openUpdateFromUrl(app) });
      }
      const session = linuxNativeSession();
      const run = session?.apps.find((entry) => entry.appId === app.id);
      if (run) items.push({ label: "End application", icon: "stop", danger: true, onClick: () => void this.endApplication(app) });
      if (items.length) items.push("separator");
    } else if (nativeAppsEnabled()) {
      items.push({ label: "Migrate to native…", icon: "package", onClick: () => this.confirmMigrate(app) });
      items.push("separator");
    }
    if (app.status === "ready" && app.update.source) {
      items.push({ label: "Check for updates", icon: "refresh", onClick: () => void this.check(app) });
    }
    if (app.status === "ready" && app.update.source) {
      items.push({ label: "Reinstall", icon: "restart", onClick: () => void this.update(app) });
    }
    if (app.status === "ready") {
      items.push({ label: "Memory and CPU…", icon: "cpu", onClick: () => this.openLimits(app) });
      items.push({ label: "Who can use it…", icon: "users", onClick: () => this.openVisibility(app) });
    }
    if (items.length) items.push("separator");
    items.push({ label: "Uninstall…", icon: "trash", danger: true, onClick: () => this.openUninstall(app) });
    return items;
  }

  private problem(app: LinuxApp): HTMLElement | null {
    if (app.status === "failed") return banner("error", "The install did not finish", app.error ?? "Try another file.");
    if (app.error) return banner("warning", "The last update did not finish", app.error);
    if (app.launchGuess) {
      return banner("info", "RAST guessed which program to start", "If the app does not open, the archive may need a different file. See Advanced below.");
    }
    return null;
  }

  private info(app: LinuxApp): HTMLElement {
    const items: Array<[string, Node | string]> = [];
    // What an install will find (version, update source) is known once it finished.
    const known = app.status === "ready" || app.status === "updating";
    if (known) {
      items.push(["Version", app.version ?? "—"]);
      items.push(["Updates from", app.update.source ? SOURCE_LABEL[app.update.source] ?? app.update.source : "None: install a newer file to update"]);
    }
    if (known && app.update.source) {
      const when = this.checking.has(app.id)
        ? "Checking…"
        : app.update.checkedAt
          ? `Checked ${relativeTime(app.update.checkedAt)}`
          : "Not checked yet";
      const newest = app.update.error && !app.update.latestVersion ? "Could not check" : latestLabel(app);
      items.push(["Newest version", h("span", {}, newest, h("span", { class: "pkg-info-sub" }, when))]);
    }
    items.push(["Memory limit", `${gb(app.memoryMb)} per person`]);
    items.push(["CPU limit", `${app.cpus} ${app.cpus === 1 ? "core" : "cores"}`]);
    items.push(["Who can use it", app.visibility === "admins" ? "Administrators only" : "Everyone who signs in"]);
    items.push(["Open now", app.running > 0 ? `${app.running} ${app.running === 1 ? "copy" : "copies"}` : "Not running"]);
    if (app.sizeBytes) items.push(["Size", formatBytes(app.sizeBytes)]);
    items.push(["Added", formatDate(app.createdAt)]);
    items.push(["Files", "Your RAST home folder"]);
    // Effective runtime policy + host GPU capability (read-only diagnostics).
    for (const row of nativeRuntimeSummary(linuxOverview()?.runtime)) {
      items.push([row.label, row.value]);
    }
    return h(
      "section",
      { class: "pkg-section" },
      h("h3", { class: "pkg-section-title" }, "Other information"),
      h("dl", { class: "pkg-info" }, ...items.map(([k, v]) => h("div", { class: "pkg-info-item" }, h("dt", {}, k), h("dd", {}, v)))),
    );
  }

  private advanced(app: LinuxApp): HTMLElement {
    const rows: Array<[string, string]> = [
      ["Installed from", app.fileName],
      ["Program", app.command.length ? app.command.join(" ") : "—"],
    ];
    if (app.package) rows.push(["Package", app.package]);
    rows.push(["App ID", app.id]);
    if (app.update.error) rows.push(["Last update check", app.update.error]);
    return h(
      "details",
      { class: "pkg-more" },
      h("summary", {}, "Advanced"),
      h(
        "div",
        { class: "pkg-more-body" },
        h("dl", { class: "pkg-kv" }, ...rows.flatMap(([k, v]) => [h("dt", {}, k), h("dd", { class: "mono" }, v)])),
      ),
    );
  }

  // ------------------------------------------------------------------ install

  /**
   * The one "Install Linux App" dialog: choose a source (upload or https
   * link), let the server validate it, review what would happen, pick who can
   * use the app, then watch the install job's stages until the app is ready.
   *
   * Nothing is installed before the server's preview and a deliberate click,
   * and the values shown always come from the server response (missing fields
   * are simply not shown instead of being guessed).
   */
  openAdd(initialFiles: File[] = []): void {
    const { ctx } = this.host;
    if (!this.enabled || this.host.readOnly()) return;
    if (!this.available) {
      ctx.toast.warning("Installing Linux apps is not available on this server.");
      return;
    }
    const dialogId = nextInstallDialogId++;
    const state: InstallDialogState = {
      tab: "upload",
      phase: "pick",
      source: null,
      origin: null,
      sourceLabel: "",
      urlDraft: "",
      fileError: "",
      preview: null,
      plan: null,
      visibility: "users",
      unsupported: false,
      error: "",
      installing: false,
      app: null,
      closed: false,
    };

    const fileInput = h("input", { type: "file", class: "hidden", accept: PACKAGE_FILE_ACCEPT, multiple: false });
    const uploadTab = h(
      "button",
      {
        class: "tab pkg-install-tab is-active",
        type: "button",
        role: "tab",
        id: `pkg-install-tab-upload-${dialogId}`,
        ariaSelected: true,
      },
      iconSvg("upload", 14),
      "Upload package",
    );
    const urlTab = h(
      "button",
      {
        class: "tab pkg-install-tab",
        type: "button",
        role: "tab",
        id: `pkg-install-tab-url-${dialogId}`,
        ariaSelected: false,
      },
      iconSvg("external", 14),
      "Install from URL",
    );
    uploadTab.setAttribute("aria-controls", `pkg-install-panel-${dialogId}`);
    urlTab.setAttribute("aria-controls", `pkg-install-panel-${dialogId}`);
    const tabs = h(
      "div",
      { class: "tabs pkg-install-tabs", role: "tablist", ariaLabel: "Where the package comes from" },
      uploadTab,
      urlTab,
    );
    const view = h("div", {
      class: "pkg-install-view",
      role: "tabpanel",
      id: `pkg-install-panel-${dialogId}`,
      ariaLabel: "Install source",
      tabIndex: -1,
    });
    const root = h("div", { class: "pkg-install" }, tabs, view, fileInput);

    let close: () => void = () => undefined;
    let stageView: InstallStageView | null = null;
    let previewUi: InstallPreviewUi | null = null;
    let primaryAction: (() => void) | null = null;
    let dismissButton: HTMLButtonElement | null = null;
    let primaryButton: HTMLButtonElement | null = null;
    let uploadedPath: string | null = null;

    const uploads = new UploadManager(ctx, this.host.lc, {
      currentDir: () => DROP_FOLDER,
      onUploaded: (entry) => {
        if (state.source === null) uploadedPath = entry.path;
      },
      onDrained: () => {
        if (state.closed) return;
        const path = uploadedPath;
        uploadedPath = null;
        if (!path) {
          fail("The upload did not finish. Check the file and try again.");
          return;
        }
        state.source = { kind: "path", path };
        state.origin = "upload";
        void runPreview();
      },
      maxUploadBytes: () => ctx.store.get().maxUploadBytes,
    });

    // ------------------------------------------------------------- painting

    const setView = (node: HTMLElement): void => {
      clear(view);
      view.appendChild(node);
    };

    const paintTabs = (): void => {
      tabs.classList.toggle("hidden", state.phase !== "pick");
      const uploadActive = state.tab === "upload";
      uploadTab.classList.toggle("is-active", uploadActive);
      uploadTab.setAttribute("aria-selected", String(uploadActive));
      urlTab.classList.toggle("is-active", !uploadActive);
      urlTab.setAttribute("aria-selected", String(!uploadActive));
      view.setAttribute("aria-labelledby", uploadActive ? uploadTab.id : urlTab.id);
    };

    const paintFoot = (): void => {
      if (dismissButton) {
        dismissButton.textContent =
          state.phase === "ready" ? "Done" : state.phase === "installing" ? "Close" : "Cancel";
      }
      if (!primaryButton) return;
      let show = true;
      switch (state.phase) {
        case "ready":
          primaryButton.textContent = "Open";
          primaryButton.disabled = state.app === null;
          primaryAction = openInstalledApp;
          break;
        case "preview": {
          const blocked =
            state.preview !== null && !state.unsupported && !previewCompatibility(state.preview).ok;
          primaryButton.textContent = state.plan ? previewInstallActionLabel(state.plan) : "Install";
          primaryButton.disabled = state.installing || blocked;
          primaryAction = () => void confirmInstall();
          break;
        }
        case "error":
          primaryButton.textContent = "Try again";
          primaryButton.disabled = state.installing || state.source === null;
          primaryAction = () => void runPreview();
          break;
        default:
          show = false;
          primaryAction = null;
          break;
      }
      primaryButton.classList.toggle("hidden", !show);
    };

    const paintView = (): void => {
      switch (state.phase) {
        case "pick":
          setView(state.tab === "upload" ? renderUploadPicker() : renderUrlPicker());
          break;
        case "preview":
          if (previewUi) setView(previewUi.el);
          else setView(h("div", { class: "pkg-preview-loading" }, "No preview."));
          break;
        case "ready":
          setView(renderReady());
          break;
        case "error":
          setView(renderError());
          break;
        default:
          if (stageView) setView(stageView.el);
          break;
      }
    };

    const restoreFocus = (): void => {
      const active = document.activeElement;
      if (
        active instanceof HTMLElement &&
        active !== document.body &&
        active.isConnected &&
        active.offsetParent !== null
      ) {
        return;
      }
      const target =
        primaryButton && !primaryButton.classList.contains("hidden")
          ? primaryButton
          : state.phase === "pick"
            ? state.tab === "upload"
              ? uploadTab
              : urlTab
            : view;
      target.focus({ preventScroll: true });
    };

    const paint = (): void => {
      paintTabs();
      paintView();
      paintFoot();
      restoreFocus();
    };

    const fail = (message: string): void => {
      state.installing = false;
      state.error = message || "The installation did not finish.";
      state.phase = "error";
      if (!state.closed) paint();
    };

    const resetToPicker = (): void => {
      state.phase = "pick";
      state.source = null;
      state.origin = null;
      state.preview = null;
      state.plan = null;
      previewUi = null;
      stageView = null;
      state.unsupported = false;
      state.error = "";
      state.fileError = "";
      state.installing = false;
      paint();
    };

    const openInstalledApp = (): void => {
      const app = state.app;
      if (!app) return;
      close();
      this.host.ctx.openApp(app.desktopId);
    };

    // --------------------------------------------------------- ready / error

    const renderReady = (): HTMLElement => {
      const app = state.app;
      if (!app) return h("div");
      const who =
        app.visibility === "admins"
          ? "Only administrators can see and open it."
          : "Everyone who signs in can see and open it, each with their own copy.";
      const desktopButton = ctx.desktop.isOnDesktop(app.desktopId)
        ? h("span", { class: "pkg-install-note" }, "Its icon is already on the desktop.")
        : (() => {
            const add = h(
              "button",
              { class: "btn btn-secondary btn-sm", type: "button" },
              iconSvg("plus", 14),
              "Add to desktop",
            );
            add.addEventListener("click", () => {
              ctx.desktop.addToDesktop(app.desktopId);
              paintView();
            });
            return add;
          })();
      return h(
        "div",
        { class: "pkg-install-ready" },
        h("div", { class: "pkg-install-ready-icon", ariaHidden: true }, iconSvg("check", 30)),
        h("div", { class: "pkg-install-ready-title" }, `${app.name} is ready`),
        app.version ? h("div", { class: "pkg-install-ready-sub" }, `Version ${app.version}`) : null,
        h("div", { class: "pkg-install-ready-detail small muted" }, `The app is installed on the server. ${who}`),
        h("div", { class: "pkg-install-ready-actions" }, desktopButton),
      );
    };

    const renderError = (): HTMLElement => {
      return h(
        "div",
        { class: "pkg-install-error" },
        banner("error", "The installation did not finish", state.error),
        state.source
          ? h("div", { class: "pkg-install-source small muted" }, `Source: ${state.sourceLabel}`)
          : null,
        h(
          "p",
          { class: "pkg-install-hint small muted" },
          "Try again checks and installs the same package once more. If it keeps failing, use the error above to fix the cause first.",
        ),
        h(
          "div",
          { class: "pkg-install-error-actions" },
          h(
            "button",
            {
              class: "btn btn-ghost btn-sm",
              type: "button",
              onClick: () => {
                resetToPicker();
              },
            },
            "Choose another source",
          ),
        ),
      );
    };

    // --------------------------------------------------------------- picking

    const renderUploadPicker = (): HTMLElement => {
      const zone = h(
        "div",
        {
          class: "pkg-install-drop",
          role: "button",
          tabIndex: 0,
          ariaLabel: "Choose a package file, or drop one here",
        },
        h("div", { class: "pkg-install-drop-icon", ariaHidden: true }, iconSvg("upload", 30)),
        h("div", { class: "pkg-install-drop-title" }, "Choose a package or drop it here"),
        h("div", { class: "pkg-install-drop-hint" }, PACKAGE_FILE_HINT),
      );
      const openFile = (): void => fileInput.click();
      zone.addEventListener("click", openFile);
      zone.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        openFile();
      });
      zone.addEventListener("dragover", (event) => {
        if (!hasFileDrag(event)) return;
        event.preventDefault();
        zone.classList.add("is-over");
      });
      zone.addEventListener("dragleave", () => zone.classList.remove("is-over"));
      zone.addEventListener("drop", (event) => {
        if (!hasFileDrag(event)) return;
        event.preventDefault();
        event.stopPropagation();
        zone.classList.remove("is-over");
        const files = Array.from(event.dataTransfer?.files ?? []);
        if (files.length > 0) startFileUpload(files);
      });
      const explained = h(
        "ul",
        { class: "pkg-install-formats" },
        h("li", {}, h("strong", {}, ".deb"), " packages are fully supported (installed by the server's package manager)."),
        h("li", {}, "Portable ", h("strong", {}, ".tar.gz"), " archives are supported when they contain a desktop launcher."),
        h("li", {}, "AppImage is not offered because it is not validated on this server."),
      );
      const station = h(
        "button",
        { class: "btn btn-ghost btn-sm pkg-install-station", type: "button" },
        iconSvg("folder", 14),
        "Pick from File Station",
      );
      station.addEventListener("click", () => {
        this.pickFromStation((path) => {
          if (state.closed) return;
          state.source = { kind: "path", path };
          state.origin = "station";
          state.sourceLabel = path;
          state.fileError = "";
          void runPreview();
        });
      });
      return h(
        "div",
        { class: "pkg-install-pick col" },
        zone,
        state.fileError ? h("div", { class: "form-error", role: "alert" }, state.fileError) : null,
        h("div", { class: "pkg-install-pick-actions" }, station),
        explained,
      );
    };

    const renderUrlPicker = (): HTMLElement => {
      const link = h("input", {
        class: "input",
        type: "url",
        placeholder: "https://vendor.example/product_1.0_amd64.deb",
        ariaLabel: "Official download link (https)",
      });
      link.value = state.urlDraft;
      const error = h("div", { class: "form-error", role: "alert" });
      const submit = h("button", { class: "btn btn-secondary btn-sm", type: "button" }, "Continue");
      const submitLink = (): void => {
        const url = link.value.trim();
        state.urlDraft = url;
        if (!url.toLowerCase().startsWith("https://")) {
          error.textContent = "Enter a link that starts with https://";
          link.focus();
          return;
        }
        error.textContent = "";
        chooseUrl(url);
      };
      link.addEventListener("input", () => {
        state.urlDraft = link.value;
        error.textContent = "";
      });
      link.addEventListener("keydown", (event) => {
        if (event.key !== "Enter") return;
        event.preventDefault();
        submitLink();
      });
      submit.addEventListener("click", submitLink);
      return h(
        "div",
        { class: "pkg-install-pick col" },
        h(
          "label",
          { class: "field" },
          h("span", { class: "field-label" }, "Official download link"),
          link,
        ),
        h(
          "p",
          { class: "pkg-install-hint small muted" },
          "Use the official vendor's https:// link. The server downloads and validates the file; nothing is installed until you confirm.",
        ),
        error,
        h("div", { class: "pkg-install-pick-actions" }, submit),
      );
    };

    const chooseUrl = (url: string): void => {
      state.source = { kind: "url", url };
      state.origin = "url";
      state.sourceLabel = url;
      state.fileError = "";
      state.error = "";
      state.preview = null;
      state.plan = null;
      previewUi = null;
      state.unsupported = false;
      void runPreview();
    };

    const startFileUpload = (files: File[]): void => {
      const file = files[0];
      if (!file || state.installing) return;
      if (files.length > 1) ctx.toast.info("Only one package is installed at a time.");
      const problem = packageFileProblem(file.name);
      if (problem) {
        state.fileError = problem;
        state.tab = "upload";
        paint();
        return;
      }
      state.source = null;
      state.sourceLabel = file.name;
      state.fileError = "";
      state.error = "";
      state.preview = null;
      state.plan = null;
      previewUi = null;
      state.unsupported = false;
      uploadedPath = null;
      state.phase = "preparing";
      stageView = this.installStageView({
        stage: "uploading",
        title: `Uploading ${file.name}`,
        note: "The package is uploaded to your Downloads folder first, then the server checks it.",
      });
      stageView.update("uploading", `Uploading ${file.name}…`, 0, 0);
      setView(h("div", { class: "pkg-install-phase" }, stageView.el, uploads.el));
      paint();
      void (async () => {
        try {
          await ctx.api.post("/api/files/mkdir", { path: DROP_FOLDER });
        } catch {
          /* the folder is already there */
        }
        if (state.closed) return;
        uploads.enqueue([file], DROP_FOLDER);
      })();
    };

    // ------------------------------------------------------- preview + install

    const runPreview = async (): Promise<void> => {
      const source = state.source;
      if (!source) return;
      state.installing = false;
      state.error = "";
      state.unsupported = false;
      state.preview = null;
      state.plan = null;
      previewUi = null;
      state.phase = "validating";
      stageView = this.installStageView({
        stage: "validating",
        title: "Validating the package",
        note: "The server is inspecting the file. Nothing has been installed yet.",
      });
      stageView.update("validating", "Validating the package…", 0, 0);
      paint();
      try {
        const res = await ctx.api.post<{ preview?: LinuxInstallPreview }>(
          "/api/linux-apps/preview",
          source.kind === "path" ? { path: source.path } : { url: source.url },
        );
        if (state.closed) return;
        if (!res?.preview) throw new Error("The server did not describe this package.");
        const availability = previewAvailability(res.preview);
        state.preview = res.preview;
        state.visibility = availability.defaultVisibility;
        state.plan = previewInstallPlan(res.preview);
        const sourceDisplay =
          source.kind === "url"
            ? { label: "Downloaded from", value: source.url }
            : { label: state.origin === "upload" ? "Uploaded to" : "File Station path", value: source.path };
        previewUi = this.installPreviewUi(res.preview, state.plan, availability, resetToPicker, sourceDisplay);
        state.phase = "preview";
        paint();
      } catch (error) {
        if (state.closed) return;
        const status = (error as { status?: number }).status;
        if (status === 404 || status === 405 || status === 501) {
          state.unsupported = true;
          state.plan = null;
          const sourceDisplay =
            source.kind === "url"
              ? { label: "Downloaded from", value: source.url }
              : { label: state.origin === "upload" ? "Uploaded to" : "File Station path", value: source.path };
          previewUi = this.unsupportedPreviewUi(sourceDisplay);
          state.phase = "preview";
          paint();
          return;
        }
        fail(errorMessage(error));
      }
    };

    const confirmInstall = async (): Promise<void> => {
      if (state.phase !== "preview" || state.installing || !state.source) return;
      state.installing = true;
      paintFoot();
      const source = state.source;
      const preview = state.preview;
      const launcher = previewUi?.launcher();
      // The availability radios live in the preview UI; read the current choice
      // at confirmation time (state.visibility only holds the server default).
      const visibility = previewUi?.visibility() ?? state.visibility;
      let res: InstallResponse;
      try {
        res = await this.requestInstall(source, launcher, visibility);
      } catch (error) {
        if (state.closed) return;
        fail(errorMessage(error));
        return;
      }
      if (state.closed) {
        // The request started the job; the App Center list keeps updating.
        if (res.jobId) void this.follow(res.jobId);
        else void this.reload();
        return;
      }
      state.app = res.app ?? null;
      const responseAppId = res.app?.id ?? null;
      const jobId = typeof res.jobId === "string" && res.jobId !== "" ? res.jobId : null;
      if (!jobId) {
        // Docker-only/older servers answer with the finished app already.
        await this.reload();
        const app = res.app ?? (preview ? this.findInstalledApp(preview) : null);
        state.installing = false;
        if (state.closed) return;
        if (app) {
          state.app = app;
          state.phase = "ready";
          paint();
        } else {
          fail("The server did not start an installation job. Open App Center and try again.");
        }
        return;
      }
      state.phase = "installing";
      stageView = this.installStageView({
        stage: "validating",
        title: preview ? `Installing ${previewName(preview)}` : "Installing the app",
        note: "You can close this dialog: the installation continues in the background and the app appears in App Center when it is ready.",
      });
      stageView.update("validating", "Queued", 0, 0);
      paint();
      // Show the installing tile behind the dialog and reattach its progress.
      void this.reload();
      const outcome = await this.follow(jobId, (update) => {
        if (state.closed || state.phase !== "installing") return;
        const stage = installStageFromMessage(update.message, update.progress, update.total);
        stageView?.update(stage, update.message, update.progress, update.total);
      });
      state.installing = false;
      if (state.closed) return;
      if (outcome.status === "done") {
        const app = await this.waitForInstalledApp(preview, appIdFromJobResult(outcome.result) ?? responseAppId);
        if (state.closed) return;
        if (app?.status === "ready") {
          state.app = app;
          state.phase = "ready";
          paint();
          return;
        }
        if (app?.status === "failed") {
          fail(app.error ?? "The server installed the package but could not register the app.");
          return;
        }
        fail("The install job finished, but the app is not ready in App Center yet. Open App Center again to check it.");
        return;
      }
      fail(outcome.status === "cancelled" ? "The installation was cancelled." : outcome.error ?? "The installation did not finish.");
    };

    // ------------------------------------------------------------ dialog shell

    close = modal({
      title: "Install Linux App",
      body: root,
      wide: true,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Install",
          variant: "primary",
          keepOpen: true,
          onClick: () => {
            primaryAction?.();
          },
        },
      ],
      onClose: () => {
        state.closed = true;
        uploads.el.remove();
        if (state.phase === "installing") {
          ctx.toast.info("The installation continues in the background. The app appears in App Center when it is ready.");
        } else if (state.phase === "preparing") {
          ctx.toast.info("The upload continues in the background.");
        }
      },
    });
    const foot = root.closest(".modal")?.querySelector<HTMLElement>(".modal-foot") ?? null;
    const footButtons = foot ? Array.from(foot.querySelectorAll<HTMLButtonElement>(".btn")) : [];
    dismissButton = footButtons[0] ?? null;
    primaryButton = footButtons[1] ?? null;
    if (dismissButton) dismissButton.title = "Close the install dialog";

    tabs.addEventListener("keydown", (event) => {
      if (state.phase !== "pick") return;
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      state.tab = state.tab === "upload" ? "url" : "upload";
      paint();
      (state.tab === "upload" ? uploadTab : urlTab).focus();
    });
    const selectTab = (tab: "upload" | "url"): void => {
      if (state.phase !== "pick" || state.tab === tab) return;
      state.tab = tab;
      paint();
    };
    uploadTab.addEventListener("click", () => selectTab("upload"));
    urlTab.addEventListener("click", () => selectTab("url"));
    fileInput.addEventListener("change", () => {
      const files = Array.from(fileInput.files ?? []);
      fileInput.value = "";
      if (files.length > 0) startFileUpload(files);
    });

    paint();
    if (initialFiles.length > 0) startFileUpload(initialFiles);
  }

  /**
   * Stage list + progress bar used while uploading, validating and installing.
   * `update` repaints in place so job messages never rebuild (and blur) the
   * dialog.
   */
  private installStageView(options: { stage: InstallStage; title: string; note?: string }): InstallStageView {
    interface StageItem {
      root: HTMLElement;
      dot: HTMLElement;
    }
    const items: StageItem[] = INSTALL_STAGES.map((stage, index) => {
      const dot = h("span", { class: "pkg-install-stage-dot", ariaHidden: true }, String(index + 1));
      const item = h(
        "li",
        { class: "pkg-install-stage is-pending" },
        dot,
        h("span", { class: "pkg-install-stage-label" }, installStageLabel(stage)),
      );
      return { root: item, dot };
    });
    const bar: ProgressHandle = progressBar();
    bar.el.setAttribute("aria-label", "Installation progress");
    const message = h("div", { class: "pkg-install-message", role: "status", ariaLive: "polite" });
    const el = h(
      "div",
      { class: "pkg-install-phase pkg-install-staging" },
      h("div", { class: "pkg-install-phase-title" }, options.title),
      h("ol", { class: "pkg-install-stages", ariaLabel: "Installation stages" }, ...items.map((item) => item.root)),
      h("div", { class: "pkg-install-progress" }, bar.el),
      message,
      options.note ? h("p", { class: "pkg-install-hint small muted" }, options.note) : null,
    );
    const barEl = (): HTMLElement | null => bar.el.querySelector<HTMLElement>(".progress-bar");
    let current: InstallStage = options.stage;
    const setItem = (index: number, kind: "done" | "active" | "pending" | "failed"): void => {
      const item = items[index];
      if (!item) return;
      item.root.classList.remove("is-done", "is-active", "is-pending", "is-failed");
      item.root.classList.add(`is-${kind}`);
      if (kind === "active") item.root.setAttribute("aria-current", "step");
      else item.root.removeAttribute("aria-current");
      if (kind === "done" || (kind === "active" && current === "ready")) {
        item.dot.replaceChildren(iconSvg("check", 13));
      } else if (kind === "active") {
        item.dot.replaceChildren(h("span", { class: "spinner pkg-install-stage-spinner" }));
      } else if (kind === "failed") {
        item.dot.replaceChildren(iconSvg("warning", 13));
      } else {
        item.dot.textContent = String(index + 1);
      }
    };
    const apply = (stage: InstallStage, text: string, progress: number, total: number): void => {
      current = stage;
      const active = installStageIndex(stage);
      items.forEach((_, index) => {
        if (stage === "ready" || index < active) setItem(index, "done");
        else if (index === active) setItem(index, "active");
        else setItem(index, "pending");
      });
      const fraction = installProgressFraction(progress, total);
      if (total <= 0 && stage !== "ready") {
        // No step count yet: show an indeterminate bar instead of fake progress.
        barEl()?.classList.add("is-indeterminate");
        bar.update(1);
        bar.el.setAttribute("aria-busy", "true");
        bar.el.removeAttribute("aria-valuenow");
      } else {
        barEl()?.classList.remove("is-indeterminate");
        bar.el.removeAttribute("aria-busy");
        bar.update(stage === "ready" ? 1 : fraction, text || undefined);
      }
      bar.setError(false);
      message.textContent = text || `${installStageLabel(stage)}…`;
    };
    return {
      el,
      update: (stage, text, progress, total) => apply(stage, text, progress, total),
    };
  }

  /**
   * The server's preview, grouped for the install dialog: what it detected,
   * where it comes from, whether it fits this server, what changes, whether
   * the app/packages/data already exist, and who may use the installed app.
   */
  private installPreviewUi(
    preview: LinuxInstallPreview,
    plan: PreviewInstallPlan,
    availability: PreviewAvailability,
    onChooseAnother: () => void,
    sourceDisplay: { label: string; value: string } | null,
  ): InstallPreviewUi {
    const compatibility = previewCompatibility(preview);
    const transaction = previewTransaction(preview);
    const dependencies = previewDependencies(preview);
    const host = previewHost(preview);
    const profile = previewProfile(preview);
    const existing = plan.existingApp;
    const name = previewName(preview);
    const version = previewVersion(preview);
    const format = previewFormat(preview);
    const serverSource = previewSource(preview);
    // Newer servers report `source: "file"`; the dialog knows the URL/path.
    const source = serverSource && serverSource.kind !== "file" ? serverSource : sourceDisplay;
    const packageName = previewPackageName(preview);
    const size = previewSizeBytes(preview);
    const packageArch = previewArchitecture(preview);

    const head = h(
      "div",
      { class: "pkg-preview-head pkg-install-preview-head" },
      h("div", { class: "pkg-icon pkg-icon-hero is-app", ariaHidden: true }, iconSvg("package", 40)),
      h(
        "div",
        { class: "pkg-preview-title" },
        h("div", { class: "pkg-preview-name" }, name),
        h(
          "div",
          { class: "pkg-preview-sub" },
          [
            version ? `Version ${version}` : "",
            format,
            size !== null && size > 0 ? formatBytes(size) : "",
          ]
            .filter((part) => part !== "")
            .join(" · "),
        ),
      ),
      h(
        "button",
        {
          class: "btn btn-ghost btn-sm pkg-install-change",
          type: "button",
          onClick: () => onChooseAnother(),
        },
        "Choose another source",
      ),
    );

    const parts: Array<HTMLElement | null> = [head];
    if (existing) {
      const detail = [existing.name ?? name, existing.version ? `version ${existing.version}` : "", existing.status ?? ""]
        .filter((part) => part !== "")
        .join(" · ");
      parts.push(
        banner(
          "warning",
          "This app is already in App Center",
          `${detail}${existing.reason ? ` — ${existing.reason}` : ""}. Installing again may create a second entry.`,
        ),
      );
    }
    if (plan.hostPackageInstalled) {
      parts.push(
        banner(
          "info",
          "The package is already installed on this server",
          plan.mode === "register"
            ? "RAST adds the existing installation to App Center instead of installing the package again."
            : "The server reuses the package that is already installed.",
        ),
      );
    }
    if (compatibility.ok) {
      parts.push(
        banner(
          "success",
          "Compatible with this server",
          compatibility.warnings.length > 0 ? compatibility.warnings.join(" · ") : undefined,
        ),
      );
    } else {
      parts.push(
        banner(
          "error",
          "This package cannot be installed here",
          [...compatibility.reasons, ...compatibility.warnings].join(" · ") || "The server reported an incompatibility.",
        ),
      );
    }

    const sourceRows: Array<[string, string]> = [];
    if (format) sourceRows.push(["Format", format]);
    if (source) sourceRows.push([source.label, source.value]);
    if (packageName) sourceRows.push(["Package", packageName]);
    if (packageArch) sourceRows.push(["Package architecture", packageArch]);
    if (host.distribution) sourceRows.push(["Server distribution", host.distribution]);
    if (host.architecture) sourceRows.push(["Server architecture", host.architecture]);
    parts.push(
      h(
        "div",
        { class: "pkg-preview-section" },
        h("div", { class: "pkg-preview-section-title" }, "Package and source"),
        h(
          "div",
          { class: "pkg-preview-list" },
          ...sourceRows.map(([label, value]) =>
            h(
              "div",
              { class: "pkg-preview-row" },
              h("span", {}, label),
              h("span", { class: "pkg-preview-mono" }, value),
            ),
          ),
        ),
      ),
    );

    if (transaction.hasChanges || transaction.preexisting) {
      const rows = transaction.rows.map((row) =>
        h(
          "div",
          {
            class: `pkg-preview-row pkg-install-transaction is-${row.kind}`,
          },
          h("span", {}, row.kind === "remove" ? "Remove" : row.kind === "upgrade" ? "Upgrade" : "Install"),
          h(
            "span",
            { class: "pkg-preview-mono" },
            row.kind === "upgrade" && row.oldVersion
              ? `${row.name} ${row.oldVersion} → ${row.version}`
              : `${row.name} ${row.version}`,
          ),
        ),
      );
      const summaryParts: string[] = [];
      if (transaction.installs > 0) summaryParts.push(`${transaction.installs} to install`);
      if (transaction.upgrades > 0) summaryParts.push(`${transaction.upgrades} to upgrade`);
      if (transaction.removals > 0) summaryParts.push(`${transaction.removals} to remove`);
      if (transaction.downloadBytes > 0) summaryParts.push(`${formatBytes(transaction.downloadBytes)} to download`);
      parts.push(
        h(
          "div",
          { class: "pkg-preview-section" },
          h(
            "div",
            { class: "pkg-preview-section-title" },
            transaction.preexisting ? "Package changes (package already on the server)" : "Package changes",
          ),
          summaryParts.length > 0 ? h("div", { class: "pkg-preview-sub" }, summaryParts.join(" · ")) : null,
          rows.length > 0 ? h("div", { class: "pkg-preview-list" }, ...rows) : null,
          transaction.removals > 0
            ? banner(
                "warning",
                "Packages will be removed",
                "These packages exist on the server and would be removed to install this app.",
              )
            : null,
        ),
      );
    }
    if (dependencies.length > 0) {
      parts.push(
        h(
          "div",
          { class: "pkg-preview-section" },
          h("div", { class: "pkg-preview-section-title" }, "Dependencies"),
          h("div", { class: "pkg-preview-mono" }, dependencies.join(", ")),
        ),
      );
    }

    const hostPackage = plan.hostPackageInstalled
      ? "Already installed (reused)"
      : format === "DEB"
        ? "Will be installed by the server's package manager"
        : "Extracted into RAST's app folder";
    const existingLabel = existing
      ? [existing.name ?? name, existing.version ?? "", existing.status ?? ""].filter((part) => part !== "").join(" · ")
      : "None found";
    parts.push(
      h(
        "div",
        { class: "pkg-preview-section" },
        h("div", { class: "pkg-preview-section-title" }, "Existing installation and data"),
        h(
          "div",
          { class: "pkg-preview-list" },
          h(
            "div",
            { class: "pkg-preview-row" },
            h("span", {}, "Host package"),
            h("span", { class: "pkg-preview-mono" }, hostPackage),
          ),
          h(
            "div",
            { class: "pkg-preview-row" },
            h("span", {}, "App Center entry"),
            h("span", { class: "pkg-preview-mono" }, existingLabel),
          ),
          h(
            "div",
            { class: "pkg-preview-row" },
            h("span", {}, "Saved app data"),
            h("span", { class: "pkg-preview-mono" }, previewProfileSummary(profile)),
          ),
        ),
      ),
    );

    // Who can use the app: the server lists the choices; users is the default.
    const group = `pkg-install-visibility-${nextInstallDialogId++}`;
    const availabilityBox = h("fieldset", { class: "pkg-install-availability" });
    if (availability.options.length > 1) {
      availabilityBox.append(
        h("legend", { class: "pkg-install-availability-title" }, "Who can use this app"),
        ...availability.options.map((option) =>
          h(
            "label",
            { class: "pkg-install-choice" },
            h("input", {
              type: "radio",
              name: group,
              value: option,
              checked: option === availability.defaultVisibility,
            }),
            h("span", {}, option === "users" ? "All RAST users" : "Administrators only"),
          ),
        ),
        h(
          "div",
          { class: "pkg-install-hint small muted" },
          "Everyone you allow still gets their own copy with separate settings, sign-ins and windows. Installing, updating and removing the app stays with administrators.",
        ),
      );
    } else {
      // Older servers: visibility can still be chosen after installing from the app's page.
      availabilityBox.append(
        h("legend", { class: "pkg-install-availability-title" }, "Who can use this app"),
        h("div", { class: "pkg-install-hint small muted" }, "The server did not offer availability choices; the app is visible to everyone who signs in. You can change this later from the app's page."),
      );
    }
    parts.push(availabilityBox);

    const candidates = previewCandidates(preview);
    let launcherSelect: HTMLSelectElement | null = null;
    if (previewNeedsChoice(preview) && candidates.length > 0) {
      launcherSelect = h("select", { class: "input", ariaLabel: "Program to start" });
      candidates.forEach((candidate, index) => {
        launcherSelect?.appendChild(
          h("option", { value: String(index), selected: index === 0 }, previewLauncherOwner(candidate)),
        );
      });
      parts.push(
        h(
          "label",
          { class: "field pkg-install-launcher" },
          h("span", { class: "field-label" }, "Program to start"),
          launcherSelect,
          h("span", { class: "field-hint" }, "The archive contains more than one program. Pick the one RAST should start."),
        ),
      );
    }

    const updateSource = preview.update?.source ?? preview.updateSource ?? null;
    if (updateSource && (updateSource.kind || updateSource.url)) {
      parts.push(
        banner(
          "info",
          "Future updates can be fetched automatically",
          updateSource.url ?? updateSource.kind ?? "",
        ),
      );
    }

    const visibility = (): LinuxAppVisibility => {
      const checked = availabilityBox.querySelector<HTMLInputElement>('input[type="radio"]:checked');
      return checked?.value === "admins" ? "admins" : checked?.value === "users" ? "users" : availability.defaultVisibility;
    };
    const launcher = (): LinuxPreviewLauncher | undefined => {
      if (!launcherSelect) return undefined;
      const index = Number.parseInt(launcherSelect.value, 10);
      return candidates[Number.isFinite(index) ? index : 0];
    };
    return {
      el: h(
        "div",
        { class: "pkg-preview pkg-install-preview" },
        ...parts.filter((part): part is HTMLElement => part !== null),
      ),
      visibility,
      launcher,
    };
  }

  /** Preview view for servers without the preview endpoint (older/Docker-only). */
  private unsupportedPreviewUi(sourceDisplay: { label: string; value: string } | null): InstallPreviewUi {
    return {
      el: h(
        "div",
        { class: "pkg-preview pkg-install-preview" },
        banner(
          "warning",
          "This server cannot preview the package",
          "This version of the server does not describe packages before installing. You can still install it, but compatibility and package changes cannot be checked first.",
        ),
        sourceDisplay
          ? h(
              "div",
              { class: "pkg-preview-section" },
              h("div", { class: "pkg-preview-section-title" }, "Source"),
              h("div", { class: "pkg-preview-mono" }, `${sourceDisplay.label}: ${sourceDisplay.value}`),
            )
          : null,
      ),
      visibility: () => "users",
      launcher: () => undefined,
    };
  }

  /**
   * POST the install request with the chosen availability. A server that does
   * not know the additive `visibility` field yet rejects the body; retry once
   * without it so the UI also works before that backend change lands.
   */
  private async requestInstall(
    source: InstallSource,
    launcher: LinuxPreviewLauncher | undefined,
    visibility: LinuxAppVisibility,
  ): Promise<InstallResponse> {
    const { ctx } = this.host;
    const body: Record<string, unknown> = source.kind === "path" ? { path: source.path } : { url: source.url };
    if (launcher) body.launcher = launcher;
    body.visibility = visibility;
    const send = (): Promise<InstallResponse> => ctx.api.post<InstallResponse>("/api/linux-apps/install", body);
    try {
      return await send();
    } catch (error) {
      const status = (error as { status?: number }).status;
      if (status === 400 && /unknown field [`'"]?visibility/i.test(errorMessage(error))) {
        delete body.visibility;
        return send();
      }
      throw error;
    }
  }

  /** The App Center row the finished install created or updated, if present. */
  private findInstalledApp(preview: LinuxInstallPreview | null, appId: string | null = null): LinuxApp | null {
    const apps = linuxApps();
    if (apps.length === 0) return null;
    if (appId) {
      const byResultId = apps.find((app) => app.id === appId);
      if (byResultId) return byResultId;
    }
    if (preview) {
      const existing = previewDuplicate(preview);
      if (existing?.id) {
        const byId = apps.find((app) => app.id === existing.id);
        if (byId) return byId;
      }
      const fileName = previewFileName(preview);
      if (fileName) {
        const byFile = apps.find((app) => app.fileName === fileName);
        if (byFile) return byFile;
      }
      const packageName = previewPackageName(preview);
      if (packageName) {
        const byPackage = apps.find((app) => app.package === packageName);
        if (byPackage) return byPackage;
      }
      const name = previewName(preview).toLowerCase();
      const byName = apps.find((app) => app.name.toLowerCase() === name);
      if (byName) return byName;
    }
    return apps.reduce<LinuxApp | null>((newest, app) => (!newest || app.updatedAt > newest.updatedAt ? app : newest), null);
  }

  /**
   * Never report success from the job alone: after the job finished, reload the
   * list until the app row exists and left the "installing" state (or give the
   * server a few seconds and return what the list has).
   */
  private async waitForInstalledApp(
    preview: LinuxInstallPreview | null,
    appId: string | null = null,
  ): Promise<LinuxApp | null> {
    let app: LinuxApp | null = null;
    for (let attempt = 0; attempt < 12; attempt += 1) {
      await this.reload();
      app = this.findInstalledApp(preview, appId);
      if (app && app.status !== "installing") return app;
      if (attempt < 11) {
        await new Promise<void>((resolve) => {
          window.setTimeout(resolve, 500);
        });
      }
    }
    return app;
  }

  /** Secondary source: a package already in File Station (no upload). */
  private pickFromStation(onPick: (path: string) => void): void {
    const { ctx, lc } = this.host;
    void pickDestination(ctx, lc, {
      title: "Install Linux App",
      confirmLabel: "Choose",
      startPath: "/",
      files: (entry) => packageFileProblem(entry.name) === null,
      emptyHint: `No supported packages here. Use ${PACKAGE_FILE_HINT}; upload one with File Station first.`,
    }).then((path) => {
      if (path) onPick(path);
    });
  }

  /**
   * Ask the server to inspect a file before updating. Shows a modal with
   * progress (a .deb package transaction can take a few seconds) and resolves
   * with the preview, `unsupported` for older servers, or an error.
   */
  private async previewInstall(source: InstallSource): Promise<PreviewResult> {
    const { ctx } = this.host;
    const body = h(
      "div",
      { class: "pkg-preview" },
      h(
        "div",
        { class: "pkg-preview-loading" },
        h("span", { class: "spinner" }),
        h("span", {}, "Checking the update…"),
      ),
    );
    const close = modal({ title: "Update from file", body, dismissible: false });
    try {
      const res = await ctx.api.post<{ preview: LinuxInstallPreview }>(
        "/api/linux-apps/preview",
        source.kind === "path" ? { path: source.path } : { url: source.url },
      );
      if (!res?.preview) return { error: "The server did not describe this file." };
      return { preview: res.preview };
    } catch (error) {
      const status = (error as { status?: number }).status;
      if (status === 404 || status === 405 || status === 501) return { unsupported: true };
      return { error: errorMessage(error) };
    } finally {
      close();
    }
  }

  /** Explicit confirm of the server's update preview. */
  private confirmPreview(preview: LinuxInstallPreview): Promise<{ ok: boolean; launcher?: LinuxPreviewLauncher }> {
    const candidates = previewCandidates(preview);
    const needsChoice = previewNeedsChoice(preview) && candidates.length > 0;
    return new Promise<{ ok: boolean; launcher?: LinuxPreviewLauncher }>((resolve) => {
      let settled = false;
      const finish = (ok: boolean, launcher?: LinuxPreviewLauncher): void => {
        if (settled) return;
        settled = true;
        resolve({ ok, ...(launcher ? { launcher } : {}) });
      };
      const chooser = needsChoice
        ? (() => {
            const select = h("select", { class: "input", ariaLabel: "Program to start" });
            candidates.forEach((candidate, index) => {
              select.appendChild(
                h(
                  "option",
                  { value: String(index), selected: index === 0 },
                  previewLauncherOwner(candidate),
                ),
              );
            });
            return { select, element: h("label", { class: "field" }, h("span", { class: "field-label" }, "Program to start"), select) };
          })()
        : null;
      const body = h("div", { class: "col" }, this.renderPreview(preview), chooser?.element ?? null);
      const chosen = (): LinuxPreviewLauncher | undefined => {
        if (!chooser) return undefined;
        const index = Number.parseInt(chooser.select.value, 10);
        return candidates[Number.isFinite(index) ? index : 0];
      };
      modal({
        title: `Update ${previewName(preview)}?`,
        body,
        wide: true,
        actions: [
          { label: "Cancel", variant: "ghost", onClick: () => finish(false) },
          { label: "Update", variant: "primary", onClick: () => finish(true, chosen()) },
        ],
        onClose: () => finish(false),
      });
    });
  }

  private renderPreview(preview: LinuxInstallPreview): HTMLElement {
    const compatibility = previewCompatibility(preview);
    const transaction = previewTransaction(preview);
    const dependencies = previewDependencies(preview);
    const identity = previewIdentity(preview);
    const version = previewVersion(preview);
    const size = previewSizeBytes(preview);
    const title = "This update will";
    const head = h(
      "div",
      { class: "pkg-preview-head" },
      h("div", { class: "pkg-icon pkg-icon-hero is-app", ariaHidden: true }, iconSvg("package", 40)),
      h(
        "div",
        { class: "pkg-preview-title" },
        h("div", { class: "pkg-preview-name" }, previewName(preview)),
        h(
          "div",
          { class: "pkg-preview-sub" },
          [
            previewFileName(preview) ?? "",
            version ? `version ${version}` : "",
            previewArchitecture(preview) ?? "",
            size ? formatBytes(size) : "",
          ]
            .filter((part) => part !== "")
            .join(" · "),
        ),
      ),
    );
    const identityList = h(
      "div",
      { class: "pkg-preview-list" },
      ...identity.map((row) =>
        h(
          "div",
          { class: "pkg-preview-row" },
          h("span", {}, row.label),
          h("span", { class: "pkg-preview-mono" }, row.value),
        ),
      ),
    );
    const warnings = compatibility.warnings;
    const compatNotice = compatibility.ok
      ? banner("success", "Compatible with this server", warnings.length > 0 ? warnings.join(" · ") : dependencies.length ? `Needs: ${dependencies.join(", ")}` : undefined)
      : banner("error", "This file is not fully compatible", compatibility.reasons.join(" · ") || "The server reported an incompatibility.");
    const compatNode = compatNotice;
    const format = previewFormat(preview);
    const parts: HTMLElement[] = [
      head,
      h(
        "div",
        { class: "pkg-preview-section" },
        h("div", { class: "pkg-preview-section-title" }, format ? `File (${format})` : "File"),
        identityList,
      ),
      compatNode,
    ];
    if (transaction.removals > 0) {
      parts.push(
        banner(
          "warning",
          "Packages will be removed",
          "These packages exist on the server and would be removed to install this app.",
        ),
      );
    }
    if (transaction.hasChanges || transaction.preexisting) {
      const rows = transaction.rows.map((row) =>
        h(
          "div",
          {
            class: `pkg-preview-row ${row.kind === "remove" ? "is-remove" : row.kind === "upgrade" ? "is-upgrade" : "is-install"}`,
          },
          h("span", {}, row.kind === "remove" ? "Remove" : row.kind === "upgrade" ? "Upgrade" : "Install"),
          h(
            "span",
            { class: "pkg-preview-mono" },
            row.kind === "upgrade" && row.oldVersion
              ? `${row.name} ${row.oldVersion} → ${row.version}`
              : `${row.name} ${row.version}`,
          ),
        ),
      );
      const summaryParts: string[] = [];
      if (transaction.installs) summaryParts.push(`${transaction.installs} to install`);
      if (transaction.upgrades) summaryParts.push(`${transaction.upgrades} to upgrade`);
      if (transaction.removals) summaryParts.push(`${transaction.removals} to remove`);
      if (transaction.downloadBytes > 0) summaryParts.push(`${formatBytes(transaction.downloadBytes)} to download`);
      parts.push(
        h(
          "div",
          { class: "pkg-preview-section" },
          h("div", { class: "pkg-preview-section-title" }, transaction.preexisting ? "Package changes (package already on the server)" : "Package changes"),
          summaryParts.length ? h("div", { class: "pkg-preview-sub" }, summaryParts.join(" · ")) : null,
          rows.length ? h("div", { class: "pkg-preview-list" }, ...rows) : null,
        ),
      );
    }
    if (dependencies.length > 0) {
      parts.push(
        h(
          "div",
          { class: "pkg-preview-section" },
          h("div", { class: "pkg-preview-section-title" }, "Dependencies"),
          h("div", { class: "pkg-preview-mono" }, dependencies.join(", ")),
        ),
      );
    }
    const updateSource = preview.update?.source ?? preview.updateSource ?? null;
    if (updateSource && (updateSource.kind || updateSource.url)) {
      parts.push(
        banner(
          "info",
          "Future updates can be fetched automatically",
          updateSource.url ?? updateSource.kind ?? "",
        ),
      );
    }
    parts.push(h("p", { class: "pkg-preview-sub" }, title + ": " + previewSummary(preview).replace(/^Install /, "")));
    return h("div", { class: "pkg-preview" }, ...parts);
  }

  /* -------------------------------------------------- native update actions */

  private openUpdateFromFile(app: LinuxApp): void {
    const input = h("input", {
      type: "file",
      class: "hidden",
      accept: ".deb,.AppImage,.appimage,.gz,.tgz,.xz,.txz,.bz2,.tbz2,.tar",
    });
    input.addEventListener("change", () => {
      const files = Array.from(input.files ?? []);
      input.remove();
      if (files.length > 0) void this.uploadUpdate(app, files);
    });
    document.body.appendChild(input);
    input.click();
  }

  private async uploadUpdate(app: LinuxApp, files: File[]): Promise<void> {
    const { ctx, lc } = this.host;
    try {
      await ctx.api.post("/api/files/mkdir", { path: DROP_FOLDER });
    } catch {
      /* the folder is already there */
    }
    const uploads = new UploadManager(ctx, lc, {
      currentDir: () => DROP_FOLDER,
      onUploaded: (entry) => {
        void this.updateFrom(app, { kind: "path", path: entry.path });
      },
      onDrained: () => undefined,
      maxUploadBytes: () => ctx.store.get().maxUploadBytes,
    });
    document.body.appendChild(uploads.el);
    uploads.enqueue(files, DROP_FOLDER);
    ctx.toast.info(`Uploading the update for ${app.name}…`);
  }

  private openUpdateFromUrl(app: LinuxApp): void {
    const { ctx } = this.host;
    const input = h("input", { class: "input", type: "url", placeholder: "https://…/app.deb", ariaLabel: "Link to the update file" });
    const error = h("div", { class: "form-error", role: "alert" });
    modal({
      title: `Update ${app.name} from a URL`,
      body: h(
        "div",
        { class: "col" },
        h("p", { class: "muted small" }, "The server downloads the file, checks it and shows what would change before updating."),
        input,
        error,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Continue",
          variant: "primary",
          onClick: (close) => {
            const url = input.value.trim();
            if (!url.startsWith("https://")) {
              error.textContent = "Enter a link that starts with https://";
              return;
            }
            close();
            void this.updateFrom(app, { kind: "url", url });
          },
        },
      ],
    });
    void ctx;
  }

  private async updateFrom(app: LinuxApp, source: InstallSource): Promise<void> {
    const { ctx } = this.host;
    const body = source.kind === "path" ? { path: source.path } : { url: source.url };
    const result = await this.previewInstall(source);
    if ("error" in result) {
      if (result.error) ctx.toast.error(result.error);
      return;
    }
    if (!("unsupported" in result)) {
      const confirmed = await this.confirmPreview(result.preview);
      if (!confirmed.ok) return;
      if (confirmed.launcher) {
        try {
          const res = await ctx.api.post<{ jobId?: string }>(`/api/linux-apps/${encodeURIComponent(app.id)}/update`, {
            ...body,
            launcher: confirmed.launcher,
          });
          if (res?.jobId) this.follow(res.jobId);
          ctx.toast.info(`Updating ${app.name}. Open copies keep running until they are closed.`);
          await this.reload();
        } catch (error) {
          ctx.toast.error(errorMessage(error));
        }
        return;
      }
    }
    try {
      const res = await ctx.api.post<{ jobId?: string }>(`/api/linux-apps/${encodeURIComponent(app.id)}/update`, body);
      if (res?.jobId) this.follow(res.jobId);
      ctx.toast.info(`Updating ${app.name}. Open copies keep running until they are closed.`);
      await this.reload();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  private async endApplication(app: LinuxApp): Promise<void> {
    const { ctx } = this.host;
    const session = linuxNativeSession();
    const run = session?.apps.find((entry) => entry.appId === app.id);
    if (!run) return;
    try {
      await ctx.api.post(`/api/linux-apps/session/runs/${encodeURIComponent(run.runId)}/close`);
      ctx.toast.info(`${app.name} was closed`);
      await this.reload();
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
  }

  private async update(app: LinuxApp): Promise<void> {
    const { ctx } = this.host;
    try {
      const res = await ctx.api.post<{ jobId?: string }>(`/api/linux-apps/${encodeURIComponent(app.id)}/update`);
      if (res?.jobId) this.follow(res.jobId);
      ctx.toast.info(`Updating ${app.name}. Open copies keep running until they are closed.`);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    }
    await this.reload();
  }

  /**
   * Progress for a job this window started (events alone may arrive first).
   * Resolves with the outcome after reloading the app list; `onUpdate` lets a
   * dialog paint the same progress it stores for the tiles.
   */
  private follow(jobId: string, onUpdate?: (update: Progress) => void): Promise<JobOutcome> {
    if (!this.progress.has(jobId)) this.progress.set(jobId, { progress: 0, total: 0, message: "Queued" });
    this.paintProgress(jobId);
    onUpdate?.(this.progress.get(jobId) ?? { progress: 0, total: 0, message: "Queued" });
    return watchJob(this.host.ctx, this.host.lc, jobId, (u) => {
      const update: Progress = { progress: u.progress, total: u.total, message: u.message ?? "" };
      this.progress.set(jobId, update);
      this.paintProgress(jobId);
      onUpdate?.(update);
    }).then((outcome) => {
      this.watched.add(jobId);
      this.progress.delete(jobId);
      void this.reload();
      return outcome;
    });
  }

  private async check(app: LinuxApp): Promise<void> {
    const { ctx } = this.host;
    this.checking.add(app.id);
    await this.reload();
    try {
      const fresh = await ctx.api.post<LinuxApp>(`/api/linux-apps/${encodeURIComponent(app.id)}/check`);
      if (fresh.update.available) ctx.toast.info(`${fresh.name} ${latestLabel(fresh)} is available`);
      else if (fresh.update.error) ctx.toast.warning(`Could not check ${fresh.name}: ${fresh.update.error}`);
      else ctx.toast.success(`${fresh.name} is up to date`);
    } catch (error) {
      ctx.toast.error(errorMessage(error));
    } finally {
      this.checking.delete(app.id);
    }
    await this.reload();
  }

  private openLimits(app: LinuxApp): void {
    const { ctx } = this.host;
    const maxCpus = Math.max(1, linuxOverview()?.limits.maxCpus ?? app.cpus);
    const memory = h("select", { class: "input" });
    const memoryChoices = [...new Set([...MEMORY_CHOICES_MB, app.memoryMb])].sort((a, b) => a - b);
    for (const mb of memoryChoices) memory.appendChild(h("option", { value: String(mb), selected: mb === app.memoryMb }, gb(mb)));
    const cpus = h("select", { class: "input" });
    for (let n = 1; n <= maxCpus; n += 1) cpus.appendChild(h("option", { value: String(n), selected: n === Math.min(app.cpus, maxCpus) }, `${n} ${n === 1 ? "core" : "cores"}`));
    const error = h("div", { class: "form-error", role: "alert" });
    const body = h(
      "div",
      { class: "col" },
      h("p", { class: "muted small" }, `The most ${app.name} may use for each person who opens it. Changes apply the next time it opens.`),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Memory"), memory),
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Processor"), cpus),
      error,
    );
    modal({
      title: `${app.name}: memory and CPU`,
      body,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Save",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            try {
              await ctx.api.patch(`/api/linux-apps/${encodeURIComponent(app.id)}`, { memoryMb: Number(memory.value), cpus: Number(cpus.value) });
              close();
              ctx.toast.success("Saved");
              await this.reload();
            } catch (err) {
              error.textContent = errorMessage(err);
            }
          },
        },
      ],
    });
  }

  private openVisibility(app: LinuxApp): void {
    const { ctx } = this.host;
    const users = h("input", { type: "radio", name: "linux-app-visibility", value: "users", checked: app.visibility !== "admins" });
    const admins = h("input", { type: "radio", name: "linux-app-visibility", value: "admins", checked: app.visibility === "admins" });
    const error = h("div", { class: "form-error", role: "alert" });
    const body = h(
      "div",
      { class: "col" },
      h("p", { class: "muted small" }, `Who can see and open ${app.name}. Everyone who opens it still gets their own copy with separate settings, sign-ins and open windows. Installing, updating and removing the app stays with administrators.`),
      h("label", { class: "checkbox" }, users, h("span", {}, "Everyone who signs in")),
      h("label", { class: "checkbox" }, admins, h("span", {}, "Administrators only")),
      error,
    );
    modal({
      title: `${app.name}: who can use it`,
      body,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Save",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            try {
              await ctx.api.post(`/api/linux-apps/${encodeURIComponent(app.id)}/visibility`, {
                visibility: admins.checked ? "admins" : "users",
              });
              close();
              ctx.toast.success(admins.checked ? `${app.name} is now visible to administrators only` : `${app.name} is now visible to everyone who signs in`);
              await this.reload();
            } catch (err) {
              error.textContent = errorMessage(err);
            }
          },
        },
      ],
    });
  }

  private openUninstall(app: LinuxApp): void {
    const { ctx } = this.host;
    const removeData = h("input", { type: "checkbox" });
    const error = h("div", { class: "form-error", role: "alert" });
    const body = h(
      "div",
      { class: "col" },
      h("p", {}, `${app.name} disappears from every desktop and open copies close. Files in RAST home folders are not touched.`),
      h(
        "label",
        { class: "checkbox" },
        removeData,
        h("span", {}, "Also delete its settings and sign-ins for everyone"),
      ),
      error,
    );
    modal({
      title: `Uninstall ${app.name}?`,
      body,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Uninstall",
          variant: "danger",
          keepOpen: true,
          onClick: async (close) => {
            try {
              await ctx.api.del(`/api/linux-apps/${encodeURIComponent(app.id)}`, { removeData: removeData.checked });
              close();
              ctx.toast.success(`${app.name} was uninstalled`);
              await this.reload();
            } catch (err) {
              error.textContent = errorMessage(err);
            }
          },
        },
      ],
    });
  }
  /* ---------------------------------------------- native install details UI */

  private backendBadge(app: LinuxApp): HTMLElement | null {
    if (isNativeApp(app)) {
      return h(
        "span",
        { class: "native-badge is-native", title: "Installed directly on the server, no container" },
        iconSvg("package", 11),
        "Native",
      );
    }
    if (app.backend === "docker" && nativeAppsEnabled()) {
      return h("span", { class: "native-badge is-docker", title: "Runs in a Docker container" }, iconSvg("docker", 11), "Docker");
    }
    return null;
  }

  private nativeSection(app: LinuxApp): HTMLElement | null {
    const native = app.native;
    if (!native) return null;
    const rows: Array<[string, Node | string]> = [
      ["Install type", isNativeApp(app) ? "Native Linux app" : "Docker container"],
      ["Package already on the server", native.preexistingPackage ? "Yes (reused, not duplicated)" : "No (installed by RAST)"],
    ];
    if (native.installPath) rows.push(["Installed at", h("span", { class: "pkg-preview-mono" }, native.installPath)]);
    if (native.previousPath) rows.push(["Previous version", h("span", { class: "pkg-preview-mono" }, native.previousPath)]);
    if (native.architecture) rows.push(["Architecture", native.architecture]);
    if (native.packageVersion) rows.push(["Package version", native.packageVersion]);
    const launcher = launcherLabel(native.launcher);
    if (launcher) rows.push(["Starts", h("span", { class: "pkg-preview-mono" }, launcher)]);
    if (native.dependencies.length > 0) rows.push(["Dependencies", native.dependencies.join(", ")]);
    if (native.packagesAdded.length > 0) rows.push(["Packages added", native.packagesAdded.join(", ")]);
    if (native.installSource) {
      const source = native.installSource;
      const label =
        source.kind === "url" && source.url
          ? source.url
          : source.path ?? source.url ?? source.kind;
      rows.push(["Installed from", h("span", { class: "pkg-preview-mono" }, label)]);
    }
    const problems: HTMLElement[] = [];
    if (!native.compatibility.ok) {
      problems.push(
        banner(
          "error",
          "This app may not run on this server",
          native.compatibility.reasons.join(" · ") || "The server reported an incompatibility.",
        ),
      );
    }
    return h(
      "section",
      { class: "pkg-section" },
      h("h3", { class: "pkg-section-title" }, "Native install"),
      ...problems,
      h(
        "dl",
        { class: "pkg-info" },
        ...rows.map(([key, value]) => h("div", { class: "pkg-info-item" }, h("dt", {}, key), h("dd", {}, value))),
      ),
    );
  }

  private migrationSection(app: LinuxApp): HTMLElement | null {
    if (!nativeAppsEnabled()) return null;
    const migration = app.native?.migration ?? null;
    const docker = app.backend !== "native";
    if (!docker && (!migration || migration.state === "none")) return null;
    const section = (title: string, ...children: Array<Node | null>): HTMLElement =>
      h(
        "section",
        { class: "pkg-section" },
        h("h3", { class: "pkg-section-title" }, title),
        ...children.filter((child): child is Node => child !== null),
      );
    if (docker) {
      return section(
        "Migrate to a native install",
        banner(
          "info",
          "This app still runs in a Docker container",
          `${app.name} can be installed directly on the server so it starts faster, uses less memory and gets its own native windows. RAST backs up the container data first.`,
          h("div", { class: "pkg-hero-actions" }, button("Migrate to native", "secondary", () => this.confirmMigrate(app))),
        ),
      );
    }
    if (!migration) return null;
    switch (migration.state) {
      case "pending":
      case "running": {
        const jobId = this.progress.keys().next().value as string | undefined;
        return section(
          "Migration",
          banner(
            "info",
            "Migrating to a native install…",
            migration.error ?? "RAST is backing up the Docker data and installing the app on the server. This can take a few minutes.",
            jobId ? this.progressEl(app, "hero") : undefined,
          ),
        );
      }
      case "failed":
        return section(
          "Migration",
          banner(
            "error",
            "The migration did not finish",
            migration.error ?? "The Docker version is still available. Check the error and try again.",
            h(
              "div",
              { class: "pkg-hero-actions" },
              button("Try again", "primary", async () => this.confirmMigrate(app)),
              button("Roll back", "secondary", async () => this.rollbackMigration(app)),
            ),
          ),
        );
      case "rolled-back":
        return section(
          "Migration",
          banner(
            "warning",
            "Rolled back to the Docker version",
            migration.backup ? `The backup is still at ${migration.backup}.` : "The app runs in Docker again.",
            h(
              "div",
              { class: "pkg-hero-actions" },
              migration.backup ? button("Clean up backup", "secondary", async () => this.cleanupMigration(app, true)) : null,
              button("Migrate again", "primary", async () => this.confirmMigrate(app)),
            ),
          ),
        );
      case "done":
        return section(
          "Migration",
          banner(
            "success",
            "Migrated to a native install",
            migration.backup
              ? `RAST kept the old Docker data as a backup at ${migration.backup}. Roll back if something is missing.`
              : "This app now runs natively.",
            h(
              "div",
              { class: "pkg-hero-actions" },
              button("Roll back to Docker", "secondary", async () => this.rollbackMigration(app)),
              migration.backup ? button("Clean up backup", "secondary", async () => this.cleanupMigration(app, true)) : null,
            ),
          ),
        );
      default:
        return null;
    }
  }

  /** Banner shown above the Linux app list when Docker apps can be migrated. */
  migrationNotice(): HTMLElement | null {
    if (!this.enabled || !nativeAppsEnabled()) return null;
    const apps = linuxApps();
    const failed = apps.filter((app) => app.native?.migration?.state === "failed");
    if (failed.length > 0) {
      return banner(
        "error",
        "A native migration failed",
        `${failed.map((app) => app.name).join(", ")} ${failed.length === 1 ? "is" : "are"} still running the Docker version. Open the app to see the error and try again.`,
        h("div", { class: "pkg-hero-actions" }, button("Review", "secondary", async () => this.host.openPage(failed[0]!.id))),
      );
    }
    const docker = apps.filter((app) => app.backend !== "native");
    if (docker.length === 0) return null;
    return banner(
      "info",
      "Linux apps can run natively",
      `${docker.length} ${docker.length === 1 ? "app still runs" : "apps still run"} in a Docker container. Migrate to a native install for faster starts, less memory and real desktop windows.`,
      h(
        "div",
        { class: "pkg-hero-actions" },
        button("Migrate an app…", "secondary", async () => this.host.openPage(docker[0]!.id)),
        button("Review apps", "secondary", async () => this.host.openPage(docker[0]!.id)),
      ),
    );
  }

  private confirmMigrate(app: LinuxApp): void {
    const { ctx } = this.host;
    const error = h("div", { class: "form-error", role: "alert" });
    modal({
      title: `Migrate ${app.name} to a native install?`,
      body: h(
        "div",
        { class: "col" },
        h("p", {}, `${app.name} currently runs in a Docker container. Migrating installs it directly on the server and switches it to the native backend.`),
        h("p", { class: "muted small" }, "RAST backs up the container's settings first. Open windows are closed during the migration. You can roll back afterwards."),
        error,
      ),
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => undefined },
        {
          label: "Migrate",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            try {
              const res = await ctx.api.post<{ jobId?: string }>(`/api/linux-apps/${encodeURIComponent(app.id)}/migrate`);
              if (res?.jobId) this.follow(res.jobId);
              close();
              ctx.toast.info(`Migrating ${app.name} to a native install…`);
              await this.reload();
            } catch (err) {
              error.textContent = errorMessage(err);
            }
          },
        },
      ],
    });
  }

  private rollbackMigration(app: LinuxApp): Promise<void> {
    const { ctx } = this.host;
    return (async () => {
      try {
        await ctx.api.post(`/api/linux-apps/${encodeURIComponent(app.id)}/migration/rollback`);
        ctx.toast.success(`${app.name} was rolled back to the Docker version`);
        await this.reload();
      } catch (error) {
        ctx.toast.error(errorMessage(error));
      }
    })();
  }

  private cleanupMigration(app: LinuxApp, removeBackup: boolean): Promise<void> {
    const { ctx } = this.host;
    return (async () => {
      try {
        await ctx.api.post(`/api/linux-apps/${encodeURIComponent(app.id)}/migration/cleanup`, { removeBackup });
        ctx.toast.success(`Cleaned up the migration for ${app.name}`);
        await this.reload();
      } catch (error) {
        ctx.toast.error(errorMessage(error));
      }
    })();
  }

}

// ---------------------------------------------------------------- helpers

function tileKind(app: LinuxApp): string {
  if (app.status === "installing" || app.status === "updating") return "busy";
  if (app.status === "failed") return "failed";
  return app.update.available ? "update" : "installed";
}

function tileSubtitle(app: LinuxApp): string {
  if (app.status === "installing") return "Installing…";
  if (app.status === "updating") return "Updating…";
  if (app.status === "failed") return "Install failed";
  if (app.update.available) return `${latestLabel(app)} available`;
  return app.version ? `Version ${app.version}` : "Linux app";
}

function heroStatus(app: LinuxApp): { kind: string; text: string } {
  switch (app.status) {
    case "installing":
      return { kind: "available", text: "Installing…" };
    case "updating":
      return { kind: "update", text: "Updating…" };
    case "failed":
      return { kind: "unavailable", text: "Not installed" };
    default:
      return app.update.available ? { kind: "update", text: `Update available: ${latestLabel(app)}` } : { kind: "installed", text: "Installed" };
  }
}

function latestLabel(app: LinuxApp): string {
  const latest = app.update.latestVersion;
  if (!latest) return "—";
  return latest === NEWER_BUILD ? "a newer build" : latest;
}

function gb(mb: number): string {
  const value = mb / 1024;
  return `${Number.isInteger(value) ? value : value.toFixed(1)} GB`;
}

function paintBar(host: HTMLElement, progress: Progress | undefined): void {
  const fill = host.querySelector<HTMLElement>(".progress-bar");
  const label = host.querySelector<HTMLElement>(".pkg-progress-label");
  const fraction = progress && progress.total > 0 ? Math.min(1, progress.progress / progress.total) : 0;
  if (fill) fill.style.width = `${Math.max(6, Math.round(fraction * 100))}%`;
  if (label && progress?.message) label.textContent = progress.message;
}

function pill(label: string, variant: string, title: string, run: () => Promise<void> | void): HTMLButtonElement {
  const btn = h("button", { class: `pkg-pill ${variant}`, type: "button", title, ariaLabel: title }, label);
  btn.addEventListener("click", async (event) => {
    event.stopPropagation();
    btn.disabled = true;
    try {
      await run();
    } finally {
      if (btn.isConnected) btn.disabled = false;
    }
  });
  return btn;
}

function button(label: string, variant: "primary" | "secondary", run: () => Promise<void> | void): HTMLButtonElement {
  const btn = h("button", { class: `btn btn-sm btn-${variant}`, type: "button" }, label);
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    try {
      await run();
    } finally {
      if (btn.isConnected) btn.disabled = false;
    }
  });
  return btn;
}

function launcherLabel(launcher: NativeAppInfo["launcher"]): string {
  if (!launcher) return "";
  const argv = launcher.argv ?? [];
  if (argv.length > 0) return argv.join(" ");
  if (launcher.desktopEntry) return launcher.desktopEntry;
  return launcher.name ?? "";
}

function hasFileDrag(event: DragEvent): boolean {
  return Array.from(event.dataTransfer?.types ?? []).includes("Files");
}

/**
 * The app id of a finished install job's result (`{id, ...}`; some servers wrap
 * it as `{app: {id}}`). Used to find the exact row after a reload instead of
 * guessing from names, which matters when an app entry already exists.
 */
function appIdFromJobResult(result: unknown): string | null {
  if (result === null || typeof result !== "object") return null;
  const value = result as { id?: unknown; app?: unknown };
  if (typeof value.id === "string" && value.id !== "") return value.id;
  const app = value.app;
  if (app !== null && typeof app === "object" && typeof (app as { id?: unknown }).id === "string") {
    const id = (app as { id: string }).id;
    return id === "" ? null : id;
  }
  return null;
}
