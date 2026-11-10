// "Personalize" dialog: live preview of wallpaper/theme with Apply/Cancel/Reset.
//
// Preview is deliberately transient: Cancel (or Esc / backdrop click) restores
// the previously applied appearance, Apply persists it through the server.

import {
  applyAppearance,
  getAppearanceService,
  normalizeAppearance,
  resolveTheme,
  type Appearance,
} from "../core/appearance";
import type { ApiClient } from "../core/api";
import { h } from "../core/dom";
import type { ToastCenter } from "../core/toast";
import { confirmDialog, errorMessage, modal } from "../core/ui";
import { createAppearanceEditor } from "../core/appearance-editor";

export interface PersonalizeOptions {
  api: ApiClient;
  toast: ToastCenter;
  /** Re-apply the theme that is really persisted (used when cancelling). */
  restoreTheme: () => void;
  onApplied: (appearance: Appearance) => void;
}

export function openPersonalizeDialog(options: PersonalizeOptions): void {
  const service = getAppearanceService(options.api);
  const previous = normalizeAppearance(service.value);
  let settled = false;

  const editor = createAppearanceEditor({
    service,
    toast: options.toast,
    showTheme: true,
    onChange: (draft) => {
      applyAppearance(draft, { preview: true });
      const resolved = resolveTheme(draft.theme);
      document.documentElement.dataset.theme = resolved;
      document.documentElement.style.colorScheme = resolved;
    },
  });

  // Start from the applied look so the preview is WYSIWYG from the first click.
  applyAppearance(previous);

  const cancelPreview = (): void => {
    if (settled) return;
    settled = true;
    applyAppearance(previous);
    options.restoreTheme();
  };

  const resetWallpaper = async (): Promise<void> => {
    const ok = await confirmDialog({
      title: "Reset appearance",
      message: "Restore the default wallpaper, fit, position and overlay? Your theme choice is kept.",
      confirmLabel: "Reset wallpaper",
    });
    if (!ok) return;
    try {
      const reset = await editor.reset();
      applyAppearance(reset, { preview: true });
      const resolved = resolveTheme(reset.theme);
      document.documentElement.dataset.theme = resolved;
      document.documentElement.style.colorScheme = resolved;
      options.toast.success("Wallpaper reset to default");
    } catch (error) {
      options.toast.error(errorMessage(error));
    }
  };

  const body = h(
    "div",
    { class: "personalize-dialog-body" },
    h(
      "p",
      { class: "small muted" },
      "Changes are previewed on your desktop. Apply saves them to your account; Cancel restores the previous look.",
    ),
    editor.el,
  );

  const close = modal({
    title: "Personalize",
    wide: true,
    body,
    onClose: cancelPreview,
    actions: [
      {
        label: "Reset to default",
        variant: "ghost",
        keepOpen: true,
        onClick: () => resetWallpaper(),
      },
      {
        label: "Cancel",
        variant: "ghost",
        keepOpen: true,
        onClick: () => close(),
      },
      {
        label: "Apply",
        variant: "primary",
        keepOpen: true,
        onClick: async () => {
          try {
            const saved = await editor.save();
            settled = true;
            applyAppearance(saved);
            options.onApplied(saved);
            options.toast.success("Appearance saved");
            close();
          } catch (error) {
            options.toast.error(errorMessage(error));
          }
        },
      },
    ],
  });
}
