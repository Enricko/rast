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
  (deployment state and the staging-first plan).

### Notes

- The rewrite is **not deployed**. Production still runs the last released
  build; `docs/DEPLOYMENT.md` explains what has to exist before it can be.

