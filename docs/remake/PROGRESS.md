# Remake progress

Living log for the RAST remake (see `CLAUDE.md` and `docs/adr/0001-incremental-remake.md`).
Update it at the end of every session.

- **Current phase:** Phase 0 — Foundation
- **Branch / version:** `Remake` / 1.0.1 (not bumped per phase; releases are pre-release only until the final remake gate)
- **Last updated:** 2026-11-15

## Phase 0 checklist

- [ ] Split the repo into the target workspace with pure moves (ADR 0002)
- [ ] Create `rast-api` with `EntryDto` and `ListingDto` (from `src/storage/ops.rs`) and generate TypeScript types with `ts-rs`
- [ ] Create `rast-ui` with one Leptos component mounted in a RAST window, built with Trunk, embedded with `rust-embed`
- [ ] Record the current compressed JS bundle size as the baseline
- [ ] Run the app matrix on 0.18.x and store results in `tests/e2e/results/`
- [x] Create `docs/remake/PROGRESS.md` and the first ADRs
- [ ] Fix the README: duplicated "Linux apps" row and stale status line
- [ ] **Exit:** CI green, still a single release binary, no behaviour change

## Done

- Remake instructions (`CLAUDE.md`) and prompt pack (`docs/remake/PROMPTS.md`).
- Versioning rule: the whole remake uses 1.0.1 on the `Remake` branch, not
  bumped per phase; GitHub releases from the branch must be pre-release until
  the final remake gate passes (commit `aeeec51`).
- Progress log and ADR template (`docs/adr/0000-template.md`,
  `docs/adr/0001-incremental-remake.md`).
- **Quality gate baseline** (at `aeeec51`, all passing):
  - `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`: clean
  - `cargo test --locked --all-targets`: 750 + 33 + 15 + 7 + 2 tests (plus empty suites), 0 failed
  - `npm --prefix frontend test`: 149 passed, 0 failed
  - `npm --prefix frontend run build`: OK (existing chunk-size warning above 600 kB)

## In progress

- Nothing yet.

## Next

1. Phase 0.2 — workspace split (plan mode, `opusplan` / `high`); ADR 0002.
2. Phase 0.3 — `rast-api` + `ts-rs` (needs approval for the `ts-rs` dependency).
3. Phase 0.4 — `rast-ui` skeleton (needs approval for leptos, wasm-bindgen, web-sys, console_error_panic_hook).
4. Phase 0.5 — README fixes.
5. Phase 0.6 — baseline app matrix (ask before running the native instance).

## Open questions

- Does `scripts/changelog-section.sh` accept the `## [1.0.1] - Unreleased`
  heading? Check before the first pre-release.
- Which release is the cut-off for removing the legacy Docker/Selkies Linux-app
  backend (Phase 4)?
- Baseline JS bundle size (compressed `frontend/dist`) is not recorded yet.

## Repository map (as surveyed)

- `src/`: `access`, `api`, `appearance`, `auth`, `backup`, `docker`,
  `linux_apps`, `monitor`, `native`, `packages`, `realtime`, `rootfs`,
  `storage`, `terminal`, `worker`, plus `audit.rs`, `config.rs`, `db.rs`,
  `error.rs`, `events.rs`, `jobs.rs`, `secrets.rs`, `server.rs`, `state.rs`,
  `update.rs`, `users.rs`. Single crate today.
- `frontend/src/`: `apps/` (applications, backup, controlpanel, files, launcher,
  linuxapp, logs, monitor, packages, terminal, registry.ts), `components/`,
  `core/`, `shell/`, `styles/`, `types/`.
- `tests/e2e/`: Playwright scenarios, `run.mjs`, `perf/`, `results/`,
  terminal benches.
- `docs/`: `ARCHITECTURE.md`, `NATIVE-LINUX-APPS.md`, `TERMINAL.md`, `API.md`,
  `REALTIME.md` and others.
