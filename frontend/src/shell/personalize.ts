// "Personalize" dialog: live preview of theme/accent/wallpaper with the same
// save workflow as Control Panel → Appearance.
//
// Preview is transient and never persisted: Cancel (or Esc / backdrop click)
// restores the last saved appearance, Apply persists theme + accent + wallpaper
// together through the shared AppearanceService.

import {
  applyAppearance,
  beginAppearancePreview,
  endAppearancePreview,
  getAppearanceService,
  getAppearanceSync,
  normalizeAppearance,
  resolveTheme,
  type Appearance,
  type ThemeSetting,
} from "../core/appearance";
import type { ApiClient } from "../core/api";
import { h } from "../core/dom";
import { iconSvg } from "../core/icons";
import type { EventStream } from "../core/sse";
import type { ToastCenter } from "../core/toast";
import { confirmDialog, errorMessage, modal } from "../core/ui";
import {
  createAppearanceEditor,
  createAppearanceSaveWorkflow,
  validateAppearanceDraft,
} from "../core/appearance-editor";

export interface PersonalizeOptions {
  api: ApiClient;
  events: EventStream;
  /** Signed-in account; appearance is cached per user. */
  userId: number;
  toast: ToastCenter;
  /** Re-apply the persisted theme choice to the store (used when cancelling). */
  restoreTheme: () => void;
  onApplied: (appearance: Appearance) => void;
}

function setRootTheme(theme: ThemeSetting): void {
  const resolved = resolveTheme(theme);
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

export function openPersonalizeDialog(options: PersonalizeOptions): void {
  const service = getAppearanceService(options.api);
  const sync = getAppearanceSync(options.api, options.events);
  const previous = normalizeAppearance(service.value);
  let settled = false;
  let previewing = false;

  const enterPreview = (): void => {
    if (previewing) return;
    beginAppearancePreview();
    previewing = true;
  };
  const exitPreview = (): void => {
    if (!previewing) return;
    endAppearancePreview();
    previewing = false;
  };

  // Start from the applied look so the preview is WYSIWYG from the first click.
  applyAppearance(previous);
  setRootTheme(previous.theme);

  const cancelButton = h(
    "button",
    { class: "btn btn-ghost", type: "button" },
    "Cancel",
  );
  const resetButton = h(
    "button",
    { class: "btn btn-ghost", type: "button" },
    iconSvg("wallpaper", 15),
    "Reset to default",
  );

  let editor: ReturnType<typeof createAppearanceEditor> | null = null;
  let workflow: ReturnType<typeof createAppearanceSaveWorkflow> | null = null;

  const conflictHost = h("div", { class: "cp-conflict", hidden: true });
  let pendingExternal: Appearance | null = null;

  const closeDialog = (): void => {
    if (settled) return;
    settled = true;
    exitPreview();
    // Restore whatever is saved (an earlier Apply may have persisted a new look).
    applyAppearance(service.value);
    options.restoreTheme();
    unsubscribeConflict();
    unsubscribeAdopt();
    unsubscribeDirty();
    workflow?.dispose();
    editor?.dispose();
    close();
  };

  const showConflict = (external: Appearance): void => {
    pendingExternal = external;
    conflictHost.hidden = false;
    conflictHost.replaceChildren(
      h(
        "div",
        { class: "banner banner-warn", role: "status" },
        h("span", { class: "banner-icon" }, iconSvg("warning", 16)),
        h(
          "div",
          { class: "banner-body" },
          h("div", { class: "banner-title" }, "Appearance changed in another session"),
          h(
            "div",
            { class: "banner-message" },
            "Your preview was kept. Keep editing and apply to overwrite, or load the version saved from the other session.",
          ),
          h(
            "div",
            { class: "row wrap" },
            h(
              "button",
              {
                class: "btn btn-sm",
                type: "button",
                onClick: () => {
                  conflictHost.hidden = true;
                  pendingExternal = null;
                },
              },
              "Keep mine",
            ),
            h(
              "button",
              {
                class: "btn btn-sm btn-primary",
                type: "button",
                onClick: () => {
                  if (!pendingExternal) return;
                  const adopted = sync.adopt(pendingExternal);
                  editor?.applyServerAppearance(adopted);
                  applyAppearance(adopted);
                  setRootTheme(adopted.theme);
                  options.restoreTheme();
                  workflow?.reset();
                  exitPreview();
                  conflictHost.hidden = true;
                  pendingExternal = null;
                },
              },
              "Use updated",
            ),
          ),
        ),
      ),
    );
  };

  editor = createAppearanceEditor({
    service,
    toast: options.toast,
    showTheme: true,
    showAccents: true,
    onLocalMutation: () => sync.markLocalSave(),
    maxUploadBytes: service.value.limits?.maxBytes,
    onChange: (draft) => {
      enterPreview();
      applyAppearance(draft, { preview: true });
      setRootTheme(draft.theme);
      const invalid = validateAppearanceDraft(draft);
      if (invalid) workflow?.setValidationError(invalid);
      else workflow?.setDirty(true);
    },
  });

  workflow = createAppearanceSaveWorkflow({
    save: () => {
      sync.markLocalSave();
      return editor!.save();
    },
    labels: { clean: "All changes saved", dirty: "Apply", saving: "Applying…", saved: "Applied", retry: "Try again" },
    onSaved: (saved) => {
      sync.markLocalSave();
      exitPreview();
      applyAppearance(saved);
      setRootTheme(saved.theme);
      options.restoreTheme();
      options.onApplied(saved);
      options.toast.success("Appearance saved");
      window.setTimeout(() => closeDialog(), 350);
    },
  });

  resetButton.addEventListener("click", () => {
    void (async () => {
      const ok = await confirmDialog({
        title: "Reset appearance",
        message: "Restore the default wallpaper, fit, position and overlay? Your theme and accent are kept.",
        confirmLabel: "Reset wallpaper",
      });
      if (!ok) return;
      resetButton.disabled = true;
      try {
        const reset = await editor!.reset();
        sync.markLocalSave();
        enterPreview();
        applyAppearance(reset, { preview: true });
        setRootTheme(reset.theme);
        workflow?.reset();
        options.toast.success("Wallpaper reset to default");
      } catch (error) {
        options.toast.error(errorMessage(error));
      } finally {
        resetButton.disabled = false;
      }
    })();
  });

  const leadingRow = h("div", { class: "appearance-actions-leading" }, resetButton, cancelButton);
  workflow.el.querySelector(".appearance-actions")?.prepend(leadingRow);
  workflow.el.classList.add("appearance-save-bar");

  const unsubscribeConflict = sync.onConflict((external) => {
    service.adopt(external);
    showConflict(external);
  });
  const unsubscribeAdopt = sync.onAdopt((adopted) => {
    if (settled) return;
    editor?.applyServerAppearance(adopted);
    if (!previewing) {
      applyAppearance(adopted);
      setRootTheme(adopted.theme);
      options.restoreTheme();
      workflow?.reset();
    }
    conflictHost.hidden = true;
    pendingExternal = null;
  });
  const unsubscribeDirty = sync.registerDirtySource(() => workflow?.isDirty() ?? false);

  const body = h(
    "div",
    { class: "personalize-dialog-body" },
    h(
      "p",
      { class: "small muted" },
      "Changes preview on the live desktop, including open windows. Apply saves theme, accent and wallpaper to your account; Cancel restores the saved look.",
    ),
    conflictHost,
    editor.el,
    workflow.el,
  );

  const close = modal({
    title: "Personalize",
    wide: true,
    body,
    onClose: () => closeDialog(),
  });

  cancelButton.addEventListener("click", () => closeDialog());
  void previous;
}
