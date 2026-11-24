#!/usr/bin/env bash
# Atomic, verifiable production deployment for RAST.
#
# Why this exists: a hand-rolled deployment replaced /usr/local/bin/rast and
# restarted only rast.service, leaving the privileged helper running from an
# unlinked inode ("/usr/local/bin/rast (deleted)") and breaking Terminal.
# This script installs versioned binaries behind a stable symlink, restarts the
# three services together, keeps the previous versions on disk (running
# processes keep their inode), smoke-tests Terminal and rolls back on failure.
#
# Usage (as root):
#   scripts/deploy-release.sh [--skip-tests] [--smoke-terminal] [--dry-run]
#
# Environment:
#   RAST_DEPLOY_SMOKE_USER   test account for the Terminal smoke (default: t-admin)
#   RAST_DEPLOY_SMOKE_CREDS  credentials file (default: /home/lufria/.config/rast-test/creds)
#   RAST_DEPLOY_BASE_URL     public URL for the smoke (default: http://127.0.0.1:8720 is not used; https://desktop.lufria.tech)
#   RAST_DEPLOY_KEEP         number of old versioned binaries to keep (default: 5)
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BIN_DIR=/usr/local/bin
BIN="$BIN_DIR/rast"
RELEASES=/usr/local/lib/rast-releases
KEEP=${RAST_DEPLOY_KEEP:-5}
SKIP_TESTS=0
SMOKE_TERMINAL=0
DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        --skip-tests) SKIP_TESTS=1 ;;
        --smoke-terminal) SMOKE_TERMINAL=1 ;;
        --dry-run) DRY_RUN=1 ;;
        -h | --help)
            sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "deploy-release: unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

say() { printf '\033[1;34m[deploy]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[deploy] WARNING:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[deploy] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }
run() { if [ "$DRY_RUN" = 1 ]; then printf '[dry-run] %s\n' "$*"; else "$@"; fi; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
cd "$ROOT"

# 0. Repository and target revision -----------------------------------------
BRANCH=$(git rev-parse --abbrev-ref HEAD)
[ "$BRANCH" = "main" ] || die "not on main (on '$BRANCH'); deploy only from main"
git diff --quiet || die "working tree has unstaged changes; commit first"
git diff --cached --quiet || die "index has staged changes; commit first"
SOURCE_SHA=$(git rev-parse HEAD)
say "source revision: $SOURCE_SHA ($(git log -1 --format='%s'))"
git fetch origin --quiet 2>/dev/null || warn "could not fetch origin (offline?)"
if git rev-parse --verify origin/main >/dev/null 2>&1; then
    if [ "$(git rev-parse origin/main)" != "$SOURCE_SHA" ]; then
        warn "HEAD differs from origin/main — the deployed build is not the remote-tested revision"
    fi
fi

# 1. Tests -------------------------------------------------------------------
if [ "$SKIP_TESTS" = 0 ]; then
    command -v cargo >/dev/null || die "cargo not found"
    say "running Rust tests (lib)"
    (cd "$ROOT" && CARGO_HOME="$ROOT/.cargo-home" cargo test --locked --lib --quiet) \
        || die "Rust lib tests failed; not deploying"
    if [ -x "$ROOT/frontend/node_modules/.bin/tsc" ]; then
        say "running frontend typecheck + unit tests"
        (cd "$ROOT/frontend" && ./node_modules/.bin/tsc --noEmit) \
            || die "frontend typecheck failed; not deploying"
        (cd "$ROOT/frontend" && npm test --silent) || die "frontend tests failed; not deploying"
    else
        warn "frontend dependencies missing; skipping frontend tests"
    fi
else
    warn "tests skipped by --skip-tests"
fi

# 2. Build from the committed revision --------------------------------------
if [ "$DRY_RUN" = 0 ]; then
    say "building the release artifact from $SOURCE_SHA"
    if [ -x "$ROOT/frontend/node_modules/.bin/vite" ]; then
        (cd "$ROOT/frontend" && npm run build --silent) || die "frontend build failed"
    else
        warn "frontend dependencies missing; the binary will embed the existing frontend/dist"
    fi
    (cd "$ROOT" && CARGO_HOME="$ROOT/.cargo-home" cargo build --release --locked --quiet) \
        || die "release build failed"
fi
RELEASE_BIN="$ROOT/target/release/rast"
[ "$DRY_RUN" = 1 ] || [ -x "$RELEASE_BIN" ] || die "release binary missing"
BIN_SHA=$(sha256sum "$RELEASE_BIN" | cut -d' ' -f1)
REPORTED=$($RELEASE_BIN version | head -n1)
say "built: $REPORTED"
say "sha256: $BIN_SHA"
say "record:  source=$SOURCE_SHA binary_sha256=$BIN_SHA deployed_at=$(date -Is)"

# 3. Backups for rollback ----------------------------------------------------
STAMP=$(date +%Y%m%d-%H%M%S)
ROLLBACK_DIR="/var/backups/rast-deploy-$STAMP"
CURRENT_TARGET=$(readlink -f "$BIN" 2>/dev/null || true)
run mkdir -p "$ROLLBACK_DIR"
if [ -n "$CURRENT_TARGET" ] && [ -f "$CURRENT_TARGET" ]; then
    run cp -a "$CURRENT_TARGET" "$ROLLBACK_DIR/previous-binary"
    say "backed up current binary: $CURRENT_TARGET"
fi
run cp -a /etc/rast/rast.toml "$ROLLBACK_DIR/rast.toml" 2>/dev/null || true
run cp -a /usr/local/bin/rast "$ROLLBACK_DIR/previous-symlink" 2>/dev/null || true

# 4. Inspect active native sessions before the restart ----------------------
BEFORE_UNITS=$(systemctl list-units --plain --no-legend 'rast-xpra-*' 'rast-app-*' 2>/dev/null | awk '{print $1}' | sort)
say "active native units before deploy: $(echo "$BEFORE_UNITS" | grep -c . || true)"

rollback() {
    local reason=$1
    warn "rolling back: $reason"
    if [ -f "$ROLLBACK_DIR/previous-binary" ]; then
        install -m 0755 "$ROLLBACK_DIR/previous-binary" "$BIN_DIR/rast"
        warn "previous binary restored from $ROLLBACK_DIR/previous-binary"
    fi
    if [ -f "$ROLLBACK_DIR/rast.toml" ]; then
        cp -a "$ROLLBACK_DIR/rast.toml" /etc/rast/rast.toml
        warn "previous configuration restored"
    fi
    systemctl restart rast-native.service rast-worker.service rast.service || warn "service restart during rollback failed"
    die "deployment rolled back: $reason"
}

# 5. Install a versioned binary + atomic symlink swap -----------------------
RELEASE_NAME="rast-$SOURCE_SHA"
RELEASE_PATH="$RELEASES/$RELEASE_NAME"
RUNNING_TARGET=$(readlink -f /proc/$(systemctl show rast-native.service -p MainPID --value)/exe 2>/dev/null || true)
say "installing $RELEASE_PATH behind $BIN (current binary is kept for running processes)"
run mkdir -p "$RELEASES"
run install -m 0755 -o root -g root "$RELEASE_BIN" "$RELEASE_PATH"
run ln -sfn "$RELEASE_PATH" "$BIN_DIR/.rast-new"
if [ "$DRY_RUN" = 0 ]; then
    ln -sfn "$RELEASE_PATH" "$BIN_DIR/rast"
else
    echo "[dry-run] ln -sfn $RELEASE_PATH $BIN"
fi
[ "$DRY_RUN" = 1 ] || [ "$(readlink -f "$BIN")" = "$RELEASE_PATH" ] || rollback "the binary symlink does not point at the new release"
say "installed symlink: $BIN -> $(readlink -f "$BIN")"

# 6. Restart the three services together ------------------------------------
say "restarting rast-native.service, rast-worker.service, rast.service"
run systemctl restart rast-native.service
run systemctl restart rast-worker.service
run systemctl restart rast.service
for _ in $(seq 1 60); do
    [ "$DRY_RUN" = 1 ] && break
    curl -fsS --max-time 2 http://127.0.0.1:8720/health >/dev/null 2>&1 && break
    sleep 0.5
done

# 7. Post-deploy verification ----------------------------------------------
[ "$DRY_RUN" = 1 ] && { say "dry run complete"; exit 0; }
HEALTH=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:8720/health)
[ "$HEALTH" = "200" ] || rollback "backend health is $HEALTH after deploy"
for svc in rast.service rast-worker.service rast-native.service; do
    STATE=$(systemctl is-active "$svc")
    [ "$STATE" = "active" ] || rollback "$svc is $STATE after deploy"
done
say "backend health: 200; services active"
/usr/local/bin/rast native-helper --check >/dev/null 2>&1 || rollback "rast native-helper --check failed"
say "native helper self-check: ok"
if [ -n "$RUNNING_TARGET" ]; then
    NEW_HELPER_BIN=$(readlink -f "/proc/$(systemctl show rast-native.service -p MainPID --value)/exe" 2>/dev/null || true)
    [ "$NEW_HELPER_BIN" = "$RELEASE_PATH" ] || warn "helper executable is $NEW_HELPER_BIN (expected $RELEASE_PATH)"
fi
AFTER_UNITS=$(systemctl list-units --plain --no-legend 'rast-xpra-*' 'rast-app-*' 2>/dev/null | awk '{print $1}' | sort)
if [ "$BEFORE_UNITS" = "$AFTER_UNITS" ]; then
    say "existing native sessions preserved ($(echo "$AFTER_UNITS" | grep -c . || true) units)"
else
    warn "native session units changed across the restart (before: $(echo "$BEFORE_UNITS" | tr '\n' ' '); after: $(echo "$AFTER_UNITS" | tr '\n' ' '))"
fi
DOCTOR=$(/usr/local/bin/rast doctor 2>&1 || true)
echo "$DOCTOR" | grep -q 'doctor: ok' || warn "rast doctor did not report ok"

# 8. Terminal smoke (the regression this script exists for) -----------------
if [ "$SMOKE_TERMINAL" = 1 ]; then
    SMOKE_USER=${RAST_DEPLOY_SMOKE_USER:-t-admin}
    if [ ! -r "${RAST_DEPLOY_SMOKE_CREDS:-/home/lufria/.config/rast-test/creds}" ]; then
        warn "no smoke credentials file; skipping Terminal smoke"
    elif ! command -v node >/dev/null; then
        warn "node not found; skipping Terminal smoke"
    else
        say "running Terminal smoke as $SMOKE_USER"
        # A just-restarted backend can be slow to serve the first session/terminal
        # handshake; retry once after a short pause before treating it as a real
        # regression (the rollback path is still exercised on repeated failure).
        SMOKE_OK=0
        for attempt in 1 2; do
            if (cd "$ROOT/tests/e2e" && node terminal/smoke.mjs --user="$SMOKE_USER"); then
                SMOKE_OK=1
                break
            fi
            if [ "$attempt" = 1 ]; then
                warn "Terminal smoke failed (attempt 1); retrying in 15 s"
                sleep 15
            fi
        done
        if [ "$SMOKE_OK" = 1 ]; then
            say "Terminal smoke: PASS"
        else
            rollback "Terminal smoke failed after deploy (2 attempts)"
        fi
    fi
fi

# 9. Keep a bounded number of old releases ----------------------------------
if [ -d "$RELEASES" ]; then
    # shellcheck disable=SC2012
    OLD=$(ls -1t "$RELEASES" 2>/dev/null | tail -n +$((KEEP + 1)) || true)
    for name in $OLD; do
        [ "$RELEASES/$name" = "$(readlink -f "$BIN")" ] && continue
        if [ -n "$RUNNING_TARGET" ] && [ "$RELEASES/$name" = "$RUNNING_TARGET" ]; then continue; fi
        rm -f "$RELEASES/$name"
    done
fi

say "deployment complete"
say "  source revision : $SOURCE_SHA"
say "  binary sha256   : $BIN_SHA"
say "  installed       : $BIN -> $(readlink -f "$BIN")"
say "  rollback data   : $ROLLBACK_DIR"
say "  rollback        : install -m 0755 $ROLLBACK_DIR/previous-binary $BIN && systemctl restart rast-native.service rast-worker.service rast.service"
