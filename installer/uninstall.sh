#!/bin/sh
# RAST uninstaller — Resource Administration & Storage Toolkit
#
# Removes the RAST systemd units and the installed binary. Configuration
# (/etc/rast), the database (/var/lib/rast) and user files (/srv/rast) are
# PRESERVED by default; only --purge deletes them, and --purge asks for a typed
# confirmation.
#
# Usage:
#   curl -fsSL https://github.com/Enricko/rast/releases/latest/download/uninstall.sh | sudo sh
#   sh uninstall.sh --purge
#   sh uninstall.sh --dry-run
#
# Run with --help for all options.
#
# Source: https://github.com/Enricko/rast  License: MIT

set -eu

REPO_SLUG="Enricko/rast"
DEFAULT_DATA_DIR="/var/lib/rast"
DEFAULT_STORAGE_DIR="/srv/rast"
CONFIG_DIR="/etc/rast"
ENV_FILE="/etc/rast/rast.env"
BIN_PATH="/usr/local/bin/rast"
LIB_DIR="/usr/local/lib/rast"
UNIT_DIR="/etc/systemd/system"
RUNTIME_DIR="/run/rast"
MAIN_DROPIN_DIR="/etc/systemd/system/rast.service.d"
WORKER_DROPIN_DIR="/etc/systemd/system/rast-worker.service.d"
UNITS="rast.service rast-worker.service"
LINK_UNITS="rast-packages-links.path rast-packages-links.service"
PROFILE_SCRIPT="/etc/profile.d/rast-packages.sh"

RAST_PURGE=${RAST_PURGE:-0}
RAST_ASSUME_YES=${RAST_ASSUME_YES:-0}
RAST_REMOVE_USER=${RAST_REMOVE_USER:-0}
RAST_KEEP_USER=${RAST_KEEP_USER:-0}
RAST_DRY_RUN=${RAST_DRY_RUN:-0}
RAST_SUDO=${RAST_SUDO:-}

SUDO=""
PARSED_BOOL=""

umask 0022

log() { printf '[rast] %s\n' "$*"; }
warn() { printf '[rast] WARNING: %s\n' "$*" >&2; }
die() {
    printf '[rast] ERROR: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'RAST_USAGE'
RAST uninstaller

Usage:
  uninstall.sh [options]

Options:
  --purge          ALSO delete /etc/rast, the data directory (default
                   /var/lib/rast) and the storage directory (default /srv/rast).
                   Requires an interactive typed confirmation.
  --yes, -y        Skip the typed confirmation for --purge (unattended use).
  --keep-user      Keep the rast/rast-worker service accounts (default).
  --remove-user    Delete the service accounts after removing the service.
                   Preserved files keep their numeric UID.
  --dry-run        Show what would be removed without changing anything.
  -h, --help       Show this help and exit.

Environment variables:
  RAST_PURGE=1          Same as --purge.
  RAST_ASSUME_YES=1     Same as --yes.
  RAST_REMOVE_USER=1    Same as --remove-user.
  RAST_DRY_RUN=1        Same as --dry-run.
  RAST_SUDO             Command used to elevate privileges (default: sudo).

By default the following is preserved:
  /etc/rast/ (configuration), /var/lib/rast/ (database, secrets, backups) and
  /srv/rast/ (user files). Re-installing RAST will pick them up again.
RAST_USAGE
}

rast_cleanup() {
    _rc=$?
    if [ "$_rc" -ne 0 ]; then
        printf '\n[rast] Uninstall did not complete (exit %s).\n' "$_rc" >&2
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
        --purge) RAST_PURGE=1 ;;
        --yes | -y) RAST_ASSUME_YES=1 ;;
        --keep-user) RAST_KEEP_USER=1 ;;
        --remove-user) RAST_REMOVE_USER=1 ;;
        --dry-run) RAST_DRY_RUN=1 ;;
        *) die "unknown option: $1 (run with --help for usage)" ;;
    esac
    shift
done

parse_bool() {
    # $1 label, $2 raw value; sets PARSED_BOOL.
    case "$2" in
        1 | true | TRUE | True | yes | YES | Yes | on | ON) PARSED_BOOL=1 ;;
        0 | false | FALSE | False | no | NO | No | off | OFF | "") PARSED_BOOL=0 ;;
        *) die "invalid $1=$2 (expected 1/true/yes/on or 0/false/no/off)" ;;
    esac
}

parse_bool "RAST_PURGE" "$RAST_PURGE"
PURGE=$PARSED_BOOL
parse_bool "RAST_ASSUME_YES" "$RAST_ASSUME_YES"
ASSUME_YES=$PARSED_BOOL
parse_bool "RAST_REMOVE_USER" "$RAST_REMOVE_USER"
REMOVE_USER=$PARSED_BOOL
parse_bool "RAST_KEEP_USER" "$RAST_KEEP_USER"
KEEP_USER=$PARSED_BOOL
parse_bool "RAST_DRY_RUN" "$RAST_DRY_RUN"
DRY_RUN=$PARSED_BOOL

if [ "$KEEP_USER" -eq 1 ] && [ "$REMOVE_USER" -eq 1 ]; then
    die "--keep-user and --remove-user are mutually exclusive"
fi
if [ "$KEEP_USER" -eq 1 ]; then
    REMOVE_USER=0
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
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

have_systemd() {
    command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]
}

config_value() {
    # Read `key = "value"` from rast.toml without a TOML parser.
    [ -f "$CONFIG_DIR/rast.toml" ] || return 0
    awk -v key="$1" '
        $1 == key && $2 == "=" {
            line = $0
            sub(/^[^"]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' "$CONFIG_DIR/rast.toml"
}

env_value() {
    # Read `KEY=value` from rast.env (last assignment wins).
    [ -f "$ENV_FILE" ] || return 0
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" "$ENV_FILE" |
        tail -n 1 |
        sed -e 's/[[:space:]]*$//' -e 's/^"//' -e 's/"$//'
}

strip_slash() {
    _p=${1%/}
    printf '%s\n' "${_p:-/}"
}

is_purgeable_path() {
    case "$1" in
        "" | / | /etc | /var | /srv | /usr | /usr/local | /var/lib | /srv/rast/) return 1 ;;
        /*) return 0 ;;
        *) return 1 ;;
    esac
}

remove_file() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            log "dry run: would remove file $1"
        else
            run rm -f -- "$1"
            log "removed file: $1"
        fi
    fi
}

remove_tree() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            log "dry run: would remove directory $1"
        else
            run rm -rf -- "$1"
            log "removed directory: $1"
        fi
    fi
}

systemctl_maybe() {
    if have_systemd; then
        run systemctl "$@" >/dev/null 2>&1 || true
    fi
}

# ---------------------------------------------------------------------------
# Inspection (read-only, also used by --dry-run)
# ---------------------------------------------------------------------------
DATA_DIR=$(config_value data_dir)
[ -n "$DATA_DIR" ] || DATA_DIR=$(env_value RAST_DATA_DIR)
DATA_DIR=$(strip_slash "${DATA_DIR:-$DEFAULT_DATA_DIR}")

STORAGE_DIR=$(config_value storage_dir)
[ -n "$STORAGE_DIR" ] || STORAGE_DIR=$(env_value RAST_STORAGE_DIR)
STORAGE_DIR=$(strip_slash "${STORAGE_DIR:-$DEFAULT_STORAGE_DIR}")

CUSTOM_CONFIG=$(env_value RAST_CONFIG)
if [ -n "$CUSTOM_CONFIG" ] && [ "${CUSTOM_CONFIG%/}" != "$CONFIG_DIR/rast.toml" ]; then
    warn "rast.env points RAST_CONFIG at $CUSTOM_CONFIG; this uninstaller only manages $CONFIG_DIR/rast.toml"
fi

if [ "$PURGE" -eq 1 ] && [ "$DRY_RUN" -eq 0 ] && [ "$ASSUME_YES" -eq 0 ]; then
    printf '\n[rast] WARNING: --purge permanently deletes configuration and user data:\n' >&2
    printf '         %s\n' "$CONFIG_DIR" >&2
    printf '         %s\n' "$DATA_DIR" >&2
    printf '         %s\n' "$STORAGE_DIR" >&2
    printf '         There is no undo unless you have a backup.\n\n' >&2
    printf 'Type PURGE to continue: ' >&2
    _confirm=""
    read -r _confirm || _confirm=""
    if [ "$_confirm" != "PURGE" ]; then
        die "purge aborted (confirmation did not match 'PURGE')"
    fi
fi

if [ "$DRY_RUN" -eq 0 ]; then
    setup_privilege
else
    if [ "$(id -u)" -ne 0 ]; then
        log "dry run: would use sudo/root privileges for system changes"
    fi
fi

log "RAST uninstaller$([ "$DRY_RUN" -eq 1 ] && printf ' (dry run)')"
log "data directory:    $DATA_DIR"
log "storage directory: $STORAGE_DIR"
log "purge data:        $([ "$PURGE" -eq 1 ] && printf yes || printf no)"
log "service accounts:  $([ "$REMOVE_USER" -eq 1 ] && printf 'remove' || printf 'keep')"

# ---------------------------------------------------------------------------
# Package Center system-wide commands (packages_system_wide): stop the
# watcher, remove RAST's links in /usr/local/bin and put back what it moved
# aside. Only files RAST wrote are touched.
# ---------------------------------------------------------------------------
LINKS_BIN="/usr/local/bin"
PACKAGES_BIN="$DATA_DIR/packages/bin"
for _unit in $LINK_UNITS; do
    if [ -f "$UNIT_DIR/$_unit" ] && head -n 1 "$UNIT_DIR/$_unit" | grep -q '^# Managed by RAST'; then
        if [ "$DRY_RUN" -eq 1 ]; then
            log "dry run: would stop, disable and remove $_unit"
        else
            systemctl_maybe disable --now "$_unit"
            remove_file "$UNIT_DIR/$_unit"
        fi
    fi
done
for _link in "$LINKS_BIN"/*; do
    [ -L "$_link" ] || continue
    _name=${_link##*/}
    if [ "$(readlink "$_link")" = "$PACKAGES_BIN/$_name" ]; then
        remove_file "$_link"
    fi
done
for _aside in "$LINKS_BIN"/.rast-displaced.*; do
    [ -e "$_aside" ] || [ -L "$_aside" ] || continue
    _name=${_aside##*/.rast-displaced.}
    if [ -e "$LINKS_BIN/$_name" ] || [ -L "$LINKS_BIN/$_name" ]; then
        warn "kept $_aside: $LINKS_BIN/$_name exists again"
    elif [ "$DRY_RUN" -eq 1 ]; then
        log "dry run: would put $LINKS_BIN/$_name back"
    else
        run mv -- "$_aside" "$LINKS_BIN/$_name"
        log "put back: $LINKS_BIN/$_name"
    fi
done

# ---------------------------------------------------------------------------
# Stop and remove services
# ---------------------------------------------------------------------------
for _unit in $UNITS; do
    if have_systemd; then
        if [ "$DRY_RUN" -eq 1 ]; then
            log "dry run: would stop and disable $_unit"
        else
            systemctl_maybe disable --now "$_unit"
            log "stopped and disabled $_unit (if present)"
        fi
    fi
done

if have_systemd; then
    for _unit in $UNITS; do
        remove_file "$UNIT_DIR/$_unit"
    done
    if [ "$DRY_RUN" -eq 0 ]; then
        run systemctl daemon-reload >/dev/null 2>&1 || true
        for _unit in $UNITS; do
            run systemctl reset-failed "$_unit" >/dev/null 2>&1 || true
        done
        log "reloaded the systemd manager configuration"
    fi
fi

remove_file "$MAIN_DROPIN_DIR/10-rast-paths.conf"
if [ -d "$MAIN_DROPIN_DIR" ] && [ "$DRY_RUN" -eq 0 ]; then
    run rmdir "$MAIN_DROPIN_DIR" 2>/dev/null || true
fi
remove_file "$WORKER_DROPIN_DIR/10-rast-paths.conf"
remove_file "$WORKER_DROPIN_DIR/20-peer-uid.conf"
if [ -d "$WORKER_DROPIN_DIR" ] && [ "$DRY_RUN" -eq 0 ]; then
    run rmdir "$WORKER_DROPIN_DIR" 2>/dev/null || true
fi

remove_file "$RUNTIME_DIR/worker.sock"
# Package Center's login-shell PATH entry (only the script RAST wrote).
if [ -f "$PROFILE_SCRIPT" ] && head -n 1 "$PROFILE_SCRIPT" | grep -q '^# Managed by RAST'; then
    remove_file "$PROFILE_SCRIPT"
fi
remove_tree "$RUNTIME_DIR"

# ---------------------------------------------------------------------------
# Remove the binary
# ---------------------------------------------------------------------------
remove_file "$BIN_PATH"
remove_file "$BIN_PATH.new"
remove_tree "$LIB_DIR"

# ---------------------------------------------------------------------------
# Optional: delete configuration and data
# ---------------------------------------------------------------------------
if [ "$PURGE" -eq 1 ]; then
    for _path in "$CONFIG_DIR" "$DATA_DIR" "$STORAGE_DIR"; do
        if is_purgeable_path "$_path"; then
            remove_tree "$_path"
        else
            warn "refusing to purge unsafe path '$_path' — remove it manually if intended"
        fi
    done
fi

# ---------------------------------------------------------------------------
# Optional: remove service accounts
# ---------------------------------------------------------------------------
if [ "$REMOVE_USER" -eq 1 ]; then
    for _user in rast-worker rast; do
        if id "$_user" >/dev/null 2>&1; then
            _uid=$(id -u "$_user" 2>/dev/null || true)
            if [ "$DRY_RUN" -eq 1 ]; then
                log "dry run: would delete service account $_user (uid ${_uid:-unknown})"
            else
                if run userdel "$_user" 2>/dev/null; then
                    log "deleted service account: $_user (was uid ${_uid:-unknown})"
                else
                    warn "could not delete service account $_user; remove it manually with: userdel $_user"
                fi
            fi
        fi
    done
    if [ "$PURGE" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
        warn "data directories were preserved but their files keep the deleted numeric UID"
        warn "chown them to a recreated account (or a new owner) before re-installing:"
        warn "  groupadd --system rast"
        warn "  useradd --system --gid rast --home-dir $DATA_DIR --no-create-home rast"
        warn "  chown -R rast:rast $DATA_DIR $STORAGE_DIR"
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
printf '\n'
if [ "$DRY_RUN" -eq 1 ]; then
    log "dry run complete — nothing was changed"
else
    log "RAST uninstall complete"
fi
printf '  removed:  %s, %s (if present)\n' "$UNIT_DIR/rast.service" "$UNIT_DIR/rast-worker.service"
printf '            %s\n' "$BIN_PATH"
printf '  removed:  %s (runtime socket directory)\n' "$RUNTIME_DIR"
if [ "$PURGE" -eq 1 ]; then
    printf '  deleted:  %s\n' "$CONFIG_DIR"
    printf '            %s\n' "$DATA_DIR"
    printf '            %s\n' "$STORAGE_DIR"
else
    printf '  kept:     %s (configuration)\n' "$CONFIG_DIR"
    printf '            %s (database, secrets, backups)\n' "$DATA_DIR"
    printf '            %s (user files)\n' "$STORAGE_DIR"
    if [ -n "$CUSTOM_CONFIG" ] && [ "${CUSTOM_CONFIG%/}" != "$CONFIG_DIR/rast.toml" ]; then
        printf '            %s (custom configuration referenced by rast.env)\n' "$CUSTOM_CONFIG"
    fi
    printf '\n  Re-install with the installer to reuse this data, or delete it\n'
    printf '  permanently by re-running this script with --purge.\n'
fi
if [ "$REMOVE_USER" -eq 1 ]; then
    printf '  service accounts rast/rast-worker were deleted\n'
else
    printf '  service accounts: kept (use --remove-user to delete them)\n'
fi
printf '\nDocumentation: https://github.com/%s/tree/main/docs\n' "$REPO_SLUG"
