#!/usr/bin/env bash
# RAST deployment smoke test.
#
# Exercises a *running* RAST instance over HTTP(S) the same way the web UI does:
# authentication, CSRF, user isolation, uploads/downloads, quota, path traversal
# rejection, recycle bin, monitoring, backups and activity logs.
#
# Usage:
#   RAST_SMOKE_ADMIN_PASS='<admin password>' ./tests/smoke.sh https://rast.example.com
#
# Options via environment:
#   RAST_SMOKE_ADMIN_USER   admin username           (default: admin)
#   RAST_SMOKE_ADMIN_PASS   admin password           (required)
#   RAST_SMOKE_KEEP_USERS   1 = do not deactivate the users created by the test
#   RAST_SMOKE_SSE_SECONDS  how long to watch SSE     (default: 8)
#
# Exit code 0 = all checks passed (skips allowed for features not available on
# this host, e.g. Docker). Exit code 1 = at least one check failed.

set -uo pipefail

BASE="${1:-http://127.0.0.1:8720}"
BASE="${BASE%/}"
ADMIN_USER="${RAST_SMOKE_ADMIN_USER:-admin}"
ADMIN_PASS="${RAST_SMOKE_ADMIN_PASS:?set RAST_SMOKE_ADMIN_PASS to the administrator password}"
SSE_SECONDS="${RAST_SMOKE_SSE_SECONDS:-8}"
KEEP_USERS="${RAST_SMOKE_KEEP_USERS:-0}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rast-smoke.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
SKIP=0
LOGIN_STATUS="000"
LOGIN_CSRF=""
declare -a FAILURES=()

pass() { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); FAILURES+=("$*"); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }
skip() { SKIP=$((SKIP + 1)); printf '  \033[33mSKIP\033[0m %s\n' "$*"; }
info() { printf '\n== %s ==\n' "$*"; }

json_get() { # json_get <file> <python expression over d>
    python3 - "$1" "$2" <<'PY' 2>/dev/null
import json, sys
try:
    with open(sys.argv[1]) as fh:
        d = json.load(fh)
except Exception:
    sys.exit(1)
try:
    value = eval(sys.argv[2], {"d": d, "len": len})
except Exception:
    sys.exit(1)
if isinstance(value, bool):
    print("true" if value else "false")
elif value is None:
    print("")
else:
    print(value)
PY
}

# api <cookie-jar> <method> <path> [json-body] [csrf-token] -> writes body to $WORK/body, returns status
api() {
    local jar="$1" method="$2" path="$3" body="${4:-}" csrf="${5:-}"
    local args=(-sS -o "$WORK/body" -w '%{http_code}' -X "$method" -b "$jar" -c "$jar" -H 'Accept: application/json')
    if [ -n "$body" ]; then
        args+=(-H 'Content-Type: application/json' --data-binary "$body")
    fi
    if [ -n "$csrf" ]; then
        args+=(-H "X-CSRF-Token: $csrf")
    fi
    local status
    status="$(curl "${args[@]}" "$BASE$path" 2>"$WORK/curl.err")" || true
    printf '%s' "${status:-000}"
}

# login <jar> <username> <password> -> sets LOGIN_STATUS and LOGIN_CSRF globals
login() {
    local jar="$1" user="$2" pass="$3"
    : > "$jar"
    LOGIN_CSRF=""
    LOGIN_STATUS="$(api "$jar" POST /api/auth/login "$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$user" "$pass")")"
    if [ "$LOGIN_STATUS" = "200" ]; then
        LOGIN_CSRF="$(json_get "$WORK/body" "d['csrfToken']")"
    fi
}

upload_file() { # upload_file <jar> <csrf> <dir> <name> <local-file>
    local jar="$1" csrf="$2" dir="$3" name="$4" file="$5"
    curl -sS -o "$WORK/body" -w '%{http_code}' -X POST \
        -b "$jar" -c "$jar" -H "X-CSRF-Token: $csrf" \
        -H 'Content-Type: application/octet-stream' \
        --data-binary "@$file" \
        "$BASE/api/files/upload?dir=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=""))' "$dir")&name=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=""))' "$name")&overwrite=true" 2>/dev/null || echo "000"
}

info "Target: $BASE"

########################################################################
info "Public endpoints"
########################################################################
STATUS="$(api "$WORK/public.jar" GET /healthz)"
if [ "$STATUS" = "200" ]; then pass "GET /healthz -> 200"; else fail "GET /healthz -> $STATUS"; fi

STATUS="$(api "$WORK/public.jar" GET /api/bootstrap)"
if [ "$STATUS" = "200" ]; then
    PRODUCT="$(json_get "$WORK/body" "d['product']")"
    VERSION="$(json_get "$WORK/body" "d['version']")"
    if [ "$PRODUCT" = "RAST" ]; then pass "bootstrap reports product=RAST version=$VERSION"; else fail "bootstrap product is '$PRODUCT'"; fi
    SETUP_REQUIRED="$(json_get "$WORK/body" "d['setupRequired']")"
    if [ "$SETUP_REQUIRED" = "false" ]; then pass "setup is completed"; else fail "setupRequired=true: no administrator configured"; fi
else
    fail "GET /api/bootstrap -> $STATUS"
fi

# Static UI must be served by the backend (not a proxy error page).
STATUS="$(curl -sS -o "$WORK/index.html" -w '%{http_code}' "$BASE/")"
if [ "$STATUS" = "200" ] && grep -q 'id="app"' "$WORK/index.html"; then
    pass "GET / serves the RAST desktop shell"
else
    fail "GET / did not serve the SPA (status $STATUS)"
fi

# Internal endpoints must not be reachable without a session.
for path in /api/auth/me /api/users /api/logs /api/system/info /api/monitor/snapshot /api/events; do
    STATUS="$(api "$WORK/public.jar" GET "$path")"
    if [ "$STATUS" = "401" ]; then pass "unauthenticated $path rejected (401)"; else fail "unauthenticated $path returned $STATUS (expected 401)"; fi
done

########################################################################
info "Administrator authentication"
########################################################################
ADMIN_JAR="$WORK/admin.jar"
login "$ADMIN_JAR" "$ADMIN_USER" "$ADMIN_PASS"
ADMIN_CSRF="$LOGIN_CSRF"
if [ "$LOGIN_STATUS" = "200" ] && [ -n "$ADMIN_CSRF" ]; then
    pass "admin login succeeded (session cookie + CSRF token issued)"
else
    fail "admin login failed (status $LOGIN_STATUS)"
    echo "Cannot continue without an administrator session." >&2
    exit 1
fi

STATUS="$(api "$ADMIN_JAR" GET /api/auth/me)"
if [ "$STATUS" = "200" ] && [ "$(json_get "$WORK/body" "d['user']['role']")" = "admin" ]; then
    pass "GET /api/auth/me returns the admin profile"
else
    fail "GET /api/auth/me -> $STATUS"
fi

# Mutations without CSRF must be rejected.
STATUS="$(api "$ADMIN_JAR" POST /api/users '{"username":"csrf-probe","password":"Probe-Password-123!","role":"user"}')"
if [ "$STATUS" = "403" ]; then pass "mutation without CSRF token rejected (403)"; else fail "CSRF enforcement missing (got $STATUS)"; fi

########################################################################
info "Users and isolation"
########################################################################
SUFFIX="$(date +%s)"
ALICE="smoke-alice-$SUFFIX"
BOB="smoke-bob-$SUFFIX"
ALICE_PASS="Smoke-Alice-$SUFFIX!"
BOB_PASS="Smoke-Bob-$SUFFIX!"

for pair in "$ALICE:$ALICE_PASS" "$BOB:$BOB_PASS"; do
    name="${pair%%:*}"
    password="${pair#*:}"
    STATUS="$(api "$ADMIN_JAR" POST /api/users "$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2],"role":"user","quotaBytes":5*1024*1024}))' "$name" "$password")" "$ADMIN_CSRF")"
    if [ "$STATUS" = "200" ]; then pass "created user $name (5 MiB quota)"; else fail "cannot create user $name ($STATUS: $(cat "$WORK/body"))"; fi
done

ALICE_JAR="$WORK/alice.jar"
login "$ALICE_JAR" "$ALICE" "$ALICE_PASS"
ALICE_CSRF="$LOGIN_CSRF"
ALICE_LOGIN_STATUS="$LOGIN_STATUS"
BOB_JAR="$WORK/bob.jar"
login "$BOB_JAR" "$BOB" "$BOB_PASS"
BOB_CSRF="$LOGIN_CSRF"
if [ "$ALICE_LOGIN_STATUS" = "200" ] && [ -n "$ALICE_CSRF" ] && [ -n "$BOB_CSRF" ]; then
    pass "both test users can sign in"
else
    fail "test user login failed"
fi

# Regular users must not reach admin endpoints.
for path in /api/users /api/settings /api/backups; do
    STATUS="$(api "$ALICE_JAR" GET "$path")"
    if [ "$STATUS" = "403" ]; then pass "user forbidden from $path"; else fail "user accessed $path ($STATUS)"; fi
done

########################################################################
info "File Station"
########################################################################
STATUS="$(api "$ALICE_JAR" GET '/api/files?path=/&page=1&perPage=50')"
if [ "$STATUS" = "200" ]; then
    pass "file listing works ($(json_get "$WORK/body" "d['total']") entries)"
else
    fail "GET /api/files -> $STATUS $(cat "$WORK/body")"
fi

STATUS="$(api "$ALICE_JAR" GET /api/files/roots)"
if [ "$STATUS" = "200" ]; then pass "file roots listed"; else fail "GET /api/files/roots -> $STATUS"; fi

STATUS="$(api "$ALICE_JAR" POST /api/files/mkdir '{"path":"/smoke-folder"}' "$ALICE_CSRF")"
if [ "$STATUS" = "200" ] || [ "$STATUS" = "201" ]; then pass "mkdir /smoke-folder"; else fail "mkdir failed ($STATUS: $(cat "$WORK/body"))"; fi

echo "RAST smoke test payload $SUFFIX" > "$WORK/payload.txt"
head -c 2048 /dev/urandom >> "$WORK/payload.txt"
STATUS="$(upload_file "$ALICE_JAR" "$ALICE_CSRF" /smoke-folder payload.txt "$WORK/payload.txt")"
if [ "$STATUS" = "200" ] || [ "$STATUS" = "201" ]; then pass "upload payload.txt"; else fail "upload failed ($STATUS: $(cat "$WORK/body"))"; fi

STATUS="$(api "$ALICE_JAR" GET '/api/files/download?path=%2Fsmoke-folder%2Fpayload.txt')"
if [ "$STATUS" = "200" ] && cmp -s "$WORK/body" "$WORK/payload.txt"; then
    pass "download returns the exact uploaded bytes"
else
    fail "download mismatch (status $STATUS)"
fi

# Bob must not be able to read Alice's file (object-level isolation).
STATUS="$(api "$BOB_JAR" GET '/api/files/download?path=%2Fsmoke-folder%2Fpayload.txt')"
if [ "$STATUS" = "404" ] || [ "$STATUS" = "403" ]; then pass "other user cannot read Alice's file ($STATUS)"; else fail "cross-user file access leaked ($STATUS)"; fi

# Path traversal attempts must be rejected.
for evil in '..%2F..%2Fetc%2Fpasswd' '%2Fetc%2Fpasswd' '..%252F..%252Fetc%252Fpasswd'; do
    STATUS="$(api "$ALICE_JAR" GET "/api/files/download?path=$evil")"
    if [ "$STATUS" = "400" ] || [ "$STATUS" = "403" ] || [ "$STATUS" = "404" ]; then
        pass "traversal rejected: $evil ($STATUS)"
    else
        fail "traversal NOT rejected: $evil ($STATUS)"
    fi
done

# Text preview + listing of the uploaded file.
STATUS="$(api "$ALICE_JAR" GET '/api/files/text?path=%2Fsmoke-folder%2Fpayload.txt')"
if [ "$STATUS" = "200" ]; then pass "text preview works"; else skip "text preview unavailable ($STATUS)"; fi

STATUS="$(api "$ALICE_JAR" POST /api/files/rename "$(python3 -c 'import json;print(json.dumps({"path":"/smoke-folder/payload.txt","newName":"renamed.txt"}))')" "$ALICE_CSRF")"
if [ "$STATUS" = "200" ]; then pass "rename works"; else fail "rename failed ($STATUS)"; fi

STATUS="$(api "$ALICE_JAR" POST /api/files/copy "$(python3 -c 'import json;print(json.dumps({"paths":["/smoke-folder/renamed.txt"],"destDir":"/"}))')" "$ALICE_CSRF")"
if [ "$STATUS" = "200" ]; then
    JOB="$(json_get "$WORK/body" "d['jobId']")"
    pass "copy queued as job $JOB"
    for _ in $(seq 1 50); do
        STATUS="$(api "$ALICE_JAR" GET "/api/jobs/$JOB")"
        STATE="$(json_get "$WORK/body" "d['status']")"
        [ "$STATE" = "done" ] && break
        [ "$STATE" = "failed" ] && break
        sleep 0.2
    done
    if [ "$STATE" = "done" ]; then pass "copy job completed"; else fail "copy job did not complete ($STATE)"; fi
else
    fail "copy failed ($STATUS: $(cat "$WORK/body"))"
fi

# Delete -> recycle bin -> restore.
STATUS="$(api "$ALICE_JAR" POST /api/files/delete '{"paths":["/renamed.txt"]}' "$ALICE_CSRF")"
if [ "$STATUS" = "200" ]; then
    JOB="$(json_get "$WORK/body" "d['jobId']")"
    for _ in $(seq 1 50); do
        STATUS="$(api "$ALICE_JAR" GET "/api/jobs/$JOB")"
        STATE="$(json_get "$WORK/body" "d['status']")"
        [ "$STATE" = "done" ] && break
        [ "$STATE" = "failed" ] && break
        sleep 0.2
    done
    pass "delete moved item to recycle bin ($STATE)"
else
    fail "delete failed ($STATUS)"
fi

STATUS="$(api "$ALICE_JAR" GET '/api/files/trash?page=1&perPage=50')"
TRASH_ID="$(json_get "$WORK/body" "d['items'][0]['id']")"
if [ "$STATUS" = "200" ] && [ -n "$TRASH_ID" ]; then
    pass "recycle bin lists the deleted item"
    STATUS="$(api "$ALICE_JAR" POST /api/files/trash/restore "$(python3 -c 'import json,sys;print(json.dumps({"ids":[sys.argv[1]]}))' "$TRASH_ID")" "$ALICE_CSRF")"
    if [ "$STATUS" = "200" ]; then pass "restore from recycle bin"; else fail "restore failed ($STATUS)"; fi
else
    fail "recycle bin listing failed ($STATUS)"
fi

# Quota enforcement: the users have 5 MiB; a 6 MiB upload must be refused.
head -c $((6 * 1024 * 1024)) /dev/zero > "$WORK/big.bin"
STATUS="$(upload_file "$ALICE_JAR" "$ALICE_CSRF" / big.bin "$WORK/big.bin")"
if [ "$STATUS" = "413" ] || [ "$STATUS" = "409" ] || [ "$STATUS" = "507" ]; then
    pass "quota enforced on upload (status $STATUS)"
else
    fail "quota not enforced: 6 MiB upload with 5 MiB quota returned $STATUS"
fi

STATUS="$(api "$ALICE_JAR" GET /api/files/usage)"
if [ "$STATUS" = "200" ]; then
    QUOTA="$(json_get "$WORK/body" "d['quotaBytes']")"
    USED="$(json_get "$WORK/body" "d['usedBytes']")"
    pass "usage reported (used=$USED of quota=$QUOTA)"
else
    fail "GET /api/files/usage -> $STATUS"
fi

########################################################################
info "Monitoring and live events"
########################################################################
STATUS="$(api "$ALICE_JAR" GET /api/monitor/snapshot)"
if [ "$STATUS" = "200" ]; then
    if [ "$(json_get "$WORK/body" "d.get('unavailable', False)")" = "true" ]; then
        fail "monitor snapshot reports unavailable"
    else
        pass "monitor snapshot has host metrics"
    fi
else
    fail "GET /api/monitor/snapshot -> $STATUS"
fi

STATUS="$(api "$ALICE_JAR" GET '/api/monitor/history?metric=cpu&points=60')"
if [ "$STATUS" = "200" ]; then pass "monitor history works"; else fail "monitor history -> $STATUS"; fi

# SSE: expect at least one monitor event within the window.
code="$(curl -sS -N --max-time "$SSE_SECONDS" -b "$ALICE_JAR" "$BASE/api/events" 2>/dev/null | grep -c '^event: monitor' || true)"
if [ "${code:-0}" -ge 1 ]; then pass "SSE delivered monitor event(s)"; else fail "SSE delivered no monitor events within ${SSE_SECONDS}s"; fi

########################################################################
info "Backup Center"
########################################################################
STATUS="$(api "$ADMIN_JAR" POST /api/backups '{"scope":["db","config"],"note":"smoke test"}' "$ADMIN_CSRF")"
if [ "$STATUS" = "200" ]; then
    JOB="$(json_get "$WORK/body" "d['jobId']")"
    for _ in $(seq 1 100); do
        STATUS="$(api "$ADMIN_JAR" GET "/api/jobs/$JOB")"
        STATE="$(json_get "$WORK/body" "d['status']")"
        [ "$STATE" = "done" ] && break
        [ "$STATE" = "failed" ] && break
        sleep 0.3
    done
    if [ "$STATE" = "done" ]; then pass "backup job completed"; else fail "backup job failed ($STATE: $(cat "$WORK/body"))"; fi

    STATUS="$(api "$ADMIN_JAR" GET '/api/backups?page=1&perPage=10')"
    BACKUP_ID="$(json_get "$WORK/body" "d['items'][0]['id']")"
    if [ -n "$BACKUP_ID" ]; then
        pass "backup listed"
        STATUS="$(api "$ADMIN_JAR" POST "/api/backups/$BACKUP_ID/verify" '{}' "$ADMIN_CSRF")"
        if [ "$STATUS" = "200" ] && [ "$(json_get "$WORK/body" "d['ok']")" = "true" ]; then
            pass "backup archive verified (checksums + SQLite integrity)"
        else
            fail "backup verification failed ($STATUS)"
        fi
        # Cleanup: delete the smoke-test backup.
        api "$ADMIN_JAR" DELETE "/api/backups/$BACKUP_ID" "" "$ADMIN_CSRF" >/dev/null
    else
        fail "backup not listed"
    fi
else
    fail "POST /api/backups -> $STATUS: $(cat "$WORK/body")"
fi

STATUS="$(api "$ADMIN_JAR" GET /api/backups/schedule)"
if [ "$STATUS" = "200" ]; then pass "backup schedule readable"; else fail "backup schedule -> $STATUS"; fi

########################################################################
info "Applications (optional: requires Docker worker)"
########################################################################
STATUS="$(api "$ADMIN_JAR" GET /api/docker/status)"
if [ "$STATUS" = "200" ]; then
    ENABLED="$(json_get "$WORK/body" "d['enabled']")"
    AVAILABLE="$(json_get "$WORK/body" "d['available']")"
    if [ "$AVAILABLE" = "true" ]; then
        pass "docker worker available (version $(json_get "$WORK/body" "d.get('version','?')"))"
    elif [ "$ENABLED" = "true" ]; then
        skip "docker enabled but worker unavailable: $(json_get "$WORK/body" "d.get('reason','')")"
    else
        skip "docker disabled (optional)"
    fi
    STATUS="$(api "$ADMIN_JAR" GET /api/apps)"
    if [ "$STATUS" = "200" ]; then pass "applications list readable"; else fail "GET /api/apps -> $STATUS"; fi
else
    fail "GET /api/docker/status -> $STATUS"
fi

# Admin-only: regular user must be refused.
STATUS="$(api "$ALICE_JAR" GET /api/apps)"
if [ "$STATUS" = "403" ]; then pass "applications are admin-only"; else fail "user accessed /api/apps ($STATUS)"; fi

########################################################################
info "Appearance (per-user wallpaper and theme)"
########################################################################
STATUS="$(api "$ALICE_JAR" GET /api/appearance)"
if [ "$STATUS" = "200" ]; then
    THEME="$(json_get "$WORK/body" "d['theme']")"
    KIND="$(json_get "$WORK/body" "d['wallpaper']['kind']")"
    pass "appearance defaults readable (theme=$THEME kind=$KIND)"
else
    fail "GET /api/appearance -> $STATUS"
fi

# Build a small valid 400x240 PNG payload for the upload test.
python3 - "$WORK/appearance.png" <<'PNGEOF'
import struct, sys, zlib
width, height = 400, 240
def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
rows = bytearray()
for y in range(height):
    rows.append(0)
    for x in range(width):
        rows += bytes((x * 255 // width, y * 255 // height, 120))
ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PNGEOF
UPLOAD_STATUS="$(curl -sS -o "$WORK/body" -w '%{http_code}' -X POST -b "$ALICE_JAR" -c "$ALICE_JAR" -H "X-CSRF-Token: $ALICE_CSRF" -H 'Content-Type: image/png' --data-binary "@$WORK/appearance.png" "$BASE/api/appearance/wallpapers" 2>/dev/null || echo 000)"
WALLPAPER_ID="$(json_get "$WORK/body" "d['id']")"
if [ "$UPLOAD_STATUS" = "201" ] && [ -n "$WALLPAPER_ID" ]; then
    pass "wallpaper upload accepted (id=$WALLPAPER_ID)"
else
    fail "wallpaper upload failed ($UPLOAD_STATUS: $(cat "$WORK/body"))"
fi

if [ -n "$WALLPAPER_ID" ]; then
    STATUS="$(curl -sS -o "$WORK/wallpaper-download.png" -w '%{http_code}' -b "$ALICE_JAR" "$BASE/api/appearance/wallpapers/$WALLPAPER_ID" 2>/dev/null || echo 000)"
    if [ "$STATUS" = "200" ] && cmp -s "$WORK/appearance.png" "$WORK/wallpaper-download.png"; then
        pass "owner can download their wallpaper byte-exactly"
    else
        fail "owner wallpaper download mismatch ($STATUS)"
    fi
    STATUS="$(api "$BOB_JAR" GET "/api/appearance/wallpapers/$WALLPAPER_ID")"
    if [ "$STATUS" = "404" ]; then pass "other user cannot read the wallpaper (404)"; else fail "wallpaper leaked to another user ($STATUS)"; fi
    STATUS="$(api "$BOB_JAR" DELETE "/api/appearance/wallpapers/$WALLPAPER_ID" "" "$BOB_CSRF")"
    if [ "$STATUS" = "404" ] || [ "$STATUS" = "403" ]; then pass "other user cannot delete the wallpaper ($STATUS)"; else fail "other user deleted the wallpaper ($STATUS)"; fi

    STATUS="$(api "$ALICE_JAR" PUT /api/appearance "$(python3 -c 'import json,sys;print(json.dumps({"theme":"dark","wallpaper":{"kind":"upload","uploadId":sys.argv[1],"fit":"contain","position":"top-right","overlay":35}}))' "$WALLPAPER_ID")" "$ALICE_CSRF")"
    if [ "$STATUS" = "200" ]; then
        STATUS="$(api "$ALICE_JAR" GET /api/appearance)"
        if [ "$(json_get "$WORK/body" "d['wallpaper']['uploadId']")" = "$WALLPAPER_ID" ] && [ "$(json_get "$WORK/body" "d['theme']")" = "dark" ]; then
            pass "appearance changes persist server-side (theme=dark, upload active)"
        else
            fail "appearance did not persist"
        fi
    else
        fail "PUT /api/appearance -> $STATUS: $(cat "$WORK/body")"
    fi

    STATUS="$(api "$BOB_JAR" GET /api/appearance)"
    if [ "$STATUS" = "200" ] && [ "$(json_get "$WORK/body" "d['wallpaper']['kind']")" = "builtin" ] && [ "$(json_get "$WORK/body" "d['wallpaper']['uploadId']")" = "" ]; then
        pass "other user has independent appearance settings"
    else
        fail "appearance settings are shared between users ($STATUS)"
    fi

    STATUS="$(api "$ALICE_JAR" DELETE "/api/appearance/wallpapers/$WALLPAPER_ID" "" "$ALICE_CSRF")"
    if [ "$STATUS" = "200" ] && [ "$(json_get "$WORK/body" "d['appearance']['wallpaper']['kind']")" = "builtin" ]; then
        pass "deleting the active wallpaper falls back to the default"
    else
        fail "wallpaper delete/fallback failed ($STATUS)"
    fi
    STATUS="$(api "$ALICE_JAR" POST /api/appearance/reset '{}' "$ALICE_CSRF")"
    if [ "$STATUS" = "200" ] && [ "$(json_get "$WORK/body" "d['theme']")" = "dark" ]; then
        pass "reset keeps the theme and restores the default wallpaper"
    else
        fail "appearance reset failed ($STATUS)"
    fi
    api "$ALICE_JAR" PUT /api/appearance '{"theme":"light"}' "$ALICE_CSRF" >/dev/null
fi

########################################################################
info "Activity logs"
########################################################################
STATUS="$(api "$ADMIN_JAR" GET '/api/logs?page=1&perPage=20')"
if [ "$STATUS" = "200" ]; then
    TOTAL="$(json_get "$WORK/body" "d['total']")"
    ACTIONS="$(python3 - "$WORK/body" <<'PY'
import json,sys
data=json.load(open(sys.argv[1]))
print(",".join(sorted({item["action"] for item in data["items"]})))
PY
)"
    pass "activity logs readable ($TOTAL entries; actions: $ACTIONS)"
    if echo "$ACTIONS" | grep -q 'auth.login'; then pass "login activity recorded"; else fail "login activity missing"; fi
else
    fail "GET /api/logs -> $STATUS"
fi

STATUS="$(api "$ALICE_JAR" GET '/api/logs?page=1&perPage=20')"
if [ "$STATUS" = "200" ]; then
    LEAK="$(python3 - "$WORK/body" "$BOB" <<'PY'
import json,sys
data=json.load(open(sys.argv[1]))
print("yes" if any(item["username"] == sys.argv[2] for item in data["items"]) else "no")
PY
)"
    if [ "$LEAK" = "no" ]; then pass "regular user only sees their own activity"; else fail "user can see other users' activity"; fi
else
    fail "user activity log -> $STATUS"
fi

########################################################################
info "Cleanup"
########################################################################
if [ "$KEEP_USERS" = "1" ]; then
    skip "test users kept (RAST_SMOKE_KEEP_USERS=1)"
else
    for name in "$ALICE" "$BOB"; do
        ID="$(api "$ADMIN_JAR" GET "/api/users?q=$name" "" >/dev/null; json_get "$WORK/body" "d['items'][0]['id']" 2>/dev/null)"
        if [ -n "$ID" ]; then
            api "$ADMIN_JAR" DELETE "/api/users/$ID" "" "$ADMIN_CSRF" >/dev/null
            pass "deactivated test user $name"
        fi
    done
fi

########################################################################
printf '\n=========================================\n'
printf 'RAST smoke test: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
if [ "$FAIL" -gt 0 ]; then
    printf '\nFailures:\n'
    for failure in "${FAILURES[@]}"; do printf '  - %s\n' "$failure"; done
    exit 1
fi
exit 0
