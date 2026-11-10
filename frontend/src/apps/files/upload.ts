// Upload queue for File Station: sequential raw-stream uploads with per-file
// progress, cancellation and conflict resolution (overwrite / skip / rename).

import { ApiError } from "../../core/api";
import type { AppContext } from "../../core/app";
import { clear, h } from "../../core/dom";
import { formatBytes } from "../../core/format";
import type { FileEntry } from "../../core/types";
import { errorMessage, modal, promptDialog } from "../../core/ui";
import { isAbortError, type AppLifecycle } from "../../components/lifecycle";
import { progressBar } from "../../components/states";
import { validateFileName } from "./types";

type UploadStatus = "queued" | "uploading" | "done" | "error" | "cancelled";

interface UploadItem {
  id: number;
  file: File;
  name: string;
  status: UploadStatus;
  loaded: number;
  total: number;
  error: string;
  controller: AbortController | null;
}

interface RowRefs {
  root: HTMLElement;
  state: HTMLElement;
  bar: { el: HTMLElement; update: (fraction: number, label?: string) => void; setError: (failed: boolean) => void };
  cancel: HTMLButtonElement;
  remove: () => void;
}

export interface UploadManagerOptions {
  /** Directory that new uploads should land in (current listing path). */
  currentDir: () => string;
  /** Called after at least one upload succeeded. */
  onUploaded: (entry: FileEntry) => void;
  /** Called when the queue drains (including cancelled/error items). */
  onDrained: () => void;
  /** Human-readable max upload size from store; 0/undefined = unknown. */
  maxUploadBytes?: () => number;
}

let nextUploadId = 1;

export class UploadManager {
  readonly el: HTMLElement;
  private readonly ctx: AppContext;
  private readonly lifecycle: AppLifecycle;
  private readonly options: UploadManagerOptions;
  private readonly header: HTMLElement;
  private readonly headerText: HTMLElement;
  private readonly list: HTMLElement;
  private readonly rows = new Map<number, RowRefs>();
  private items: UploadItem[] = [];
  private running = false;
  private hideTimer = 0;
  private cancelAllButton: HTMLButtonElement;

  constructor(ctx: AppContext, lifecycle: AppLifecycle, options: UploadManagerOptions) {
    this.ctx = ctx;
    this.lifecycle = lifecycle;
    this.options = options;

    this.headerText = h("span", { class: "upload-head-text" });
    this.cancelAllButton = h("button", { class: "btn btn-subtle btn-sm", type: "button" }, "Cancel all");
    this.cancelAllButton.addEventListener("click", () => this.cancelAll());
    this.header = h(
      "div",
      { class: "upload-head" },
      this.headerText,
      this.cancelAllButton,
      closeButton(() => this.dismiss()),
    );
    this.list = h("div", { class: "upload-list" });
    this.el = h("div", { class: "upload-tray hidden", role: "region", ariaLabel: "Uploads" }, this.header, this.list);
  }

  get busy(): boolean {
    return this.running || this.items.some((item) => item.status === "queued" || item.status === "uploading");
  }

  enqueue(files: FileList | File[], targetDir?: string): void {
    const incoming: File[] = [];
    if (Array.isArray(files)) incoming.push(...files);
    else {
      for (let i = 0; i < files.length; i += 1) {
        const file = files.item(i);
        if (file) incoming.push(file);
      }
    }
    if (incoming.length === 0) return;

    const max = this.options.maxUploadBytes?.() ?? 0;
    const rejected: string[] = [];
    for (const file of incoming) {
      if (max > 0 && file.size > max) {
        rejected.push(`${file.name} (${formatBytes(file.size)} > ${formatBytes(max)})`);
        continue;
      }
      this.items.push({
        id: nextUploadId++,
        file,
        name: file.name,
        status: "queued",
        loaded: 0,
        total: file.size,
        error: "",
        controller: null,
      });
    }
    if (rejected.length > 0) {
      this.ctx.toast.error(
        `Skipped ${rejected.length} file(s) larger than the ${formatBytes(max)} upload limit: ${rejected.join(", ")}`,
      );
    }
    if (this.items.length === 0) return;
    this.renderList();
    const dir = targetDir ?? this.options.currentDir();
    void this.run(dir);
  }

  dismiss(): void {
    this.cancelAll();
    this.items = [];
    this.rows.clear();
    clear(this.list);
    this.el.classList.add("hidden");
  }

  private async run(dir: string): Promise<void> {
    if (this.running) return;
    this.running = true;
    if (this.hideTimer) {
      this.lifecycle.clearTimeout(this.hideTimer);
      this.hideTimer = 0;
    }
    this.el.classList.remove("hidden");
    this.updateHeader();

    for (;;) {
      const item = this.items.find((candidate) => candidate.status === "queued");
      if (!item || this.lifecycle.isDisposed) break;
      await this.uploadOne(item, dir);
    }

    this.running = false;
    this.updateHeader();
    this.options.onDrained();
    const hasWork = this.items.some((item) => item.status === "queued" || item.status === "uploading");
    if (!hasWork) {
      this.hideTimer = this.lifecycle.setTimeout(() => {
        this.el.classList.add("hidden");
        this.items = [];
        this.rows.clear();
        clear(this.list);
        this.updateHeader();
      }, 4000);
    }
  }

  private async uploadOne(item: UploadItem, dir: string): Promise<void> {
    item.status = "uploading";
    item.controller = new AbortController();
    this.patchRow(item);

    let attemptName = item.name;
    let overwrite = false;
    for (;;) {
      try {
        const entry = await this.ctx.api.upload(dir, item.file, {
          name: attemptName,
          overwrite,
          signal: item.controller.signal,
          onProgress: (loaded, total) => {
            item.loaded = loaded;
            item.total = total || item.file.size;
            this.patchRowProgress(item);
          },
        });
        item.status = "done";
        item.loaded = item.total;
        this.patchRow(item);
        this.options.onUploaded(entry as FileEntry);
        return;
      } catch (error) {
        if (isAbortError(error) || (error instanceof ApiError && error.code === "aborted")) {
          item.status = "cancelled";
          this.patchRow(item);
          return;
        }
        if (error instanceof ApiError && error.status === 409) {
          const resolution = await this.resolveConflict(item.name);
          if (resolution === "cancel-all") {
            item.status = "cancelled";
            this.patchRow(item);
            this.cancelAll();
            return;
          }
          if (resolution === "skip") {
            item.status = "cancelled";
            item.error = "Skipped (name already exists)";
            this.patchRow(item);
            return;
          }
          if (resolution.overwrite) {
            overwrite = true;
            continue;
          }
          attemptName = resolution.name;
          continue;
        }
        item.status = "error";
        item.error = errorMessage(error);
        this.patchRow(item);
        return;
      }
    }
  }

  private resolveConflict(
    name: string,
  ): Promise<{ overwrite: true; name: string } | { overwrite: false; name: string } | "skip" | "cancel-all"> {
    return new Promise((resolve) => {
      let settled = false;
      const finish = (value: { overwrite: true; name: string } | { overwrite: false; name: string } | "skip" | "cancel-all") => {
        if (settled) return;
        settled = true;
        close();
        resolve(value);
      };
      const body = h(
        "div",
        { class: "col" },
        h("p", {}, `“${name}” already exists in this folder.`),
        h("p", { class: "small muted" }, "Choose what should happen to the new file. The existing file is never removed unless you choose Overwrite."),
      );
      const close = modal({
        title: "File name conflict",
        body,
        onClose: () => {
          if (!settled) {
            settled = true;
            resolve("skip");
          }
        },
        actions: [
          { label: "Skip", variant: "ghost", onClick: () => finish("skip") },
          {
            label: "Rename…",
            variant: "ghost",
            keepOpen: true,
            onClick: async () => {
              const next = await promptDialog({
                title: "Rename upload",
                label: "New file name",
                value: name,
                confirmLabel: "Use name",
                validate: validateFileName,
              });
              if (next) finish({ overwrite: false, name: next.trim() });
            },
          },
          {
            label: "Overwrite",
            variant: "danger",
            onClick: () => finish({ overwrite: true, name }),
          },
        ],
      });
    });
  }

  private cancelAll(): void {
    for (const item of this.items) {
      if (item.status === "queued") {
        item.status = "cancelled";
        this.patchRow(item);
      } else if (item.status === "uploading") {
        item.controller?.abort();
      }
    }
    this.updateHeader();
  }

  private renderList(): void {
    this.rows.clear();
    clear(this.list);
    // Keep the DOM bounded even for huge drops.
    const visible = this.items.slice(0, 100);
    for (const item of visible) this.list.appendChild(this.buildRow(item));
    if (this.items.length > visible.length) {
      this.list.appendChild(
        h("div", { class: "upload-more small muted" }, `…and ${this.items.length - visible.length} more file(s)`),
      );
    }
    this.updateHeader();
  }

  private buildRow(item: UploadItem): HTMLElement {
    const name = h("div", { class: "upload-name" }, item.name);
    name.title = item.name;
    const state = h("span", { class: "upload-state small muted" }, "Queued");
    const bar = progressBar();
    const cancelBtn = h("button", { class: "upload-cancel", type: "button", ariaLabel: `Cancel upload of ${item.name}` });
    cancelBtn.addEventListener("click", () => {
      if (item.status === "queued") {
        item.status = "cancelled";
        this.patchRow(item);
      } else if (item.status === "uploading") {
        item.controller?.abort();
      }
    });
    const removeBtn = h("button", { class: "upload-cancel", type: "button", ariaLabel: `Dismiss ${item.name}` });
    removeBtn.textContent = "×";
    removeBtn.addEventListener("click", () => {
      this.items = this.items.filter((candidate) => candidate.id !== item.id);
      this.rows.get(item.id)?.root.remove();
      this.rows.delete(item.id);
      this.updateHeader();
    });
    removeBtn.classList.add("hidden");
    const root = h(
      "div",
      { class: "upload-item" },
      h("div", { class: "upload-row" }, name, state, cancelBtn, removeBtn),
      bar.el,
    );
    this.rows.set(item.id, { root, state, bar, cancel: cancelBtn, remove: () => removeBtn.classList.remove("hidden") });
    return root;
  }

  private patchRow(item: UploadItem): void {
    const row = this.rows.get(item.id);
    if (!row) return;
    row.root.dataset.status = item.status;
    row.bar.setError(item.status === "error");
    const fraction = item.total > 0 ? item.loaded / item.total : 0;
    row.bar.update(item.status === "done" ? 1 : fraction);
    row.state.textContent = labelFor(item);
    if (item.status === "uploading") {
      row.cancel.textContent = "Cancel";
      row.cancel.classList.remove("hidden");
    } else {
      row.cancel.classList.add("hidden");
      row.remove();
    }
    if (item.status === "error" && item.error) row.root.title = item.error;
    this.updateHeader();
  }

  private patchRowProgress(item: UploadItem): void {
    const row = this.rows.get(item.id);
    if (!row) return;
    const fraction = item.total > 0 ? item.loaded / item.total : 0;
    row.bar.update(fraction);
    row.state.textContent = `${formatBytes(item.loaded)} / ${formatBytes(item.total)}`;
  }

  private updateHeader(): void {
    const active = this.items.filter((item) => item.status === "uploading" || item.status === "queued").length;
    const done = this.items.filter((item) => item.status === "done").length;
    const failed = this.items.filter((item) => item.status === "error").length;
    if (this.items.length === 0) {
      this.headerText.textContent = "No uploads";
    } else if (active > 0) {
      this.headerText.textContent = `Uploading — ${done} done, ${active} remaining${failed ? `, ${failed} failed` : ""}`;
    } else {
      this.headerText.textContent = `Uploads finished — ${done} done${failed ? `, ${failed} failed` : ""}`;
    }
    this.cancelAllButton.classList.toggle("hidden", active === 0);
  }
}

function labelFor(item: UploadItem): string {
  switch (item.status) {
    case "queued":
      return "Queued";
    case "uploading":
      return "Uploading…";
    case "done":
      return `Done · ${formatBytes(item.total || item.file.size)}`;
    case "cancelled":
      return item.error || "Cancelled";
    case "error":
      return item.error || "Failed";
    default:
      return "";
  }
}

function closeButton(onClick: () => void): HTMLButtonElement {
  const btn = h("button", { class: "upload-close", type: "button", ariaLabel: "Dismiss uploads" }, "×");
  btn.addEventListener("click", onClick);
  return btn;
}
