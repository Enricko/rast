# 0001. Incremental remake in the same repository

- Status: accepted
- Date: 2026-11-15

## Context

RAST 0.18.x is a single Rust binary (Axum + SQLite) that embeds a
TypeScript/Vite frontend. It already runs in production on the development
host and on installed servers, which update through `rast update`.

The goal is a Linux Web OS written in full-stack Rust: one real Linux account
and one HOME per user, every installed Linux app in the launcher, and a Leptos
(WASM) frontend. A rewrite from scratch would leave RAST unusable for months,
lose the fixes recorded in `CHANGELOG.md`, and could not be tested against the
existing e2e scenarios.

## Decision

Remake RAST incrementally inside this repository, on the `Remake` branch,
following the phases in `CLAUDE.md`:

- RAST keeps working, and the existing tests stay green, at every commit.
- Phases run in order; a phase starts only after the previous phase's exit
  criteria pass.
- The whole remake uses version **1.0.1** and it is not bumped per phase. Any
  GitHub release from this branch is marked pre-release until the final
  remake gate passes, because `rast update` (`/releases/latest`) offers the
  newest non-pre-release to every installed server, production included.
- Changes are recorded under `## [1.0.1] - Unreleased` in `CHANGELOG.md`.
- Progress is tracked in `docs/remake/PROGRESS.md`; significant decisions get
  their own ADR.

## Consequences

- No big-bang cutover: TypeScript and Leptos apps run side by side during the
  migration, and the legacy Docker backend stays until its migration path has
  shipped.
- Every change carries the cost of compatibility (API, config keys, on-disk
  layout, forward-only migrations), so breaking changes need maintainer
  approval.
- The production safety rules in `CLAUDE.md` apply throughout: testing happens
  on the disposable instance only.
- A pre-release-only policy delays a normal release of 1.0.1 until the final
  remake gate; the maintainer decides when to publish.
