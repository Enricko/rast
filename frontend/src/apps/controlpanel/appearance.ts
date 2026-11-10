// Control Panel → Appearance: theme and per-user wallpaper.
//
// This is the same editor the Personalize dialog uses, so there is exactly one
// appearance system. Changes preview immediately and are persisted with Save.

import {
  applyAppearance,
  getAppearanceService,
  normalizeAppearance,
  type Appearance,
} from "../../core/appearance";
import { createAppearanceEditor } from "../../core/appearance-editor";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { confirmDialog, errorMessage } from "../../core/ui";
import { notifyError, pageIntro, type TabRenderContext } from "./common";

export async function renderAppearanceTab(tab: TabRenderContext): Promise<() => void> {
  const { ctx, host, actions } = tab;
  host.replaceChildren();
  actions.replaceChildren();

  const service = getAppearanceService(ctx.api);
  let dirty = false;
  let draft: Appearance = normalizeAppearance(service.value);

  // Make sure the editor reflects the server copy (fresh login / another tab).
  try {
    draft = normalizeAppearance(await service.load(ctx.store.get().user?.id ?? 0));
  } catch (error) {
    ctx.toast.warning(`Using cached appearance: ${errorMessage(error)}`);
  }

  const editor = createAppearanceEditor({
    service,
    toast: ctx.toast,
    showTheme: true,
    onChange: (next) => {
      draft = next;
      dirty = true;
      updateActions();
    },
  });
  editor.setDraft(draft);

  const saveButton = h(
    "button",
    { class: "btn btn-primary", type: "button", disabled: true },
    iconSvg("check", 15),
    "Save changes",
  );
  const resetButton = h(
    "button",
    { class: "btn btn-subtle", type: "button" },
    iconSvg("restore", 15),
    "Reset to default",
  );
  const status = h("span", { class: "appearance-preview-note" });

  function updateActions(): void {
    saveButton.disabled = !dirty;
    status.textContent = dirty ? "Unsaved preview changes" : "All changes saved";
  }
  updateActions();

  saveButton.addEventListener("click", async () => {
    saveButton.disabled = true;
    try {
      const saved = await editor.save();
      draft = normalizeAppearance(saved);
      dirty = false;
      ctx.store.setThemePreference(saved.theme);
      applyAppearance(saved);
      updateActions();
      ctx.toast.success("Appearance saved");
    } catch (error) {
      notifyError(ctx, error);
      updateActions();
    }
  });

  resetButton.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "Reset appearance",
      message: "Restore the default wallpaper, fit, position and overlay? Your theme choice is kept.",
      confirmLabel: "Reset wallpaper",
    });
    if (!ok) return;
    resetButton.disabled = true;
    try {
      const reset = await editor.reset();
      draft = normalizeAppearance({ ...reset, theme: draft.theme });
      applyAppearance(draft);
      dirty = false;
      updateActions();
      ctx.toast.success("Wallpaper reset to default");
    } catch (error) {
      notifyError(ctx, error);
    } finally {
      resetButton.disabled = false;
    }
  });

  actions.append(saveButton, resetButton, status);
  host.append(
    pageIntro(
      "Appearance",
      "Choose how your desktop looks. Wallpaper and theme are stored with your account, so they follow you to any browser.",
    ),
    editor.el,
  );

  return () => {
    editor.dispose();
  };
}
