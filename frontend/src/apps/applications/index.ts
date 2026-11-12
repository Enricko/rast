// Docker (app id `applications`), laid out like Docker Desktop: every container
// on the server grouped by Compose project (RAST apps first), images and
// volumes, and a page per container with logs, configuration and usage. RAST
// apps keep their deploy dialog, detail window (logs, history) and delete flow.
// OWNER: teammate `frontend-apps`.

import type { AppDefinition, AppContext } from "../../core/app";
import type { ConnectionState } from "../../core/sse";
import { h, clear, field } from "../../core/dom";
import { debounceTask, getRealtime, type RealtimeStore } from "../../core/realtime";
import { createLiveIndicator, NoticeBar, type LiveIndicator } from "../../components/live";
import { formatBytes, formatPercent, formatDateTime, relativeTime, formatTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type {
  AppEnvVar,
  DockerStatus,
  ManagedApp,
  Permissions,
} from "../../core/types";
import { confirmDialog, contextMenu, copyText, errorMessage, modal, type ContextMenuEntry } from "../../core/ui";
import { AppLifecycle, isAbortError } from "../../components/lifecycle";
import { watchJob, jobProgressFraction, jobProgressLabel } from "../../components/jobs";
import {
  banner,
  emptyBlock,
  formError,
  isUnavailableError,
  progressBar,
  renderLoadFailure,
  unavailableReason,
} from "../../components/states";
import { mountContainerPage, type ContainerPage } from "./container-page";
import {
  containerStateLabel,
  containerTone,
  filterGroups,
  filterImages,
  filterVolumes,
  groupContainers,
  groupState,
  imageLabel,
  imageRefError,
  indexStats,
  isRunning,
  portLinks,
  shortId,
  usageFor,
  type ContainerGroup,
  type ContainerStat,
  type EngineContainer,
  type EngineImage,
  type EngineVolume,
  type PortMap,
  type UsageFilter,
} from "./model";

// ---------------------------------------------------------------------------
// API payload shapes (docs/API.md).
// ---------------------------------------------------------------------------

interface ContainerResourceStat {
  name?: string;
  cpuPercent?: number;
  memoryBytes?: number;
  memoryLimitBytes?: number;
}

interface PortMappingLike {
  hostPort?: number;
  containerPort?: number;
  protocol?: string;
  hostIp?: string | null;
}

/** Read-only viewers receive metadata only: env values/compose/logs are admin-only. */
interface ManagedAppView extends ManagedApp {
  envCount?: number;
  secretCount?: number;
}

interface AppDetail extends ManagedAppView {
  description?: string;
  /** The backend returns one resource sample per container. */
  resources?: ContainerResourceStat[];
  compose?: string;
}

interface DeployResponse {
  app?: ManagedApp;
  jobId?: string;
}

interface LogLine {
  ts: number;
  stream: string;
  message: string;
}

interface DeploymentRecord {
  id?: number | string;
  action: string;
  status: string;
  detail: string;
  createdAt: number;
  finishedAt?: number | null;
}

interface HistoryResponse {
  items?: DeploymentRecord[];
  history?: DeploymentRecord[];
}

interface ActionResponse {
  jobId?: string;
  app?: ManagedApp;
}

/** `app.changed` payload (docs/REALTIME.md). */
interface AppChangedEvent {
  appId?: string;
  status?: string;
  containers?: unknown[];
}

/** `docker.changed` payload (docs/REALTIME.md). */
interface DockerChangedEvent {
  enabled?: boolean;
  available?: boolean;
  version?: string | null;
  reason?: string | null;
}

/** `job.*` payload subset used for application jobs. */
interface AppJobEvent {
  id?: string;
  kind?: string;
  progress?: number;
  total?: number;
  message?: string;
  error?: string;
}

/** `app.logs` SSE event payload. */
interface AppLogsEvent {
  appId?: string;
  lines?: LogLine[];
  reset?: boolean;
}

interface LogStreamUi {
  pre: HTMLElement;
  status: HTMLElement;
  reconnect: HTMLButtonElement;
}

/** Local aliases for the shell-carried access policy (docs/GROUPS.md). */
type PermissionsState = Permissions;

interface PermissionsCarrier {
  permissions?: Permissions | null;
  user?: { id?: number; role?: string } | null;
}

type DetailTab = "overview" | "logs" | "history";
type AppAction = "start" | "stop" | "restart" | "redeploy";

type EngineView = "containers" | "images" | "volumes";

const TAIL_OPTIONS = [200, 500, 1000];

// ---------------------------------------------------------------------------
// Application wrapper
// ---------------------------------------------------------------------------

class ApplicationsApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly bannerHost: HTMLElement;
  private readonly statusBadge: HTMLElement;
  private readonly deployButton: HTMLButtonElement;
  private readonly pullButton: HTMLButtonElement;
  private readonly search: HTMLInputElement;
  private readonly sidebar: HTMLElement;
  private readonly main: HTMLElement;
  private readonly viewHost: HTMLElement;
  private readonly realtime: RealtimeStore;
  private readonly liveIndicator: LiveIndicator;
  private readonly notice: NoticeBar;
  private readonly syncApps: { run: () => void; cancel: () => void };
  private readonly syncContainers: { run: () => void; cancel: () => void };
  private readonly syncInventory: { run: () => void; cancel: () => void };
  private detailChangedHook: ((data: AppChangedEvent) => void) | null = null;
  private readonly accessCheck: { run: () => void; cancel: () => void };
  private localPermissions: PermissionsState | null = null;
  private readOnly = false;
  private denied = false;
  private lastAccessSignature = "";
  private lastPermissionsRef: unknown = undefined;
  private activeModalClose: (() => void) | null = null;
  private detailAccessHandler: ((readOnly: boolean) => void) | null = null;

  private view: EngineView = "containers";
  private query = "";
  private runningOnly = false;
  private imageFilter: UsageFilter = "all";
  private volumeFilter: UsageFilter = "all";
  private readonly expanded = new Set<string>();
  private page: ContainerPage | null = null;
  private busy = new Set<string>();

  private docker: DockerStatus | null = null;
  private dockerError: unknown = null;
  private apps: ManagedAppView[] = [];
  private appsLoading = true;
  private appsError: unknown = null;
  private containers: EngineContainer[] = [];
  private containersLoaded = false;
  private containersError: unknown = null;
  private stats = new Map<string, ContainerStat>();
  private images: EngineImage[] = [];
  private imagesLoaded = false;
  private imagesError: unknown = null;
  private imagesTotal = 0;
  private volumes: EngineVolume[] = [];
  private volumesLoaded = false;
  private volumesError: unknown = null;
  private disposed = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.realtime = getRealtime(ctx.events);
    this.notice = new NoticeBar(this.lc);
    this.liveIndicator = createLiveIndicator(this.realtime, this.lc);
    this.syncApps = debounceTask(() => void this.reloadAfterEvent(), 250);
    this.syncContainers = debounceTask(() => void this.loadContainers(), 400);
    this.syncInventory = debounceTask(() => void this.loadInventory(), 300);
    this.lc.add(() => {
      this.syncApps.cancel();
      this.syncContainers.cancel();
      this.syncInventory.cancel();
    });

    this.statusBadge = h("span", { class: "dk-engine muted small" }, "Checking Docker…");
    this.search = h("input", { class: "input input-search dk-search", type: "search", placeholder: "Search", ariaLabel: "Search" });
    this.search.addEventListener("input", () => {
      this.query = this.search.value;
      this.renderView();
    });
    this.deployButton = h(
      "button",
      { class: "btn btn-primary btn-sm", type: "button", disabled: true, title: "Deploy a Compose app" },
      iconSvg("plus", 14),
      h("span", { class: "dk-btn-label" }, "New app"),
    );
    this.deployButton.addEventListener("click", () => void this.openDeployDialog());
    this.pullButton = h(
      "button",
      { class: "btn btn-primary btn-sm", type: "button", title: "Download an image from Docker Hub or a registry" },
      iconSvg("download", 14),
      h("span", { class: "dk-btn-label" }, "Pull image"),
    );
    this.pullButton.addEventListener("click", () => this.openPullDialog());
    const refresh = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Refresh", ariaLabel: "Refresh" }, iconSvg("refresh", 16));
    refresh.addEventListener("click", () => void this.refreshAll(true));

    const toolbar = h(
      "div",
      { class: "app-toolbar dk-toolbar", role: "toolbar", ariaLabel: "Docker" },
      refresh,
      this.search,
      h("div", { class: "app-toolbar-spacer" }),
      this.liveIndicator.el,
      this.pullButton,
      this.deployButton,
    );

    this.bannerHost = h("div", { class: "appmgmt-banners" });
    this.viewHost = h("div", { class: "dk-view" });
    this.sidebar = h("nav", { class: "app-sidebar dk-sidebar", ariaLabel: "Docker views" });
    this.main = h("div", { class: "app-main dk-main" }, this.bannerHost, this.viewHost);
    ctx.root.classList.add("applications-app", "docker-app");
    ctx.root.append(toolbar, this.notice.el, h("div", { class: "app-split dk-split" }, this.sidebar, this.main));

    this.lc.add(() => {
      this.disposed = true;
      this.detailChangedHook = null;
      this.page?.dispose();
      this.page = null;
    });
    this.subscribeRealtime();
    this.accessCheck = debounceTask(() => void this.onAccessChanged(), 150);
    this.lastAccessSignature = `${this.ctx.store.get().user?.id ?? 0}|${this.ctx.store.get().user?.role ?? ""}`;
    this.lastPermissionsRef = this.ctx.store.get().permissions ?? null;
    this.lc.add(() => this.accessCheck.cancel());
    this.lc.add(
      this.ctx.store.subscribe(() => {
        const state = this.ctx.store.get() as unknown as {
          permissions?: unknown;
          user?: { id?: number; role?: string } | null;
        };
        const permissionsRef = state.permissions ?? null;
        const signature = `${state.user?.id ?? 0}|${state.user?.role ?? ""}`;
        if (permissionsRef === this.lastPermissionsRef && signature === this.lastAccessSignature) return;
        this.lastPermissionsRef = permissionsRef;
        this.lastAccessSignature = signature;
        this.accessCheck.run();
      }),
    );
    this.lc.add(this.realtime.on("access.changed", () => this.accessCheck.run()));
    // Containers change outside RAST too (crashes, other tools): keep the list
    // and the usage numbers fresh while the window is in view.
    this.lc.setInterval(() => {
      if (!this.visible()) return;
      if (this.view === "containers" && !this.page) {
        void this.loadContainers();
        void this.loadStats();
      }
    }, 8_000);
    this.applyAccessPolicy();
  }

  private visible(): boolean {
    return !this.disposed && !this.denied && !document.hidden && !this.ctx.window.isMinimized();
  }

  /** Only administrators can list and change the engine itself. */
  private engineAccess(): boolean {
    return this.ctx.store.isAdmin();
  }

  private canChange(): boolean {
    return !this.readOnly && !this.denied && this.dockerUsable();
  }

  // ----------------------------------------------------------- access policy

  private readGrant(): { allowed: boolean; readOnly: boolean } {
    const state = this.ctx.store.get() as unknown as PermissionsCarrier;
    const grant = state.permissions?.apps?.applications ?? this.localPermissions?.apps?.applications;
    if (grant && typeof grant.allowed === "boolean") {
      return { allowed: grant.allowed, readOnly: grant.allowed && Boolean(grant.readOnly) };
    }
    // Fallback to the pre-groups behaviour: Docker is admin-only.
    const admin = this.ctx.store.isAdmin();
    return { allowed: admin, readOnly: false };
  }

  private async onAccessChanged(): Promise<void> {
    if (this.disposed) return;
    const state = this.ctx.store.get() as unknown as PermissionsCarrier;
    if (!state.permissions) {
      // Shell not storing permissions yet: re-authorize ourselves defensively.
      try {
        const me = await this.ctx.api.get<{ permissions?: Permissions }>("/api/auth/me");
        this.localPermissions = me.permissions ?? null;
      } catch {
        this.localPermissions = null;
      }
    }
    if (this.disposed) return;
    this.applyAccessPolicy();
  }

  private applyAccessPolicy(): void {
    if (this.disposed) return;
    const grant = this.readGrant();
    const wasDenied = this.denied;
    this.denied = !grant.allowed;
    this.readOnly = grant.allowed && grant.readOnly;
    if (this.denied) {
      this.activeModalClose?.();
      this.closePage();
      this.deployButton.disabled = true;
      this.renderDenied();
      return;
    }
    if (!this.engineAccess() && this.view !== "containers") this.view = "containers";
    this.detailAccessHandler?.(this.readOnly);
    if (wasDenied) void this.refreshAll();
    this.render();
  }

  private renderDenied(): void {
    clear(this.bannerHost);
    clear(this.viewHost);
    clear(this.sidebar);
    this.bannerHost.appendChild(
      h(
        "div",
        { class: "app-access-state", role: "alert" },
        h("div", { class: "app-access-title" }, "Docker access changed"),
        h(
          "div",
          { class: "app-access-detail" },
          "Your access to Docker was revoked by a group policy change. Ask an administrator if you need access again.",
        ),
        (() => {
          const btn = h("button", { class: "btn btn-ghost", type: "button" }, "Close window");
          btn.addEventListener("click", () => this.ctx.window.close());
          return btn;
        })(),
      ),
    );
    this.statusBadge.textContent = "Access denied";
    this.deployButton.classList.add("hidden");
    this.pullButton.classList.add("hidden");
    this.notice.show("Your access to Docker was changed.", { kind: "warning" });
    this.ctx.toast.warning("Your access to Docker was changed.");
  }

  // -------------------------------------------------------------- realtime

  private subscribeRealtime(): void {
    this.lc.add(this.realtime.on<AppChangedEvent>("app.changed", (data) => this.onAppChanged(data)));
    this.lc.add(this.realtime.on<DockerChangedEvent>("docker.changed", (data) => this.onDockerChanged(data)));
    this.lc.add(this.realtime.on("docker.inventory", () => this.syncInventory.run()));
    for (const kind of ["job.done", "job.failed", "job.cancelled"]) {
      this.lc.add(
        this.realtime.on<AppJobEvent>(kind, (data) => this.onAppJobEvent(kind, data)),
      );
    }
    // Older backends emitted `app.status`; keep the list in sync when it arrives.
    this.lc.add(this.realtime.on("app.status", () => this.syncApps.run()));
    this.lc.add(
      this.realtime.onResync(() => {
        void this.refreshAll(true);
        this.detailChangedHook?.({});
      }),
    );
  }

  private onAppChanged(data: AppChangedEvent | null): void {
    if (this.disposed || !data || typeof data !== "object") return;
    const appId = typeof data.appId === "string" ? data.appId : "";
    const index = appId ? this.apps.findIndex((app) => app.id === appId) : -1;
    const current = index >= 0 ? this.apps[index] : undefined;
    if (!current) {
      // A new application (deployed from another session) or no id.
      this.syncApps.run();
      this.syncContainers.run();
      return;
    }
    const containers = normalizeContainers(data.containers) ?? current.containers;
    this.apps[index] = {
      ...current,
      status: normalizeStatus(data.status, current.status),
      containers,
      updatedAt: Date.now(),
    };
    this.syncContainers.run();
    this.renderView();
    this.detailChangedHook?.(data);
  }

  private onDockerChanged(data: DockerChangedEvent | null): void {
    if (this.disposed || !data || typeof data !== "object") return;
    const enabled = typeof data.enabled === "boolean" ? data.enabled : this.docker?.enabled ?? true;
    const available = typeof data.available === "boolean" ? data.available : this.docker?.available ?? false;
    this.docker = {
      enabled,
      available,
      version: typeof data.version === "string" ? data.version : this.docker?.version,
      reason: typeof data.reason === "string" ? data.reason : available ? "" : this.docker?.reason,
    };
    this.dockerError = null;
    this.render();
    this.syncInventory.run();
  }

  private onAppJobEvent(kind: string, data: AppJobEvent | null): void {
    if (this.disposed || !data || typeof data !== "object") return;
    const jobKind = typeof data.kind === "string" ? data.kind : "";
    if (jobKind.startsWith("docker")) {
      this.syncInventory.run();
      return;
    }
    if (!jobKind.startsWith("app")) return;
    if (kind === "job.failed") {
      this.notice.show(`Application job failed: ${data.error ?? "unknown error"}`, { kind: "error" });
    }
    this.syncApps.run();
    this.syncContainers.run();
  }

  /** Reconcile the list after an event that we cannot apply in place. */
  private async reloadAfterEvent(): Promise<void> {
    if (this.disposed) return;
    await Promise.all([this.loadApps(), this.loadDockerStatus()]);
  }

  async start(): Promise<void> {
    this.ctx.setTitle("Docker");
    await this.hydratePermissions();
    this.applyAccessPolicy();
    if (this.denied) return;
    await this.refreshAll();
  }

  /** Older shells may not copy `permissions` into the store yet. */
  private async hydratePermissions(): Promise<void> {
    if (this.ctx.store.get().permissions) return;
    try {
      const me = await this.ctx.api.get<{ permissions?: Permissions }>("/api/auth/me");
      if (me.permissions) this.localPermissions = me.permissions;
    } catch {
      /* fall back to role defaults */
    }
  }

  private async refreshAll(silent = false): Promise<void> {
    if (!silent) {
      this.appsLoading = true;
      this.render();
    }
    await Promise.all([this.loadDockerStatus(), this.loadApps(), this.loadContainers(), this.loadStats(), this.loadInventory()]);
    this.page?.refresh();
  }

  /** Whatever the current view shows, plus the container list (sidebar count). */
  private async loadInventory(): Promise<void> {
    if (this.disposed || this.denied) return;
    const loads: Array<Promise<void>> = [this.loadContainers()];
    if (this.view === "images" || this.imagesLoaded) loads.push(this.loadImages());
    if (this.view === "volumes" || this.volumesLoaded) loads.push(this.loadVolumes());
    await Promise.all(loads);
  }

  private async loadDockerStatus(): Promise<void> {
    if (this.denied) return;
    try {
      const status = await this.ctx.api.get<DockerStatus>("/api/docker/status");
      if (this.disposed) return;
      this.docker = status;
      this.dockerError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.docker = null;
      this.dockerError = error;
    }
    this.render();
  }

  private async loadApps(): Promise<void> {
    if (this.denied) {
      this.appsLoading = false;
      return;
    }
    try {
      const payload = await this.ctx.api.get<ManagedApp[] | { items?: ManagedApp[] }>("/api/apps");
      if (this.disposed) return;
      this.apps = listOf<ManagedAppView>(payload);
      this.appsError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.appsError = error;
      this.apps = [];
    } finally {
      this.appsLoading = false;
      if (!this.disposed) this.renderView();
    }
  }

  private async loadContainers(): Promise<void> {
    if (this.denied || !this.engineAccess()) return;
    try {
      const payload = await this.ctx.api.get<{ items?: EngineContainer[] }>("/api/docker/containers", { perPage: 200 });
      if (this.disposed) return;
      this.containers = listOf<EngineContainer>(payload).map(normalizeEngineContainer);
      this.containersError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.containersError = error;
    } finally {
      this.containersLoaded = true;
      if (!this.disposed) {
        this.renderSidebar();
        if (this.view === "containers") this.renderView();
      }
    }
  }

  private async loadStats(): Promise<void> {
    if (this.denied || !this.engineAccess() || this.docker?.enabled === false) return;
    try {
      const payload = await this.ctx.api.get<{ items?: ContainerStat[] }>("/api/docker/stats");
      if (this.disposed) return;
      this.stats = indexStats(listOf<ContainerStat>(payload));
    } catch {
      // Usage numbers are optional; the list still works without them.
      return;
    }
    if (this.view === "containers" && !this.page) this.renderView();
  }

  private async loadImages(): Promise<void> {
    if (this.denied || !this.engineAccess()) return;
    try {
      const payload = await this.ctx.api.get<{ items?: EngineImage[]; totalBytes?: number }>("/api/docker/images");
      if (this.disposed) return;
      this.images = listOf<EngineImage>(payload);
      this.imagesTotal = typeof payload.totalBytes === "number" ? payload.totalBytes : 0;
      this.imagesError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.imagesError = error;
    } finally {
      this.imagesLoaded = true;
      if (!this.disposed) {
        this.renderSidebar();
        if (this.view === "images") this.renderView();
      }
    }
  }

  private async loadVolumes(): Promise<void> {
    if (this.denied || !this.engineAccess()) return;
    try {
      const payload = await this.ctx.api.get<{ items?: EngineVolume[] }>("/api/docker/volumes");
      if (this.disposed) return;
      this.volumes = listOf<EngineVolume>(payload);
      this.volumesError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.volumesError = error;
    } finally {
      this.volumesLoaded = true;
      if (!this.disposed) {
        this.renderSidebar();
        if (this.view === "volumes") this.renderView();
      }
    }
  }

  private dockerUsable(): boolean {
    return Boolean(this.docker?.enabled && this.docker?.available);
  }

  // ------------------------------------------------------------- rendering

  private render(): void {
    if (this.disposed) return;
    if (this.denied) {
      this.renderDenied();
      return;
    }
    const usable = this.dockerUsable();
    this.statusBadge.className = `dk-engine small is-${usable ? "ok" : this.docker?.enabled === false ? "idle" : "warn"}`;
    this.statusBadge.replaceChildren(
      h("span", { class: "dk-dot", ariaHidden: true }),
      h(
        "span",
        {},
        !this.docker
          ? this.dockerError
            ? "Docker status unknown"
            : "Checking Docker…"
          : !this.docker.enabled
            ? "Docker is turned off"
            : this.docker.available
              ? `Engine running${this.docker.version ? ` · ${this.docker.version}` : ""}`
              : "Engine not reachable",
      ),
    );
    this.deployButton.classList.toggle("hidden", this.readOnly || this.view !== "containers" || Boolean(this.page));
    this.deployButton.disabled = !usable || this.readOnly;
    this.pullButton.classList.toggle("hidden", this.readOnly || this.view !== "images" || !this.engineAccess());
    this.pullButton.disabled = !usable;
    this.search.classList.toggle("hidden", Boolean(this.page));
    this.renderBanners(usable);
    this.renderSidebar();
    this.renderView();
  }

  private renderBanners(usable: boolean): void {
    clear(this.bannerHost);
    if (this.readOnly) {
      this.bannerHost.appendChild(
        banner(
          "info",
          "Read-only access",
          "You can look at apps and their logs. Starting, stopping and deploying need an administrator.",
        ),
      );
    }
    if (this.dockerError) {
      if (isUnavailableError(this.dockerError)) {
        this.bannerHost.appendChild(
          banner(
            "warning",
            "Docker integration is not available on this server",
            unavailableReason(this.dockerError),
            this.dockerPrerequisites(),
          ),
        );
      } else {
        this.bannerHost.appendChild(
          banner("error", "Could not read Docker status", errorMessage(this.dockerError)),
        );
      }
      return;
    }
    if (!this.docker) return;
    if (!this.docker.enabled) {
      this.bannerHost.appendChild(
        banner(
          "info",
          "Docker integration is disabled",
          "Application deployment is turned off. Enable it with RAST_DOCKER_ENABLED=true and restart the RAST service.",
          this.dockerPrerequisites(),
        ),
      );
      return;
    }
    if (!this.docker.available) {
      this.bannerHost.appendChild(
        banner(
          "warning",
          "Docker worker unavailable",
          this.docker.reason || "The privileged rast-worker service is not responding, so container actions are disabled.",
          this.dockerPrerequisites(),
        ),
      );
      return;
    }
    if (!usable) return;
  }

  private dockerPrerequisites(): HTMLElement {
    const list = h(
      "ul",
      { class: "appmgmt-prereqs small muted" },
      h("li", {}, "RAST_DOCKER_ENABLED=true in /etc/rast/rast.env"),
      h("li", {}, "Docker Engine installed and running on the host"),
      h("li", {}, "rast-worker.service started (systemctl status rast-worker)"),
      h("li", {}, "Worker socket readable by the rast service user"),
    );
    return h("div", { class: "appmgmt-prereq-box" }, h("div", { class: "small" }, "Prerequisites"), list);
  }

  private renderSidebar(): void {
    if (this.denied) return;
    clear(this.sidebar);
    const running = this.containers.filter(isRunning).length;
    const views: Array<{ id: EngineView; label: string; icon: string; count: string; title: string }> = [
      {
        id: "containers",
        label: "Containers",
        icon: "box",
        count: this.engineAccess() && this.containersLoaded ? String(running) : "",
        title: `${running} running of ${this.containers.length}`,
      },
    ];
    if (this.engineAccess()) {
      views.push(
        { id: "images", label: "Images", icon: "layers", count: this.imagesLoaded ? String(this.images.length) : "", title: "Images on this server" },
        { id: "volumes", label: "Volumes", icon: "database", count: this.volumesLoaded ? String(this.volumes.length) : "", title: "Volumes on this server" },
      );
    }
    const list = h("div", { class: "dk-nav" });
    for (const v of views) {
      const active = this.view === v.id;
      const button = h(
        "button",
        { class: `nav-item dk-nav-item${active ? " is-active" : ""}`, type: "button", ariaPressed: active },
        iconSvg(v.icon, 16),
        h("span", { class: "dk-nav-label" }, v.label),
        v.count ? h("span", { class: "dk-nav-count", title: v.title }, v.count) : null,
      );
      button.addEventListener("click", () => this.switchView(v.id));
      list.appendChild(button);
    }
    this.sidebar.append(list, this.statusBadge);
  }

  private switchView(view: EngineView): void {
    this.closePage();
    if (this.view === view) {
      this.render();
      return;
    }
    this.view = view;
    this.query = "";
    this.search.value = "";
    this.render();
    if (view === "images" && !this.imagesLoaded) void this.loadImages();
    if (view === "volumes" && !this.volumesLoaded) void this.loadVolumes();
    if (view === "containers") void this.loadStats();
    this.main.scrollTop = 0;
  }

  private renderView(): void {
    if (this.disposed || this.denied || this.page) return;
    const scrollTop = this.main.scrollTop;
    clear(this.viewHost);
    if (this.view === "images") this.renderImages();
    else if (this.view === "volumes") this.renderVolumes();
    else this.renderContainersView();
    this.main.scrollTop = scrollTop;
  }

  private viewHeader(title: string, chips: Array<Node | null>, extra?: Node | null): HTMLElement {
    return h(
      "div",
      { class: "dk-view-head" },
      h("h2", { class: "dk-view-title" }, title),
      h("div", { class: "dk-chips" }, ...chips.filter((chip): chip is Node => Boolean(chip))),
      h("div", { class: "app-toolbar-spacer" }),
      extra ?? null,
    );
  }

  private chip(label: string, value: string, title?: string): HTMLElement {
    return h("span", { class: "dk-chip", title: title ?? "" }, h("span", { class: "muted" }, label), " ", h("strong", {}, value));
  }

  private segmented(value: UsageFilter, onChange: (next: UsageFilter) => void): HTMLElement {
    const group = h("div", { class: "dk-segmented", role: "group", ariaLabel: "Show" });
    const options: Array<[UsageFilter, string]> = [
      ["all", "All"],
      ["used", "In use"],
      ["unused", "Unused"],
    ];
    for (const [id, label] of options) {
      const button = h("button", { class: `dk-segment${value === id ? " is-active" : ""}`, type: "button", ariaPressed: value === id }, label);
      button.addEventListener("click", () => onChange(id));
      group.append(button);
    }
    return group;
  }

  // ------------------------------------------------------------ containers

  private renderContainersView(): void {
    const engine = this.engineAccess();
    const groups = groupContainers(engine ? this.containers : null, this.apps);
    const usage = usageFor(this.containers, this.stats);
    const runningSwitch = h("label", { class: "dk-switch" }, h("input", { type: "checkbox", checked: this.runningOnly }), h("span", {}, "Only running"));
    runningSwitch.querySelector("input")!.addEventListener("change", (event) => {
      this.runningOnly = (event.target as HTMLInputElement).checked;
      this.renderView();
    });
    this.viewHost.append(
      this.viewHeader(
        "Containers",
        engine && usage.sampled > 0
          ? [this.chip("CPU", formatPercent(usage.cpuPercent, 1), "Total of all running containers; 100% is one full core"), this.chip("Memory", formatBytes(usage.memoryBytes))]
          : [],
        runningSwitch,
      ),
    );

    if (engine && this.containersError && this.containers.length === 0) {
      const host = h("div", { class: "dk-body" });
      renderLoadFailure(host, this.containersError, { title: "Could not list containers", retry: () => void this.loadContainers() });
      this.viewHost.append(host);
      return;
    }
    if (this.appsError && !engine) {
      const host = h("div", { class: "dk-body" });
      renderLoadFailure(host, this.appsError, { title: "Could not load apps", retry: () => void this.loadApps() });
      this.viewHost.append(host);
      return;
    }
    const loading = engine ? !this.containersLoaded : this.appsLoading;
    if (loading) {
      this.viewHost.append(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading containers…"));
      return;
    }
    const shown = filterGroups(groups, this.query, this.runningOnly);
    if (groups.length === 0) {
      const deploy = h("button", { class: "btn btn-primary", type: "button", disabled: !this.canChange() }, "Deploy an app");
      deploy.addEventListener("click", () => void this.openDeployDialog());
      this.viewHost.append(emptyBlock("No containers yet", "Deploy a Compose app, or run one from an image.", this.readOnly ? undefined : deploy));
      return;
    }
    if (shown.length === 0) {
      this.viewHost.append(h("div", { class: "dk-empty muted" }, this.query ? `Nothing matches “${this.query.trim()}”.` : "No container is running."));
      return;
    }
    const table = h("div", { class: "dk-table dk-containers", role: "table", ariaLabel: "Containers" });
    table.append(
      h(
        "div",
        { class: "dk-row dk-row-head", role: "row" },
        h("span", { class: "dk-cell dk-cell-name", role: "columnheader" }, "Name"),
        h("span", { class: "dk-cell dk-cell-state", role: "columnheader" }, "Status"),
        engine ? h("span", { class: "dk-cell dk-cell-num", role: "columnheader" }, "CPU") : null,
        engine ? h("span", { class: "dk-cell dk-cell-num dk-cell-mem", role: "columnheader" }, "Memory") : null,
        h("span", { class: "dk-cell dk-cell-ports", role: "columnheader" }, "Ports"),
        h("span", { class: "dk-cell dk-cell-actions", role: "columnheader" }, h("span", { class: "sr-only" }, "Actions")),
      ),
    );
    for (const group of shown) {
      if (group.kind === "single") {
        const container = group.containers[0];
        if (container) table.append(this.containerRow(container, false));
        continue;
      }
      const open = this.expanded.has(group.key) || (this.query.trim() !== "" && group.containers.length > 0);
      table.append(this.groupRow(group, open));
      if (open) for (const container of group.containers) table.append(this.containerRow(container, true));
    }
    this.viewHost.append(table);
  }

  private stateCell(label: string, tone: string): HTMLElement {
    return h("span", { class: "dk-cell dk-cell-state", role: "cell" }, h("span", { class: `dk-dot is-${tone}`, ariaHidden: true }), h("span", { class: "dk-state-label" }, label));
  }

  private portsCell(ports: PortMap[]): HTMLElement {
    const links = portLinks(ports, window.location);
    const cell = h("span", { class: "dk-cell dk-cell-ports", role: "cell" });
    if (links.length === 0) {
      cell.append(h("span", { class: "muted" }, "—"));
      return cell;
    }
    for (const link of links.slice(0, 2)) {
      cell.append(
        link.href
          ? h("a", { class: "dk-port", href: link.href, target: "_blank", rel: "noopener noreferrer", title: `Open ${link.href}`, onClick: (event) => event.stopPropagation() }, link.label, iconSvg("external", 11))
          : h("span", { class: "dk-port is-plain", title: "Published on the server" }, link.label),
      );
    }
    if (links.length > 2) cell.append(h("span", { class: "muted small" }, `+${links.length - 2}`));
    return cell;
  }

  private usageCells(containers: EngineContainer[]): Array<HTMLElement | null> {
    if (!this.engineAccess()) return [null, null];
    const usage = usageFor(containers.filter(isRunning), this.stats);
    const has = usage.sampled > 0;
    return [
      h("span", { class: "dk-cell dk-cell-num", role: "cell" }, has ? formatPercent(usage.cpuPercent, 1) : h("span", { class: "muted" }, "—")),
      h("span", { class: "dk-cell dk-cell-num dk-cell-mem", role: "cell" }, has ? formatBytes(usage.memoryBytes) : h("span", { class: "muted" }, "—")),
    ];
  }

  private iconButton(icon: string, label: string, onClick: () => void, options: { disabled?: boolean; danger?: boolean } = {}): HTMLButtonElement {
    const button = h("button", { class: `dk-icon-btn${options.danger ? " is-danger" : ""}`, type: "button", title: label, ariaLabel: label, disabled: options.disabled ?? false }, iconSvg(icon, 15));
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      onClick();
    });
    return button;
  }

  private menuButton(label: string, items: () => ContextMenuEntry[]): HTMLButtonElement {
    const button = this.iconButton("more", `More actions for ${label}`, () => {
      const rect = button.getBoundingClientRect();
      contextMenu(rect.right - 210, rect.bottom + 4, items(), { ariaLabel: label });
    });
    return button;
  }

  private groupRow(group: ContainerGroup, open: boolean): HTMLElement {
    const state = groupState(group);
    const app = group.app ? this.findApp(group.app.id) : undefined;
    const ports = group.containers.flatMap((c) => c.ports);
    const toggle = h(
      "button",
      { class: "dk-toggle", type: "button", ariaExpanded: open, ariaLabel: `${open ? "Hide" : "Show"} containers of ${group.name}`, disabled: group.containers.length === 0 },
      iconSvg(open ? "chevronDown" : "chevronRight", 14),
    );
    toggle.addEventListener("click", (event) => {
      event.stopPropagation();
      if (this.expanded.has(group.key)) this.expanded.delete(group.key);
      else this.expanded.add(group.key);
      this.renderView();
    });
    const name = h(
      "span",
      { class: "dk-cell dk-cell-name", role: "cell" },
      toggle,
      h("span", { class: `dk-glyph${app ? " is-app" : ""}`, ariaHidden: true }, iconSvg(app ? "docker" : "layers", 16)),
      h(
        "span",
        { class: "dk-name-text" },
        h("span", { class: "dk-name", title: group.name }, group.name),
        h("span", { class: "dk-sub muted" }, app ? `RAST app · ${plural(group.containers.length, "container")}` : `Compose · ${plural(group.containers.length, "container")}`),
      ),
    );
    const busy = this.busy.has(group.key) || app?.status === "deploying";
    const actions = h("span", { class: "dk-cell dk-cell-actions", role: "cell" });
    if (busy) {
      actions.append(h("span", { class: "spinner spinner-sm", ariaLabel: "Working" }));
    } else if (this.canChange()) {
      const anyRunning = state.running > 0;
      if (app) {
        actions.append(
          anyRunning
            ? this.iconButton("stop", `Stop ${group.name}`, () => void this.performAction(app, "stop"))
            : this.iconButton("play", `Start ${group.name}`, () => void this.performAction(app, "start")),
          this.iconButton("restart", `Restart ${group.name}`, () => void this.performAction(app, "restart"), { disabled: !anyRunning }),
          this.menuButton(group.name, () => [
            { label: "Details and logs", icon: "logs", onClick: () => void this.openDetail(app.id) },
            { label: "Redeploy", icon: "refresh", onClick: () => void this.performAction(app, "redeploy") },
            "separator",
            { label: "Delete app…", icon: "trash", danger: true, onClick: () => void this.openDeleteDialog(app) },
          ]),
        );
      } else {
        const ids = group.containers.map((c) => c.id);
        actions.append(
          anyRunning
            ? this.iconButton("stop", `Stop ${group.name}`, () => void this.groupAction(group, "stop"))
            : this.iconButton("play", `Start ${group.name}`, () => void this.groupAction(group, "start")),
          this.iconButton("restart", `Restart ${group.name}`, () => void this.groupAction(group, "restart"), { disabled: !anyRunning }),
          this.menuButton(group.name, () => [
            { label: "Manage as RAST app…", icon: "download", onClick: () => void this.importContainers(ids, group.name) },
          ]),
        );
      }
    } else if (app) {
      actions.append(this.iconButton("logs", `Details of ${group.name}`, () => void this.openDetail(app.id)));
    }
    const row = h(
      "div",
      { class: `dk-row dk-row-group${open ? " is-open" : ""}`, role: "row", tabIndex: 0 },
      name,
      this.stateCell(state.label, state.tone),
      ...this.usageCells(group.containers),
      this.portsCell(ports),
      actions,
    );
    if (app?.lastError) row.title = `Last error: ${app.lastError}`;
    const activate = (): void => {
      if (app) void this.openDetail(app.id);
      else toggle.click();
    };
    row.addEventListener("click", activate);
    row.addEventListener("keydown", (event) => {
      if (event.target !== row) return;
      if (event.key === "Enter") activate();
      else if (event.key === "ArrowRight" && !open && group.containers.length) toggle.click();
      else if (event.key === "ArrowLeft" && open) toggle.click();
    });
    return row;
  }

  private containerRow(container: EngineContainer, nested: boolean): HTMLElement {
    const running = isRunning(container);
    const engine = this.engineAccess();
    const name = h(
      "span",
      { class: "dk-cell dk-cell-name", role: "cell" },
      nested ? h("span", { class: "dk-indent", ariaHidden: true }) : null,
      h("span", { class: "dk-glyph", ariaHidden: true }, iconSvg("box", 16)),
      h(
        "span",
        { class: "dk-name-text" },
        h("span", { class: "dk-name", title: container.name }, nested && container.service ? container.service : container.name),
        h("span", { class: "dk-sub muted mono", title: container.image }, container.image),
      ),
    );
    const actions = h("span", { class: "dk-cell dk-cell-actions", role: "cell" });
    if (this.busy.has(container.id)) {
      actions.append(h("span", { class: "spinner spinner-sm", ariaLabel: "Working" }));
    } else if (engine && this.canChange()) {
      actions.append(
        running
          ? this.iconButton("stop", `Stop ${container.name}`, () => void this.containerAction(container, "stop"))
          : this.iconButton("play", `Start ${container.name}`, () => void this.containerAction(container, "start")),
        this.iconButton("restart", `Restart ${container.name}`, () => void this.containerAction(container, "restart"), { disabled: !running }),
        this.menuButton(container.name, () => this.containerMenu(container)),
      );
    }
    const row = h(
      "div",
      { class: `dk-row dk-row-container${nested ? " is-nested" : ""}`, role: "row", tabIndex: engine ? 0 : -1 },
      name,
      this.stateCell(containerStateLabel(container), containerTone(container)),
      ...this.usageCells([container]),
      this.portsCell(container.ports),
      actions,
    );
    if (engine) {
      row.addEventListener("click", () => this.openPage(container.id));
      row.addEventListener("keydown", (event) => {
        if (event.target === row && event.key === "Enter") this.openPage(container.id);
      });
    }
    return row;
  }

  private containerMenu(container: EngineContainer): ContextMenuEntry[] {
    const items: ContextMenuEntry[] = [{ label: "Logs and details", icon: "logs", onClick: () => this.openPage(container.id) }];
    const link = portLinks(container.ports, window.location).find((l) => l.href);
    if (link?.href) {
      const href = link.href;
      items.push({ label: `Open ${link.label.split(":")[0]} in browser`, icon: "external", onClick: () => window.open(href, "_blank", "noopener") });
    }
    if (container.managed) {
      const app = this.apps.find((a) => a.slug === container.slug);
      if (app) items.push("separator", { label: "Open app", icon: "docker", onClick: () => void this.openDetail(app.id) });
    } else {
      items.push(
        "separator",
        { label: "Manage as RAST app…", icon: "download", onClick: () => void this.importContainers([container.id], container.name) },
        { label: "Delete…", icon: "trash", danger: true, onClick: () => void this.deleteContainer(container) },
      );
    }
    return items;
  }

  private async containerAction(container: EngineContainer, action: "start" | "stop" | "restart"): Promise<void> {
    if (!this.canChange() || this.busy.has(container.id)) return;
    this.busy.add(container.id);
    this.renderView();
    const done = { start: "started", stop: "stopped", restart: "restarted" } as const;
    try {
      await this.ctx.api.post(`/api/docker/containers/${encodeURIComponent(container.id)}/${action}`);
      this.ctx.toast.success(`${container.name} ${done[action]}`);
    } catch (error) {
      this.ctx.toast.error(`${container.name}: ${errorMessage(error)}`);
    } finally {
      this.busy.delete(container.id);
    }
    await Promise.all([this.loadContainers(), this.loadStats()]);
  }

  /** Start/stop/restart every container of a Compose project RAST does not manage. */
  private async groupAction(group: ContainerGroup, action: "start" | "stop" | "restart"): Promise<void> {
    if (!this.canChange() || this.busy.has(group.key)) return;
    const targets = group.containers.filter((c) => (action === "start" ? !isRunning(c) : isRunning(c)));
    if (targets.length === 0) return;
    this.busy.add(group.key);
    this.renderView();
    const results = await Promise.allSettled(
      targets.map((c) => this.ctx.api.post(`/api/docker/containers/${encodeURIComponent(c.id)}/${action}`)),
    );
    this.busy.delete(group.key);
    const failed = results.filter((r) => r.status === "rejected") as PromiseRejectedResult[];
    const done = { start: "started", stop: "stopped", restart: "restarted" } as const;
    if (failed.length === 0) this.ctx.toast.success(`${group.name} ${done[action]}`);
    else this.ctx.toast.error(`${group.name}: ${failed.length} of ${targets.length} failed — ${errorMessage(failed[0]!.reason)}`);
    await Promise.all([this.loadContainers(), this.loadStats()]);
  }

  private async deleteContainer(container: EngineContainer): Promise<void> {
    if (!this.canChange()) return;
    const confirmed = await confirmDialog({
      title: "Delete container",
      message: `Delete “${container.name}”? It is stopped and removed. Its volumes and data stay.`,
      confirmLabel: "Delete",
      danger: true,
      requireTyped: container.name,
    });
    if (!confirmed || this.disposed) return;
    this.busy.add(container.id);
    this.renderView();
    try {
      await this.ctx.api.del(`/api/docker/containers/${encodeURIComponent(container.id)}`, { confirm: container.name });
      this.ctx.toast.success(`${container.name} deleted`);
    } catch (error) {
      this.ctx.toast.error(`Could not delete ${container.name}: ${errorMessage(error)}`);
    } finally {
      this.busy.delete(container.id);
    }
    await this.loadContainers();
  }

  // --------------------------------------------------------- container page

  private openPage(id: string): void {
    if (!this.engineAccess()) return;
    this.page?.dispose();
    clear(this.viewHost);
    this.page = mountContainerPage({
      ctx: this.ctx,
      lc: this.lc,
      id,
      canChange: () => this.canChange(),
      onBack: () => this.closePage(true),
      onChanged: () => void this.loadContainers(),
      openApp: (appId) => void this.openDetail(appId),
      importContainers: (ids, label) => this.importContainers(ids, label),
    });
    this.viewHost.append(this.page.el);
    this.render();
    this.main.scrollTop = 0;
  }

  private closePage(rerender = false): void {
    if (!this.page) return;
    this.page.dispose();
    this.page = null;
    if (rerender) {
      this.render();
      void this.loadStats();
    }
  }

  // ---------------------------------------------------------------- images

  private renderImages(): void {
    const used = this.images.filter((i) => i.inUse).length;
    const untaggedUnused = this.images.filter((i) => !i.reference && !i.inUse).length;
    const prune = h(
      "button",
      { class: "btn btn-ghost btn-sm", type: "button", disabled: !this.canChange() || untaggedUnused === 0, title: "Delete untagged images that no container uses" },
      iconSvg("trash", 13),
      h("span", {}, "Clean up"),
    );
    prune.addEventListener("click", () => void this.pruneImages(untaggedUnused));
    this.viewHost.append(
      this.viewHeader(
        "Images",
        this.imagesLoaded && !this.imagesError
          ? [this.chip("Total", formatBytes(this.imagesTotal), "Sum of image sizes; shared layers are counted once per image"), this.chip("In use", `${used} of ${this.images.length}`)]
          : [],
        h("div", { class: "row" }, this.segmented(this.imageFilter, (next) => {
          this.imageFilter = next;
          this.renderView();
        }), this.readOnly ? null : prune),
      ),
    );
    if (this.imagesError) {
      const host = h("div", { class: "dk-body" });
      renderLoadFailure(host, this.imagesError, { title: "Could not list images", retry: () => void this.loadImages() });
      this.viewHost.append(host);
      return;
    }
    if (!this.imagesLoaded) {
      this.viewHost.append(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading images…"));
      return;
    }
    const shown = filterImages(this.images, this.query, this.imageFilter);
    if (shown.length === 0) {
      this.viewHost.append(
        this.images.length === 0
          ? emptyBlock("No images", "Pull an image from Docker Hub to get started.")
          : h("div", { class: "dk-empty muted" }, this.query ? `Nothing matches “${this.query.trim()}”.` : "No images here."),
      );
      return;
    }
    const table = h("div", { class: "dk-table dk-images", role: "table", ariaLabel: "Images" });
    table.append(
      h(
        "div",
        { class: "dk-row dk-row-head", role: "row" },
        h("span", { class: "dk-cell dk-cell-name", role: "columnheader" }, "Name"),
        h("span", { class: "dk-cell dk-cell-state", role: "columnheader" }, "Status"),
        h("span", { class: "dk-cell dk-cell-created", role: "columnheader" }, "Created"),
        h("span", { class: "dk-cell dk-cell-num", role: "columnheader" }, "Size"),
        h("span", { class: "dk-cell dk-cell-actions", role: "columnheader" }, h("span", { class: "sr-only" }, "Actions")),
      ),
    );
    for (const image of shown) table.append(this.imageRow(image));
    this.viewHost.append(table);
  }

  private imageRow(image: EngineImage): HTMLElement {
    const label = imageLabel(image);
    const busyKey = `image:${image.id}:${image.reference}`;
    const usedBy = image.usedBy.length ? image.usedBy.join(", ") : "";
    const name = h(
      "span",
      { class: "dk-cell dk-cell-name", role: "cell" },
      h("span", { class: "dk-glyph", ariaHidden: true }, iconSvg("layers", 16)),
      h(
        "span",
        { class: "dk-name-text" },
        h("span", { class: `dk-name${image.reference ? "" : " muted"}`, title: label }, label),
        h("span", { class: "dk-sub muted mono", title: image.id }, shortId(image.id)),
      ),
    );
    const status = image.inUse
      ? h("span", { class: "dk-cell dk-cell-state", role: "cell", title: usedBy ? `Used by ${usedBy}` : "Used by a container" }, h("span", { class: "dk-pill is-ok" }, "In use"))
      : h("span", { class: "dk-cell dk-cell-state", role: "cell" }, h("span", { class: "dk-pill" }, "Unused"));
    const actions = h("span", { class: "dk-cell dk-cell-actions", role: "cell" });
    if (this.busy.has(busyKey)) {
      actions.append(h("span", { class: "spinner spinner-sm", ariaLabel: "Working" }));
    } else if (this.canChange()) {
      actions.append(
        this.menuButton(label, () => {
          const items: ContextMenuEntry[] = [];
          if (image.reference) {
            items.push({ label: "Pull again", icon: "download", onClick: () => void this.pullImage(image.reference) });
          }
          items.push({
            label: "Copy image ID",
            icon: "copy",
            onClick: () => void copyText(image.id).then((ok) => (ok ? this.ctx.toast.success("Image ID copied") : this.ctx.toast.error("Could not copy"))),
          });
          items.push("separator", {
            label: "Delete…",
            icon: "trash",
            danger: true,
            disabled: image.inUse,
            onClick: () => void this.deleteImage(image),
          });
          return items;
        }),
      );
    }
    return h(
      "div",
      { class: "dk-row", role: "row" },
      name,
      status,
      h("span", { class: "dk-cell dk-cell-created muted", role: "cell" }, image.createdSince || "—"),
      h("span", { class: "dk-cell dk-cell-num", role: "cell" }, image.sizeBytes !== null ? formatBytes(image.sizeBytes) : image.size || "—"),
      actions,
    );
  }

  private openPullDialog(initial = ""): void {
    if (!this.canChange()) return;
    const input = h("input", { class: "input mono", type: "text", placeholder: "nginx:latest", autocomplete: "off", spellcheck: false, value: initial, ariaLabel: "Image name" });
    const error = formError("");
    const body = h(
      "div",
      { class: "col" },
      field("Image", input, "From Docker Hub (nginx, postgres:17) or a registry (ghcr.io/owner/app:tag)."),
      error,
    );
    const close = modal({
      title: "Pull image",
      body,
      onClose: () => undefined,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Pull",
          variant: "primary",
          keepOpen: true,
          onClick: (closeDialog) => {
            const problem = imageRefError(input.value);
            if (problem) {
              error.textContent = problem;
              input.focus();
              return;
            }
            closeDialog();
            void this.pullImage(input.value.trim());
          },
        },
      ],
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        (body.closest(".modal")?.querySelector(".btn-primary") as HTMLButtonElement | null)?.click();
      }
    });
    void close;
    window.setTimeout(() => input.focus(), 0);
  }

  private async pullImage(image: string): Promise<void> {
    if (!this.canChange()) return;
    try {
      const response = await this.ctx.api.post<{ jobId?: string }>("/api/docker/images/pull", { image });
      this.ctx.toast.info(`Pulling ${image}…`);
      // The desktop announces when the download ends (finished or failed).
      if (response.jobId) await watchJob(this.ctx, this.lc, response.jobId);
      if (this.disposed) return;
    } catch (error) {
      this.ctx.toast.error(`Could not pull ${image}: ${errorMessage(error)}`);
    }
    await this.loadImages();
  }

  private async deleteImage(image: EngineImage): Promise<void> {
    if (!this.canChange() || image.inUse) return;
    const label = imageLabel(image);
    const target = image.reference || image.id;
    const confirmed = await confirmDialog({
      title: "Delete image",
      message: image.reference
        ? `Delete “${label}”? Images you built on this server can't be downloaded again.`
        : `Delete the untagged image ${shortId(image.id)}?`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!confirmed || this.disposed) return;
    const busyKey = `image:${image.id}:${image.reference}`;
    this.busy.add(busyKey);
    this.renderView();
    try {
      await this.ctx.api.post("/api/docker/images/remove", { image: target });
      this.ctx.toast.success(`${label} deleted`);
    } catch (error) {
      this.ctx.toast.error(`Could not delete ${label}: ${errorMessage(error)}`);
    } finally {
      this.busy.delete(busyKey);
    }
    await this.loadImages();
  }

  private async pruneImages(count: number): Promise<void> {
    if (!this.canChange()) return;
    const confirmed = await confirmDialog({
      title: "Clean up images",
      message: `Delete ${plural(count, "untagged image")} that no container uses? Tagged images stay.`,
      confirmLabel: "Clean up",
      danger: true,
    });
    if (!confirmed || this.disposed) return;
    try {
      const result = await this.ctx.api.post<{ deleted?: number; reclaimed?: string }>("/api/docker/images/prune");
      this.ctx.toast.success(`${plural(result.deleted ?? 0, "image")} deleted${result.reclaimed ? ` · ${result.reclaimed} freed` : ""}`);
    } catch (error) {
      this.ctx.toast.error(`Clean up failed: ${errorMessage(error)}`);
    }
    await this.loadImages();
  }

  // --------------------------------------------------------------- volumes

  private renderVolumes(): void {
    const used = this.volumes.filter((v) => v.inUse).length;
    this.viewHost.append(
      this.viewHeader(
        "Volumes",
        this.volumesLoaded && !this.volumesError ? [this.chip("In use", `${used} of ${this.volumes.length}`)] : [],
        this.segmented(this.volumeFilter, (next) => {
          this.volumeFilter = next;
          this.renderView();
        }),
      ),
    );
    if (this.volumesError) {
      const host = h("div", { class: "dk-body" });
      renderLoadFailure(host, this.volumesError, { title: "Could not list volumes", retry: () => void this.loadVolumes() });
      this.viewHost.append(host);
      return;
    }
    if (!this.volumesLoaded) {
      this.viewHost.append(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading volumes…"));
      return;
    }
    const shown = filterVolumes(this.volumes, this.query, this.volumeFilter);
    if (shown.length === 0) {
      this.viewHost.append(
        this.volumes.length === 0
          ? emptyBlock("No volumes", "Volumes appear here when a container stores data in one.")
          : h("div", { class: "dk-empty muted" }, this.query ? `Nothing matches “${this.query.trim()}”.` : "No volumes here."),
      );
      return;
    }
    const table = h("div", { class: "dk-table dk-volumes", role: "table", ariaLabel: "Volumes" });
    table.append(
      h(
        "div",
        { class: "dk-row dk-row-head", role: "row" },
        h("span", { class: "dk-cell dk-cell-name", role: "columnheader" }, "Name"),
        h("span", { class: "dk-cell dk-cell-state", role: "columnheader" }, "Status"),
        h("span", { class: "dk-cell dk-cell-users", role: "columnheader" }, "Used by"),
        h("span", { class: "dk-cell dk-cell-actions", role: "columnheader" }, h("span", { class: "sr-only" }, "Actions")),
      ),
    );
    for (const volume of shown) table.append(this.volumeRow(volume));
    this.viewHost.append(table);
  }

  private volumeRow(volume: EngineVolume): HTMLElement {
    const busyKey = `volume:${volume.name}`;
    const displayName = volume.anonymous ? `${volume.name.slice(0, 12)}…` : volume.name;
    const name = h(
      "span",
      { class: "dk-cell dk-cell-name", role: "cell" },
      h("span", { class: "dk-glyph", ariaHidden: true }, iconSvg("database", 16)),
      h(
        "span",
        { class: "dk-name-text" },
        h("span", { class: "dk-name mono", title: volume.name }, displayName),
        h("span", { class: "dk-sub muted" }, volume.app ? `RAST app ${volume.app}` : volume.anonymous ? "Anonymous" : volume.project ? `Compose ${volume.project}` : volume.driver),
      ),
    );
    const status = h("span", { class: "dk-cell dk-cell-state", role: "cell" }, h("span", { class: `dk-pill${volume.inUse ? " is-ok" : ""}` }, volume.inUse ? "In use" : "Unused"));
    const users = h("span", { class: "dk-cell dk-cell-users muted", role: "cell", title: volume.usedBy.join(", ") }, volume.usedBy.length ? volume.usedBy.join(", ") : "—");
    const actions = h("span", { class: "dk-cell dk-cell-actions", role: "cell" });
    if (this.busy.has(busyKey)) {
      actions.append(h("span", { class: "spinner spinner-sm", ariaLabel: "Working" }));
    } else if (this.canChange()) {
      const reason = volume.inUse ? "Used by a container" : volume.app ? "Deleted with its app" : "";
      actions.append(
        this.menuButton(volume.name, () => [
          {
            label: "Copy name",
            icon: "copy",
            onClick: () => void copyText(volume.name).then((ok) => (ok ? this.ctx.toast.success("Volume name copied") : this.ctx.toast.error("Could not copy"))),
          },
          "separator",
          {
            label: reason ? `Delete (${reason.toLowerCase()})` : "Delete…",
            icon: "trash",
            danger: true,
            disabled: Boolean(reason),
            onClick: () => void this.deleteVolume(volume),
          },
        ]),
      );
    }
    return h("div", { class: "dk-row", role: "row" }, name, status, users, actions);
  }

  private async deleteVolume(volume: EngineVolume): Promise<void> {
    if (!this.canChange() || volume.inUse || volume.app) return;
    const confirmed = await confirmDialog({
      title: "Delete volume",
      message: `Delete “${volume.name}” and all the data in it? This can't be undone.`,
      confirmLabel: "Delete volume",
      danger: true,
      requireTyped: volume.name,
    });
    if (!confirmed || this.disposed) return;
    const busyKey = `volume:${volume.name}`;
    this.busy.add(busyKey);
    this.renderView();
    try {
      await this.ctx.api.post("/api/docker/volumes/remove", { name: volume.name, confirm: volume.name });
      this.ctx.toast.success(`${volume.name} deleted`);
    } catch (error) {
      this.ctx.toast.error(`Could not delete ${volume.name}: ${errorMessage(error)}`);
    } finally {
      this.busy.delete(busyKey);
    }
    await this.loadVolumes();
  }

  // --------------------------------------------------------------- actions

  private findApp(id: string): ManagedAppView | undefined {
    return this.apps.find((app) => app.id === id);
  }

  private async performAction(app: ManagedAppView, action: AppAction): Promise<void> {
    if (this.denied) {
      this.ctx.toast.warning("Your access to Docker was changed.");
      return;
    }
    if (this.readOnly) {
      this.ctx.toast.warning("You have read-only access to Docker.");
      return;
    }
    const labels: Record<AppAction, string> = {
      start: "Start",
      stop: "Stop",
      restart: "Restart",
      redeploy: "Redeploy",
    };
    if (action === "redeploy") {
      const confirmed = await confirmDialog({
        title: "Redeploy app",
        message: `Redeploy “${app.name}”? Its containers are recreated from the saved Compose file; it is briefly offline.`,
        confirmLabel: "Redeploy",
        danger: true,
      });
      if (!confirmed || this.disposed) return;
    }
    const key = `app:${app.id}`;
    this.busy.add(key);
    this.renderView();
    try {
      const response = await this.ctx.api.post<ActionResponse>(`/api/apps/${encodeURIComponent(app.id)}/${action}`);
      if (response?.jobId) {
        const outcome = await watchJob(this.ctx, this.lc, response.jobId, () => undefined);
        if (this.disposed || this.ctx.window.isClosed()) return;
        if (outcome.status === "done") this.ctx.toast.success(`${app.name}: ${labels[action].toLowerCase()} done`);
        else if (outcome.status === "failed") this.ctx.toast.error(`${labels[action]} failed: ${outcome.error ?? "unknown error"}`);
        else this.ctx.toast.warning(`${labels[action]} cancelled`);
      } else {
        this.ctx.toast.success(`${labels[action]} requested for ${app.name}`);
      }
    } catch (error) {
      this.ctx.toast.error(`${labels[action]} failed: ${errorMessage(error)}`);
    } finally {
      this.busy.delete(key);
    }
    await Promise.all([this.loadApps(), this.loadDockerStatus(), this.loadContainers()]);
  }

  // ---------------------------------------------------------------- deploy

  private async openDeployDialog(): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Docker was changed." : "You have read-only access to Docker.",
      );
      return;
    }
    if (!this.dockerUsable()) {
      this.ctx.toast.warning("Application deployment is unavailable: Docker worker is not ready.");
      return;
    }
    const nameInput = h("input", { class: "input", type: "text", placeholder: "my-application", autocomplete: "off" });
    const composeInput = h("textarea", {
      class: "textarea appmgmt-compose",
      placeholder: "services:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"8080:80\"",
    });
    const envRows = h("div", { class: "appmgmt-env-rows" });
    const envList: Array<{ key: HTMLInputElement; value: HTMLInputElement; secret: HTMLInputElement; root: HTMLElement }> = [];
    const validation = formError("");
    const submitProgress = progressBar();
    submitProgress.el.classList.add("hidden");
    const submitStatus = h("div", { class: "small muted" });
    const addRow = (initial?: AppEnvVar) => {
      const key = h("input", { class: "input appmgmt-env-key", type: "text", placeholder: "KEY", value: initial?.key ?? "" });
      const value = h("input", { class: "input appmgmt-env-value", type: "text", placeholder: "value", value: initial?.value ?? "" });
      if (initial?.secret) value.type = "password";
      const secret = h("input", { type: "checkbox", ariaLabel: "Secret value" });
      secret.checked = Boolean(initial?.secret);
      secret.addEventListener("change", () => {
        value.type = secret.checked ? "password" : "text";
      });
      const remove = h("button", { class: "appmgmt-icon-btn is-danger", type: "button", ariaLabel: "Remove variable", title: "Remove variable" });
      remove.appendChild(iconSvg("close", 14));
      const root = h(
        "div",
        { class: "appmgmt-env-row" },
        key,
        value,
        h("label", { class: "checkbox appmgmt-secret-label", title: "Encrypt and mask this value at rest" }, secret, "secret"),
        remove,
      );
      remove.addEventListener("click", () => {
        const index = envList.findIndex((row) => row.root === root);
        if (index >= 0) envList.splice(index, 1);
        root.remove();
      });
      envList.push({ key, value, secret, root });
      envRows.appendChild(root);
    };
    const addButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, iconSvg("plus", 13), document.createTextNode("Add variable"));
    addButton.addEventListener("click", () => addRow());

    const header = h(
      "div",
      { class: "appmgmt-deploy-head" },
      h("div", { class: "small muted" }, "The application name becomes the Docker Compose project name. The Compose file is validated on the server before anything runs."),
    );
    const body = h(
      "div",
      { class: "col appmgmt-deploy-form" },
      header,
      h("label", { class: "field" }, h("span", { class: "field-label" }, "Application name"), nameInput),
      h(
        "label",
        { class: "field" },
        h("span", { class: "field-label" }, "Compose YAML"),
        composeInput,
        h("span", { class: "field-hint" }, "Must define a services: section. Invalid YAML is rejected by the server with a reason."),
      ),
      h("div", { class: "field" }, h("span", { class: "field-label" }, "Environment variables"), envRows, addButton),
      submitProgress.el,
      submitStatus,
      validation,
    );

    let submitted = false;
    const close = modal({
      title: "Deploy application",
      body,
      wide: true,
      onClose: () => {
        /* nothing to abort: the request completes server-side */
      },
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Deploy",
          variant: "primary",
          keepOpen: true,
          onClick: async (closeDialog) => {
            if (submitted) return;
            validation.textContent = "";
            const name = nameInput.value.trim();
            const compose = composeInput.value;
            const nameError = validateAppName(name);
            if (nameError) {
              validation.textContent = nameError;
              nameInput.focus();
              return;
            }
            const composeError = validateCompose(compose);
            if (composeError) {
              validation.textContent = composeError;
              composeInput.focus();
              return;
            }
            const env: AppEnvVar[] = [];
            const seen = new Set<string>();
            for (const row of envList) {
              const key = row.key.value.trim();
              if (!key && !row.value.value) continue;
              if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
                validation.textContent = `Invalid environment variable name: “${key || "(empty)"}”`;
                row.key.focus();
                return;
              }
              if (seen.has(key)) {
                validation.textContent = `Duplicate environment variable: ${key}`;
                row.key.focus();
                return;
              }
              seen.add(key);
              env.push({ key, value: row.value.value, secret: row.secret.checked });
            }
            submitted = true;
            submitProgress.el.classList.remove("hidden");
            submitProgress.update(0.08);
            submitStatus.textContent = "Validating Compose file and creating the project…";
            try {
              const response = await this.ctx.api.post<DeployResponse>("/api/apps", { name, compose, env });
              if (response?.jobId) {
                submitStatus.textContent = "Deployment queued…";
                const outcome = await watchJob(this.ctx, this.lc, response.jobId, (update) => {
                  submitProgress.update(jobProgressFraction(update));
                  submitStatus.textContent = jobProgressLabel(update) || "Deploying…";
                });
                if (outcome.status === "done") {
                  submitProgress.update(1);
                  this.ctx.toast.success(`Application “${name}” deployed`);
                  closeDialog();
                  await Promise.all([this.loadApps(), this.loadContainers()]);
                  return;
                }
                submitProgress.setError(true);
                validation.textContent =
                  outcome.status === "failed" ? `Deployment failed: ${outcome.error ?? "unknown error"}` : "Deployment was cancelled.";
                submitted = false;
                return;
              }
              submitProgress.update(1);
              this.ctx.toast.success(`Application “${name}” deployed`);
              closeDialog();
              await Promise.all([this.loadApps(), this.loadContainers()]);
            } catch (error) {
              submitProgress.setError(true);
              validation.textContent = `Deployment failed: ${errorMessage(error)}`;
              submitted = false;
            }
          },
        },
      ],
    });
    void close;
    window.setTimeout(() => nameInput.focus(), 40);
  }

  // --------------------------------------------------------------- details

  private async openDetail(appId: string): Promise<void> {
    const summary = this.findApp(appId);
    if (!summary) return;
    const localCleanups: Array<() => void> = [];
    let closed = false;
    let tab: DetailTab = "overview";
    let logsTail = 200;
    let detail: AppDetail = { ...summary };
    let detailController: AbortController | null = null;
    let historyLoaded = false;

    const statusLine = h("div", { class: "appmgmt-detail-status row wrap" });
    const overviewHost = h("div", { class: "appmgmt-tab-body" });
    const logsHost = h("div", { class: "appmgmt-tab-body" });
    const historyHost = h("div", { class: "appmgmt-tab-body" });
    const tabBar = h("div", { class: "tabs appmgmt-detail-tabs" });
    const actionRow = h("div", { class: "appmgmt-detail-actions" });
    const actionButton = (icon: string, label: string, variant: "ghost" | "danger", onClick: () => void) => {
      const btn = h("button", { class: `btn btn-${variant} btn-sm`, type: "button" }, iconSvg(icon, 13), document.createTextNode(label));
      btn.addEventListener("click", onClick);
      return btn;
    };
    if (!this.readOnly) {
      actionRow.append(
        actionButton("play", "Start", "ghost", () => void this.performAction(detail, "start")),
        actionButton("stop", "Stop", "ghost", () => void this.performAction(detail, "stop")),
        actionButton("restart", "Restart", "ghost", () => void this.performAction(detail, "restart")),
        actionButton("refresh", "Redeploy", "ghost", () => void this.performAction(detail, "redeploy")),
        actionButton("trash", "Delete", "danger", () => {
          close();
          void this.openDeleteDialog(detail);
        }),
      );
    }
    const body = h(
      "div",
      { class: "appmgmt-detail" },
      statusLine,
      actionRow,
      tabBar,
      overviewHost,
      logsHost,
      historyHost,
    );

    // Connection badge for this window (same semantics as the toolbar badge).
    const detailLiveDot = h("span", { class: "live-dot" });
    const detailLiveLabel = h("span", { class: "live-label" });
    const detailLive = h(
      "span",
      { class: "live-badge", role: "status", ariaLive: "polite", title: "Realtime update status" },
      detailLiveDot,
      detailLiveLabel,
    );
    const renderDetailLive = (state: ConnectionState): void => {
      detailLive.dataset.state = state;
      detailLiveLabel.textContent =
        state === "connected" ? "Live" : state === "offline" ? "Offline" : state === "connecting" ? "Connecting…" : "Reconnecting…";
    };
    localCleanups.push(this.realtime.onStatus(renderDetailLive));
    renderDetailLive(this.realtime.status);

    const detailSync = debounceTask(() => void refreshDetail(), 350);
    localCleanups.push(() => detailSync.cancel());

    // ------------------------------------------------------- logs streaming
    const LOG_BUFFER_MAX_LINES = 500;
    const LOG_BUFFER_MAX_CHARS = 500_000;
    const logBuffer: LogLine[] = [];
    let logBufferChars = 0;
    let logStream: EventSource | null = null;
    let logAttempts = 0;
    let logRetryTimer = 0;
    let logWatchdog = 0;
    let logUi: LogStreamUi | null = null;
    let logState: "idle" | "connecting" | "live" | "reconnecting" | "unavailable" = "idle";
    let logsVisible = false;

    const formatLine = (line: LogLine): string =>
      `${formatTime(line.ts)} ${String(line.stream || "out").slice(0, 5).padEnd(5)} ${line.message}`;

    const renderLogStatus = (): void => {
      if (!logUi) return;
      const labels: Record<typeof logState, string> = {
        idle: "Idle",
        connecting: "Connecting…",
        live: "Live",
        reconnecting: "Reconnecting…",
        unavailable: "Unavailable",
      };
      logUi.status.dataset.state = logState;
      logUi.status.textContent = labels[logState];
      logUi.reconnect.classList.toggle("hidden", logState === "live" || logState === "connecting");
    };

    const renderLogs = (): void => {
      if (!logUi) return;
      const pre = logUi.pre;
      const atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 6;
      pre.textContent = logBuffer.length > 0 ? logBuffer.map(formatLine).join("\n") : "(no log output yet)";
      if (atBottom) pre.scrollTop = pre.scrollHeight;
      renderLogStatus();
    };

    const appendLogs = (lines: LogLine[], reset: boolean): void => {
      if (reset) {
        logBuffer.length = 0;
        logBufferChars = 0;
      }
      for (const line of lines) {
        if (!line || typeof line !== "object") continue;
        const message = typeof line.message === "string" ? line.message : "";
        logBuffer.push({
          ts: typeof line.ts === "number" ? line.ts : 0,
          stream: typeof line.stream === "string" ? line.stream : "stdout",
          message,
        });
        logBufferChars += message.length + 24;
      }
      while (logBuffer.length > LOG_BUFFER_MAX_LINES) {
        const dropped = logBuffer.shift();
        logBufferChars -= (dropped?.message.length ?? 0) + 24;
      }
      while (logBufferChars > LOG_BUFFER_MAX_CHARS && logBuffer.length > 1) {
        const dropped = logBuffer.shift();
        logBufferChars -= (dropped?.message.length ?? 0) + 24;
      }
      if (logBufferChars < 0) logBufferChars = 0;
      renderLogs();
    };

    const stopLogStream = (): void => {
      if (logStream) {
        logStream.close();
        logStream = null;
      }
      if (logRetryTimer) {
        this.lc.clearTimeout(logRetryTimer);
        logRetryTimer = 0;
      }
      if (logWatchdog) {
        this.lc.clearTimeout(logWatchdog);
        logWatchdog = 0;
      }
    };

    const scheduleLogRetry = (): void => {
      logAttempts += 1;
      if (logAttempts > 3) {
        logState = "unavailable";
        renderLogStatus();
        return;
      }
      logState = "reconnecting";
      renderLogStatus();
      const delay = Math.min(8000, 800 * 2 ** (logAttempts - 1));
      logRetryTimer = this.lc.setTimeout(() => {
        logRetryTimer = 0;
        connectLogStream();
      }, delay);
    };

    const connectLogStream = (): void => {
      if (closed || this.disposed || !logsVisible) return;
      stopLogStream();
      logState = "connecting";
      renderLogStatus();
      const url = `/api/apps/${encodeURIComponent(detail.id)}/logs/stream?tail=${logsTail}`;
      let source: EventSource;
      try {
        source = new EventSource(url, { withCredentials: true });
      } catch {
        logState = "unavailable";
        renderLogStatus();
        return;
      }
      logStream = source;
      let opened = false;
      source.addEventListener("open", () => {
        if (logStream !== source) return;
        opened = true;
        logAttempts = 0;
        logState = "live";
        renderLogStatus();
      });
      source.addEventListener("app.logs", ((event: MessageEvent<string>) => {
        if (logStream !== source) return;
        let payload: AppLogsEvent | null = null;
        try {
          payload = JSON.parse(event.data) as AppLogsEvent;
        } catch {
          payload = null;
        }
        if (!payload || typeof payload !== "object") return;
        if (typeof payload.appId === "string" && payload.appId !== detail.id) return;
        logAttempts = 0;
        logState = "live";
        const lines = Array.isArray(payload.lines) ? payload.lines : [];
        appendLogs(lines, payload.reset === true);
      }) as EventListener);
      source.addEventListener("error", () => {
        if (logStream !== source) return;
        if (source.readyState === EventSource.CLOSED) {
          stopLogStream();
          scheduleLogRetry();
        } else {
          logState = "reconnecting";
          renderLogStatus();
        }
      });
      logWatchdog = this.lc.setTimeout(() => {
        logWatchdog = 0;
        if (!opened && logStream === source) {
          stopLogStream();
          scheduleLogRetry();
        }
      }, 6000);
    };

    const buildLogsUi = (): void => {
      clear(logsHost);
      const controls = h("div", { class: "appmgmt-log-controls row wrap" });
      const tailSelect = h("select", { class: "select appmgmt-tail-select", ariaLabel: "Log tail length" });
      for (const size of TAIL_OPTIONS) {
        const option = h("option", { value: String(size) }, `${size} lines`);
        if (size === logsTail) option.selected = true;
        tailSelect.appendChild(option);
      }
      tailSelect.addEventListener("change", () => {
        logsTail = Number(tailSelect.value) || 200;
        logAttempts = 0;
        if (logsVisible) connectLogStream();
      });
      const status = h("span", { class: "live-badge app-log-live", role: "status", ariaLive: "polite" }, h("span", { class: "live-dot" }), h("span", { class: "live-label" }, "Connecting…"));
      const reconnect = h("button", { class: "btn btn-ghost btn-sm hidden", type: "button" }, "Reconnect");
      reconnect.addEventListener("click", () => {
        logAttempts = 0;
        connectLogStream();
      });
      const download = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("download", 13), document.createTextNode("Download logs"));
      const pre = h("pre", { class: "appmgmt-logs mono", ariaLabel: "Application log stream" });
      download.addEventListener("click", () => {
        const blob = new Blob([pre.textContent ?? ""], { type: "text/plain;charset=utf-8" });
        const url = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = `${detail.slug || "application"}-logs.txt`;
        anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 4000);
      });
      const bufferHint = h("span", { class: "small muted app-log-buffer" }, "Live stream · last 500 lines kept");
      controls.append(tailSelect, status, reconnect, download, bufferHint);
      logsHost.append(controls, pre);
      logUi = { pre, status, reconnect };
      renderLogs();
    };

    // --------------------------------------------------------------- tabs
    const setTab = (next: DetailTab): void => {
      tab = next;
      for (const button of Array.from(tabBar.children)) {
        const el = button as HTMLElement;
        el.classList.toggle("is-active", el.dataset.tab === next);
      }
      overviewHost.classList.toggle("hidden", next !== "overview");
      logsHost.classList.toggle("hidden", next !== "logs");
      historyHost.classList.toggle("hidden", next !== "history");
      logsVisible = next === "logs";
      if (next === "logs") {
        if (this.readOnly) {
          this.ctx.toast.warning("Container logs require an administrator.");
          return;
        }
        if (!logUi) buildLogsUi();
        connectLogStream();
      } else {
        stopLogStream();
      }
      if (next === "history") void loadHistory();
    };
    const tabs: Array<{ id: DetailTab; label: string }> = this.readOnly
      ? [
          { id: "overview", label: "Overview" },
          { id: "history", label: "History" },
        ]
      : [
          { id: "overview", label: "Overview" },
          { id: "logs", label: "Logs" },
          { id: "history", label: "History" },
        ];
    for (const entry of tabs) {
      const btn = h("button", { class: `tab${entry.id === tab ? " is-active" : ""}`, type: "button", dataset: { tab: entry.id } }, entry.label);
      btn.addEventListener("click", () => setTab(entry.id));
      tabBar.appendChild(btn);
    }

    const renderStatus = (): void => {
      clear(statusLine);
      statusLine.append(
        h("span", { class: `badge ${statusClass(detail.status)}` }, statusLabel(detail.status)),
        h("span", { class: "mono small muted" }, detail.slug),
        h("span", { class: "small muted" }, `Updated ${relativeTime(detail.updatedAt)}`),
        detailLive,
      );
      if (detail.lastError) statusLine.appendChild(h("span", { class: "badge error" }, "last error"));
    };

    const renderOverview = (): void => {
      clear(overviewHost);
      if (detail.lastError) {
        overviewHost.appendChild(banner("error", "Last deployment error", detail.lastError));
      }
      const kv = h("dl", { class: "kv appmgmt-kv" });
      kv.append(
        h("dt", {}, "Status"),
        h("dd", {}, statusLabel(detail.status)),
        h("dt", {}, "Created"),
        h("dd", {}, formatDateTime(detail.createdAt)),
        h("dt", {}, "Updated"),
        h("dd", {}, formatDateTime(detail.updatedAt)),
      );
      overviewHost.appendChild(h("div", { class: "appmgmt-panel" }, kv));

      const resources = Array.isArray(detail.resources) ? detail.resources : [];
      if (resources.length > 0) {
        overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Resource usage"));
        const resourceRows = resources.map((resource) =>
          h(
            "tr",
            {},
            h("td", { class: "mono small" }, resource.name ?? "—"),
            h("td", { class: "num" }, formatPercent(resource.cpuPercent, 1)),
            h("td", { class: "num" }, formatBytes(resource.memoryBytes)),
            h("td", { class: "num small muted" }, formatBytes(resource.memoryLimitBytes)),
          ),
        );
        overviewHost.appendChild(
          h(
            "div",
            { class: "table-wrap appmgmt-mini-table" },
            h(
              "table",
              { class: "table" },
              h("thead", {}, h("tr", {}, h("th", {}, "Container"), h("th", { class: "num" }, "CPU"), h("th", { class: "num" }, "Memory"), h("th", { class: "num" }, "Limit"))),
              h("tbody", {}, ...resourceRows),
            ),
          ),
        );
      }

      const containers = detail.containers ?? [];
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, `Containers (${containers.length})`));
      if (containers.length === 0) {
        overviewHost.appendChild(h("div", { class: "small muted" }, "No containers reported. The application may not be deployed yet."));
      } else {
        const rows = containers.map((container) =>
          h(
            "tr",
            {},
            h("td", {}, container.name),
            h("td", { class: "mono small" }, container.image),
            h("td", {}, h("span", { class: `badge ${container.state === "running" ? "ok" : ""}` }, container.state)),
            h("td", { class: "small muted" }, portLabels(container.ports).join(", ") || "—"),
          ),
        );
        overviewHost.appendChild(
          h(
            "div",
            { class: "table-wrap appmgmt-mini-table" },
            h(
              "table",
              { class: "table" },
              h("thead", {}, h("tr", {}, h("th", {}, "Name"), h("th", {}, "Image"), h("th", {}, "State"), h("th", {}, "Ports"))),
              h("tbody", {}, ...rows),
            ),
          ),
        );
      }

      const ports = publishedPorts(detail);
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Published ports"));
      overviewHost.appendChild(
        ports.length > 0
          ? h("div", { class: "appmgmt-chips" }, ...ports.map((port) => h("span", { class: "badge" }, port)))
          : h("div", { class: "small muted" }, "No published ports."),
      );

      const volumes = detail.volumes ?? [];
      overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Volumes"));
      overviewHost.appendChild(
        volumes.length > 0
          ? h(
              "ul",
              { class: "appmgmt-plain-list small" },
              ...volumes.slice(0, 50).map((volume) => h("li", { class: "mono" }, volume)),
            )
          : h("div", { class: "small muted" }, "No volumes declared."),
      );

      const env = detail.env ?? [];
      if (this.readOnly) {
        overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Environment"));
        const total = typeof detail.envCount === "number" ? detail.envCount : env.length;
        const secrets = typeof detail.secretCount === "number" ? detail.secretCount : 0;
        overviewHost.appendChild(
          h(
            "div",
            { class: "appmgmt-restricted-note small muted" },
            total > 0
              ? `${total} environment variable(s) defined (${secrets} secret). Values are visible to administrators only.`
              : "No environment variables are defined for this application.",
          ),
        );
      } else if (env.length > 0) {
        overviewHost.appendChild(h("h4", { class: "appmgmt-subhead" }, "Environment"));
        const envRowsEl = h("div", { class: "appmgmt-env-view" });
        for (const row of env.slice(0, 100)) {
          envRowsEl.appendChild(
            h(
              "div",
              { class: "appmgmt-env-view-row" },
              h("span", { class: "mono" }, row.key),
              h("span", { class: "mono muted" }, row.secret ? "•••••• (secret)" : row.value),
            ),
          );
        }
        overviewHost.appendChild(envRowsEl);
      }
    };

    const loadHistory = async (): Promise<void> => {
      clear(historyHost);
      historyHost.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading deployment history…"));
      try {
        const payload = await this.ctx.api.get<HistoryResponse | DeploymentRecord[]>(
          `/api/apps/${encodeURIComponent(detail.id)}/history`,
        );
        if (closed) return;
        const records = listOf<DeploymentRecord>(payload, "history");
        historyLoaded = true;
        clear(historyHost);
        if (records.length === 0) {
          historyHost.appendChild(h("div", { class: "small muted" }, "No deployments recorded yet."));
          return;
        }
        const timeline = h("ol", { class: "appmgmt-timeline" });
        for (const record of records.slice(0, 100)) {
          timeline.appendChild(
            h(
              "li",
              { class: `appmgmt-timeline-item is-${recordStatusClass(record.status)}` },
              h("div", { class: "appmgmt-timeline-dot" }),
              h(
                "div",
                { class: "appmgmt-timeline-body" },
                h(
                  "div",
                  { class: "appmgmt-timeline-head" },
                  h("span", { class: "appmgmt-timeline-action" }, record.action || "deployment"),
                  h("span", { class: `badge ${statusClass(record.status)}` }, record.status),
                  h("span", { class: "small muted" }, formatDateTime(record.createdAt)),
                ),
                record.detail ? h("div", { class: "small muted" }, record.detail) : null,
              ),
            ),
          );
        }
        if (this.readOnly) {
          historyHost.appendChild(
            h("div", { class: "appmgmt-restricted-note small muted" }, "Deployment error details are visible to administrators only."),
          );
        }
        historyHost.appendChild(timeline);
      } catch (error) {
        if (closed) return;
        clear(historyHost);
        renderLoadFailure(historyHost, error, {
          title: "Could not load deployment history",
          retry: () => void loadHistory(),
        });
      }
    };

    const refreshDetail = async (): Promise<void> => {
      detailController?.abort();
      detailController = this.lc.trackController(new AbortController());
      const request = detailController;
      try {
        const fresh = await this.ctx.api.get<AppDetail>(`/api/apps/${encodeURIComponent(appId)}`, undefined, {
          signal: request.signal,
        });
        if (request.signal.aborted || closed) return;
        detail = { ...detail, ...fresh };
      } catch (error) {
        if (isAbortError(error) || request.signal.aborted || closed) return;
        if (isUnavailableError(error)) {
          statusLine.appendChild(h("span", { class: "badge warn" }, unavailableReason(error)));
        }
        return;
      } finally {
        this.lc.releaseController(request);
      }
      renderStatus();
      if (tab === "overview") renderOverview();
    };

    // Live updates while the modal is open.
    const offChanged = this.realtime.on<AppChangedEvent>("app.changed", (data) => {
      if (!data || (data.appId && data.appId !== appId)) return;
      if (typeof data.status === "string") detail.status = normalizeStatus(data.status, detail.status);
      const containers = normalizeContainers(data.containers);
      if (containers) detail.containers = containers;
      detail.updatedAt = Date.now();
      renderStatus();
      if (tab === "overview") renderOverview();
      if (tab === "history") {
        historyLoaded = false;
        void loadHistory();
      }
      detailSync.run();
    });
    localCleanups.push(offChanged);
    const offResync = this.realtime.onResync(() => {
      detailSync.run();
      if (tab === "history") {
        historyLoaded = false;
        void loadHistory();
      }
      if (tab === "logs") {
        logAttempts = 0;
        connectLogStream();
      }
    });
    localCleanups.push(offResync);

    renderStatus();
    renderOverview();

    const close = modal({
      title: `Application — ${detail.name || detail.slug}`,
      body,
      wide: true,
      onClose: () => {
        closed = true;
        for (const cleanup of localCleanups) {
          try {
            cleanup();
          } catch {
            /* ignore */
          }
        }
        localCleanups.length = 0;
        stopLogStream();
        detailController?.abort();
        this.activeModalClose = null;
        this.detailAccessHandler = null;
        if (this.detailChangedHook) this.detailChangedHook = null;
      },
    });
    this.lc.add(close);
    this.activeModalClose = close;
    this.detailAccessHandler = (readOnly) => {
      if (readOnly) {
        if (tab === "logs") setTab("overview");
        const logsTab = tabBar.querySelector<HTMLElement>('[data-tab="logs"]');
        logsTab?.classList.add("hidden");
        stopLogStream();
        if (tab === "overview") {
          renderStatus();
          renderOverview();
        }
      }
    };

    // Expose partial updates to the list subscriber (single active modal).
    this.detailChangedHook = (data) => {
      if (data.appId && data.appId !== appId) return;
      if (typeof data.status === "string") detail.status = normalizeStatus(data.status, detail.status);
      const containers = normalizeContainers(data.containers);
      if (containers) detail.containers = containers;
      detail.updatedAt = Date.now();
      renderStatus();
      if (tab === "overview") renderOverview();
      detailSync.run();
    };

    // Load history lazily when the user opens the tab, but preload quietly so
    // a deployment event can refresh it in the background.
    void historyLoaded;
    setTab("overview");
  }

  // ----------------------------------------------------------------- delete

  private async openDeleteDialog(app: ManagedAppView): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Docker was changed." : "You have read-only access to Docker.",
      );
      return;
    }
    let removeVolumes = false;
    const typed = h("input", { class: "input", type: "text", placeholder: app.name, autocomplete: "off" });
    const validation = formError("");
    const keepRadio = h("input", { type: "radio", name: `delete-mode-${app.id}` });
    const purgeRadio = h("input", { type: "radio", name: `delete-mode-${app.id}` });
    keepRadio.checked = true;
    keepRadio.addEventListener("change", () => {
      removeVolumes = false;
    });
    purgeRadio.addEventListener("change", () => {
      removeVolumes = true;
    });
    const body = h(
      "div",
      { class: "col" },
      h("p", {}, `Delete the managed application “${app.name}”? This removes its containers and RAST configuration.`),
      h(
        "label",
        { class: "appmgmt-delete-option" },
        keepRadio,
        h(
          "span",
          {},
          h("span", { class: "appmgmt-delete-option-title" }, "Keep data (recommended)"),
          h("span", { class: "small muted" }, "Container volumes and bind-mount data stay on disk."),
        ),
      ),
      h(
        "label",
        { class: "appmgmt-delete-option is-danger" },
        purgeRadio,
        h(
          "span",
          {},
          h("span", { class: "appmgmt-delete-option-title" }, "Remove volumes"),
          h("span", { class: "small muted" }, "Deletes the application's Docker volumes. This cannot be undone."),
        ),
      ),
      h("p", { class: "small muted" }, `Type “${app.name}” to confirm.`),
      typed,
      validation,
    );
    modal({
      title: "Delete application",
      body,
      onClose: () => undefined,
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Delete application",
          variant: "danger",
          keepOpen: true,
          onClick: async (closeDialog) => {
            validation.textContent = "";
            if (typed.value.trim() !== app.name) {
              validation.textContent = `Type “${app.name}” exactly to confirm.`;
              typed.focus();
              return;
            }
            try {
              await this.ctx.api.del(`/api/apps/${encodeURIComponent(app.id)}`, {
                removeVolumes: removeVolumes ? "true" : "false",
              });
              this.ctx.toast.success(`Application “${app.name}” deleted${removeVolumes ? " (volumes removed)" : " (volumes kept)"}`);
              closeDialog();
              await Promise.all([this.loadApps(), this.loadContainers()]);
            } catch (error) {
              validation.textContent = errorMessage(error);
            }
          },
        },
      ],
    });
  }

  // ----------------------------------------------------------------- import

  /** Adopt containers as a managed RAST app. Resolves true once imported. */
  private async importContainers(ids: string[], label: string): Promise<boolean> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Docker was changed." : "You have read-only access to Docker.",
      );
      return false;
    }
    if (ids.length === 0) return false;
    const confirmed = await confirmDialog({
      title: "Manage as RAST app",
      message: `Turn “${label}” into a RAST app? RAST recreates ${ids.length === 1 ? "the container" : `its ${ids.length} containers`} from a Compose file it keeps, so you can redeploy and back it up. Volumes and data stay.`,
      confirmLabel: "Manage as app",
    });
    if (!confirmed) return false;
    try {
      await this.ctx.api.post("/api/docker/import", { containerIds: ids });
      this.ctx.toast.success(`${label} is now a RAST app`);
      await Promise.all([this.loadApps(), this.loadContainers()]);
      return true;
    } catch (error) {
      this.ctx.toast.error(`Import failed: ${errorMessage(error)}`);
      return false;
    }
  }
}

function plural(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}

function normalizeEngineContainer(value: EngineContainer): EngineContainer {
  return {
    ...value,
    ports: Array.isArray(value.ports) ? value.ports : [],
    mounts: Array.isArray(value.mounts) ? value.mounts : [],
    labels: value.labels && typeof value.labels === "object" ? value.labels : {},
    project: value.project ?? "",
    service: value.service ?? "",
    status: value.status ?? value.state ?? "",
    slug: value.slug ?? null,
    managed: Boolean(value.managed),
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function listOf<T>(payload: unknown, key = "items"): T[] {
  if (Array.isArray(payload)) return payload as T[];
  if (payload && typeof payload === "object") {
    const record = payload as Record<string, unknown>;
    const candidate = record[key] ?? record.items ?? record.containers;
    if (Array.isArray(candidate)) return candidate as T[];
  }
  return [];
}

function portLabel(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (value && typeof value === "object") {
    const mapping = value as PortMappingLike;
    if (typeof mapping.containerPort !== "number") return null;
    const protocol = mapping.protocol && mapping.protocol !== "tcp" ? `/${mapping.protocol}` : "";
    if (typeof mapping.hostPort === "number") return `${mapping.hostPort}:${mapping.containerPort}${protocol}`;
    return `${mapping.containerPort}${protocol}`;
  }
  return null;
}

function portLabels(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  const out: string[] = [];
  for (const value of values) {
    const label = portLabel(value);
    if (label && !out.includes(label)) out.push(label);
  }
  return out;
}

/** Published ports for an app: top-level list when present, else container mappings. */
function publishedPorts(app: { ports?: unknown; containers?: Array<{ ports?: unknown }> }): string[] {
  const direct = portLabels(app.ports);
  if (direct.length > 0) return direct;
  const out: string[] = [];
  for (const container of app.containers ?? []) {
    for (const label of portLabels(container.ports)) {
      if (!out.includes(label)) out.push(label);
    }
  }
  return out;
}

function normalizeContainers(value: unknown): ManagedApp["containers"] | undefined {
  if (!Array.isArray(value)) return undefined;
  const out: NonNullable<ManagedApp["containers"]> = [];
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const record = item as Record<string, unknown>;
    if (typeof record.name !== "string") continue;
    out.push({
      name: record.name,
      state: typeof record.state === "string" ? record.state : "",
      image: typeof record.image === "string" ? record.image : "",
      ports: Array.isArray(record.ports) ? portLabels(record.ports) : undefined,
    });
  }
  return out;
}

function normalizeStatus(value: unknown, fallback: ManagedApp["status"]): ManagedApp["status"] {
  switch (value) {
    case "running":
    case "stopped":
    case "partial":
    case "error":
    case "deploying":
    case "unknown":
      return value;
    default:
      return fallback;
  }
}

function statusLabel(status: string): string {
  switch (status) {
    case "running":
      return "running";
    case "stopped":
      return "stopped";
    case "partial":
      return "partially running";
    case "deploying":
      return "deploying";
    case "error":
      return "error";
    default:
      return status || "unknown";
  }
}

function statusClass(status: string): string {
  switch (status) {
    case "running":
    case "done":
      return "ok";
    case "partial":
    case "deploying":
    case "pending":
    case "running_deployment":
      return "warn";
    case "error":
    case "failed":
      return "error";
    case "stopped":
      return "";
    default:
      return "";
  }
}

function recordStatusClass(status: string): string {
  return status === "failed" || status === "error" ? "error" : status === "running" ? "warn" : "ok";
}

function validateAppName(name: string): string | null {
  if (!name) return "Application name is required";
  if (name.length > 63) return "Application name must be 63 characters or fewer";
  if (!/^[a-z0-9][a-z0-9_-]*$/.test(name)) {
    return "Use lowercase letters, digits, dashes and underscores; must start with a letter or digit";
  }
  return null;
}

function validateCompose(compose: string): string | null {
  const text = compose.trim();
  if (!text) return "Compose YAML is required";
  if (!/^\s*services\s*:/m.test(text)) return "Compose YAML must define a services: section";
  if (/\t/.test(text)) return "Compose YAML must not contain tab characters — use spaces for indentation";
  return null;
}

// ---------------------------------------------------------------------------

export const applicationsApp: AppDefinition = {
  id: "applications",
  name: "Docker",
  icon: "docker",
  description: "Deploy and manage Docker containers",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1040, h: 680 },
  minSize: { w: 380, h: 320 },
  async mount(ctx: AppContext) {
    const app = new ApplicationsApp(ctx);
    await app.start();
  },
};
