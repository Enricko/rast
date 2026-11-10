// File Station: browsing, uploads, downloads, previews, share roots, recycle
// bin, quota display and keyboard shortcuts. All user data is rendered with
// textContent through h() — never innerHTML.

import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import {
  formatBytes,
  formatBytesShort,
  formatDateTime,
  relativeTime,
} from "../../core/format";
import { fileIcon, iconSvg } from "../../core/icons";
import { confirmDialog, contextMenu, errorMessage, promptDialog } from "../../core/ui";
import type { FileEntry, FileListing, FileRoot, TrashEntry } from "../../core/types";
import { AppLifecycle, isAbortError } from "../../components/lifecycle";
import { watchJob, jobProgressFraction, jobProgressLabel, type JobUpdate } from "../../components/jobs";
import { Pager } from "../../components/pager";
import {
  isUnavailableError,
  progressBar,
  renderLoadFailure,
  unavailableReason,
  type ProgressHandle,
} from "../../components/states";
import { openPreview } from "./preview";
import { pickDestination } from "./picker";
import { UploadManager } from "./upload";
import {
  joinPath,
  listingItems,
  previewKind,
  sortEntries,
  validateFileName,
  type JobRef,
  type TrashPage,
  type UsageStats,
} from "./types";

type Section = "files" | "trash";
type ViewMode = "list" | "grid";
type SortKey = "name" | "size" | "modified";

const VIEW_KEY = "rast.files.view";
const PER_PAGE_DEFAULT = 50;

export class FileStation {
  private readonly ctx: AppContext;
  private readonly lc: AppLifecycle;

  // Persistent DOM.
  private readonly toolbar: HTMLElement;
  private readonly filesGroup: HTMLElement;
  private readonly selectionGroup: HTMLElement;
  private readonly trashGroup: HTMLElement;
  private readonly uploadButton: HTMLButtonElement;
  private readonly newFolderButton: HTMLButtonElement;
  private readonly downloadButton: HTMLButtonElement;
  private readonly renameButton: HTMLButtonElement;
  private readonly copyButton: HTMLButtonElement;
  private readonly moveButton: HTMLButtonElement;
  private readonly deleteButton: HTMLButtonElement;
  private readonly restoreButton: HTMLButtonElement;
  private readonly purgeButton: HTMLButtonElement;
  private readonly emptyBinButton: HTMLButtonElement;
  private readonly searchInput: HTMLInputElement;
  private readonly listViewButton: HTMLButtonElement;
  private readonly gridViewButton: HTMLButtonElement;
  private readonly fileInput: HTMLInputElement;
  private readonly rootsNav: HTMLElement;
  private readonly trashNav: HTMLButtonElement;
  private readonly usageHost: HTMLElement;
  private readonly crumbs: HTMLElement;
  private readonly statusLine: HTMLElement;
  private readonly panel: HTMLElement;
  private readonly selectionInfo: HTMLElement;
  private readonly pager: Pager;
  private readonly detail: HTMLElement;
  private readonly split: HTMLElement;
  private readonly dropOverlay: HTMLElement;
  private readonly uploads: UploadManager;

  // State.
  private section: Section = "files";
  private path = "/";
  private roots: FileRoot[] = [];
  private rootsError: unknown = null;
  private listing: FileListing | null = null;
  private loading = true;
  private loadError: unknown = null;
  private query = "";
  private sort: SortKey = "name";
  private order: "asc" | "desc" = "asc";
  private page = 1;
  private perPage = PER_PAGE_DEFAULT;
  private total = 0;
  private view: ViewMode;
  private selection = new Set<string>();
  private anchorPath: string | null = null;
  private rowEls = new Map<string, HTMLElement>();
  private checkboxEls = new Map<string, HTMLInputElement>();
  private selectAllBox: HTMLInputElement | null = null;
  private usage: UsageStats | null = null;
  private usageError: unknown = null;
  private trashItems: TrashEntry[] = [];
  private trashTotal = 0;
  private trashPage = 1;
  private trashSelection = new Set<string>();
  private trashRows = new Map<string, HTMLElement>();
  private listingRequest: AbortController | null = null;
  private trashRequest: AbortController | null = null;
  private activeJobId: string | null = null;
  private jobUpdate: JobUpdate | null = null;
  private readonly jobBar: ProgressHandle;
  private searchTimer = 0;
  private dragDepth = 0;
  private disposed = false;

  constructor(ctx: AppContext) {
    this.ctx = ctx;
    this.lc = new AppLifecycle(ctx);
    this.view = readViewPreference();
    this.jobBar = progressBar();

    // ------------------------------------------------------------- toolbar
    this.uploadButton = toolButton("Upload", "upload", () => this.fileInput.click(), "primary");
    this.newFolderButton = toolButton("New folder", "folder", () => void this.newFolder());
    this.downloadButton = toolButton("Download", "download", () => void this.downloadSelected());
    this.renameButton = toolButton("Rename", "rename", () => void this.renameSelected());
    this.copyButton = toolButton("Copy", "copy", () => void this.transferSelected("copy"));
    this.moveButton = toolButton("Move", "move", () => void this.transferSelected("move"));
    this.deleteButton = toolButton("Delete", "trash", () => void this.deleteSelected(), "danger");
    this.restoreButton = toolButton("Restore", "restore", () => void this.restoreSelected());
    this.purgeButton = toolButton("Delete forever", "trash", () => void this.purgeSelected(), "danger");
    this.emptyBinButton = toolButton("Empty recycle bin", "trash", () => void this.emptyBin(), "danger");

    this.filesGroup = h("div", { class: "fs-toolbar-group" }, this.uploadButton, this.newFolderButton);
    this.selectionGroup = h(
      "div",
      { class: "fs-toolbar-group" },
      this.downloadButton,
      this.renameButton,
      this.copyButton,
      this.moveButton,
      this.deleteButton,
    );
    this.trashGroup = h(
      "div",
      { class: "fs-toolbar-group" },
      this.restoreButton,
      this.purgeButton,
      this.emptyBinButton,
    );

    this.searchInput = h("input", {
      class: "input fs-search",
      type: "search",
      placeholder: "Search this folder…",
      ariaLabel: "Search files in this folder",
    });
    this.searchInput.addEventListener("input", () => {
      this.lc.clearTimeout(this.searchTimer);
      this.searchTimer = this.lc.setTimeout(() => {
        this.query = this.searchInput.value.trim();
        this.page = 1;
        this.selection.clear();
        void this.loadListing();
      }, 350);
    });
    this.searchInput.addEventListener("keydown", (event) => {
      if (event.key !== "Enter") return;
      event.preventDefault();
      this.lc.clearTimeout(this.searchTimer);
      this.query = this.searchInput.value.trim();
      this.page = 1;
      this.selection.clear();
      void this.loadListing();
    });

    this.listViewButton = viewButton("list", "List view", () => this.setView("list"));
    this.gridViewButton = viewButton("grid", "Grid view", () => this.setView("grid"));

    this.toolbar = h(
      "div",
      { class: "app-toolbar fs-toolbar", role: "toolbar", ariaLabel: "File actions" },
      this.filesGroup,
      this.selectionGroup,
      this.trashGroup,
      h("div", { class: "app-toolbar-spacer" }),
      this.searchInput,
      toolButton("", "refresh", () => this.refresh(), "subtle", "Refresh"),
      this.listViewButton,
      this.gridViewButton,
    );

    this.fileInput = h("input", { type: "file", multiple: true, class: "hidden", tabIndex: -1 });
    this.fileInput.addEventListener("change", () => {
      const files = this.fileInput.files;
      if (files && files.length > 0) this.uploads.enqueue(files, this.path);
      this.fileInput.value = "";
    });

    // ------------------------------------------------------------ sidebar
    this.rootsNav = h("div", { class: "nav-list", role: "navigation", ariaLabel: "File locations" });
    this.trashNav = h(
      "button",
      { class: "nav-item fs-trash-nav", type: "button" },
      iconSvg("trash", 16),
      h("span", {}, "Recycle bin"),
    );
    this.trashNav.addEventListener("click", () => void this.showSection("trash"));
    this.usageHost = h("div", { class: "fs-usage" });
    const sidebar = h(
      "aside",
      { class: "app-sidebar fs-sidebar", ariaLabel: "Locations" },
      h("div", { class: "fs-sidebar-title" }, "Locations"),
      this.rootsNav,
      h("div", { class: "menu-sep" }),
      this.trashNav,
      h("div", { class: "menu-sep" }),
      this.usageHost,
    );

    // --------------------------------------------------------------- main
    this.crumbs = h("div", { class: "fs-crumb-list", role: "navigation", ariaLabel: "Breadcrumbs" });
    this.statusLine = h("div", { class: "fs-status-line" });
    this.panel = h("div", { class: "fs-panel", role: "region", ariaLabel: "Files" });
    this.selectionInfo = h("div", { class: "fs-selection-info small muted" });
    this.pager = new Pager({
      onChange: (page, perPage) => {
        if (this.section === "trash") {
          this.trashPage = page;
          this.perPage = perPage;
          void this.loadTrash();
        } else {
          this.page = page;
          this.perPage = perPage;
          this.selection.clear();
          void this.loadListing();
        }
      },
      label: "File pages",
    });
    const main = h(
      "main",
      { class: "app-main fs-main" },
      h("div", { class: "fs-crumbs" }, this.crumbs, h("div", { class: "fs-crumbs-spacer" }), this.statusLine),
      this.panel,
      h("div", { class: "fs-footer" }, this.selectionInfo, this.pager.el),
    );

    this.detail = h("aside", { class: "fs-detail", ariaLabel: "Details" });

    this.split = h("div", { class: "app-split fs-split" }, sidebar, main, this.detail);

    this.dropOverlay = h(
      "div",
      { class: "fs-drop-overlay hidden" },
      iconSvg("upload", 34),
      h("div", { class: "fs-drop-title" }, "Drop files to upload"),
      h("div", { class: "small muted" }, "They will be uploaded to the current folder."),
    );

    this.uploads = new UploadManager(this.ctx, this.lc, {
      currentDir: () => this.path,
      onUploaded: () => {
        /* refresh once the queue drains to keep requests bounded */
      },
      onDrained: () => void this.refresh(),
      maxUploadBytes: () => this.ctx.store.get().maxUploadBytes,
    });

    ctx.root.classList.add("fs-app");
    ctx.root.append(this.toolbar, this.split, this.dropOverlay, this.uploads.el, this.fileInput);
    this.registerInteractions();
    this.setView(this.view, false);
    this.renderChrome();
  }

  async start(): Promise<void> {
    this.ctx.setTitle("File Station");
    this.renderUsage();
    await Promise.all([this.loadRoots(), this.loadListing(), this.loadUsage()]);
  }

  // ------------------------------------------------------------- lifecycle

  private registerInteractions(): void {
    const keyHandler = (event: KeyboardEvent) => this.onKeyDown(event);
    document.addEventListener("keydown", keyHandler);
    this.lc.add(() => {
      this.disposed = true;
      document.removeEventListener("keydown", keyHandler);
      this.listingRequest?.abort();
      this.trashRequest?.abort();
    });

    this.ctx.root.addEventListener("dragenter", (event) => {
      if (this.section !== "files" || !hasFileDrag(event)) return;
      event.preventDefault();
      this.dragDepth += 1;
      this.dropOverlay.classList.remove("hidden");
    });
    this.ctx.root.addEventListener("dragover", (event) => {
      if (this.section !== "files" || !hasFileDrag(event)) return;
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    });
    this.ctx.root.addEventListener("dragleave", (event) => {
      if (this.section !== "files" || !hasFileDrag(event)) return;
      this.dragDepth = Math.max(0, this.dragDepth - 1);
      if (this.dragDepth === 0) this.dropOverlay.classList.add("hidden");
    });
    this.ctx.root.addEventListener("drop", (event) => {
      if (this.section !== "files" || !hasFileDrag(event)) return;
      event.preventDefault();
      this.dragDepth = 0;
      this.dropOverlay.classList.add("hidden");
      const files = event.dataTransfer?.files;
      if (files && files.length > 0) this.uploads.enqueue(files, this.path);
    });
    // Clicking blank space clears the selection.
    this.panel.addEventListener("pointerdown", (event) => {
      if (event.target instanceof HTMLElement && event.target.closest(".fs-table, .fs-grid")) return;
      if (this.selection.size > 0) this.clearSelection();
    });
  }

  private onKeyDown(event: KeyboardEvent): void {
    if (this.disposed) return;
    if (!this.windowFocused()) return;
    if (document.querySelector(".modal-backdrop")) return;
    const target = event.target;
    if (target instanceof HTMLElement && target.closest("input, textarea, select, [contenteditable='true']")) return;

    if (event.key === "F5") {
      event.preventDefault();
      this.refresh();
      return;
    }
    if (event.key === "F2" && this.section === "files") {
      event.preventDefault();
      if (this.selection.size === 1) void this.renameSelected();
      return;
    }
    if (event.key === "Delete") {
      if (this.section === "trash") {
        if (this.trashSelection.size > 0) void this.purgeSelected();
      } else if (this.selection.size > 0) {
        event.preventDefault();
        void this.deleteSelected();
      }
      return;
    }
    if ((event.ctrlKey || event.metaKey) && (event.key === "a" || event.key === "A")) {
      if (this.section !== "files") return;
      event.preventDefault();
      this.selectAllOnPage();
      return;
    }
    if (event.key === "Escape") {
      if (this.selection.size > 0) this.clearSelection();
      else if (this.trashSelection.size > 0) {
        this.trashSelection.clear();
        this.updateSelectionUi();
      }
    }
  }

  private windowFocused(): boolean {
    const win = this.ctx.root.closest(".window");
    return win ? win.classList.contains("is-focused") : true;
  }

  // --------------------------------------------------------------- loading

  private async loadRoots(): Promise<void> {
    try {
      const roots = await this.ctx.api.get<FileRoot[]>("/api/files/roots");
      if (this.disposed) return;
      const list = Array.isArray(roots) ? roots : [];
      if (!list.some((root) => root.kind === "home")) {
        list.unshift({ name: "Home", path: "/", kind: "home" });
      }
      this.roots = list;
      this.rootsError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.roots = [{ name: "Home", path: "/", kind: "home" }];
      this.rootsError = error;
    }
    this.renderRoots();
    this.renderChrome();
  }

  private async loadListing(): Promise<void> {
    if (this.disposed || this.section !== "files") return;
    this.listingRequest?.abort();
    const request = this.lc.trackController(new AbortController());
    this.listingRequest = request;
    this.loading = true;
    this.renderStatusLine();
    try {
      const listing = await this.ctx.api.get<FileListing>(
        "/api/files",
        {
          path: this.path,
          page: this.page,
          perPage: this.perPage,
          sort: this.sort,
          order: this.order,
          q: this.query || undefined,
        },
        { signal: request.signal },
      );
      if (request.signal.aborted || this.disposed) return;
      this.listing = listing;
      this.loadError = null;
      this.page = typeof listing?.page === "number" ? listing.page : this.page;
      this.perPage = typeof listing?.perPage === "number" ? listing.perPage : this.perPage;
      this.total = typeof listing?.total === "number" ? listing.total : listingItems(listing).length;
      const valid = new Set(listingItems(listing).map((entry) => entry.path));
      this.selection = new Set([...this.selection].filter((path) => valid.has(path)));
      this.anchorPath = this.anchorPath && valid.has(this.anchorPath) ? this.anchorPath : null;
      this.pager.set(this.page, this.perPage, this.total);
    } catch (error) {
      if (isAbortError(error) || request.signal.aborted || this.disposed) return;
      this.loadError = error;
      this.listing = null;
      this.selection.clear();
      this.anchorPath = null;
    } finally {
      if (this.listingRequest === request) {
        this.listingRequest = null;
        this.loading = false;
      }
      this.lc.releaseController(request);
      if (!this.disposed) {
        this.renderPanel();
        this.renderCrumbs();
        this.renderStatusLine();
        this.updateSelectionUi();
      }
    }
  }

  private async loadUsage(): Promise<void> {
    try {
      const usage = await this.ctx.api.get<UsageStats>("/api/files/usage");
      if (this.disposed) return;
      this.usage = usage;
      this.usageError = null;
    } catch (error) {
      if (isAbortError(error) || this.disposed) return;
      this.usageError = error;
    }
    this.renderUsage();
  }

  private async loadTrash(): Promise<void> {
    if (this.disposed || this.section !== "trash") return;
    this.trashRequest?.abort();
    const request = this.lc.trackController(new AbortController());
    this.trashRequest = request;
    this.loading = true;
    this.loadError = null;
    this.trashItems = [];
    this.trashSelection.clear();
    this.renderStatusLine();
    this.renderPanel();
    try {
      const page = await this.ctx.api.get<TrashPage>(
        "/api/files/trash",
        { page: this.trashPage, perPage: this.perPage },
        { signal: request.signal },
      );
      if (request.signal.aborted || this.disposed) return;
      this.trashItems = Array.isArray(page?.items) ? page.items : [];
      this.trashTotal = typeof page?.total === "number" ? page.total : this.trashItems.length;
      this.trashPage = typeof page?.page === "number" ? page.page : this.trashPage;
      this.pager.set(this.trashPage, this.perPage, this.trashTotal);
    } catch (error) {
      if (isAbortError(error) || request.signal.aborted || this.disposed) return;
      this.loadError = error;
      this.trashItems = [];
    } finally {
      if (this.trashRequest === request) {
        this.trashRequest = null;
        this.loading = false;
      }
      this.lc.releaseController(request);
      if (!this.disposed) {
        this.renderPanel();
        this.renderCrumbs();
        this.renderStatusLine();
        this.updateSelectionUi();
      }
    }
  }

  private refresh(): void {
    this.renderRoots();
    this.renderUsage();
    if (this.section === "trash") void this.loadTrash();
    else void this.loadListing();
    void this.loadUsage();
  }

  // -------------------------------------------------------------- rendering

  private renderChrome(): void {
    this.renderRoots();
    this.renderUsage();
    this.renderCrumbs();
    this.renderPanel();
    this.renderStatusLine();
    this.updateSelectionUi();
    this.filesGroup.classList.toggle("hidden", this.section !== "files");
    this.selectionGroup.classList.toggle("hidden", this.section !== "files");
    this.trashGroup.classList.toggle("hidden", this.section !== "trash");
    this.searchInput.classList.toggle("hidden", this.section !== "files");
    this.listViewButton.classList.toggle("hidden", this.section !== "files");
    this.gridViewButton.classList.toggle("hidden", this.section !== "files");
  }

  private renderRoots(): void {
    clear(this.rootsNav);
    let activeRoot: FileRoot | null = null;
    for (const root of this.roots) {
      const matches =
        this.section === "files" &&
        (this.path === root.path || this.path.startsWith(root.path === "/" ? "/" : `${root.path}/`));
      if (matches && (!activeRoot || root.path.length > activeRoot.path.length)) activeRoot = root;
    }
    for (const root of this.roots) {
      const btn = h(
        "button",
        {
          class: `nav-item${activeRoot === root ? " is-active" : ""}`,
          type: "button",
          title: root.path,
        },
        iconSvg(root.kind === "home" ? "files" : "folder", 16),
        h("span", { class: "nav-label" }, root.name),
      );
      if (root.kind === "share" && root.perms) {
        btn.appendChild(h("span", { class: `badge ${root.perms === "read" ? "" : "info"} fs-root-badge` }, root.perms === "read" ? "read-only" : root.perms));
      }
      btn.addEventListener("click", () => void this.navigate(root.path));
      this.rootsNav.appendChild(btn);
    }
    this.trashNav.classList.toggle("is-active", this.section === "trash");
    if (this.rootsError) {
      this.rootsNav.appendChild(
        h(
          "div",
          { class: "fs-sidebar-note small" },
          isUnavailableError(this.rootsError)
            ? `Shared folders unavailable: ${unavailableReason(this.rootsError)}`
            : `Could not load locations: ${errorMessage(this.rootsError)}`,
        ),
      );
    }
  }

  private renderUsage(): void {
    clear(this.usageHost);
    const quota = this.usage?.quotaBytes ?? this.listing?.quota?.quotaBytes ?? 0;
    const used = this.usage?.usedBytes ?? this.listing?.quota?.usedBytes ?? 0;
    const free = Math.max(0, quota - used);
    const title = h("div", { class: "fs-sidebar-title" }, "Storage");
    this.usageHost.appendChild(title);
    if (this.usageError && !this.usage) {
      this.usageHost.appendChild(
        h(
          "div",
          { class: "fs-sidebar-note small" },
          isUnavailableError(this.usageError)
            ? `Usage unavailable: ${unavailableReason(this.usageError)}`
            : `Usage unavailable: ${errorMessage(this.usageError)}`,
        ),
      );
      return;
    }
    const fraction = quota > 0 ? Math.min(1, used / quota) : 0;
    const bar = h("div", { class: "progress fs-usage-bar" });
    const fill = h("div", { class: "progress-bar" });
    fill.style.width = `${(fraction * 100).toFixed(1)}%`;
    if (fraction > 0.92) fill.classList.add("is-error");
    bar.appendChild(fill);
    this.usageHost.appendChild(bar);
    this.usageHost.appendChild(
      h("div", { class: "fs-usage-line" }, quota > 0 ? `${formatBytes(used)} of ${formatBytes(quota)}` : `${formatBytes(used)} used · unlimited`),
    );
    if (quota > 0) {
      this.usageHost.appendChild(h("div", { class: "fs-usage-line muted" }, `${formatBytes(free)} free`));
    }
    if (this.usage) {
      this.usageHost.appendChild(
        h("div", { class: "fs-usage-line muted" }, `Recycle bin: ${formatBytes(this.usage.trashBytes ?? 0)}`),
      );
      const byType = Object.entries(this.usage.byType ?? {}).sort((a, b) => b[1] - a[1]);
      if (byType.length > 0) {
        const chips = h("div", { class: "fs-type-chips" });
        for (const [label, bytes] of byType.slice(0, 4)) {
          chips.appendChild(h("span", { class: "badge", title: label }, `${label} · ${formatBytes(bytes)}`));
        }
        this.usageHost.appendChild(chips);
      }
    }
  }

  private renderCrumbs(): void {
    clear(this.crumbs);
    if (this.section === "trash") {
      this.crumbs.appendChild(h("span", { class: "fs-crumb is-current", ariaLabel: "Current folder" }, "Recycle bin"));
      return;
    }
    const listing = this.listing;
    const crumbs = Array.isArray(listing?.breadcrumbs) ? listing.breadcrumbs : [];
    if (crumbs.length > 0) {
      crumbs.forEach((crumb, index) => {
        if (index > 0) this.crumbs.appendChild(h("span", { class: "fs-crumb-sep" }, "/"));
        const isLast = index === crumbs.length - 1;
        const btn = h(
          "button",
          {
            class: `fs-crumb${isLast ? " is-current" : ""}`,
            type: "button",
            disabled: isLast,
          },
          crumb.name,
        );
        if (isLast) btn.setAttribute("aria-current", "page");
        if (!isLast) btn.addEventListener("click", () => void this.navigate(crumb.path));
        this.crumbs.appendChild(btn);
      });
    } else {
      this.crumbs.appendChild(h("span", { class: "fs-crumb is-current" }, this.path));
    }
    if (this.query) {
      this.crumbs.appendChild(h("span", { class: "fs-crumb-sep" }, "·"));
      this.crumbs.appendChild(
        h(
          "span",
          { class: "fs-search-chip" },
          `Search “${this.query}”`,
          (() => {
            const clearBtn = h("button", { class: "fs-search-clear", type: "button", ariaLabel: "Clear search" }, "×");
            clearBtn.addEventListener("click", () => {
              this.query = "";
              this.searchInput.value = "";
              this.page = 1;
              void this.loadListing();
            });
            return clearBtn;
          })(),
        ),
      );
    }
  }

  private renderStatusLine(): void {
    clear(this.statusLine);
    if (this.loading) {
      this.statusLine.appendChild(h("span", { class: "spinner spinner-sm" }));
      this.statusLine.appendChild(h("span", { class: "small muted" }, "Loading…"));
      return;
    }
    if (this.activeJobId && this.jobUpdate) {
      const wrap = h("div", { class: "fs-job" }, this.jobBar.el, h("span", { class: "small" }, jobProgressLabel(this.jobUpdate)));
      const cancel = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, "Cancel");
      cancel.addEventListener("click", () => void this.cancelActiveJob());
      wrap.appendChild(cancel);
      this.jobBar.update(jobProgressFraction(this.jobUpdate));
      this.statusLine.appendChild(wrap);
    }
  }

  private setJob(update: JobUpdate | null, jobId: string | null): void {
    this.jobUpdate = update;
    this.activeJobId = jobId;
    this.renderStatusLine();
  }

  private renderPanel(): void {
    clear(this.panel);
    this.rowEls.clear();
    this.checkboxEls.clear();
    this.trashRows.clear();
    this.selectAllBox = null;
    if (this.section === "trash") {
      this.renderTrashPanel();
      return;
    }
    if (this.loading && !this.listing) {
      this.panel.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading files…"));
      return;
    }
    if (this.loadError) {
      renderLoadFailure(this.panel, this.loadError, {
        title: "Could not load this folder",
        retry: () => void this.loadListing(),
      });
      return;
    }
    const items = this.currentEntries();
    if (items.length === 0) {
      if (this.query) {
        this.panel.appendChild(
          h(
            "div",
            { class: "empty-state" },
            h("div", { class: "empty-title" }, "No matches"),
            h("div", { class: "empty-detail" }, `Nothing in this folder matches “${this.query}”.`),
          ),
        );
      } else {
        this.panel.appendChild(
          h(
            "div",
            { class: "empty-state" },
            h("div", { class: "empty-title" }, "This folder is empty"),
            h("div", { class: "empty-detail" }, "Upload files or create a folder to get started."),
            toolButton("Upload files", "upload", () => this.fileInput.click(), "primary"),
          ),
        );
      }
      return;
    }
    if (this.view === "grid") this.renderGrid(items);
    else this.renderTable(items);
  }

  private currentEntries(): FileEntry[] {
    const items = listingItems(this.listing);
    return sortEntries(items, this.sort, this.order);
  }

  private renderTable(items: FileEntry[]): void {
    const allOnPageSelected = items.length > 0 && items.every((entry) => this.selection.has(entry.path));
    this.selectAllBox = h("input", {
      type: "checkbox",
      ariaLabel: "Select all files on this page",
    });
    this.selectAllBox.checked = allOnPageSelected;
    this.selectAllBox.indeterminate = !allOnPageSelected && items.some((entry) => this.selection.has(entry.path));
    this.selectAllBox.addEventListener("change", () => {
      if (this.selectAllBox?.checked) this.selectAllOnPage();
      else this.clearSelection();
    });

    const head = h(
      "tr",
      {},
      h("th", { class: "fs-check" }, this.selectAllBox),
      sortHeader("Name", "name", this.sort, this.order, () => this.changeSort("name")),
      sortHeader("Size", "size", this.sort, this.order, () => this.changeSort("size"), "num fs-col-size"),
      sortHeader("Modified", "modified", this.sort, this.order, () => this.changeSort("modified"), "fs-col-modified"),
    );
    const body = h("tbody");
    for (const entry of items) body.appendChild(this.buildTableRow(entry));
    const table = h("table", { class: "table fs-table" }, h("thead", {}, head), body);
    this.panel.appendChild(h("div", { class: "table-wrap fs-table-wrap" }, table));
  }

  private buildTableRow(entry: FileEntry): HTMLTableRowElement {
    const checkbox = h("input", { type: "checkbox", ariaLabel: `Select ${entry.name}` });
    checkbox.checked = this.selection.has(entry.path);
    checkbox.addEventListener("click", (event) => {
      event.stopPropagation();
      event.preventDefault();
      this.selectEntry(entry, event);
    });
    this.checkboxEls.set(entry.path, checkbox);

    const icon = h("span", { class: "fs-entry-icon" });
    icon.appendChild(iconSvg(fileIcon(entry), 17));
    const name = h(
      "span",
      { class: "fs-entry-name", title: entry.name },
      entry.name,
      entry.kind === "dir" ? h("span", { class: "fs-entry-kind small muted" }, "folder") : null,
    );
    if (!entry.readable) name.appendChild(h("span", { class: "badge warn fs-entry-badge" }, "no read"));
    if (!entry.writable) name.appendChild(h("span", { class: "badge fs-entry-badge" }, "read-only"));

    const row = h(
      "tr",
      { tabIndex: 0, class: this.selection.has(entry.path) ? "is-selected" : "" },
      h("td", { class: "fs-check" }, checkbox),
      h("td", { class: "fs-name-cell" }, h("div", { class: "fs-entry" }, icon, name)),
      h(
        "td",
        { class: "num fs-col-size" },
        entry.kind === "dir" ? "—" : formatBytesShort(entry.size),
      ),
      h(
        "td",
        { class: "fs-col-modified", title: formatDateTime(entry.modifiedAt) },
        relativeTime(entry.modifiedAt),
      ),
    );
    row.addEventListener("click", (event) => {
      if ((event.target as HTMLElement).closest("input")) return;
      this.selectEntry(entry, event);
    });
    row.addEventListener("dblclick", (event) => {
      if ((event.target as HTMLElement).closest("input")) return;
      this.openEntry(entry);
    });
    row.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      if (!this.selection.has(entry.path)) this.selectSingle(entry);
      this.showItemMenu(entry, event.clientX, event.clientY);
    });
    row.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        this.openEntry(entry);
      }
    });
    this.rowEls.set(entry.path, row);
    return row;
  }

  private renderGrid(items: FileEntry[]): void {
    const grid = h("div", { class: "fs-grid", role: "list" });
    for (const entry of items) {
      const card = h(
        "div",
        {
          class: `fs-card${this.selection.has(entry.path) ? " is-selected" : ""}`,
          role: "listitem",
          tabIndex: 0,
          title: entry.path,
        },
        h("div", { class: "fs-card-icon" }, iconSvg(fileIcon(entry), 30)),
        h("div", { class: "fs-card-name" }, entry.name),
        h("div", { class: "fs-card-meta small muted" }, entry.kind === "dir" ? "Folder" : formatBytesShort(entry.size)),
      );
      card.addEventListener("click", (event) => this.selectEntry(entry, event));
      card.addEventListener("dblclick", (event) => {
        if ((event.target as HTMLElement).closest("input")) return;
        this.openEntry(entry);
      });
      card.addEventListener("contextmenu", (event) => {
        event.preventDefault();
        if (!this.selection.has(entry.path)) this.selectSingle(entry);
        this.showItemMenu(entry, event.clientX, event.clientY);
      });
      card.addEventListener("keydown", (event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          this.openEntry(entry);
        }
      });
      this.rowEls.set(entry.path, card);
      grid.appendChild(card);
    }
    this.panel.appendChild(grid);
  }

  private renderTrashPanel(): void {
    if (this.loading && this.trashItems.length === 0) {
      this.panel.appendChild(h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading recycle bin…"));
      return;
    }
    if (this.loadError) {
      renderLoadFailure(this.panel, this.loadError, {
        title: "Could not load the recycle bin",
        retry: () => void this.loadTrash(),
      });
      return;
    }
    if (this.trashItems.length === 0) {
      this.panel.appendChild(
        h(
          "div",
          { class: "empty-state" },
          h("div", { class: "empty-title" }, "Recycle bin is empty"),
          h("div", { class: "empty-detail" }, "Deleted files are kept here until you restore or permanently remove them."),
        ),
      );
      return;
    }
    const head = h(
      "tr",
      {},
      h("th", { class: "fs-check" }),
      h("th", {}, "Original location"),
      h("th", {}, "Type"),
      h("th", { class: "num fs-col-size" }, "Size"),
      h("th", { class: "fs-col-modified" }, "Deleted"),
      h("th", { class: "fs-actions-col" }, "Actions"),
    );
    const body = h("tbody");
    for (const item of this.trashItems) {
      const checkbox = h("input", { type: "checkbox", ariaLabel: `Select ${item.originalPath}` });
      checkbox.checked = this.trashSelection.has(item.id);
      checkbox.addEventListener("change", () => {
        if (checkbox.checked) this.trashSelection.add(item.id);
        else this.trashSelection.delete(item.id);
        this.updateTrashSelectionUi();
      });
      const restore = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, "Restore");
      restore.addEventListener("click", () => void this.restoreIds([item.id]));
      const purge = h("button", { class: "btn btn-danger btn-sm", type: "button" }, "Delete");
      purge.addEventListener("click", () => void this.purgeIds([item.id]));

      const row = h(
        "tr",
        {},
        h("td", { class: "fs-check" }, checkbox),
        h("td", { class: "fs-trash-path" }, item.originalPath),
        h("td", {}, item.kind === "dir" ? "Folder" : "File"),
        h("td", { class: "num fs-col-size" }, formatBytesShort(item.sizeBytes)),
        h("td", { class: "fs-col-modified", title: formatDateTime(item.deletedAt) }, relativeTime(item.deletedAt)),
        h("td", { class: "fs-actions-col" }, h("div", { class: "row" }, restore, purge)),
      );
      row.classList.toggle("is-selected", this.trashSelection.has(item.id));
      this.trashRows.set(item.id, row);
      body.appendChild(row);
    }
    this.panel.appendChild(
      h("div", { class: "table-wrap fs-table-wrap" }, h("table", { class: "table fs-table" }, h("thead", {}, head), body)),
    );
  }

  private updateTrashSelectionUi(): void {
    for (const [id, row] of this.trashRows) row.classList.toggle("is-selected", this.trashSelection.has(id));
    const count = this.trashSelection.size;
    this.restoreButton.disabled = count === 0;
    this.purgeButton.disabled = count === 0;
    this.emptyBinButton.disabled = this.trashTotal === 0 && this.trashItems.length === 0;
    this.selectionInfo.textContent =
      count > 0 ? `${count} item(s) selected` : this.trashTotal > 0 ? `${this.trashTotal} item(s) in recycle bin` : "";
  }

  private updateSelectionUi(): void {
    if (this.section === "trash") {
      this.detail.classList.remove("is-open");
      this.split.classList.remove("has-detail");
      clear(this.detail);
      this.updateTrashSelectionUi();
      return;
    }
    const items = this.currentEntries();
    for (const entry of items) {
      const selected = this.selection.has(entry.path);
      this.rowEls.get(entry.path)?.classList.toggle("is-selected", selected);
      const box = this.checkboxEls.get(entry.path);
      if (box) box.checked = selected;
    }
    if (this.selectAllBox) {
      const all = items.length > 0 && items.every((entry) => this.selection.has(entry.path));
      this.selectAllBox.checked = all;
      this.selectAllBox.indeterminate = !all && items.some((entry) => this.selection.has(entry.path));
    }
    const count = this.selection.size;
    this.downloadButton.disabled = !this.canDownload();
    this.renameButton.disabled = count !== 1;
    this.copyButton.disabled = count === 0;
    this.moveButton.disabled = count === 0;
    this.deleteButton.disabled = count === 0;
    this.newFolderButton.disabled = this.section !== "files";
    this.uploadButton.disabled = this.section !== "files";
    this.selectionInfo.textContent =
      count > 0 ? `${count} item(s) selected` : this.total > 0 ? `${this.total.toLocaleString()} item(s)` : "";
    this.renderDetail();
  }

  private renderDetail(): void {
    clear(this.detail);
    if (this.section !== "files" || this.selection.size !== 1) {
      this.detail.classList.remove("is-open");
      this.split.classList.remove("has-detail");
      return;
    }
    const entry = listingItems(this.listing).find((candidate) => this.selection.has(candidate.path));
    if (!entry) {
      this.detail.classList.remove("is-open");
      this.split.classList.remove("has-detail");
      return;
    }
    this.detail.classList.add("is-open");
    this.split.classList.add("has-detail");
    const icon = h("div", { class: "fs-detail-icon" });
    icon.appendChild(iconSvg(fileIcon(entry), 34));
    const close = h("button", { class: "fs-detail-close", type: "button", ariaLabel: "Clear selection" }, "×");
    close.addEventListener("click", () => this.clearSelection());
    const rows: Array<[string, string]> = [
      ["Type", entry.kind === "dir" ? "Folder" : entry.mime || "File"],
      ["Size", entry.kind === "dir" ? "—" : formatBytes(entry.size)],
      ["Modified", `${formatDateTime(entry.modifiedAt)} (${relativeTime(entry.modifiedAt)})`],
      ["Permissions", `${entry.readable ? "read" : "no read"} · ${entry.writable ? "write" : "read-only"}`],
    ];
    const dl = h("dl", { class: "kv fs-detail-kv" });
    for (const [label, value] of rows) {
      dl.append(h("dt", {}, label), h("dd", {}, value));
    }
    const actions = h("div", { class: "fs-detail-actions" });
    if (entry.kind === "dir") {
      actions.appendChild(toolButton("Open", "folder", () => this.navigate(entry.path), "primary", undefined, true));
    } else {
      if (previewKind(entry)) {
        actions.appendChild(
          toolButton("Preview", "eye", () => openPreview(this.ctx, this.lc, entry), "primary", undefined, true),
        );
      }
      actions.appendChild(toolButton("Download", "download", () => this.ctx.api.download(entry.path), "ghost", undefined, true));
    }
    actions.appendChild(toolButton("Rename", "rename", () => void this.renameSelected(), "ghost", undefined, true));
    actions.appendChild(toolButton("Delete", "trash", () => void this.deleteSelected(), "danger", undefined, true));

    this.detail.append(
      h("div", { class: "fs-detail-head" }, icon, close),
      h("div", { class: "fs-detail-name" }, entry.name),
      h("div", { class: "fs-detail-path mono small muted" }, entry.path),
      dl,
      actions,
    );
  }

  // ------------------------------------------------------------- selection

  private selectEntry(entry: FileEntry, event: MouseEvent): void {
    if (event.shiftKey && this.anchorPath) {
      this.rangeSelect(entry);
    } else if (event.ctrlKey || event.metaKey) {
      if (this.selection.has(entry.path)) this.selection.delete(entry.path);
      else this.selection.add(entry.path);
      this.anchorPath = entry.path;
    } else {
      this.selectSingle(entry);
    }
    this.updateSelectionUi();
  }

  private selectSingle(entry: FileEntry): void {
    this.selection = new Set([entry.path]);
    this.anchorPath = entry.path;
  }

  private rangeSelect(entry: FileEntry): void {
    const items = this.currentEntries();
    const anchorIndex = this.anchorPath ? items.findIndex((candidate) => candidate.path === this.anchorPath) : -1;
    const index = items.findIndex((candidate) => candidate.path === entry.path);
    if (index < 0) return;
    if (anchorIndex < 0) {
      this.selectSingle(entry);
      return;
    }
    const start = Math.min(anchorIndex, index);
    const end = Math.max(anchorIndex, index);
    for (let i = start; i <= end; i += 1) {
      const item = items[i];
      if (item) this.selection.add(item.path);
    }
  }

  private selectAllOnPage(): void {
    const items = this.currentEntries();
    if (items.length > 0 && items.every((entry) => this.selection.has(entry.path))) {
      this.selection.clear();
    } else {
      for (const entry of items) this.selection.add(entry.path);
    }
    this.updateSelectionUi();
  }

  private clearSelection(): void {
    this.selection.clear();
    this.anchorPath = null;
    this.updateSelectionUi();
  }

  private canDownload(): boolean {
    if (this.selection.size !== 1) return false;
    const entry = listingItems(this.listing).find((candidate) => this.selection.has(candidate.path));
    return entry?.kind === "file";
  }

  // ------------------------------------------------------------- navigation

  private async navigate(path: string): Promise<void> {
    this.section = "files";
    this.path = path || "/";
    this.page = 1;
    this.query = "";
    this.searchInput.value = "";
    this.selection.clear();
    this.anchorPath = null;
    this.listing = null;
    this.total = 0;
    this.loading = true;
    this.ctx.setTitle(`File Station — ${this.path}`);
    this.renderChrome();
    await this.loadListing();
    this.renderUsage();
  }

  private async showSection(section: Section): Promise<void> {
    if (section === this.section) return;
    this.section = section;
    this.selection.clear();
    this.anchorPath = null;
    this.trashSelection.clear();
    if (section === "trash") {
      this.path = "/";
      this.searchInput.value = "";
      this.query = "";
      this.ctx.setTitle("File Station — Recycle bin");
      this.trashPage = 1;
      this.loading = true;
      this.renderChrome();
      await this.loadTrash();
    } else {
      this.loading = true;
      this.ctx.setTitle(`File Station — ${this.path}`);
      this.renderChrome();
      await this.loadListing();
    }
  }

  private setView(view: ViewMode, rerender = true): void {
    this.view = view;
    try {
      window.localStorage.setItem(VIEW_KEY, view);
    } catch {
      /* storage disabled */
    }
    this.listViewButton.classList.toggle("is-active", view === "list");
    this.gridViewButton.classList.toggle("is-active", view === "grid");
    this.listViewButton.setAttribute("aria-pressed", String(view === "list"));
    this.gridViewButton.setAttribute("aria-pressed", String(view === "grid"));
    if (rerender) this.renderPanel();
  }

  private changeSort(key: SortKey): void {
    if (this.sort === key) {
      this.order = this.order === "asc" ? "desc" : "asc";
    } else {
      this.sort = key;
      this.order = key === "name" ? "asc" : "desc";
    }
    this.page = 1;
    this.selection.clear();
    void this.loadListing();
  }

  private onItemContextSelect(entry: FileEntry): void {
    if (!this.selection.has(entry.path)) this.selectSingle(entry);
  }

  private showItemMenu(entry: FileEntry, x: number, y: number): void {
    const items: Array<
      | { label: string; icon?: string; danger?: boolean; disabled?: boolean; onClick: () => void }
      | "separator"
    > = [];
    items.push({
      label: entry.kind === "dir" ? "Open" : previewKind(entry) ? "Preview" : "Open",
      icon: entry.kind === "dir" ? "folder" : "file",
      onClick: () => this.openEntry(entry),
    });
    if (entry.kind === "file") {
      items.push({ label: "Download", icon: "download", onClick: () => this.ctx.api.download(entry.path) });
      items.push("separator");
    }
    items.push(
      { label: "Rename…", icon: "rename", onClick: () => void this.renameSelected() },
      { label: "Copy to…", icon: "copy", onClick: () => void this.transferSelected("copy") },
      { label: "Move to…", icon: "move", onClick: () => void this.transferSelected("move") },
      "separator",
      { label: "Delete", icon: "trash", danger: true, onClick: () => void this.deleteSelected() },
    );
    this.onItemContextSelect(entry);
    this.updateSelectionUi();
    contextMenu(x, y, items);
  }

  private openEntry(entry: FileEntry): void {
    if (entry.kind === "dir") {
      void this.navigate(entry.path);
    } else if (previewKind(entry)) {
      openPreview(this.ctx, this.lc, entry);
    } else {
      this.ctx.toast.info("No inline preview for this file type — use Download to open it locally.");
    }
  }

  // ---------------------------------------------------------------- actions

  private async newFolder(): Promise<void> {
    if (this.section !== "files") return;
    const name = await promptDialog({
      title: "New folder",
      label: "Folder name",
      placeholder: "New folder",
      confirmLabel: "Create",
      validate: validateFileName,
    });
    if (!name) return;
    try {
      await this.ctx.api.post("/api/files/mkdir", { path: joinPath(this.path, name.trim()) });
      this.ctx.toast.success("Folder created");
      await this.loadListing();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Could not create folder: ${errorMessage(error)}`);
    }
  }

  private async renameSelected(): Promise<void> {
    if (this.selection.size !== 1) return;
    const entry = listingItems(this.listing).find((candidate) => this.selection.has(candidate.path));
    if (!entry) return;
    const name = await promptDialog({
      title: "Rename",
      label: "New name",
      value: entry.name,
      confirmLabel: "Rename",
      validate: validateFileName,
    });
    if (!name || name.trim() === entry.name) return;
    try {
      await this.ctx.api.post("/api/files/rename", { path: entry.path, newName: name.trim() });
      this.ctx.toast.success("Renamed");
      this.selection.clear();
      await this.loadListing();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Rename failed: ${errorMessage(error)}`);
    }
  }

  private async downloadSelected(): Promise<void> {
    if (!this.canDownload()) {
      this.ctx.toast.warning("Select a single file to download.");
      return;
    }
    const entry = listingItems(this.listing).find((candidate) => this.selection.has(candidate.path));
    if (entry) this.ctx.api.download(entry.path);
  }

  private async transferSelected(kind: "copy" | "move"): Promise<void> {
    if (this.selection.size === 0) return;
    const entries = listingItems(this.listing).filter((entry) => this.selection.has(entry.path));
    if (entries.length === 0) return;
    const destDir = await pickDestination(this.ctx, this.lc, {
      title: kind === "copy" ? "Copy to folder" : "Move to folder",
      confirmLabel: kind === "copy" ? "Copy here" : "Move here",
      startPath: this.path,
      exclude: new Set(entries.filter((entry) => entry.kind === "dir").map((entry) => entry.path)),
    });
    if (!destDir) return;
    const paths = entries.map((entry) => entry.path);
    try {
      const response = await this.ctx.api.post<JobRef>(kind === "copy" ? "/api/files/copy" : "/api/files/move", {
        paths,
        destDir,
      });
      this.selection.clear();
      this.updateSelectionUi();
      if (response?.jobId) {
        await this.runJob(kind === "copy" ? "Copy" : "Move", response.jobId);
      } else {
        await this.loadListing();
      }
    } catch (error) {
      this.ctx.toast.error(`${kind === "copy" ? "Copy" : "Move"} failed: ${errorMessage(error)}`);
    }
  }

  private async deleteSelected(): Promise<void> {
    if (this.selection.size === 0) return;
    const count = this.selection.size;
    const confirmed = await confirmDialog({
      title: "Move to recycle bin",
      message: `Move ${count} item(s) to the recycle bin? You can restore them later.`,
      confirmLabel: "Move to recycle bin",
      danger: true,
    });
    if (!confirmed) return;
    const paths = [...this.selection];
    try {
      const response = await this.ctx.api.post<JobRef>("/api/files/delete", { paths });
      this.selection.clear();
      this.updateSelectionUi();
      if (response?.jobId) await this.runJob("Delete", response.jobId);
      else await this.loadListing();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Delete failed: ${errorMessage(error)}`);
    }
  }

  private async runJob(label: string, jobId: string): Promise<void> {
    this.setJob({ progress: 0, total: 0, message: label }, jobId);
    const outcome = await watchJob(this.ctx, this.lc, jobId, (update) => this.setJob(update, jobId));
    this.setJob(null, null);
    if (this.disposed) return;
    if (outcome.status === "done") {
      this.ctx.toast.success(`${label} finished`);
    } else if (outcome.status === "failed") {
      this.ctx.toast.error(`${label} failed: ${outcome.error ?? "unknown error"}`);
    } else {
      this.ctx.toast.warning(`${label} was cancelled`);
    }
    await this.loadListing();
    void this.loadUsage();
  }

  private async cancelActiveJob(): Promise<void> {
    const jobId = this.activeJobId;
    if (!jobId) return;
    try {
      await this.ctx.api.post(`/api/jobs/${encodeURIComponent(jobId)}/cancel`);
      this.ctx.toast.info("Cancellation requested");
    } catch (error) {
      this.ctx.toast.error(`Could not cancel: ${errorMessage(error)}`);
    }
  }

  // ------------------------------------------------------------ recycle bin

  private async restoreSelected(): Promise<void> {
    if (this.trashSelection.size === 0) return;
    await this.restoreIds([...this.trashSelection]);
  }

  private async restoreIds(ids: string[]): Promise<void> {
    if (ids.length === 0) return;
    try {
      await this.ctx.api.post("/api/files/trash/restore", { ids });
      this.ctx.toast.success(`${ids.length} item(s) restored`);
      this.trashSelection.clear();
      await this.loadTrash();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Restore failed: ${errorMessage(error)}`);
    }
  }

  private async purgeSelected(): Promise<void> {
    if (this.trashSelection.size === 0) return;
    await this.purgeIds([...this.trashSelection]);
  }

  private async purgeIds(ids: string[]): Promise<void> {
    if (ids.length === 0) return;
    const confirmed = await confirmDialog({
      title: "Delete permanently",
      message: `Permanently delete ${ids.length} item(s)? This cannot be undone.`,
      confirmLabel: "Delete permanently",
      danger: true,
      requireTyped: "DELETE",
    });
    if (!confirmed) return;
    try {
      await this.ctx.api.post("/api/files/trash/delete", { ids });
      this.ctx.toast.success("Permanently deleted");
      this.trashSelection.clear();
      await this.loadTrash();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Permanent delete failed: ${errorMessage(error)}`);
    }
  }

  private async emptyBin(): Promise<void> {
    if (this.trashTotal === 0 && this.trashItems.length === 0) return;
    const confirmed = await confirmDialog({
      title: "Empty recycle bin",
      message: "Permanently delete every item in the recycle bin? This cannot be undone.",
      confirmLabel: "Empty recycle bin",
      danger: true,
      requireTyped: "EMPTY",
    });
    if (!confirmed) return;
    try {
      await this.ctx.api.del("/api/files/trash");
      this.ctx.toast.success("Recycle bin emptied");
      this.trashSelection.clear();
      await this.loadTrash();
      void this.loadUsage();
    } catch (error) {
      this.ctx.toast.error(`Could not empty recycle bin: ${errorMessage(error)}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Small builders
// ---------------------------------------------------------------------------

function toolButton(
  label: string,
  iconName: string,
  onClick: () => void,
  variant: "primary" | "ghost" | "danger" | "subtle" = "ghost",
  ariaLabel?: string,
  small = true,
): HTMLButtonElement {
  const btn = h("button", { class: `btn btn-${variant}${small ? " btn-sm" : ""}`, type: "button" });
  btn.appendChild(iconSvg(iconName, 15));
  if (label) btn.appendChild(document.createTextNode(label));
  if (ariaLabel) btn.setAttribute("aria-label", ariaLabel);
  btn.addEventListener("click", onClick);
  return btn;
}

function viewButton(iconName: string, label: string, onClick: () => void): HTMLButtonElement {
  const btn = h("button", { class: "fs-view-btn", type: "button", ariaLabel: label, title: label });
  btn.appendChild(iconSvg(iconName, 17));
  btn.addEventListener("click", onClick);
  return btn;
}

function sortHeader(
  label: string,
  key: SortKey,
  active: SortKey,
  order: "asc" | "desc",
  onClick: () => void,
  className = "",
): HTMLTableCellElement {
  const btn = h("button", { class: `fs-sort-btn${active === key ? " is-active" : ""}`, type: "button" }, label);
  if (active === key) {
    const arrow = h("span", { class: "fs-sort-arrow" });
    arrow.appendChild(iconSvg(order === "asc" ? "arrowUp" : "arrowDown", 12));
    btn.appendChild(arrow);
  } else {
    const arrow = h("span", { class: "fs-sort-arrow is-idle" });
    arrow.appendChild(iconSvg("sort", 12));
    btn.appendChild(arrow);
  }
  btn.addEventListener("click", onClick);
  const th = h("th", { class: className });
  th.appendChild(btn);
  return th;
}

function hasFileDrag(event: DragEvent): boolean {
  const types = event.dataTransfer?.types;
  if (!types) return false;
  for (let i = 0; i < types.length; i += 1) {
    if (types[i] === "Files") return true;
  }
  return false;
}

function readViewPreference(): ViewMode {
  try {
    const stored = window.localStorage.getItem(VIEW_KEY);
    if (stored === "grid" || stored === "list") return stored;
  } catch {
    /* storage disabled */
  }
  return "list";
}

/** Mount the File Station for the shell. */
export async function mountFileStation(ctx: AppContext): Promise<void> {
  const station = new FileStation(ctx);
  await station.start();
}
