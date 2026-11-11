#!/bin/sh
# RAST installer — Resource Administration & Storage Toolkit
#
# POSIX sh script designed for:
#   curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh | sudo sh
#
# It installs a prebuilt, checksum-verified `rast` binary, creates the service
# accounts and directories, installs the systemd units and starts the service.
# Re-running it upgrades in place and preserves /etc/rast, /var/lib/rast and
# /srv/rast. It never touches firewalls, DNS or reverse-proxy configuration.
#
# Run with --help for all options and environment variables, or --dry-run to
# see exactly what would change without modifying the system.
#
# Source: https://github.com/Enricko/rast  License: MIT

set -eu

REPO_SLUG="Enricko/rast"
GITHUB_LATEST_URL="https://github.com/${REPO_SLUG}/releases/latest"
API_LATEST_URL="https://api.github.com/repos/${REPO_SLUG}/releases/latest"
DEFAULT_RELEASE_BASE="https://github.com/${REPO_SLUG}/releases/download"
# Overwritten with the exact tag when this file is attached to a GitHub release,
# so the install.sh asset of a release always installs that same release.
RAST_DEFAULT_VERSION="latest"

DEFAULT_BIND="127.0.0.1:8720"
DEFAULT_DATA_DIR="/var/lib/rast"
DEFAULT_STORAGE_DIR="/srv/rast"
BIN_PATH="/usr/local/bin/rast"
LIB_DIR="/usr/local/lib/rast"
CONFIG_DIR="/etc/rast"
CONFIG_FILE="/etc/rast/rast.toml"
UNIT_DIR="/etc/systemd/system"
MAIN_DROPIN_DIR="/etc/systemd/system/rast.service.d"
WORKER_DROPIN_DIR="/etc/systemd/system/rast-worker.service.d"

# ---------------------------------------------------------------------------
# Environment defaults (documented in --help and docs/INSTALL.md)
# ---------------------------------------------------------------------------
RAST_VERSION=${RAST_VERSION:-$RAST_DEFAULT_VERSION}
RAST_BIND=${RAST_BIND:-$DEFAULT_BIND}
RAST_PUBLIC_URL=${RAST_PUBLIC_URL:-}
RAST_DATA_DIR=${RAST_DATA_DIR:-$DEFAULT_DATA_DIR}
RAST_STORAGE_DIR=${RAST_STORAGE_DIR:-$DEFAULT_STORAGE_DIR}
RAST_TRUSTED_PROXIES=${RAST_TRUSTED_PROXIES:-}
RAST_DOCKER_ENABLED=${RAST_DOCKER_ENABLED:-auto}
RAST_ENABLE_WORKER=${RAST_ENABLE_WORKER:-auto}
RAST_NO_START=${RAST_NO_START:-0}
RAST_DRY_RUN=${RAST_DRY_RUN:-0}
RAST_REQUIRE_SIGNATURE=${RAST_REQUIRE_SIGNATURE:-}
RAST_VERIFY_ATTESTATION=${RAST_VERIFY_ATTESTATION:-}
RAST_MINISIGN_PUBKEY=${RAST_MINISIGN_PUBKEY:-}
RAST_COSIGN_PUBKEY=${RAST_COSIGN_PUBKEY:-}
RAST_RELEASE_BASE_URL=${RAST_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE}
RAST_SUDO=${RAST_SUDO:-}

RAST_TMP=""
SUDO=""
NOLOGIN=""
PARSED_BOOL=""

# System administration binaries live in sbin directories that are not on the
# PATH of unprivileged or non-login shells; the installer must find useradd,
# install, systemctl, ... regardless of how it was invoked.
PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${PATH:-}"
export PATH

umask 0022

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
log() { printf '[rast] %s\n' "$*"; }
warn() { printf '[rast] WARNING: %s\n' "$*" >&2; }
die() {
    printf '[rast] ERROR: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'RAST_USAGE'
RAST installer

Usage:
  install.sh [options]
  curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh | sudo sh
  curl -fsSL .../install.sh | sh -s -- --version 0.1.0 --public-url https://rast.example.com

Options:
  --version X.Y.Z        Install a specific release instead of the latest one.
  --bind ADDR            Listen address (default 127.0.0.1:8720).
  --public-url URL       Canonical HTTPS URL, e.g. https://rast.example.com.
  --data-dir DIR         Data/database directory (default /var/lib/rast).
  --storage-dir DIR      User files directory (default /srv/rast).
  --trusted-proxies LIST Comma-separated proxy CIDRs, e.g. 127.0.0.1/32,::1/128.
  --docker               Enable the Docker/Applications integration.
  --no-docker            Disable the Docker/Applications integration.
  --enable-worker        Install and start rast-worker.service (needs Docker).
  --no-worker            Do not install/start rast-worker.service.
  --no-start             Install files only; do not enable or start services.
  --dry-run              Detect, download and verify, but change nothing.
  --verify-attestation   Also verify the binary's GitHub build-provenance
                         attestation with `gh attestation verify` (fail closed).
  -h, --help             Show this help and exit.

Environment variables (flags take precedence):
  RAST_VERSION            Release tag or version, e.g. v0.1.0 (default: latest).
  RAST_BIND               Listen address.
  RAST_PUBLIC_URL         Canonical public URL (must be http:// or https://).
  RAST_DATA_DIR           Data directory (absolute path).
  RAST_STORAGE_DIR        Storage directory (absolute path).
  RAST_TRUSTED_PROXIES    Comma-separated CIDRs of trusted reverse proxies.
  RAST_DOCKER_ENABLED     auto|1|0 — Docker integration (default auto-detect).
  RAST_ENABLE_WORKER      auto|1|0 — worker unit (default auto-detect Docker).
  RAST_NO_START           1 to skip enabling/starting services.
  RAST_DRY_RUN            1 to run in dry-run mode.
  RAST_RELEASE_BASE_URL   Base URL containing v<version>/ directories, used for
                          mirrors/air-gap installs. Default:
                          https://github.com/Enricko/rast/releases/download
                          (with a custom base, pass an explicit RAST_VERSION).
  RAST_VERIFY_ATTESTATION 1 to verify GitHub build-provenance attestations
                          (Sigstore keyless) with `gh attestation verify`.
  RAST_REQUIRE_SIGNATURE  minisign|cosign — fail unless a signature verifies.
  RAST_MINISIGN_PUBKEY    Path to a local minisign public key file.
  RAST_COSIGN_PUBKEY      Path to a local cosign public key file.
  RAST_SUDO               Command used to elevate privileges (default: sudo).

When the worker is installed, the installer also restricts its socket to the
uid of the rast web account (RAST_WORKER_ALLOWED_UID drop-in); the worker
accepts only whitelisted Docker operations from that account.

The installer requires: systemd, an x86_64 or aarch64 CPU, glibc, and curl or
wget. It runs only the documented steps, always verifies the SHA-256 checksum of
the downloaded binary and refuses to continue on a mismatch. With
--verify-attestation it additionally verifies the GitHub/Sigstore
build-provenance attestation (requires the `gh` CLI 2.49+). It never configures
the firewall, DNS, TLS certificates or a reverse proxy — see docs/INSTALL.md.

Documentation: https://github.com/Enricko/rast/blob/main/docs/INSTALL.md
RAST_USAGE
}

rast_cleanup() {
    _rc=$?
    if [ -n "$RAST_TMP" ] && [ -d "$RAST_TMP" ]; then
        case "$RAST_TMP" in
            "${TMPDIR:-/tmp}"/rast-install.*) rm -rf -- "$RAST_TMP" ;;
            *) warn "refusing to remove unexpected temporary path: $RAST_TMP" ;;
        esac
    fi
    if [ "$_rc" -ne 0 ]; then
        printf '\n[rast] Installation failed (exit %s).\n' "$_rc" >&2
        printf '[rast] Existing configuration and user data were not deleted.\n' >&2
        printf '[rast] Check the error above, then re-run the installer. Help: docs/INSTALL.md\n' >&2
    fi
    exit "$_rc"
}

trap 'rast_cleanup' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        --version)
            shift
            [ "$#" -gt 0 ] || die "--version requires a value"
            RAST_VERSION=$1
            ;;
        --version=*) RAST_VERSION=${1#*=} ;;
        --bind)
            shift
            [ "$#" -gt 0 ] || die "--bind requires a value"
            RAST_BIND=$1
            ;;
        --bind=*) RAST_BIND=${1#*=} ;;
        --public-url)
            shift
            [ "$#" -gt 0 ] || die "--public-url requires a value"
            RAST_PUBLIC_URL=$1
            ;;
        --public-url=*) RAST_PUBLIC_URL=${1#*=} ;;
        --data-dir)
            shift
            [ "$#" -gt 0 ] || die "--data-dir requires a value"
            RAST_DATA_DIR=$1
            ;;
        --data-dir=*) RAST_DATA_DIR=${1#*=} ;;
        --storage-dir)
            shift
            [ "$#" -gt 0 ] || die "--storage-dir requires a value"
            RAST_STORAGE_DIR=$1
            ;;
        --storage-dir=*) RAST_STORAGE_DIR=${1#*=} ;;
        --trusted-proxies)
            shift
            [ "$#" -gt 0 ] || die "--trusted-proxies requires a value"
            RAST_TRUSTED_PROXIES=$1
            ;;
        --trusted-proxies=*) RAST_TRUSTED_PROXIES=${1#*=} ;;
        --docker) RAST_DOCKER_ENABLED=1 ;;
        --no-docker) RAST_DOCKER_ENABLED=0 ;;
        --enable-worker) RAST_ENABLE_WORKER=1 ;;
        --no-worker) RAST_ENABLE_WORKER=0 ;;
        --no-start) RAST_NO_START=1 ;;
        --dry-run) RAST_DRY_RUN=1 ;;
        --verify-attestation) RAST_VERIFY_ATTESTATION=1 ;;
        *) die "unknown option: $1 (run with --help for usage)" ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Small utilities
# ---------------------------------------------------------------------------
parse_bool() {
    # $1 label, $2 raw value, $3 default for empty value; sets PARSED_BOOL.
    case "$2" in
        1 | true | TRUE | True | yes | YES | Yes | on | ON) PARSED_BOOL=1 ;;
        0 | false | FALSE | False | no | NO | No | off | OFF) PARSED_BOOL=0 ;;
        "")
            case "$3" in
                auto) die "internal error: auto default passed to parse_bool for $1" ;;
                *) PARSED_BOOL=$3 ;;
            esac
            ;;
        *) die "invalid $1=$2 (expected 1/true/yes/on or 0/false/no/off)" ;;
    esac
}

sed_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/[|&]/\\&/g'
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        die "no SHA-256 tool found (need sha256sum, shasum or openssl)"
    fi
}

# ---------------------------------------------------------------------------
# Detection
# ---------------------------------------------------------------------------
detect_arch() {
    _machine=$(uname -m 2>/dev/null || true)
    case "$_machine" in
        x86_64 | amd64) printf 'x86_64\n' ;;
        aarch64 | arm64) printf 'aarch64\n' ;;
        *)
            die "unsupported CPU architecture '${_machine:-unknown}' (supported: x86_64, aarch64)"
            ;;
    esac
}

detect_os() {
    _pretty="Linux (unknown distribution)"
    _id="unknown"
    _family="unknown"
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        if [ -n "${PRETTY_NAME:-}" ]; then _pretty=$PRETTY_NAME; fi
        _id=${ID:-unknown}
        case "${ID:-} ${ID_LIKE:-}" in
            *debian* | *ubuntu*) _family=debian ;;
            *) _family=other ;;
        esac
    fi
    printf '%s|%s|%s\n' "$_pretty" "$_family" "$_id"
}

require_systemd() {
    if [ ! -d /run/systemd/system ] || ! command -v systemctl >/dev/null 2>&1; then
        die "systemd is required but was not detected (no /run/systemd/system or systemctl).
RAST uses systemd units for its server and worker processes. For non-systemd
systems install manually — see docs/INSTALL.md, section 'Manual installation'."
    fi
}

detect_docker() {
    if command -v docker >/dev/null 2>&1 && [ -S /var/run/docker.sock ]; then
        printf '1\n'
    else
        printf '0\n'
    fi
}

setup_privilege() {
    if [ "$(id -u)" -eq 0 ]; then
        SUDO=""
        return 0
    fi
    if [ -n "$RAST_SUDO" ]; then
        SUDO=$RAST_SUDO
    elif command -v sudo >/dev/null 2>&1; then
        SUDO=sudo
    else
        die "root privileges are required (run as root, or install sudo or set RAST_SUDO)"
    fi
    log "root privileges required for system changes; using '$SUDO'"
}

run() {
    if [ -n "$SUDO" ]; then
        "$SUDO" "$@"
    else
        "$@"
    fi
}

require_commands() {
    for _cmd in useradd groupadd usermod getent install; do
        command -v "$_cmd" >/dev/null 2>&1 ||
            die "required command '$_cmd' not found (install the shadow/passwd and coreutils packages)"
    done
    for _candidate in /usr/sbin/nologin /sbin/nologin /bin/false; do
        if [ -x "$_candidate" ]; then
            NOLOGIN=$_candidate
            break
        fi
    done
    [ -n "$NOLOGIN" ] || die "no nologin/false shell found in /usr/sbin, /sbin or /bin"
}

# ---------------------------------------------------------------------------
# Version resolution and download
# ---------------------------------------------------------------------------
normalize_version() {
    _v=$1
    case "$_v" in v*) _v=${_v#v} ;; esac
    if ! printf '%s' "$_v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
        die "invalid version '$1' (expected X.Y.Z, optionally with a pre-release suffix)"
    fi
    printf '%s\n' "$_v"
}

resolve_latest_tag() {
    _tag=""
    if command -v curl >/dev/null 2>&1; then
        _effective=$(curl -fsSL --retry 2 --connect-timeout 15 --max-time 60 \
            -o /dev/null -w '%{url_effective}' "$GITHUB_LATEST_URL" 2>/dev/null || true)
        case "$_effective" in
            */tag/*) _tag=${_effective##*/tag/} ;;
        esac
        if [ -z "$_tag" ]; then
            _json=$(curl -fsSL --retry 2 --connect-timeout 15 --max-time 60 \
                -H 'Accept: application/vnd.github+json' "$API_LATEST_URL" 2>/dev/null || true)
            _tag=$(printf '%s\n' "$_json" |
                sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
                head -n 1)
        fi
    elif command -v wget >/dev/null 2>&1; then
        _json=$(wget -q -T 30 -O - "$API_LATEST_URL" 2>/dev/null || true)
        _tag=$(printf '%s\n' "$_json" |
            sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
            head -n 1)
    else
        die "curl or wget is required to download RAST"
    fi
    if [ -z "$_tag" ]; then
        die "cannot determine the latest RAST release (no releases published yet or no network).
Re-run with an explicit version, for example: RAST_VERSION=0.1.0 sh install.sh"
    fi
    printf '%s\n' "$_tag"
}

download() {
    # $1 URL, $2 destination, $3 human description
    _url=$1
    _dest=$2
    _what=${3:-file}
    rm -f -- "$_dest"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --retry-delay 1 --connect-timeout 20 --max-time 900 \
            -o "$_dest" "$_url" || die "failed to download $_what from $_url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$_dest" "$_url" || die "failed to download $_what from $_url"
    else
        die "curl or wget is required to download RAST"
    fi
    [ -s "$_dest" ] || die "downloaded $_what is empty: $_url"
}

verify_checksum() {
    # $1 SHA256SUMS path, $2 file, $3 asset name
    _sums=$1
    _file=$2
    _name=$3
    _expected=$(awk -v f="$_name" '$2 == f || $2 == "*" f { print $1; exit }' "$_sums")
    [ -n "$_expected" ] || die "the release SHA256SUMS file has no entry for $_name — refusing to install"
    _actual=$(sha256_file "$_file")
    if [ "$_expected" != "$_actual" ]; then
        die "SHA-256 checksum mismatch for $_name
  expected: $_expected
  actual:   $_actual
The download is corrupt or has been tampered with. Nothing was installed."
    fi
    log "SHA-256 verified for $_name"
}

verify_signature() {
    # $1 release base URL, $2 downloaded file, $3 asset name
    [ -n "$RAST_REQUIRE_SIGNATURE" ] || return 0
    _base=$1
    _file=$2
    _name=$3
    case "$RAST_REQUIRE_SIGNATURE" in
        minisign)
            command -v minisign >/dev/null 2>&1 ||
                die "RAST_REQUIRE_SIGNATURE=minisign but the minisign tool is not installed"
            if [ -z "$RAST_MINISIGN_PUBKEY" ] || [ ! -f "$RAST_MINISIGN_PUBKEY" ]; then
                die "RAST_REQUIRE_SIGNATURE=minisign requires RAST_MINISIGN_PUBKEY to point at a local public key file"
            fi
            download "$_base/$_name.minisig" "$RAST_TMP/$_name.minisig" "minisign signature"
            if ! minisign -Vm "$_file" -p "$RAST_MINISIGN_PUBKEY" -x "$RAST_TMP/$_name.minisig" >/dev/null; then
                die "minisign signature verification failed for $_name — refusing to install"
            fi
            log "minisign signature verified for $_name"
            ;;
        cosign)
            command -v cosign >/dev/null 2>&1 ||
                die "RAST_REQUIRE_SIGNATURE=cosign but the cosign tool is not installed"
            if [ -z "$RAST_COSIGN_PUBKEY" ] || [ ! -f "$RAST_COSIGN_PUBKEY" ]; then
                die "RAST_REQUIRE_SIGNATURE=cosign requires RAST_COSIGN_PUBKEY to point at a local public key file"
            fi
            download "$_base/$_name.sig" "$RAST_TMP/$_name.sig" "cosign signature"
            if ! cosign verify-blob --key "$RAST_COSIGN_PUBKEY" \
                --signature "$RAST_TMP/$_name.sig" "$_file" >/dev/null 2>&1; then
                die "cosign signature verification failed for $_name — refusing to install"
            fi
            log "cosign signature verified for $_name"
            ;;
        *)
            die "unsupported RAST_REQUIRE_SIGNATURE=$RAST_REQUIRE_SIGNATURE (expected 'minisign' or 'cosign')"
            ;;
    esac
}

verify_attestation() {
    # $1 downloaded file, $2 asset name. Optional fail-closed provenance check.
    [ "$ATTESTATION_VERIFY" -eq 1 ] || return 0
    _file=$1
    _name=$2
    command -v gh >/dev/null 2>&1 ||
        die "RAST_VERIFY_ATTESTATION is enabled but the GitHub CLI 'gh' is not installed.
Install it from https://cli.github.com/ or re-run without --verify-attestation."
    if ! gh attestation verify --help >/dev/null 2>&1; then
        die "the installed 'gh' CLI does not support 'gh attestation verify' (requires GitHub CLI 2.49 or newer).
Upgrade gh or re-run without --verify-attestation."
    fi
    if ! gh attestation verify "$_file" --repo "$REPO_SLUG" >/dev/null 2>&1; then
        die "build-provenance attestation verification failed for $_name — refusing to install.
The binary was not attested by the $REPO_SLUG release workflow (or GitHub could not be reached)."
    fi
    log "build-provenance attestation verified for $_name"
}

# ---------------------------------------------------------------------------
# User, directory and file management
# ---------------------------------------------------------------------------
ensure_group() {
    _group=$1
    if getent group "$_group" >/dev/null 2>&1; then
        return 0
    fi
    run groupadd --system "$_group"
    log "created system group $_group"
}

ensure_user() {
    # $1 user, $2 primary group, $3 home dir, $4 comment
    _user=$1
    _pgroup=$2
    _home=$3
    _comment=$4
    if id "$_user" >/dev/null 2>&1; then
        return 0
    fi
    run useradd --system --gid "$_pgroup" --home-dir "$_home" --no-create-home \
        --shell "$NOLOGIN" --comment "$_comment" "$_user"
    log "created system user $_user (group $_pgroup, shell $NOLOGIN)"
}

ensure_worker_user() {
    ensure_user rast-worker rast /nonexistent "RAST Docker worker"
    _current_group=$(id -gn rast-worker 2>/dev/null || true)
    if [ "$_current_group" != "rast" ]; then
        run usermod -g rast rast-worker
        log "set rast-worker primary group to rast"
    fi
    if getent group docker >/dev/null 2>&1; then
        if ! id -nG rast-worker 2>/dev/null | tr ' ' '\n' | grep -qx docker; then
            run usermod -aG docker rast-worker
            log "added rast-worker to the docker group"
        fi
    else
        warn "the 'docker' group does not exist; rast-worker cannot access the Docker socket until Docker is installed"
    fi
}

install_managed_file() {
    # $1 source, $2 destination, $3 mode, $4 owner, $5 group
    _src=$1
    _dst=$2
    _mode=$3
    _owner=$4
    _grp=$5
    if [ -f "$_dst" ] && cmp -s -- "$_src" "$_dst"; then
        log "unchanged: $_dst"
        return 0
    fi
    run install -m "$_mode" -o "$_owner" -g "$_grp" "$_src" "$_dst"
    log "installed: $_dst"
}

# >>> BEGIN embedded deploy/rast.toml.example
# Generated by scripts/sync-installer-assets.sh from deploy/rast.toml.example — do not edit.
write_config_template() {
    cat > "$1" <<'RAST_EMBED_CONFIG_TEMPLATE'
# RAST configuration file — /etc/rast/rast.toml
#
# Every setting is optional; the built-in defaults are shown below. Values are
# loaded once at startup: run `systemctl restart rast.service` after editing.
#
# Precedence (lowest to highest):
#   built-in defaults  <  this file  <  RAST_* environment variables  <  CLI flags
#
# Environment variables may also be placed in /etc/rast/rast.env (mode 0600
# root:rast), which rast.service loads when present. See docs/CONFIGURATION.md
# for the full reference, validation ranges and examples.
#
# Unknown keys are rejected at startup so that typos never go unnoticed; run
# `rast doctor` to validate the resulting configuration.

# ---------------------------------------------------------------------------
# Network
# ---------------------------------------------------------------------------

# Listen address. Keep it on loopback when a reverse proxy terminates TLS.
# env: RAST_BIND
bind = "127.0.0.1:8720"

# Canonical public URL (no trailing slash needed). Required in production:
# it drives the Secure cookie flag, login redirects and links.
# Must start with http:// or https://.
# env: RAST_PUBLIC_URL
# public_url = "https://rast.example.com"

# Reverse proxies whose X-Forwarded-For / CF-Connecting-IP headers are trusted.
# CIDR notation is required; bare IP addresses are rejected. Only list proxies
# you control — otherwise clients could spoof their address.
# env: RAST_TRUSTED_PROXIES (comma separated)
trusted_proxies = []
# trusted_proxies = ["127.0.0.1/32", "::1/128"]

# Honour forwarding headers at all. Default: automatic — enabled when
# trusted_proxies is non-empty. Set explicitly only if you know why.
# env: RAST_TRUST_FORWARDED_HEADERS
# trust_forwarded_headers = true

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

# Database, backups, managed applications and key material. Must be on a local
# filesystem with working SQLite locking (no NFS/SMB/CIFS).
# env: RAST_DATA_DIR
data_dir = "/var/lib/rast"

# User homes, shared folders and the recycle bin.
# Recommended: a separate volume; never set it inside data_dir.
# env: RAST_STORAGE_DIR
storage_dir = "/srv/rast"

# Unix socket of the privileged Docker worker. The directory is created by
# rast-worker.service (RuntimeDirectory) and removed when it stops.
# env: RAST_WORKER_SOCKET
worker_socket = "/run/rast/worker.sock"

# ---------------------------------------------------------------------------
# Sessions and authentication
# ---------------------------------------------------------------------------

# Sliding session lifetime in hours (1–8760). The absolute cap is 30 days.
# env: RAST_SESSION_TTL_HOURS
session_ttl_hours = 168

# Force the Secure cookie attribute. Default: automatic — true when public_url
# is https://. Leave unset unless you terminate TLS in an unusual way.
# env: RAST_SECURE_COOKIES
# secure_cookies = true

# Require `rast setup-token` before the web-based first-run setup is allowed.
# CLI account creation (`rast user create-admin`) always works. Recommended for
# any internet-reachable instance.
# env: RAST_SETUP_TOKEN_REQUIRED
setup_token_required = false

# ---------------------------------------------------------------------------
# Storage, uploads and quotas
# ---------------------------------------------------------------------------

# Maximum size of a single uploaded file in bytes (1 MiB – 1 TiB).
# Keep reverse-proxy body limits at or above this value.
# env: RAST_MAX_UPLOAD_BYTES
max_upload_bytes = 10737418240

# Default per-user quota in bytes (0 = unlimited). New accounts only; existing
# quotas are changed per user in the UI/CLI.
# env: RAST_DEFAULT_QUOTA_BYTES
default_quota_bytes = 21474836480

# ---------------------------------------------------------------------------
# Database and background jobs
# ---------------------------------------------------------------------------

# SQLite connection pool size (1–64). 8 is plenty for a home server.
# env: RAST_DB_MAX_CONNECTIONS
db_max_connections = 8

# Concurrent background jobs (1–8): copies, moves, deletes, backups, deploys.
# env: RAST_JOB_CONCURRENCY
job_concurrency = 2

# ---------------------------------------------------------------------------
# Monitoring and logging
# ---------------------------------------------------------------------------

# Resource monitor sampling interval in seconds (1–300).
# env: RAST_MONITOR_INTERVAL_SECS
monitor_interval_secs = 5

# Log level written to stderr/journald: error, warn, info, debug, trace.
# RUST_LOG, when set, overrides this at runtime.
# env: RAST_LOG_LEVEL
log_level = "info"

# ---------------------------------------------------------------------------
# Optional Docker integration
# ---------------------------------------------------------------------------

# Enable the Applications app (Docker Compose deployments). Requires Docker and
# rast-worker.service; the server itself never accesses the Docker socket.
# env: RAST_DOCKER_ENABLED
docker_enabled = false
RAST_EMBED_CONFIG_TEMPLATE
}
# <<< END embedded deploy/rast.toml.example

# >>> BEGIN embedded deploy/rast.service
# Generated by scripts/sync-installer-assets.sh from deploy/rast.service — do not edit.
write_unit_rast_service() {
    cat > "$1" <<'RAST_EMBED_RAST_SERVICE'
[Unit]
Description=RAST — Resource Administration & Storage Toolkit
Documentation=https://github.com/Enricko/rast/blob/main/docs/INSTALL.md
Documentation=https://github.com/Enricko/rast/blob/main/docs/OPERATIONS.md
After=network-online.target
Wants=network-online.target

[Service]
# `rast serve` runs in the foreground, logs to stderr (journald) and shuts down
# gracefully on SIGTERM (in-flight requests finish, then WAL checkpoint).
Type=simple
ExecStart=/usr/local/bin/rast serve
Restart=on-failure
RestartSec=2
TimeoutStopSec=30
KillSignal=SIGTERM

# Unprivileged service account created by the installer.
User=rast
Group=rast
WorkingDirectory=/var/lib/rast
Environment=RAST_CONFIG=/etc/rast/rast.toml
# Optional operator overrides (RAST_* variables, mode 0600 root:rast).
EnvironmentFile=-/etc/rast/rast.env

# The configuration file must exist before the service starts. The installer
# creates it; if you delete it, systemd will report a failed unit instead of
# silently starting with defaults.
# Config is read once at startup: `systemctl restart rast.service` after edits.

# Files written by the server are 0640/0750: no access for other users.
UMask=0027

# CPU/memory guardrails. Tune with a drop-in, e.g.
#   /etc/systemd/system/rast.service.d/20-memory.conf
#   [Service]
#   MemoryHigh=1G
#   MemoryMax=2G
MemoryHigh=512M
MemoryMax=1G
LimitNOFILE=65536

# ---- Sandboxing -----------------------------------------------------------
# The server itself never needs privileges: it listens on an unprivileged port,
# opens sockets, reads/writes its own directories and talks to the worker over a
# Unix socket. Docker access is deliberately delegated to rast-worker.service.
NoNewPrivileges=yes
CapabilityBoundingSet=
AmbientCapabilities=
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/rast /srv/rast
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
# Block genuinely dangerous syscall groups without whitelisting everything
# (a whitelist such as @system-service can break new runtimes; switch to it via
# a drop-in only after testing on your distribution).
SystemCallFilter=~@mount @swap @reboot @obsolete @raw-io @cpu-emulation

[Install]
WantedBy=multi-user.target
RAST_EMBED_RAST_SERVICE
}
# <<< END embedded deploy/rast.service

# >>> BEGIN embedded deploy/rast-worker.service
# Generated by scripts/sync-installer-assets.sh from deploy/rast-worker.service — do not edit.
write_unit_rast_worker() {
    cat > "$1" <<'RAST_EMBED_RAST_WORKER'
[Unit]
Description=RAST privileged worker (Docker administration)
Documentation=https://github.com/Enricko/rast/blob/main/docs/ARCHITECTURE.md
Documentation=https://github.com/Enricko/rast/blob/main/docs/OPERATIONS.md
After=network-online.target docker.service
Wants=network-online.target docker.service
# The worker is only useful when Docker is available. If Docker is installed
# later, restart this unit (`systemctl restart rast-worker.service`).
ConditionPathExists=/var/run/docker.sock

[Service]
# Internal JSON-lines service on /run/rast/worker.sock. The web server never
# touches the Docker socket; this process does, and only runs whitelisted
# `docker compose`/`docker` operations with structured arguments (no shell).
Type=simple
ExecStart=/usr/local/bin/rast worker
Restart=on-failure
RestartSec=2
TimeoutStopSec=30
KillSignal=SIGTERM

# Dedicated account. Primary group is `rast` so that the socket created here is
# owned rast-worker:rast and the unprivileged rast.service (group rast) can
# connect. `docker` is a supplementary group because /var/run/docker.sock is
# root:docker 0660 — membership in `docker` is effectively root-equivalent.
User=rast-worker
Group=rast
SupplementaryGroups=docker
WorkingDirectory=/var/lib/rast
Environment=RAST_CONFIG=/etc/rast/rast.toml
# The worker chowns its socket to this group (rast), so rast.service — which
# runs as rast:rast — can connect to /run/rast/worker.sock. Never add the
# unprivileged web server to the `docker` group instead.
Environment=RAST_WORKER_SOCKET_GROUP=rast
# Defense in depth: only the web server's uid may connect. The installer writes
# /etc/systemd/system/rast-worker.service.d/20-peer-uid.conf with
# RAST_WORKER_ALLOWED_UID=<uid of the rast user>; set it manually if you install
# the worker by hand.
EnvironmentFile=-/etc/rast/rast.env
# Keep the Docker CLI from trying to write to a read-only home directory.
Environment=DOCKER_CONFIG=/run/rast

# Socket directory. Created before ExecStart, removed on stop, preserved across
# restarts so clients see a stable path. Mode/ownership: rast-worker:rast 0750.
RuntimeDirectory=rast
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=restart

# Socket files must stay group-readable/writable (0660) for rast.service, so
# the worker uses a permissive group umask instead of the server's 0027.
UMask=0007
LimitNOFILE=65536
MemoryHigh=256M
MemoryMax=1G

# ---- Sandboxing -----------------------------------------------------------
# The worker needs: the Docker socket (/run/docker.sock, not covered by
# ProtectSystem), its runtime directory (/run/rast) and read-only access to
# managed compose files under /var/lib/rast/apps. It does not need privileges
# of its own — the Docker daemon performs privileged operations.
NoNewPrivileges=yes
CapabilityBoundingSet=
AmbientCapabilities=
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/rast/apps
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallArchitectures=native
# Go-based Docker/Compose binaries are sensitive to overly tight whitelists.
# A blacklist of dangerous groups is used for compatibility; tighten with a
# drop-in (SystemCallFilter=@system-service) if your Docker version allows it.
SystemCallFilter=~@mount @swap @reboot @obsolete @raw-io

[Install]
WantedBy=multi-user.target
RAST_EMBED_RAST_WORKER
}
# <<< END embedded deploy/rast-worker.service

render_main_config() {
    # $1 destination path
    _dst=$1
    write_config_template "$RAST_TMP/rast.toml.tpl"
    _e_bind=$(sed_escape "$BIND")
    _e_data=$(sed_escape "$DATA_DIR")
    _e_storage=$(sed_escape "$STORAGE_DIR")
    sed \
        -e "s|^bind = .*|bind = \"$_e_bind\"|" \
        -e "s|^data_dir = .*|data_dir = \"$_e_data\"|" \
        -e "s|^storage_dir = .*|storage_dir = \"$_e_storage\"|" \
        -e "s|^docker_enabled = .*|docker_enabled = $DOCKER_ENABLED_BOOL|" \
        "$RAST_TMP/rast.toml.tpl" >"$RAST_TMP/rast.toml.step1"

    if [ -n "$PUBLIC_URL" ]; then
        _e_url=$(sed_escape "$PUBLIC_URL")
        sed -e "s|^# public_url = .*|public_url = \"$_e_url\"|" \
            "$RAST_TMP/rast.toml.step1" >"$RAST_TMP/rast.toml.step2"
        mv -- "$RAST_TMP/rast.toml.step2" "$RAST_TMP/rast.toml.step1"
    fi

    if [ -n "$TRUSTED_PROXIES" ]; then
        _proxy_list=""
        _old_ifs=$IFS
        IFS=,
        # Intentional word splitting on commas; the value was validated above.
        # shellcheck disable=SC2086
        for _proxy in $TRUSTED_PROXIES; do
            _proxy_list="${_proxy_list}${_proxy_list:+, }\"${_proxy}\""
        done
        IFS=$_old_ifs
        sed -e "s|^trusted_proxies = .*|trusted_proxies = [$_proxy_list]|" \
            "$RAST_TMP/rast.toml.step1" >"$RAST_TMP/rast.toml.step2"
        mv -- "$RAST_TMP/rast.toml.step2" "$RAST_TMP/rast.toml.step1"
    fi

    cp -- "$RAST_TMP/rast.toml.step1" "$_dst"
}

write_paths_dropin() {
    # $1 destination, rest: paths
    _dst=$1
    shift
    {
        printf '[Service]\n'
        printf '# Generated by the RAST installer: custom data/storage directories.\n'
        printf 'ReadWritePaths=\n'
        printf 'ReadWritePaths='
        _first=1
        for _path in "$@"; do
            if [ "$_first" -eq 1 ]; then
                _first=0
            else
                printf ' '
            fi
            printf '%s' "$_path"
        done
        printf '\n'
    } >"$_dst"
}

wait_for_health() {
    _bind=$1
    _host=${_bind%:*}
    _port=${_bind##*:}
    case "$_host" in
        0.0.0.0 | "[::]" | "::" | "") _host=127.0.0.1 ;;
    esac
    _host=${_host#[}
    _host=${_host%]}
    case "$_host" in
        *:*) _url_host="[$_host]" ;;
        *) _url_host=$_host ;;
    esac
    _i=0
    while [ "$_i" -lt 30 ]; do
        if command -v curl >/dev/null 2>&1; then
            if curl -fsS --max-time 2 "http://$_url_host:$_port/healthz" >/dev/null 2>&1; then
                return 0
            fi
        elif command -v wget >/dev/null 2>&1; then
            if wget -q -T 2 -O /dev/null "http://$_url_host:$_port/healthz" 2>/dev/null; then
                return 0
            fi
        fi
        _i=$((_i + 1))
        sleep 1
    done
    return 1
}

# ---------------------------------------------------------------------------
# Parse boolean/mode settings and validate the requested configuration
# ---------------------------------------------------------------------------
parse_bool "RAST_NO_START" "$RAST_NO_START" 0
NO_START=$PARSED_BOOL
parse_bool "RAST_DRY_RUN" "$RAST_DRY_RUN" 0
DRY_RUN=$PARSED_BOOL
parse_bool "RAST_VERIFY_ATTESTATION" "$RAST_VERIFY_ATTESTATION" 0
ATTESTATION_VERIFY=$PARSED_BOOL

case "$RAST_DOCKER_ENABLED" in
    "" | auto | AUTO | Auto) DOCKER_CHOICE=auto ;;
    1 | true | TRUE | True | yes | YES | Yes | on | ON) DOCKER_CHOICE=1 ;;
    0 | false | FALSE | False | no | NO | No | off | OFF) DOCKER_CHOICE=0 ;;
    *) die "invalid RAST_DOCKER_ENABLED=$RAST_DOCKER_ENABLED (expected auto, 1 or 0)" ;;
esac
case "$RAST_ENABLE_WORKER" in
    "" | auto | AUTO | Auto) WORKER_CHOICE=auto ;;
    1 | true | TRUE | True | yes | YES | Yes | on | ON) WORKER_CHOICE=1 ;;
    0 | false | FALSE | False | no | NO | No | off | OFF) WORKER_CHOICE=0 ;;
    *) die "invalid RAST_ENABLE_WORKER=$RAST_ENABLE_WORKER (expected auto, 1 or 0)" ;;
esac

[ -n "$RAST_BIND" ] || die "bind address must not be empty"
case "$RAST_BIND" in
    *:*) ;;
    *) die "invalid bind address '$RAST_BIND' (expected HOST:PORT, e.g. 127.0.0.1:8720)" ;;
esac
case "$RAST_BIND" in
    *" "* | *"	"*) die "invalid bind address '$RAST_BIND' (whitespace is not allowed)" ;;
esac
BIND=$RAST_BIND
_bind_port=${BIND##*:}
case "$_bind_port" in
    0 | "" | *[!0-9]*) die "invalid port in bind address '$BIND' (expected 1-65535)" ;;
esac

PUBLIC_URL=""
if [ -n "$RAST_PUBLIC_URL" ]; then
    case "$RAST_PUBLIC_URL" in
        http://* | https://*) ;;
        *) die "invalid public URL '$RAST_PUBLIC_URL' (must start with http:// or https://)" ;;
    esac
    case "$RAST_PUBLIC_URL" in
        *[[:space:]]*) die "invalid public URL '$RAST_PUBLIC_URL' (whitespace is not allowed)" ;;
    esac
    PUBLIC_URL=${RAST_PUBLIC_URL%/}
fi

case "$RAST_DATA_DIR" in
    /*) ;;
    *) die "data directory must be an absolute path: '$RAST_DATA_DIR'" ;;
esac
case "$RAST_STORAGE_DIR" in
    /*) ;;
    *) die "storage directory must be an absolute path: '$RAST_STORAGE_DIR'" ;;
esac
[ "$RAST_DATA_DIR" != "/" ] || die "data directory must not be /"
[ "$RAST_STORAGE_DIR" != "/" ] || die "storage directory must not be /"
DATA_DIR=${RAST_DATA_DIR%/}
STORAGE_DIR=${RAST_STORAGE_DIR%/}
[ "$DATA_DIR" != "$STORAGE_DIR" ] ||
    die "data directory and storage directory must be different"
case "$STORAGE_DIR" in
    "$DATA_DIR"/*) die "storage directory must not be inside the data directory" ;;
esac

TRUSTED_PROXIES=""
if [ -n "$RAST_TRUSTED_PROXIES" ]; then
    if ! printf '%s' "$RAST_TRUSTED_PROXIES" |
        grep -Eq '^[[:space:]]*[0-9A-Fa-f:.]+/[0-9]{1,3}([[:space:]]*,[[:space:]]*[0-9A-Fa-f:.]+/[0-9]{1,3})*[[:space:]]*$'; then
        die "invalid RAST_TRUSTED_PROXIES='$RAST_TRUSTED_PROXIES' (expected comma-separated CIDRs, e.g. 127.0.0.1/32,::1/128)"
    fi
    TRUSTED_PROXIES=$(printf '%s' "$RAST_TRUSTED_PROXIES" | tr -d '[:space:]')
fi

case "$RAST_RELEASE_BASE_URL" in
    *[[:space:]]*) die "RAST_RELEASE_BASE_URL must not contain whitespace" ;;
esac
RELEASE_BASE=${RAST_RELEASE_BASE_URL%/}

# ---------------------------------------------------------------------------
# Detection and release resolution
# ---------------------------------------------------------------------------
ARCH=$(detect_arch)
_os_info=$(detect_os)
OS_PRETTY=${_os_info%%|*}
_rest=${_os_info#*|}
OS_FAMILY=${_rest%%|*}
OS_ID=${_rest#*|}

require_systemd
require_commands
DOCKER_DETECTED=$(detect_docker)

case "$WORKER_CHOICE" in
    auto)
        if [ "$DOCKER_DETECTED" -eq 1 ]; then WORKER_ENABLED=1; else WORKER_ENABLED=0; fi
        ;;
    1) WORKER_ENABLED=1 ;;
    0) WORKER_ENABLED=0 ;;
esac
case "$DOCKER_CHOICE" in
    auto)
        if [ "$DOCKER_DETECTED" -eq 1 ]; then DOCKER_ENABLED=1; else DOCKER_ENABLED=0; fi
        ;;
    1) DOCKER_ENABLED=1 ;;
    0) DOCKER_ENABLED=0 ;;
esac
if [ "$DOCKER_ENABLED" -eq 1 ]; then DOCKER_ENABLED_BOOL=true; else DOCKER_ENABLED_BOOL=false; fi
if [ "$DOCKER_ENABLED" -eq 1 ] && [ "$WORKER_ENABLED" -eq 0 ]; then
    warn "Docker integration is enabled but the worker is disabled; the Applications app will report Docker as unavailable"
fi
if [ "$WORKER_ENABLED" -eq 1 ] && [ "$DOCKER_DETECTED" -eq 0 ]; then
    warn "the worker was requested but Docker was not detected; rast-worker.service will stay inactive until Docker is installed"
fi

if [ "$RAST_VERSION" = "latest" ] || [ -z "$RAST_VERSION" ]; then
    log "resolving the latest RAST release"
    _tag=$(resolve_latest_tag)
else
    _tag=$RAST_VERSION
fi
VERSION=$(normalize_version "$_tag")
TAG="v$VERSION"
RELEASE_URL="$RELEASE_BASE/$TAG"
BINARY_ASSET="rast-linux-$ARCH"

log "RAST $VERSION for linux/$ARCH"
log "platform: $OS_PRETTY"
if [ "$OS_FAMILY" != "debian" ]; then
    warn "distribution '$OS_ID' is not a tested Debian/Ubuntu derivative; systemd, useradd and install are required and the unit paths must match your distro (best-effort support)"
fi

if [ "$DRY_RUN" -eq 0 ]; then
    setup_privilege
else
    if [ "$(id -u)" -ne 0 ]; then
        log "dry run: would use sudo/root privileges for system changes"
    fi
fi

RAST_TMP=$(mktemp -d "${TMPDIR:-/tmp}/rast-install.XXXXXX") ||
    die "cannot create a temporary directory in ${TMPDIR:-/tmp}"

# ---------------------------------------------------------------------------
# Download and verify
# ---------------------------------------------------------------------------
log "downloading $BINARY_ASSET and SHA256SUMS from $RELEASE_URL"
download "$RELEASE_URL/$BINARY_ASSET" "$RAST_TMP/$BINARY_ASSET" "RAST binary"
download "$RELEASE_URL/SHA256SUMS" "$RAST_TMP/SHA256SUMS" "SHA256SUMS"
verify_checksum "$RAST_TMP/SHA256SUMS" "$RAST_TMP/$BINARY_ASSET" "$BINARY_ASSET"
verify_signature "$RELEASE_URL" "$RAST_TMP/$BINARY_ASSET" "$BINARY_ASSET"
verify_attestation "$RAST_TMP/$BINARY_ASSET" "$BINARY_ASSET"

chmod 0755 "$RAST_TMP/$BINARY_ASSET"
if ! _version_output=$("$RAST_TMP/$BINARY_ASSET" version 2>&1); then
    die "the downloaded binary cannot run on this host:
$_version_output
This usually means a glibc version or CPU architecture mismatch. Build RAST
from source instead — see docs/INSTALL.md, section 'Build from source'."
fi
_binary_version=$(printf '%s\n' "$_version_output" | head -n 1 | awk '{print $2}')
if [ "$_binary_version" != "$VERSION" ]; then
    die "release $TAG contains a binary reporting version '${_binary_version:-unknown}' — refusing to install"
fi
log "binary self-check passed (rast version $VERSION)"

_old_version=""
if [ -x "$BIN_PATH" ]; then
    _old_version=$("$BIN_PATH" version 2>/dev/null | head -n 1 | awk '{print $2}') || _old_version=""
    case "$_old_version" in
        "" | *[!0-9A-Za-z._-]*) _old_version="unknown" ;;
    esac
fi

# ---------------------------------------------------------------------------
# Plan
# ---------------------------------------------------------------------------
SERVICE_URL=${PUBLIC_URL:-http://$BIND}

# Human-readable strings used by the dry-run and final summaries.
if [ "$WORKER_ENABLED" -eq 1 ]; then
    WORKER_STATE="enabled"
    WORKER_ACCOUNTS=", rast-worker (group docker)"
    WORKER_SERVICE=", rast-worker.service"
    WORKER_SUMMARY="rast-worker.service"
else
    WORKER_STATE="disabled"
    WORKER_ACCOUNTS=""
    WORKER_SERVICE=""
    WORKER_SUMMARY="disabled (no Docker detected or disabled explicitly)"
fi
if [ "$DOCKER_ENABLED" -eq 1 ]; then DOCKER_STATE="enabled"; else DOCKER_STATE="disabled"; fi
if [ "$NO_START" -eq 1 ]; then START_STATE="no"; else START_STATE="yes"; fi
if [ "$ATTESTATION_VERIFY" -eq 1 ]; then
    ATTESTATION_STATE="verified via gh attestation"
else
    ATTESTATION_STATE="not verified (enable with --verify-attestation)"
fi
if [ -f "$CONFIG_FILE" ]; then CONFIG_STATE="preserved"; else CONFIG_STATE="created from template"; fi

if [ "$DRY_RUN" -eq 1 ]; then
    printf '\n'
    log "DRY RUN — no changes will be made. Planned actions:"
    printf '  version:          %s (tag %s)\n' "$VERSION" "$TAG"
    printf '  architecture:     linux/%s\n' "$ARCH"
    printf '  platform:         %s\n' "$OS_PRETTY"
    printf '  binary:           %s (0755 root:root)\n' "$BIN_PATH"
    printf '  previous binary:  %s\n' "${_old_version:-none}"
    printf '  service accounts: rast%s\n' "$WORKER_ACCOUNTS"
    printf '  data directory:   %s (0750 rast:rast)\n' "$DATA_DIR"
    printf '  storage:          %s, %s/homes, %s/shared (0755 rast:rast), %s/.trash (0700)\n' \
        "$STORAGE_DIR" "$STORAGE_DIR" "$STORAGE_DIR" "$STORAGE_DIR"
    printf '  configuration:    %s (%s)\n' "$CONFIG_FILE" "$CONFIG_STATE"
    printf '  bind/public url:  %s / %s\n' "$BIND" "${PUBLIC_URL:-<not set>}"
    printf '  trusted proxies:  %s\n' "${TRUSTED_PROXIES:-<none>}"
    printf '  provenance:       %s\n' "$ATTESTATION_STATE"
    printf '  docker apps:      %s (worker %s)\n' "$DOCKER_STATE" "$WORKER_STATE"
    printf '  services:         rast.service%s\n' "$WORKER_SERVICE"
    printf '  start services:   %s\n' "$START_STATE"
    printf '\nRe-run without --dry-run (or RAST_DRY_RUN=0) to apply.\n'
    exit 0
fi

# ---------------------------------------------------------------------------
# Service accounts and directories
# ---------------------------------------------------------------------------
log "creating service accounts and directories"
ensure_group rast
ensure_user rast rast /nonexistent "RAST service account"
if [ "$WORKER_ENABLED" -eq 1 ]; then
    ensure_worker_user
fi

run install -d -m 0750 -o rast -g rast "$DATA_DIR"
run install -d -m 0755 -o rast -g rast "$STORAGE_DIR" \
    "$STORAGE_DIR/homes" "$STORAGE_DIR/shared"
# The recycle bin is private to the service account (deleted files may contain
# sensitive data); rast itself also enforces 0700 on .trash at startup.
run install -d -m 0700 -o rast -g rast "$STORAGE_DIR/.trash"
run install -d -m 0750 -o root -g rast "$CONFIG_DIR"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
if [ -f "$CONFIG_FILE" ]; then
    log "preserving existing configuration: $CONFIG_FILE"
    if [ "$BIND" != "$DEFAULT_BIND" ] || [ -n "$PUBLIC_URL" ] || \
        [ "$DATA_DIR" != "$DEFAULT_DATA_DIR" ] || [ "$STORAGE_DIR" != "$DEFAULT_STORAGE_DIR" ] || \
        [ -n "$TRUSTED_PROXIES" ] || [ "$DOCKER_ENABLED" -eq 1 ]; then
        warn "configuration options were supplied but $CONFIG_FILE already exists; it was NOT modified"
        warn "edit it manually or remove it before re-running the installer to apply the new values"
    fi
else
    log "creating $CONFIG_FILE from the packaged template"
    render_main_config "$RAST_TMP/rast.toml.rendered"
    install_managed_file "$RAST_TMP/rast.toml.rendered" "$CONFIG_FILE" 0640 root rast
fi

# ---------------------------------------------------------------------------
# Binary
# ---------------------------------------------------------------------------
if [ -n "$_old_version" ] && [ "$_old_version" != "$VERSION" ]; then
    log "replacing RAST $_old_version with $VERSION"
    run install -d -m 0755 -o root -g root "$LIB_DIR"
    run cp -p -- "$BIN_PATH" "$LIB_DIR/rast-$_old_version"
    log "kept previous binary as $LIB_DIR/rast-$_old_version (for rollback)"
else
    log "installing RAST $VERSION"
fi
run install -m 0755 -o root -g root "$RAST_TMP/$BINARY_ASSET" "$BIN_PATH.new"
run mv -f -- "$BIN_PATH.new" "$BIN_PATH"
log "installed: $BIN_PATH"

# ---------------------------------------------------------------------------
# systemd units
# ---------------------------------------------------------------------------
write_unit_rast_service "$RAST_TMP/rast.service"
install_managed_file "$RAST_TMP/rast.service" "$UNIT_DIR/rast.service" 0644 root root

if [ "$WORKER_ENABLED" -eq 1 ]; then
    write_unit_rast_worker "$RAST_TMP/rast-worker.service"
    install_managed_file "$RAST_TMP/rast-worker.service" "$UNIT_DIR/rast-worker.service" 0644 root root
fi

# systemd's ProtectSystem=strict makes everything but /usr, /boot and /etc
# read-only; custom data/storage locations therefore need ReadWritePaths.
if [ "$DATA_DIR" != "$DEFAULT_DATA_DIR" ] || [ "$STORAGE_DIR" != "$DEFAULT_STORAGE_DIR" ]; then
    run install -d -m 0755 "$MAIN_DROPIN_DIR"
    write_paths_dropin "$RAST_TMP/10-rast-paths.conf" "$DATA_DIR" "$STORAGE_DIR"
    install_managed_file "$RAST_TMP/10-rast-paths.conf" "$MAIN_DROPIN_DIR/10-rast-paths.conf" 0644 root root
else
    if [ -f "$MAIN_DROPIN_DIR/10-rast-paths.conf" ]; then
        run rm -f -- "$MAIN_DROPIN_DIR/10-rast-paths.conf"
        log "removed stale custom-path drop-in for rast.service"
    fi
    if [ -d "$MAIN_DROPIN_DIR" ]; then
        run rmdir "$MAIN_DROPIN_DIR" 2>/dev/null || true
    fi
fi

if [ "$WORKER_ENABLED" -eq 1 ]; then
    # Defense in depth: only the web server's uid may connect to the worker
    # socket (the worker enforces this with SO_PEERCRED).
    _rast_uid=$(id -u rast 2>/dev/null || true)
    if [ -n "$_rast_uid" ]; then
        run install -d -m 0755 "$WORKER_DROPIN_DIR"
        {
            printf '[Service]\n'
            printf '# Generated by the RAST installer: accept only the rast web server uid.\n'
            printf 'Environment=RAST_WORKER_ALLOWED_UID=%s\n' "$_rast_uid"
        } > "$RAST_TMP/20-rast-peer-uid.conf"
        install_managed_file "$RAST_TMP/20-rast-peer-uid.conf" "$WORKER_DROPIN_DIR/20-peer-uid.conf" 0644 root root
    else
        warn "could not resolve the uid of the rast user; worker socket peer-uid restriction not installed"
    fi
fi

if [ "$WORKER_ENABLED" -eq 1 ]; then
    if [ "$DATA_DIR" != "$DEFAULT_DATA_DIR" ]; then
        run install -d -m 0755 "$WORKER_DROPIN_DIR"
        write_paths_dropin "$RAST_TMP/10-rast-worker-paths.conf" "$DATA_DIR/apps"
        install_managed_file "$RAST_TMP/10-rast-worker-paths.conf" "$WORKER_DROPIN_DIR/10-rast-paths.conf" 0644 root root
    else
        if [ -f "$WORKER_DROPIN_DIR/10-rast-paths.conf" ]; then
            run rm -f -- "$WORKER_DROPIN_DIR/10-rast-paths.conf"
            log "removed stale custom-path drop-in for rast-worker.service"
        fi
    fi
else
    # Worker disabled: clean up both installer-managed drop-ins.
    if [ -f "$WORKER_DROPIN_DIR/10-rast-paths.conf" ]; then
        run rm -f -- "$WORKER_DROPIN_DIR/10-rast-paths.conf"
        log "removed stale custom-path drop-in for rast-worker.service"
    fi
    if [ -f "$WORKER_DROPIN_DIR/20-peer-uid.conf" ]; then
        run rm -f -- "$WORKER_DROPIN_DIR/20-peer-uid.conf"
        log "removed stale peer-uid drop-in for rast-worker.service"
    fi
    if [ -d "$WORKER_DROPIN_DIR" ]; then
        run rmdir "$WORKER_DROPIN_DIR" 2>/dev/null || true
    fi
fi

run systemctl daemon-reload

# ---------------------------------------------------------------------------
# Enable and start
# ---------------------------------------------------------------------------
if [ "$NO_START" -eq 1 ]; then
    log "skipping service start (--no-start / RAST_NO_START=1)"
    if run systemctl is-active --quiet rast.service; then
        warn "rast.service is still running the previously installed binary; restart it to apply the update: systemctl restart rast.service"
    fi
else
    run systemctl enable rast.service
    if run systemctl is-active --quiet rast.service; then
        run systemctl restart rast.service
        log "restarted rast.service"
    else
        run systemctl start rast.service
        log "started rast.service"
    fi

    if [ "$WORKER_ENABLED" -eq 1 ]; then
        run systemctl enable rast-worker.service
        if run systemctl is-active --quiet rast-worker.service; then
            run systemctl restart rast-worker.service
            log "restarted rast-worker.service"
        else
            run systemctl start rast-worker.service
            if run systemctl is-active --quiet rast-worker.service; then
                log "started rast-worker.service"
            else
                warn "rast-worker.service is installed but inactive (Docker not available yet?)"
                warn "install/start Docker, then: systemctl restart rast-worker.service"
            fi
        fi
    elif [ -f "$UNIT_DIR/rast-worker.service" ]; then
        run systemctl disable --now rast-worker.service 2>/dev/null || true
        log "worker disabled: rast-worker.service stopped and disabled (unit file kept)"
    fi

    if wait_for_health "$BIND"; then
        log "health check passed: $SERVICE_URL (/healthz)"
    else
        die "rast.service was started but /healthz did not answer at $SERVICE_URL within 30 seconds.
Inspect the service logs with:
  journalctl -u rast.service -n 100 --no-pager
Common causes: invalid configuration (run 'rast doctor'), address already in use,
or a missing writable directory."
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [ "$NO_START" -eq 1 ]; then
    SERVICE_STATE=" (installed, not started)"
else
    SERVICE_STATE=" (enabled, running)"
fi

printf '\n'
if [ -n "$_old_version" ] && [ "$_old_version" != "$VERSION" ]; then
    log "RAST updated from $_old_version to $VERSION"
else
    log "RAST $VERSION installed"
fi
printf '  binary:        %s\n' "$BIN_PATH"
printf '  configuration: %s\n' "$CONFIG_FILE"
printf '  data:          %s\n' "$DATA_DIR"
printf '  storage:       %s\n' "$STORAGE_DIR"
printf '  service:       rast.service%s\n' \
    "$SERVICE_STATE"
    printf '  worker:        %s\n' "$WORKER_SUMMARY"
printf '  web UI:        %s\n' "$SERVICE_URL"
printf '\nNext steps:\n'
printf '  1. Create the first administrator (there is no default password):\n'
printf '       sudo -u rast rast user create-admin admin --password-stdin\n'
printf '     Alternatively use the web setup page with a one-shot token:\n'
printf '       sudo -u rast rast setup-token\n'
printf '  2. Put RAST behind TLS: set public_url/trusted_proxies and use nginx,\n'
printf '     Caddy or Cloudflare Tunnel — see docs/INSTALL.md.\n'
printf '  3. Verify the installation at any time:\n'
printf '       sudo -u rast rast doctor\n'
printf '       sudo rast status\n'
printf '\nDocumentation: https://github.com/Enricko/rast/tree/main/docs\n'
printf 'Uninstall:     https://github.com/Enricko/rast/releases/latest/download/uninstall.sh\n'
