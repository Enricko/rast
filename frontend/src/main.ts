// RAST web entry point: load design system styles and start the desktop shell.
//
// Styles are imported in cascade order: the base design system first, then the
// shell, then application-specific styles (apps.css is owned by the app team).

import "./styles/base.css";
import "./styles/shell.css";
import "./styles/apps.css";
import "./styles/theme.css";
import { boot } from "./shell";

const SPLASH_TIMEOUT_MS = 12_000;

function dismissSplash(): void {
  document.body.classList.add("app-ready");
  const splash = document.getElementById("boot-splash");
  if (splash) {
    window.setTimeout(() => splash.remove(), 460);
  }
}

function fatalFallback(error: unknown): void {
  const app = document.getElementById("app");
  if (!app) return;
  app.replaceChildren();
  app.className = "auth-shell";
  const card = document.createElement("div");
  card.className = "auth-card auth-card-fatal";
  const title = document.createElement("h1");
  title.className = "auth-title";
  title.textContent = "RAST could not start";
  const detail = document.createElement("div");
  detail.className = "auth-subtitle";
  detail.textContent = error instanceof Error ? error.message : "Unexpected startup error";
  const retry = document.createElement("button");
  retry.className = "btn btn-primary";
  retry.type = "button";
  retry.textContent = "Reload";
  retry.addEventListener("click", () => window.location.reload());
  card.append(title, detail, retry);
  app.appendChild(card);
}

const app = document.getElementById("app");
if (app) {
  bootstrap();
} else {
  dismissSplash();
}

function bootstrap(): void {
  const failsafe = window.setTimeout(dismissSplash, SPLASH_TIMEOUT_MS);
  boot(document.getElementById("app") as HTMLElement)
    .then(() => {
      window.clearTimeout(failsafe);
      dismissSplash();
    })
    .catch((error: unknown) => {
      window.clearTimeout(failsafe);
      console.error("RAST failed to start", error);
      dismissSplash();
      fatalFallback(error);
    });
}
