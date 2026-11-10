// Reusable appearance editor: wallpaper source, fit/position/overlay and theme.
//
// Used by the Personalize dialog (with live preview) and by
// Control Panel → Appearance (with explicit save). All state is draft-local;
// only the caller persists it through the shared AppearanceService.

import {
  BUILTIN_WALLPAPERS,
  WALLPAPER_POSITIONS,
  normalizeAppearance,
  uploadUrl,
  type Appearance,
  type AppearanceService,
  type ThemeSetting,
  type WallpaperFit,
  type WallpaperPosition,
} from "./appearance";
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
  /** Called for every draft change (used for live preview). */
  onChange?: (draft: Appearance) => void;
  /** Max upload size from the server (bytes). */
  maxUploadBytes?: number;
}

export interface AppearanceEditor {
  el: HTMLElement;
  getDraft(): Appearance;
  setDraft(draft: Appearance): void;
  applyServerAppearance(appearance: Appearance): void;
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
  const gallerySection = buildGallerySection();
  const colorSection = buildColorSection();
  const layoutSection = buildLayoutSection();
  const uploadSection = buildUploadSection();

  const el = h(
    "div",
    { class: "appearance-layout" },
    themeSection?.el ?? null,
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
    gallerySection.refresh(draft);
    uploadSection.refresh(draft);
    colorSection.refresh(draft);
    layoutSection.refresh(draft);
  }

  // ------------------------------------------------------------- theme row
  function buildThemeSection(): { el: HTMLElement; refresh: (draft: Appearance) => void } {
    const buttons = new Map<ThemeSetting, HTMLButtonElement>();
    const group = h("div", { class: "segmented", role: "group", ariaLabel: "Theme" });
    const items: Array<{ value: ThemeSetting; label: string; icon: string }> = [
      { value: "light", label: "Light", icon: "sun" },
      { value: "dark", label: "Dark", icon: "moon" },
      { value: "system", label: "System", icon: "monitor" },
    ];
    for (const item of items) {
      const button = h(
        "button",
        {
          type: "button",
          ariaPressed: false,
          onClick: () => {
            draft = { ...draft, theme: item.value };
            emitChange();
          },
        },
        iconSvg(item.icon, 15),
        h("span", {}, item.label),
      );
      buttons.set(item.value, button);
      group.appendChild(button);
    }
    const el = section(
      "Theme",
      "Window colours for your account; the wallpaper stays the same.",
      h("div", { class: "appearance-row" }, group),
    );
    return {
      el,
      refresh: (state) => {
        for (const [value, button] of buttons) {
          const active = state.theme === value;
          button.classList.toggle("is-active", active);
          button.setAttribute("aria-pressed", String(active));
        }
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
                try {
                  const updated = await service.removeUpload(upload.id);
                  draft = normalizeAppearance(updated);
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
    const fitButtons = new Map<WallpaperFit, HTMLButtonElement>();
    const fitGroup = h("div", { class: "segmented", role: "group", ariaLabel: "Image fit" });
    for (const fit of ["cover", "contain", "center"] as WallpaperFit[]) {
      const button = h(
        "button",
        {
          type: "button",
          ariaPressed: false,
          onClick: () => {
            draft = { ...draft, wallpaper: { ...draft.wallpaper, fit } };
            emitChange();
          },
        },
        fit === "cover" ? "Cover" : fit === "contain" ? "Contain" : "Center",
      );
      fitButtons.set(fit, button);
      fitGroup.appendChild(button);
    }

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
        for (const [fit, button] of fitButtons) {
          const active = state.wallpaper.fit === fit;
          button.classList.toggle("is-active", active);
          button.setAttribute("aria-pressed", String(active));
        }
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
    save: async () => {
      const saved = await service.save({
        theme: draft.theme,
        wallpaper: draft.wallpaper,
      });
      draft = normalizeAppearance(saved);
      refresh();
      return saved;
    },
    reset: async () => {
      const reset = await service.reset();
      draft = { ...normalizeAppearance(reset), theme: draft.theme };
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
