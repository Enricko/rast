// Desktop shell entry point: bootstrap, auth screen routing, desktop lifecycle
// and session-expiry recovery (including reopening the previous workspace).

import { ApiClient, ApiError, apiErrorMessage } from "../core/api";
import { h } from "../core/dom";
import { iconSvg } from "../core/icons";
import { EventStream } from "../core/sse";
import { Store } from "../core/store";
import { promptDialog } from "../core/ui";
import type { Bootstrap, MeResponse } from "../core/types";
import type { ToastCenter } from "../core/toast";
import type { AppDefinition } from "../core/app";
import type { WindowHandle } from "../core/wm";
import { renderLogin, renderSetup } from "./auth";
import { applyDocumentBrand, setDocumentTitle, setFavicon } from "./brand";
import { startDesktop } from "./desktop";
import { forcePasswordChange } from "./dialogs";
import type { DesktopController, SessionSnapshot, ShellDeps } from "./shared";

export async function boot(root: HTMLElement): Promise<void> {
  const deps: ShellDeps = {
    api: new ApiClient(),
    store: new Store(),
    events: new EventStream(),
    root,
  };
  const { api, store, events } = deps;

  store.subscribe((state) => {
    setDocumentTitle(state.siteName);
    setFavicon(state.theme);
  });

  let bootstrap: Bootstrap;
  try {
    bootstrap = await api.get<Bootstrap>("/api/bootstrap");
  } catch (error) {
    renderFatal(deps, "Cannot reach the RAST server", apiErrorMessage(error));
    return;
  }

  store.patch({
    siteName: bootstrap.product || "RAST",
    version: bootstrap.version,
    maxUploadBytes: bootstrap.maxUploadBytes,
    loginMessage: "",
  });
  applyDocumentBrand(store.get().siteName, store.get().theme);

  let desktop: DesktopController | null = null;
  let pendingRestore: SessionSnapshot | null = null;
  let handlingLostSession = false;

  const onAuthenticated = (me: MeResponse): void => {
    handlingLostSession = false;
    desktop?.dispose();
    desktop = startDesktop(deps, me, {
      restore: pendingRestore,
      onSignedOut: (notice) => {
        pendingRestore = null;
        showLogin(notice);
      },
      onSessionRevoked: (reason) => {
        if (handlingLostSession) return;
        handlingLostSession = true;
        pendingRestore = null;
        showLogin(
          reason
            ? `Your session was ended by the server (${reason}). Sign in again to continue.`
            : "Your session was ended by the server. Sign in again to continue.",
        );
      },
    });
    pendingRestore = null;
  };

  const showLogin = (notice?: string): void => {
    desktop?.dispose();
    desktop = null;
    events.stop();
    // Dialogs live outside the desktop root; never leave one floating over the
    // login screen after a forced sign-out.
    document.querySelectorAll(".modal-backdrop").forEach((node) => node.remove());
    store.patch({ user: null, csrfToken: "" });
    renderLogin(deps, {
      onAuthenticated,
      ...(notice ? { notice } : {}),
    });
  };

  api.onSessionLost(() => {
    if (!desktop || handlingLostSession) return;
    handlingLostSession = true;
    // Preserve the workspace: remember which apps were open, then re-open them
    // automatically after the user signs in again.
    pendingRestore = desktop.snapshot();
    desktop.dispose();
    desktop = null;
    events.stop();
    store.patch({ user: null, csrfToken: "" });
    renderLogin(deps, {
      notice: "Your session expired. Sign in to continue where you left off.",
      onAuthenticated,
    });
  });

  if (bootstrap.setupRequired) {
    store.patch({ version: bootstrap.version });
    renderSetup(deps, bootstrap, { onAuthenticated });
    return;
  }

  if (!bootstrap.authenticated) {
    showLogin();
    return;
  }
  try {
    const me = await api.get<MeResponse>("/api/auth/me");
    onAuthenticated(me);
  } catch (error) {
    if (error instanceof ApiError && (error.isUnauthorized || error.isForbidden)) {
      showLogin();
    } else {
      renderFatal(deps, "RAST is not ready", apiErrorMessage(error));
    }
  }
}

function renderFatal(deps: ShellDeps, title: string, detail: string): void {
  deps.root.replaceChildren();
  deps.root.className = "auth-shell";
  const mark = h("div", { class: "auth-mark" });
  mark.appendChild(iconSvg("logo", 30));
  const card = h(
    "div",
    { class: "auth-card auth-card-fatal", role: "alert" },
    h("div", { class: "auth-brand" }, mark, h("div", { class: "auth-brand-text" }, h("div", { class: "auth-brand-name" }, "RAST"), h("div", { class: "auth-brand-sub" }, "Resource Administration & Storage Toolkit"))),
    h("h1", { class: "auth-title" }, title),
    h("div", { class: "auth-subtitle" }, detail),
    h(
      "div",
      { class: "row auth-fatal-actions" },
      h("button", { class: "btn btn-primary", type: "button", onClick: () => window.location.reload() }, "Retry"),
    ),
    h(
      "div",
      { class: "auth-hint small muted" },
      "If the problem persists, check the RAST service status on the server (systemctl status rast) and the server logs.",
    ),
  );
  deps.root.appendChild(h("div", { class: "auth-layout auth-layout-fatal" }, card));
}

/** Kept for compatibility with earlier shell exports. */
export function promptPasswordChange(deps: ShellDeps, toast: ToastCenter): void {
  forcePasswordChange(deps, toast);
}

export { promptDialog };
export type { WindowHandle, AppDefinition, ShellDeps };
