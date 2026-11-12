#!/usr/bin/env bash
# Run RAST on your own computer (macOS or Linux) without installing it as a
# system service. Everything lives in one folder (default ~/RAST-local), the
# web desktop listens on this computer only, and Ctrl+C stops it.
#
#   scripts/run-local.sh                    # uses ./target/release/rast
#   RAST_BIN=~/Downloads/rast scripts/run-local.sh
#   RAST_LOCAL_DIR=~/rast-test RAST_PORT=8730 scripts/run-local.sh
#
# The Docker app works when the `docker` command does (Docker Desktop or
# another engine). The Docker helper (`rast worker`) then runs as you, with
# your Docker access, instead of as a separate privileged service.
#
# Written for bash 3.2 (the version macOS ships).
set -euo pipefail

script_dir=$(cd "$(dirname "$0")" && pwd)
rast_bin=${RAST_BIN:-"$script_dir/../target/release/rast"}
local_dir=${RAST_LOCAL_DIR:-"$HOME/RAST-local"}
port=${RAST_PORT:-8720}

if [ ! -x "$rast_bin" ]; then
  echo "RAST binary not found at $rast_bin" >&2
  echo "Build it first (cargo build --release) or set RAST_BIN=/path/to/rast." >&2
  exit 1
fi
case "$port" in
  '' | *[!0-9]*)
    echo "RAST_PORT must be a number, got '$port'" >&2
    exit 1
    ;;
esac

mkdir -p "$local_dir/data" "$local_dir/storage"
chmod 700 "$local_dir"

# Only RAST's own settings: no config file, so nothing under /etc is read.
unset RAST_CONFIG
export RAST_DATA_DIR="$local_dir/data"
export RAST_STORAGE_DIR="$local_dir/storage"
export RAST_BIND="127.0.0.1:$port"
export RAST_PUBLIC_URL="http://127.0.0.1:$port"
# Unix socket paths are limited to 104 bytes on macOS (108 on Linux).
socket_dir=""
socket="$local_dir/worker.sock"
if [ "${#socket}" -gt 100 ]; then
  socket_dir=$(mktemp -d "${TMPDIR:-/tmp}/rast.XXXXXX")
  socket="$socket_dir/worker.sock"
fi
export RAST_WORKER_SOCKET="$socket"

worker_pid=""
stop_worker() {
  if [ -n "$worker_pid" ]; then
    kill "$worker_pid" 2>/dev/null || true
    wait "$worker_pid" 2>/dev/null || true
    worker_pid=""
  fi
  if [ -n "$socket_dir" ]; then
    rm -rf "$socket_dir"
  fi
}
trap stop_worker EXIT
trap 'exit 130' INT TERM

if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  export RAST_DOCKER_ENABLED=true
  "$rast_bin" worker >>"$local_dir/worker.log" 2>&1 &
  worker_pid=$!
  tries=0
  while [ ! -S "$socket" ] && kill -0 "$worker_pid" 2>/dev/null && [ "$tries" -lt 50 ]; do
    sleep 0.1
    tries=$((tries + 1))
  done
  if [ -S "$socket" ]; then
    echo "Docker helper started (log: $local_dir/worker.log)."
  else
    echo "The Docker helper did not start; the Docker app stays off. Its log ends with:" >&2
    tail -n 3 "$local_dir/worker.log" >&2 || true
    stop_worker
    export RAST_DOCKER_ENABLED=false
  fi
else
  export RAST_DOCKER_ENABLED=false
  echo "Docker is not running here, so the Docker app stays off."
  echo "Start Docker Desktop and run this script again to use it."
fi

if [ ! -e "$local_dir/data/rast.db" ]; then
  echo
  echo "First run: create the administrator account with this setup token."
  "$rast_bin" setup-token
fi

echo "RAST is starting on http://127.0.0.1:$port (data in $local_dir). Press Ctrl+C to stop."
"$rast_bin" serve
