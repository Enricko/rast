// Login and first-run setup screens.

import { ApiError, apiErrorMessage } from "../core/api";
import { h } from "../core/dom";
import {
  formField,
  passwordField,
  validatePassword,
  validateRequiredText,
  validateUsername,
  withButtonBusy,
} from "../core/forms";
import { formatDuration } from "../core/format";
import { iconSvg } from "../core/icons";
import type { Bootstrap, LoginResponse, MeResponse } from "../core/types";
import { copyText } from "../core/ui";
import type { AuthOptions, ShellDeps } from "./shared";

const VERSION_FALLBACK = "";

async function loginAndLoad(deps: ShellDeps, username: string, password: string): Promise<MeResponse> {
  const response = await deps.api.post<LoginResponse>(
    "/api/auth/login",
    { username, password },
    { allowUnauthorized: true },
  );
  deps.api.setCsrfToken(response.csrfToken);
  const me = await deps.api.get<MeResponse>("/api/auth/me");
  deps.api.setCsrfToken(me.csrfToken);
  deps.store.patch({
    user: me.user,
    features: me.features,
    csrfToken: me.csrfToken,
    sessionExpiresAt: me.sessionExpiresAt,
  });
  return me;
}

function brandLockup(compact = false): HTMLElement {
  const mark = h("span", { class: compact ? "auth-mark auth-mark-sm" : "auth-mark" });
  mark.appendChild(iconSvg("logo", compact ? 22 : 30));
  return h(
    "div",
    { class: compact ? "auth-brand auth-brand-compact" : "auth-brand" },
    mark,
    h(
      "div",
      { class: "auth-brand-text" },
      h("div", { class: "auth-brand-name" }, "RAST"),
      h("div", { class: "auth-brand-sub" }, "Resource Administration & Storage Toolkit"),
    ),
  );
}

/** Shared split-screen layout used by login and setup. */
function renderAuthLayout(
  deps: ShellDeps,
  content: HTMLElement,
  options: { visualDetail?: string } = {},
): { card: HTMLElement; cardBody: HTMLElement } {
  const store = deps.store;
  deps.root.replaceChildren();
  deps.root.className = "auth-shell";

  const features: Array<{ icon: string; title: string; detail: string }> = [
    { icon: "files", title: "Files & shared folders", detail: "Browse, upload and manage server storage." },
    { icon: "users", title: "People & permissions", detail: "Accounts, quotas, roles and an audit trail." },
    { icon: "activity", title: "Live operations", detail: "Monitoring, containers and backups in one place." },
  ];
  const featureList = h("ul", { class: "auth-features" });
  for (const feature of features) {
    featureList.appendChild(
      h(
        "li",
        { class: "auth-feature" },
        h("span", { class: "auth-feature-icon" }, iconSvg(feature.icon, 18)),
        h(
          "span",
          { class: "auth-feature-text" },
          h("span", { class: "auth-feature-title" }, feature.title),
          h("span", { class: "auth-feature-detail" }, feature.detail),
        ),
      ),
    );
  }

  const visual = h(
    "div",
    { class: "auth-visual" },
    h("div", { class: "auth-visual-glow", ariaHidden: true }),
    brandLockup(),
    h(
      "p",
      { class: "auth-tagline" },
      options.visualDetail ?? "A calm, fast home for your self-hosted server. Everything stays on your hardware.",
    ),
    featureList,
    h("div", { class: "auth-visual-foot small faint" }, "Self-hosted · No telemetry · Original interface design"),
  );

  const card = h("div", { class: "auth-card" }, content);
  const version = store.get().version;
  const themeToggle = h("button", {
    class: "auth-theme-toggle",
    type: "button",
    ariaLabel: "Toggle dark and light mode",
    title: "Toggle dark and light mode",
    onClick: () => store.toggleTheme(),
  });
  const refreshTheme = () => {
    themeToggle.replaceChildren(iconSvg(store.get().theme === "dark" ? "sun" : "moon", 17));
    themeToggle.setAttribute(
      "aria-label",
      store.get().theme === "dark" ? "Switch to light mode" : "Switch to dark mode",
    );
  };
  refreshTheme();
  const unsubscribe = store.subscribe(refreshTheme);
  const detachCheck = window.setInterval(() => {
    if (!themeToggle.isConnected) {
      window.clearInterval(detachCheck);
      unsubscribe();
    }
  }, 1000);

  const panel = h(
    "div",
    { class: "auth-panel" },
    themeToggle,
    card,
    h(
      "div",
      { class: "auth-foot" },
      h("span", {}, version ? `RAST v${version}` : "RAST"),
      h("span", {}, "Sessions are secured with HttpOnly cookies"),
    ),
  );

  deps.root.appendChild(h("div", { class: "auth-layout" }, visual, panel));
  return { card, cardBody: card };
}

function alertBox(message?: string): HTMLElement {
  const alert = h("div", { class: "auth-alert", role: "alert", hidden: !message }, message ?? "");
  return alert;
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

export function renderLogin(deps: ShellDeps, options: AuthOptions): void {
  const version = deps.store.get().version || VERSION_FALLBACK;
  const notice = options.notice;
  const usernameInput = h("input", {
    class: "input",
    type: "text",
    name: "username",
    autocomplete: "username",
    placeholder: "username",
    ariaLabel: "Username",
    spellcheck: false,
  });
  const usernameField = formField({ label: "Username", control: usernameInput, required: true });
  const password = passwordField({
    label: "Password",
    autocomplete: "current-password",
    placeholder: "password",
    showStrength: false,
    required: true,
  });
  const capsHint = h("div", { class: "auth-caps", hidden: true }, "Caps Lock is on");
  const alert = alertBox(notice ? "" : undefined);
  if (notice) {
    alert.className = "auth-alert auth-alert-info";
    alert.hidden = false;
    alert.textContent = notice;
  }
  const submit = h(
    "button",
    { class: "btn btn-primary auth-submit", type: "submit" },
    iconSvg("arrowRight", 17),
    "Sign in",
  );

  const setCaps = (event: KeyboardEvent) => {
    const active = typeof event.getModifierState === "function" && event.getModifierState("CapsLock");
    capsHint.hidden = !active;
  };
  password.input.addEventListener("keydown", setCaps);
  password.input.addEventListener("keyup", setCaps);

  let lockTimer: number | undefined;
  let lockRemaining = 0;

  const startLockout = (seconds: number, message: string): void => {
    lockRemaining = Math.max(1, Math.round(seconds));
    submit.disabled = true;
    password.input.disabled = true;
    alert.className = "auth-alert auth-alert-warn";
    alert.hidden = false;
    const render = () => {
      if (!alert.isConnected) {
        if (lockTimer !== undefined) window.clearInterval(lockTimer);
        return;
      }
      alert.textContent = `${message} Try again in ${formatDuration(lockRemaining)}.`;
    };
    render();
    if (lockTimer !== undefined) window.clearInterval(lockTimer);
    lockTimer = window.setInterval(() => {
      lockRemaining -= 1;
      if (lockRemaining <= 0) {
        window.clearInterval(lockTimer);
        lockTimer = undefined;
        submit.disabled = false;
        password.input.disabled = false;
        alert.className = "auth-alert auth-alert-info";
        alert.textContent = "You can try signing in again now.";
        return;
      }
      render();
    }, 1000);
  };

  const form = h(
    "form",
    {
      class: "auth-form",
      noValidate: true,
      onSubmit: async (event) => {
        event.preventDefault();
        if (submit.disabled) return;
        usernameField.setError(null);
        password.setError(null);
        let invalid = false;
        if (!usernameInput.value.trim()) {
          usernameField.setError("Enter your username");
          invalid = true;
        }
        if (!password.value()) {
          password.setError("Enter your password");
          invalid = true;
        }
        if (invalid) {
          (usernameInput.value.trim() ? password.input : usernameInput).focus();
          return;
        }
        if (notice) {
          alert.hidden = true;
        }
        await withButtonBusy(submit, "Signing in…", async () => {
          try {
            const me = await loginAndLoad(deps, usernameInput.value.trim(), password.value());
            options.onAuthenticated(me);
          } catch (error) {
            if (error instanceof ApiError && error.isRateLimited) {
              startLockout(error.retryAfterSecs ?? 60, error.message || "Too many sign-in attempts.");
            } else if (error instanceof ApiError && error.isNetworkError) {
              alert.className = "auth-alert auth-alert-error";
              alert.hidden = false;
              alert.textContent = "Cannot reach the RAST server. Check your connection and try again.";
            } else {
              alert.className = "auth-alert auth-alert-error";
              alert.hidden = false;
              alert.textContent = apiErrorMessage(error);
              password.input.value = "";
              password.input.focus();
            }
          }
        });
      },
    },
    h("label", { class: "field" }, h("span", { class: "field-label" }, "Username"), usernameInput),
    password.el,
    capsHint,
    alert,
    submit,
  );

  const content = h(
    "div",
    { class: "auth-content" },
    h("div", { class: "auth-card-brand" }, brandLockup(true)),
    h("h1", { class: "auth-title" }, "Welcome back"),
    h("div", { class: "auth-subtitle" }, "Sign in to your server workspace."),
    form,
    h(
      "div",
      { class: "auth-hint small muted" },
      "No public registration — ask an administrator for an account.",
    ),
    h("div", { class: "auth-version small faint" }, version ? `RAST v${version}` : "RAST"),
  );
  renderAuthLayout(deps, content);
  window.setTimeout(() => usernameInput.focus(), 40);
}

// ---------------------------------------------------------------------------
// First-run setup
// ---------------------------------------------------------------------------

export function renderSetup(
  deps: ShellDeps,
  bootstrap: Bootstrap,
  options: { onAuthenticated(me: MeResponse): void },
): void {
  const tokenRequired = bootstrap.setupTokenRequired;
  const tokenInput = h("input", {
    class: "input mono",
    type: "text",
    autocomplete: "off",
    spellcheck: false,
    placeholder: "rast_setup_…",
    ariaLabel: "Setup token",
  });
  const tokenField = formField({
    label: "Setup token",
    control: tokenInput,
    required: tokenRequired,
    hint: tokenRequired
      ? "Generate one on the server with the command below, then paste it here."
      : "This server does not require a setup token.",
  });
  const command = "sudo -u rast rast setup-token";
  const copyCommand = h(
    "button",
    {
      class: "btn btn-sm auth-token-copy",
      type: "button",
      onClick: () => {
        void copyText(command).then((ok) => {
          copyCommand.textContent = ok ? "Copied" : "Copy failed";
          window.setTimeout(() => {
            copyCommand.replaceChildren(iconSvg("copy", 14), "Copy command");
          }, 1600);
        });
      },
    },
    iconSvg("copy", 14),
    "Copy command",
  );

  const usernameInput = h("input", {
    class: "input",
    type: "text",
    autocomplete: "username",
    value: "admin",
    ariaLabel: "Administrator username",
    spellcheck: false,
  });
  const usernameField = formField({
    label: "Administrator username",
    control: usernameInput,
    required: true,
    hint: "Letters, digits, dot, dash and underscore.",
  });
  const displayInput = h("input", {
    class: "input",
    type: "text",
    autocomplete: "name",
    value: "Administrator",
    ariaLabel: "Display name",
  });
  const displayField = formField({ label: "Display name", control: displayInput });
  const password = passwordField({
    label: "Password",
    autocomplete: "new-password",
    required: true,
    showStrength: true,
  });
  const confirm = h("input", {
    class: "input",
    type: "password",
    autocomplete: "new-password",
    ariaLabel: "Confirm password",
  });
  const confirmField = formField({ label: "Confirm password", control: confirm, required: true });
  const alert = alertBox();
  const submit = h(
    "button",
    { class: "btn btn-primary auth-submit", type: "submit" },
    iconSvg("shield", 17),
    "Create administrator",
  );

  const form = h(
    "form",
    {
      class: "auth-form",
      noValidate: true,
      onSubmit: async (event) => {
        event.preventDefault();
        if (submit.disabled) return;
        tokenField.setError(null);
        usernameField.setError(null);
        displayField.setError(null);
        password.setError(null);
        confirmField.setError(null);
        alert.hidden = true;
        let invalid = false;
        if (tokenRequired && !tokenInput.value.trim()) {
          tokenField.setError("Paste the setup token printed by the server command");
          invalid = true;
        }
        const usernameError = validateUsername(usernameInput.value);
        if (usernameError) {
          usernameField.setError(usernameError);
          invalid = true;
        }
        const displayError = displayInput.value.trim() ? validateRequiredText(displayInput.value, "Display name", 64) : null;
        if (displayError) {
          displayField.setError(displayError);
          invalid = true;
        }
        const passwordError = validatePassword(password.value());
        if (passwordError) {
          password.setError(passwordError);
          invalid = true;
        }
        if (password.value() !== confirm.value) {
          confirmField.setError("Passwords do not match");
          invalid = true;
        }
        if (invalid) return;

        await withButtonBusy(submit, "Creating administrator…", async () => {
          try {
            await deps.api.post(
              "/api/setup",
              {
                token: tokenRequired ? tokenInput.value.trim() : undefined,
                username: usernameInput.value.trim(),
                password: password.value(),
                displayName: displayInput.value.trim() || undefined,
              },
              { allowUnauthorized: true },
            );
            // Sign in immediately after successful setup for a smooth first run.
            try {
              const me = await loginAndLoad(deps, usernameInput.value.trim(), password.value());
              options.onAuthenticated(me);
            } catch {
              renderLogin(deps, {
                notice: "Administrator created. Sign in to continue.",
                onAuthenticated: options.onAuthenticated,
              });
            }
          } catch (error) {
            if (error instanceof ApiError && (error.isForbidden || error.isConflict)) {
              if (error.code === "forbidden" && /setup has already been completed/i.test(error.message)) {
                renderLogin(deps, {
                  notice: "This server has already been set up. Sign in to continue.",
                  onAuthenticated: options.onAuthenticated,
                });
                return;
              }
              tokenField.setError(apiErrorMessage(error));
              tokenInput.focus();
              return;
            }
            alert.className = "auth-alert auth-alert-error";
            alert.hidden = false;
            alert.textContent = apiErrorMessage(error);
          }
        });
      },
    },
    tokenRequired ? tokenField.el : null,
    tokenRequired ? h("div", { class: "auth-token-hint" }, h("span", { class: "mono small" }, command), copyCommand) : null,
    h("div", { class: "auth-divider", role: "separator", ariaLabel: "Administrator account" }),
    usernameField.el,
    displayField.el,
    password.el,
    confirmField.el,
    alert,
    submit,
  );

  const content = h(
    "div",
    { class: "auth-content auth-content-setup" },
    h("div", { class: "auth-card-brand" }, brandLockup(true)),
    h("h1", { class: "auth-title" }, "Set up RAST"),
    h(
      "div",
      { class: "auth-subtitle" },
      bootstrap.version
        ? `Create the first administrator account for v${bootstrap.version}.`
        : "Create the first administrator account.",
    ),
    h(
      "div",
      { class: "auth-notice small" },
      "No administrator exists yet. This one-time setup creates the first admin account; later accounts are managed from the Control Panel.",
    ),
    form,
  );
  renderAuthLayout(deps, content, {
    visualDetail: bootstrap.description || "Welcome to your new server workspace. Start by creating the first administrator account.",
  });
  window.setTimeout(() => (tokenRequired ? tokenInput : usernameInput).focus(), 40);
}
