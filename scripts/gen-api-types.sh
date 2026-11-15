#!/usr/bin/env bash
# Regenerate the TypeScript types that ts-rs exports from crates/rast-api into
# frontend/src/generated/. Deletes every generated file first (README.md is
# hand-written and kept), so a type that was removed or renamed leaves no stale
# .ts file behind. Extra arguments are passed to `cargo test`.
#
# Usage: scripts/gen-api-types.sh [cargo test args...]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/frontend/src/generated"

mkdir -p "$out"
find "$out" -mindepth 1 ! -name README.md -delete

cd "$root"
cargo test --locked -p rast-api "$@"
