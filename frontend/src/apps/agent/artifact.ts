// AI Agent artifacts: what the model showed with the `artifact` tool, as a
// card in the chat and a panel beside it. Web pages and SVG pictures show in
// a sandboxed frame the server serves (scripts run, but in an opaque origin
// without RAST's session or network: see src/api/ai.rs `get_artifact`);
// documents render as Markdown; code shows as text. The panel has every
// version of the artifact, Preview / Code, Copy, Download and (pages) Open
// in a new tab.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { copyText } from "../../core/ui";
import { renderMarkdown } from "./markdown";
import { ARTIFACT_KINDS, artifactFileName, artifactMime, type ArtifactVersion, type ToolBlock } from "./model";

export interface ArtifactPane {
  el: HTMLElement;
  /** Show `callId` (one of `list`, the chat's artifacts). */
  show(list: ArtifactVersion[], callId: string, conversationId: string): void;
  /** The chat's artifacts changed: keep the shown one current. */
  update(list: ArtifactVersion[]): void;
  close(): void;
  /** The version shown, or null when closed. */
  shown(): string | null;
}

type Mode = "preview" | "code";

export function artifactUrl(conversationId: string, callId: string): string {
  return `/api/ai/conversations/${encodeURIComponent(conversationId)}/artifacts/${encodeURIComponent(callId)}`;
}

/** Whether the kind shows as a page in a frame (else in the app). */
const framed = (artifact: ArtifactVersion) => artifact.kind === "html" || artifact.kind === "svg";

export function createArtifactPane(ctx: AppContext, onClose: () => void): ArtifactPane {
  let list: ArtifactVersion[] = [];
  let shown: ArtifactVersion | null = null;
  let conversation = "";
  let mode: Mode = "preview";
  let frameFor = "";

  const title = h("div", { class: "ai-artifact-title" });
  const sub = h("div", { class: "ai-artifact-sub small muted" });
  const button = (icon: string, label: string, onClick: () => void) =>
    h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: label, ariaLabel: label, onClick }, iconSvg(icon, 15));
  const older = button("chevronLeft", "Earlier version", () => step(-1));
  const newer = button("chevronRight", "Later version", () => step(1));
  const versions = h("div", { class: "ai-artifact-versions" }, older, newer);
  const previewTab = h("button", { class: "segment", type: "button", onClick: () => setMode("preview") }, "Preview");
  const codeTab = h("button", { class: "segment", type: "button", onClick: () => setMode("code") }, "Code");
  const modes = h("div", { class: "segmented ai-artifact-modes", role: "group", ariaLabel: "Show" }, previewTab, codeTab);
  const openTab = h(
    "a",
    { class: "btn btn-ghost btn-icon btn-sm", target: "_blank", rel: "noopener", title: "Open in a new tab", ariaLabel: "Open in a new tab" },
    iconSvg("external", 15),
  );
  const actions = h(
    "div",
    { class: "ai-artifact-actions" },
    button("copy", "Copy", () => void copy()),
    button("download", "Download", () => download()),
    openTab,
    button("close", "Close", () => onClose()),
  );
  const head = h("header", { class: "ai-artifact-head" }, h("div", { class: "ai-artifact-titles" }, title, sub), versions, modes, actions);
  const body = h("div", { class: "ai-artifact-body" });
  const el = h("aside", { class: "ai-artifact", ariaLabel: "Artifact" }, head, body);
  el.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && shown) {
      event.preventDefault();
      event.stopPropagation();
      onClose();
    }
  });

  function siblings(): ArtifactVersion[] {
    return shown ? list.filter((a) => a.key === shown!.key) : [];
  }

  function step(by: number): void {
    const all = siblings();
    const at = all.findIndex((a) => a.callId === shown?.callId);
    const next = all[at + by];
    if (next) {
      shown = next;
      render();
    }
  }

  function setMode(next: Mode): void {
    mode = next;
    render();
  }

  async function copy(): Promise<void> {
    if (!shown) return;
    if (await copyText(shown.content)) ctx.toast.success("Copied.");
    else ctx.toast.error("The browser kept the clipboard closed; use Download instead.");
  }

  function download(): void {
    if (!shown) return;
    const url = URL.createObjectURL(new Blob([shown.content], { type: `${artifactMime(shown.kind)};charset=utf-8` }));
    const link = h("a", { href: url, download: artifactFileName(shown) });
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
  }

  function render(): void {
    if (!shown) return;
    const artifact = shown;
    const all = siblings();
    const at = all.findIndex((a) => a.callId === artifact.callId);
    const kind = ARTIFACT_KINDS[artifact.kind];
    title.textContent = artifact.title;
    title.title = artifact.title;
    const language = artifact.kind === "code" && artifact.language ? ` · ${artifact.language}` : "";
    sub.textContent = all.length > 1 ? `${kind.label}${language} · Version ${at + 1} of ${all.length}` : `${kind.label}${language}`;
    versions.classList.toggle("hidden", all.length < 2);
    older.toggleAttribute("disabled", at <= 0);
    newer.toggleAttribute("disabled", at >= all.length - 1);
    const hasPreview = artifact.kind !== "code";
    modes.classList.toggle("hidden", !hasPreview);
    const view: Mode = hasPreview ? mode : "code";
    previewTab.classList.toggle("is-active", view === "preview");
    previewTab.setAttribute("aria-pressed", String(view === "preview"));
    codeTab.classList.toggle("is-active", view === "code");
    codeTab.setAttribute("aria-pressed", String(view === "code"));
    openTab.classList.toggle("hidden", !framed(artifact));
    if (framed(artifact)) openTab.setAttribute("href", artifactUrl(conversation, artifact.callId));
    if (view === "preview" && framed(artifact)) {
      // The same version keeps its frame (and what was done in it).
      if (frameFor === artifact.callId && body.querySelector("iframe")) return;
      const frame = document.createElement("iframe");
      frame.className = "ai-artifact-frame";
      frame.title = artifact.title;
      frame.setAttribute("sandbox", "allow-scripts allow-forms allow-modals allow-popups allow-downloads");
      frame.setAttribute("referrerpolicy", "no-referrer");
      frame.src = artifactUrl(conversation, artifact.callId);
      frameFor = artifact.callId;
      body.replaceChildren(frame);
      return;
    }
    frameFor = "";
    if (view === "preview" && artifact.kind === "markdown") {
      const doc = h("div", { class: "ai-md ai-artifact-doc" });
      renderMarkdown(doc, artifact.content, {
        fullHeadings: true,
        onCopy: (code) => void copyText(code).then((ok) => ok && ctx.toast.success("Code copied.")),
      });
      body.replaceChildren(doc);
      return;
    }
    const code = h("code", {});
    code.textContent = artifact.content;
    body.replaceChildren(h("pre", { class: "ai-artifact-code" }, code));
  }

  return {
    el,
    show(next, callId, conversationId) {
      const found = next.find((a) => a.callId === callId);
      if (!found) return;
      if (shown?.key !== found.key) mode = "preview";
      list = next;
      conversation = conversationId;
      shown = found;
      render();
    },
    update(next) {
      list = next;
      if (shown) render();
    },
    close() {
      shown = null;
      frameFor = "";
      body.replaceChildren();
    },
    shown: () => shown?.callId ?? null,
  };
}

/** The card an artifact call shows in the chat. */
export function artifactCard(block: ToolBlock, artifact: ArtifactVersion | null, onOpen: (callId: string) => void): HTMLElement {
  const input = typeof block.input === "object" && block.input ? block.input : {};
  const kindName = typeof input.kind === "string" ? input.kind.toLowerCase() : "";
  const kind = ARTIFACT_KINDS[kindName as keyof typeof ARTIFACT_KINDS] ?? { label: "Artifact", icon: "compose" };
  const name = artifact?.title ?? (typeof input.title === "string" && input.title.trim() ? input.title.trim() : "Artifact");
  if (block.status === "error" || !artifact) {
    const note = block.status === "error" ? block.error || "It could not be shown." : "Writing…";
    return h(
      "div",
      { class: `ai-artifact-card is-${block.status}`, dataset: { call: block.id } },
      h("span", { class: "ai-artifact-card-icon" }, iconSvg(kind.icon, 18)),
      h("span", { class: "ai-artifact-card-text" }, h("span", { class: "ai-artifact-card-title" }, name), h("span", { class: "ai-artifact-card-sub small" }, note)),
    );
  }
  const detail = artifact.version > 1 ? `${kind.label} · Version ${artifact.version}` : kind.label;
  return h(
    "button",
    { class: "ai-artifact-card", type: "button", dataset: { call: block.id }, title: `Open ${artifact.title}`, onClick: () => onOpen(artifact.callId) },
    h("span", { class: "ai-artifact-card-icon" }, iconSvg(kind.icon, 18)),
    h("span", { class: "ai-artifact-card-text" }, h("span", { class: "ai-artifact-card-title" }, artifact.title), h("span", { class: "ai-artifact-card-sub small" }, detail)),
    h("span", { class: "ai-artifact-card-open small" }, "Open"),
  );
}
