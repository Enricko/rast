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
