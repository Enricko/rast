#!/usr/bin/env bash
# Clean-environment installer test for RAST.
#
# Builds a local release-like artifact set (frontend + release binary + installer
# assets), then runs `installer/install.sh` inside a throwaway systemd container
# and verifies the installed result end to end:
#
#   * accounts, directories, modes and ownership;
#   * installed binary + version, config file + permissions, systemd units;
#   * service enabled/active, /healthz, admin creation, login API, CSRF;
#   * idempotent reinstall preserves configuration and data;
#   * uninstaller preserves data by default, then reinstalls cleanly;
#   * `--purge` removes all data.
#
# Nothing is ever installed on the host: the script refuses --local, and all
# mutations happen inside a disposable container that is removed on exit.
#
# Usage:
#   scripts/test-installer-clean.sh                 # build + test (systemd container)
#   scripts/test-installer-clean.sh --skip-build --artifacts dist
#   GH_TOKEN=$(gh auth token) scripts/test-installer-clean.sh --verify-attestation
#   scripts/test-installer-clean.sh --verify-attestation --gh-bin /path/to/gh
#
# --verify-attestation copies the caller's gh CLI (>= 2.49) into the container
# and makes the installer verify GitHub/Sigstore build provenance. GitHub
# requires authentication for the attestation API even on public repositories,
# so export GH_TOKEN (e.g. `GH_TOKEN=$(gh auth token)`).
#   scripts/test-installer-clean.sh --mode stub     # fallback when systemd-in-container is unavailable
#   scripts/test-installer-clean.sh --keep-container
#
# Requirements: Docker (usable directly or via passwordless sudo), ~4 GB free
# disk, network access to pull the official Ubuntu base image on first run.
#
# Exit status: 0 when every check passed, 1 when any check failed.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)

VERSION=""
MODE="auto"
IMAGE="rast-installer-test:systemd"
REBUILD_IMAGE=0
SKIP_BUILD=0
SKIP_FRONTEND=0
ARTIFACTS=""
KEEP_CONTAINER=0
BOOT_TIMEOUT=120
VERIFY_ATTESTATION=0
GH_BIN_CLI=""
VERBOSE=0
LOCAL=0
ALLOW_HOST=0
EVIDENCE=""

CONTAINER=""
WORK=""
RELEASES=""
EFFECTIVE_MODE=""
CONTAINER_RELEASES=""
DOCKER=()
PASS=0
FAIL=0
SKIP=0

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
log() { printf '[clean-test] %s\n' "$*"; }

record() {
    if [[ -n "$EVIDENCE" ]]; then
        printf '%s\n' "$*" >>"$EVIDENCE"
    fi
    printf '%s\n' "$*"
}

pass() {
    PASS=$((PASS + 1))
    record "PASS  $1"
}

fail() {
    FAIL=$((FAIL + 1))
    record "FAIL  $1"
    if [[ -n "${2:-}" ]]; then
        record "      $2"
    fi
}

skip() {
    SKIP=$((SKIP + 1))
    record "SKIP  $1"
}

die() {
    record "ERROR $*"
    exit 1
}

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

cleanup() {
    local rc=$?
    if [[ -n "$CONTAINER" ]]; then
        if [[ "$rc" -ne 0 || "$VERBOSE" -eq 1 ]]; then
            log "collecting container diagnostics from $CONTAINER"
            "${DOCKER[@]}" logs "$CONTAINER" >"$WORK/container.log" 2>&1 || true
            if [[ -f "$WORK/container.log" ]]; then
                record "--- container logs (tail) ---"
                tail -n 30 "$WORK/container.log" >>"$EVIDENCE" || true
            fi
            if [[ "$EFFECTIVE_MODE" == "systemd" ]]; then
                "${DOCKER[@]}" exec "$CONTAINER" journalctl -u rast.service --no-pager -n 80 \
                    >"$WORK/rast-journal.log" 2>&1 || true
                if [[ -s "$WORK/rast-journal.log" ]]; then
                    record "--- rast.service journal (tail) ---"
                    tail -n 40 "$WORK/rast-journal.log" >>"$EVIDENCE" || true
                fi
            else
                "${DOCKER[@]}" exec "$CONTAINER" sh -c 'cat /var/log/rast-fake.log 2>/dev/null; echo "--- fake-systemctl ---"; cat /var/log/fake-systemctl.log 2>/dev/null' \
                    >"$WORK/rast-fallback.log" 2>&1 || true
                if [[ -s "$WORK/rast-fallback.log" ]]; then
                    record "--- fallback service/systemctl log (tail) ---"
                    tail -n 40 "$WORK/rast-fallback.log" >>"$EVIDENCE" || true
                fi
            fi
        fi
        if [[ "$KEEP_CONTAINER" -eq 1 ]]; then
            record "container kept for inspection: ${DOCKER[*]} exec -it $CONTAINER sh"
        else
            "${DOCKER[@]}" rm -f "$CONTAINER" >/dev/null 2>&1 || true
        fi
    fi
    if [[ -n "$WORK" && -d "$WORK" ]]; then
        rm -rf -- "$WORK"
    fi
    exit "$rc"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        --skip-frontend) SKIP_FRONTEND=1 ;;
        --artifacts)
            shift
            [[ $# -gt 0 ]] || die "--artifacts requires a directory"
            ARTIFACTS=$1
            ;;
        --artifacts=*) ARTIFACTS=${1#*=} ;;
        --version)
            shift
            [[ $# -gt 0 ]] || die "--version requires a value"
            VERSION=${1#v}
            ;;
        --version=*) VERSION=${1#*=}; VERSION=${VERSION#v} ;;
        --image)
            shift
            [[ $# -gt 0 ]] || die "--image requires a value"
            IMAGE=$1
            ;;
        --image=*) IMAGE=${1#*=} ;;
        --mode)
            shift
            [[ $# -gt 0 ]] || die "--mode requires auto, systemd or stub"
            MODE=$1
            ;;
        --mode=*) MODE=${1#*=} ;;
        --rebuild-image) REBUILD_IMAGE=1 ;;
        --keep-container) KEEP_CONTAINER=1 ;;
        --verify-attestation) VERIFY_ATTESTATION=1 ;;
        --gh-bin)
            shift
            [[ $# -gt 0 ]] || die "--gh-bin requires a path to a gh executable"
            GH_BIN_CLI=$1
            ;;
        --gh-bin=*) GH_BIN_CLI=${1#*=} ;;
        --verbose) VERBOSE=1 ;;
        --timeout)
            shift
            [[ $# -gt 0 ]] || die "--timeout requires seconds"
            BOOT_TIMEOUT=$1
            ;;
        --timeout=*) BOOT_TIMEOUT=${1#*=} ;;
        --evidence)
            shift
            [[ $# -gt 0 ]] || die "--evidence requires a file"
            EVIDENCE=$1
            ;;
        --evidence=*) EVIDENCE=${1#*=} ;;
        --local) LOCAL=1 ;;
        --allow-host) ALLOW_HOST=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (try --help)"
            ;;
    esac
    shift
done

case "$MODE" in
    auto | systemd | stub) ;;
    *) die "invalid --mode '$MODE' (expected auto, systemd or stub)" ;;
esac
if [[ ! "$BOOT_TIMEOUT" =~ ^[0-9]+$ ]] || [[ "$BOOT_TIMEOUT" -lt 10 ]]; then
    die "--timeout must be an integer >= 10 (got '$BOOT_TIMEOUT')"
fi
if [[ "$LOCAL" -eq 1 ]]; then
    if [[ "$ALLOW_HOST" -eq 1 ]]; then
        die "--local is not supported: this harness never installs on the host.
Expected use is a disposable container; to test on a real machine, run
installer/install.sh there deliberately (see docs/INSTALL.md)."
    fi
    die "--local requires --allow-host, and host installation is intentionally
unsupported by this harness; use the container test instead."
fi
if [[ -n "$VERSION" ]] && ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
    die "invalid --version '$VERSION' (expected X.Y.Z)"
fi
if [[ "$SKIP_BUILD" -eq 1 && -z "$ARTIFACTS" ]]; then
    if [[ -d "$ROOT/dist" ]]; then
        ARTIFACTS="$ROOT/dist"
    else
        die "--skip-build requires --artifacts DIR (or a dist/ directory)"
    fi
fi

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"

# The harness is often started from a non-login shell (CI, sandboxes) where
# rustup's shims are not on PATH; locate the toolchain before building.
if ! command -v cargo >/dev/null 2>&1; then
    for _cargo_dir in "$HOME/.cargo/bin" /usr/local/cargo/bin; do
        if [[ -x "$_cargo_dir/cargo" ]]; then
            PATH="$_cargo_dir:$PATH"
            export PATH
            break
        fi
    done
fi

detect_docker() {
    if command -v docker >/dev/null 2>&1 && timeout 15 docker info >/dev/null 2>&1; then
        DOCKER=(docker)
        return 0
    fi
    if command -v sudo >/dev/null 2>&1 && command -v docker >/dev/null 2>&1 &&
        timeout 15 sudo -n docker info >/dev/null 2>&1; then
        DOCKER=(sudo -n docker)
        log "Docker access via 'sudo -n docker'"
        return 0
    fi
    die "Docker is not available (need a working docker CLI: docker info, or passwordless sudo).
The clean-environment test requires Docker; for the manual procedure see docs/INSTALL.md."
}

arch_asset() {
    case "$(uname -m)" in
        x86_64 | amd64) printf 'x86_64\n' ;;
        aarch64 | arm64) printf 'aarch64\n' ;;
        *) die "unsupported host architecture: $(uname -m)" ;;
    esac
}

# ---------------------------------------------------------------------------
# Image
# ---------------------------------------------------------------------------
build_image() {
    if "${DOCKER[@]}" image inspect "$IMAGE" >/dev/null 2>&1 && [[ "$REBUILD_IMAGE" -eq 0 ]]; then
        log "reusing Docker image $IMAGE"
        return 0
    fi
    if [[ "$IMAGE" != "rast-installer-test:systemd" ]]; then
        "${DOCKER[@]}" image inspect "$IMAGE" >/dev/null 2>&1 ||
            die "image $IMAGE is not present locally; pull/build it first or use the default test image"
        return 0
    fi
    log "building systemd test image $IMAGE (first run installs systemd via apt; needs network)"
    local dir="$WORK/image"
    mkdir -p "$dir"
    cat >"$dir/Dockerfile" <<'DOCKERFILE'
# Throwaway systemd image used by scripts/test-installer-clean.sh.
# Based on the official Ubuntu image; RAST itself is installed by the script.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive container=docker
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus curl ca-certificates procps iproute2 sudo \
 && rm -rf /var/lib/apt/lists/*
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
DOCKERFILE
    "${DOCKER[@]}" build -t "$IMAGE" "$dir" || die "docker build failed (see output above)"
}

# ---------------------------------------------------------------------------
# Artifacts
# ---------------------------------------------------------------------------
detect_version() {
    if [[ -n "$VERSION" ]]; then
        return 0
    fi
    VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
    [[ -n "$VERSION" ]] || die "cannot determine the version from Cargo.toml; pass --version"
}

stage_artifacts() {
    local asset
    asset="rast-linux-$(arch_asset)"
    RELEASES="$WORK/releases/v$VERSION"
    mkdir -p "$RELEASES"
    local binary="$1"
    cp -- "$binary" "$RELEASES/$asset"
    chmod 0755 "$RELEASES/$asset"
    cp -- "$ROOT/installer/install.sh" "$RELEASES/install.sh"
    cp -- "$ROOT/installer/uninstall.sh" "$RELEASES/uninstall.sh"
    cp -- "$ROOT/deploy/rast.toml.example" "$RELEASES/rast.toml.example"
    chmod 0755 "$RELEASES/install.sh" "$RELEASES/uninstall.sh"
    (
        cd "$RELEASES"
        sha256sum "$asset" install.sh uninstall.sh rast.toml.example >SHA256SUMS
    )
    log "staged release artifacts in $RELEASES"
}

build_artifacts() {
    detect_version
    log "building release artifacts for version $VERSION"
    command -v cargo >/dev/null 2>&1 || die "cargo not found (install the Rust toolchain pinned in rust-toolchain.toml)"
    command -v npm >/dev/null 2>&1 || die "npm not found (Node.js >= 22 is required for the frontend build)"

    if [[ "$SKIP_FRONTEND" -eq 1 ]]; then
        [[ -f "$ROOT/frontend/dist/index.html" ]] ||
            die "--skip-frontend requires an existing frontend/dist build"
        log "reusing existing frontend/dist (--skip-frontend)"
    else
        (
            cd "$ROOT/frontend"
            if [[ ! -d node_modules ]]; then
                log "installing frontend dependencies with npm ci"
                npm ci
            fi
            log "building frontend with npm run build"
            npm run build
        )
    fi
    [[ -f "$ROOT/frontend/dist/index.html" ]] || die "frontend/dist/index.html is missing after the build"

    log "building release binary with cargo build --release --locked"
    (
        cd "$ROOT"
        cargo build --release --locked
    )
    [[ -x "$ROOT/target/release/rast" ]] || die "target/release/rast was not produced"
    stage_artifacts "$ROOT/target/release/rast"
}

stage_from_artifacts() {
    detect_version
    local src="$ARTIFACTS"
    [[ -d "$src" ]] || die "artifacts directory not found: $src"
    src=$(cd -- "$src" && pwd)
    local asset
    asset="rast-linux-$(arch_asset)"
    local required=("$asset" install.sh uninstall.sh SHA256SUMS)
    local f
    for f in "${required[@]}"; do
        [[ -f "$src/$f" ]] || die "missing required artifact: $src/$f"
    done
    RELEASES="$WORK/releases/v$VERSION"
    mkdir -p "$RELEASES"
    cp -a -- "$src/$asset" "$src/install.sh" "$src/uninstall.sh" "$RELEASES/"
    if [[ -f "$src/rast.toml.example" ]]; then
        cp -a -- "$src/rast.toml.example" "$RELEASES/"
    else
        cp -a -- "$ROOT/deploy/rast.toml.example" "$RELEASES/"
    fi
    chmod 0755 "$RELEASES/install.sh" "$RELEASES/uninstall.sh" "$RELEASES/$asset"
    (
        cd "$RELEASES"
        sha256sum "$asset" install.sh uninstall.sh rast.toml.example >SHA256SUMS
    )
    log "staged release artifacts from $src into $RELEASES"
}

# ---------------------------------------------------------------------------
# Container lifecycle
# ---------------------------------------------------------------------------
container_running() {
    [[ "$("${DOCKER[@]}" inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" == "true" ]]
}

wait_container_alive() {
    local waited=0
    while [[ "$waited" -lt 30 ]]; do
        if container_running; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

start_systemd_container() {
    log "starting systemd container (privileged, cgroupns=host, /releases read-only)"
    "${DOCKER[@]}" run -d --name "$CONTAINER" \
        --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        --tmpfs /run --tmpfs /run/lock --tmpfs /tmp:rw,exec,nosuid,nodev \
        -v "$WORK/releases":/releases:ro \
        --stop-signal=SIGRTMIN+3 \
        "$IMAGE" /sbin/init >/dev/null ||
        return 1

    local deadline=$((SECONDS + BOOT_TIMEOUT))
    local state=""
    while [[ "$SECONDS" -lt "$deadline" ]]; do
        if ! container_running; then
            log "container exited during boot:"
            "${DOCKER[@]}" logs "$CONTAINER" 2>&1 | tail -n 20 || true
            return 1
        fi
        state=$("${DOCKER[@]}" exec "$CONTAINER" systemctl is-system-running 2>/dev/null || true)
        case "$state" in
            running | degraded)
                log "systemd is $state (boot took $((SECONDS - (deadline - BOOT_TIMEOUT)))s)"
                return 0
                ;;
        esac
        sleep 1
    done
    log "systemd did not become ready within ${BOOT_TIMEOUT}s (last state: '${state:-unknown}')"
    return 1
}

start_stub_container() {
    log "starting fallback container with a stubbed systemctl (no systemd boot)"
    "${DOCKER[@]}" run -d --name "$CONTAINER" \
        --tmpfs /run --tmpfs /tmp:rw,exec,nosuid,nodev \
        -v "$WORK/releases":/releases:ro \
        "$IMAGE" sleep infinity >/dev/null ||
        return 1
    wait_container_alive || return 1

    # shellcheck disable=SC2016 # the stub must be written literally
    "${DOCKER[@]}" exec "$CONTAINER" sh -c '
        set -e
        mkdir -p /run/systemd/system /var/log
        cat > /usr/local/bin/rast-fake-systemctl <<"STUB"
#!/bin/sh
# Minimal systemctl stand-in for the container fallback test. It records calls
# and can start/stop the real RAST server so that the installer health check,
# login API and reinstall/uninstall flows can still be exercised.
log=/var/log/fake-systemctl.log
echo "systemctl $*" >>"$log" 2>/dev/null || true
unit=""
for arg in "$@"; do
    case "$arg" in
        *.service) unit="$arg" ;;
    esac
done
verb=${1:-}
case "$verb" in
    is-active)
        if [ -f /run/fake-rast.pid ] && kill -0 "$(cat /run/fake-rast.pid)" 2>/dev/null; then
            exit 0
        fi
        exit 3
        ;;
    is-enabled)
        [ -f /etc/systemd/system/rast.service ] && exit 0
        exit 1
        ;;
    start | restart)
        if [ "$unit" = "rast.service" ]; then
            pkill -u rast -x rast >/dev/null 2>&1 || true
            rm -f /run/fake-rast.pid
            sleep 0.2
            sudo -u rast /usr/local/bin/rast serve >/var/log/rast-fake.log 2>&1 &
            echo $! >/run/fake-rast.pid
            sleep 1
        fi
        exit 0
        ;;
    stop | disable)
        pkill -u rast -x rast >/dev/null 2>&1 || true
        rm -f /run/fake-rast.pid
        exit 0
        ;;
    *)
        exit 0
        ;;
esac
STUB
        chmod 0755 /usr/local/bin/rast-fake-systemctl
        mv /usr/local/bin/rast-fake-systemctl /usr/bin/systemctl
        # A no-op wrapper used by the uninstaller/service management paths.
        touch /var/log/fake-systemctl.log
    '
    log "stub systemctl installed (fallback mode records calls to /var/log/fake-systemctl.log)"
}

start_container() {
    CONTAINER="rast-installer-clean-$$-$(date +%s)"
    EFFECTIVE_MODE=$MODE
    if [[ "$MODE" == "systemd" ]]; then
        start_systemd_container || die "could not start a systemd-enabled container (see log above)"
        return 0
    fi
    if [[ "$MODE" == "stub" ]]; then
        start_stub_container || die "could not start the fallback container"
        return 0
    fi
    # auto: try systemd first, fall back to the stubbed install verification.
    if start_systemd_container; then
        EFFECTIVE_MODE="systemd"
        return 0
    fi
    log "systemd-in-container unavailable; falling back to stubbed-systemctl verification"
    "${DOCKER[@]}" rm -f "$CONTAINER" >/dev/null 2>&1 || true
    CONTAINER="rast-installer-clean-$$-$(date +%s)"
    EFFECTIVE_MODE="stub"
    start_stub_container || die "could not start the fallback container"
}

# ---------------------------------------------------------------------------
# Command helpers
# ---------------------------------------------------------------------------
cexec() {
    "${DOCKER[@]}" exec "$CONTAINER" "$@"
}

check() {
    local desc=$1
    shift
    if "$@" >"$WORK/check.out" 2>&1; then
        pass "$desc"
    else
        fail "$desc"
        if [[ "$VERBOSE" -eq 1 || "$FAIL" -le 5 ]]; then
            sed -e 's/^/      /' "$WORK/check.out" | head -n 20 >>"$EVIDENCE" || true
        fi
    fi
}

check_sh() {
    local desc=$1
    local script=$2
    check "$desc" cexec sh -c "$script"
}

capture_sh() {
    local script=$1
    cexec sh -c "$script" 2>/dev/null | tr -d '\r' || true
}

check_value() {
    local desc=$1
    local expected=$2
    local actual=$3
    actual=$(printf '%s' "$actual" | tr -d '\r' | head -n 1)
    if [[ "$expected" == "$actual" ]]; then
        pass "$desc"
    else
        fail "$desc" "expected '$expected', got '$actual'"
    fi
}

check_contains() {
    local desc=$1
    local expected=$2
    local haystack=$3
    if printf '%s' "$haystack" | grep -qF -- "$expected"; then
        pass "$desc"
    else
        fail "$desc" "expected to find '$expected'"
    fi
}

run_installer() {
    local outfile=$1
    local args=(--version "$VERSION")
    local exec_cmd=("${DOCKER[@]}" exec)
    if [[ "$VERIFY_ATTESTATION" -eq 1 ]]; then
        args+=(--verify-attestation)
        # `gh attestation verify` needs authentication even for public
        # releases; pass the caller's GH_TOKEN into the container if present.
        if [[ -n "${GH_TOKEN:-}" ]]; then
            exec_cmd+=(-e "GH_TOKEN=${GH_TOKEN}")
        fi
    fi
    exec_cmd+=("$CONTAINER" env RAST_RELEASE_BASE_URL=file:///releases RAST_VERSION="$VERSION" \
        sh "$CONTAINER_RELEASES/install.sh" "${args[@]}")
    "${exec_cmd[@]}" >"$outfile" 2>&1
}

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------
common_checks() {
    record "--- static installation checks ---"

    check_sh "system user rast exists" 'id -u rast >/dev/null 2>&1'
    check_value "rast primary group is rast" "rast" "$(capture_sh 'id -gn rast')"
    check_value "rast login shell is nologin" "/usr/sbin/nologin" "$(capture_sh 'getent passwd rast | cut -d: -f7')"
    check_value "rast home directory is /nonexistent" "/nonexistent" "$(capture_sh 'getent passwd rast | cut -d: -f6')"
    check_sh "group rast exists" 'getent group rast >/dev/null'
    check_sh "worker account not created (no Docker present)" '! id rast-worker >/dev/null 2>&1'

    check_value "binary mode/owner" "755 root:root" "$(capture_sh 'stat -c "%a %U:%G" /usr/local/bin/rast')"
    local version_out
    version_out=$(capture_sh '/usr/local/bin/rast version')
    check_contains "installed version is $VERSION" "RAST $VERSION" "$version_out"

    check_value "config mode/owner" "640 root:rast" "$(capture_sh 'stat -c "%a %U:%G" /etc/rast/rast.toml')"
    check_contains "config bind address" 'bind = "127.0.0.1:8720"' "$(capture_sh 'cat /etc/rast/rast.toml')"
    check_contains "config docker_enabled=false" "docker_enabled = false" "$(capture_sh 'cat /etc/rast/rast.toml')"
    check_value "data dir mode/owner" "750 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /var/lib/rast')"
    check_value "storage dir mode/owner" "755 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /srv/rast')"
    check_value "storage homes mode/owner" "755 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /srv/rast/homes')"
    check_value "storage shared mode/owner" "755 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /srv/rast/shared')"
    check_value "trash dir mode/owner" "700 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /srv/rast/.trash')"
    if [[ "$EFFECTIVE_MODE" == "stub" ]]; then
        # Fallback mode starts the server outside systemd (no UMask=0027).
        check_value "database owner" "rast:rast" "$(capture_sh 'stat -c "%U:%G" /var/lib/rast/rast.db')"
    else
        check_value "database mode/owner" "640 rast:rast" "$(capture_sh 'stat -c "%a %U:%G" /var/lib/rast/rast.db')"
    fi
    check_value "secret.key mode" "600" "$(capture_sh 'stat -c "%a" /var/lib/rast/secret.key')"

    check_value "service unit mode/owner" "644 root:root" "$(capture_sh 'stat -c "%a %U:%G" /etc/systemd/system/rast.service')"
    check_sh "worker unit not installed (no Docker present)" 'test ! -f /etc/systemd/system/rast-worker.service'
    check_contains "unit starts rast serve" "ExecStart=/usr/local/bin/rast serve" "$(capture_sh 'cat /etc/systemd/system/rast.service')"

    check_contains "healthz returns ok" "ok" "$(capture_sh 'curl -fsS --max-time 5 http://127.0.0.1:8720/healthz')"
    check_contains "status --json reports version" "\"version\": \"$VERSION\"" "$(capture_sh 'sudo -u rast /usr/local/bin/rast status --json')"
}

systemd_checks() {
    record "--- systemd checks ---"
    check_value "rast.service is enabled" "enabled" "$(capture_sh 'systemctl is-enabled rast.service')"
    check_value "rast.service is active" "active" "$(capture_sh 'systemctl is-active rast.service')"
    check_sh "enable symlink exists" 'test -L /etc/systemd/system/multi-user.target.wants/rast.service'
    local main_pid
    main_pid=$(capture_sh 'systemctl show -p MainPID --value rast.service')
    if [[ "${main_pid:-0}" =~ ^[0-9]+$ && "${main_pid:-0}" -gt 0 ]]; then
        pass "rast.service has a main process (pid $main_pid)"
    else
        fail "rast.service has a main process" "MainPID='${main_pid:-empty}'"
    fi
}

stub_checks() {
    record "--- fallback (stubbed systemctl) checks ---"
    local log_text
    log_text=$(capture_sh 'cat /var/log/fake-systemctl.log')
    check_contains "installer called systemctl daemon-reload" "daemon-reload" "$log_text"
    check_contains "installer called systemctl enable rast.service" "enable rast.service" "$log_text"
    check_contains "installer called systemctl start rast.service" "start rast.service" "$log_text"
    check_sh "fallback RAST process is running as rast" 'pgrep -u rast -x rast >/dev/null'
}

login_checks() {
    record "--- account and HTTP login checks ---"
    local password="CleanTestPassw0rd123"
    check_sh "admin account created via CLI" \
        "printf '%s\n' '$password' | sudo -u rast /usr/local/bin/rast user create-admin admin --password-stdin"

    local login_code
    login_code=$(capture_sh "curl -sS -o /tmp/login.json -w '%{http_code}' -c /tmp/rast-cookies \
        -H 'Content-Type: application/json' \
        -d '{\"username\":\"admin\",\"password\":\"$password\"}' \
        http://127.0.0.1:8720/api/auth/login")
    check_value "POST /api/auth/login returns 200" "200" "$login_code"
    check_contains "login response contains csrfToken" '"csrfToken"' "$(capture_sh 'cat /tmp/login.json')"
    check_contains "login response contains admin user" '"username":"admin"' "$(capture_sh 'cat /tmp/login.json')"
    check_contains "GET /api/auth/me works with session cookie" '"username":"admin"' \
        "$(capture_sh 'curl -fsS -b /tmp/rast-cookies http://127.0.0.1:8720/api/auth/me')"
    check_value "mutation without CSRF token is rejected" "403" \
        "$(capture_sh "curl -sS -o /dev/null -w '%{http_code}' -b /tmp/rast-cookies -X POST http://127.0.0.1:8720/api/auth/logout")"

    local csrf
    csrf=$(capture_sh "sed -n 's/.*\"csrfToken\":\"\\([^\"]*\\)\".*/\\1/p' /tmp/login.json | head -n 1")
    if [[ -n "$csrf" ]]; then
        pass "CSRF token extracted from login response"
        check_sh "mutation with CSRF token succeeds" \
            "curl -sS -o /dev/null -w '%{http_code}' -b /tmp/rast-cookies -H 'X-CSRF-Token: $csrf' -X POST http://127.0.0.1:8720/api/auth/logout | grep -Eq '^(200|204)$'"
    else
        fail "CSRF token extracted from login response"
    fi
}

idempotent_reinstall_checks() {
    record "--- idempotent reinstall checks ---"
    check_sh "write marker files" \
        'echo keep > /var/lib/rast/installer-test-marker && chown rast:rast /var/lib/rast/installer-test-marker && echo keep > /etc/rast/installer-test-marker'
    local config_sum_before
    config_sum_before=$(capture_sh 'sha256sum /etc/rast/rast.toml | cut -d" " -f1')

    local out="$WORK/reinstall.log"
    if run_installer "$out"; then
        local reinstall_output
        reinstall_output=$(cat "$out")
        pass "reinstall exits 0"
        check_contains "reinstall preserves existing configuration" "preserving existing configuration" "$reinstall_output"
    else
        fail "reinstall exits 0" "see $out"
        sed -e 's/^/      /' "$out" | tail -n 30 >>"$EVIDENCE" || true
    fi

    check_sh "reinstall kept /var/lib/rast marker" 'test -f /var/lib/rast/installer-test-marker'
    check_sh "reinstall kept /etc/rast marker" 'test -f /etc/rast/installer-test-marker'
    local config_sum_after
    config_sum_after=$(capture_sh 'sha256sum /etc/rast/rast.toml | cut -d" " -f1')
    check_value "reinstall did not rewrite rast.toml" "$config_sum_before" "$config_sum_after"
    check_contains "healthz ok after reinstall" "ok" "$(capture_sh 'curl -fsS --max-time 5 http://127.0.0.1:8720/healthz')"
    check_contains "version unchanged after reinstall" "RAST $VERSION" "$(capture_sh '/usr/local/bin/rast version')"
    check_sh "database preserved after reinstall" 'test -s /var/lib/rast/rast.db'
}

uninstall_checks() {
    record "--- uninstall (default: keep data) checks ---"
    local out="$WORK/uninstall.log"
    if cexec sh "$CONTAINER_RELEASES/uninstall.sh" >"$out" 2>&1; then
        pass "uninstaller exits 0"
    else
        fail "uninstaller exits 0" "see $out"
        sed -e 's/^/      /' "$out" | tail -n 30 >>"$EVIDENCE" || true
    fi
    check_sh "binary removed" 'test ! -e /usr/local/bin/rast'
    check_sh "rast.service unit removed" 'test ! -e /etc/systemd/system/rast.service'
    check_sh "configuration kept" 'test -f /etc/rast/rast.toml'
    check_sh "data directory kept" 'test -f /var/lib/rast/installer-test-marker'
    check_sh "storage directory kept" 'test -d /srv/rast'
    check_sh "service accounts kept by default" 'id rast >/dev/null 2>&1'
    check_sh "service is no longer running" '! curl -fsS --max-time 2 http://127.0.0.1:8720/healthz >/dev/null 2>&1'

    record "--- reinstall over preserved data ---"
    local reinstall_log="$WORK/reinstall-after-uninstall.log"
    if run_installer "$reinstall_log"; then
        pass "reinstall after uninstall exits 0"
    else
        fail "reinstall after uninstall exits 0" "see $reinstall_log"
        sed -e 's/^/      /' "$reinstall_log" | tail -n 30 >>"$EVIDENCE" || true
    fi
    check_contains "healthz ok after reinstall over preserved data" "ok" \
        "$(capture_sh 'curl -fsS --max-time 5 http://127.0.0.1:8720/healthz')"
    check_contains "existing admin still works after reinstall" '"username":"admin"' \
        "$(capture_sh "curl -sS -o /tmp/login2.json -w '%{http_code}' -c /tmp/rast-cookies2 \
            -H 'Content-Type: application/json' \
            -d '{\"username\":\"admin\",\"password\":\"CleanTestPassw0rd123\"}' \
            http://127.0.0.1:8720/api/auth/login >/dev/null; cat /tmp/login2.json")"
    check_sh "config marker survived reinstall" 'test -f /etc/rast/installer-test-marker'

    record "--- purge checks ---"
    local purge_log="$WORK/purge.log"
    if printf 'PURGE\n' | "${DOCKER[@]}" exec -i "$CONTAINER" sh "$CONTAINER_RELEASES/uninstall.sh" --purge >"$purge_log" 2>&1; then
        pass "uninstaller --purge exits 0 (typed confirmation accepted)"
    else
        fail "uninstaller --purge exits 0" "see $purge_log"
        sed -e 's/^/      /' "$purge_log" | tail -n 30 >>"$EVIDENCE" || true
    fi
    check_sh "/etc/rast removed by --purge" 'test ! -e /etc/rast'
    check_sh "data directory removed by --purge" 'test ! -e /var/lib/rast'
    check_sh "storage directory removed by --purge" 'test ! -e /srv/rast'
    check_sh "binary removed by --purge" 'test ! -e /usr/local/bin/rast'
    check_sh "service unit removed by --purge" 'test ! -e /etc/systemd/system/rast.service'
    check_sh "no RAST process left running" '! pgrep -x rast >/dev/null 2>&1'
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
if [[ -z "$EVIDENCE" ]]; then
    EVIDENCE=$(mktemp "${TMPDIR:-/tmp}/rast-installer-clean-evidence.XXXXXX.log")
fi
: >"$EVIDENCE"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/rast-installer-clean.XXXXXX")

detect_docker
detect_version
CONTAINER_RELEASES="/releases/v$VERSION"

record "RAST clean-environment installer test"
record "date:        $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
record "host:        $(uname -srm)"
record "version:     $VERSION"
record "mode:        $MODE (image: $IMAGE)"
record "docker:      $("${DOCKER[@]}" version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)"
record "evidence:    $EVIDENCE"
record ""

log "evidence file: $EVIDENCE"
build_image
if [[ "$SKIP_BUILD" -eq 1 ]]; then
    stage_from_artifacts
else
    build_artifacts
fi
start_container
record "container:   $CONTAINER (effective mode: $EFFECTIVE_MODE)"
record ""

if [[ "$VERIFY_ATTESTATION" -eq 1 ]]; then
    # The installer verifies GitHub/Sigstore provenance with `gh attestation
    # verify`, so the container needs a gh CLI (>= 2.49) and outbound network.
    if [[ -z "$GH_BIN_CLI" ]]; then
        GH_BIN_CLI=$(command -v gh || true)
    fi
    if [[ -z "$GH_BIN_CLI" || ! -x "$GH_BIN_CLI" ]]; then
        die "--verify-attestation requires a GitHub CLI executable; install gh >= 2.49 or pass --gh-bin PATH"
    fi
    if ! "$GH_BIN_CLI" attestation verify --help >/dev/null 2>&1; then
        die "the gh CLI at $GH_BIN_CLI does not support 'gh attestation verify' (requires GitHub CLI 2.49+)"
    fi
    log "copying gh CLI ($("$GH_BIN_CLI" --version 2>/dev/null | head -n 1)) into the container"
    "${DOCKER[@]}" cp "$GH_BIN_CLI" "$CONTAINER:/usr/local/bin/gh" ||
        die "could not copy the gh CLI into the container"
    cexec chmod 0755 /usr/local/bin/gh
    if [[ -n "${GH_TOKEN:-}" ]]; then
        record "attestation: enabled (gh copied into container; GH_TOKEN passed for GitHub API auth)"
    else
        record "attestation: enabled (gh copied into container; no GH_TOKEN - gh may require 'gh auth login')"
    fi
fi

record "--- installer run (first install) ---"
install_log="$WORK/install.log"
if run_installer "$install_log"; then
    pass "installer exits 0"
    sed -e 's/^/      /' "$install_log" | tail -n 12 >>"$EVIDENCE" || true
    if [[ "$VERIFY_ATTESTATION" -eq 1 ]]; then
        check_contains "installer verified build provenance attestation" \
            "build-provenance attestation verified" "$(cat "$install_log")"
    fi
else
    fail "installer exits 0"
    sed -e 's/^/      /' "$install_log" | tail -n 60 >>"$EVIDENCE" || true
    die "first install failed; see the installer output above (container diagnostics follow)"
fi

common_checks
if [[ "$EFFECTIVE_MODE" == "systemd" ]]; then
    systemd_checks
else
    skip "systemd native enable/active checks (fallback mode)"
    stub_checks
fi
login_checks
idempotent_reinstall_checks
uninstall_checks

record ""
record "=== summary: ${PASS} passed, ${FAIL} failed, ${SKIP} skipped ==="
record "container:   $CONTAINER"
record "image:       $IMAGE"
record "version:     $VERSION"
record "mode:        $EFFECTIVE_MODE"
record "evidence:    $EVIDENCE"

if [[ "$FAIL" -gt 0 ]]; then
    log "RESULT: FAILED (${FAIL} check(s) failed)"
    exit 1
fi
log "RESULT: PASSED (${PASS} checks, ${SKIP} skipped)"
