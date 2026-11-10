// Control Panel → Appearance: one editor shared with the Personalize dialog.
//
// The draft previews immediately across the whole UI (CSS variables + data-accent
// on the document root, so open windows/dialogs update without remounting).
// "Save changes" persists theme + accent + wallpaper together; "Discard changes"
// and leaving the tab restore the saved appearance.

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
} from "../../core/appearance";
import {
  createAppearanceEditor,
  createAppearanceSaveWorkflow,
  validateAppearanceDraft,
} from "../../core/appearance-editor";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { confirmDialog, errorMessage } from "../../core/ui";
import { pageIntro, type TabRenderContext } from "./common";

function setRootTheme(theme: ThemeSetting): void {
  const resolved = resolveTheme(theme);
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

export async function renderAppearanceTab(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  const service = getAppearanceService(ctx.api);
  const sync = getAppearanceSync(ctx.api, ctx.events);
  const userId = ctx.store.get().user?.id ?? 0;

  // Refresh from the server so another device's last save is reflected.
  try {
    await service.load(userId);
  } catch (error) {
    ctx.toast.warning(`Showing cached appearance: ${errorMessage(error)}`);
  }

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
  const applySaved = (): Appearance => {
    exitPreview();
    const saved = normalizeAppearance(service.value);
    applyAppearance(saved);
    ctx.store.setThemePreference(saved.theme);
    return saved;
  };

  // Buttons are created first so the workflow state callback can update them.
  const discardButton = h(
    "button",
    { class: "btn btn-ghost", type: "button", disabled: true },
    iconSvg("restore", 15),
    "Discard changes",
  );
  const resetButton = h(
    "button",
    { class: "btn btn-ghost", type: "button" },
    iconSvg("wallpaper", 15),
    "Reset to default",
  );

  let editor: ReturnType<typeof createAppearanceEditor> | null = null;
  let workflow: ReturnType<typeof createAppearanceSaveWorkflow> | null = null;

  editor = createAppearanceEditor({
    service,
    toast: ctx.toast,
    showTheme: true,
    showAccents: true,
    onLocalMutation: () => sync.markLocalSave(),
    onChange: (draft) => {
      enterPreview();
      applyAppearance(draft);
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
    onSaved: (saved) => {
      sync.markLocalSave();
      exitPreview();
      ctx.store.setThemePreference(saved.theme);
      applyAppearance(saved);
    },
    onStateChange: (state) => {
      discardButton.disabled = ["clean", "saving", "saved"].includes(state);
    },
  });

  discardButton.addEventListener("click", () => {
    if (!workflow?.isDirty()) return;
    editor?.discard();
    applySaved();
    workflow?.reset();
  });
  resetButton.addEventListener("click", () => {
    void (async () => {
      const ok = await confirmDialog({
        title: "Reset appearance",
        message:
          "Restore the default wallpaper, fit, position and overlay? Your saved theme and accent are kept. This also clears unsaved preview changes.",
        confirmLabel: "Reset wallpaper",
      });
      if (!ok) return;
      resetButton.disabled = true;
      try {
        const reset = await editor!.reset();
        sync.markLocalSave();
        exitPreview();
        applyAppearance(reset);
        ctx.store.setThemePreference(reset.theme);
        workflow?.reset();
        ctx.toast.success("Wallpaper reset to default");
      } catch (error) {
        ctx.toast.error(errorMessage(error));
        workflow?.setDirty(true);
      } finally {
        resetButton.disabled = false;
      }
    })();
  });
  const leadingRow = h("div", { class: "appearance-actions-leading" }, resetButton, discardButton);
  workflow.el.querySelector(".appearance-actions")?.prepend(leadingRow);

  // --------------------------------------------------------- cross-session sync
  const conflictHost = h("div", { class: "cp-conflict", hidden: true });
  let pendingExternal: Appearance | null = null;

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
            "Your unsaved changes were kept. Keep editing and save to overwrite, or load the version saved from the other session.",
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
                  applySaved();
                  workflow?.reset();
                  conflictHost.hidden = true;
                  pendingExternal = null;
                  ctx.toast.success("Loaded the appearance saved in the other session");
                },
              },
              "Use updated",
            ),
          ),
        ),
      ),
    );
  };

  const unsubscribeConflict = sync.onConflict((external) => {
    // Keep the newer saved appearance as the revert baseline without touching
    // the draft that is currently being edited.
    service.adopt(external);
    showConflict(external);
  });
  const unsubscribeAdopt = sync.onAdopt((adopted) => {
    editor?.applyServerAppearance(adopted);
    applySaved();
    workflow?.reset();
    conflictHost.hidden = true;
    pendingExternal = null;
  });
  const unsubscribeDirty = sync.registerDirtySource(() => workflow?.isDirty() ?? false);

  // ------------------------------------------------------------------- layout
  const intro = pageIntro(
    "Appearance",
    "Choose how your desktop looks. Theme, accent and wallpaper are stored with your account, so they follow you to any browser. Changes preview instantly; Save applies them.",
  );

  const saveHost = workflow.el;
  saveHost.classList.add("appearance-save-bar");
  host.append(intro, conflictHost, saveHost, editor.el);

  const cleanup = (): void => {
    unsubscribeConflict();
    unsubscribeAdopt();
    unsubscribeDirty();
    workflow?.dispose();
    editor?.dispose();
    // Leaving without saving must not leave a preview applied.
    applySaved();
  };
  return cleanup;
}
