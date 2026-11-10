#!/usr/bin/env bash
# RAST example-app smoke test: deploy, inspect, restart, redeploy and delete a
# managed Compose application through the HTTP API.
#
# Usage:
#   RAST_URL=https://rast.example.com RAST_USER=admin RAST_PASS=... \
#   RAST_MESSAGE="hello" ./verify.sh [--keep]
#
# The script deliberately uses only curl + python3 (no jq dependency). It also
# checks the safety rails: a Compose file that bind-mounts /etc must be
# rejected, and a secret environment value must never be echoed back.
#
# Requirements: a RAST server with RAST_DOCKER_ENABLED=true, a running
# rast-worker.service and permission to pull nginx:alpine.

set -euo pipefail

RAST_URL="${RAST_URL:-http://127.0.0.1:8720}"
RAST_USER="${RAST_USER:-admin}"
RAST_PASS="${RAST_PASS:?set RAST_PASS to the admin password}"
RAST_MESSAGE="${RAST_MESSAGE:-hello from RAST}"
APP_NAME="${APP_NAME:-Hello RAST}"
KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

COOKIE_JAR="$(mktemp)"
trap 'rm -f "$COOKIE_JAR"' EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL:\033[0m %s\n' "$*" >&2; exit 1; }

# json_get EXPR: read a JSON document on stdin and evaluate a python expression
# with `data` bound to it.
json_get() {
  python3 -c 'import json,sys; data=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"
}

api() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-sS -X "$method" -b "$COOKIE_JAR" -c "$COOKIE_JAR"
              -H "Content-Type: application/json" -H "X-CSRF-Token: $CSRF")
  if [[ -n "$body" ]]; then args+=(-d "$body"); fi
  curl "${args[@]}" "$RAST_URL$path"
}

say "health"
curl -sS "$RAST_URL/healthz" | grep -qx 'ok' || fail "server is not healthy"

say "login as $RAST_USER"
LOGIN="$(curl -sS -c "$COOKIE_JAR" -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,sys; print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$RAST_USER" "$RAST_PASS")" \
  "$RAST_URL/api/auth/login")"
CSRF="$(printf '%s' "$LOGIN" | json_get 'data["csrfToken"]')"
[[ -n "$CSRF" ]] || fail "login failed: $LOGIN"

say "docker status"
STATUS="$(api GET /api/docker/status)"
printf '%s\n' "$STATUS"
AVAILABLE="$(printf '%s' "$STATUS" | json_get 'data["available"]')"
[[ "$AVAILABLE" == "True" ]] || fail "docker worker is not available: $STATUS"

# ---------------------------------------------------------------------------
# Safety rail: absolute host bind mounts must be rejected by validation.
# ---------------------------------------------------------------------------
say "reject unsafe compose (bind mount of /etc)"
UNSAFE='{"name":"Unsafe","compose":"services:\n  web:\n    image: nginx:alpine\n    volumes:\n      - /etc:/host-etc\n"}'
UNSAFE_RESP="$(api POST /api/apps "$UNSAFE" || true)"
printf '%s\n' "$UNSAFE_RESP"
if printf '%s' "$UNSAFE_RESP" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if "error" in d else 1)' 2>/dev/null; then
  say "unsafe compose correctly rejected"
else
  fail "unsafe compose was accepted: $UNSAFE_RESP"
fi

# ---------------------------------------------------------------------------
# Create the example app (secret value must be masked in responses).
# ---------------------------------------------------------------------------
COMPOSE="$(cat "$(dirname "$0")/compose.yaml")"
BODY="$(python3 - <<PY
import json
compose = open("$(dirname "$0")/compose.yaml").read()
print(json.dumps({
    "name": "$APP_NAME",
    "compose": compose,
    "env": [
        {"key": "MESSAGE", "value": "$RAST_MESSAGE", "secret": False},
        {"key": "DEMO_TOKEN", "value": "top-secret-token", "secret": True},
    ],
}))
PY
)"
say "create application"
CREATED="$(api POST /api/apps "$BODY")"
printf '%s\n' "$CREATED"
APP_ID="$(printf '%s' "$CREATED" | json_get 'data["app"]["id"]')"
JOB_ID="$(printf '%s' "$CREATED" | json_get 'data["jobId"]')"
[[ -n "$APP_ID" ]] || fail "create failed: $CREATED"
printf '%s' "$CREATED" | grep -q 'top-secret-token' && fail "secret value leaked in create response"
printf '%s' "$CREATED" | grep -q '••••' || fail "secret value is not masked"

wait_job() {
  local job_id="$1" state
  for _ in $(seq 1 120); do
    state="$(api GET "/api/jobs/$job_id" | json_get 'data["status"]')"
    case "$state" in
      done) return 0 ;;
      failed|cancelled) api GET "/api/jobs/$job_id" >&2; return 1 ;;
      *) sleep 1 ;;
    esac
  done
  return 1
}

say "wait for deploy job $JOB_ID"
wait_job "$JOB_ID" || fail "deployment job failed"

say "application detail"
DETAIL="$(api GET "/api/apps/$APP_ID")"
printf '%s\n' "$DETAIL"
printf '%s' "$DETAIL" | json_get 'data["status"]' | grep -Eq 'running|deploying' || fail "unexpected status"
printf '%s' "$DETAIL" | grep -q 'top-secret-token' && fail "secret value leaked in detail response"

say "logs"
api GET "/api/apps/$APP_ID/logs?tail=20" | head -c 2000; echo

say "restart"
RESTART="$(api POST "/api/apps/$APP_ID/restart" '{}')"
wait_job "$(printf '%s' "$RESTART" | json_get 'data["jobId"]')" || fail "restart failed"

say "history"
HISTORY="$(api GET "/api/apps/$APP_ID/history")"
printf '%s\n' "$HISTORY" | head -c 2000; echo
[[ "$(printf '%s' "$HISTORY" | json_get 'len(data["items"])')" -ge 2 ]] || fail "history is missing entries"

say "containers are flagged managed"
CONTAINERS="$(api GET /api/docker/containers)"
printf '%s\n' "$CONTAINERS" | python3 -c '
import json,sys
d = json.load(sys.stdin)
managed = [c for c in d["items"] if c.get("managed")]
assert managed, "no managed container found"
print("managed containers:", [c["name"] for c in managed])
'

say "delete (volumes kept)"
DELETED="$(api DELETE "/api/apps/$APP_ID?removeVolumes=false")"
printf '%s\n' "$DELETED"
printf '%s' "$DELETED" | json_get 'data["deleted"]' | grep -q True || fail "delete failed"

if [[ "$KEEP" == "1" ]]; then
  say "leaving app in place (--keep was not requested to delete)"; exit 0
fi
say "all checks passed"
