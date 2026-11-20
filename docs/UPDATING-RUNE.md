# Updating Rune

RAST is built on [Rune](https://github.com/anomalyco/rune), a pre-release UI
framework. This document explains how Rune reaches this repository, how to find
out whether a new revision exists, and how to update it safely.

## How Rune is consumed

- Rune publishes **no releases, no tags and no crates.io package**. Its version
  is `0.0.1` and its updates are plain commits on the `dev` branch.
- We therefore consume it as a **git dependency pinned to one full 40-character
  `dev` revision**, in the root `Cargo.toml`:

  ```toml
  rune = { git = "https://github.com/anomalyco/rune", rev = "<40-char rev>" }
  ```

- Rune's rule is *“pin a rev of `dev`; `dev` is never rewritten”*: a pinned
  revision keeps building and is never moved under us.
- `Cargo.lock` records the same revision, so every build — ours, CI, a release —
  uses the exact same source until we deliberately update.
- Rune also **vendors six patched crates** in its repository. Cargo does not
  inherit `[patch.crates-io]` through a git dependency, so our root `Cargo.toml`
  repeats that table, on the same revision. When Rune bumps one of those crates
  or adds another, our table must follow. `scripts/update-rune.sh` regenerates
  it from Rune's manifest, so this cannot drift by hand.

Nothing updates automatically: there is no auto-update, no nightly channel and
no branch tracking. A build is reproducible until someone bumps the pin.

## Is there a new patch?

There are no patches to subscribe to — every commit on `dev` is a new
pre-release. To check *“did `dev` move?”*:

```sh
scripts/check-rune-pin.sh
```

- exit `0`: the pin is the head of `dev`;
- exit `10`: a newer revision exists (the script prints it).

CI runs the same script weekly (`.github/workflows/ci.yml`, job `rune-pin`), so
a new revision shows up as a failed scheduled build instead of being missed.

## Updating

Do it on a review branch, never straight on the main branch:

```sh
git switch -c chore/rune-<rev>
scripts/update-rune.sh            # latest `dev`
# or pin a specific revision:
scripts/update-rune.sh <40-char-rev>
```

The script:

1. resolves the target revision,
2. reads Rune's root `Cargo.toml` at that revision and rewrites our
   `[patch.crates-io]` table from it,
3. rewrites the `rev` in `Cargo.toml` and refreshes `Cargo.lock`,
4. builds, runs the headless test and renders a headless snapshot,
5. prints the Rune commits between the old and the new revision,
6. leaves the tree dirty for review — it never commits, tags or pushes.

Then run the gates and commit:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo run --locked -p rast-ui --bin rast -- --snap target/rast.png --scene opened
scripts/web-build.sh            # needs wasm-bindgen-cli matching Cargo.lock
```

```sh
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): update rune to <short rev>"
```

Mention breaking changes in the commit body, and add a `CHANGELOG.md` entry when
the update changes something users can see.

## When the update breaks the build

Rune is pre-release; breaking API changes between revisions are expected. Two
shapes:

- **Our code no longer compiles** — fix the call sites in the same commit. Check
  `crates/rune/README.md` at the new revision; it is the API reference.
- **Rune itself does not compile** — check that the toolchain matches Rune's
  `rust-toolchain.toml` (currently `1.99.1`). Bump our `rust-toolchain.toml` and
  this document in the same commit, then rerun.

If the new revision is not ready, roll back with
`git checkout -- Cargo.toml Cargo.lock` (or revert the commit). The pin always
makes that safe.

## Gotchas

- **`wasm-bindgen-cli` must match** the `wasm-bindgen` version in `Cargo.lock`
  (currently `0.2.129`). Reinstall it after a Rune update:
  `cargo install wasm-bindgen-cli --version <version> --locked --force`.
- **Patch both `winit` and `sctk-adwaita`** when patching either: Rune's winit
  calls into the vendored Wayland frame, and a workspace that patches only one
  does not compile. `scripts/update-rune.sh` handles this.
- **Toolchain**: CI and the workspace read `rust-toolchain.toml`; Rune's own pin
  is authoritative. A Rune update that bumps the compiler needs our file bumped
  too.
- **GPU in CI**: Rune needs a wgpu adapter even for headless rendering. CI uses
  the software Vulkan driver (`mesa-vulkan-drivers` + `VK_ICD_FILENAMES=…/lvp_icd.json`).
- **Web target**: Rune's web build needs WebGPU in the browser. There is no
  WebGL fallback in our page.

## Local Rune source

`.rune-src/` (gitignored) is a shallow checkout of Rune that
`scripts/update-rune.sh` refreshes and uses to read the target revision's
manifest and `git log`. Recreate it with:

```sh
git clone --depth 1 --branch dev https://github.com/anomalyco/rune.git .rune-src
```
