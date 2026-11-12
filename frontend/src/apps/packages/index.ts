// App Center: verified runtimes (Python, PHP CLI, Go, Node.js, imports
// from GitHub) installed side by side. The list is a calm grid of tiles with
// one action each (Install, Update or Installed); a package's page has the
// details, with paths, checks, consumers and history folded away. Catalog,
// server and terminal information live in Settings.

import type { AppContext, AppDefinition } from "../../core/app";
import { clear, h } from "../../core/dom";
import { debounceTask, getRealtime, type RealtimeStore } from "../../core/realtime";
import { createLiveIndicator, NoticeBar, type LiveIndicator } from "../../components/live";
import { formatBytes, formatDateTime, relativeTime } from "../../core/format";
import { appIconSvg, iconSvg } from "../../core/icons";
import type { Permissions } from "../../core/types";
import { confirmDialog, contextMenu, copyText, errorMessage, modal, type ContextMenuEntry } from "../../core/ui";
import { AppLifecycle } from "../../components/lifecycle";
import { watchJob } from "../../components/jobs";
import { banner, emptyBlock, renderLoadFailure } from "../../components/states";
import { openGithubImport } from "./github";
import { LinuxApps } from "./linux";
import { onLinuxApps } from "../linuxapp/catalog";
import {
  filterApps,
  filterPackages,
  groupByChannel,
  jobActionLabel,
  jobCancellable,
  jobFraction,
  linkLabel,
  monogram,
  monogramColor,
  shortSha,
  sortForTiles,
  stateLabel,
  statusLine,
  suggestedVersion,
  systemSourceLabel,
  systemWideText,
  tileState,
  updateSummary,
  versionOptionLabel,
  type PackageFilter,
  type TileState,
} from "./format";
import type {
  AppPackage,
  ArtifactView,
  CatalogStatus,
  ConsumerView,
  ExecutablePath,
  InstalledView,
  JobEvent,
  JobHistory,
  Overview,
  PackageDetail,
  PackageEvent,
  PackageView,
  VersionChoice,
} from "./types";

interface LiveJob {
  jobId: string;
  packageId: string;
  progress: number;
  total: number;
  message: string;
}

const VIEWS: Array<{ id: PackageFilter; label: string; icon: string }> = [
  { id: "installed", label: "Installed", icon: "check" },
  { id: "all", label: "All Packages", icon: "package" },
  { id: "updates", label: "Updates", icon: "refresh" },
];

class PackageCenterApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly realtime: RealtimeStore;
  private readonly notice: NoticeBar;
  private readonly live: LiveIndicator;
  private readonly backButton: HTMLButtonElement;
  private readonly checkButton: HTMLButtonElement;
  private readonly search: HTMLInputElement;
  private readonly githubButton: HTMLButtonElement;
  private readonly settingsButton: HTMLButtonElement;
  private readonly linuxButton: HTMLButtonElement;
  private readonly linux: LinuxApps;
  private readonly sidebar: HTMLElement;
  private readonly main: HTMLElement;
  private readonly sync: { run: () => void; cancel: () => void };

  private overview: Overview | null = null;
  private loadError: unknown = null;
  /** Null until the first load: Installed when something is installed, else All Packages. */
  private view: PackageFilter | null = null;
  private query = "";
  private grid: HTMLElement | null = null;
  private appGrid: HTMLElement | null = null;
  private appSection: HTMLElement | null = null;
  private runtimeSection: HTMLElement | null = null;
  private selectedId: string | null = null;
  /** The RAST app whose page is open (instead of a package's). */
  private selectedApp: string | null = null;
  /** The Linux app whose page is open. */
  private selectedLinux: string | null = null;
  private linuxGrid: HTMLElement | null = null;
  private linuxSection: HTMLElement | null = null;
  /** Apps this account can open now (their tiles offer Open). */
  private openableSignature = "";
  private detail: PackageDetail | null = null;
  private detailError: unknown = null;
  private checking = false;
  private readonly jobs = new Map<string, LiveJob>();
  private readonly busy = new Set<string>();
  /** Jobs this window asked to cancel (the button shows "Cancelling…"). */
  private readonly cancelling = new Set<string>();
  private readOnly = false;
  private denied = false;
  private disposed = false;
  /** Folded sections the user opened on a package page (kept across live refreshes). */
  private readonly openSections = new Set<string>();
  /** Selected version in the version dialog. */
  private readonly pickerVersion = new Map<string, string>();
  private readonly pickerDefault = new Map<string, boolean>();

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.realtime = getRealtime(ctx.events);
    this.notice = new NoticeBar(this.lc);
    this.live = createLiveIndicator(this.realtime, this.lc);
    this.sync = debounceTask(() => void this.reload(), 200);
    this.lc.add(() => {
      this.disposed = true;
      this.sync.cancel();
    });

    this.backButton = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Back", ariaLabel: "Back" }, iconSvg("arrowLeft", 16));
    this.backButton.addEventListener("click", () => this.closeDetail());
    this.checkButton = h(
      "button",
      { class: "btn btn-ghost btn-icon btn-sm pkg-check-btn", type: "button", title: "Check for updates", ariaLabel: "Check for updates" },
      iconSvg("refresh", 16),
    );
    this.checkButton.addEventListener("click", () => void this.checkNow());
    this.search = h("input", { class: "input input-search pkg-search", type: "search", placeholder: "Search", ariaLabel: "Search packages" });
    this.search.addEventListener("input", () => {
      this.query = this.search.value;
      if (this.selectedId || this.selectedApp || this.selectedLinux) this.closeDetail();
      else this.paintGrid();
    });
    this.githubButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", title: "Install a package from a GitHub release" },
      iconSvg("github", 14),
      h("span", { class: "pkg-btn-label" }, "Install from GitHub"),
    );
    this.githubButton.addEventListener("click", () => this.openImport());
    this.settingsButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", title: "Catalog, server and terminal settings" },
      iconSvg("settings", 14),
      h("span", { class: "pkg-btn-label" }, "Settings"),
    );
    this.settingsButton.addEventListener("click", () => this.openSettings());
    this.linux = new LinuxApps({
      ctx,
      lc: this.lc,
      realtime: this.realtime,
      readOnly: () => this.readOnly,
      openPage: (id) => this.openLinuxPage(id),
    });
    this.linuxButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", title: "Install a .deb package, an AppImage or a .tar.gz archive" },
      iconSvg("plus", 14),
      h("span", { class: "pkg-btn-label" }, "Add app"),
    );
    this.linuxButton.addEventListener("click", () => this.linux.openAdd());
    const toolbar = h(
      "div",
      { class: "app-toolbar pkg-toolbar", role: "toolbar", ariaLabel: "App Center" },
      this.backButton,
      this.checkButton,
      this.search,
      this.linuxButton,
      this.githubButton,
      this.settingsButton,
      this.live.el,
    );
    this.sidebar = h("nav", { class: "app-sidebar pkg-sidebar", ariaLabel: "Package views" });
    this.main = h("div", { class: "app-main pkg-main" });
    ctx.root.classList.add("packages-app");
    ctx.root.append(toolbar, this.notice.el, h("div", { class: "app-split pkg-split" }, this.sidebar, this.main));
    this.linux.attachDrop(ctx.root);

    this.subscribe();
    // Keep relative times and the stale indicator honest without a reload.
    this.lc.setInterval(() => this.sync.run(), 60_000);
  }

  async start(): Promise<void> {
    this.ctx.setTitle("App Center");
    this.applyParams(this.ctx.params);
    this.openableSignature = this.openableApps();
    this.ctx.onReopen((params) => {
      this.applyParams(params);
      this.render();
    });
    this.applyAccess();
    if (this.denied) return;
    this.lc.add(onLinuxApps(() => this.render()));
    this.render();
    void this.linux.reload();
    await this.reload();
  }

  /**
   * `{ view }` opens a list, `{ app }` an app's page (Applications, the
   * launcher) and `{ linux }` a Linux app's page (File Station opens it after
   * starting an install).
   */
  private applyParams(params: unknown): void {
    const wanted = (params ?? {}) as { view?: unknown; app?: unknown; linux?: unknown };
    if (wanted.view === "installed" || wanted.view === "all" || wanted.view === "updates") {
      this.view = wanted.view;
      this.selectedId = null;
      this.selectedApp = null;
      this.selectedLinux = null;
      this.detail = null;
    }
    if (typeof wanted.app === "string" && wanted.app) {
      this.selectedApp = wanted.app;
      this.selectedId = null;
      this.selectedLinux = null;
      this.detail = null;
    }
    if (typeof wanted.linux === "string" && wanted.linux) {
      this.selectedLinux = wanted.linux;
      this.selectedApp = null;
      this.selectedId = null;
      this.detail = null;
    }
  }

  private openableApps(): string {
    return this.ctx.desktop
      .apps()
      .map((app) => app.id)
      .join(",");
  }

  private canOpen(appId: string): boolean {
    return this.ctx.desktop.apps().some((app) => app.id === appId);
  }

  // ------------------------------------------------------------------ access

  private grant(): { allowed: boolean; readOnly: boolean } {
    if (this.ctx.store.isAdmin()) return { allowed: true, readOnly: false };
    const permissions = (this.ctx.store.get() as unknown as { permissions?: Permissions | null }).permissions;
    const grant = permissions?.apps?.packages;
    if (grant && typeof grant.allowed === "boolean") {
      return { allowed: grant.allowed, readOnly: grant.allowed && Boolean(grant.readOnly) };
    }
    return { allowed: false, readOnly: true };
  }

  private applyAccess(): void {
    const grant = this.grant();
    const wasDenied = this.denied;
    this.denied = !grant.allowed;
    this.readOnly = grant.readOnly || Boolean(this.overview && !this.overview.canManage);
    if (this.denied) {
      clear(this.sidebar);
      clear(this.main);
      this.main.appendChild(
        h(
          "div",
          { class: "app-access-state", role: "alert" },
          h("div", { class: "app-access-title" }, "App Center is not available"),
          h("div", { class: "app-access-detail" }, "Your account has no access to App Center. Ask an administrator if you need it."),
        ),
      );
      for (const b of [this.checkButton, this.githubButton, this.settingsButton, this.backButton, this.linuxButton]) b.classList.add("hidden");
      this.search.classList.add("hidden");
      return;
    }
    this.checkButton.classList.toggle("hidden", this.readOnly);
    this.githubButton.classList.toggle("hidden", this.readOnly);
    this.linuxButton.classList.toggle("hidden", this.readOnly || !this.linux.enabled);
    for (const el of [this.settingsButton, this.backButton, this.search]) el.classList.remove("hidden");
    if (wasDenied) void this.reload();
  }

  // ---------------------------------------------------------------- realtime

  private subscribe(): void {
    this.lc.add(
      this.realtime.on<PackageEvent>("package.changed", (data) => {
        if (data?.jobId && data.packageId) {
          const live = this.jobs.get(data.jobId);
          if (!live) this.jobs.set(data.jobId, { jobId: data.jobId, packageId: data.packageId, progress: 0, total: 0, message: "Queued" });
        }
        if (data?.reason === "failed" && data.jobId && this.jobs.has(data.jobId)) this.jobs.delete(data.jobId);
        this.sync.run();
      }),
    );
    this.lc.add(
      this.realtime.on<PackageEvent>("package.catalog", (data) => {
        if (data?.reason === "checking") {
          this.checking = true;
          this.renderChrome();
          return;
        }
        this.checking = false;
        if (data?.reason === "checked" && data.changed) {
          this.notice.show("The verified catalog changed. The list is up to date.", { kind: "info" });
        }
        this.sync.run();
      }),
    );
    for (const kind of ["job.queued", "job.start", "job.progress", "job.done", "job.failed", "job.cancelled"]) {
      this.lc.add(this.realtime.on<JobEvent>(kind, (data) => this.onJob(kind, data)));
    }
    this.lc.add(this.realtime.onResync(() => this.sync.run()));
    this.lc.add(this.realtime.on("access.changed", () => {
      this.applyAccess();
      this.sync.run();
    }));
    this.lc.add(
      this.ctx.store.subscribe(() => {
        const before = `${this.denied}|${this.readOnly}`;
        const openable = this.openableApps();
        const openableChanged = openable !== this.openableSignature;
        this.openableSignature = openable;
        this.applyAccess();
        if ((`${this.denied}|${this.readOnly}` !== before || openableChanged) && !this.denied) this.render();
      }),
    );
  }

  private onJob(kind: string, data: JobEvent | null): void {
    if (this.disposed || !data?.id || !String(data.kind ?? "").startsWith("package")) return;
    const id = data.id;
    if (kind === "job.done" || kind === "job.failed" || kind === "job.cancelled") {
      const known = this.jobs.get(id);
      // installVersion reports its own jobs; this covers jobs started elsewhere.
      if (!this.busy.has(id) && this.cancelling.delete(id)) this.reportCancel(kind, known?.packageId ?? this.packageForJob(id));
      this.jobs.delete(id);
      this.sync.run();
      return;
    }
    const known = this.jobs.get(id);
    const packageId = known?.packageId ?? this.packageForJob(id);
    if (!packageId) {
      this.sync.run();
      return;
    }
    this.jobs.set(id, {
      jobId: id,
      packageId,
      progress: typeof data.progress === "number" ? data.progress : known?.progress ?? 0,
      total: typeof data.total === "number" ? data.total : known?.total ?? 0,
      message: typeof data.message === "string" && data.message ? data.message : known?.message ?? "Working…",
    });
    this.paintProgress(packageId);
  }

  private packageForJob(jobId: string): string | null {
    for (const p of this.overview?.packages ?? []) {
      if (p.activeJob?.jobId === jobId) return p.id;
    }
    return null;
  }

  private liveJobFor(p: PackageView): LiveJob | null {
    for (const job of this.jobs.values()) if (job.packageId === p.id) return job;
    const active = p.activeJob;
    if (active && (active.status === "running" || active.status === "pending" || active.status === "queued")) {
      return { jobId: active.jobId, packageId: p.id, progress: active.progress, total: active.total, message: active.message || jobActionLabel(active) };
    }
    return null;
  }

  /** Update progress bars in place (no full re-render on every event). */
  private paintProgress(packageId: string): void {
    const job = [...this.jobs.values()].find((j) => j.packageId === packageId);
    const id = CSS.escape(packageId);
    const hosts = this.main.querySelectorAll<HTMLElement>(`[data-progress-for="${id}"]`);
    if (hosts.length === 0) {
      this.sync.run();
      return;
    }
    // A running job replaces the package's action button.
    for (const action of this.main.querySelectorAll<HTMLElement>(`[data-action-for="${id}"]`)) {
      action.classList.toggle("hidden", Boolean(job));
    }
    for (const host of hosts) {
      if (!job) {
        host.classList.add("hidden");
        continue;
      }
      host.classList.remove("hidden");
      paintBar(host, job, this.cancelling.has(job.jobId));
    }
  }

  // ------------------------------------------------------------------- data

  private async reload(): Promise<void> {
    if (this.disposed || this.denied) return;
    try {
      this.overview = await this.ctx.api.get<Overview>("/api/packages");
      this.loadError = null;
      this.checking = this.overview.catalog.checking;
      // Forget live jobs the server no longer reports as active.
      const active = new Set(this.overview.packages.map((p) => p.activeJob?.jobId).filter(Boolean) as string[]);
      for (const id of [...this.jobs.keys()]) {
        if (!active.has(id) && !this.busy.has(id)) this.jobs.delete(id);
      }
    } catch (error) {
      if (this.disposed) return;
      this.loadError = error;
      const status = (error as { status?: number }).status;
      if (status === 403) {
        this.denied = true;
        this.applyAccess();
        return;
      }
    }
    if (this.selectedId) await this.loadDetail(this.selectedId);
    this.applyAccess();
    if (!this.denied) this.render();
  }

  private async loadDetail(id: string): Promise<void> {
    try {
      this.detail = await this.ctx.api.get<PackageDetail>(`/api/packages/${encodeURIComponent(id)}`);
      this.detailError = null;
    } catch (error) {
      this.detail = null;
      this.detailError = error;
    }
  }

  private currentView(): PackageFilter {
    if (this.view) return this.view;
    return (this.overview?.counts.installed ?? 0) > 0 ? "installed" : "all";
  }

  // ----------------------------------------------------------------- render

  private render(): void {
    if (this.disposed || this.denied) return;
    this.renderChrome();
    this.renderSidebar();
    if (this.selectedLinux) this.renderLinuxDetail();
    else if (this.selectedApp) this.renderAppDetail();
    else if (this.selectedId) this.renderDetail();
    else this.renderList();
  }

  /** Toolbar state: back only on a package page, refresh spins while checking. */
  private renderChrome(): void {
    this.backButton.disabled = !this.selectedId && !this.selectedApp && !this.selectedLinux;
    const checking = this.checking || Boolean(this.overview?.catalog.checking);
    this.checkButton.disabled = checking;
    this.checkButton.classList.toggle("is-checking", checking);
    this.checkButton.title = checking ? "Checking the catalog…" : "Check for updates";
    const status = this.sidebar.querySelector(".pkg-catalog-status");
    if (status) status.replaceWith(this.catalogStatus());
  }

  private renderSidebar(): void {
    clear(this.sidebar);
    const updates = (this.overview?.counts.updates ?? 0) + this.linux.updateCount();
    const list = h("div", { class: "pkg-nav" });
    for (const v of VIEWS) {
      const active = !this.selectedId && !this.selectedApp && !this.selectedLinux && this.currentView() === v.id;
      const count = v.id === "updates" && updates > 0 ? updates : 0;
      const button = h(
        "button",
        { class: `nav-item pkg-nav-item${active ? " is-active" : ""}`, type: "button", ariaPressed: active },
        iconSvg(v.icon, 16),
        h("span", { class: "pkg-nav-label" }, v.label),
        count > 0 ? h("span", { class: "pkg-count", title: `${count} with a newer version` }, String(count)) : null,
      );
      button.addEventListener("click", () => {
        this.view = v.id;
        this.selectedId = null;
        this.selectedApp = null;
        this.selectedLinux = null;
        this.detail = null;
        this.render();
      });
      list.appendChild(button);
    }
    this.sidebar.append(list, this.catalogStatus());
  }

  /** One line at the bottom of the sidebar; Settings has the details. */
  private catalogStatus(): HTMLElement {
    const c = this.overview?.catalog;
    let kind = "ok";
    let title = "Catalog verified";
    let detail = "";
    if (this.checking || c?.checking) {
      kind = "checking";
      title = "Checking for updates…";
    } else if (!c) {
      kind = "checking";
      title = "Loading…";
    } else if (c.sequence == null) {
      kind = "error";
      title = "Catalog not verified";
      detail = c.lastError ? "The last check failed" : "Not checked yet";
    } else if (c.expired) {
      kind = "error";
      title = "Catalog expired";
      detail = c.checkedAt ? `Checked ${relativeTime(c.checkedAt)}` : "";
    } else {
      if (c.stale || c.lastError) {
        kind = "warn";
        title = c.lastError ? "Last check failed" : "Catalog not checked recently";
      }
      detail = c.checkedAt ? `Checked ${relativeTime(c.checkedAt)}` : "";
    }
    const button = h(
      "button",
      { class: `pkg-catalog-status is-${kind}`, type: "button", title: c ? catalogTooltip(c) : "" },
      kind === "checking" ? h("span", { class: "spinner pkg-spinner" }) : iconSvg(kind === "ok" ? "shield" : "warning", 15),
      h("span", { class: "pkg-catalog-text" }, h("span", { class: "pkg-catalog-title" }, title), detail ? h("span", { class: "pkg-catalog-detail" }, detail) : null),
    );
    button.addEventListener("click", () => this.openSettings());
    return button;
  }

  private renderList(): void {
    clear(this.main);
    this.grid = null;
    this.appGrid = null;
    this.appSection = null;
    this.linuxGrid = null;
    this.linuxSection = null;
    this.runtimeSection = null;
    if (this.loadError && !this.overview) {
      renderLoadFailure(this.main, this.loadError, { title: "App Center could not load", retry: () => void this.reload() });
      return;
    }
    if (!this.overview) {
      this.main.appendChild(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), "Loading packages…"));
      return;
    }
    const content = h("div", { class: "app-content pkg-content" });
    for (const note of this.listNotices()) content.appendChild(note);
    const view = this.currentView();
    const title = VIEWS.find((v) => v.id === view)?.label ?? "Packages";
    content.appendChild(h("h2", { class: "pkg-heading" }, title));
    // Updates come with RAST for its own apps: the Updates view lists runtimes only.
    if (view !== "updates" && this.overview.apps?.length) {
      this.appGrid = h("div", { class: "pkg-grid" });
      this.appSection = h("section", { class: "pkg-group", ariaLabel: "Apps" }, h("h3", { class: "pkg-group-title" }, "Apps"), this.appGrid);
      content.appendChild(this.appSection);
    }
    // Linux apps (administrators): Updates lists only those with a newer version.
    if (this.linux.enabled) {
      this.linuxGrid = h("div", { class: "pkg-grid" });
      this.linuxSection = h("section", { class: "pkg-group", ariaLabel: "Linux apps" }, h("h3", { class: "pkg-group-title" }, "Linux apps"), this.linuxGrid);
      content.appendChild(this.linuxSection);
    }
    this.grid = h("div", { class: "pkg-grid" });
    this.runtimeSection = h(
      "section",
      { class: "pkg-group", ariaLabel: "Runtimes" },
      this.appSection || this.linuxSection ? h("h3", { class: "pkg-group-title" }, "Runtimes") : null,
      this.grid,
    );
    content.appendChild(this.runtimeSection);
    this.paintGrid();
    this.main.appendChild(content);
  }

  /** Only what needs attention: access, and a catalog that cannot offer anything. */
  private listNotices(): HTMLElement[] {
    const notes: HTMLElement[] = [];
    const overview = this.overview;
    if (!overview) return notes;
    if (this.readOnly) {
      notes.push(banner("info", "Read-only access", "Installing and changing versions requires an administrator."));
    }
    const catalog = overview.catalog;
    const checkAction = () => {
      if (this.readOnly) return undefined;
      const btn = h("button", { class: "btn btn-sm btn-secondary banner-action", type: "button" }, "Check for updates");
      btn.addEventListener("click", () => void this.checkNow());
      return btn;
    };
    if (catalog.sequence == null) {
      notes.push(
        banner(
          catalog.lastError ? "error" : "info",
          catalog.lastError ? "The package catalog could not be verified" : "The package catalog has not been loaded yet",
          catalog.lastError ?? "Check for updates to fetch and verify it.",
          checkAction(),
        ),
      );
    } else if (catalog.expired) {
      notes.push(banner("warning", "The catalog has expired", "Check for updates to refresh it.", checkAction()));
    } else if (overview.packages.some((p) => p.official) && overview.packages.every((p) => !p.official || p.versions.length === 0)) {
      notes.push(
        banner(
          "warning",
          "No versions to install yet",
          `This server has catalog sequence ${catalog.sequence}, which lists no versions. Check for updates to load the newest catalog.`,
          checkAction(),
        ),
      );
    }
    return notes;
  }

  private paintGrid(): void {
    const grid = this.grid;
    if (!grid) return;
    clear(grid);
    const view = this.currentView();
    const apps = this.appGrid ? filterApps(this.overview?.apps ?? [], view, this.query) : [];
    if (this.appGrid && this.appSection) {
      clear(this.appGrid);
      for (const app of apps) this.appGrid.appendChild(this.appTile(app));
      this.appSection.hidden = apps.length === 0;
    }
    const linuxApps = this.linux.filtered(view, this.query);
    if (this.linuxGrid && this.linuxSection) {
      clear(this.linuxGrid);
      for (const app of linuxApps) this.linuxGrid.appendChild(this.linux.tile(app));
      // An empty All Packages list offers the install as a tile.
      const offerAdd = view === "all" && !this.query && !this.readOnly && this.linux.dockerEnabled;
      if (offerAdd && linuxApps.length === 0) this.linuxGrid.appendChild(this.linux.addTile());
      this.linuxSection.hidden = linuxApps.length === 0 && !(offerAdd && linuxApps.length === 0);
      // Docker -> native migration notice: only when the server supports native apps.
      this.linuxSection.querySelector(".native-migration-notice")?.remove();
      const migrationNotice = this.linux.migrationNotice();
      if (migrationNotice && !this.linuxSection.hidden) {
        migrationNotice.classList.add("native-migration-notice");
        this.linuxSection.insertBefore(migrationNotice, this.linuxGrid);
      }
    }
    const packages = sortForTiles(filterPackages(this.overview?.packages ?? [], view, this.query));
    if (this.runtimeSection) {
      this.runtimeSection.hidden = packages.length === 0 && (apps.length > 0 || linuxApps.length > 0) && (Boolean(this.query) || view === "updates");
    }
    if (packages.length === 0) {
      grid.appendChild(
        this.query
          ? emptyBlock("No matching packages", "Try another search.")
          : view === "installed"
            ? emptyBlock(this.appSection ? "No runtimes installed yet" : "Nothing installed yet", "Install a runtime from All Packages.")
            : view === "updates"
              ? emptyBlock("Everything is up to date")
              : emptyBlock("No packages", "Check for updates to load the catalog."),
      );
      return;
    }
    for (const p of packages) grid.appendChild(this.tile(p));
  }

  private tile(p: PackageView): HTMLElement {
    const state = tileState(p);
    const open = h("button", { class: "pkg-tile-name", type: "button", title: `Open ${p.name}` }, p.name);
    open.addEventListener("click", (event) => {
      event.stopPropagation();
      this.openDetail(p.id);
    });
    const tile = h(
      "article",
      { class: `pkg-tile is-${state.action}`, dataset: { packageId: p.id } },
      this.iconEl(p, "tile"),
      h(
        "div",
        { class: "pkg-tile-body" },
        open,
        h("div", { class: "pkg-tile-sub", title: tileTitle(p, state) }, state.subtitle),
        h("div", { class: "pkg-tile-foot" }, this.tileAction(p, state), this.progressEl(p)),
      ),
    );
    tile.addEventListener("click", (event) => {
      if ((event.target as HTMLElement).closest("button, a")) return;
      this.openDetail(p.id);
    });
    return tile;
  }

  // ------------------------------------------------------------------ apps

  private appTile(app: AppPackage): HTMLElement {
    const open = h("button", { class: "pkg-tile-name", type: "button", title: `About ${app.name}` }, app.name);
    open.addEventListener("click", (event) => {
      event.stopPropagation();
      this.openAppPage(app.id);
    });
    const tile = h(
      "article",
      { class: `pkg-tile is-app is-${app.installed ? "installed" : "install"}`, dataset: { appId: app.id } },
      this.appIconEl(app, "tile"),
      h(
        "div",
        { class: "pkg-tile-body" },
        open,
        h("div", { class: "pkg-tile-sub" }, app.removable ? "Optional app" : "Built-in app"),
        h("div", { class: "pkg-tile-foot" }, this.appTileAction(app)),
      ),
    );
    tile.addEventListener("click", (event) => {
      if ((event.target as HTMLElement).closest("button, a")) return;
      this.openAppPage(app.id);
    });
    return tile;
  }

  /** Open when it is installed (and this account may open it), else Install. */
  private appTileAction(app: AppPackage): HTMLElement {
    if (app.installed) {
      if (app.id !== "packages" && this.canOpen(app.id)) {
        return this.pill("Open", "is-quiet", `Open ${app.name}`, () => this.ctx.openApp(app.id));
      }
      return h("span", { class: "pkg-pill is-static" }, "Installed");
    }
    if (this.readOnly) return h("span", { class: "pkg-pill is-static" }, "Not installed");
    return this.pill("Install", "is-accent", `Install ${app.name}`, () => this.setAppInstalled(app, true));
  }

  private appIconEl(app: AppPackage, size: "tile" | "hero"): HTMLElement {
    return h("div", { class: `pkg-icon pkg-icon-${size} is-app`, ariaHidden: true }, appIconSvg(app.id, "apps", size === "hero" ? 54 : 34));
  }

  private openAppPage(id: string): void {
    this.selectedApp = id;
    this.selectedId = null;
    this.detail = null;
    this.render();
    this.main.scrollTop = 0;
  }

  private renderAppDetail(): void {
    clear(this.main);
    this.grid = null;
    this.appGrid = null;
    const content = h("div", { class: "app-content pkg-content pkg-detail" });
    this.main.appendChild(content);
    if (!this.overview) {
      content.appendChild(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), "Loading…"));
      return;
    }
    const app = this.overview.apps?.find((candidate) => candidate.id === this.selectedApp);
    if (!app) {
      content.appendChild(emptyBlock("This app is not available", "Go back to the list of packages."));
      return;
    }
    const kind = app.removable ? "Optional" : "Built-in";
    content.append(
      h(
        "section",
        { class: "pkg-hero" },
        this.appIconEl(app, "hero"),
        h(
          "div",
          { class: "pkg-hero-body" },
          h("div", { class: "pkg-hero-publisher" }, iconSvg("shield", 13), `RAST · ${kind}`),
          h("h2", { class: "pkg-hero-name" }, app.name),
          h("div", { class: `pkg-hero-status is-${app.installed ? "installed" : "available"}` }, app.installed ? "Installed" : "Not installed"),
          this.appActions(app),
        ),
      ),
      h("p", { class: "pkg-about" }, app.description),
    );
    const items: Array<[string, string]> = [
      ["Developer", "RAST"],
      ["Version", this.ctx.store.get().version ? `RAST ${this.ctx.store.get().version}` : "Comes with RAST"],
      ["Type", app.removable ? "Optional: can be uninstalled" : "Built-in: always installed"],
      ["Updates", "Updated together with RAST"],
    ];
    if (app.removable) items.push(["When uninstalled", uninstallKeeps(app)]);
    content.appendChild(
      h(
        "section",
        { class: "pkg-section" },
        h("h3", { class: "pkg-section-title" }, "Other information"),
        h("dl", { class: "pkg-info" }, ...items.map(([k, v]) => h("div", { class: "pkg-info-item" }, h("dt", {}, k), h("dd", {}, v)))),
      ),
    );
  }

  private appActions(app: AppPackage): HTMLElement | null {
    const buttons: HTMLButtonElement[] = [];
    if (app.installed) {
      if (app.id !== "packages" && this.canOpen(app.id)) buttons.push(this.actionButton("Open", "primary", () => this.ctx.openApp(app.id)));
      if (app.removable && !this.readOnly) {
        const uninstall = this.actionButton("Uninstall", "secondary", () => this.setAppInstalled(app, false));
        uninstall.title = `Hide ${app.name} for everyone; its data stays`;
        buttons.push(uninstall);
      }
    } else if (!this.readOnly) {
      buttons.push(this.actionButton("Install", "primary", () => this.setAppInstalled(app, true)));
    }
    return buttons.length ? h("div", { class: "pkg-hero-actions" }, ...buttons) : null;
  }

  private async setAppInstalled(app: AppPackage, installed: boolean): Promise<void> {
    if (!installed) {
      const ok = await confirmDialog({
        title: `Uninstall ${app.name}?`,
        message: `${app.name} disappears from every desktop and menu, and its windows close for everyone. ${uninstallKeeps(app)} You can install it again at any time.`,
        confirmLabel: "Uninstall",
        danger: true,
      });
      if (!ok) return;
    }
    const id = encodeURIComponent(app.id);
    try {
      const res = installed
        ? await this.ctx.api.post<{ apps?: AppPackage[] }>(`/api/packages/apps/${id}/install`)
        : await this.ctx.api.del<{ apps?: AppPackage[] }>(`/api/packages/apps/${id}`, { confirm: app.id });
      if (this.overview && res?.apps) this.overview.apps = res.apps;
      this.ctx.toast.success(installed ? `${app.name} is installed` : `${app.name} was uninstalled`);
      // The desktop, menus and this window follow the new permissions.
      await this.ctx.refreshUser().catch(() => undefined);
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.render();
    this.sync.run();
  }

  private tileAction(p: PackageView, state: TileState): HTMLElement {
    let el: HTMLElement;
    const version = state.version;
    if ((state.action === "update" || state.action === "install") && version && !this.readOnly) {
      const update = state.action === "update";
      el = this.pill(update ? "Update" : "Install", update ? "is-strong" : "is-accent", `${update ? "Update" : "Install"} ${p.name} ${version}`, () =>
        this.installVersion(p, version, update ? "update" : "install", state.makeDefault ?? true),
      );
    } else {
      const label =
        state.action === "update" ? "Update available" : state.action === "installed" ? "Installed" : state.action === "unavailable" ? "Unavailable" : "Not installed";
      el = h("span", { class: "pkg-pill is-static", title: state.action === "unavailable" ? p.unavailableReason ?? "" : "" }, label);
    }
    el.dataset.actionFor = p.id;
    if (this.liveJobFor(p)) el.classList.add("hidden");
    return el;
  }

  private pill(label: string, variant: string, title: string, run: () => Promise<void> | void): HTMLButtonElement {
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

  private iconEl(p: PackageView, size: "tile" | "hero"): HTMLElement {
    const text = monogram(p.name, p.id);
    const el = h("div", { class: `pkg-icon pkg-icon-${size}`, ariaHidden: true, dataset: { len: String(text.length) } }, text);
    el.style.setProperty("--pkg-color", monogramColor(p.id));
    return el;
  }

  private progressEl(p: PackageView, size: "tile" | "hero" = "tile"): HTMLElement {
    const job = this.liveJobFor(p);
    const cancel = this.readOnly
      ? null
      : h(
          "button",
          { class: `btn btn-sm pkg-cancel ${size === "hero" ? "btn-secondary" : "btn-ghost btn-icon"}`, type: "button", title: "Cancel", ariaLabel: `Cancel ${p.name}` },
          iconSvg("close", 14),
          size === "hero" ? h("span", {}, "Cancel") : null,
        );
    cancel?.addEventListener("click", (event) => {
      event.stopPropagation();
      const live = this.liveJobFor(p);
      if (live) void this.cancelJob(p, live);
    });
    const host = h(
      "div",
      { class: `pkg-progress is-${size}${job ? "" : " hidden"}`, role: "status", ariaLive: "polite", dataset: { progressFor: p.id } },
      h("div", { class: "pkg-progress-row" }, h("div", { class: "progress" }, h("div", { class: "progress-bar" })), cancel),
      h("div", { class: "pkg-progress-label" }, ""),
    );
    if (job) paintBar(host, job, this.cancelling.has(job.jobId));
    return host;
  }

  private actionButton(label: string, variant: "primary" | "ghost" | "danger" | "secondary", run: () => Promise<void> | void): HTMLButtonElement {
    const btn = h("button", { class: `btn btn-sm btn-${variant}`, type: "button" }, label);
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

  private openDetail(id: string): void {
    this.selectedId = id;
    this.detail = null;
    this.detailError = null;
    this.openSections.clear();
    this.render();
    this.main.scrollTop = 0;
    void this.loadDetail(id).then(() => {
      if (this.selectedId === id) this.render();
    });
  }

  private openLinuxPage(id: string): void {
    this.selectedLinux = id;
    this.selectedId = null;
    this.selectedApp = null;
    this.detail = null;
    this.render();
    this.main.scrollTop = 0;
  }

  private renderLinuxDetail(): void {
    const scroll = this.main.scrollTop;
    clear(this.main);
    this.grid = null;
    this.appGrid = null;
    this.linuxGrid = null;
    const content = h("div", { class: "app-content pkg-content pkg-detail" }, ...this.linux.page(this.selectedLinux ?? ""));
    this.main.appendChild(content);
    this.main.scrollTop = scroll;
  }

  private closeDetail(): void {
    if (!this.selectedId && !this.selectedApp && !this.selectedLinux) return;
    this.selectedId = null;
    this.selectedApp = null;
    this.selectedLinux = null;
    this.detail = null;
    this.render();
  }

  // ---------------------------------------------------------------- details

  private renderDetail(): void {
    const scroll = this.main.scrollTop;
    clear(this.main);
    this.grid = null;
    const content = h("div", { class: "app-content pkg-content pkg-detail" });
    this.main.appendChild(content);
    if (this.detailError) {
      const host = h("div", {});
      content.appendChild(host);
      renderLoadFailure(host, this.detailError, { title: "The package could not load", retry: () => this.openDetail(this.selectedId ?? "") });
      return;
    }
    const detail = this.detail;
    if (!detail) {
      content.appendChild(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), "Loading package…"));
      return;
    }
    const p = detail.package;
    const sections: Array<HTMLElement | null> = [
      this.hero(p),
      p.description ? h("p", { class: "pkg-about" }, p.description) : null,
      this.infoSection(p),
      this.versionsSection(p),
      this.technicalSection(p),
      this.consumersSection(p),
      this.historySection(detail.jobs),
    ];
    for (const section of sections) if (section) content.appendChild(section);
    this.main.scrollTop = scroll;
  }

  private hero(p: PackageView): HTMLElement {
    const status = statusLine(p);
    const pending = updateSummary(p);
    const publisher = p.official
      ? h("div", { class: "pkg-hero-publisher", title: p.verification.detail }, iconSvg("shield", 13), `${p.publisher.name} · Official`)
      : h("div", { class: "pkg-hero-publisher", title: p.verification.detail }, iconSvg("github", 13), `${p.sourceRepo ?? p.publisher.name} · Third-party`);
    return h(
      "section",
      { class: "pkg-hero" },
      this.iconEl(p, "hero"),
      h(
        "div",
        { class: "pkg-hero-body" },
        publisher,
        h("h2", { class: "pkg-hero-name" }, p.name),
        h("div", { class: `pkg-hero-status is-${status.kind}` }, status.text),
        pending.kind === "detected" ? h("div", { class: "pkg-hero-note" }, iconSvg("clock", 13), pending.text) : null,
        p.unavailableReason && p.installed.length === 0 ? h("div", { class: "pkg-hero-note is-warn" }, p.unavailableReason) : null,
        this.heroActions(p),
        this.progressEl(p, "hero"),
      ),
    );
  }

  /** One main button (Update, Install or Up to date) and a menu for the rest. */
  private heroActions(p: PackageView): HTMLElement | null {
    if (this.readOnly) return null;
    const state = tileState(p);
    const version = state.version;
    let primary: HTMLButtonElement;
    if ((state.action === "update" || state.action === "install") && version) {
      const update = state.action === "update";
      primary = this.actionButton(update ? `Update to ${version}` : `Install ${version}`, "primary", () =>
        this.installVersion(p, version, update ? "update" : "install", state.makeDefault ?? true),
      );
    } else {
      primary = h("button", { class: "btn btn-sm btn-secondary", type: "button", disabled: true }, state.action === "installed" ? "Up to date" : "Unavailable");
    }
    const items = this.menuItems(p);
    const group = h("div", { class: "pkg-split-btn" }, primary);
    if (items.length) {
      const caret = h(
        "button",
        { class: `btn btn-sm ${primary.disabled ? "btn-secondary" : "btn-primary"} btn-icon pkg-caret`, type: "button", title: "More actions", ariaLabel: "More actions" },
        iconSvg("chevronDown", 14),
      );
      caret.addEventListener("click", () => {
        const rect = caret.getBoundingClientRect();
        contextMenu(rect.left, rect.bottom + 4, items, { ariaLabel: `${p.name} actions` });
      });
      group.appendChild(caret);
    }
    const uninstall =
      p.installed.length > 0
        ? this.actionButton("Uninstall", "secondary", () => this.uninstall(p))
        : null;
    if (uninstall) uninstall.title = `Remove every ${p.name} version installed by App Center`;
    const actions = h("div", { class: "pkg-hero-actions", dataset: { actionFor: p.id } }, group, uninstall);
    if (this.liveJobFor(p)) actions.classList.add("hidden");
    return actions;
  }

  private menuItems(p: PackageView): ContextMenuEntry[] {
    const items: ContextMenuEntry[] = [];
    if (p.official && p.versions.length > 0) {
      items.push({ label: "Install another version…", icon: "download", onClick: () => this.openVersionDialog(p) });
    }
    if (!p.official) {
      items.push({ label: "Install a newer release…", icon: "github", onClick: () => this.openImport() });
    }
    if (p.system && p.defaultVersion && p.system.onPath) {
      items.push({ label: `Use the system version (${p.system.version ?? p.system.command})`, icon: "restore", onClick: () => void this.useSystem(p) });
    }
    if (p.homepage) {
      const url = p.homepage;
      if (items.length) items.push("separator");
      items.push({ label: `Open ${linkLabel(url)}`, icon: "external", onClick: () => window.open(url, "_blank", "noopener,noreferrer") });
    }
    return items;
  }

  private infoSection(p: PackageView): HTMLElement {
    const items: Array<[string, Node | string]> = [];
    items.push(["Developer", p.publisher.url ? externalLink(p.publisher.url, p.publisher.name) : p.publisher.name]);
    items.push(["Installed version", installedText(p)]);
    items.push(["Newest version", p.latest?.version ?? "—"]);
    items.push(["In the terminal", this.terminalText(p)]);
    const everywhere = systemWideText(p, this.overview?.shell?.system);
    if (everywhere) {
      items.push([
        "sudo and services",
        h("span", {}, everywhere.text, everywhere.sub ? h("span", { class: "pkg-info-sub" }, everywhere.sub) : null),
      ]);
    }
    items.push(["Source", p.official ? "Official RAST catalog" : externalLink(`https://github.com/${p.sourceRepo ?? ""}`, p.sourceRepo ?? "GitHub")]);
    items.push(["Category", p.packageType === "runtime" ? "Runtime" : p.packageType]);
    if (p.license) items.push(["License", p.license]);
    if (p.homepage) items.push(["Homepage", externalLink(p.homepage, linkLabel(p.homepage))]);
    return h(
      "section",
      { class: "pkg-section" },
      h("h3", { class: "pkg-section-title" }, "Other information"),
      h("dl", { class: "pkg-info" }, ...items.map(([k, v]) => h("div", { class: "pkg-info-item" }, h("dt", {}, k), h("dd", {}, v)))),
    );
  }

  private terminalText(p: PackageView): HTMLElement {
    const t = p.terminal;
    const shell = this.overview?.shell;
    const offPath = p.defaultVersion && shell && !shell.active;
    const note = offPath
      ? h(
          "span",
          { class: "pkg-info-sub", title: shell.enabled ? `Run ${shell.syncCommand}` : "packages_shell_path = false" },
          "App Center versions are not on the terminal PATH",
        )
      : null;
    if (!t) return h("span", {}, "—", note);
    return h(
      "span",
      { title: t.path },
      h("code", {}, t.command),
      ` runs ${t.version ?? "an unknown version"}`,
      t.source === "system" ? " (system)" : "",
      note,
    );
  }

  private versionsSection(p: PackageView): HTMLElement {
    const section = h("section", { class: "pkg-section" }, h("h3", { class: "pkg-section-title" }, "Versions on this server"));
    const rows: HTMLElement[] = [];
    if (p.system) rows.push(this.systemRow(p));
    for (const v of p.installed) rows.push(this.installedRow(p, v));
    if (rows.length) section.appendChild(h("div", { class: "pkg-vlist" }, ...rows));
    else section.appendChild(h("p", { class: "pkg-empty-line" }, "No version is installed."));
    if (!this.readOnly && p.official && p.versions.length > 0) {
      const another = h("button", { class: "btn btn-ghost btn-sm pkg-another", type: "button" }, iconSvg("plus", 14), "Install another version");
      another.addEventListener("click", () => this.openVersionDialog(p));
      section.appendChild(another);
    }
    return section;
  }

  private systemRow(p: PackageView): HTMLElement {
    const s = p.system!;
    // The system copy is the default only while App Center has none.
    const isDefault = !p.defaultVersion;
    const actions = h("div", { class: "pkg-row-actions" });
    if (!this.readOnly && p.defaultVersion && s.onPath) {
      const use = this.actionButton("Make default", "ghost", () => this.useSystem(p));
      use.title = "Terminals run the system version again";
      actions.appendChild(use);
    }
    return h(
      "div",
      { class: "pkg-vrow" },
      h("span", { class: "pkg-vrow-version" }, s.version ?? "Unknown"),
      h(
        "span",
        { class: "pkg-vrow-info", title: s.path },
        h("span", { class: "badge" }, "System"),
        isDefault ? h("span", { class: "badge info" }, "Default") : null,
        h("span", { class: "pkg-vrow-meta" }, systemSourceLabel(s)),
      ),
      actions,
    );
  }

  private installedRow(p: PackageView, v: InstalledView): HTMLElement {
    const actions = h("div", { class: "pkg-row-actions" });
    if (!this.readOnly) {
      if (!v.isDefault) actions.appendChild(this.actionButton("Make default", "ghost", () => this.makeDefault(p, v.version)));
      // The server refuses pinned versions, and the default while others are installed.
      if (v.pinnedBy.length === 0 && (!v.isDefault || p.installed.length === 1)) {
        actions.appendChild(this.actionButton("Remove", "ghost", () => this.remove(p, v)));
      }
    }
    return h(
      "div",
      { class: "pkg-vrow" },
      h("span", { class: "pkg-vrow-version" }, v.version, v.revision > 1 ? h("span", { class: "pkg-vrow-meta" }, ` rev ${v.revision}`) : null),
      h(
        "span",
        { class: "pkg-vrow-info", title: v.path },
        v.isDefault ? h("span", { class: "badge info" }, "Default") : null,
        v.status !== "ok" ? h("span", { class: "badge error", title: "The files were removed outside RAST" }, "Files missing") : null,
        v.pinnedBy.length ? h("span", { class: "badge warn", title: `Pinned by ${v.pinnedBy.join(", ")}` }, "Pinned") : null,
        h("span", { class: "pkg-vrow-meta", title: formatDateTime(v.installedAt) }, `${formatBytes(v.sizeBytes)} · installed ${relativeTime(v.installedAt)}`),
      ),
      actions,
    );
  }

  /** A folded section that stays open across live refreshes. */
  private fold(key: string, title: string, count: number | null, ...body: Array<Node | null>): HTMLElement {
    const el = h(
      "details",
      { class: "pkg-more" },
      h("summary", {}, title, count != null ? h("span", { class: "pkg-summary-count" }, ` ${count}`) : null),
      h("div", { class: "pkg-more-body" }, ...body),
    );
    el.open = this.openSections.has(key);
    el.addEventListener("toggle", () => {
      if (el.open) this.openSections.add(key);
      else this.openSections.delete(key);
    });
    return el;
  }

  private technicalSection(p: PackageView): HTMLElement {
    const blocks: Array<HTMLElement | null> = [];
    blocks.push(
      techBlock("Verification", h("div", {}, h("strong", {}, p.verification.label)), h("div", { class: "small muted" }, p.verification.detail)),
    );
    if (p.defaultVersion && p.defaultExecutables.length) {
      blocks.push(
        techBlock(
          `Stable paths (follow the default, ${p.defaultVersion})`,
          h("div", { class: "pkg-paths" }, ...p.defaultExecutables.map((e) => pathRow(e, this.ctx))),
        ),
      );
    }
    for (const v of p.installed) {
      const bundled = v.bundled.map((b) => `${b.name} ${b.version}`).join(", ");
      blocks.push(
        techBlock(
          `${p.name} ${v.version}`,
          h("div", { class: "pkg-paths" }, h("div", { class: "pkg-path-root mono small", title: v.path }, v.path), ...v.executables.map((e) => pathRow(e, this.ctx))),
          h(
            "div",
            { class: "pkg-meta small muted" },
            `${v.target} · installed ${formatDateTime(v.installedAt)} by ${v.installedBy}`,
            bundled ? ` · bundled ${bundled}` : "",
            v.releaseTag ? ` · release ${v.releaseTag}` : "",
            v.pinnedBy.length ? ` · pinned by ${v.pinnedBy.join(", ")}` : "",
          ),
          v.checkOutput ? h("pre", { class: "code pkg-check-output" }, v.checkOutput) : null,
        ),
      );
    }
    const s = p.system;
    if (s) {
      blocks.push(
        techBlock(
          `System ${p.name} ${s.version ?? ""}`.trim(),
          h(
            "div",
            { class: "pkg-paths" },
            pathRow({ name: s.command, path: s.path }, this.ctx),
            s.realPath !== s.path ? h("div", { class: "pkg-path-root mono small", title: s.realPath }, `→ ${s.realPath}`) : null,
          ),
          h(
            "div",
            { class: "pkg-meta small muted" },
            `${systemSourceLabel(s)} · `,
            s.version ? `version read by running ${s.versionCommand}` : `the version could not be read (${s.versionCommand})`,
            s.onPath ? "" : " · not on the system PATH",
            s.detectedAt ? ` · checked ${relativeTime(s.detectedAt)}` : "",
          ),
          h("pre", { class: "code pkg-check-output" }, s.output),
          h("p", { class: "small muted" }, "Installed outside App Center: RAST never changes or removes it. Updates install next to it."),
        ),
      );
    }
    for (const d of p.detected) {
      blocks.push(
        techBlock(
          d.source === "github" ? `New release ${d.version}` : `Upstream ${d.version} (${stateLabel(d.state)})`,
          h("div", { class: "small muted" }, d.detail, d.detectedAt ? ` Detected ${d.detectedAt.replace("T", " ").replace("Z", " UTC")}.` : ""),
          d.releaseUrl ? externalLink(d.releaseUrl, "View release") : null,
        ),
      );
    }
    if (p.unsupported.length) {
      blocks.push(
        techBlock(
          `${p.unsupported.length} catalog version${p.unsupported.length === 1 ? "" : "s"} this RAST cannot use`,
          h("ul", { class: "pkg-unsupported" }, ...p.unsupported.map((u) => h("li", {}, h("strong", {}, u.version), ` — ${u.reason}`))),
        ),
      );
    }
    return this.fold("technical", "Paths and verification", null, ...blocks);
  }

  private consumersSection(p: PackageView): HTMLElement | null {
    if (p.installed.length === 0 && p.consumers.length === 0) return null;
    const body: Array<HTMLElement | null> = [
      h("p", { class: "small muted" }, "Record what uses this runtime. A consumer that follows the default moves with it; a pinned one keeps its version and protects it from removal."),
    ];
    if (p.consumers.length) {
      const list = h("div", { class: "pkg-consumers" });
      for (const c of p.consumers) list.appendChild(this.consumerRow(p, c));
      body.push(list);
    }
    if (!this.readOnly && p.installed.length > 0) {
      const add = this.actionButton("Add consumer", "ghost", () => this.editConsumer(p, null));
      body.push(h("div", {}, add));
    }
    return this.fold("consumers", "Consumers", p.consumers.length, ...body);
  }

  private consumerRow(p: PackageView, c: ConsumerView): HTMLElement {
    const actions = h("div", { class: "pkg-row-actions" });
    if (!this.readOnly) {
      actions.append(
        this.actionButton("Edit", "ghost", () => this.editConsumer(p, c)),
        this.actionButton("Delete", "ghost", () => this.deleteConsumer(c)),
      );
    }
    return h(
      "div",
      { class: "pkg-consumer" },
      h(
        "div",
        { class: "pkg-installed-head" },
        h("strong", {}, c.name),
        c.followsDefault
          ? h("span", { class: "badge info" }, `Follows default · ${c.effectiveVersion ?? "—"}`)
          : h("span", { class: "badge warn" }, `Pinned · ${c.version}`),
        h("div", { class: "grow" }),
        actions,
      ),
      c.description ? h("div", { class: "small muted" }, c.description) : null,
      h("div", { class: "pkg-paths" }, ...c.executables.map((e) => pathRow(e, this.ctx))),
    );
  }

  private historySection(jobs: JobHistory[]): HTMLElement {
    if (jobs.length === 0) return this.fold("history", "Activity", 0, h("p", { class: "small muted" }, "No install or update jobs yet."));
    const table = h(
      "table",
      { class: "table pkg-history" },
      h("thead", {}, h("tr", {}, h("th", {}, "Job"), h("th", {}, "Status"), h("th", {}, "By"), h("th", {}, "Started"), h("th", {}, "Detail"))),
    );
    const body = h("tbody", {});
    for (const j of jobs) {
      const kind = j.status === "done" ? "ok" : j.status === "failed" ? "error" : j.status === "cancelled" ? "warn" : "info";
      body.appendChild(
        h(
          "tr",
          {},
          h("td", {}, jobActionLabel(j)),
          h("td", {}, h("span", { class: `badge ${kind}` }, j.status)),
          h("td", {}, j.username),
          h("td", { title: formatDateTime(j.createdAt) }, relativeTime(j.createdAt)),
          h("td", { class: "pkg-history-detail" }, j.error ?? j.message ?? ""),
        ),
      );
    }
    table.appendChild(body);
    return this.fold("history", "Activity", jobs.length, h("div", { class: "pkg-table-wrap" }, table));
  }

  // ---------------------------------------------------------------- dialogs

  private openVersionDialog(p: PackageView): void {
    if (p.versions.length === 0) return;
    const select = h("select", { class: "select pkg-version-select", ariaLabel: "Version" });
    const remembered = this.pickerVersion.get(p.id);
    const fallback = suggestedVersion(p)?.version ?? p.versions[0]?.version ?? "";
    const chosen = p.versions.some((v) => v.version === remembered) ? remembered! : fallback;
    for (const group of groupByChannel(p.versions)) {
      const og = document.createElement("optgroup");
      og.label = group.label;
      for (const v of group.versions) og.appendChild(h("option", { value: v.version, selected: v.version === chosen }, versionOptionLabel(v)));
      select.appendChild(og);
    }
    const makeDefault = h("input", { type: "checkbox", class: "checkbox-input" });
    makeDefault.checked = this.pickerDefault.get(p.id) ?? !p.defaultVersion;
    makeDefault.addEventListener("change", () => this.pickerDefault.set(p.id, makeDefault.checked));
    const info = h("div", { class: "pkg-version-info" });
    const installBtn = h("button", { class: "btn btn-primary", type: "button" }, "Install");
    let close: () => void = () => undefined;
    installBtn.addEventListener("click", () => {
      const v = p.versions.find((x) => x.version === select.value);
      if (!v) return;
      close();
      // The server labels the job (install, update or reinstall) by what it does.
      void this.installVersion(p, v.version, "install", makeDefault.checked);
    });
    const paint = () => {
      this.pickerVersion.set(p.id, select.value);
      const v = p.versions.find((x) => x.version === select.value);
      clear(info);
      if (!v) return;
      info.appendChild(versionInfo(v));
      const installed = v.installedRevision != null && v.installedRevision >= v.revision;
      installBtn.disabled = !v.compatible || installed || this.liveJobFor(p) !== null;
      installBtn.textContent = installed ? "Installed" : v.installedRevision != null ? `Reinstall ${v.version} (rev ${v.revision})` : `Install ${v.version}`;
    };
    select.addEventListener("change", paint);
    paint();
    const defaultLabel = p.defaultVersion
      ? `Make it the default (now ${p.defaultVersion})`
      : p.system
        ? `Make it the default (terminals now run the system ${p.system.version ?? p.system.command})`
        : "Make it the default";
    const body = h(
      "div",
      { class: "col pkg-version-dialog" },
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Version"), select),
      h("label", { class: "check pkg-default-check" }, makeDefault, h("span", {}, defaultLabel)),
      h("div", { class: "pkg-dialog-actions" }, installBtn),
      info,
    );
    close = modal({
      title: `Install ${p.name}`,
      body,
      wide: true,
      actions: [{ label: "Close", variant: "ghost", onClick: (done) => done() }],
    });
  }

  private openSettings(): void {
    const overview = this.overview;
    if (!overview) return;
    const c = overview.catalog;
    const host = overview.host;
    const shell = overview.shell;
    const badge = c.sequence == null
      ? h("span", { class: "badge error" }, "Not verified")
      : c.expired
        ? h("span", { class: "badge error" }, "Expired")
        : c.stale
          ? h("span", { class: "badge warn" }, "Not checked recently")
          : h("span", { class: "badge ok" }, "Verified");
    const catalogRows: Array<[string, Node | string]> = [
      ["Source", c.official ? "Official RAST catalog" : c.name || c.url],
      ["Signing key", c.keyId ? `${c.keyId}${c.officialKey ? " (official)" : ""}` : "—"],
      ["Sequence", c.sequence != null ? `${c.sequence}${c.generatedAt ? ` · generated ${c.generatedAt.replace("T", " ").replace("Z", " UTC")}` : ""}` : "—"],
      ["Last verified", c.checkedAt ? `${relativeTime(c.checkedAt)} (${formatDateTime(c.checkedAt)})` : "Never"],
      [
        "Automatic checks",
        c.intervalMinutes > 0
          ? `Every ${formatInterval(c.intervalMinutes)}${c.nextCheckAt ? ` · next ${formatDateTime(c.nextCheckAt)}` : ""}`
          : "Off",
      ],
    ];
    if (c.lastError) catalogRows.push(["Last error", h("span", { class: "pkg-error-text" }, c.lastError)]);
    let close: () => void = () => undefined;
    const check = h("button", { class: "btn btn-secondary btn-sm", type: "button" }, iconSvg("refresh", 14), "Check for updates");
    check.disabled = this.checking || this.readOnly;
    check.addEventListener("click", () => {
      close();
      void this.checkNow();
    });
    const terminal = !shell
      ? "—"
      : shell.active
        ? h("span", {}, "On: login shells run the App Center defaults first", h("span", { class: "pkg-settings-sub mono" }, shell.profile))
        : !shell.enabled
          ? h("span", {}, "Off (packages_shell_path = false)")
          : h("span", {}, "Not set up yet. Run this once as root:", commandLine(shell.syncCommand, this.ctx));
    const links = shell?.system;
    const everywhere = !links
      ? "—"
      : !links.enabled
        ? h("span", {}, "Off: only login shells use App Center versions", h("span", { class: "pkg-settings-sub mono" }, "packages_system_wide = false"))
        : !links.watcher
          ? h("span", {}, "Not set up yet. Run this once as root:", commandLine(shell.syncCommand, this.ctx))
          : h(
              "span",
              {},
              links.linked.length ? `On: ${links.linked.join(", ")}` : "On: nothing linked yet",
              h("span", { class: "pkg-settings-sub mono" }, links.directory),
              links.displaced.length ? h("span", { class: "pkg-settings-sub" }, `Moved aside: ${links.displaced.join(", ")}`) : null,
            );
    const body = h(
      "div",
      { class: "pkg-settings" },
      h(
        "section",
        { class: "pkg-settings-section" },
        h("h3", { class: "pkg-settings-title" }, "Catalog", badge),
        kvList(catalogRows),
        this.readOnly ? null : h("div", {}, check),
      ),
      h(
        "section",
        { class: "pkg-settings-section" },
        h("h3", { class: "pkg-settings-title" }, "This server"),
        kvList([
          ["Platform", host.target],
          ["C library", host.libc === "glibc" ? `glibc ${host.glibcVersion ?? "?"}` : host.libc],
          ["RAST", host.rastVersion],
          ["Terminal PATH", terminal],
          ["sudo and services", everywhere],
        ]),
      ),
    );
    close = modal({ title: "App Center settings", body, actions: [{ label: "Close", variant: "ghost", onClick: (done) => done() }] });
  }

  // ---------------------------------------------------------------- actions

  private async checkNow(): Promise<void> {
    if (this.checking) return;
    this.checking = true;
    this.renderChrome();
    try {
      const res = await this.ctx.api.post<{ catalog: CatalogStatus }>("/api/packages/check");
      if (res.catalog.lastError) {
        this.ctx.toast.error(`Catalog check failed: ${res.catalog.lastError}`);
      } else {
        this.ctx.toast.success(`Catalog verified (sequence ${res.catalog.sequence ?? "?"})`);
      }
    } catch (error) {
      this.ctx.toast.error(`Catalog check failed: ${errorMessage(error)}`);
    } finally {
      this.checking = false;
      await this.reload();
    }
  }

  private async installVersion(p: PackageView, version: string, action: "install" | "update", makeDefault: boolean): Promise<void> {
    const path = action === "update" ? "update" : "install";
    let jobId = "";
    try {
      const res = await this.ctx.api.post<{ jobId: string }>(`/api/packages/${encodeURIComponent(p.id)}/${path}`, { version, makeDefault });
      jobId = res.jobId;
    } catch (error) {
      this.ctx.toast.error(`Cannot ${action} ${p.name} ${version}: ${errorMessage(error)}`);
      return;
    }
    this.busy.add(jobId);
    this.jobs.set(jobId, { jobId, packageId: p.id, progress: 0, total: 0, message: `Queued: ${action === "update" ? "update to" : "install"} ${version}` });
    this.paintProgress(p.id);
    this.ctx.toast.info(`${action === "update" ? "Updating" : "Installing"} ${p.name} ${version}…`);
    const outcome = await watchJob(this.ctx, this.lc, jobId, (u) => {
      const live = this.jobs.get(jobId);
      if (live) {
        live.progress = u.progress;
        live.total = u.total;
        if (u.message) live.message = u.message;
        this.paintProgress(p.id);
      }
    });
    this.busy.delete(jobId);
    this.jobs.delete(jobId);
    const askedToCancel = this.cancelling.delete(jobId);
    if (this.disposed) return;
    if (outcome.status === "done") {
      this.ctx.toast.success(
        `${askedToCancel ? "Too late to cancel: " : ""}${p.name} ${version} installed and verified${makeDefault ? " — now the default" : ""}`,
      );
    } else if (outcome.status === "failed") {
      this.ctx.toast.error(`${p.name} ${version} was not installed: ${outcome.error ?? "unknown error"}. Installed versions are unchanged.`);
    } else if (askedToCancel) {
      this.ctx.toast.info(`${p.name} ${version} cancelled. Nothing was changed.`);
    }
    this.sync.run();
  }

  private async cancelJob(p: PackageView, job: LiveJob): Promise<void> {
    if (!job.jobId || this.cancelling.has(job.jobId)) return;
    this.cancelling.add(job.jobId);
    this.paintProgress(p.id);
    try {
      const res = await this.ctx.api.post<{ cancelRequested: boolean }>(`/api/jobs/${encodeURIComponent(job.jobId)}/cancel`);
      if (!res.cancelRequested) {
        this.cancelling.delete(job.jobId);
        this.ctx.toast.info(`${p.name}: the job already finished.`);
        this.sync.run();
      }
    } catch (error) {
      this.cancelling.delete(job.jobId);
      this.paintProgress(p.id);
      this.ctx.toast.error(`Cannot cancel: ${errorMessage(error)}`);
    }
  }

  private reportCancel(kind: string, packageId: string | null): void {
    const name = this.overview?.packages.find((x) => x.id === packageId)?.name ?? "The package";
    if (kind === "job.cancelled") this.ctx.toast.info(`${name}: cancelled. Nothing was changed.`);
    else if (kind === "job.done") this.ctx.toast.info(`${name}: too late to cancel, the install finished.`);
  }

  private async uninstall(p: PackageView): Promise<void> {
    const versions = p.installed.map((v) => v.version).join(", ");
    const size = p.installed.reduce((sum, v) => sum + v.sizeBytes, 0);
    const s = p.system;
    const after = s
      ? `The system ${p.name} ${s.version ?? ""} (${systemSourceLabel({ ...s, displaced: false })}) is used again.`.replace(/ +\(/, " (")
      : `${p.name} will no longer be available on this server.`;
    const ok = await confirmDialog({
      title: `Uninstall ${p.name}?`,
      message: `This removes ${versions} (${formatBytes(size)}) and its commands from App Center. ${after} You can install it again later.`,
      confirmLabel: "Uninstall",
      danger: true,
      requireTyped: p.id,
    });
    if (!ok) return;
    try {
      await this.ctx.api.del(`/api/packages/${encodeURIComponent(p.id)}`, { confirm: p.id });
      this.ctx.toast.success(`${p.name} uninstalled`);
      // An imported package is gone entirely; an official one can be installed again.
      if (!p.official && this.selectedId === p.id) {
        this.selectedId = null;
        this.detail = null;
        this.render();
      }
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.sync.run();
  }

  private async makeDefault(p: PackageView, version: string): Promise<void> {
    const ok = await confirmDialog({
      title: `Make ${p.name} ${version} the default?`,
      message: `Terminals and the stable paths under runtimes/${p.id}/current switch to ${version}. Consumers that follow the default switch too; pinned consumers keep their versions.${p.defaultVersion ? ` ${p.defaultVersion} stays installed.` : ""}`,
      confirmLabel: "Make default",
    });
    if (!ok) return;
    try {
      const res = await this.ctx.api.put<{ activation?: { conflicts?: string[] } }>(`/api/packages/${encodeURIComponent(p.id)}/default`, { version });
      const conflicts = res.activation?.conflicts ?? [];
      this.ctx.toast.success(`${p.name} ${version} is now the default`);
      if (conflicts.length) {
        this.ctx.toast.warning(`Shims not created for ${conflicts.join(", ")} (owned by another package).`);
      }
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.sync.run();
  }

  private async useSystem(p: PackageView): Promise<void> {
    const s = p.system;
    if (!s) return;
    const ok = await confirmDialog({
      title: `Use the system ${p.name} ${s.version ?? ""}?`.replace(/ \?$/, "?"),
      message: `Terminals run ${s.path} again. ${p.defaultVersion ?? "The installed version"} stays installed and can be made the default again.`,
      confirmLabel: "Use the system version",
    });
    if (!ok) return;
    try {
      await this.ctx.api.put(`/api/packages/${encodeURIComponent(p.id)}/default`, { system: true });
      this.ctx.toast.success(`Terminals use the system ${p.name} again`);
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.sync.run();
  }

  private async remove(p: PackageView, v: InstalledView): Promise<void> {
    const ok = await confirmDialog({
      title: `Remove ${p.name} ${v.version}?`,
      message: `This deletes ${v.path}. Pinned or default versions cannot be removed. This cannot be undone, but you can install the version again.`,
      confirmLabel: "Remove",
      danger: true,
      requireTyped: v.version,
    });
    if (!ok) return;
    try {
      await this.ctx.api.del(`/api/packages/${encodeURIComponent(p.id)}/versions/${encodeURIComponent(v.version)}`, { confirm: v.version });
      this.ctx.toast.success(`${p.name} ${v.version} removed`);
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.sync.run();
  }

  private editConsumer(p: PackageView, existing: ConsumerView | null): Promise<void> {
    return new Promise((resolve) => {
      const name = h("input", { class: "input", type: "text", value: existing?.name ?? "", placeholder: "e.g. billing-worker", dataset: { autofocus: "true" } });
      const description = h("input", { class: "input", type: "text", value: existing?.description ?? "", placeholder: "Optional" });
      const version = h("select", { class: "select" });
      version.appendChild(h("option", { value: "", selected: !existing || existing.followsDefault }, `Follow the default${p.defaultVersion ? ` (${p.defaultVersion})` : ""}`));
      for (const v of p.installed) {
        version.appendChild(h("option", { value: v.version, selected: existing?.version === v.version }, `Pin ${v.version}`));
      }
      const error = h("div", { class: "form-error hidden", role: "alert" });
      const body = h(
        "div",
        { class: "col pkg-consumer-form" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Name"), name),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Version"), version),
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Description"), description),
        error,
      );
      modal({
        title: existing ? `Edit ${existing.name}` : `Add a ${p.name} consumer`,
        body,
        onClose: () => resolve(),
        actions: [
          { label: "Cancel", variant: "ghost", onClick: (close) => close() },
          {
            label: existing ? "Save" : "Add",
            variant: "primary",
            keepOpen: true,
            onClick: async (close) => {
              const payload = { name: name.value, description: description.value, packageId: p.id, version: version.value || null };
              try {
                if (existing) await this.ctx.api.put(`/api/packages/consumers/${existing.id}`, payload);
                else await this.ctx.api.post("/api/packages/consumers", payload);
                close();
                this.sync.run();
              } catch (err) {
                error.textContent = errorMessage(err);
                error.classList.remove("hidden");
                throw err;
              }
            },
          },
        ],
      });
    });
  }

  private async deleteConsumer(c: ConsumerView): Promise<void> {
    const ok = await confirmDialog({
      title: `Delete consumer ${c.name}?`,
      message: c.followsDefault ? "It no longer follows the default." : `${c.version} will no longer be protected by this pin.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await this.ctx.api.del(`/api/packages/consumers/${c.id}`);
    } catch (error) {
      this.ctx.toast.error(errorMessage(error));
    }
    this.sync.run();
  }

  private openImport(): void {
    openGithubImport(this.ctx, {
      onStarted: (jobId, packageId, version) => {
        this.busy.add(jobId);
        this.jobs.set(jobId, { jobId, packageId, progress: 0, total: 0, message: `Queued: install ${version}` });
        this.view = "installed";
        this.selectedId = null;
        void this.reload();
        void watchJob(this.ctx, this.lc, jobId, (u) => {
          const live = this.jobs.get(jobId);
          if (live) {
            live.progress = u.progress;
            live.total = u.total;
            if (u.message) live.message = u.message;
            this.paintProgress(packageId);
          }
        }).then((outcome) => {
          this.busy.delete(jobId);
          this.jobs.delete(jobId);
          const askedToCancel = this.cancelling.delete(jobId);
          if (this.disposed) return;
          if (outcome.status === "done") this.ctx.toast.success(`${packageId} ${version} installed from GitHub`);
          else if (outcome.status === "failed") this.ctx.toast.error(`Import failed: ${outcome.error ?? "unknown error"}`);
          else if (askedToCancel) this.ctx.toast.info(`${packageId} ${version} cancelled. Nothing was changed.`);
          this.sync.run();
        });
      },
    });
  }
}

// ------------------------------------------------------------------ helpers

function paintBar(host: HTMLElement, job: LiveJob, cancelRequested = false): void {
  const fill = host.querySelector<HTMLElement>(".progress-bar");
  const label = host.querySelector<HTMLElement>(".pkg-progress-label");
  const cancel = host.querySelector<HTMLButtonElement>(".pkg-cancel");
  if (cancel) {
    cancel.hidden = !job.jobId || !jobCancellable(job.message);
    cancel.disabled = cancelRequested;
    const text = cancel.querySelector("span");
    if (text) text.textContent = cancelRequested ? "Cancelling…" : "Cancel";
  }
  const fraction = jobFraction(job.progress, job.total);
  if (fill) {
    fill.style.width = job.total > 0 ? `${(fraction * 100).toFixed(1)}%` : "100%";
    fill.classList.toggle("is-indeterminate", job.total <= 0);
  }
  if (label) {
    label.textContent = job.total > 0 ? `${Math.round(fraction * 100)}% · ${job.message}` : job.message;
    label.title = job.message;
  }
}

/** Hover text for a tile's version line. */
function tileTitle(p: PackageView, state: TileState): string {
  if (state.action === "update") {
    const u = p.updates[0]!;
    return `Update ready: ${u.from}${u.fromSystem ? " (system)" : ""} → ${u.to}`;
  }
  if (state.action === "installed" && p.installed.length === 0 && p.system) {
    return `${p.system.path} · ${systemSourceLabel(p.system)}`;
  }
  if (state.action === "install") return `Newest version ${state.version ?? ""}`.trim();
  return p.unavailableReason ?? "";
}

function installedText(p: PackageView): string {
  const def = p.installed.find((v) => v.isDefault);
  const others = p.installed.filter((v) => v !== def).map((v) => v.version);
  const parts: string[] = [];
  if (def) parts.push(def.version);
  parts.push(...others);
  if (p.system) parts.push(`${p.system.version ?? "unknown"} (system)`);
  return parts.length ? parts.join(", ") : "—";
}

function externalLink(url: string, label: string): HTMLAnchorElement {
  return h("a", { href: url, target: "_blank", rel: "noopener noreferrer", class: "pkg-break" }, label);
}

function techBlock(title: string, ...body: Array<Node | null>): HTMLElement {
  return h("div", { class: "pkg-tech-block" }, h("div", { class: "pkg-tech-title" }, title), ...body);
}

function kvList(rows: Array<[string, Node | string]>): HTMLElement {
  return h("dl", { class: "kv pkg-kv" }, ...rows.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
}

function commandLine(command: string, ctx: AppContext): HTMLElement {
  const copy = h("button", { class: "btn btn-icon btn-sm pkg-copy", type: "button", title: "Copy command", ariaLabel: "Copy command" }, iconSvg("copy", 13));
  copy.addEventListener("click", async () => {
    if (await copyText(command)) ctx.toast.success("Command copied");
  });
  return h("span", { class: "pkg-command" }, h("code", {}, command), copy);
}

function pathRow(e: ExecutablePath, ctx: AppContext): HTMLElement {
  const copy = h("button", { class: "btn btn-icon btn-sm pkg-copy", type: "button", title: `Copy path of ${e.name}`, ariaLabel: `Copy path of ${e.name}` }, iconSvg("copy", 13));
  copy.addEventListener("click", async () => {
    const ok = await copyText(e.path);
    if (ok) ctx.toast.success(`Copied ${e.path}`);
  });
  return h("div", { class: "pkg-path" }, h("span", { class: "pkg-path-name" }, e.name), h("code", { class: "pkg-path-value", title: e.path }, e.path), copy);
}

function artifactRows(a: ArtifactView): Node[] {
  const p = a.provenance;
  const rows: Node[] = [
    h("dt", {}, "Artifact"),
    h("dd", {}, h("a", { href: a.url, target: "_blank", rel: "noopener noreferrer", class: "pkg-break" }, a.url)),
    h("dt", {}, "Size · SHA-256"),
    h("dd", {}, `${formatBytes(a.size)} · `, h("code", { title: a.sha256 }, shortSha(a.sha256))),
    h("dt", {}, "Provenance"),
    h(
      "dd",
      {},
      p.kind === "built" ? "Built in CI from " : "Upstream release ",
      p.source ? h("a", { href: p.source, target: "_blank", rel: "noopener noreferrer", class: "pkg-break" }, p.source) : "—",
    ),
  ];
  if (p.verification) rows.push(h("dt", {}, "Verified by"), h("dd", {}, p.verification));
  if (p.buildUrl) rows.push(h("dt", {}, "Build"), h("dd", {}, h("a", { href: p.buildUrl, target: "_blank", rel: "noopener noreferrer" }, "Workflow run")));
  if (p.attestationUrl) rows.push(h("dt", {}, "Attestation"), h("dd", {}, h("a", { href: p.attestationUrl, target: "_blank", rel: "noopener noreferrer" }, "Build provenance")));
  if (a.requires.glibc) rows.push(h("dt", {}, "Requires"), h("dd", {}, `glibc ${a.requires.glibc} or newer`));
  if (a.requires.notes) rows.push(h("dt", {}, "Host notes"), h("dd", {}, a.requires.notes));
  return rows;
}

function versionInfo(v: VersionChoice): HTMLElement {
  const box = h("div", { class: "pkg-version-box" });
  if (!v.compatible) {
    box.appendChild(banner("warning", `${v.version} is not available for this server`, v.reason ?? "Incompatible."));
  }
  const kv = h(
    "dl",
    { class: "kv pkg-kv" },
    h("dt", {}, "Targets"),
    h("dd", {}, v.targets.join(", ")),
    v.releaseDate ? h("dt", {}, "Released") : null,
    v.releaseDate ? h("dd", {}, v.releaseDate) : null,
    v.bundled.length ? h("dt", {}, "Bundled") : null,
    v.bundled.length ? h("dd", {}, v.bundled.map((b) => `${b.name} ${b.version}`).join(", ")) : null,
    v.extensions.length ? h("dt", {}, "Extensions") : null,
    v.extensions.length ? h("dd", { class: "pkg-extensions" }, v.extensions.join(", ")) : null,
    v.executables.length ? h("dt", {}, "Executables") : null,
    v.executables.length ? h("dd", {}, v.executables.map((e) => e.path).join(", ")) : null,
    ...(v.artifact ? artifactRows(v.artifact) : []),
  );
  box.appendChild(kv);
  if (v.notes) box.appendChild(h("p", { class: "small muted" }, v.notes));
  return box;
}

function formatInterval(minutes: number): string {
  if (minutes % 1440 === 0) return `${minutes / 1440} day${minutes === 1440 ? "" : "s"}`;
  if (minutes % 60 === 0) return `${minutes / 60} hour${minutes === 60 ? "" : "s"}`;
  return `${minutes} minutes`;
}

/** What stays when an optional app is uninstalled. */
function uninstallKeeps(app: AppPackage): string {
  if (app.id === "applications") return "Running containers keep running, and their data stays.";
  if (app.id === "backup") return "Scheduled backups keep running, and existing backups stay.";
  return "Its data stays.";
}

function catalogTooltip(c: CatalogStatus): string {
  const lines = [c.url];
  if (c.keyId) lines.push(`Signed with ${c.keyId}${c.officialKey ? " (official key)" : ""}`);
  if (c.sequence != null) lines.push(`Sequence ${c.sequence}, generated ${c.generatedAt ?? "?"}`);
  if (c.expiresAt) lines.push(`Expires ${c.expiresAt}`);
  if (c.lastError) lines.push(`Last error: ${c.lastError}`);
  return lines.join("\n");
}

export const packagesApp: AppDefinition = {
  id: "packages",
  name: "App Center",
  icon: "package",
  description: "Install and manage apps and verified runtimes",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1060, h: 700 },
  minSize: { w: 380, h: 340 },
  async mount(ctx: AppContext) {
    const app = new PackageCenterApp(ctx);
    await app.start();
  },
};
