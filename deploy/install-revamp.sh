#!/usr/bin/env bash
# Publish the new RAST client shell at https://desktop-revamp.lufria.tech/.
#
# On the host, run the full install as root:
#
#     sudo deploy/install-revamp.sh [--dry-run]
#     deploy/install-revamp.sh --web-only [--dry-run] [--web-root DIR]
#
# --web-only skips nginx and the tunnel and needs no root in a writable web root.
# The full install does one thing per step, each with a check:
#   1. publishes the new assets from dist/web, then atomically replaces index.html
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
custom_root=false
while (($#)); do
    case "$1" in
        --dry-run) dry_run=true ;;
        --web-only) web_only=true ;;
        --web-root)
            [[ $# -ge 2 && -n $2 ]] || { echo "--web-root needs a directory" >&2; exit 2; }
            WEB_ROOT=$2
            custom_root=true
            shift
            ;;
        -h | --help) sed -n '2,32p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1 (try --dry-run or --web-only)" >&2; exit 2 ;;
    esac
    shift
done
if $custom_root && ! $web_only; then
    echo "--web-root is only for --web-only; the nginx site uses a fixed document root" >&2
    exit 2
fi
if ! $dry_run && ! $web_only && [[ $(id -u) -ne 0 ]]; then
    echo "run as root: sudo deploy/install-revamp.sh (or use --web-only)" >&2
    exit 1
fi

say() {
    printf '%s\n' "$*"
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# --- 1. static files -------------------------------------------------------
# Read the source index once so we keep its asset digest and bytes together
# even if a build finishes during publication. Only copy that active release.
python3 - "$repo_root/dist/web" "$WEB_ROOT" "$dry_run" <<'PY'
import errno
import hashlib
from html.parser import HTMLParser
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile


def bundle_digest(root):
    """Hash sorted file paths and bytes. Keep in sync with web-build.sh."""
    if root.is_symlink() or not root.is_dir():
        raise SystemExit(f"Expected a regular release directory: {root}")
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise SystemExit(f"Unsupported bundle entry: {path}")
        if path.is_file():
            digest.update(path.relative_to(root).as_posix().encode("utf-8") + b"\0")
            with path.open("rb") as file:
                digest.update(hashlib.file_digest(file, "sha256").digest())
    return digest.hexdigest()


class Entrypoints(HTMLParser):
    def __init__(self):
        super().__init__()
        self.digests = []

    def handle_starttag(self, tag, attrs):
        if tag == "script":
            src = dict(attrs).get("src") or ""
            match = re.fullmatch(r"\./assets/([0-9a-f]{64})/boot\.js", src)
            if match:
                self.digests.append(match[1])


def publish(source, root, dry_run):
    def incomplete():
        raise SystemExit("dist/web is missing or incomplete; run scripts/web-build.sh first")

    source_index = source / "index.html"
    if not source_index.is_file():
        incomplete()
    index = source_index.read_bytes()
    parser = Entrypoints()
    parser.feed(index.decode("utf-8"))
    if len(parser.digests) != 1:
        incomplete()
    digest, = parser.digests
    release = source / "assets" / digest
    if not all((release / name).is_file() for name in ("boot.js", "app.js", "app_bg.wasm")):
        incomplete()
    if bundle_digest(release) != digest:
        raise SystemExit(f"Source release does not match its digest: {release}; rebuild into a clean --out")
    if dry_run:
        print(f"dry run: would publish assets/{digest} then index.html to {root}")
        return

    root.mkdir(parents=True, exist_ok=True)
    assets = root / "assets"
    try:
        assets.mkdir()
    except FileExistsError:
        if assets.is_symlink() or not assets.is_dir():
            raise SystemExit(f"Expected a regular assets directory: {assets}")
    else:
        assets.chmod(0o755)
        if os.geteuid() == 0:
            # A full install into a user-owned web root must not prevent that
            # owner from adding releases with --web-only later.
            owner = root.stat()
            os.chown(assets, owner.st_uid, owner.st_gid)

    installed = assets / digest
    if not os.path.lexists(installed):
        # Stage on the same filesystem in a private directory. Readers can
        # only see a fully copied release once the single rename succeeds.
        with tempfile.TemporaryDirectory(prefix=".publish-", dir=assets) as stage:
            staged = Path(stage) / digest
            shutil.copytree(release, staged, symlinks=True)
            if bundle_digest(staged) != digest:
                raise SystemExit(f"Source release changed while being copied: {release}")
            staged.chmod(0o755)
            for entry in staged.rglob("*"):
                entry.chmod(0o755 if entry.is_dir() else 0o644)
            try:
                staged.rename(installed)
            except OSError as error:
                # A concurrent publish may have installed this exact release.
                if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
                    raise
    if bundle_digest(installed) != digest:
        raise SystemExit(f"Existing release does not match its digest: {installed}; refusing to overwrite it")

    # Leave all prior releases and flat legacy assets alone. An in-flight page
    # (including one using old fixed URLs) can still load its matching bundle.
    with tempfile.TemporaryDirectory(prefix=".publish-", dir=root) as stage:
        staged = Path(stage) / "index.html"
        staged.write_bytes(index)
        staged.chmod(0o644)
        staged.replace(root / "index.html")
    print(f"==> published assets/{digest} and index.html to {root}")


try:
    publish(Path(sys.argv[1]), Path(sys.argv[2]).resolve(), sys.argv[3] == "true")
except PermissionError as error:
    raise SystemExit(f"Publishing requires a writable web root and assets directory: {error}") from error
PY

if $dry_run; then
    if $web_only; then
        say "(web files only; nginx and the tunnel are left alone)."
    else
        say "and ensure the nginx site on 127.0.0.1:$SITE_PORT and the tunnel"
        say "rule for $DOMAIN."
    fi
    exit 0
fi

if $web_only; then
    say "==> web files updated; nginx and the tunnel were not touched"
    if ! $custom_root; then
        say "Done. https://$DOMAIN/"
    fi
    exit 0
fi

cd "$repo_root"

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
for _ in 1 2 3 4 5 6 7 8 9 10; do
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
