// App Center: Linux apps (administrators only). A .deb package, an AppImage
// or a portable archive from File Station becomes an app with its own window;
// each person's copy runs on the server and stops when the window closes.
//
// Tiles have one action (Open, Update or progress); the app's page has the
// rest: updates from the official source, memory and CPU limits, uninstall.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { formatBytes, formatDate, relativeTime } from "../../core/format";
import { iconSvg } from "../../core/icons";
import type { RealtimeStore } from "../../core/realtime";
import { contextMenu, errorMessage, modal, type ContextMenuEntry } from "../../core/ui";
import type { AppLifecycle } from "../../components/lifecycle";
import { watchJob } from "../../components/jobs";
import { banner, emptyBlock } from "../../components/states";
import { pickDestination } from "../files/picker";
import { UploadManager } from "../files/upload";
import {
  isLinuxPackageName,
  isOpenable,
  linuxApps,
  linuxAppById,
  linuxOverview,
  loadLinuxApps,
  NEWER_BUILD,
  type LinuxApp,
} from "../linuxapp/catalog";
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

const FORMAT_LABEL: Record<string, string> = {
  deb: ".deb package",
  appimage: "AppImage",
  tarball: "Portable archive",
};

const SOURCE_LABEL: Record<string, string> = {
  apt: "Its official package repository",
  github: "GitHub releases",
  zsync: "The AppImage's update server",
};

/** The file types an app can be added from, as shown to people. */
const ADD_FORMATS = ".deb, .AppImage or .tar.gz";

/** Where a dropped file is saved (the person's home). */
const DROP_FOLDER = "/Downloads";

const MEMORY_CHOICES_MB = [1024, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768];

export class LinuxApps {
  private readonly host: LinuxHost;
  /** Running install and update jobs, by job id. */
  private readonly progress = new Map<string, Progress>();
  private readonly checking = new Set<string>();
  private uploads: UploadManager | null = null;

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
          if (data?.id && this.progress.delete(data.id)) void this.reload();
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

  reload(): Promise<void> {
    if (!this.enabled) return Promise.resolve();
    return loadLinuxApps(this.host.ctx.api).catch(() => undefined);
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
   * Dropping a .deb, AppImage or archive on App Center uploads it to the
   * person's Downloads folder and installs it.
   */
  attachDrop(root: HTMLElement): void {
    if (!this.enabled) return;
    const { ctx, lc } = this.host;
    const overlay = h(
      "div",
      { class: "fs-drop-overlay hidden" },
      iconSvg("upload", 34),
      h("div", { class: "fs-drop-title" }, "Drop to add an app"),
      h("div", { class: "small muted" }, `${ADD_FORMATS}. The file is saved in your Downloads folder.`),
    );
    const uploads = new UploadManager(ctx, lc, {
      currentDir: () => DROP_FOLDER,
      onUploaded: (entry) => void this.install({ path: entry.path }),
      onDrained: () => undefined,
      maxUploadBytes: () => ctx.store.get().maxUploadBytes,
    });
    this.uploads = uploads;
    root.append(overlay, uploads.el);
    let depth = 0;
    const active = (event: DragEvent): boolean => !this.host.readOnly() && hasFileDrag(event);
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
      void this.dropped(Array.from(event.dataTransfer?.files ?? []));
    });
  }

  private async dropped(files: File[]): Promise<void> {
    const { ctx } = this.host;
    const usable = files.filter((file) => isLinuxPackageName(file.name));
    if (usable.length === 0) {
      if (files.length) ctx.toast.warning(`That file type is not supported. Use ${ADD_FORMATS}.`);
      return;
    }
    if (!this.dockerEnabled) {
      ctx.toast.warning("Linux apps are not available here: they need Docker on a Linux server.");
      return;
    }
    try {
      await ctx.api.post("/api/files/mkdir", { path: DROP_FOLDER });
    } catch {
      /* the folder is already there */
    }
    this.uploads?.enqueue(usable, DROP_FOLDER);
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
        h("div", { class: "pkg-tile-sub" }, tileSubtitle(app)),
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
      { class: "pkg-tile is-linux-add", type: "button", title: "Install a .deb package, an AppImage or a .tar.gz archive" },
      h("div", { class: "pkg-icon pkg-icon-tile is-add", ariaHidden: true }, iconSvg("plus", 26)),
      h("div", { class: "pkg-tile-body" }, h("span", { class: "pkg-tile-name" }, "Add an app"), h("div", { class: "pkg-tile-sub" }, ADD_FORMATS)),
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
        h("div", { class: "pkg-hero-publisher" }, iconSvg("box", 13), `${FORMAT_LABEL[app.format] ?? app.format} · Linux app`),
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
    if (app.status === "ready" && app.update.source) {
      items.push({ label: "Check for updates", icon: "refresh", onClick: () => void this.check(app) });
    }
    if (app.status === "ready" && app.update.source) {
      items.push({ label: "Reinstall", icon: "restart", onClick: () => void this.update(app) });
    }
    if (app.status === "ready") {
      items.push({ label: "Memory and CPU…", icon: "cpu", onClick: () => this.openLimits(app) });
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
    items.push(["Open now", app.running > 0 ? `${app.running} ${app.running === 1 ? "copy" : "copies"}` : "Not running"]);
    if (app.sizeBytes) items.push(["Size", formatBytes(app.sizeBytes)]);
    items.push(["Added", formatDate(app.createdAt)]);
    items.push(["Files", "Your RAST home folder"]);
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

  // ---------------------------------------------------------------- actions

  openAdd(): void {
    const { ctx } = this.host;
    if (!this.dockerEnabled) {
      ctx.toast.warning("Linux apps are not available here: they need Docker on a Linux server.");
      return;
    }
    // A file from this device (dropped anywhere while the dialog is open, or
    // chosen), or one already in File Station.
    const input = h("input", { type: "file", class: "hidden", accept: ".deb,.AppImage,.appimage,.gz,.tgz,.xz,.txz,.bz2,.tbz2,.tar" });
    let close: () => void = () => undefined;
    const take = (files: File[]): void => {
      close();
      void this.dropped(files);
    };
    input.addEventListener("change", () => take(Array.from(input.files ?? [])));
    const choose = h("button", { class: "btn btn-primary btn-sm", type: "button" }, "Choose a file");
    choose.addEventListener("click", () => input.click());
    const zone = h(
      "div",
      { class: "linux-drop" },
      iconSvg("upload", 30),
      h("div", { class: "linux-drop-title" }, "Drop a file here"),
      h("div", { class: "small muted" }, ADD_FORMATS),
      choose,
    );
    const fromStation = h("button", { class: "btn btn-ghost btn-sm", type: "button" }, iconSvg("folder", 14), "Pick from File Station");
    fromStation.addEventListener("click", () => {
      close();
      this.pickFromStation();
    });
    const link = h("input", { class: "input", type: "url", placeholder: "https://…/app.deb", ariaLabel: "Link to the file" });
    const linkError = h("div", { class: "form-error", role: "alert" });
    const useLink = h("button", { class: "btn btn-secondary btn-sm", type: "submit" }, "Install");
    const linkForm = h(
      "form",
      { class: "linux-link" },
      h("label", { class: "small muted" }, "Or paste a link to the file"),
      h("div", { class: "linux-link-row" }, link, useLink),
      linkError,
    );
    linkForm.addEventListener("submit", (event) => {
      event.preventDefault();
      const url = link.value.trim();
      if (!url.startsWith("https://")) {
        linkError.textContent = "Enter a link that starts with https://";
        return;
      }
      useLink.disabled = true;
      void this.install({ url }).then((error) => {
        useLink.disabled = false;
        if (error) linkError.textContent = error;
        else close();
      });
    });
    const onOver = (event: DragEvent): void => {
      if (!hasFileDrag(event)) return;
      event.preventDefault();
      zone.classList.add("is-over");
    };
    const onLeave = (event: DragEvent): void => {
      if (!event.relatedTarget) zone.classList.remove("is-over");
    };
    const onDrop = (event: DragEvent): void => {
      if (!hasFileDrag(event)) return;
      event.preventDefault();
      event.stopPropagation();
      take(Array.from(event.dataTransfer?.files ?? []));
    };
    document.addEventListener("dragover", onOver, true);
    document.addEventListener("dragleave", onLeave, true);
    document.addEventListener("drop", onDrop, true);
    close = modal({
      title: "Add an app",
      body: h("div", { class: "col linux-add" }, zone, linkForm, fromStation, input),
      actions: [{ label: "Cancel", variant: "ghost", onClick: () => {} }],
      onClose: () => {
        document.removeEventListener("dragover", onOver, true);
        document.removeEventListener("dragleave", onLeave, true);
        document.removeEventListener("drop", onDrop, true);
      },
    });
  }

  private pickFromStation(): void {
    const { ctx, lc } = this.host;
    void pickDestination(ctx, lc, {
      title: "Add an app",
      confirmLabel: "Install",
      startPath: "/",
      files: (entry) => isLinuxPackageName(entry.name),
      emptyHint: "No .deb, AppImage or .tar.gz files here. Upload one with File Station first.",
    }).then((path) => {
      if (path) void this.install({ path });
    });
  }

  /**
   * Install a File Station file (`path`) or a file behind a link (`url`, the
   * server downloads it) and show the app's page. Resolves with the error
   * message when the install could not start.
   */
  private async install(source: { path: string } | { url: string }): Promise<string | null> {
    const { ctx } = this.host;
    try {
      const res = await ctx.api.post<{ app: LinuxApp; jobId: string }>("/api/linux-apps/install", source);
      ctx.toast.info(`Installing ${res.app.name}. This can take a few minutes.`);
      this.follow(res.jobId);
      await this.reload();
      this.host.openPage(res.app.id);
      return null;
    } catch (error) {
      const message = errorMessage(error);
      if ("path" in source) ctx.toast.error(message);
      return message;
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

  /** Progress for a job this window started (events alone may arrive first). */
  private follow(jobId: string): void {
    if (!this.progress.has(jobId)) this.progress.set(jobId, { progress: 0, total: 0, message: "Queued" });
    void watchJob(this.host.ctx, this.host.lc, jobId, (u) => {
      this.progress.set(jobId, { progress: u.progress, total: u.total, message: u.message ?? "" });
      this.paintProgress(jobId);
    }).then(() => {
      this.progress.delete(jobId);
      void this.reload();
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

function hasFileDrag(event: DragEvent): boolean {
  return Array.from(event.dataTransfer?.types ?? []).includes("Files");
}
