// Control Panel → Profile: identity, display name, password, appearance and
// active sessions.

import { apiErrorMessage } from "../../core/api";
import { badge, card, iconButton, usageBar } from "../../core/components";
import { h } from "../../core/dom";
import { formatBytes, formatDateTime, relativeTime } from "../../core/format";
import { formField, passwordField, validatePassword, validateRequiredText, withButtonBusy } from "../../core/forms";
import { iconSvg } from "../../core/icons";
import type { MeResponse, SessionInfo } from "../../core/types";
import { confirmDialog, errorMessage } from "../../core/ui";
import { describeUserAgent, scrollToSection, type TabRenderContext } from "./common";

export async function renderProfileTab(tab: TabRenderContext): Promise<void> {
  const { ctx, host, actions } = tab;
  const me = await ctx.api.get<MeResponse>("/api/auth/me");
  const user = me.user;

  host.replaceChildren();
  actions.replaceChildren();
  const intro = h(
    "div",
    { class: "cp-intro" },
    h("h2", { class: "cp-intro-title" }, "Your profile"),
    h("p", { class: "cp-intro-detail muted" }, "Update how you appear on RAST, change your password and review where you are signed in. Wallpaper and theme live in the Appearance section."),
  );
  host.appendChild(intro);

  actions.appendChild(
    iconButton("refresh", "Refresh", () => {
      void reloadAll();
    }, { title: "Refresh profile" }),
  );

  // ---------------------------------------------------------------- identity
  const identity = h(
    "div",
    { class: "identity" },
    h("div", { class: "avatar avatar-xl" }, (user.displayName || user.username).slice(0, 1).toUpperCase()),
    h(
      "div",
      { class: "identity-text" },
      h("div", { class: "identity-name" }, user.displayName || user.username),
      h("div", { class: "identity-meta small muted" }, `@${user.username} · ${user.role === "admin" ? "Administrator" : "User"}`),
      h(
        "div",
        { class: "row wrap identity-badges" },
        badge(user.role === "admin" ? "Administrator" : "User", user.role === "admin" ? "info" : "neutral"),
        user.active ? badge("Active", "ok") : badge("Disabled", "neutral"),
        user.mustChangePassword ? badge("Password change required", "warn") : null,
      ),
    ),
  );

  const accountCard = card({
    title: "Account",
    subtitle: "Read-only account details",
    children: [
      identity,
      h(
        "dl",
        { class: "kv compact" },
        h("dt", {}, "Storage quota"),
        h("dd", {}, usageBar(user.usedBytes, user.quotaBytes, { unlimitedLabel: "no quota" })),
        h("dt", {}, "Last sign-in"),
        h("dd", {}, user.lastLoginAt ? `${relativeTime(user.lastLoginAt)} (${formatDateTime(user.lastLoginAt)})` : "Never"),
        h("dt", {}, "Account created"),
        h("dd", {}, formatDateTime(user.createdAt)),
        h("dt", {}, "Quota"),
        h("dd", {}, user.quotaBytes > 0 ? formatBytes(user.quotaBytes) : "Unlimited"),
      ),
    ],
  });

  // ------------------------------------------------------------ display name
  const displayInput = h("input", {
    class: "input",
    type: "text",
    value: user.displayName,
    autocomplete: "name",
    ariaLabel: "Display name",
  });
  const displayField = formField({
    label: "Display name",
    control: displayInput,
    required: true,
    hint: "Shown in the taskbar, user menus and activity logs.",
  });
  const displaySaved = h("span", { class: "saved-note small", ariaLive: "polite" });
  const saveDisplay = h("button", { class: "btn btn-primary", type: "button" }, "Save profile");
  saveDisplay.addEventListener("click", () => {
    const error = validateRequiredText(displayInput.value, "Display name", 64);
    if (error) {
      displayField.setError(error);
      displayInput.focus();
      return;
    }
    displayField.setError(null);
    void withButtonBusy(saveDisplay, "Saving…", async () => {
      try {
        await ctx.api.patch(`/api/users/${user.id}`, { displayName: displayInput.value.trim() });
        await ctx.refreshUser();
        displaySaved.textContent = "Saved";
        window.setTimeout(() => {
          displaySaved.textContent = "";
        }, 2400);
        ctx.toast.success("Display name updated");
        void reloadIdentity();
      } catch (error) {
        displayField.setError(apiErrorMessage(error));
      }
    });
  });
  const displayCard = card({
    title: "Profile",
    subtitle: "How your name appears across the workspace",
    children: [displayField.el, h("div", { class: "row wrap form-actions" }, saveDisplay, displaySaved)],
  });

  // ---------------------------------------------------------------- password
  const current = h("input", {
    class: "input",
    type: "password",
    autocomplete: "current-password",
    ariaLabel: "Current password",
  });
  const currentField = formField({ label: "Current password", control: current, required: true });
  const next = passwordField({ label: "New password", autocomplete: "new-password", required: true, showStrength: true });
  const confirm = h("input", {
    class: "input",
    type: "password",
    autocomplete: "new-password",
    ariaLabel: "Confirm new password",
  });
  const confirmField = formField({ label: "Confirm new password", control: confirm, required: true });
  const passwordButton = h("button", { class: "btn btn-primary", type: "button" }, "Change password");
  passwordButton.addEventListener("click", () => {
    currentField.setError(null);
    next.setError(null);
    confirmField.setError(null);
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
    void withButtonBusy(passwordButton, "Updating…", async () => {
      try {
        await ctx.api.post("/api/auth/password", {
          currentPassword: current.value,
          newPassword: next.value(),
        });
        current.value = "";
        next.reset();
        confirm.value = "";
        ctx.toast.success("Password changed. Other sessions were signed out.");
        void loadSessions();
      } catch (error) {
        currentField.setError(apiErrorMessage(error));
      }
    });
  });
  const passwordCard = card({
    title: "Password",
    subtitle: "Changing your password signs out all other sessions",
    children: [
      currentField.el,
      next.el,
      confirmField.el,
      h("div", { class: "row wrap form-actions" }, passwordButton),
    ],
  });

  // ---------------------------------------------------------------- sessions
  const sessionsCard = card({
    title: "Active sessions",
    subtitle: "Devices currently signed in to your account",
    actions: [
      iconButton("refresh", "Refresh sessions", () => void loadSessions(), { size: "sm" }),
      h(
        "button",
        { class: "btn btn-sm btn-danger", type: "button", onClick: () => void revokeOtherSessions() },
        "Sign out others",
      ),
    ],
    children: [h("div", { class: "sessions-host" }, h("div", { class: "loading-block" }, h("span", { class: "spinner" })))],
  });
  sessionsCard.id = "sessions";
  const sessionsHost = sessionsCard.querySelector<HTMLElement>(".sessions-host");

  // ------------------------------------------------------------------ layout
  const grid = h(
    "div",
    { class: "cp-grid" },
    accountCard,
    displayCard,
    passwordCard,
    h("div", { class: "cp-span-2" }, sessionsCard),
  );
  host.appendChild(grid);

  // -------------------------------------------------------------- behaviour
  async function loadSessions(): Promise<void> {
    if (!sessionsHost) return;
    sessionsHost.replaceChildren(h("div", { class: "loading-block" }, h("span", { class: "spinner" }), h("span", { class: "muted" }, "Loading sessions…")));
    try {
      const sessions = await ctx.api.get<SessionInfo[]>("/api/auth/sessions");
      sessionsHost.replaceChildren();
      if (sessions.length === 0) {
        sessionsHost.appendChild(h("div", { class: "table-empty" }, "No active sessions."));
        return;
      }
      const list = h("div", { class: "session-list", role: "list" });
      for (const session of sessions) {
        list.appendChild(sessionRow(session));
      }
      sessionsHost.appendChild(
        h(
          "div",
          {},
          h("div", { class: "small muted session-count" }, `${sessions.length} active session${sessions.length === 1 ? "" : "s"}`),
          list,
        ),
      );
    } catch (error) {
      sessionsHost.replaceChildren(
        h(
          "div",
          { class: "error-state inline-state" },
          h("div", { class: "error-title" }, "Could not load sessions"),
          h("div", { class: "error-detail" }, errorMessage(error)),
          h("button", { class: "btn btn-sm", type: "button", onClick: () => void loadSessions() }, "Retry"),
        ),
      );
    }
  }

  function sessionRow(session: SessionInfo): HTMLElement {
    const isCurrent = session.current === true;
    const revoke = h(
      "button",
      {
        class: "btn btn-sm btn-danger",
        type: "button",
        ariaLabel: "Revoke this session",
        onClick: () => void revokeSession(session),
      },
      "Revoke",
    );
    if (isCurrent) {
      revoke.disabled = true;
      revoke.title = "This is your current session. Use Sign out to end it.";
    }
    return h(
      "div",
      { class: "session-row", role: "listitem" },
      h("span", { class: "session-icon" }, iconSvg("monitor", 18)),
      h(
        "div",
        { class: "session-main" },
        h(
          "div",
          { class: "session-title" },
          describeUserAgent(session.userAgent),
          isCurrent ? badge("This device", "ok") : null,
        ),
        h(
          "div",
          { class: "session-meta small muted" },
          `${session.ip || "unknown IP"} · last active ${relativeTime(session.lastSeenAt)} · signed in ${formatDateTime(session.createdAt)}`,
        ),
      ),
      h("div", { class: "session-actions" }, revoke),
    );
  }

  async function revokeSession(session: SessionInfo): Promise<void> {
    const ok = await confirmDialog({
      title: "Revoke session",
      message: `Sign out the session from ${session.ip || "this device"}? The device will need to sign in again.`,
      confirmLabel: "Revoke session",
      danger: true,
    });
    if (!ok) return;
    try {
      await ctx.api.del(`/api/auth/sessions/${encodeURIComponent(session.id)}`);
      ctx.toast.success("Session revoked");
      void loadSessions();
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  async function revokeOtherSessions(): Promise<void> {
    const ok = await confirmDialog({
      title: "Sign out other sessions",
      message: "Sign out every session except this one? Other devices will need to sign in again.",
      confirmLabel: "Sign out others",
      danger: true,
    });
    if (!ok) return;
    try {
      await ctx.api.del("/api/auth/sessions");
      ctx.toast.success("Other sessions signed out");
      void loadSessions();
    } catch (error) {
      ctx.toast.error(apiErrorMessage(error));
    }
  }

  async function reloadIdentity(): Promise<void> {
    try {
      const fresh = await ctx.api.get<MeResponse>("/api/auth/me");
      if (fresh.user.displayName !== user.displayName) {
        user.displayName = fresh.user.displayName;
        const nameEl = identity.querySelector<HTMLElement>(".identity-name");
        if (nameEl) nameEl.textContent = fresh.user.displayName;
        const avatar = identity.querySelector<HTMLElement>(".avatar");
        if (avatar) avatar.textContent = fresh.user.displayName.slice(0, 1).toUpperCase();
      }
    } catch {
      /* the tray is already refreshed by ctx.refreshUser */
    }
  }

  async function reloadAll(): Promise<void> {
    await renderProfileTab(tab);
  }

  await loadSessions();
  scrollToSection(host, tab.section);
}

