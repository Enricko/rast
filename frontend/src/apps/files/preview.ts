// Safe inline previews: images via <img>, PDFs via a sandboxed iframe and text
// via /api/files/text rendered with textContent (never innerHTML).

import { ApiError } from "../../core/api";
import { h } from "../../core/dom";
import type { AppContext } from "../../core/app";
import type { FileEntry } from "../../core/types";
import { errorMessage, modal } from "../../core/ui";
import { isAbortError, type AppLifecycle } from "../../components/lifecycle";
import { isUnavailableError, unavailableReason, unavailableState } from "../../components/states";
import { formatBytes, formatDateTime } from "../../core/format";
import { ACCESS_CHANGED_ITEM_MESSAGE, previewKind, type TextPayload } from "./types";

const TEXT_PREVIEW_BYTES = 262_144;

export function openPreview(ctx: AppContext, lifecycle: AppLifecycle, entry: FileEntry): void {
  const kind = previewKind(entry);
  const body = h("div", { class: "preview-body" });
  let controller: AbortController | null = null;
  let closed = false;

  const meta = h(
    "div",
    { class: "preview-meta" },
    h("span", {}, entry.mime || "unknown type"),
    h("span", {}, formatBytes(entry.size)),
    h("span", {}, formatDateTime(entry.modifiedAt)),
  );
  const actions = h(
    "div",
    { class: "preview-actions" },
    actionButton("Download", () => ctx.api.download(entry.path)),
  );
  if (kind === "image" || kind === "pdf") {
    const anchor = h(
      "a",
      {
        class: "btn btn-ghost",
        href: ctx.api.previewUrl(entry.path),
        target: "_blank",
        rel: "noopener noreferrer",
      },
      "Open in new tab",
    );
    actions.appendChild(anchor);
  }
  body.append(meta, actions);

  const frame = h("div", { class: "preview-frame" });
  body.appendChild(frame);

  if (kind === "image") {
    const image = h("img", { class: "preview-image" });
    image.loading = "lazy";
    image.decoding = "async";
    image.alt = entry.name;
    image.src = ctx.api.previewUrl(entry.path);
    image.addEventListener("error", () => {
      frame.replaceChildren(
        h(
          "div",
          { class: "preview-note" },
          "The image could not be displayed. It may have been moved, your access may have changed, or the file may be too large for the browser preview.",
        ),
      );
    });
    frame.appendChild(image);
  } else if (kind === "pdf") {
    const iframe = document.createElement("iframe");
    iframe.className = "preview-pdf";
    iframe.title = `PDF preview of ${entry.name}`;
    iframe.setAttribute("sandbox", "allow-same-origin");
    iframe.setAttribute("referrerpolicy", "no-referrer");
    iframe.src = ctx.api.previewUrl(entry.path);
    frame.appendChild(iframe);
    frame.appendChild(
      h(
        "div",
        { class: "preview-note small muted" },
        "If the browser blocks the embedded PDF viewer, use “Open in new tab” or download the file.",
      ),
    );
  } else if (kind === "text") {
    frame.appendChild(
      h("div", { class: "spinner-wrap" }, h("span", { class: "spinner" }), "Loading text…"),
    );
    controller = lifecycle.trackController(new AbortController());
    const request = controller;
    ctx.api
      .get<TextPayload>(
        "/api/files/text",
        { path: entry.path, maxBytes: TEXT_PREVIEW_BYTES },
        { signal: request.signal },
      )
      .then((payload) => {
        if (closed) return;
        const pre = h("pre", { class: "preview-text" });
        pre.textContent = typeof payload?.content === "string" ? payload.content : "";
        frame.replaceChildren(pre);
        if (payload?.truncated) {
          frame.appendChild(
            h(
              "div",
              { class: "preview-note small muted" },
              `Preview truncated at ${formatBytes(TEXT_PREVIEW_BYTES)}. Download the file to see the full content.`,
            ),
          );
        }
      })
      .catch((error: unknown) => {
        if (closed || isAbortError(error)) return;
        if (isUnavailableError(error)) {
          frame.replaceChildren(
            unavailableState({
              title: "Text preview not available",
              reason: unavailableReason(error),
            }),
          );
          return;
        }
        if (error instanceof ApiError && (error.status === 403 || error.status === 404)) {
          frame.replaceChildren(h("div", { class: "preview-note" }, ACCESS_CHANGED_ITEM_MESSAGE));
          return;
        }
        frame.replaceChildren(
          h("div", { class: "preview-note" }, `Could not load preview: ${errorMessage(error)}`),
        );
      })
      .finally(() => lifecycle.releaseController(request));
  } else {
    frame.appendChild(
      h(
        "div",
        { class: "preview-note" },
        `No inline preview is available for “${entry.name}”. Download the file to open it locally.`,
      ),
    );
  }

  modal({
    title: `Preview — ${entry.name}`,
    body,
    wide: true,
    onClose: () => {
      closed = true;
      controller?.abort();
    },
  });
}

function actionButton(label: string, onClick: () => void): HTMLButtonElement {
  const btn = h("button", { class: "btn btn-ghost", type: "button" }, label);
  btn.addEventListener("click", onClick);
  return btn;
}
