// Backup Center: archive list, manual backup creation with live job progress,
// schedule editor, integrity verification and an offline restore guide.
// OWNER: teammate `frontend-apps`.

import type { AppDefinition, AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { debounceTask, getRealtime, type RealtimeStore } from "../../core/realtime";
import { createLiveIndicator, NoticeBar, type LiveIndicator } from "../../components/live";
import {
  formatBytes,
  formatDate,
  formatDateTime,
  formatDuration,
  relativeTime,
} from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { Backup, BackupSchedule, Permissions } from "../../core/types";
import { confirmDialog, errorMessage, modal } from "../../core/ui";
import { AppLifecycle, isAbortError } from "../../components/lifecycle";
import { watchJob, jobProgressFraction, jobProgressLabel } from "../../components/jobs";
import { Pager } from "../../components/pager";
import {
  banner,
  emptyBlock,
  formError,
  isUnavailableError,
  progressBar,
  renderLoadFailure,
  unavailableReason,
} from "../../components/states";

interface BackupListResponse {
  items?: BackupRow[];
  total?: number;
  page?: number;
  perPage?: number;
}

/** Read-only viewers receive metadata only (docs/AUDIT-NOTES.md F1). */
interface BackupRow extends Backup {
  downloadable?: boolean;
}

interface CreateBackupResponse {
  jobId?: string;
  backup?: Backup;
}

interface VerifyResponse {
  ok?: boolean;
  valid?: boolean;
  reason?: string;
  message?: string;
  sha256?: string;
}

type RestoreStep = string | { title?: string; text?: string; message?: string; detail?: string; command?: string | null };

interface RestoreGuideResponse {
  steps?: RestoreStep[];
  warning?: string;
  archive?: string;
  /** Schema/rollback caveat for administrators (F3). */
  rollback?: string;
}

/** `job.*` payload subset for backup jobs. */
interface BackupJobEvent {
  id?: string;
  kind?: string;
  progress?: number;
  total?: number;
  message?: string;
  error?: string;
}

/** `backup.changed` payload. */
interface BackupChangedEvent {
  reason?: string;
  id?: string;
}

/** Local aliases for the shell-carried access policy (docs/GROUPS.md). */
type PermissionsState = Permissions;

interface PermissionsCarrier {
  permissions?: Permissions | null;
  user?: { id?: number; role?: string } | null;
}

const SCOPES: Array<{ id: string; label: string; description: string }> = [
  {
    id: "db",
    label: "Database",
    description: "Users, settings, shares metadata and activity log (SQLite snapshot).",
  },
  {
    id: "config",
    label: "Configuration",
    description: "RAST configuration files, environment file and TLS material references.",
  },
  {
    id: "users",
    label: "User files",
    description: "Per-user home directories under the storage root.",
  },
  {
    id: "apps",
    label: "Applications",
    description: "Managed application Compose definitions, environment and bind data.",
  },
];

const FREQUENCIES: Array<{ value: BackupSchedule["frequency"]; label: string }> = [
  { value: "hourly", label: "Hourly" },
  { value: "daily", label: "Daily" },
  { value: "weekly", label: "Weekly" },
];

const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

class BackupCenterApp {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;
  private readonly bannerHost: HTMLElement;
  private readonly scheduleHost: HTMLElement;
  private readonly listHost: HTMLElement;
  private readonly createButton: HTMLButtonElement;
  private readonly scheduleButton: HTMLButtonElement;
  private readonly pager: Pager;
  private readonly contentEl: HTMLElement;
  private readonly jobStatusHost: HTMLElement;
  private readonly realtime: RealtimeStore;
  private readonly liveIndicator: LiveIndicator;
  private readonly notice: NoticeBar;
  private readonly syncList: { run: () => void; cancel: () => void };
  private readonly syncSchedule: { run: () => void; cancel: () => void };

  private backups: BackupRow[] = [];
  private total = 0;
  private page = 1;
  private perPage = 25;
  private loading = true;
  private listError: unknown = null;
  private schedule: BackupSchedule | null = null;
  private scheduleError: unknown = null;
  private unavailable: unknown = null;
  private jobStatus: { jobId: string; progress: number; total: number; message: string } | null = null;
  private scheduleEditorConflict: HTMLElement | null = null;
  private scheduleEditorDirty = false;
  private readonly accessCheck: { run: () => void; cancel: () => void };
  private localPermissions: PermissionsState | null = null;
  private readOnly = false;
  private denied = false;
  private lastAccessSignature = "";
  private lastPermissionsRef: unknown = undefined;
  private activeModalClose: (() => void) | null = null;
  private disposed = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.realtime = getRealtime(ctx.events);
    this.notice = new NoticeBar(this.lc);
    this.liveIndicator = createLiveIndicator(this.realtime, this.lc);
    this.syncList = debounceTask(() => void this.loadBackups(), 250);
    this.syncSchedule = debounceTask(() => void this.loadSchedule(), 300);
    this.lc.add(() => {
      this.syncList.cancel();
      this.syncSchedule.cancel();
    });

    this.createButton = h("button", { class: "btn btn-primary btn-sm", type: "button" }, iconSvg("plus", 14), document.createTextNode("Create backup"));
    this.createButton.addEventListener("click", () => void this.openCreateDialog());
    this.scheduleButton = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("clock", 14), document.createTextNode("Schedule"));
    this.scheduleButton.addEventListener("click", () => void this.openScheduleEditor());
    const toolbar = h(
      "div",
      { class: "app-toolbar", role: "toolbar", ariaLabel: "Backup actions" },
      this.createButton,
      this.scheduleButton,
      h("div", { class: "app-toolbar-spacer" }),
      this.liveIndicator.el,
    );

    this.bannerHost = h("div", { class: "backup-banners" });
    this.jobStatusHost = h("div", { class: "backup-job-status hidden", role: "status", ariaLive: "polite" });
    this.scheduleHost = h("div", { class: "backup-schedule" });
    this.listHost = h("div", { class: "backup-list" });
    this.pager = new Pager({
      onChange: (page, perPage) => {
        this.page = page;
        this.perPage = perPage;
        void this.loadBackups();
      },
      label: "Backup pages",
      perPageOptions: [10, 25, 50],
    });
    this.contentEl = h(
      "div",
      { class: "app-content backup-content" },
      this.bannerHost,
      this.jobStatusHost,
      this.scheduleHost,
      this.listHost,
    );
    ctx.root.classList.add("backup-app");
    ctx.root.append(toolbar, this.notice.el, this.contentEl);

    this.lc.add(() => {
      this.disposed = true;
      this.scheduleEditorConflict = null;
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
    this.applyAccessPolicy();
  }

  // ----------------------------------------------------------- access policy


  private readGrant(): { allowed: boolean; readOnly: boolean } {
    const state = this.ctx.store.get() as unknown as PermissionsCarrier;
    const grant = state.permissions?.apps?.backup ?? this.localPermissions?.apps?.backup;
    if (grant && typeof grant.allowed === "boolean") {
      return { allowed: grant.allowed, readOnly: grant.allowed && Boolean(grant.readOnly) };
    }
    const admin = this.ctx.store.isAdmin();
    return { allowed: admin, readOnly: false };
  }

  private async onAccessChanged(): Promise<void> {
    if (this.disposed) return;
    const state = this.ctx.store.get() as unknown as PermissionsCarrier;
    if (!state.permissions) {
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
      this.activeModalClose = null;
      this.renderDenied();
      return;
    }
    if (wasDenied) void this.loadBackups();
    this.render();
  }

  private renderDenied(): void {
    clear(this.bannerHost);
    clear(this.jobStatusHost);
    clear(this.scheduleHost);
    clear(this.listHost);
    this.bannerHost.appendChild(
      h(
        "div",
        { class: "app-access-state", role: "alert" },
        h("div", { class: "app-access-title" }, "Backup Center access changed"),
        h(
          "div",
          { class: "app-access-detail" },
          "Your access to Backup Center was revoked by a group policy change. Ask an administrator if you need access again.",
        ),
        (() => {
          const btn = h("button", { class: "btn btn-ghost", type: "button" }, "Close window");
          btn.addEventListener("click", () => this.ctx.window.close());
          return btn;
        })(),
      ),
    );
    this.createButton.classList.add("hidden");
    this.scheduleButton.classList.add("hidden");
    this.notice.show("Your access to Backup Center was changed.", { kind: "warning" });
    this.ctx.toast.warning("Your access to Backup Center was changed.");
  }

  // -------------------------------------------------------------- realtime

  private subscribeRealtime(): void {
    this.lc.add(
      this.realtime.on<BackupChangedEvent>("backup.changed", (data) => {
        if (this.disposed) return;
        this.syncList.run();
        this.syncSchedule.run();
        if (data?.reason && this.scheduleEditorConflict) {
          this.signalScheduleConflict(`The backup schedule changed on the server (${data.reason}). Review before saving.`);
        }
      }),
    );
    this.lc.add(
      this.realtime.on("backup.done", () => {
        if (this.disposed) return;
        this.ctx.toast.success("A backup finished");
        this.jobStatus = null;
        this.renderJobStatus();
        this.syncList.run();
        this.syncSchedule.run();
      }),
    );
    for (const kind of ["job.queued", "job.start", "job.progress"]) {
      this.lc.add(this.realtime.on<BackupJobEvent>(kind, (data) => this.onBackupJob(kind, data)));
    }
    for (const kind of ["job.done", "job.failed", "job.cancelled"]) {
      this.lc.add(this.realtime.on<BackupJobEvent>(kind, (data) => this.onBackupJob(kind, data)));
    }
    this.lc.add(
      this.realtime.onResync(() => {
        if (this.disposed) return;
        this.syncList.run();
        this.syncSchedule.run();
      }),
    );
  }

  private onBackupJob(kind: string, data: BackupJobEvent | null): void {
    if (this.disposed || !data || typeof data !== "object") return;
    const jobKind = typeof data.kind === "string" ? data.kind : "";
    if (!jobKind.startsWith("backup")) return;
    const jobId = typeof data.id === "string" ? data.id : "";
    if (kind === "job.progress" || kind === "job.start" || kind === "job.queued") {
      if (!jobId) return;
      this.jobStatus = {
        jobId,
        progress: typeof data.progress === "number" ? data.progress : 0,
        total: typeof data.total === "number" ? data.total : 0,
        message: typeof data.message === "string" ? data.message : "",
      };
      this.renderJobStatus();
      return;
    }
    this.jobStatus = null;
    this.renderJobStatus();
    if (kind === "job.failed") {
      this.ctx.toast.error(`Backup failed: ${data.error ?? "unknown error"}`);
    } else if (kind === "job.done") {
      this.ctx.toast.success("Backup finished");
    }
    this.syncList.run();
    this.syncSchedule.run();
  }

  private renderJobStatus(): void {
    clear(this.jobStatusHost);
    const status = this.jobStatus;
    if (!status) {
      this.jobStatusHost.classList.add("hidden");
      return;
    }
    this.jobStatusHost.classList.remove("hidden");
    const fraction = status.total > 0 ? Math.min(1, status.progress / status.total) : 0;
    const bar = h("div", { class: "progress backup-job-bar" });
    const fill = h("div", { class: "progress-bar" });
    fill.style.width = `${(fraction * 100).toFixed(1)}%`;
    bar.appendChild(fill);
    this.jobStatusHost.append(
      h("span", { class: "backup-job-label" }, "Backup job running"),
      bar,
      h(
        "span",
        { class: "small muted backup-job-message" },
        status.total > 0
          ? `${Math.min(status.progress, status.total)} / ${status.total}${status.message ? ` · ${status.message}` : ""}`
          : status.message || "Queued…",
      ),
    );
  }

  private signalScheduleConflict(message: string): void {
    const target = this.scheduleEditorConflict;
    if (!target || !this.scheduleEditorDirty) return;
    target.textContent = message;
    target.classList.remove("hidden");
    this.notice.show(message, { kind: "warning" });
  }

  async start(): Promise<void> {
    this.ctx.setTitle("Backup Center");
    await this.hydratePermissions();
    this.applyAccessPolicy();
    if (this.denied) return;
    this.render();
    await Promise.all([this.loadBackups(), this.loadSchedule()]);
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

  private async loadBackups(): Promise<void> {
    if (this.disposed || this.denied) return;
    try {
      const payload = await this.ctx.api.get<BackupListResponse>("/api/backups", {
        page: this.page,
        perPage: this.perPage,
      });
      if (this.disposed) return;
      const items = Array.isArray(payload?.items) ? payload.items : [];
      this.backups = items as BackupRow[];
      this.total = typeof payload?.total === "number" ? payload.total : items.length;
      this.page = typeof payload?.page === "number" ? payload.page : this.page;
      this.listError = null;
      this.unavailable = null;
      this.pager.set(this.page, this.perPage, this.total);
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.backups = [];
      this.total = 0;
      this.listError = error;
      if (isUnavailableError(error)) this.unavailable = error;
    } finally {
      this.loading = false;
      if (!this.disposed) this.render();
    }
  }

  private async loadSchedule(): Promise<void> {
    if (this.denied) return;
    try {
      const payload = await this.ctx.api.get<{ schedule?: BackupSchedule | null } | BackupSchedule>(
        "/api/backups/schedule",
      );
      if (this.disposed) return;
      const wrapped = payload && typeof payload === "object" && "schedule" in payload ? payload.schedule : null;
      this.schedule = wrapped ?? (payload as BackupSchedule | null);
      this.scheduleError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.scheduleError = error;
    }
    if (!this.disposed) this.render();
  }

  // ------------------------------------------------------------- rendering

  private render(): void {
    if (this.disposed) return;
    if (this.denied) {
      this.renderDenied();
      return;
    }
    const scrollTop = this.contentEl?.scrollTop ?? 0;
    this.renderBanners();
    this.renderSchedule();
    this.renderList();
    this.createButton.classList.toggle("hidden", this.readOnly);
    this.scheduleButton.classList.toggle("hidden", this.readOnly);
    this.createButton.disabled = this.unavailable !== null;
    this.createButton.title = this.unavailable !== null ? "Backup creation is not available on this server yet" : "";
    this.scheduleButton.disabled = this.unavailable !== null;
    if (this.contentEl) this.contentEl.scrollTop = scrollTop;
  }

  private renderBanners(): void {
    clear(this.bannerHost);
    if (this.readOnly) {
      this.bannerHost.appendChild(
        banner(
          "info",
          "Read-only access",
          "A group policy grants you read-only access to Backup Center. You can see when backups ran, their size, scope and status. Archives contain account data and are available to administrators only, so file names, checksums, downloads and the restore guide are hidden. Creating, deleting, verifying or scheduling backups requires an administrator.",
        ),
      );
    }
    if (this.unavailable) {
      this.bannerHost.appendChild(
        banner(
          "warning",
          "Backup Center is not available yet",
          unavailableReason(this.unavailable),
        ),
      );
    } else if (this.scheduleError && isUnavailableError(this.scheduleError)) {
      this.bannerHost.appendChild(
        banner("info", "Schedule API unavailable", unavailableReason(this.scheduleError)),
      );
    }
    const running = this.backups.find((backup) => backup.status === "running");
    if (running) {
      this.bannerHost.appendChild(
        banner(
          "info",
          "A backup is running",
          `“${running.fileName || running.id}” is being written. Progress updates automatically when the job finishes.`,
        ),
      );
    }
  }

  private renderSchedule(): void {
    clear(this.scheduleHost);
    const card = h("div", { class: "backup-schedule-card" });
    const head = h(
      "div",
      { class: "backup-schedule-head" },
      h("h3", { class: "backup-section-title" }, "Backup schedule"),
      this.schedule
        ? h("span", { class: `badge ${this.schedule.enabled ? "ok" : ""}` }, this.schedule.enabled ? "enabled" : "disabled")
        : null,
    );
    card.appendChild(head);

    if (this.scheduleError) {
      card.appendChild(
        h(
          "div",
          { class: "small muted" },
          isUnavailableError(this.scheduleError)
            ? `Schedule unavailable: ${unavailableReason(this.scheduleError)}`
            : `Could not load the schedule: ${errorMessage(this.scheduleError)}`,
        ),
      );
    } else if (!this.schedule) {
      card.appendChild(h("div", { class: "small muted" }, "Loading schedule…"));
    } else {
      const schedule = this.schedule;
      const kv = h("dl", { class: "kv backup-schedule-kv" });
      kv.append(
        h("dt", {}, "Frequency"),
        h("dd", {}, frequencyLabel(schedule.frequency)),
        h("dt", {}, "Time"),
        h("dd", {}, schedule.frequency === "hourly" ? "Every hour" : timeLabel(schedule.timeOfDay)),
      );
      if (schedule.frequency === "weekly") {
        kv.append(h("dt", {}, "Weekday"), h("dd", {}, weekDayLabel(schedule.weekday)));
      }
      kv.append(
        h("dt", {}, "Retention"),
        h("dd", {}, `${schedule.retentionCount} archive(s)`),
        h("dt", {}, "Scopes"),
        h("dd", { class: "backup-scope-list" }, ...scopeBadges(schedule.scopes)),
        h("dt", {}, "Last run"),
        h("dd", {}, schedule.lastRunAt ? `${formatDateTime(schedule.lastRunAt)} (${relativeTime(schedule.lastRunAt)})` : "never"),
        h("dt", {}, "Next run"),
        h(
          "dd",
          {},
          schedule.enabled
            ? schedule.nextRunAt > 0
              ? `${formatDateTime(schedule.nextRunAt)} (${relativeFuture(schedule.nextRunAt)})`
              : "not scheduled yet"
            : "schedule disabled",
        ),
      );
      card.appendChild(kv);
      card.appendChild(
        h(
          "div",
          { class: "backup-retention-note small muted" },
          `Retention keeps the ${schedule.retentionCount} newest scheduled archives and prunes older ones automatically. Manual backups created with “Create backup” are not removed by retention.`,
        ),
      );
    }

    const actions = h("div", { class: "backup-schedule-actions" });
    const edit = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, "Edit schedule");
    edit.addEventListener("click", () => void this.openScheduleEditor());
    const explain = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, "How retention works");
    explain.addEventListener("click", () => this.openRetentionHelp());
    if (!this.readOnly) actions.append(edit);
    actions.append(explain);
    card.appendChild(actions);
    this.scheduleHost.appendChild(card);
  }

  private renderList(): void {
    clear(this.listHost);
    const head = h(
      "div",
      { class: "backup-list-head" },
      h("h3", { class: "backup-section-title" }, "Archives"),
      h("span", { class: "small muted" }, `${this.total.toLocaleString()} archive(s)`),
    );
    this.listHost.appendChild(head);

    if (this.loading) {
      this.listHost.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading backups…"));
      return;
    }
    if (this.listError) {
      renderLoadFailure(this.listHost, this.listError, {
        title: "Could not load backups",
        retry: () => {
          this.loading = true;
          this.renderList();
          void this.loadBackups();
        },
      });
      return;
    }
    if (this.backups.length === 0) {
      const create = h(
        "button",
        { class: "btn btn-primary", type: "button", disabled: this.unavailable !== null },
        "Create your first backup",
      );
      if (this.readOnly) create.classList.add("hidden");
      create.addEventListener("click", () => void this.openCreateDialog());
      this.listHost.appendChild(
        emptyBlock(
          "No backups yet",
          "Create a manual archive now, or configure the schedule to protect the database and configuration automatically.",
          create,
        ),
      );
      return;
    }

    const tableHead = h(
      "tr",
      {},
      h("th", {}, "Archive"),
      h("th", { class: "backup-col-created" }, "Created"),
      h("th", { class: "num backup-col-size" }, "Size"),
      h("th", { class: "backup-col-scope" }, "Scope"),
      h("th", {}, "Status"),
      h("th", { class: "backup-col-actions" }, "Actions"),
    );
    const body = h("tbody");
    for (const backup of this.backups) body.appendChild(this.renderRow(backup));
    this.listHost.appendChild(
      h("div", { class: "table-wrap backup-table-wrap" }, h("table", { class: "table backup-table" }, h("thead", {}, tableHead), body)),
    );
    this.listHost.appendChild(h("div", { class: "backup-footer" }, h("div", { class: "small muted" }), this.pager.el));
  }

  private renderRow(backup: BackupRow): HTMLTableRowElement {
    const restricted = this.readOnly || backup.downloadable === false;
    const displayName = restricted ? `Backup ${backup.id.slice(0, 8)}` : backup.fileName || backup.id;
    const name = h("div", { class: "backup-file-cell" }, h("div", { class: "backup-file-name" }, displayName));
    if (restricted) {
      name.appendChild(
        h("div", { class: "small muted" }, "Archive details are available to administrators only."),
      );
    }
    const checksum = h(
      "div",
      { class: "backup-checksum mono small muted", title: backup.sha256 || "no checksum" },
      backup.sha256 ? `sha256 ${backup.sha256.slice(0, 16)}…` : "no checksum",
    );
    if (!restricted) {
      if (backup.sha256) {
        const copy = h("button", { class: "backup-copy", type: "button", ariaLabel: "Copy checksum", title: "Copy checksum" });
        copy.appendChild(iconSvg("copy", 12));
        copy.addEventListener("click", () => void this.copyChecksum(backup.sha256));
        checksum.appendChild(copy);
      }
      name.appendChild(checksum);
      if (backup.note) name.appendChild(h("div", { class: "small muted backup-note" }, backup.note));
    }
    if (backup.error) name.appendChild(h("div", { class: "small backup-row-error" }, backup.error));

    const actions = h("div", { class: "backup-row-actions" });
    const iconAction = (icon: string, label: string, onClick: () => void, disabled = false, danger = false) => {
      const btn = h(
        "button",
        {
          class: `appmgmt-icon-btn${danger ? " is-danger" : ""}`,
          type: "button",
          ariaLabel: label,
          title: label,
          disabled,
        },
      );
      btn.appendChild(iconSvg(icon, 15));
      btn.addEventListener("click", onClick);
      return btn;
    };
    const done = backup.status === "done";
    if (!restricted) {
      actions.append(iconAction("download", "Download archive", () => this.download(backup), !done));
      if (!this.readOnly) {
        actions.append(iconAction("shield", "Verify integrity", () => void this.verify(backup), !done));
      }
      actions.append(iconAction("info", "Restore guide", () => this.openRestoreGuide(backup)));
    }
    if (!this.readOnly && !restricted) {
      actions.append(iconAction("trash", "Delete archive", () => this.deleteBackup(backup), false, true));
    }

    return h(
      "tr",
      {},
      h("td", {}, name),
      h("td", { class: "backup-col-created", title: formatDateTime(backup.createdAt) }, relativeTime(backup.createdAt)),
      h("td", { class: "num backup-col-size" }, formatBytes(backup.sizeBytes)),
      h("td", { class: "backup-col-scope" }, h("div", { class: "backup-scope-list" }, ...scopeBadges(backup.scope))),
      h("td", {}, h("span", { class: `badge ${backupStatusClass(backup.status)}` }, backup.status || "unknown")),
      h("td", { class: "backup-col-actions" }, actions),
    );
  }

  // --------------------------------------------------------------- actions

  private async copyChecksum(checksum: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(checksum);
      this.ctx.toast.success("Checksum copied");
    } catch {
      this.ctx.toast.warning("Could not access the clipboard; select the checksum manually.");
    }
  }

  private download(backup: BackupRow): void {
    if (this.readOnly || backup.downloadable === false) {
      this.ctx.toast.warning("Backup archives contain account data and are available to administrators only.");
      return;
    }
    const anchor = document.createElement("a");
    anchor.href = `/api/backups/${encodeURIComponent(backup.id)}/download`;
    anchor.download = backup.fileName || `backup-${backup.id}.tar.gz`;
    anchor.rel = "noopener";
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  }

  private async verify(backup: Backup): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Backup Center was changed." : "You have read-only access to Backup Center.",
      );
      return;
    }
    const body = h("div", { class: "col" }, h("p", {}, `Verifying “${backup.fileName}” checks integrity without modifying the archive.`));
    const status = h("div", { class: "small muted" }, "Running integrity check…");
    body.appendChild(status);
    const closeDialog = modal({
      title: "Verify backup",
      body,
      actions: [{ label: "Close", variant: "ghost", onClick: () => {} }],
    });
    this.lc.add(closeDialog);
    try {
      const result = await this.ctx.api.post<VerifyResponse>(`/api/backups/${encodeURIComponent(backup.id)}/verify`);
      if (this.disposed) return;
      const ok = result?.ok ?? result?.valid ?? false;
      status.textContent = ok
        ? `Integrity verified${result?.sha256 ? ` — sha256 ${result.sha256}` : ""}.`
        : `Verification failed: ${result?.reason || result?.message || "the archive did not pass the integrity check."}`;
      status.className = ok ? "small backup-verify-ok" : "small backup-verify-fail";
      if (ok) this.ctx.toast.success("Backup integrity verified");
      else this.ctx.toast.error("Backup verification failed");
    } catch (error) {
      if (this.disposed) return;
      status.textContent = `Verification failed: ${errorMessage(error)}`;
      status.className = "small backup-verify-fail";
      this.ctx.toast.error(`Verification failed: ${errorMessage(error)}`);
    }
  }

  private async deleteBackup(backup: Backup): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Backup Center was changed." : "You have read-only access to Backup Center.",
      );
      return;
    }
    const confirmed = await confirmDialog({
      title: "Delete backup archive",
      message: `Delete “${backup.fileName || backup.id}” and its archive file? This cannot be undone. Other backups are not affected.`,
      confirmLabel: "Delete archive",
      danger: true,
      requireTyped: "DELETE",
    });
    if (!confirmed) return;
    try {
      await this.ctx.api.del(`/api/backups/${encodeURIComponent(backup.id)}`);
      this.ctx.toast.success("Backup deleted");
      await this.loadBackups();
    } catch (error) {
      this.ctx.toast.error(`Could not delete backup: ${errorMessage(error)}`);
    }
  }

  // --------------------------------------------------------- create dialog

  private async openCreateDialog(): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Backup Center was changed." : "You have read-only access to Backup Center.",
      );
      return;
    }
    if (this.unavailable) {
      this.ctx.toast.warning("Backup creation is not available on this server yet.");
      return;
    }
    const selected = new Set<string>(["db"]);
    const scopeRows = SCOPES.map((scope) => {
      const checkbox = h("input", { type: "checkbox", ariaLabel: scope.label });
      checkbox.checked = selected.has(scope.id);
      checkbox.addEventListener("change", () => {
        if (checkbox.checked) selected.add(scope.id);
        else selected.delete(scope.id);
      });
      return h(
        "label",
        { class: "backup-scope-option" },
        checkbox,
        h("span", {}, h("span", { class: "backup-scope-option-title" }, scope.label), h("span", { class: "small muted" }, scope.description)),
      );
    });
    const noteInput = h("textarea", {
      class: "textarea backup-note-input",
      placeholder: "Optional note (e.g. before upgrading to 2.x)",
      rows: 2,
    });
    noteInput.maxLength = 200;
    const validation = formError("");
    const progress = progressBar();
    progress.el.classList.add("hidden");
    const progressLabel = h("div", { class: "small muted" });
    const body = h(
      "div",
      { class: "col" },
      h("p", { class: "small muted" }, "Select what to include. The archive is written to the backup directory on the server; download it afterwards from the list."),
      h("div", { class: "backup-scope-options" }, ...scopeRows),
      h(
        "label",
        { class: "field" },
        h("span", { class: "field-label" }, "Note"),
        noteInput,
        h("span", { class: "field-hint" }, "Up to 200 characters."),
      ),
      progress.el,
      progressLabel,
      validation,
    );
    let submitting = false;
    const closeCreate = modal({
      title: "Create backup",
      body,
      wide: true,
      onClose: () => {
        this.activeModalClose = null;
      },
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Start backup",
          variant: "primary",
          keepOpen: true,
          onClick: async (closeDialog) => {
            if (submitting) return;
            validation.textContent = "";
            if (selected.size === 0) {
              validation.textContent = "Select at least one scope to back up.";
              return;
            }
            submitting = true;
            progress.el.classList.remove("hidden");
            progress.update(0.05);
            progressLabel.textContent = "Queuing backup job…";
            try {
              const response = await this.ctx.api.post<CreateBackupResponse>("/api/backups", {
                scope: [...selected],
                note: noteInput.value.trim(),
              });
              if (response?.jobId) {
                const outcome = await watchJob(this.ctx, this.lc, response.jobId, (update) => {
                  progress.update(jobProgressFraction(update));
                  progressLabel.textContent = jobProgressLabel(update) || "Creating backup…";
                });
                if (outcome.status === "done") {
                  progress.update(1);
                  this.ctx.toast.success("Backup created");
                  closeDialog();
                  await this.loadBackups();
                  return;
                }
                progress.setError(true);
                validation.textContent =
                  outcome.status === "failed" ? `Backup failed: ${outcome.error ?? "unknown error"}` : "Backup was cancelled.";
                progressLabel.textContent = "";
                submitting = false;
                return;
              }
              this.ctx.toast.success("Backup requested");
              closeDialog();
              await this.loadBackups();
            } catch (error) {
              progress.setError(true);
              validation.textContent = `Backup failed: ${errorMessage(error)}`;
              progressLabel.textContent = "";
              submitting = false;
            }
          },
        },
      ],
    });
    this.activeModalClose = closeCreate;
  }

  // ------------------------------------------------------- schedule editor

  private async openScheduleEditor(): Promise<void> {
    if (this.denied || this.readOnly) {
      this.ctx.toast.warning(
        this.denied ? "Your access to Backup Center was changed." : "You have read-only access to Backup Center.",
      );
      return;
    }
    if (this.unavailable) {
      this.ctx.toast.warning("The backup schedule API is not available on this server yet.");
      return;
    }
    if (this.scheduleError) {
      this.ctx.toast.error(`Could not load the current schedule: ${errorMessage(this.scheduleError)}`);
      return;
    }
    const current = this.schedule;
    if (!current) return;

    const enabled = h("input", { type: "checkbox" });
    enabled.checked = Boolean(current.enabled);
    const frequency = h("select", { class: "select", ariaLabel: "Backup frequency" });
    for (const entry of FREQUENCIES) {
      const option = h("option", { value: entry.value }, entry.label);
      if (entry.value === current.frequency) option.selected = true;
      frequency.appendChild(option);
    }
    const time = h("input", { class: "input", type: "time", value: normalizeTime(current.timeOfDay) });
    const weekday = h("select", { class: "select", ariaLabel: "Weekday" });
    WEEKDAYS.forEach((label, index) => {
      const option = h("option", { value: String(index) }, label);
      if (index === current.weekday) option.selected = true;
      weekday.appendChild(option);
    });
    const retention = h("input", { class: "input", type: "number", min: "1", max: "365", step: "1", value: String(current.retentionCount || 7) });
    const scopeChecks = SCOPES.map((scope) => {
      const checkbox = h("input", { type: "checkbox", ariaLabel: scope.label });
      checkbox.checked = current.scopes.includes(scope.id);
      return { scope, checkbox };
    });
    const validation = formError("");
    const conflict = h("div", { class: "realtime-conflict hidden", role: "status", ariaLive: "polite" });
    this.scheduleEditorConflict = conflict;
    this.scheduleEditorDirty = false;
    const markDirty = (): void => {
      this.scheduleEditorDirty = true;
    };
    for (const control of [enabled, frequency, time, weekday, retention, ...scopeChecks.map((entry) => entry.checkbox)]) {
      control.addEventListener("change", markDirty);
      control.addEventListener("input", markDirty);
    }
    const timeField = h("label", { class: "field backup-time-field" }, h("span", { class: "field-label" }, "Time of day"), time);
    const weekdayField = h("label", { class: "field backup-weekday-field" }, h("span", { class: "field-label" }, "Weekday"), weekday);
    const syncVisibility = () => {
      const freq = frequency.value;
      timeField.classList.toggle("hidden", freq === "hourly");
      weekdayField.classList.toggle("hidden", freq !== "weekly");
    };
    frequency.addEventListener("change", syncVisibility);
    syncVisibility();

    const body = h(
      "div",
      { class: "col" },
      h(
        "label",
        { class: "checkbox backup-enable-row" },
        enabled,
        h("span", {}, "Enable scheduled backups"),
      ),
      h(
        "div",
        { class: "backup-form-grid" },
        h("label", { class: "field" }, h("span", { class: "field-label" }, "Frequency"), frequency),
        timeField,
        weekdayField,
        h(
          "label",
          { class: "field" },
          h("span", { class: "field-label" }, "Retention count"),
          retention,
          h("span", { class: "field-hint" }, "Keep this many archives; older scheduled archives are pruned."),
        ),
      ),
      h(
        "div",
        { class: "field" },
        h("span", { class: "field-label" }, "Scopes"),
        h(
          "div",
          { class: "backup-scope-options" },
          ...scopeChecks.map(({ scope, checkbox }) =>
            h(
              "label",
              { class: "backup-scope-option" },
              checkbox,
              h("span", {}, h("span", { class: "backup-scope-option-title" }, scope.label), h("span", { class: "small muted" }, scope.description)),
            ),
          ),
        ),
      ),
      h(
        "div",
        { class: "backup-retention-note small muted" },
        "Scheduled backups run server-side. This page shows the next scheduled run after saving; RAST never triggers an online restore from the browser.",
      ),
      conflict,
      validation,
    );

    const closeSchedule = modal({
      title: "Backup schedule",
      body,
      wide: true,
      onClose: () => {
        this.scheduleEditorConflict = null;
        this.scheduleEditorDirty = false;
        this.activeModalClose = null;
      },
      actions: [
        { label: "Cancel", variant: "ghost", onClick: () => {} },
        {
          label: "Save schedule",
          variant: "primary",
          keepOpen: true,
          onClick: async (closeDialog) => {
            validation.textContent = "";
            const scopes = scopeChecks.filter((entry) => entry.checkbox.checked).map((entry) => entry.scope.id);
            if (scopes.length === 0) {
              validation.textContent = "Select at least one scope.";
              return;
            }
            const retentionValue = Number(retention.value);
            if (!Number.isInteger(retentionValue) || retentionValue < 1 || retentionValue > 365) {
              validation.textContent = "Retention must be a whole number between 1 and 365.";
              return;
            }
            const timeValue = frequency.value === "hourly" ? normalizeTime(time.value || "00:00") : time.value;
            if (!/^\d{2}:\d{2}$/.test(timeValue)) {
              validation.textContent = "Choose a valid time of day.";
              return;
            }
            const payload = {
              enabled: enabled.checked,
              frequency: frequency.value,
              timeOfDay: timeValue,
              weekday: Number(weekday.value),
              retentionCount: retentionValue,
              scopes,
            };
            try {
              await this.ctx.api.put("/api/backups/schedule", payload);
              this.ctx.toast.success("Backup schedule saved");
              closeDialog();
              await this.loadSchedule();
            } catch (error) {
              validation.textContent = `Could not save schedule: ${errorMessage(error)}`;
            }
          },
        },
      ],
    });
    this.activeModalClose = closeSchedule;
  }

  private openRetentionHelp(): void {
    const body = h(
      "div",
      { class: "col" },
      h("p", {}, "Retention limits how many automatically created archives are kept on disk."),
      h(
        "ul",
        { class: "backup-help-list" },
        h("li", {}, "After each scheduled run, RAST counts the scheduled archives for the schedule."),
        h("li", {}, "Archives beyond the retention count are deleted oldest-first."),
        h("li", {}, "Manual backups created with “Create backup” are never pruned automatically."),
        h("li", {}, "The download, verify and restore-guide actions are always read-only and never modify the server."),
      ),
      h("p", { class: "small muted" }, "Keep at least two archives and copy important ones off the server."),
    );
    const close = modal({ title: "How retention works", body, actions: [{ label: "Close", variant: "ghost", onClick: () => {} }] });
    this.lc.add(close);
  }

  // --------------------------------------------------------- restore guide

  private async openRestoreGuide(backup: BackupRow): Promise<void> {
    if (this.readOnly || backup.downloadable === false) {
      this.ctx.toast.warning("The restore guide is available to administrators only.");
      return;
    }
    const stepsHost = h("div", { class: "backup-restore-steps" }, h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading restore guide…"));
    const body = h(
      "div",
      { class: "col" },
      banner(
        "warning",
        "Restore is an offline operation",
        "RAST does not perform an online restore from the web UI. Stop the service first, run the CLI restore command as the rast user, then start the service again. Keep the current data directory backed up before restoring.",
      ),
      stepsHost,
    );
    const close = modal({
      title: `Restore guide — ${backup.fileName || backup.id}`,
      body,
      wide: true,
      actions: [
        { label: "Download archive", variant: "ghost", onClick: () => this.download(backup) },
        { label: "Close", variant: "primary", onClick: () => {} },
      ],
    });
    this.lc.add(close);
    try {
      const guide = await this.ctx.api.get<RestoreGuideResponse>("/api/backups/restore-guide", {
        backupId: backup.id,
      });
      const steps = Array.isArray(guide?.steps) ? guide.steps : [];
      clear(stepsHost);
      if (steps.length > 0) {
        if (guide?.warning) {
          stepsHost.appendChild(banner("warning", "Application data is not covered", guide.warning));
        }
        stepsHost.appendChild(renderSteps(steps));
        if (guide?.rollback) {
          stepsHost.appendChild(banner("warning", "Rollback and schema compatibility", guide.rollback));
        }
        stepsHost.appendChild(
          h(
            "div",
            { class: "small muted backup-restore-fallback" },
            "Always follow the command names for your installation. The archive must be readable by the rast service user.",
          ),
        );
      } else {
        stepsHost.appendChild(fallbackSteps(backup));
      }
    } catch (error) {
      clear(stepsHost);
      if (isUnavailableError(error)) {
        stepsHost.appendChild(
          h("div", { class: "small muted" }, `The server has no restore guide yet (${unavailableReason(error)}). Use the manual steps below.`),
        );
      }
      stepsHost.appendChild(fallbackSteps(backup));
    }
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function scopeBadges(scopes: string[] | undefined): HTMLElement[] {
  const list = Array.isArray(scopes) ? scopes : [];
  if (list.length === 0) return [h("span", { class: "small muted" }, "—")];
  return list.slice(0, 6).map((scope) => h("span", { class: "badge" }, scopeLabel(scope)));
}

function scopeLabel(scope: string): string {
  const known = SCOPES.find((entry) => entry.id === scope);
  return known ? known.label : scope;
}

function frequencyLabel(frequency: string): string {
  return FREQUENCIES.find((entry) => entry.value === frequency)?.label ?? frequency;
}

function weekDayLabel(weekday: number): string {
  if (!Number.isInteger(weekday) || weekday < 0 || weekday > 6) return `day ${weekday}`;
  return WEEKDAYS[weekday] ?? `day ${weekday}`;
}

function timeLabel(value: string): string {
  return /^\d{2}:\d{2}/.test(value) ? value : value || "—";
}

function relativeFuture(ts: number): string {
  const diff = ts - Date.now();
  if (diff <= 0) return "due now";
  return `in ${formatDuration(Math.floor(diff / 1000))}`;
}

function normalizeTime(value: string | undefined): string {
  if (!value) return "03:00";
  const match = /^(\d{1,2}):(\d{2})/.exec(value);
  if (!match) return "03:00";
  const hours = Math.min(23, Math.max(0, Number(match[1])));
  const minutes = Math.min(59, Math.max(0, Number(match[2])));
  return `${String(hours).padStart(2, "0")}:${String(minutes).padStart(2, "0")}`;
}

function backupStatusClass(status: string): string {
  switch (status) {
    case "done":
      return "ok";
    case "running":
      return "warn";
    case "failed":
      return "error";
    default:
      return "";
  }
}

function renderSteps(steps: RestoreStep[]): HTMLElement {
  const list = h("ol", { class: "backup-restore-list" });
  for (const step of steps) {
    if (typeof step === "string") {
      list.appendChild(h("li", {}, h("span", {}, step)));
      continue;
    }
    const item = h("li", {});
    if (step.title) item.appendChild(h("div", { class: "backup-restore-step-title" }, step.title));
    const text = step.text ?? step.message ?? step.detail;
    if (text) item.appendChild(h("div", { class: "small muted" }, text));
    if (step.command) item.appendChild(h("pre", { class: "code backup-command" }, step.command));
    list.appendChild(item);
  }
  return list;
}

function fallbackSteps(backup: Backup): HTMLElement {
  const fileName = backup.fileName || `${backup.id}.tar.gz`;
  const list = h("ol", { class: "backup-restore-list" });
  list.appendChild(
    h(
      "li",
      {},
      h("div", { class: "backup-restore-step-title" }, "Download the archive"),
      h("div", { class: "small muted" }, `Save “${fileName}” to the server (for example /var/lib/rast/backups/).`),
    ),
  );
  list.appendChild(
    h(
      "li",
      {},
      h("div", { class: "backup-restore-step-title" }, "Stop the RAST service"),
      h("pre", { class: "code backup-command" }, "sudo systemctl stop rast"),
    ),
  );
  list.appendChild(
    h(
      "li",
      {},
      h("div", { class: "backup-restore-step-title" }, "Restore from the archive (offline)"),
      h(
        "pre",
        { class: "code backup-command" },
        `sudo -u rast rast restore --from /var/lib/rast/backups/${fileName}`,
      ),
      h("div", { class: "small muted" }, "The service must be stopped. RAST never runs this command from the web UI."),
    ),
  );
  list.appendChild(
    h(
      "li",
      {},
      h("div", { class: "backup-restore-step-title" }, "Start the service and verify"),
      h("pre", { class: "code backup-command" }, "sudo systemctl start rast"),
      h("div", { class: "small muted" }, `Archive created ${formatDate(backup.createdAt)} · size ${formatBytes(backup.sizeBytes)}.`),
    ),
  );
  return list;
}

// ---------------------------------------------------------------------------

export const backupApp: AppDefinition = {
  id: "backup",
  name: "Backup Center",
  icon: "backup",
  description: "Back up and restore RAST configuration and data",
  adminOnly: true,
  singleton: true,
  defaultSize: { w: 1020, h: 680 },
  minSize: { w: 380, h: 320 },
  async mount(ctx: AppContext) {
    const app = new BackupCenterApp(ctx);
    await app.start();
  },
};
