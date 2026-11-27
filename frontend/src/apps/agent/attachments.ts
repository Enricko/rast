// AI Agent attachments: files added to the message box (the attach button,
// dropped on the chat or pasted) upload at once and show as chips above the
// text; sent messages show theirs (images as pictures, other files as chips
// that download). Files: images, PDFs and text, up to 10 MiB each (see
// src/ai/files.rs).

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { formatBytes } from "../../core/format";
import { iconSvg } from "../../core/icons";
import { errorMessage } from "../../core/ui";
import { MAX_FILE_BYTES, MAX_FILES, fileUrl, type FileBlock } from "./model";

interface Pending {
  key: number;
  name: string;
  status: "uploading" | "ready" | "error";
  stored?: FileBlock;
  error?: string;
  abort: AbortController;
  preview?: string;
  el: HTMLElement;
}

export interface Attachments {
  /** The chips above the text (hidden while empty). */
  tray: HTMLElement;
  /** The attach button for the composer bar. */
  button: HTMLElement;
  add(files: Iterable<File>): void;
  /** Files ready to send, in order. */
  ready(): FileBlock[];
  /** Whether some file is still uploading. */
  busy(): boolean;
  /** After sending: forget them (they belong to the message now). */
  sent(): void;
  /** Leaving: take every unsent file out. */
  clear(): void;
}

const KIND_LABEL: Record<FileBlock["kind"], string> = { image: "Image", pdf: "PDF", text: "Text" };

function picture(src: string, alt: string, className = ""): HTMLImageElement {
  const img = document.createElement("img");
  if (className) img.className = className;
  img.alt = alt;
  img.loading = "lazy";
  img.decoding = "async";
  img.src = src;
  return img;
}

export function createAttachments(ctx: AppContext, onChange: () => void): Attachments {
  const items: Pending[] = [];
  let nextKey = 1;
  const tray = h("div", { class: "ai-attachments hidden", role: "list", ariaLabel: "Files to send" });
  const picker = h("input", { type: "file", multiple: true, class: "hidden", tabIndex: -1, ariaHidden: true }) as HTMLInputElement;
  const button = h(
    "button",
    { class: "ai-chip ai-attach", type: "button", title: "Add files: images, PDFs or text (up to 10 MiB each)", ariaLabel: "Add files" },
    iconSvg("attach", 14),
  );
  button.addEventListener("click", () => picker.click());
  picker.addEventListener("change", () => {
    add(Array.from(picker.files ?? []));
    picker.value = "";
  });
  ctx.root.appendChild(picker);

  function render(): void {
    tray.classList.toggle("hidden", items.length === 0);
    tray.replaceChildren(...items.map((item) => item.el));
    onChange();
  }

  function chip(item: Pending, file: File): HTMLElement {
    const isImage = file.type.startsWith("image/") && !file.type.includes("svg");
    if (isImage) item.preview = URL.createObjectURL(file);
    const remove = h(
      "button",
      { class: "ai-attachment-remove", type: "button", title: "Remove", ariaLabel: `Remove ${file.name}` },
      iconSvg("close", 12),
    );
    remove.addEventListener("click", () => drop(item));
    return h(
      "div",
      { class: "ai-attachment is-uploading", role: "listitem", title: file.name },
      item.preview ? picture(item.preview, "", "ai-attachment-thumb") : h("span", { class: "ai-attachment-icon" }, iconSvg("file", 16)),
      h(
        "span",
        { class: "ai-attachment-text" },
        h("span", { class: "ai-attachment-name" }, file.name),
        h("span", { class: "ai-attachment-detail" }, "Uploading…"),
      ),
      h("span", { class: "ai-attachment-bar", ariaHidden: true }, h("span", {})),
      remove,
    );
  }

  function setDetail(item: Pending, text: string): void {
    const detail = item.el.querySelector(".ai-attachment-detail");
    if (detail) detail.textContent = text;
  }

  function add(files: Iterable<File>): void {
    for (const file of files) {
      if (items.length >= MAX_FILES) {
        ctx.toast.warning(`A message takes up to ${MAX_FILES} files.`);
        break;
      }
      if (file.size > MAX_FILE_BYTES) {
        ctx.toast.warning(`${file.name} is larger than 10 MiB.`);
        continue;
      }
      const item: Pending = { key: nextKey++, name: file.name, status: "uploading", abort: new AbortController(), el: h("div") };
      item.el = chip(item, file);
      items.push(item);
      void upload(item, file);
    }
    render();
  }

  async function upload(item: Pending, file: File): Promise<void> {
    const bar = item.el.querySelector<HTMLElement>(".ai-attachment-bar > span");
    try {
      const stored = await ctx.api.uploadRaw<Omit<FileBlock, "type">>(`/api/ai/files?${new URLSearchParams({ name: file.name || "pasted" })}`, file, {
        signal: item.abort.signal,
        onProgress: (loaded, total) => {
          if (bar) bar.style.width = `${Math.round((loaded / Math.max(1, total)) * 100)}%`;
        },
      });
      if (!items.includes(item)) {
        // Removed while it uploaded.
        void ctx.api.del(fileUrl(stored.id)).catch(() => undefined);
        return;
      }
      item.status = "ready";
      item.stored = { type: "file", ...stored };
      item.el.classList.remove("is-uploading");
      setDetail(item, `${KIND_LABEL[stored.kind] ?? "File"} · ${formatBytes(stored.size)}`);
    } catch (error) {
      if (!items.includes(item)) return;
      item.status = "error";
      item.error = errorMessage(error);
      item.el.classList.remove("is-uploading");
      item.el.classList.add("is-error");
      item.el.title = item.error;
      setDetail(item, item.error);
    }
    onChange();
  }

  function drop(item: Pending): void {
    const at = items.indexOf(item);
    if (at < 0) return;
    items.splice(at, 1);
    if (item.status === "uploading") item.abort.abort();
    if (item.stored) void ctx.api.del(fileUrl(item.stored.id)).catch(() => undefined);
    if (item.preview) URL.revokeObjectURL(item.preview);
    render();
  }

  return {
    tray,
    button,
    add,
    ready: () => items.flatMap((item) => (item.status === "ready" && item.stored ? [item.stored] : [])),
    busy: () => items.some((item) => item.status === "uploading"),
    sent: () => {
      for (const item of items.splice(0)) if (item.preview) URL.revokeObjectURL(item.preview);
      render();
    },
    clear: () => {
      for (const item of [...items]) drop(item);
    },
  };
}

/** A sent message's files: pictures for images, chips for the rest. */
export function sentFiles(files: FileBlock[]): HTMLElement | null {
  if (!files.length) return null;
  return h(
    "div",
    { class: "ai-msg-files" },
    ...files.map((file) =>
      file.kind === "image"
        ? h(
            "a",
            { class: "ai-file-picture", href: fileUrl(file.id), target: "_blank", rel: "noopener", title: file.name },
            picture(fileUrl(file.id), file.name),
          )
        : h(
            "a",
            { class: "ai-file", href: fileUrl(file.id), download: file.name, title: `Download ${file.name}` },
            h("span", { class: "ai-attachment-icon" }, iconSvg("file", 16)),
            h(
              "span",
              { class: "ai-attachment-text" },
              h("span", { class: "ai-attachment-name" }, file.name),
              h("span", { class: "ai-attachment-detail" }, `${KIND_LABEL[file.kind] ?? "File"} · ${formatBytes(file.size)}`),
            ),
          ),
    ),
  );
}
