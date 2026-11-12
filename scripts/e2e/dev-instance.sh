#!/usr/bin/env bash
# RAST end-to-end dev instance manager.
#
# Runs a disposable RAST instance *next to* a production installation:
#   bind          127.0.0.1:8731 (8720 is production; 8721 is the host
#                 nginx reverse proxy for it, so the dev instance uses 8731)
#   data          /tmp/rast-e2e/data       (owned by the `rast` service user)
#   storage       /tmp/rast-e2e/storage    (owned by the `rast` service user)
#   config        /tmp/rast-e2e/rast.toml
#   worker        `rast worker` (docker mode) on /tmp/rast-e2e/run-worker/worker.sock
#   native helper `rast native-helper` (native mode, root) on /tmp/rast-e2e/run-native/native.sock
#
# It never touches /var/lib/rast, /etc/rast or rast.service/rast-worker.service.
# The worker is a *private* instance because the dev config stages app files
# under /tmp/rast-e2e/data/linux-apps: the production worker would reject them
# (and its binary predates the migration's worker operations).
#
# Usage:
#   scripts/e2e/dev-instance.sh up [docker|native]   start (switch mode if given)
#   scripts/e2e/dev-instance.sh down                  stop everything
#   scripts/e2e/dev-instance.sh restart [mode]        down + up
#   scripts/e2e/dev-instance.sh status                show what is running
#   scripts/e2e/dev-instance.sh reset [mode]          wipe data/storage and start fresh
#   scripts/e2e/dev-instance.sh restart-server        restart only `rast serve`
#                                                     (worker/helper keep running)
#   scripts/e2e/dev-instance.sh logs [server|worker|helper] [-f]
#   scripts/e2e/dev-instance.sh creds                 print the credentials file path
#   scripts/e2e/dev-instance.sh health                curl /healthz
#   scripts/e2e/dev-instance.sh shell                 interactive shell as the server user
#   scripts/e2e/dev-instance.sh fix-perms             repair native ownership/modes (0750/0711)
#   scripts/e2e/dev-instance.sh native-layout         (re)create the native data layout only
#   scripts/e2e/dev-instance.sh build                 build the snapshot binary + frontend
#
# Environment overrides:
#   RAST_E2E_ROOT   (default /tmp/rast-e2e)
#   RAST_E2E_BIN    (default $RAST_E2E_ROOT/bin/rast)
#   RAST_E2E_BIND   (default 127.0.0.1:8731; 8721 is taken by nginx on this host)
#
# All state is disposable. The production instance is never restarted or
# modified; run as the normal operator account (sudo is used for the service
# user and for root-only helpers).
set -euo pipefail

ROOT="${RAST_E2E_ROOT:-/tmp/rast-e2e}"
BIN="${RAST_E2E_BIN:-$ROOT/bin/rast}"
BIND="${RAST_E2E_BIND:-127.0.0.1:8731}"
BASE_URL="http://$BIND"
CONFIG="$ROOT/rast.toml"
DATA="$ROOT/data"
STORAGE="$ROOT/storage"
LOGS="$ROOT/logs"
PID_DIR="$ROOT/pids"
WORKER_RUN_DIR="$ROOT/run-worker"
WORKER_SOCK="$WORKER_RUN_DIR/worker.sock"
NATIVE_RUN_DIR="$ROOT/run-native"
NATIVE_SOCK="$NATIVE_RUN_DIR/native.sock"
CREDS="$ROOT/credentials.env"
MODE_FILE="$ROOT/mode"
SERVER_USER="${RAST_E2E_SERVER_USER:-rast}"
SERVER_LOG="$LOGS/server.log"
WORKER_LOG="$LOGS/worker.log"
HELPER_LOG="$LOGS/helper.log"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
ENV_LOCAL="$REPO_DIR/tests/e2e/.env.local"

# Test accounts. Passwords are generated once, kept in $CREDS (mode 0600, not
# in the repository) and mirrored to tests/e2e/.env.local for Playwright.
ADMIN_USER=admin
USER1=alice
USER2=bob

say() { printf '\033[1;34m[e2e]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[e2e]\033[0m WARNING: %s\n' "$*" >&2; }
die() { printf '\033[1;31m[e2e]\033[0m ERROR: %s\n' "$*" >&2; exit 1; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

server_uid() { id -u "$SERVER_USER" 2>/dev/null || echo 0; }

# root-owned processes cannot be probed with a plain `kill -0` from the
# operator account, so liveness checks go through sudo (which is already
# required to manage this instance).
pid_of() {
  local pidfile="$1"
  [ -f "$pidfile" ] || return 1
  local pid
  pid=$(cat "$pidfile" 2>/dev/null || true)
  case "$pid" in '' | *[!0-9]*) return 1 ;; esac
  sudo kill -0 "$pid" 2>/dev/null || return 1
  printf '%s' "$pid"
}

# Kill any process left behind by a crashed earlier run, matched by the
# private config path so production (`rist`/`rast.service`) is never touched.
cleanup_strays() {
  local pattern="$BIN --config $CONFIG"
  local pids
  pids=$(pgrep -f -- "$pattern" 2>/dev/null || true)
  if [ -n "${pids:-}" ]; then
    say "stopping stray dev processes: $(echo "$pids" | tr '\n' ' ')"
    # shellcheck disable=SC2086
    sudo kill -TERM $pids 2>/dev/null || true
    sleep 1
    # shellcheck disable=SC2086
    sudo kill -KILL $pids 2>/dev/null || true
  fi
}

# Wait until a pid file points at a live process.
wait_pid_file() {
  local pidfile="$1" what="$2" tries="${3:-100}"
  local i
  for ((i = 0; i < tries; i++)); do
    if pid_of "$pidfile" >/dev/null; then return 0; fi
    sleep 0.1
  done
  return 1
}

wait_socket() {
  local path="$1" what="$2" tries="${3:-100}"
  local i
  for ((i = 0; i < tries; i++)); do
    # The socket directory is 0750 root:<server group>; the operator account
    # cannot traverse it, so probe through sudo.
    sudo test -S "$path" && return 0
    sleep 0.1
  done
  warn "$what socket did not appear at $path"
  return 1
}

wait_http() {
  local tries="${1:-150}"
  local i
  for ((i = 0; i < tries; i++)); do
    if curl -fsS --max-time 2 "$BASE_URL/healthz" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  return 1
}

require_binary() {
  [ -x "$BIN" ] || die "snapshot binary not found at $BIN — run: scripts/e2e/build-snapshot.sh"
}

# ---------------------------------------------------------------------------
# credentials
# ---------------------------------------------------------------------------
gen_password() {
  # 24 chars from a safe alphabet; satisfies the password policy (>= 10).
  LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 24 || true
}

ensure_creds() {
  if [ -f "$CREDS" ]; then
    # shellcheck disable=SC1090
    . "$CREDS"
    return 0
  fi
  local a u1 u2
  a=$(gen_password); u1=$(gen_password); u2=$(gen_password)
  umask 077
  cat >"$CREDS" <<EOF
# Generated by scripts/e2e/dev-instance.sh — do not commit.
RAST_E2E_BASE_URL=$BASE_URL
RAST_E2E_ADMIN_USER=$ADMIN_USER
RAST_E2E_ADMIN_PASSWORD=$a
RAST_E2E_USER1_USER=$USER1
RAST_E2E_USER1_PASSWORD=$u1
RAST_E2E_USER2_USER=$USER2
RAST_E2E_USER2_PASSWORD=$u2
EOF
  chmod 600 "$CREDS"
  umask 022
  # shellcheck disable=SC1090
  . "$CREDS"
  say "generated test credentials in $CREDS"
}

mirror_creds_to_repo() {
  # The repo's tests/e2e/.env.local is the shared default-instance handoff used
  # by the Playwright harness. A non-default instance (RAST_E2E_ROOT) must not
  # overwrite it, or two parallel instances would point runs at each other.
  if [ "$ROOT" != "/tmp/rast-e2e" ]; then
    umask 077
    cp "$CREDS" "$ROOT/env.local"
    chmod 600 "$ROOT/env.local"
    say "non-default instance: credentials in $CREDS (repo .env.local left untouched)"
    return 0
  fi
  mkdir -p "$(dirname "$ENV_LOCAL")"
  (umask 077; cp "$CREDS" "$ENV_LOCAL")
  chmod 600 "$ENV_LOCAL"
  # .env.local matches the repository .gitignore; verify and warn loudly if not.
  if command -v git >/dev/null 2>&1 && ! git -C "$REPO_DIR" check-ignore -q "$ENV_LOCAL" 2>/dev/null; then
    warn "$ENV_LOCAL is NOT git-ignored — do not commit it, or add it to .gitignore"
  fi
}

# ---------------------------------------------------------------------------
# configuration
# ---------------------------------------------------------------------------
render_config() {
  local mode="$1"
  local native_enabled=false
  [ "$mode" = native ] && native_enabled=true
  mkdir -p "$ROOT"
  cat >"$CONFIG" <<EOF
# RAST E2E dev instance — generated by scripts/e2e/dev-instance.sh.
# Disposable. Never used by a production installation.
bind = "$BIND"
public_url = "$BASE_URL"
data_dir = "$DATA"
storage_dir = "$STORAGE"
secure_cookies = false
setup_token_required = false
trust_forwarded_headers = false
job_concurrency = 2
monitor_interval_secs = 2
max_upload_bytes = 2147483648
packages_check_interval_minutes = 0

# Docker backend (legacy baseline). The private worker is started by the same
# script; the production worker socket is deliberately not used.
docker_enabled = true
worker_socket = "$WORKER_SOCK"

# Native backend (one shared Xpra session per RAST user).
native_enabled = $native_enabled
native_helper_socket = "$NATIVE_SOCK"
linux_xpra_bin = "/usr/bin/xpra"
linux_uid_base = 210000
linux_idle_minutes = 2
linux_session_memory_mb = 4096
linux_session_cpus = 2
linux_session_tasks = 4096
linux_app_memory_mb = 2048
linux_app_cpus = 1
linux_app_tasks = 2048
linux_admission_reserve_mb = 512
EOF
  chmod 644 "$CONFIG"
}

mode_of() {
  if [ -f "$MODE_FILE" ]; then cat "$MODE_FILE"; else echo docker; fi
}

# ---------------------------------------------------------------------------
# process control
# ---------------------------------------------------------------------------
# start_<user> <logfile> <pidfile> <cmd...>
# The child's own pid is written from inside the elevated shell, so stop works
# even though sudo itself is not the process we want to signal.
start_as() {
  local user="$1" logfile="$2" pidfile="$3"
  shift 3
  mkdir -p "$PID_DIR" "$LOGS"
  sudo rm -f "$pidfile"
  if [ "$user" = root ]; then
    sudo bash -c 'echo $$ >"$1"; shift; exec "$@"' bash "$pidfile" "$@" >>"$logfile" 2>&1 &
  else
    sudo -u "$user" bash -c 'echo $$ >"$1"; shift; exec "$@"' bash "$pidfile" "$@" >>"$logfile" 2>&1 &
  fi
}

# Debug builds serve the frontend from <repo>/frontend/dist at runtime (the
# release binary embeds it). The server runs as $SERVER_USER, which usually
# cannot traverse the operator's home directory: add an ACL traverse entry
# where needed instead of loosening directory modes.
ensure_asset_access() {
  local root="$REPO_DIR/frontend/dist"
  [ -d "$root" ] || return 0
  local path="$root" need=()
  while [ "$path" != "/" ] && [ -n "$path" ]; do
    if ! sudo -u "$SERVER_USER" test -x "$path" 2>/dev/null; then need+=("$path"); fi
    path=$(dirname "$path")
  done
  if [ "${#need[@]}" -gt 0 ]; then
    say "granting $SERVER_USER traverse access to frontend assets (ACL)"
    local entry
    for entry in "${need[@]}"; do
      sudo setfacl -m "u:$SERVER_USER:x" "$entry" 2>/dev/null ||
        warn "cannot add an ACL on $entry; debug asset serving may fail"
    done
  fi
  if ! sudo -u "$SERVER_USER" test -r "$root/index.html" 2>/dev/null; then
    warn "$root/index.html is not readable by $SERVER_USER (build the frontend or fix permissions)"
  fi
}

start_server() {
  require_binary
  ensure_asset_access
  say "starting RAST server on $BASE_URL (data: $DATA)"
  start_as "$SERVER_USER" "$SERVER_LOG" "$PID_DIR/server.pid" \
    "$BIN" --config "$CONFIG" serve
  if ! wait_pid_file "$PID_DIR/server.pid" "server" 100; then
    tail -n 30 "$SERVER_LOG" >&2 || true
    die "the dev server failed to start"
  fi
  if ! wait_http 150; then
    tail -n 40 "$SERVER_LOG" >&2 || true
    die "the dev server did not become healthy at $BASE_URL/healthz"
  fi
  say "server healthy (pid $(cat "$PID_DIR/server.pid"))"
}

start_worker() {
  require_binary
  say "starting private Docker worker on $WORKER_SOCK"
  start_as root "$WORKER_LOG" "$PID_DIR/worker.pid" \
    env RAST_WORKER_SOCKET_GROUP="$SERVER_USER" \
    RAST_WORKER_ALLOWED_UID="$(server_uid)" \
    "$BIN" --config "$CONFIG" worker
  if ! wait_pid_file "$PID_DIR/worker.pid" "worker" 100; then
    tail -n 30 "$WORKER_LOG" >&2 || true
    die "the dev worker failed to start"
  fi
  wait_socket "$WORKER_SOCK" "worker" 100 || warn "docker apps will stay unavailable"
  say "worker ready (pid $(cat "$PID_DIR/worker.pid"))"
}

helper_supported() {
  # `native-helper` does not exist until the native backend lands; --help exits
  # 0 for a known subcommand and non-zero for an unknown one.
  "$BIN" native-helper --help >/dev/null 2>&1
}

install_native_prerequisites() {
  # The helper's transient units live in rast-linux.slice (aggregate cgroup);
  # without it every `systemd-run --slice=rast-linux.slice` fails.
  local slice="$REPO_DIR/deploy/rast-linux.slice"
  if [ -f "$slice" ]; then
    if ! cmp -s "$slice" /etc/systemd/system/rast-linux.slice 2>/dev/null; then
      say "installing $slice"
      sudo install -m 0644 "$slice" /etc/systemd/system/rast-linux.slice
      sudo systemctl daemon-reload
    fi
  else
    warn "deploy/rast-linux.slice is missing; native session units will fail"
  fi
  # Xpra needs the X11 socket directory to be world-writable (normally 1777).
  if [ -d /tmp/.X11-unix ]; then
    local mode
    mode=$(stat -c '%a' /tmp/.X11-unix 2>/dev/null || echo "")
    case "$mode" in
      1777) ;;
      *) sudo chmod 1777 /tmp/.X11-unix 2>/dev/null || warn "/tmp/.X11-unix is mode $mode and could not be fixed" ;;
    esac
  fi
}

start_helper() {
  require_binary
  if ! helper_supported; then
    warn "this snapshot has no 'native-helper' subcommand yet; native launches will fail"
    return 1
  fi
  install_native_prerequisites
  say "checking the native helper environment"
  sudo "$BIN" --config "$CONFIG" native-helper --check >>"$HELPER_LOG" 2>&1 ||
    warn "native helper environment check reported problems (see $HELPER_LOG)"
  say "starting root native helper on $NATIVE_SOCK"
  start_as root "$HELPER_LOG" "$PID_DIR/helper.pid" \
    "$BIN" --config "$CONFIG" native-helper
  if ! wait_pid_file "$PID_DIR/helper.pid" "helper" 100; then
    tail -n 40 "$HELPER_LOG" >&2 || true
    warn "the native helper failed to start (native launches will fail)"
    return 1
  fi
  if ! wait_socket "$NATIVE_SOCK" "native helper" 100; then
    return 1
  fi
  say "native helper ready (pid $(cat "$PID_DIR/helper.pid"))"
  return 0
}

stop_one() {
  local name="$1" pidfile="$2" as_root="$3"
  local pid
  if pid=$(pid_of "$pidfile"); then
    say "stopping $name (pid $pid)"
    if [ "$as_root" = root ]; then sudo kill -TERM "$pid" 2>/dev/null || true; else sudo -u "$SERVER_USER" kill -TERM "$pid" 2>/dev/null || true; fi
    local i
    for ((i = 0; i < 100; i++)); do
      sudo kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    if sudo kill -0 "$pid" 2>/dev/null; then
      warn "$name did not stop in 10s; sending SIGKILL"
      if [ "$as_root" = root ]; then sudo kill -KILL "$pid" 2>/dev/null || true; else sudo -u "$SERVER_USER" kill -KILL "$pid" 2>/dev/null || true; fi
    fi
  fi
  sudo rm -f "$pidfile"
}

stop_all() {
  stop_one server "$PID_DIR/server.pid" user
  stop_one helper "$PID_DIR/helper.pid" root
  stop_one worker "$PID_DIR/worker.pid" root
  sudo rm -f "$WORKER_SOCK" "$NATIVE_SOCK"
}

# ---------------------------------------------------------------------------
# bootstrap: admin + regular users
# ---------------------------------------------------------------------------
create_admin_cli() {
  if sudo -u "$SERVER_USER" test -f "$DATA/rast.db"; then
    return 0
  fi
  say "creating the first administrator via the CLI"
  # The CLI applies migrations and creates the DB; another instance must not be
  # running on this data directory (we are before `serve` starts).
  sudo -u "$SERVER_USER" env RAST_DATA_DIR="$DATA" RAST_STORAGE_DIR="$STORAGE" \
    "$BIN" --config "$CONFIG" user create-admin "$ADMIN_USER" --password "$RAST_E2E_ADMIN_PASSWORD"
}

api_login() { # user password cookie_jar -> prints csrf token
  local user="$1" password="$2" jar="$3"
  local body
  body=$(jq -nc --arg u "$user" --arg p "$password" '{username:$u,password:$p}')
  local response http
  response=$(curl -sS -c "$jar" -H 'content-type: application/json' -d "$body" \
    -w '\n%{http_code}' "$BASE_URL/api/auth/login")
  http=$(printf '%s' "$response" | tail -n 1)
  response=$(printf '%s' "$response" | sed '$d')
  [ "$http" = 200 ] || { printf '%s\n' "$response" >&2; return 1; }
  printf '%s' "$response" | jq -r '.csrfToken'
}

api_user_id() { # jar csrf username -> prints id or empty
  local jar="$1" csrf="$2" user="$3"
  curl -sS -b "$jar" -H "x-csrf-token: $csrf" \
    "$BASE_URL/api/users?q=$user&perPage=100" |
    jq -r --arg u "$user" '.items[] | select(.username == $u) | .id' | head -n 1
}

ensure_user() { # jar csrf username password display role
  local jar="$1" csrf="$2" user="$3" password="$4" display="$5" role="${6:-user}"
  local body
  body=$(jq -nc --arg u "$user" --arg p "$password" --arg d "$display" --arg r "$role" \
    '{username:$u,password:$p,displayName:$d,role:$r,mustChangePassword:false}')
  local response http
  response=$(curl -sS -b "$jar" -H "x-csrf-token: $csrf" -H 'content-type: application/json' \
    -d "$body" -w '\n%{http_code}' "$BASE_URL/api/users")
  http=$(printf '%s' "$response" | tail -n 1)
  response=$(printf '%s' "$response" | sed '$d')
  case "$http" in
    200 | 201) say "created account $user"; return 0 ;;
    409) ;; # exists: fall through to a password reset so credentials stay true
    *) warn "creating $user failed ($http): $response"; return 1 ;;
  esac
  local id
  id=$(api_user_id "$jar" "$csrf" "$user")
  [ -n "$id" ] || { warn "cannot find existing account $user"; return 1; }
  response=$(curl -sS -b "$jar" -H "x-csrf-token: $csrf" -H 'content-type: application/json' \
    -d "$(jq -nc --arg p "$password" '{password:$p}')" \
    -w '\n%{http_code}' "$BASE_URL/api/users/$id/password")
  http=$(printf '%s' "$response" | tail -n 1)
  [ "$http" = 200 ] || [ "$http" = 204 ] || { warn "resetting $user password failed ($http)"; return 1; }
  # The admin password reset forces a password change by default, which would
  # lock automated sessions out of every endpoint; test accounts stay usable.
  response=$(curl -sS -b "$jar" -H "x-csrf-token: $csrf" -H 'content-type: application/json' \
    -X PATCH -d '{"mustChangePassword":false}' \
    -w '\n%{http_code}' "$BASE_URL/api/users/$id")
  http=$(printf '%s' "$response" | tail -n 1)
  [ "$http" = 200 ] || warn "clearing mustChangePassword for $user failed ($http)"
  say "reset existing account $user"
}

bootstrap_users() {
  local jar="$ROOT/.admin.cookies"
  local csrf
  csrf=$(api_login "$ADMIN_USER" "$RAST_E2E_ADMIN_PASSWORD" "$jar") || die "admin login failed — is the config/admin correct?"
  ensure_user "$jar" "$csrf" "$USER1" "$RAST_E2E_USER1_PASSWORD" "Alice E2E" user || true
  ensure_user "$jar" "$csrf" "$USER2" "$RAST_E2E_USER2_PASSWORD" "Bob E2E" user || true
  rm -f "$jar"
}

# Layout the native split expects. Production creates these with tmpfiles;
# the dev instance has no tmpfiles, and the root helper's bootstrap may have
# created some of them root-owned, so this is idempotent and repairs ownership.
cmd_native_layout() {
  sudo install -d -o "$SERVER_USER" -g "$SERVER_USER" -m 0750 "$DATA" "$DATA/linux-apps" 2>/dev/null || true
  sudo install -d -o "$SERVER_USER" -g "$SERVER_USER" -m 0750 \
    "$DATA/linux-apps/staging" "$DATA/linux-apps/apps" 2>/dev/null || true
  sudo install -d -o root -g "$SERVER_USER" -m 0711 "$DATA/linux-apps/profiles" 2>/dev/null || true
  sudo install -d -o root -g "$SERVER_USER" -m 0711 "$DATA/linux-users" 2>/dev/null || true
  # The server creates its staging subdirectories inside linux-apps.
  sudo chown "$SERVER_USER:$SERVER_USER" "$DATA/linux-apps" 2>/dev/null || true
  sudo chmod 0750 "$DATA/linux-apps" 2>/dev/null || true
  say "native data layout ready ($DATA/linux-apps 0750 $SERVER_USER, staging/apps 0750, profiles/linux-users 0711 root:$SERVER_USER)"
}

# The native helper creates profiles 0700 for the identity but its ancestors
# (<data>/linux-apps, <data>/linux-apps/profiles, <data>/linux-users) must be
# traversable by that identity. Production precedent: /var/lib/rast is 0711.
cmd_fix_perms() {
  cmd_native_layout
  sudo chmod 711 "$DATA" "$STORAGE" 2>/dev/null || true
  local dir
  for dir in "$DATA/linux-users" "$DATA/linux-apps" "$DATA/linux-apps/profiles"; do
    if sudo test -d "$dir"; then
      sudo chmod 711 "$dir" || warn "cannot chmod 711 $dir"
    fi
  done
  # Per-user profile parents (profiles/<app>/<user>) and identity homes.
  if sudo test -d "$DATA/linux-apps/profiles"; then
    while IFS= read -r dir; do
      [ -n "$dir" ] && sudo chmod 711 "$dir" 2>/dev/null || true
    done < <(sudo find "$DATA/linux-apps/profiles" -mindepth 1 -maxdepth 2 -type d 2>/dev/null)
  fi
  if sudo test -d "$DATA/linux-users"; then
    while IFS= read -r dir; do
      [ -n "$dir" ] || continue
      # The identity's home must be owned by its own account, 0700 (xpra
      # stores .Xauthority there and needs to write it). The directory name is
      # the RAST user id, NOT an account name, so resolve the owner from the
      # passwd home field (a numeric `id -u 2` would resolve to the system
      # "bin" account!).
      local owner
      owner=$(getent passwd | awk -F: -v home="$dir" '$6 == home { print $1; exit }')
      if [ -n "$owner" ]; then
        sudo chown "$owner:$owner" "$dir" 2>/dev/null || true
        sudo chmod 700 "$dir" 2>/dev/null || true
      else
        sudo chmod 711 "$dir" 2>/dev/null || true
      fi
    done < <(sudo find "$DATA/linux-users" -mindepth 1 -maxdepth 1 -type d 2>/dev/null)
  fi
  say "native ancestor permissions fixed (traversable 0711; profile data stays 0700)"
}

# ---------------------------------------------------------------------------
# commands
# ---------------------------------------------------------------------------
cmd_up() {
  local requested="${1:-}"
  need_cmd curl
  need_cmd jq
  require_binary

  local mode
  if [ -n "$requested" ]; then
    case "$requested" in
      docker | native) mode="$requested" ;;
      *) die "mode must be 'docker' or 'native', got '$requested'" ;;
    esac
  else
    mode=$(mode_of)
  fi

  # A server (or leftovers) from a previous run would hold the instance lock.
  if pid_of "$PID_DIR/server.pid" >/dev/null || pid_of "$PID_DIR/worker.pid" >/dev/null ||
    pid_of "$PID_DIR/helper.pid" >/dev/null; then
    say "previous dev instance processes found; stopping first"
    stop_all
  fi
  cleanup_strays

  mkdir -p "$ROOT" "$DATA" "$STORAGE" "$LOGS" "$PID_DIR"
  # The server user must be able to traverse the root, read the config and
  # write its pid/log files; data and storage belong to the server user.
  sudo chmod 755 "$ROOT" "$LOGS"
  sudo chmod 755 "$PID_DIR"
  sudo chown "$SERVER_USER:$SERVER_USER" "$PID_DIR"
  # The server owns its data, but native identities own their homes and
  # profiles: a recursive chown would break XAUTHORITY and the profile
  # environment (and is not what the production installer does).
  sudo chown "$SERVER_USER:$SERVER_USER" "$DATA" "$STORAGE"
  sudo find "$DATA" -mindepth 1 -maxdepth 1 \
    ! -name linux-apps ! -name linux-users \
    -exec chown -R "$SERVER_USER:$SERVER_USER" {} + 2>/dev/null || true
  sudo find "$STORAGE" -mindepth 1 -maxdepth 1 \
    -exec chown -R "$SERVER_USER:$SERVER_USER" {} + 2>/dev/null || true
  # Traversable but not listable: native app identities (rast-u<id>) must walk
  # through data_dir into their profile directory. Production uses 0711 for
  # /var/lib/rast; 0700 here breaks `systemd` CHDIR (status=200/CHDIR).
  sudo chmod 711 "$DATA" "$STORAGE"
  # Unix sockets live in their own directories because `rast worker` tightens
  # the socket's parent directory to 0750. Pre-create it as root:<server group>
  # so the chmod keeps the group traverse bit the server needs to connect.
  sudo install -d -o root -g "$SERVER_USER" -m 750 "$WORKER_RUN_DIR" "$NATIVE_RUN_DIR"

  render_config "$mode"
  echo "$mode" >"$MODE_FILE"

  ensure_creds
  if ! sudo -u "$SERVER_USER" test -f "$DATA/rast.db"; then
    create_admin_cli
  fi

  start_worker
  if [ "$mode" = native ]; then
    cmd_native_layout
    start_helper || warn "continuing without the native helper"
  fi
  start_server
  bootstrap_users
  mirror_creds_to_repo
  if [ "$mode" = native ]; then
    cmd_fix_perms
  fi

  say "dev instance is up (mode: $mode)"
  say "  URL:     $BASE_URL"
  say "  creds:   $CREDS"
  say "  logs:    $LOGS"
  say "  worker:  $WORKER_SOCK"
  [ "$mode" = native ] && say "  helper:  $NATIVE_SOCK"
}

cmd_down() {
  stop_all
  say "dev instance stopped"
}

cmd_status() {
  local mode
  mode=$(mode_of)
  local ok=0
  printf 'mode:        %s\n' "$mode"
  printf 'url:         %s\n' "$BASE_URL"
  printf 'config:      %s\n' "$CONFIG"
  printf 'data:        %s\n' "$DATA"
  printf 'storage:     %s\n' "$STORAGE"
  if pid=$(pid_of "$PID_DIR/server.pid"); then
    printf 'server:      running (pid %s)\n' "$pid"
    if curl -fsS --max-time 2 "$BASE_URL/healthz" >/dev/null 2>&1; then
      printf 'health:      ok\n'
    else
      printf 'health:      NOT RESPONDING\n'; ok=1
    fi
  else
    printf 'server:      stopped\n'; ok=1
  fi
  if pid=$(pid_of "$PID_DIR/worker.pid"); then
    printf 'worker:      running (pid %s, socket %s)\n' "$pid" "$WORKER_SOCK"
  else
    printf 'worker:      stopped\n'; ok=1
  fi
  if pid=$(pid_of "$PID_DIR/helper.pid"); then
    printf 'native:      running (pid %s, socket %s)\n' "$pid" "$NATIVE_SOCK"
  else
    printf 'native:      stopped\n'
  fi
  if [ -f /etc/systemd/system/rast-linux.slice ]; then
    printf 'slice:       installed (/etc/systemd/system/rast-linux.slice)\n'
  else
    printf 'slice:       not installed\n'
  fi
  [ -f "$CREDS" ] && printf 'creds:       %s\n' "$CREDS"
  if [ -x "$BIN" ]; then
    printf 'binary:      %s (%s)\n' "$BIN" "$("$BIN" version 2>/dev/null | head -n 1)"
  else
    printf 'binary:      missing (%s)\n' "$BIN"; ok=1
  fi
  return "$ok"
}

cmd_reset() {
  local mode="${1:-}"
  cmd_down
  say "removing $DATA, $STORAGE, $LOGS and runtime files"
  sudo rm -rf "$DATA" "$STORAGE" "$LOGS" "$PID_DIR" "$CONFIG" "$MODE_FILE" \
    "$WORKER_RUN_DIR" "$NATIVE_RUN_DIR" "$ROOT/.admin.cookies"
  # Keep credentials so repeat runs use the same accounts.
  cmd_up "${mode:-}"
}

# Restart only the web server: used by the service-restart-recovery scenario.
# The worker and native helper keep running, so the test isolates server
# restart behaviour (session adoption vs. leftover copies).
cmd_restart_server() {
  stop_one server "$PID_DIR/server.pid" user
  start_server
}

cmd_logs() {
  local which="${1:-server}" follow="${2:-}"
  local file
  case "$which" in
    server) file="$SERVER_LOG" ;;
    worker) file="$WORKER_LOG" ;;
    helper) file="$HELPER_LOG" ;;
    *) die "unknown log '$which' (server|worker|helper)" ;;
  esac
  [ -f "$file" ] || die "no log at $file"
  if [ "$follow" = -f ] || [ "$follow" = --follow ]; then
    tail -n 50 -f "$file"
  else
    tail -n 100 "$file"
  fi
}

cmd_health() {
  curl -fsS "$BASE_URL/healthz" && echo
}

cmd_shell() {
  sudo -u "$SERVER_USER" env RAST_DATA_DIR="$DATA" RAST_STORAGE_DIR="$STORAGE" \
    bash --rcfile /dev/null -i
}

cmd_creds() {
  [ -f "$CREDS" ] || die "no credentials yet — run 'up' first"
  printf '%s\n' "$CREDS"
  cat "$CREDS"
}

cmd_build() {
  "$SCRIPT_DIR/build-snapshot.sh"
}

usage() {
  sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

main() {
  local command="${1:-up}"
  shift || true
  case "$command" in
    up | start) cmd_up "${1:-}" ;;
    down | stop) cmd_down ;;
    restart) cmd_down; cmd_up "${1:-}" ;;
    restart-server) cmd_restart_server ;;
    fix-perms) cmd_fix_perms ;;
    native-layout) cmd_native_layout ;;
    status) cmd_status ;;
    reset) cmd_reset "${1:-}" ;;
    logs) cmd_logs "${1:-server}" "${2:-}" ;;
    creds) cmd_creds ;;
    health) cmd_health ;;
    shell) cmd_shell ;;
    build) cmd_build ;;
    help | -h | --help) usage ;;
    *) die "unknown command '$command' (try: up|down|restart|status|reset|logs|creds|health|shell|build)" ;;
  esac
}

main "$@"
