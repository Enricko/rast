#!/usr/bin/env bash
# Build the E2E snapshot binary: a debug `rast` binary (fast to rebuild while
# the migration is in progress) copied to /tmp/rast-e2e/bin/rast.
#
# The debug binary serves the frontend from frontend/dist at runtime, so the
# frontend build step is only needed when the frontend sources changed. Pass
# --no-frontend to skip it, --release for an optimized baseline binary.
#
# Cargo builds may collide with other agents' builds in the same checkout:
# cargo waits on its lock, but a transient compile error caused by an
# in-flight edit is retried a few times.
#
# Usage: scripts/e2e/build-snapshot.sh [--release] [--no-frontend] [--attempts N]
set -euo pipefail

ROOT="${RAST_E2E_ROOT:-/tmp/rast-e2e}"
BIN="$ROOT/bin/rast"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

profile=debug
frontend=1
attempts=3
while [ $# -gt 0 ]; do
  case "$1" in
    --release) profile=release ;;
    --no-frontend) frontend=0 ;;
    --attempts) attempts="${2:?--attempts needs a number}"; shift ;;
    -h | --help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\033[1;34m[e2e/build]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[e2e/build]\033[0m WARNING: %s\n' "$*" >&2; }

mkdir -p "$ROOT/bin" "$ROOT/logs"
cd "$REPO_DIR"

if [ "$frontend" = 1 ]; then
  if [ -d frontend/node_modules ]; then
    say "building the frontend into frontend/dist"
    if ! npm --prefix frontend run build:only >>"$ROOT/logs/frontend-build.log" 2>&1; then
      warn "frontend build failed (see $ROOT/logs/frontend-build.log); keeping the existing dist"
    fi
  else
    warn "frontend/node_modules missing; skipping the frontend build (run npm --prefix frontend ci)"
  fi
fi

attempt=1
while :; do
  say "building the snapshot binary ($profile, attempt $attempt/$attempts)"
  log="$ROOT/logs/cargo-build.log"
  if [ "$profile" = release ]; then
    if cargo build --release --bin rast >>"$log" 2>&1; then
      built=target/release/rast
      break
    fi
  else
    if cargo build --bin rast >>"$log" 2>&1; then
      built=target/debug/rast
      break
    fi
  fi
  if [ "$attempt" -ge "$attempts" ]; then
    echo "--- last 40 lines of $log ---" >&2
    tail -n 40 "$log" >&2
    echo "the checkout does not compile right now (another agent may be mid-edit); retry later" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  warn "build failed (mid-migration tree?); retrying in 10s"
  sleep 10
done

install -m 0755 "$built" "$BIN"
say "snapshot binary: $BIN ($("$BIN" version | head -n 1))"
