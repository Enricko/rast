// Reusable appearance editor: wallpaper source, fit/position/overlay and theme.
//
// Used by the Personalize dialog (with live preview) and by
// Control Panel → Appearance (with explicit save). All state is draft-local;
// only the caller persists it through the shared AppearanceService.

import {
  ACCENT_PRESETS,
  BUILTIN_WALLPAPERS,
  WALLPAPER_POSITIONS,
  accentPreset,
  appearancePatch,
  hasAppearanceChanges,
  normalizeAppearance,
  resolveTheme,
  uploadUrl,
  type AccentId,
  type Appearance,
  type AppearanceService,
  type ThemeSetting,
  type WallpaperFit,
  type WallpaperPosition,
} from "./appearance";
import { segmented } from "./components";
import { h } from "./dom";
import { formatBytes } from "./format";
import { iconSvg } from "./icons";
import type { ToastCenter } from "./toast";
import { confirmDialog, errorMessage } from "./ui";

export interface AppearanceEditorOptions {
  service: AppearanceService;
  toast: ToastCenter;
  /** Show the light/dark/system theme control (default true). */
  showTheme?: boolean;
  /** Show the ten accent presets (default true). */
  showAccents?: boolean;
  /** Called for every draft change (used for live preview). */
  onChange?: (draft: Appearance) => void;
  /**
   * Called immediately before a server mutation owned by this editor (save,
   * reset, upload, delete) so the realtime layer can ignore the echo event.
   */
  onLocalMutation?: () => void;
  /** Max upload size from the server (bytes). */
  maxUploadBytes?: number;
}

export interface AppearanceEditor {
  el: HTMLElement;
  getDraft(): Appearance;
  setDraft(draft: Appearance): void;
  applyServerAppearance(appearance: Appearance): void;
  /** True when the draft differs from the last saved appearance. */
  isDirty(): boolean;
  /** Revert the draft to the saved appearance (does not call the server). */
  discard(): Appearance;
  save(): Promise<Appearance>;
  reset(): Promise<Appearance>;
  dispose(): void;
}

const COLOR_PRESETS = [
  "#1B2430",
  "#27313D",
  "#3C4754",
  "#4B5560",
  "#5B6B5F",
  "#2E4756",
  "#443C5C",
  "#6B4F3F",
];

export function createAppearanceEditor(options: AppearanceEditorOptions): AppearanceEditor {
  const service = options.service;
  const toast = options.toast;
  const maxBytes = options.maxUploadBytes ?? service.value.limits?.maxBytes ?? 10 * 1024 * 1024;
  let draft = normalizeAppearance(service.value);
  const disposers: Array<() => void> = [];

  const themeSection = options.showTheme === false ? null : buildThemeSection();
  const accentSection = options.showAccents === false ? null : buildAccentSection();
  const gallerySection = buildGallerySection();
  const colorSection = buildColorSection();
  const layoutSection = buildLayoutSection();
  const uploadSection = buildUploadSection();

  const el = h(
    "div",
    { class: "appearance-layout" },
    themeSection?.el ?? null,
    accentSection?.el ?? null,
    gallerySection.el,
    uploadSection.el,
    colorSection.el,
    layoutSection.el,
  );

  function emitChange(): void {
    draft = normalizeAppearance(draft);
    refresh();
    options.onChange?.(draft);
  }

  function setKind(kind: "builtin" | "upload" | "color"): void {
    draft = { ...draft, wallpaper: { ...draft.wallpaper, kind } };
    emitChange();
  }

  function refresh(): void {
    themeSection?.refresh(draft);
    accentSection?.refresh(draft);
    gallerySection.refresh(draft);
    uploadSection.refresh(draft);
    colorSection.refresh(draft);
    layoutSection.refresh(draft);
  }

  // ------------------------------------------------------------- theme row
  function buildThemeSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const control = segmented<ThemeSetting>({
      items: [
        { value: "light", label: "Light", icon: "sun" },
        { value: "dark", label: "Dark", icon: "moon" },
        { value: "system", label: "System", icon: "monitor" },
      ],
      value: draft.theme,
      ariaLabel: "Colour mode",
      onChange: (value) => {
        draft = { ...draft, theme: value };
        emitChange();
      },
    });
    const el = section(
      "Mode",
      "Light, dark, or follow the operating system. The wallpaper and accent stay the same.",
      h("div", { class: "appearance-row" }, control.el),
      h(
        "div",
        { class: "appearance-preview-note" },
        iconSvg("info", 14),
        "System mode resolves in each browser and follows live OS changes.",
      ),
    );
    return {
      el,
      refresh: (state) => control.setValue(state.theme),
    };
  }

  // ------------------------------------------------------- accent presets (10)
  function buildAccentSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const grid = h("div", { class: "accent-grid", role: "radiogroup", ariaLabel: "Accent colour" });
    const buttons: HTMLButtonElement[] = [];
    const preview = h("div", { class: "accent-preview", ariaHidden: true });

    function renderPreview(): void {
      const preset = accentPreset(draft.accent);
      const dark = resolveTheme(draft.theme) === "dark";
      const css = dark ? preset.dark : preset.light;
      preview.replaceChildren(
        h("button", { class: "btn btn-primary btn-sm", type: "button", tabIndex: -1 }, "Primary"),
        h("span", { class: "accent-preview-link" }, "A link"),
        h("span", { class: "accent-preview-selection" }, "Selected"),
        h("input", {
          class: "input",
          type: "text",
          value: "Form field",
          tabIndex: -1,
          style: "max-width:140px",
        }),
        h("span", { class: "small faint" }, css.toUpperCase()),
      );
    }

    function selectAccent(id: AccentId, focus = false): void {
      draft = { ...draft, accent: id };
      for (const button of buttons) {
        const active = button.dataset.accent === id;
        button.classList.toggle("is-selected", active);
        button.setAttribute("aria-checked", String(active));
        button.tabIndex = active ? 0 : -1;
      }
      renderPreview();
      emitChange();
      if (focus) buttons.find((b) => b.dataset.accent === id)?.focus();
    }

    ACCENT_PRESETS.forEach((preset, index) => {
      const swatch = h("span", {
        class: "accent-swatch",
        style: `--swatch:${preset.light}`,
        dataset: { light: preset.light, dark: preset.dark },
      });
      const button = h(
        "button",
        {
          class: "accent-option",
          type: "button",
          role: "radio",
          ariaChecked: false,
          dataset: { accent: preset.id },
          onClick: () => selectAccent(preset.id),
          onKeyDown: (event) => {
            const keys = ["ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp", "Home", "End"];
            if (!keys.includes(event.key)) return;
            event.preventDefault();
            let next = index;
            if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (index + 1) % buttons.length;
            else if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (index - 1 + buttons.length) % buttons.length;
            else if (event.key === "Home") next = 0;
            else if (event.key === "End") next = buttons.length - 1;
            const target = buttons[next];
            if (target?.dataset.accent) selectAccent(target.dataset.accent as AccentId, true);
          },
        },
        swatch,
        h(
          "span",
          { class: "accent-name" },
          h("span", { class: "accent-label" }, preset.label),
          h("span", { class: "accent-hex" }, preset.light.toUpperCase()),
        ),
      );
      buttons.push(button);
      grid.appendChild(button);
    });

    const el = section(
      "Colors",
      "Ten presets. Success, warning and error keep their own semantic colours.",
      grid,
      preview,
    );
    return {
      el,
      refresh: (state) => {
        const dark = resolveTheme(state.theme) === "dark";
        for (const button of buttons) {
          const id = button.dataset.accent as AccentId;
          const preset = accentPreset(id);
          const active = state.accent === id;
          button.classList.toggle("is-selected", active);
          button.setAttribute("aria-checked", String(active));
          button.tabIndex = active ? 0 : -1;
          const swatch = button.querySelector<HTMLElement>(".accent-swatch");
          const hex = button.querySelector<HTMLElement>(".accent-hex");
          if (swatch) swatch.style.setProperty("--swatch", dark ? preset.dark : preset.light);
          if (hex) hex.textContent = (dark ? preset.dark : preset.light).toUpperCase();
        }
        renderPreview();
      },
    };
  }

  // --------------------------------------------------- built-in wallpapers
  function buildGallerySection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const gallery = h("div", { class: "wallpaper-gallery" });
    const tiles = new Map<string, HTMLButtonElement>();
    for (const wallpaper of BUILTIN_WALLPAPERS) {
      const tile = h(
        "button",
        {
          class: "wallpaper-tile",
          type: "button",
          ariaPressed: false,
          onClick: () => {
            draft = {
              ...draft,
              wallpaper: { ...draft.wallpaper, kind: "builtin", builtinId: wallpaper.id },
            };
            emitChange();
          },
        },
        h("span", {
          class: "wallpaper-tile-media",
          style: `background-image:url("${wallpaper.file}")`,
        }),
        h("span", { class: "wallpaper-tile-label" }, wallpaper.label),
        h("span", { class: "wallpaper-tile-detail" }, wallpaper.detail),
      );
      tiles.set(wallpaper.id, tile);
      gallery.appendChild(tile);
    }
    const colorTile = h(
      "button",
      {
        class: "wallpaper-tile",
        type: "button",
        ariaPressed: false,
        onClick: () => setKind("color"),
      },
      h("span", { class: "wallpaper-tile-media", style: "background:var(--surface-3)" }, h("span", { class: "color-tile-dot" })),
      h("span", { class: "wallpaper-tile-label" }, "Solid colour"),
      h("span", { class: "wallpaper-tile-detail" }, "A calm single colour, no image"),
    );
    colorTile.querySelector(".wallpaper-tile-media")?.appendChild(
      h("span", { class: "color-tile-preview", style: "background:var(--wallpaper-color)" }),
    );
    gallery.appendChild(colorTile);

    const el = section(
      "Wallpaper",
      "Bundled nature scenes are stored with the application and need no upload.",
      gallery,
    );
    return {
      el,
      refresh: (state) => {
        const selectedBuiltin = state.wallpaper.kind === "builtin" ? state.wallpaper.builtinId : "";
        for (const [id, tile] of tiles) {
          const active = id === selectedBuiltin;
          tile.classList.toggle("is-selected", active);
          tile.setAttribute("aria-pressed", String(active));
        }
        const colorActive = state.wallpaper.kind === "color";
        colorTile.classList.toggle("is-selected", colorActive);
        colorTile.setAttribute("aria-pressed", String(colorActive));
        const preview = colorTile.querySelector<HTMLElement>(".color-tile-preview");
        if (preview) preview.style.background = state.wallpaper.color;
      },
    };
  }

  // ----------------------------------------------------------- solid colour
  function buildColorSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const input = h("input", {
      class: "color-input",
      type: "color",
      value: draft.wallpaper.color,
      ariaLabel: "Wallpaper colour",
    });
    input.addEventListener("input", () => {
      draft = { ...draft, wallpaper: { ...draft.wallpaper, kind: "color", color: input.value } };
      emitChange();
    });
    const swatches = h("div", { class: "color-swatches" });
    const swatchButtons: HTMLButtonElement[] = [];
    for (const color of COLOR_PRESETS) {
      const swatch = h("button", {
        class: "color-swatch",
        type: "button",
        title: color,
        ariaLabel: `Use colour ${color}`,
        style: `background:${color}`,
        onClick: () => {
          draft = { ...draft, wallpaper: { ...draft.wallpaper, kind: "color", color } };
          input.value = color;
          emitChange();
        },
      });
      swatchButtons.push(swatch);
      swatches.appendChild(swatch);
    }
    const el = section(
      "Solid colour",
      "Useful on low-power displays or for a distraction-free desktop.",
      h("div", { class: "color-row" }, input, swatches),
    );
    return {
      el,
      refresh: (state) => {
        if (state.wallpaper.kind === "color") input.value = state.wallpaper.color;
        for (const swatch of swatchButtons) {
          swatch.classList.toggle("is-selected", swatch.title === state.wallpaper.color);
        }
      },
    };
  }

  // ------------------------------------------------------ uploads (private)
  function buildUploadSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const list = h("div", { class: "wallpaper-upload-row" });
    const empty = h("div", { class: "wallpaper-upload-empty" }, "No private wallpapers yet.");
    const fileInput = h("input", {
      class: "hidden",
      type: "file",
      accept: "image/jpeg,image/png,image/webp",
      ariaLabel: "Upload wallpaper",
    });
    const choose = h(
      "button",
      { class: "btn btn-sm", type: "button" },
      iconSvg("upload", 15),
      "Choose image…",
    );
    const status = h("span", { class: "field-hint" }, `JPEG, PNG or WebP · up to ${formatBytes(maxBytes)}`);
    choose.addEventListener("click", () => fileInput.click());
    fileInput.addEventListener("change", () => {
      const file = fileInput.files?.[0];
      fileInput.value = "";
      if (!file) return;
      void handleUpload(file);
    });
    const zone = h(
      "div",
      { class: "wallpaper-upload-zone" },
      choose,
      status,
      fileInput,
    );

    async function handleUpload(file: File): Promise<void> {
      if (file.size > maxBytes) {
        toast.error(`Image is too large (${formatBytes(file.size)}). Maximum is ${formatBytes(maxBytes)}.`);
        return;
      }
      choose.disabled = true;
      status.textContent = "Uploading and validating…";
      options.onLocalMutation?.();
      try {
        const upload = await service.upload(file);
        // Merge the new upload into the draft so the "My wallpapers" list
        // refreshes immediately (the service keeps its own copy).
        draft = {
          ...draft,
          uploads: service.value.uploads,
          wallpaper: { ...draft.wallpaper, kind: "upload", uploadId: upload.id },
        };
        emitChange();
        toast.success("Wallpaper uploaded");
      } catch (error) {
        toast.error(errorMessage(error));
        status.textContent = `JPEG, PNG or WebP · up to ${formatBytes(maxBytes)}`;
      } finally {
        choose.disabled = false;
      }
    }

    const el = section(
      "My wallpapers",
      "Private to your account. Other users cannot open these images.",
      empty,
      list,
      zone,
    );
    let lastUploads: string[] = [];
    return {
      el,
      refresh: (state) => {
        const uploads = state.uploads ?? [];
        const signature = uploads.map((upload) => upload.id).join(",");
        empty.hidden = uploads.length > 0;
        if (signature === lastUploads.join(",")) {
          for (const card of Array.from(list.children)) {
            const id = (card as HTMLElement).dataset.uploadId;
            card.classList.toggle(
              "is-selected",
              state.wallpaper.kind === "upload" && state.wallpaper.uploadId === id,
            );
          }
          return;
        }
        lastUploads = uploads.map((upload) => upload.id);
        list.replaceChildren();
        for (const upload of uploads) {
          const media = h("span", {
            class: "wallpaper-tile-media",
            style: `background-image:url("${uploadUrl(upload.id)}")`,
          });
          if (state.wallpaper.kind === "upload" && state.wallpaper.uploadId === upload.id) {
            media.appendChild(h("span", { class: "wallpaper-tile-check" }, "✓"));
          }
          const remove = h(
            "button",
            {
              class: "btn btn-sm btn-subtle",
              type: "button",
              title: "Delete this wallpaper",
              ariaLabel: "Delete this wallpaper",
              onClick: async () => {
                const ok = await confirmDialog({
                  title: "Delete wallpaper",
                  message: "Delete this uploaded wallpaper? If it is currently active the desktop falls back to the default wallpaper.",
                  confirmLabel: "Delete",
                  danger: true,
                });
                if (!ok) return;
                options.onLocalMutation?.();
                try {
                  const updated = await service.removeUpload(upload.id);
                  // Keep the rest of the draft; only a draft that pointed at
                  // the deleted image falls back to the saved wallpaper.
                  const usedDeleted = draft.wallpaper.kind === "upload" && draft.wallpaper.uploadId === upload.id;
                  draft = normalizeAppearance({
                    ...draft,
                    uploads: updated.uploads,
                    wallpaper: usedDeleted ? updated.wallpaper : draft.wallpaper,
                  });
                  emitChange();
                  toast.success("Wallpaper deleted");
                } catch (error) {
                  toast.error(errorMessage(error));
                }
              },
            },
            iconSvg("trash", 14),
          );
          const select = h(
            "button",
            {
              class: "wallpaper-tile",
              type: "button",
              style: "flex:1;min-width:0",
              onClick: () => {
                draft = {
                  ...draft,
                  wallpaper: { ...draft.wallpaper, kind: "upload", uploadId: upload.id },
                };
                emitChange();
              },
            },
            media,
            h(
              "span",
              { class: "wallpaper-upload-actions" },
              h("span", { class: "wallpaper-upload-name" }, `${upload.width}×${upload.height}`),
              h("span", { class: "wallpaper-upload-name" }, formatBytes(upload.sizeBytes)),
            ),
          );
          const card = h(
            "div",
            { class: "wallpaper-upload-card", dataset: { uploadId: upload.id } },
            select,
            h("div", { class: "wallpaper-upload-actions" }, h("span", { class: "wallpaper-upload-name" }, "Private"), remove),
          );
          if (state.wallpaper.kind === "upload" && state.wallpaper.uploadId === upload.id) {
            card.classList.add("is-selected");
          }
          list.appendChild(card);
        }
      },
    };
  }

  // ----------------------------------------------------- fit / position / overlay
  function buildLayoutSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const fitControl = segmented<WallpaperFit>({
      items: [
        { value: "cover", label: "Cover" },
        { value: "contain", label: "Contain" },
        { value: "center", label: "Center" },
      ],
      value: draft.wallpaper.fit,
      ariaLabel: "Image fit",
      onChange: (fit) => {
        draft = { ...draft, wallpaper: { ...draft.wallpaper, fit } };
        emitChange();
      },
    });
    const fitGroup = fitControl.el;

    const positionGrid = h("div", { class: "position-grid", role: "group", ariaLabel: "Image position" });
    const positionButtons = new Map<WallpaperPosition, HTMLButtonElement>();
    for (const position of WALLPAPER_POSITIONS) {
      const button = h("button", {
        class: "position-cell",
        type: "button",
        title: position.label,
        ariaLabel: `Position ${position.label}`,
        ariaPressed: false,
        onClick: () => {
          draft = { ...draft, wallpaper: { ...draft.wallpaper, position: position.id } };
          emitChange();
        },
      });
      positionButtons.set(position.id, button);
      positionGrid.appendChild(button);
    }

    const overlayInput = h("input", {
      type: "range",
      min: "0",
      max: "60",
      step: "5",
      value: String(draft.wallpaper.overlay),
      ariaLabel: "Dark overlay",
    });
    const overlayValue = h("span", { class: "appearance-range-value" }, `${draft.wallpaper.overlay}%`);
    overlayInput.addEventListener("input", () => {
      const overlay = Number(overlayInput.value);
      overlayValue.textContent = `${overlay}%`;
      draft = { ...draft, wallpaper: { ...draft.wallpaper, overlay } };
      emitChange();
    });

    const el = section(
      "Layout",
      "Control how the image fills the desktop and add a little darkening for legibility.",
      h(
        "div",
        { class: "appearance-row", style: "margin-bottom:12px" },
        h("div", { class: "appearance-field" }, h("span", { class: "appearance-field-label" }, "Fit"), fitGroup),
        h(
          "div",
          { class: "appearance-field" },
          h("span", { class: "appearance-field-label" }, "Position"),
          positionGrid,
        ),
        h(
          "div",
          { class: "appearance-field", style: "flex:1;min-width:220px" },
          h("span", { class: "appearance-field-label" }, "Dark overlay"),
          h("div", { class: "appearance-range" }, overlayInput, overlayValue),
        ),
      ),
      h("div", { class: "appearance-preview-note" }, iconSvg("info", 14), "Position applies to Cover and Contain; Center keeps the image at its natural size."),
    );
    return {
      el,
      refresh: (state) => {
        fitControl.setValue(state.wallpaper.fit);
        for (const [position, button] of positionButtons) {
          const active = state.wallpaper.position === position;
          button.classList.toggle("is-selected", active);
          button.setAttribute("aria-pressed", String(active));
        }
        overlayInput.value = String(state.wallpaper.overlay);
        overlayValue.textContent = `${state.wallpaper.overlay}%`;
      },
    };
  }

  refresh();

  return {
    el,
    getDraft: () => normalizeAppearance(draft),
    setDraft: (next) => {
      draft = normalizeAppearance(next);
      refresh();
    },
    applyServerAppearance: (appearance) => {
      draft = normalizeAppearance(appearance);
      refresh();
    },
    isDirty: () => hasAppearanceChanges(service.value, draft),
    discard: () => {
      draft = normalizeAppearance(service.value);
      refresh();
      return draft;
    },
    save: async () => {
      options.onLocalMutation?.();
      // Only what the user changed: settings this tab did not touch keep
      // whatever the server has (another device may have saved meanwhile).
      const saved = await service.save(appearancePatch(normalizeAppearance(service.value), normalizeAppearance(draft)));
      draft = normalizeAppearance(saved);
      refresh();
      return saved;
    },
    reset: async () => {
      // The server keeps the persisted theme and accent on reset; use its
      // response as the new draft baseline instead of the preview state.
      options.onLocalMutation?.();
      const reset = await service.reset();
      draft = normalizeAppearance(reset);
      refresh();
      return draft;
    },
    dispose: () => {
      for (const dispose of disposers) dispose();
    },
  };

  function section(title: string, hint: string, ...children: Array<Node | null>): HTMLElement {
    return h(
      "div",
      { class: "appearance-section" },
      h(
        "div",
        { class: "appearance-section-head" },
        h("span", { class: "appearance-section-title" }, title),
        h("span", { class: "appearance-section-hint" }, hint),
      ),
      ...children.filter((child): child is Node => child !== null),
    );
  }
}

/** Validate a draft that is about to be saved (mirrors the API contract). */
export function validateAppearanceDraft(appearance: Appearance): string | null {
  const wallpaper = appearance.wallpaper;
  if (wallpaper.kind === "upload") {
    if (!wallpaper.uploadId) return "Choose one of your uploaded wallpapers or switch back to a built-in one.";
    const upload = appearance.uploads.find((item) => item.id === wallpaper.uploadId);
    if (!upload) return "That uploaded wallpaper is no longer available. Pick another one.";
  }
  if (wallpaper.kind === "color" && !/^#[0-9a-fA-F]{6}$/.test(wallpaper.color)) {
    return "Pick a solid colour in the format #RRGGBB.";
  }
  if (wallpaper.kind === "builtin" && !BUILTIN_WALLPAPERS.some((item) => item.id === wallpaper.builtinId)) {
    return "That built-in wallpaper is not available in this build.";
  }
  if (!Number.isFinite(wallpaper.overlay) || wallpaper.overlay < 0 || wallpaper.overlay > 60) {
    return "The dark overlay must be between 0% and 60%.";
  }
  return null;
}

// ---------------------------------------------------------------------------
// Shared save workflow
//
// Both surfaces (Control Panel → Appearance and the Personalize dialog) use the
// same finite-state machine so the button, status line, spinner and error
// handling always agree:
//
//   clean  -> Save disabled, label "All changes saved"
//   dirty  -> Save enabled, label "Save changes"
//   saving -> Save disabled + spinner, label "Saving…"
//   saved  -> label "Saved" for a moment, then back to clean
//   invalid-> Save disabled, inline validation message
//   error  -> error banner + label "Try again" (still enabled)
// ---------------------------------------------------------------------------

export type AppearanceSaveState = "clean" | "dirty" | "saving" | "saved" | "invalid" | "error";

export interface AppearanceSaveWorkflowOptions {
  /** Persist the current draft; must resolve with the saved appearance. */
  save: () => Promise<Appearance>;
  onSaved?: (saved: Appearance) => void;
  /** Extra controls rendered at the start of the action row (Cancel, Reset…). */
  leading?: Node[];
  /** Notified whenever the FSM state changes (used to enable Discard). */
  onStateChange?: (state: AppearanceSaveState) => void;
  labels?: Partial<{ clean: string; dirty: string; saving: string; saved: string; retry: string }>;
}

export interface AppearanceSaveWorkflow {
  el: HTMLElement;
  saveButton: HTMLButtonElement;
  status: HTMLElement;
  error: HTMLElement;
  readonly state: AppearanceSaveState;
  isDirty(): boolean;
  setDirty(dirty: boolean): void;
  /** Show an inline validation message without contacting the server. */
  setValidationError(message: string | null): void;
  /** Run the save FSM (also used by the "Try again" button state). */
  save(): Promise<void>;
  /** Return to the pristine "All changes saved" state. */
  reset(): void;
  dispose(): void;
}

const DEFAULT_LABELS = {
  clean: "All changes saved",
  dirty: "Save changes",
  saving: "Saving…",
  saved: "Saved",
  retry: "Try again",
};

export function createAppearanceSaveWorkflow(options: AppearanceSaveWorkflowOptions): AppearanceSaveWorkflow {
  const labels = { ...DEFAULT_LABELS, ...(options.labels ?? {}) };
  let state: AppearanceSaveState = "clean";
  let dirty = false;
  let validation: string | null = null;
  let savedTimer: number | undefined;
  let disposed = false;
  let inFlight = false;

  const error = h("div", { class: "banner banner-error", hidden: true, role: "alert" });
  const status = h("div", { class: "form-status" });
  const saveButton = h("button", { class: "btn btn-primary", type: "button", disabled: true });
  const actions = h(
    "div",
    { class: "appearance-actions" },
    ...(options.leading ?? []),
    h("div", { class: "spacer" }),
    status,
    saveButton,
  );
  const el = h("div", { class: "appearance-save" }, error, actions);

  function setButtonContent(label: string, iconName?: string, loading = false): void {
    saveButton.classList.toggle("is-loading", loading);
    saveButton.setAttribute("aria-busy", String(loading));
    const nodes: Node[] = [];
    if (iconName && !loading) nodes.push(iconSvg(iconName, 15));
    nodes.push(document.createTextNode(label));
    saveButton.replaceChildren(...nodes);
  }

  function render(): void {
    if (disposed) return;
    if (savedTimer !== undefined) {
      window.clearTimeout(savedTimer);
      savedTimer = undefined;
    }
    error.hidden = true;
    error.textContent = "";
    status.className = "form-status";
    saveButton.classList.remove("is-loading");
    saveButton.removeAttribute("aria-busy");
    switch (state) {
      case "clean":
        saveButton.disabled = true;
        setButtonContent(labels.dirty);
        status.textContent = labels.clean;
        status.classList.add("is-saved");
        break;
      case "dirty":
        saveButton.disabled = false;
        setButtonContent(labels.dirty, "check");
        status.textContent = "Unsaved changes";
        status.classList.add("is-dirty");
        break;
      case "saving":
        saveButton.disabled = true;
        setButtonContent(labels.saving, undefined, true);
        status.textContent = labels.saving;
        status.classList.add("is-saving");
        break;
      case "saved":
        saveButton.disabled = true;
        setButtonContent(labels.saved, "check");
        status.textContent = labels.saved;
        status.classList.add("is-saved");
        break;
      case "invalid":
        saveButton.disabled = true;
        setButtonContent(labels.dirty, "check");
        status.textContent = validation ?? "Fix the highlighted fields";
        status.classList.add("is-error");
        if (validation) {
          error.hidden = false;
          error.textContent = validation;
        }
        break;
      case "error":
        saveButton.disabled = false;
        setButtonContent(labels.retry, "refresh");
        status.textContent = "Could not save";
        status.classList.add("is-error");
        break;
    }
    options.onStateChange?.(state);
  }

  function toClean(): void {
    state = "clean";
    dirty = false;
    validation = null;
    render();
  }

  function setDirty(next: boolean): void {
    if (disposed || inFlight) return;
    dirty = next;
    if (!next) {
      toClean();
      return;
    }
    validation = null;
    state = "dirty";
    render();
  }

  function setValidationError(message: string | null): void {
    if (disposed || inFlight) return;
    validation = message;
    state = message ? "invalid" : dirty ? "dirty" : "clean";
    render();
  }

  async function runSave(): Promise<void> {
    if (disposed || inFlight) return;
    if (!dirty) {
      render();
      return;
    }
    inFlight = true;
    state = "saving";
    render();
    try {
      const saved = await options.save();
      if (disposed) return;
      inFlight = false;
      state = "saved";
      dirty = false;
      validation = null;
      render();
      options.onSaved?.(saved);
      savedTimer = window.setTimeout(() => {
        savedTimer = undefined;
        if (disposed || state !== "saved") return;
        toClean();
      }, 1800);
    } catch (saveError) {
      if (disposed) return;
      inFlight = false;
      state = "error";
      dirty = true;
      render();
      error.hidden = false;
      error.textContent = errorMessage(saveError);
    }
  }

  saveButton.addEventListener("click", () => {
    if (state === "saving") return;
    void runSave();
  });

  render();

  return {
    el,
    saveButton,
    status,
    error,
    get state() {
      return state;
    },
    isDirty: () => dirty,
    setDirty,
    setValidationError,
    save: runSave,
    reset: toClean,
    dispose: () => {
      disposed = true;
      if (savedTimer !== undefined) window.clearTimeout(savedTimer);
      savedTimer = undefined;
    },
  };
}
