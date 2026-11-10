// Shell-level dialogs: forced password change, keyboard shortcuts, about and
// the desktop wallpaper picker.

import { apiErrorMessage } from "../core/api";
import { h } from "../core/dom";
import { formField, passwordField, validatePassword } from "../core/forms";
import { iconSvg } from "../core/icons";
import type { ToastCenter } from "../core/toast";
import type { MeResponse, User } from "../core/types";
import { modal } from "../core/ui";
import type { ShellDeps } from "./shared";

/** Blocking dialog shown when the account has mustChangePassword set. */
export function forcePasswordChange(
  deps: ShellDeps,
  toast: ToastCenter,
  options: { onDone?: (user: User) => void } = {},
): void {
  const current = h("input", {
    class: "input",
    type: "password",
    autocomplete: "current-password",
    placeholder: "Current password",
  });
  const currentField = formField({ label: "Current password", control: current, required: true });
  const next = passwordField({
    label: "New password",
    autocomplete: "new-password",
    required: true,
    showStrength: true,
  });
  const confirm = h("input", {
    class: "input",
    type: "password",
    autocomplete: "new-password",
    placeholder: "Repeat new password",
  });
  const confirmField = formField({ label: "Confirm new password", control: confirm, required: true });
  const alert = h("div", { class: "banner banner-warn", hidden: true });
  const body = h(
    "div",
    { class: "col" },
    h(
      "p",
      { class: "muted" },
      "Your administrator requires a new password before you continue. The new password must satisfy the server policy shown below.",
    ),
    currentField.el,
    next.el,
    confirmField.el,
    alert,
  );

  modal({
    title: "Set a new password",
    body,
    dismissible: false,
    actions: [
      {
        label: "Update password",
        variant: "primary",
        keepOpen: true,
        onClick: async (closeModal) => {
          currentField.setError(null);
          next.setError(null);
          confirmField.setError(null);
          alert.hidden = true;
          let invalid = false;
          if (!current.value) {
            currentField.setError("Enter your current password");
            invalid = true;
          }
          const policyError = validatePassword(next.value());
          if (policyError) {
            next.setError(policyError);
            invalid = true;
          }
          if (next.value() !== confirm.value) {
            confirmField.setError("Passwords do not match");
            invalid = true;
          }
          if (invalid) return;
          try {
            await deps.api.post("/api/auth/password", {
              currentPassword: current.value,
              newPassword: next.value(),
            });
            const me = await deps.api.get<MeResponse>("/api/auth/me");
            deps.api.setCsrfToken(me.csrfToken);
            deps.store.patch({
              user: me.user,
              features: me.features,
              csrfToken: me.csrfToken,
              sessionExpiresAt: me.sessionExpiresAt,
            });
            toast.success("Password updated");
            closeModal();
            options.onDone?.(me.user);
          } catch (error) {
            alert.hidden = false;
            alert.textContent = apiErrorMessage(error);
            throw error;
          }
        },
      },
    ],
  });
  window.setTimeout(() => current.focus(), 40);
}

const SHORTCUTS: Array<{ keys: string[]; label: string }> = [
  { keys: ["Ctrl", "Space"], label: "Open the start menu and application search" },
  { keys: ["Cmd", "Space"], label: "Open the start menu (macOS)" },
  { keys: ["?"], label: "Show this keyboard shortcuts dialog" },
  { keys: ["Esc"], label: "Close menus, dialogs and popovers" },
  { keys: ["↑", "↓", "←", "→"], label: "Move between desktop icons or search results" },
  { keys: ["Enter"], label: "Open the selected application" },
  { keys: ["Tab"], label: "Move focus between controls" },
  { keys: ["Double-click"], label: "Maximize or restore a window title bar" },
  { keys: ["Right-click"], label: "Open the desktop, icon or taskbar context menu" },
];

export function showShortcutsDialog(): void {
  const list = h("div", { class: "shortcut-list" });
  for (const shortcut of SHORTCUTS) {
    list.appendChild(
      h(
        "div",
        { class: "shortcut-row" },
        h("span", { class: "shortcut-keys" }, ...shortcut.keys.map((key) => h("kbd", {}, key))),
        h("span", { class: "shortcut-label" }, shortcut.label),
      ),
    );
  }
  modal({
    title: "Keyboard shortcuts",
    body: h(
      "div",
      {},
      h(
        "p",
        { class: "muted" },
        "RAST is designed to be fully usable from the keyboard. On Windows and Linux use Ctrl where macOS uses Cmd.",
      ),
      list,
    ),
  });
}

export function showAboutDialog(version: string, siteName: string): void {
  const mark = h("div", { class: "about-mark" });
  mark.appendChild(iconSvg("logo", 34));
  modal({
    title: "About",
    body: h(
      "div",
      { class: "about" },
      mark,
      h("div", { class: "about-name" }, siteName.trim() || "RAST"),
      h("div", { class: "about-version muted" }, version ? `Version ${version}` : "Version unavailable"),
      h(
        "p",
        { class: "about-copy muted" },
        "RAST — Resource Administration & Storage Toolkit. A self-hosted server administration workspace: files, users, containers, monitoring and backups in one desktop-like interface.",
      ),
      h("div", { class: "about-meta small faint" }, "Original interface design. No telemetry. All data stays on your server."),
    ),
  });
}
