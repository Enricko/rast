#!/usr/bin/env bash
# Build the Leptos UI (crates/rast-ui) with Trunk into crates/rast-ui/dist.
# rast-server embeds that directory; a release build fails without it.
#
# Needs: the pinned Rust toolchain with the wasm32-unknown-unknown target
# (`rustup target add wasm32-unknown-unknown`) and Trunk 0.22
# (`cargo install trunk --locked --version '~0.22'`). Trunk downloads the
# matching wasm-bindgen and wasm-opt into its own cache on first use.
#
# Usage: scripts/build-ui.sh [extra trunk build args...]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

command -v trunk >/dev/null || {
  echo "trunk is not installed: cargo install trunk --locked --version '~0.22'" >&2
  exit 1
}
case "$(trunk --version)" in
  "trunk 0.22."*) ;;
  *) echo "unsupported $(trunk --version); Trunk 0.22.x is required" >&2; exit 1 ;;
esac

cd "$root/crates/rast-ui"
rm -rf dist
trunk build --release --locked "$@"

rm -f dist/.trunk-lock
for f in rast-ui.js rast-ui_bg.wasm; do
  [ -s "dist/$f" ] || { echo "missing dist/$f after the Trunk build" >&2; exit 1; }
done
echo "rast-ui built into crates/rast-ui/dist:"
ls -l dist
