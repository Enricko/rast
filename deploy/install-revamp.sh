#!/usr/bin/env bash
# Publish the new RAST client shell at https://desktop-revamp.lufria.tech/.
#
# Run this ON THE HOST as root:
#
#     sudo scripts/install-revamp.sh [--dry-run]
#
# It is idempotent and does one thing per step, each with a check:
#   1. copies the built browser bundle (dist/web) to /srv/rast-static/revamp
#   2. installs the nginx site that serves it on 127.0.0.1:8722
#   3. adds the Cloudflare Tunnel ingress rule
#      desktop-revamp.lufria.tech -> http://127.0.0.1:8722 (backup first)
#   4. reloads nginx, restarts cloudflared, then checks the site locally and
#      from the public URL
#
# The running desktop at https://desktop.lufria.tech/ is a different nginx
# server block and a different tunnel hostname, so it keeps serving old RAST.
#
# Undo:
#   rm /etc/nginx/sites-enabled/revamp /etc/nginx/sites-available/revamp
#   nginx -t && systemctl reload nginx
#   remove the desktop-revamp.lufria.tech ingress from /etc/cloudflared/config.yml
#   systemctl restart cloudflared
#
# This script deliberately does not touch DNS: the record for
# desktop-revamp.lufria.tech must point at the tunnel (Cloudflare dashboard:
# a proxied CNAME to <tunnel-id>.cfargotunnel.com, or
# `cloudflared tunnel route dns <tunnel> desktop-revamp.lufria.tech`).
set -euo pipefail

DOMAIN=desktop-revamp.lufria.tech
SITE_PORT=8722
WEB_ROOT=/srv/rast-static/revamp
NGINX_SITE=/etc/nginx/sites-available/revamp
TUNNEL_CONFIG=/etc/cloudflared/config.yml
TUNNEL_LINE="  - hostname: $DOMAIN"

dry_run=false
web_only=false
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=true ;;
        --web-only) web_only=true ;;
        -h | --help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown argument: $arg (try --dry-run or --web-only)" >&2; exit 2 ;;
    esac
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

if [[ ! -f dist/web/index.html || ! -f dist/web/app_bg.wasm ]]; then
    echo "dist/web is missing or incomplete; build it first:" >&2
    echo "  scripts/web-build.sh" >&2
    exit 1
fi

say() {
    printf '%s\n' "$*"
}

if $dry_run; then
    say "dry run: would publish $repo_root/dist/web to $WEB_ROOT"
    if $web_only; then
        say "(web files only; nginx and the tunnel are left alone)."
    else
        say "and ensure the nginx site on 127.0.0.1:$SITE_PORT and the tunnel"
        say "rule for $DOMAIN."
    fi
    exit 0
fi

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root: sudo scripts/install-revamp.sh" >&2
    exit 1
fi

# --- 1. static files -------------------------------------------------------
say "==> publishing $(du -sh dist/web | cut -f1) of browser build to $WEB_ROOT"
install -d -m 0755 "$WEB_ROOT"
if command -v rsync >/dev/null 2>&1; then
    rsync -a --delete dist/web/ "$WEB_ROOT/"
else
    find "$WEB_ROOT" -mindepth 1 -delete
    cp -a dist/web/. "$WEB_ROOT/"
fi
chmod -R a+rX "$WEB_ROOT"

if $web_only; then
    say "==> web files updated; nginx and the tunnel were not touched"
    say "Done. https://$DOMAIN/"
    exit 0
fi

# --- 2. nginx site ---------------------------------------------------------
say "==> installing nginx site for $DOMAIN"
install -d -m 0755 /etc/nginx/sites-available /etc/nginx/sites-enabled
install -m 0644 deploy/nginx-revamp-site.conf "$NGINX_SITE"
ln -sfn "$NGINX_SITE" /etc/nginx/sites-enabled/revamp
if ! nginx -t; then
    say "nginx rejected the configuration; removing the new site again"
    rm -f /etc/nginx/sites-enabled/revamp
    exit 1
fi
systemctl reload nginx
say "    nginx serves $WEB_ROOT on 127.0.0.1:$SITE_PORT"

# --- 3. Cloudflare Tunnel ingress -----------------------------------------
say "==> ensuring the tunnel route for $DOMAIN"
tunnel_changed=false
if grep -qF "$TUNNEL_LINE" "$TUNNEL_CONFIG"; then
    say "    route already present"
else
    backup="$TUNNEL_CONFIG.bak-$(date +%Y%m%d-%H%M%S)"
    cp -a "$TUNNEL_CONFIG" "$backup"
    say "    backup: $backup"
    python3 - "$TUNNEL_CONFIG" "$DOMAIN" "$SITE_PORT" <<'PY'
import sys

path, domain, port = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path).read()
if f"hostname: {domain}" in text:
    raise SystemExit(0)
catch_all = "\n  - service: http_status:404\n"
if catch_all not in text:
    raise SystemExit("no catch-all rule in the tunnel config; refusing to edit it")
rule = f"\n  - hostname: {domain}\n    service: http://127.0.0.1:{port}\n"
open(path, "w").write(text.replace(catch_all, rule + catch_all, 1))
print(f"    added {domain} -> http://127.0.0.1:{port}")
PY
    if cloudflared tunnel --config "$TUNNEL_CONFIG" ingress validate >/dev/null 2>&1; then
        say "    tunnel config validates"
    else
        say "    warning: cloudflared could not validate the config; restoring the backup"
        cp -a "$backup" "$TUNNEL_CONFIG"
        exit 1
    fi
    tunnel_changed=true
fi
if $tunnel_changed; then
    say "==> applying the tunnel configuration"
    systemctl reload-or-restart cloudflared
    sleep 2
else
    say "    cloudflared left running (configuration unchanged)"
fi

# --- 4. checks -------------------------------------------------------------
say "==> local check"
curl -fsS -o /dev/null -H "Host: $DOMAIN" "http://127.0.0.1:$SITE_PORT/"
say "    nginx answered"
say "==> public check (Cloudflare may need a few seconds)"
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "https://$DOMAIN/" 2>/dev/null) && [[ $code == 200 ]]; then
        say "    https://$DOMAIN/ -> HTTP $code"
        say
        say "Done. The client shell is live at https://$DOMAIN/"
        exit 0
    fi
    sleep 3
done
say "    public URL still answers ${code:-nothing}; DNS may not point at the tunnel yet:"
say "    - Cloudflare dashboard: proxied CNAME $DOMAIN -> <tunnel-id>.cfargotunnel.com, or"
say "    - cloudflared tunnel route dns <tunnel-name> $DOMAIN"
exit 1
