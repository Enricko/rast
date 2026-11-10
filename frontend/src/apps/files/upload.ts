// Upload queue for File Station: sequential raw-stream uploads with per-file
// progress, cancellation and conflict resolution (overwrite / skip / rename).
//
// Small files (<= 90 MiB) use the single-request streaming endpoint. Larger
// files use the chunked/resumable endpoints so reverse proxies (e.g.
// Cloudflare's 100 MB request cap) never see one huge body:
//   POST /api/files/upload/init      -> uploadId + chunkSize
//   POST /api/files/upload/chunk     -> raw chunk at the current offset
//   POST /api/files/upload/complete  -> final file entry
//   DELETE /api/files/upload         -> cancel + release the quota reservation

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

/** Files above this size are uploaded with resumable chunks. */
const CHUNKED_THRESHOLD = 90 * 1024 * 1024;
/** Used when the server response omits a chunk size. */
const FALLBACK_CHUNK_SIZE = 64 * 1024 * 1024;
const MAX_CHUNK_RETRIES = 3;
const CHUNK_RETRY_BASE_MS = 400;

interface UploadItem {
  id: number;
  file: File;
  name: string;
  status: UploadStatus;
  loaded: number;
  total: number;
  error: string;
  controller: AbortController | null;
  /** Server-side chunked upload session id (null for small uploads). */
  uploadId: string | null;
  /** Current chunk number / total chunks for chunked uploads. */
  part: number;
  parts: number;
}

interface RowRefs {
  root: HTMLElement;
  state: HTMLElement;
  bar: { el: HTMLElement; update: (fraction: number, label?: string) => void; setError: (failed: boolean) => void };
  cancel: HTMLButtonElement;
  remove: () => void;
}

interface InitUploadResponse {
  uploadId: string;
  chunkSize: number;
  offset: number;
  size: number;
  expiresAt: number;
}

interface ChunkResponse {
  uploadId?: string;
  received?: number;
  size?: number;
  offset?: number;
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

/** Error thrown by a chunk request, optionally telling us where to resume. */
class ChunkUploadError extends ApiError {
  readonly expectedOffset: number | null;

  constructor(status: number, code: string, message: string, expectedOffset: number | null) {
    super(status, code, message);
    this.name = "ChunkUploadError";
    this.expectedOffset = expectedOffset;
  }
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
        uploadId: null,
        part: 0,
        parts: file.size > CHUNKED_THRESHOLD ? chunkCount(file.size, FALLBACK_CHUNK_SIZE) : 1,
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
    try {
      if (item.file.size > CHUNKED_THRESHOLD) {
        await this.uploadChunked(item, dir);
      } else {
        await this.uploadSingle(item, dir);
      }
    } catch (error) {
      if (item.status === "uploading") {
        item.status = "error";
        item.error = friendlyUploadError(error);
        this.patchRow(item);
      }
    }
  }

  // ------------------------------------------------------- single request

  private async uploadSingle(item: UploadItem, dir: string): Promise<void> {
    const controller = item.controller;
    if (!controller) return;
    let attemptName = item.name;
    let overwrite = false;
    for (;;) {
      try {
        const entry = await this.ctx.api.upload(dir, item.file, {
          name: attemptName,
          overwrite,
          signal: controller.signal,
          onProgress: (loaded, total) => {
            item.loaded = loaded;
            item.total = total || item.file.size;
            this.patchRowProgress(item);
          },
        });
        this.finishItem(item, entry as FileEntry);
        return;
      } catch (error) {
        if (this.isCancelled(item, error)) return;
        if (error instanceof ApiError && error.status === 409) {
          const resolution = await this.resolveConflict(item.name);
          if (resolution === "cancel-all") {
            this.cancelItem(item, "Cancelled");
            this.cancelAll();
            return;
          }
          if (resolution === "skip") {
            this.cancelItem(item, "Skipped (name already exists)");
            return;
          }
          if (resolution.overwrite) {
            overwrite = true;
            continue;
          }
          attemptName = resolution.name;
          continue;
        }
        throw error;
      }
    }
  }

  // ------------------------------------------------------- chunked request

  private async uploadChunked(item: UploadItem, dir: string): Promise<void> {
    const controller = item.controller;
    if (!controller) return;
    const init = await this.initChunked(item, dir, controller.signal);
    if (!init) return; // cancelled/skipped while resolving conflicts
    item.uploadId = init.uploadId;

    const size = item.file.size;
    const chunkSize = resolveChunkSize(init.chunkSize);
    const parts = chunkCount(size, chunkSize);
    item.parts = parts;
    let offset = Math.min(size, Math.max(0, Number(init.offset) || 0));
    item.loaded = offset;
    let retries = 0;

    while (offset < size) {
      if (controller.signal.aborted) {
        await this.cancelRemote(item);
        item.status = "cancelled";
        this.patchRow(item);
        return;
      }
      const end = Math.min(size, offset + chunkSize);
      const blob = item.file.slice(offset, end);
      item.part = Math.min(parts, Math.floor(offset / chunkSize) + 1);
      this.patchRow(item);
      try {
        const result = await sendChunk(
          init.uploadId,
          offset,
          blob,
          this.ctx.api.getCsrfToken(),
          controller.signal,
          (loadedInChunk) => {
            item.loaded = offset + Math.min(blob.size, loadedInChunk);
            this.patchRowProgress(item);
          },
        );
        const nextOffset =
          typeof result?.offset === "number" && result.offset >= offset ? Math.min(size, result.offset) : offset + blob.size;
        offset = nextOffset;
        item.loaded = offset;
        retries = 0;
        this.patchRowProgress(item);
      } catch (error) {
        if (controller.signal.aborted || this.isCancelled(item, error)) {
          await this.cancelRemote(item);
          if (item.status === "uploading") {
            item.status = "cancelled";
            this.patchRow(item);
          }
          return;
        }
        if (error instanceof ApiError && error.status === 401) {
          this.notifySessionLost();
          await this.cancelRemote(item);
          throw new Error("Your session expired. Sign in again to continue the upload.");
        }
        if (error instanceof ApiError && error.status === 413) {
          await this.cancelRemote(item);
          throw new Error("The server rejected this upload: storage quota exceeded.");
        }
        if (error instanceof ApiError && error.status === 404) {
          item.uploadId = null;
          throw new Error("The upload session expired on the server. Upload the file again.");
        }
        // Offset mismatch (409) carries the authoritative offset in the
        // message; resume from there instead of resending from scratch.
        if (error instanceof ChunkUploadError && error.status === 409 && error.expectedOffset !== null) {
          offset = Math.min(size, Math.max(0, error.expectedOffset));
          item.loaded = offset;
          retries = 0;
          this.patchRowProgress(item);
          continue;
        }
        retries += 1;
        if (retries > MAX_CHUNK_RETRIES) {
          await this.cancelRemote(item);
          throw error;
        }
        await abortableSleep(CHUNK_RETRY_BASE_MS * retries, controller.signal);
      }
    }

    // Finish the session (retry once on transient failures).
    let entry: FileEntry | null = null;
    let completeAttempt = 0;
    while (entry === null) {
      try {
        entry = await this.ctx.api.post<FileEntry>(
          `/api/files/upload/complete?uploadId=${encodeURIComponent(init.uploadId)}`,
        );
      } catch (error) {
        if (this.isCancelled(item, error)) {
          await this.cancelRemote(item);
          item.status = "cancelled";
          this.patchRow(item);
          return;
        }
        completeAttempt += 1;
        if (completeAttempt > 1) {
          await this.cancelRemote(item);
          throw error;
        }
        await abortableSleep(CHUNK_RETRY_BASE_MS, controller.signal);
      }
    }
    item.uploadId = null;
    item.loaded = size;
    this.finishItem(item, entry);
  }

  private async initChunked(item: UploadItem, dir: string, signal: AbortSignal): Promise<InitUploadResponse | null> {
    let attemptName = item.name;
    let overwrite = false;
    for (;;) {
      if (signal.aborted) {
        item.status = "cancelled";
        this.patchRow(item);
        return null;
      }
      try {
        return await this.ctx.api.post<InitUploadResponse>("/api/files/upload/init", {
          dir,
          name: attemptName,
          size: item.file.size,
          overwrite,
        });
      } catch (error) {
        if (this.isCancelled(item, error)) return null;
        if (error instanceof ApiError && error.status === 409 && !overwrite) {
          const resolution = await this.resolveConflict(item.name);
          if (resolution === "cancel-all") {
            this.cancelItem(item, "Cancelled");
            this.cancelAll();
            return null;
          }
          if (resolution === "skip") {
            this.cancelItem(item, "Skipped (name already exists)");
            return null;
          }
          if (resolution.overwrite) overwrite = true;
          else attemptName = resolution.name;
          continue;
        }
        if (error instanceof ApiError && error.status === 413) {
          throw new Error("Not enough storage quota for this file. Free up space or ask an administrator.");
        }
        if (error instanceof ApiError && error.status === 401) {
          this.notifySessionLost();
          throw new Error("Your session expired. Sign in again to continue the upload.");
        }
        throw error;
      }
    }
  }

  private async cancelRemote(item: UploadItem): Promise<void> {
    const uploadId = item.uploadId;
    if (!uploadId) return;
    item.uploadId = null;
    try {
      await this.ctx.api.del("/api/files/upload", { uploadId });
    } catch {
      // Best effort: the server expires abandoned sessions on its own.
    }
  }

  private isCancelled(item: UploadItem, error: unknown): boolean {
    if (item.controller?.signal.aborted) return true;
    if (isAbortError(error)) return true;
    return error instanceof ApiError && error.code === "aborted";
  }

  private cancelItem(item: UploadItem, reason: string): void {
    item.status = "cancelled";
    item.error = reason;
    this.patchRow(item);
  }

  private finishItem(item: UploadItem, entry: FileEntry): void {
    item.status = "done";
    item.loaded = item.total;
    this.patchRow(item);
    this.options.onUploaded(entry as FileEntry);
  }

  private notifySessionLost(): void {
    // A regular API request flips the shell's session-lost listener on 401.
    void this.ctx.api.get("/api/auth/me").catch(() => undefined);
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
    row.state.textContent = progressLabel(item);
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

// ---------------------------------------------------------------------------
// Chunk transport helpers
// ---------------------------------------------------------------------------

function resolveChunkSize(value: unknown): number {
  const size = typeof value === "number" && Number.isFinite(value) ? Math.floor(value) : 0;
  if (size <= 0) return FALLBACK_CHUNK_SIZE;
  return Math.min(size, 256 * 1024 * 1024);
}

function chunkCount(size: number, chunkSize: number): number {
  return Math.max(1, Math.ceil(size / chunkSize));
}

function parseExpectedOffset(message: string): number | null {
  const match = /expected\s+(\d+)\s+bytes/i.exec(message);
  if (!match) return null;
  const value = Number(match[1]);
  return Number.isFinite(value) && value >= 0 ? value : null;
}

/** Send one raw chunk with upload progress; resolves with the server offset. */
function sendChunk(
  uploadId: string,
  offset: number,
  blob: Blob,
  csrfToken: string,
  signal: AbortSignal,
  onProgress: (loaded: number) => void,
): Promise<ChunkResponse> {
  return new Promise<ChunkResponse>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(
      "POST",
      `/api/files/upload/chunk?uploadId=${encodeURIComponent(uploadId)}&offset=${offset}`,
    );
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.setRequestHeader("X-CSRF-Token", csrfToken);
    xhr.withCredentials = true;
    xhr.upload.onprogress = (event) => {
      if (event.lengthComputable) onProgress(event.loaded);
    };
    xhr.onload = () => {
      let payload: unknown = null;
      try {
        payload = JSON.parse(xhr.responseText);
      } catch {
        payload = null;
      }
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve((payload ?? {}) as ChunkResponse);
        return;
      }
      const err = (payload as { error?: { code?: string; message?: string }; offset?: number } | null)?.error;
      const message = err?.message ?? `Chunk upload failed (${xhr.status})`;
      const expected =
        typeof (payload as { offset?: unknown } | null)?.offset === "number"
          ? Number((payload as { offset: number }).offset)
          : parseExpectedOffset(message);
      reject(new ChunkUploadError(xhr.status, err?.code ?? "chunk_failed", message, expected));
    };
    xhr.onerror = () => reject(new ApiError(0, "network_error", "Upload connection failed"));
    xhr.onabort = () => reject(new ApiError(0, "aborted", "Upload cancelled"));
    if (signal.aborted) {
      xhr.abort();
      return;
    }
    signal.addEventListener("abort", () => xhr.abort(), { once: true });
    xhr.send(blob);
  });
}

function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const timer = window.setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      window.clearTimeout(timer);
      resolve();
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

function friendlyUploadError(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 413) return "Not enough storage quota for this upload.";
    if (error.status === 429) return "Too many uploads right now — wait a moment and retry.";
    if (error.status === 401) return "Your session expired. Sign in again.";
    if (error.status === 409) return `Upload conflict: ${error.message}`;
    return error.message || `Upload failed (${error.status})`;
  }
  return errorMessage(error);
}

function labelFor(item: UploadItem): string {
  switch (item.status) {
    case "queued":
      return "Queued";
    case "uploading":
      return progressLabel(item);
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

function progressLabel(item: UploadItem): string {
  if (item.parts > 1) {
    return `Part ${Math.max(1, item.part)}/${item.parts} · ${formatBytes(item.loaded)} / ${formatBytes(item.total)}`;
  }
  return `${formatBytes(item.loaded)} / ${formatBytes(item.total)}`;
}

function closeButton(onClick: () => void): HTMLButtonElement {
  const btn = h("button", { class: "upload-close", type: "button", ariaLabel: "Dismiss uploads" }, "×");
  btn.addEventListener("click", onClick);
  return btn;
}
