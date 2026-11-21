# Changelog

All notable changes to RAST are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project does not
follow semantic versioning yet (the rewrite starts at `0.1.0`).

## [Unreleased]

### Changed

- Started over: all of the previous RAST code (0.18.x and the 1.0.1 remake) was
  removed on 2026-11-21 and the desktop is being rebuilt on the
  [Rune](https://github.com/anomalyco/rune) UI framework. The old tree remains on
  branch `Remake` and tag `archive/pre-rune-rewrite-2026-11-21`; production keeps
  running the last released build until the rewrite ships.

### Added

- New Cargo workspace on Rust 1.99.1 with `crates/rast-ui`, the first Rune
  shell: one window, the RAST identity, and a headless-tested interactive
  element.
- `scripts/web-build.sh`: static WASM + WebGPU build of the shell.
- `scripts/check-rune-pin.sh` and `scripts/update-rune.sh`: Rune is pinned to a
  `dev` revision; these report newer revisions and move the pin together with
  the vendored-crate patch table.
- CI: format, clippy, tests, headless render proof, wasm build, and a weekly
  check for a newer Rune revision.
- **Client shell on the revive branch `revamp/remake`** (native Linux, Windows
  and macOS, plus the WebGPU browser build):
  - connects to the configured RAST server and shows *Connecting*,
    *Connected*, *Server unreachable* or *Set up the server*;
  - survives server downtime: it keeps running, retries on a backoff
    (1 s → 2 s → 4 s → 8 s → 15 s → 30 s) and reconnects by itself when the
    server answers again, re-checking every 10 s while connected;
  - *Reconnect now* for an immediate attempt;
  - Settings screen for the server address and for turning automatic
    reconnection **off** (optional, persisted);
  - per-OS settings file (`~/.config/rast/client.conf`,
    `~/Library/Application Support/rast/client.conf`, `%APPDATA%\rast\client.conf`),
    `RAST_CONFIG` to relocate it and `RAST_AUTO_RECONNECT=0` for one run.
- CI matrix builds and tests the app on Linux, Windows and macOS, renders every
  client screen headlessly, and packages release binaries for Linux and Windows
  on demand.
- `docs/CLIENT.md` (behaviour, settings, platforms) and `docs/DEPLOYMENT.md`
  (deployment state, what the sandbox cannot do, and the safe staging-first plan).
- Deployment pieces for the browser build: `deploy/nginx-revamp.conf` (serve it
  at `/revamp/` next to the running desktop without replacing it),
  `scripts/publish-web.sh` (build + print the root commands) and
  `scripts/serve-web.py` (dev server with the right `.wasm` MIME type).
- `deploy/install-revamp.sh`: one idempotent root command that publishes the
  browser build at the new `desktop-revamp.lufria.tech` (static files, nginx
  site on `127.0.0.1:8722`, Cloudflare Tunnel ingress with a config backup,
  local and public checks). The old `desktop.lufria.tech` is untouched.
- The page bootstrap moved to `crates/rast-ui/boot.js` so the deployed page
  works under the production CSP (`script-src 'self' 'wasm-unsafe-eval'`, no
  inline script); `scripts/serve-web.py` now mirrors the production headers, and
  the browser build was verified booting in headless Chromium with WebGPU.
- **Device login** in the client shell: first launch creates a local account
  (username, 8+ character password, confirmation), later launches open on a
  sign-in screen, and *Sign out* returns to it. The password is hashed with
  Argon2id and stored in a `0600` file per OS; the plaintext is never stored,
  logged or sent. Argon2id parameters, file paths and the recommended split
  between the device file and the server's future SQLite database are in
  `docs/AUTH-STORAGE.md`.
- `deploy/install-revamp.sh --web-only`: republish the static browser build
  without touching nginx or the tunnel (fast path after the first deployment).
- Browser login: the wasm build now opens on the same device login, storing only
  the Argon2id hash in this browser's `localStorage` (native builds keep the
  `0600` file). `desktop-revamp.lufria.tech` was republished with it and verified
  in headless Chromium.
- `desktop-revamp.lufria.tech` is live: the installer added the nginx site on
  `127.0.0.1:8722` and the Cloudflare Tunnel ingress; the old
  `desktop.lufria.tech` keeps serving the released RAST.

### Fixed

- The revamp web build publishes matching JavaScript and WebAssembly under a
  content-addressed URL. The installer switches the HTML only after all assets
  are ready and keeps old release files, so deploys no longer mix cached glue
  with a different Wasm binary.
- Restored RAST's classic login in Rune: Alpine Dawn wallpaper, the brand panel,
  blue sign-in action, and a light/dark switch. Small windows use a scrollable
  single column. Existing local accounts keep working without migration.
- The classic browser login uses a responsive HTML form instead of a GPU
  canvas. Browser zoom and mobile scrolling work without reinitializing a
  render surface. Rune still renders the unlocked workspace; saved local
  credentials and native sign-in are unchanged.

### Notes

- The preview runs at `desktop-revamp.lufria.tech`; production at
  `desktop.lufria.tech` still runs the previous release.

