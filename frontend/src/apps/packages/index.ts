// Package Center: verified runtimes (Python, PHP CLI, Go, Node.js, imports
// from GitHub) installed side by side, with a selectable RAST default,
// updates that keep the previous version for rollback, consumers that pin
// versions, and live job progress.

import type { AppContext, AppDefinition } from "../../core/app";
import { clear, h } from "../../core/dom";
import { debounceTask, getRealtime, type RealtimeStore } from "../../core/realtime";
import { createLiveIndicator, NoticeBar, type LiveIndicator } from "../../components/live";
import { formatBytes, formatDateTime, relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { Permissions } from "../../core/types";
import { confirmDialog, copyText, errorMessage, modal } from "../../core/ui";
import { AppLifecycle } from "../../components/lifecycle";
import { watchJob } from "../../components/jobs";
import { banner, emptyBlock, renderLoadFailure } from "../../components/states";
import { openGithubImport } from "./github";
import {
  filterPackages,
  groupByChannel,
  jobActionLabel,
  jobFraction,
  monogram,
  monogramColor,
  shortSha,
  stateLabel,
  suggestedVersion,
  systemSourceLabel,
  updateSummary,
  versionOptionLabel,
  type PackageFilter,
} from "./format";
import type {
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
  packageId: string;
  progress: number;
  total: number;
  message: string;
}

const VIEWS: Array<{ id: PackageFilter; label: string; icon: string; countKey: string }> = [
  { id: "all", label: "All Packages", icon: "package", countKey: "all" },
  { id: "installed", label: "Installed", icon: "check", countKey: "installed" },
  { id: "updates", label: "Updates", icon: "refresh", countKey: "updates" },
];

class PackageCenterApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly realtime: RealtimeStore;
  private readonly notice: NoticeBar;
  private readonly live: LiveIndicator;
  private readonly checkButton: HTMLButtonElement;
  private readonly githubButton: HTMLButtonElement;
  private readonly statusChip: HTMLElement;
  private readonly sidebar: HTMLElement;
  private readonly main: HTMLElement;
  private readonly sync: { run: () => void; cancel: () => void };

  private overview: Overview | null = null;
  private loadError: unknown = null;
  private view: PackageFilter = "all";
  private query = "";
  private selectedId: string | null = null;
  private detail: PackageDetail | null = null;
  private detailError: unknown = null;
  private checking = false;
  private readonly jobs = new Map<string, LiveJob>();
  private readonly busy = new Set<string>();
  private readOnly = false;
  private denied = false;
  private disposed = false;
  /** Selected version in the details picker (kept across live refreshes). */
  private pickerVersion = new Map<string, string>();
  private pickerDefault = new Map<string, boolean>();

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

    this.checkButton = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", title: "Fetch and verify the signed catalog now" },
      iconSvg("refresh", 14),
      document.createTextNode("Check updates"),
    );
    this.checkButton.addEventListener("click", () => void this.checkNow());
    this.githubButton = h(
      "button",
      { class: "btn btn-primary btn-sm", type: "button" },
      iconSvg("github", 14),
      document.createTextNode("Install from GitHub"),
    );
    this.githubButton.addEventListener("click", () => this.openImport());
    this.statusChip = h("span", { class: "pkg-status-chip", role: "status", ariaLive: "polite" });
    const toolbar = h(
      "div",
      { class: "app-toolbar pkg-toolbar", role: "toolbar", ariaLabel: "Package actions" },
      this.githubButton,
      this.checkButton,
      this.statusChip,
      h("div", { class: "app-toolbar-spacer" }),
      this.live.el,
    );
    this.sidebar = h("nav", { class: "app-sidebar pkg-sidebar", ariaLabel: "Package views" });
    this.main = h("div", { class: "app-main pkg-main" });
    ctx.root.classList.add("packages-app");
    ctx.root.append(toolbar, this.notice.el, h("div", { class: "app-split pkg-split" }, this.sidebar, this.main));

    this.subscribe();
    // Keep relative times and the stale indicator honest without a reload.
    this.lc.setInterval(() => this.sync.run(), 60_000);
  }

  async start(): Promise<void> {
    this.ctx.setTitle("Package Center");
    this.applyAccess();
    if (this.denied) return;
    this.render();
    await this.reload();
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
          h("div", { class: "app-access-title" }, "Package Center is not available"),
          h("div", { class: "app-access-detail" }, "Your account has no access to Package Center. Ask an administrator if you need it."),
        ),
      );
      this.checkButton.classList.add("hidden");
      this.githubButton.classList.add("hidden");
      return;
    }
    this.checkButton.classList.toggle("hidden", this.readOnly);
    this.githubButton.classList.toggle("hidden", this.readOnly);
    if (wasDenied) void this.reload();
  }

  // ---------------------------------------------------------------- realtime

  private subscribe(): void {
    this.lc.add(
      this.realtime.on<PackageEvent>("package.changed", (data) => {
        if (data?.jobId && data.packageId) {
          const live = this.jobs.get(data.jobId);
          if (!live) this.jobs.set(data.jobId, { packageId: data.packageId, progress: 0, total: 0, message: "Queued" });
        }
        if (data?.reason === "failed" && data.jobId && this.jobs.has(data.jobId)) this.jobs.delete(data.jobId);
        this.sync.run();
      }),
    );
    this.lc.add(
      this.realtime.on<PackageEvent>("package.catalog", (data) => {
        if (data?.reason === "checking") {
          this.checking = true;
          this.renderStatusChip();
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
        this.applyAccess();
        if (`${this.denied}|${this.readOnly}` !== before && !this.denied) this.render();
      }),
    );
  }

  private onJob(kind: string, data: JobEvent | null): void {
    if (this.disposed || !data?.id || !String(data.kind ?? "").startsWith("package")) return;
    const id = data.id;
    if (kind === "job.done" || kind === "job.failed" || kind === "job.cancelled") {
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
    if (active && (active.status === "running" || active.status === "queued")) {
      return { packageId: p.id, progress: active.progress, total: active.total, message: active.message || jobActionLabel(active) };
    }
    return null;
  }

  /** Update progress bars in place (no full re-render on every event). */
  private paintProgress(packageId: string): void {
    const job = [...this.jobs.values()].find((j) => j.packageId === packageId);
    const hosts = this.main.querySelectorAll<HTMLElement>(`[data-progress-for="${CSS.escape(packageId)}"]`);
    if (hosts.length === 0) {
      this.sync.run();
      return;
    }
    for (const host of hosts) {
      if (!job) {
        host.classList.add("hidden");
        continue;
      }
      host.classList.remove("hidden");
      const fill = host.querySelector<HTMLElement>(".progress-bar");
      const label = host.querySelector<HTMLElement>(".pkg-progress-label");
      const fraction = jobFraction(job.progress, job.total);
      if (fill) {
        fill.style.width = job.total > 0 ? `${(fraction * 100).toFixed(1)}%` : "100%";
        fill.classList.toggle("is-indeterminate", job.total <= 0);
      }
      if (label) label.textContent = job.total > 0 ? `${job.message} · ${Math.round(fraction * 100)}%` : job.message;
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

  // ----------------------------------------------------------------- render

  private render(): void {
    if (this.disposed || this.denied) return;
    this.renderStatusChip();
    this.renderSidebar();
    if (this.selectedId) this.renderDetail();
    else this.renderList();
  }

  private renderStatusChip(): void {
    clear(this.statusChip);
    const catalog = this.overview?.catalog;
    if (this.checking || catalog?.checking) {
      this.statusChip.className = "pkg-status-chip is-checking";
      this.statusChip.append(h("span", { class: "spinner pkg-spinner" }), "Checking the catalog…");
      this.checkButton.disabled = true;
      return;
    }
    this.checkButton.disabled = false;
    if (!catalog) return;
    const stale = catalog.stale;
    this.statusChip.className = `pkg-status-chip ${stale ? "is-stale" : "is-fresh"}`;
    this.statusChip.append(
      iconSvg(stale ? "warning" : "shield", 13),
      catalog.sequence == null
        ? "Catalog not loaded"
        : `${stale ? "Stale · " : ""}checked ${catalog.checkedAt ? relativeTime(catalog.checkedAt) : "never"}`,
    );
    this.statusChip.title = catalogTooltip(catalog);
  }

  private renderSidebar(): void {
    clear(this.sidebar);
    const counts = this.overview?.counts ?? {};
    const list = h("div", { class: "pkg-nav" });
    for (const v of VIEWS) {
      const active = !this.selectedId && this.view === v.id;
      const count = counts[v.countKey] ?? 0;
      const ready = v.id === "updates" ? counts.updatesReady ?? 0 : 0;
      const button = h(
        "button",
        { class: `nav-item pkg-nav-item${active ? " is-active" : ""}`, type: "button", ariaPressed: active },
        iconSvg(v.icon, 16),
        h("span", { class: "pkg-nav-label" }, v.label),
        h("span", { class: `pkg-count${ready > 0 ? " is-ready" : ""}`, title: ready > 0 ? `${ready} ready to install` : "" }, String(count)),
      );
      button.addEventListener("click", () => {
        this.view = v.id;
        this.selectedId = null;
        this.detail = null;
        this.render();
      });
      list.appendChild(button);
    }
    this.sidebar.appendChild(list);

    const catalog = this.overview?.catalog;
    if (catalog) this.sidebar.appendChild(this.catalogCard(catalog));
    const host = this.overview?.host;
    if (host) {
      this.sidebar.appendChild(
        h(
          "div",
          { class: "pkg-side-card" },
          h("div", { class: "pkg-side-title" }, "This server"),
          h("div", { class: "pkg-side-line mono" }, host.target),
          h(
            "div",
            { class: "pkg-side-line" },
            host.libc === "glibc" ? `glibc ${host.glibcVersion ?? "?"}` : host.libc,
          ),
          h("div", { class: "pkg-side-line" }, `RAST ${host.rastVersion}`),
          this.shellLine(),
        ),
      );
    }
  }

  /** Whether terminals pick up the Package Center defaults. */
  private shellLine(): HTMLElement | null {
    const shell = this.overview?.shell;
    if (!shell) return null;
    if (shell.active) {
      return h("div", { class: "pkg-side-line", title: `${shell.profile} puts ${shell.binDir} first on PATH` }, "Terminal PATH: on");
    }
    if (!shell.enabled) {
      return h("div", { class: "pkg-side-line faint", title: "packages_shell_path = false" }, "Terminal PATH: off");
    }
    return h(
      "div",
      { class: "pkg-side-line pkg-side-warn", title: `Writes ${shell.profile}` },
      "Terminal PATH not set up. Run ",
      h("code", {}, shell.syncCommand),
    );
  }

  private catalogCard(catalog: CatalogStatus): HTMLElement {
    const badge = catalog.expired
      ? h("span", { class: "badge error" }, "Expired")
      : catalog.stale
        ? h("span", { class: "badge warn" }, "Stale")
        : h("span", { class: "badge ok" }, "Verified");
    const card = h(
      "div",
      { class: "pkg-side-card" },
      h("div", { class: "pkg-side-title" }, "Catalog", badge),
      h("div", { class: "pkg-side-line" }, catalog.official ? "Official RAST catalog" : catalog.name || "Custom catalog"),
      catalog.keyId
        ? h("div", { class: "pkg-side-line", title: catalog.officialKey ? "Official signing key" : "Administrator-configured key" }, `Signed · ${catalog.keyId}`)
        : null,
      catalog.sequence != null ? h("div", { class: "pkg-side-line" }, `Sequence ${catalog.sequence}`) : null,
      h(
        "div",
        { class: "pkg-side-line" },
        catalog.checkedAt ? `Last verified ${relativeTime(catalog.checkedAt)}` : "Never verified",
      ),
      catalog.intervalMinutes > 0
        ? h(
            "div",
            { class: "pkg-side-line faint" },
            catalog.nextCheckAt ? `Next automatic check ${formatDateTime(catalog.nextCheckAt)}` : `Checks every ${formatInterval(catalog.intervalMinutes)}`,
          )
        : h("div", { class: "pkg-side-line faint" }, "Automatic checks are off"),
    );
    if (catalog.lastError) {
      card.appendChild(h("div", { class: "pkg-side-error", role: "alert" }, `Last check failed: ${catalog.lastError}`));
    }
    return card;
  }

  private renderList(): void {
    clear(this.main);
    if (this.loadError && !this.overview) {
      renderLoadFailure(this.main, this.loadError, { title: "Package Center could not load", retry: () => void this.reload() });
      return;
    }
    if (!this.overview) {
      this.main.appendChild(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), "Loading packages…"));
      return;
    }
    const content = h("div", { class: "app-content pkg-content" });
    if (this.readOnly) {
      content.appendChild(banner("info", "Read-only access", "You can review packages, versions and paths. Installing and changing versions requires an administrator."));
    }
    const catalog = this.overview.catalog;
    if (catalog.sequence == null) {
      content.appendChild(
        banner(
          catalog.lastError ? "error" : "info",
          catalog.lastError ? "The package catalog could not be verified" : "The package catalog has not been loaded yet",
          catalog.lastError ?? "Use Check updates to fetch and verify the signed catalog.",
        ),
      );
    } else if (catalog.expired) {
      content.appendChild(banner("warning", "The cached catalog has expired", "Installs still use the last verified catalog. Check for updates to refresh it."));
    }

    const search = h("input", {
      class: "input input-search pkg-search",
      type: "search",
      placeholder: "Search packages",
      value: this.query,
      ariaLabel: "Search packages",
    });
    search.addEventListener("input", () => {
      this.query = search.value;
      this.paintCards(grid);
    });
    const title = VIEWS.find((v) => v.id === this.view)?.label ?? "Packages";
    content.appendChild(h("div", { class: "pkg-list-head" }, h("h2", { class: "pkg-heading" }, title), search));
    const grid = h("div", { class: "pkg-grid" });
    content.appendChild(grid);
    this.paintCards(grid);
    this.main.appendChild(content);
  }

  private paintCards(grid: HTMLElement): void {
    clear(grid);
    const packages = filterPackages(this.overview?.packages ?? [], this.view, this.query);
    if (packages.length === 0) {
      const empty =
        this.view === "installed"
          ? emptyBlock("Nothing installed yet", "Install a runtime from All Packages or import one from GitHub.")
          : this.view === "updates"
            ? emptyBlock("Everything is up to date", "Installed packages have no newer verified version.")
            : emptyBlock(this.query ? "No matching packages" : "No packages", this.query ? "Try another search." : "Check for updates to load the catalog.");
      grid.appendChild(empty);
      return;
    }
    for (const p of packages) grid.appendChild(this.card(p));
  }

  private card(p: PackageView): HTMLElement {
    const def = p.installed.find((v) => v.isDefault);
    const others = p.installed.filter((v) => !v.isDefault).map((v) => v.version);
    const update = updateSummary(p);
    const next = suggestedVersion(p);
    const actions = h("div", { class: "pkg-card-actions" });
    if (!this.readOnly) {
      const first = p.updates[0];
      if (first) {
        actions.appendChild(this.actionButton(`Update to ${first.to}`, "primary", () => this.installVersion(p, first.to, "update", first.replacesDefault)));
      } else if (p.installed.length === 0 && !p.system && next && p.official) {
        actions.appendChild(this.actionButton(`Install ${next.version}`, "primary", () => this.installVersion(p, next.version, "install", true)));
      }
    }
    const details = this.actionButton("Details", "ghost", () => this.openDetail(p.id));
    actions.appendChild(details);

    const sys = p.system;
    const systemNote = sys ? h("span", { class: "muted", title: `${sys.path} · ${systemSourceLabel(sys)}` }, ` · system ${sys.version ?? "?"}`) : null;
    const installedLine = def
      ? h(
          "div",
          { class: "pkg-card-line" },
          h("span", { class: "pkg-card-key" }, "Installed"),
          h("strong", {}, def.version),
          h("span", { class: "badge info pkg-mini-badge" }, "default"),
          others.length ? h("span", { class: "muted" }, ` + ${others.join(", ")}`) : null,
          systemNote,
        )
      : p.installed.length
        ? h("div", { class: "pkg-card-line" }, h("span", { class: "pkg-card-key" }, "Installed"), p.installed.map((v) => v.version).join(", "), systemNote)
        : sys
          ? h(
              "div",
              { class: "pkg-card-line", title: `${sys.path} · ${systemSourceLabel(sys)}` },
              h("span", { class: "pkg-card-key" }, "Installed"),
              h("strong", {}, sys.version ?? "unknown version"),
              h("span", { class: "badge pkg-mini-badge" }, "system"),
              h("span", { class: "muted pkg-card-source" }, `${systemSourceLabel(sys)} · ${sys.path}`),
            )
          : h("div", { class: "pkg-card-line muted" }, h("span", { class: "pkg-card-key" }, "Installed"), "—");
    const latestLine = h(
      "div",
      { class: "pkg-card-line" },
      h("span", { class: "pkg-card-key" }, "Latest"),
      p.latest ? h("span", {}, p.latest.version) : h("span", { class: "muted" }, "—"),
    );
    const card = h(
      "article",
      { class: "pkg-card", dataset: { packageId: p.id } },
      h(
        "div",
        { class: "pkg-card-head" },
        this.monogramEl(p),
        h(
          "div",
          { class: "pkg-card-title" },
          h("div", { class: "pkg-name" }, p.name),
          h("div", { class: "pkg-publisher" }, p.publisher.name, " · ", p.packageType),
        ),
        sourceBadge(p),
      ),
      h("p", { class: "pkg-desc" }, p.description || "No description."),
      installedLine,
      latestLine,
      update.kind !== "none"
        ? h("div", { class: `pkg-update pkg-update-${update.kind}` }, iconSvg(update.kind === "ready" ? "download" : "clock", 13), update.text)
        : null,
      p.unavailableReason && p.installed.length === 0
        ? h("div", { class: "pkg-unavailable", title: p.unavailableReason }, iconSvg("info", 13), p.unavailableReason)
        : null,
      update.kind === "none" && (p.installed.length > 0 || sys) && p.latest
        ? h("div", { class: "pkg-update pkg-update-current" }, iconSvg("check", 13), upToDateText(p))
        : null,
      this.progressEl(p),
      actions,
    );
    card.addEventListener("dblclick", (event) => {
      if ((event.target as HTMLElement).closest("button")) return;
      this.openDetail(p.id);
    });
    return card;
  }

  private monogramEl(p: PackageView): HTMLElement {
    const el = h("div", { class: "pkg-monogram", ariaHidden: true }, monogram(p.name, p.id));
    el.style.setProperty("--pkg-color", monogramColor(p.id));
    return el;
  }

  private progressEl(p: PackageView): HTMLElement {
    const job = this.liveJobFor(p);
    const host = h(
      "div",
      { class: `pkg-progress${job ? "" : " hidden"}`, role: "status", ariaLive: "polite", dataset: { progressFor: p.id } },
      h("div", { class: "progress" }, h("div", { class: "progress-bar" })),
      h("div", { class: "pkg-progress-label small" }, job?.message ?? ""),
    );
    if (job) {
      const fill = host.querySelector<HTMLElement>(".progress-bar");
      if (fill) {
        fill.style.width = job.total > 0 ? `${(jobFraction(job.progress, job.total) * 100).toFixed(1)}%` : "100%";
        fill.classList.toggle("is-indeterminate", job.total <= 0);
      }
    }
    return host;
  }

  private actionButton(label: string, variant: "primary" | "ghost" | "danger", run: () => Promise<void> | void): HTMLButtonElement {
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
    this.render();
    void this.loadDetail(id).then(() => {
      if (this.selectedId === id) this.render();
    });
  }

  // ---------------------------------------------------------------- details

  private renderDetail(): void {
    clear(this.main);
    const back = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, "← All packages");
    back.addEventListener("click", () => {
      this.selectedId = null;
      this.detail = null;
      this.render();
    });
    const content = h("div", { class: "app-content pkg-content pkg-detail" }, h("div", { class: "pkg-detail-nav" }, back));
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
      this.detailHeader(p),
      this.progressEl(p),
      this.updateSection(p),
      this.systemSection(p),
      this.installedSection(p),
      this.versionPicker(p),
      this.consumersSection(p),
      this.historySection(detail.jobs),
    ];
    for (const section of sections) if (section) content.appendChild(section);
  }

  private detailHeader(p: PackageView): HTMLElement {
    const sourceLink = p.official
      ? h("span", {}, "Official RAST catalog")
      : h("a", { href: `https://github.com/${p.sourceRepo ?? ""}`, target: "_blank", rel: "noopener noreferrer" }, p.sourceRepo ?? "GitHub");
    return h(
      "section",
      { class: "pkg-detail-head" },
      h(
        "div",
        { class: "pkg-detail-title" },
        this.monogramEl(p),
        h(
          "div",
          {},
          h("h2", { class: "pkg-heading" }, p.name),
          h("div", { class: "pkg-publisher" }, `${p.publisher.name} (${p.publisher.id}) · ${p.packageType}`),
        ),
        sourceBadge(p),
      ),
      p.description ? h("p", { class: "pkg-desc pkg-desc-full" }, p.description) : null,
      h(
        "dl",
        { class: "kv pkg-kv" },
        h("dt", {}, "Source"),
        h("dd", {}, sourceLink),
        h("dt", {}, "Verification"),
        h("dd", {}, h("div", {}, h("strong", {}, p.verification.label)), h("div", { class: "small muted" }, p.verification.detail)),
        p.homepage ? h("dt", {}, "Homepage") : null,
        p.homepage ? h("dd", {}, h("a", { href: p.homepage, target: "_blank", rel: "noopener noreferrer" }, p.homepage)) : null,
        p.license ? h("dt", {}, "License") : null,
        p.license ? h("dd", {}, p.license) : null,
        h("dt", {}, "RAST default"),
        h(
          "dd",
          {},
          p.defaultVersion
            ? h("div", { class: "pkg-paths" }, h("div", {}, h("strong", {}, p.defaultVersion), h("span", { class: "muted" }, " · stable paths follow the default")), ...p.defaultExecutables.map((e) => pathRow(e, this.ctx)))
            : h("span", { class: "muted" }, p.system ? "None: the system version is used" : "None selected"),
        ),
        h("dt", {}, "Terminal"),
        h("dd", {}, this.terminalInfo(p)),
      ),
    );
  }

  private terminalInfo(p: PackageView): HTMLElement {
    const t = p.terminal;
    const shell = this.overview?.shell;
    const notOnPath =
      p.defaultVersion && shell && !shell.active
        ? h(
            "div",
            { class: "small muted" },
            shell.enabled
              ? `Package Center versions are not on the terminal PATH yet. Run ${shell.syncCommand}.`
              : "Package Center versions are not on the terminal PATH (packages_shell_path = false).",
          )
        : null;
    if (!t) {
      return h("div", {}, h("span", { class: "muted" }, "Not available in a terminal"), notOnPath);
    }
    return h(
      "div",
      {},
      h(
        "div",
        {},
        h("code", {}, t.command),
        " runs ",
        h("strong", {}, t.version ?? "an unknown version"),
        h("span", { class: "muted" }, t.source === "package-center" ? " · Package Center default" : " · system"),
      ),
      h("div", { class: "small muted mono pkg-break" }, t.path),
      notOnPath,
    );
  }

  private systemSection(p: PackageView): HTMLElement | null {
    const s = p.system;
    if (!s) return null;
    const section = h("section", { class: "pkg-section" }, h("h3", { class: "pkg-section-title" }, "On this server"));
    const actions = h("div", { class: "pkg-row-actions" });
    if (!this.readOnly && p.defaultVersion && s.onPath) {
      actions.appendChild(this.actionButton("Use the system version", "ghost", () => this.useSystem(p)));
    }
    const inTerminal = p.terminal?.source === "system";
    section.appendChild(
      h(
        "div",
        { class: `pkg-installed-row${inTerminal ? " is-default" : ""}` },
        h(
          "div",
          { class: "pkg-installed-head" },
          h("strong", { class: "pkg-version" }, s.version ?? "Unknown version"),
          h(
            "span",
            { class: "pkg-badges" },
            h("span", { class: "badge" }, "System"),
            inTerminal ? h("span", { class: "badge info" }, "Used by terminals") : null,
          ),
          h("div", { class: "grow" }),
          actions,
        ),
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
        h(
          "p",
          { class: "small muted" },
          "Installed outside Package Center, so RAST never changes or removes it. An update installs the new version next to it; services that use this one keep using it.",
        ),
      ),
    );
    return section;
  }

  private updateSection(p: PackageView): HTMLElement | null {
    if (p.updates.length === 0 && p.detected.length === 0 && !p.unavailableReason) return null;
    const section = h("section", { class: "pkg-section" });
    for (const u of p.updates) {
      const keeps = u.from;
      const sys = p.system;
      const detail = u.fromSystem && sys
        ? `Installs ${u.to} from the verified catalog next to the system's ${keeps} (${sys.path}, ${systemSourceLabel(sys)}). ${
            u.replacesDefault
              ? `After it passes its checks, it becomes the default and terminals run ${u.to}.`
              : "The RAST default does not change."
          } The system version is not modified: services that use it keep it, and Use the system version switches back.`
        : u.replacesDefault
          ? `Installs ${u.to} next to ${keeps}. After it passes its checks, ${u.to} becomes the RAST default; ${keeps} stays installed so you can switch back.`
          : `Installs ${u.to} next to ${keeps}. The RAST default does not change; ${keeps} stays installed.`;
      const action = this.readOnly
        ? undefined
        : this.actionButton(`Install ${u.to}`, "primary", () => this.installVersion(p, u.to, "update", u.replacesDefault));
      section.appendChild(banner("success", `Update ready to install: ${u.from}${u.fromSystem ? " (system)" : ""} → ${u.to}`, detail, action));
    }
    for (const d of p.detected) {
      const link = d.releaseUrl ? h("a", { href: d.releaseUrl, target: "_blank", rel: "noopener noreferrer", class: "banner-action" }, "View release") : undefined;
      section.appendChild(
        banner(
          d.state === "failed" ? "warning" : "info",
          d.source === "github" ? `New release ${d.version} published` : `New upstream version detected: ${d.version} (${stateLabel(d.state)})`,
          `${d.detail}${d.detectedAt ? ` Detected ${d.detectedAt.replace("T", " ").replace("Z", " UTC")}.` : ""}`,
          link,
        ),
      );
    }
    if (p.unavailableReason) section.appendChild(banner("warning", "Not available for this server", p.unavailableReason));
    return section;
  }

  private installedSection(p: PackageView): HTMLElement {
    const section = h("section", { class: "pkg-section" }, h("h3", { class: "pkg-section-title" }, "Installed versions"));
    if (p.installed.length === 0) {
      section.appendChild(h("p", { class: "muted small" }, p.system ? "No version is installed by Package Center." : "No version is installed."));
      return section;
    }
    const list = h("div", { class: "pkg-installed" });
    for (const v of p.installed) list.appendChild(this.installedRow(p, v));
    section.appendChild(list);
    return section;
  }

  private installedRow(p: PackageView, v: InstalledView): HTMLElement {
    const actions = h("div", { class: "pkg-row-actions" });
    if (!this.readOnly) {
      if (!v.isDefault) actions.appendChild(this.actionButton("Make default", "ghost", () => this.makeDefault(p, v.version)));
      actions.appendChild(this.actionButton("Remove", "danger", () => this.remove(p, v)));
    }
    const badges = h(
      "span",
      { class: "pkg-badges" },
      v.isDefault ? h("span", { class: "badge info" }, "RAST default") : null,
      v.status !== "ok" ? h("span", { class: "badge error", title: "The files were removed outside RAST" }, "Files missing") : null,
      v.pinnedBy.length ? h("span", { class: "badge warn", title: `Pinned by ${v.pinnedBy.join(", ")}` }, `Pinned by ${v.pinnedBy.length}`) : null,
    );
    const bundled = v.bundled.map((b) => `${b.name} ${b.version}`).join(", ");
    return h(
      "div",
      { class: `pkg-installed-row${v.isDefault ? " is-default" : ""}` },
      h(
        "div",
        { class: "pkg-installed-head" },
        h("strong", { class: "pkg-version" }, v.version),
        v.revision > 1 ? h("span", { class: "muted small" }, `rev ${v.revision}`) : null,
        badges,
        h("div", { class: "grow" }),
        actions,
      ),
      h("div", { class: "pkg-paths" }, h("div", { class: "pkg-path-root mono small", title: v.path }, v.path), ...v.executables.map((e) => pathRow(e, this.ctx))),
      h(
        "div",
        { class: "pkg-meta small muted" },
        `${v.target} · ${formatBytes(v.sizeBytes)} on disk · installed ${formatDateTime(v.installedAt)} by ${v.installedBy}`,
        bundled ? ` · bundled ${bundled}` : "",
        v.releaseTag ? ` · release ${v.releaseTag}` : "",
      ),
      v.checkOutput ? h("pre", { class: "code pkg-check-output" }, v.checkOutput) : null,
      v.pinnedBy.length ? h("div", { class: "small muted" }, `Pinned by: ${v.pinnedBy.join(", ")}`) : null,
    );
  }

  private versionPicker(p: PackageView): HTMLElement | null {
    if (!p.official || p.versions.length === 0) {
      if (!p.official) {
        return h(
          "section",
          { class: "pkg-section" },
          h("h3", { class: "pkg-section-title" }, "Versions"),
          h("p", { class: "small muted" }, "Imported packages are installed release by release. Use Install from GitHub with a newer release link to add another version."),
        );
      }
      return null;
    }
    const section = h("section", { class: "pkg-section" }, h("h3", { class: "pkg-section-title" }, "Install a version"));
    const select = h("select", { class: "select pkg-version-select", ariaLabel: "Version" });
    const remembered = this.pickerVersion.get(p.id);
    const fallback = suggestedVersion(p)?.version ?? p.versions[0]?.version ?? "";
    const chosen = p.versions.some((v) => v.version === remembered) ? remembered! : fallback;
    for (const group of groupByChannel(p.versions)) {
      const og = document.createElement("optgroup");
      og.label = group.label;
      for (const v of group.versions) {
        const option = h("option", { value: v.version, selected: v.version === chosen }, versionOptionLabel(v));
        og.appendChild(option);
      }
      select.appendChild(og);
    }
    const info = h("div", { class: "pkg-version-info" });
    const makeDefault = h("input", { type: "checkbox", class: "checkbox-input" });
    const hasDefault = Boolean(p.defaultVersion);
    makeDefault.checked = this.pickerDefault.get(p.id) ?? !hasDefault;
    makeDefault.addEventListener("change", () => this.pickerDefault.set(p.id, makeDefault.checked));
    const installBtn = this.actionButton("Install", "primary", () => {
      const v = p.versions.find((x) => x.version === select.value);
      if (!v) return;
      // The server labels the job (install, update or reinstall) by what it does.
      return this.installVersion(p, v.version, "install", makeDefault.checked);
    });
    const paint = () => {
      this.pickerVersion.set(p.id, select.value);
      const v = p.versions.find((x) => x.version === select.value);
      clear(info);
      if (!v) return;
      info.appendChild(versionInfo(v));
      const installed = v.installedRevision != null && v.installedRevision >= v.revision;
      installBtn.disabled = !v.compatible || installed || this.readOnly || this.liveJobFor(p) !== null;
      installBtn.textContent = installed ? "Installed" : v.installedRevision != null ? `Reinstall ${v.version} (rev ${v.revision})` : `Install ${v.version}`;
    };
    select.addEventListener("change", paint);
    paint();
    const controls = h(
      "div",
      { class: "pkg-picker-controls" },
      select,
      this.readOnly
        ? null
        : h(
            "label",
            { class: "check pkg-default-check" },
            makeDefault,
            h(
              "span",
              {},
              hasDefault
                ? `Make it the RAST default (now ${p.defaultVersion})`
                : p.system
                  ? `Make it the default (terminals now run the system ${p.system.version ?? p.system.command})`
                  : "Make it the RAST default",
            ),
          ),
      this.readOnly ? null : installBtn,
    );
    section.append(controls, info);
    if (p.unsupported.length) {
      section.appendChild(
        h(
          "details",
          { class: "pkg-unsupported" },
          h("summary", {}, `${p.unsupported.length} catalog version${p.unsupported.length === 1 ? "" : "s"} this RAST cannot use`),
          h("ul", {}, ...p.unsupported.map((u) => h("li", {}, h("strong", {}, u.version), ` — ${u.reason}`))),
        ),
      );
    }
    return section;
  }

  private consumersSection(p: PackageView): HTMLElement {
    const section = h(
      "section",
      { class: "pkg-section" },
      h("h3", { class: "pkg-section-title" }, "Consumers"),
      h(
        "p",
        { class: "small muted" },
        "Record what uses this runtime. A consumer that follows the default moves with it; a pinned consumer keeps its version and protects it from removal.",
      ),
    );
    if (p.consumers.length) {
      const list = h("div", { class: "pkg-consumers" });
      for (const c of p.consumers) list.appendChild(this.consumerRow(p, c));
      section.appendChild(list);
    }
    if (!this.readOnly && p.installed.length > 0) {
      const add = this.actionButton("Add consumer", "ghost", () => this.editConsumer(p, null));
      section.appendChild(add);
    }
    return section;
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
    const section = h("section", { class: "pkg-section" }, h("h3", { class: "pkg-section-title" }, "Job history"));
    if (jobs.length === 0) {
      section.appendChild(h("p", { class: "small muted" }, "No install or update jobs yet."));
      return section;
    }
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
    section.appendChild(h("div", { class: "pkg-table-wrap" }, table));
    return section;
  }

  // ---------------------------------------------------------------- actions

  private async checkNow(): Promise<void> {
    if (this.checking) return;
    this.checking = true;
    this.renderStatusChip();
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
    this.jobs.set(jobId, { packageId: p.id, progress: 0, total: 0, message: `Queued: ${action === "update" ? "update to" : "install"} ${version}` });
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
    if (this.disposed) return;
    if (outcome.status === "done") {
      this.ctx.toast.success(`${p.name} ${version} installed and verified${makeDefault ? " — now the RAST default" : ""}`);
    } else if (outcome.status === "failed") {
      this.ctx.toast.error(`${p.name} ${version} was not installed: ${outcome.error ?? "unknown error"}. Installed versions are unchanged.`);
    }
    this.sync.run();
  }

  private async makeDefault(p: PackageView, version: string): Promise<void> {
    const ok = await confirmDialog({
      title: `Make ${p.name} ${version} the default?`,
      message: `The stable paths under runtimes/${p.id}/current and the bin shims switch to ${version}. Consumers that follow the default switch too; pinned consumers keep their versions.${p.defaultVersion ? ` ${p.defaultVersion} stays installed.` : ""}`,
      confirmLabel: "Make default",
    });
    if (!ok) return;
    try {
      const res = await this.ctx.api.put<{ activation?: { conflicts?: string[] } }>(`/api/packages/${encodeURIComponent(p.id)}/default`, { version });
      const conflicts = res.activation?.conflicts ?? [];
      this.ctx.toast.success(`${p.name} ${version} is now the RAST default`);
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
      message: `Terminals run ${s.path} again. The Package Center shims and runtimes/${p.id}/current are removed; ${p.defaultVersion ?? "the installed versions"} stays installed and can be made the default again.`,
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
      version.appendChild(h("option", { value: "", selected: !existing || existing.followsDefault }, `Follow the RAST default${p.defaultVersion ? ` (${p.defaultVersion})` : ""}`));
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
        this.jobs.set(jobId, { packageId, progress: 0, total: 0, message: `Queued: install ${version}` });
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
          if (this.disposed) return;
          if (outcome.status === "done") this.ctx.toast.success(`${packageId} ${version} installed from GitHub`);
          else if (outcome.status === "failed") this.ctx.toast.error(`Import failed: ${outcome.error ?? "unknown error"}`);
          this.sync.run();
        });
      },
    });
  }
}

// ------------------------------------------------------------------ helpers

/** "Up to date", naming the release line when a newer line exists. */
function upToDateText(p: PackageView): string {
  const current = p.installed.find((v) => v.isDefault)?.channel ?? p.updates[0]?.channel ?? null;
  const line = current ? p.versions.find((v) => v.channel === current) : undefined;
  if (!line || !p.latest || p.latest.channel === line.channel) return "Up to date";
  return `Up to date in ${line.channelLabel || line.channel}`;
}

function sourceBadge(p: PackageView): HTMLElement {
  return p.official
    ? h("span", { class: "badge ok pkg-source", title: p.verification.detail }, iconSvg("shield", 12), "Official")
    : h("span", { class: "badge warn pkg-source", title: p.verification.detail }, iconSvg("github", 12), "Third-party");
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
  name: "Package Center",
  icon: "package",
  description: "Install and manage verified runtimes side by side",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1060, h: 700 },
  minSize: { w: 380, h: 340 },
  async mount(ctx: AppContext) {
    const app = new PackageCenterApp(ctx);
    await app.start();
  },
};
