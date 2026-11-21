#!/usr/bin/env bash
# Build the browser bundle and print the exact host-side commands that publish it
# at https://desktop.lufria.tech/revamp/ (next to the running RAST desktop).
#
# This script never touches the host itself: nginx and /srv need root, and this
# repository is developed inside a sandbox that cannot write outside its checkout.
# The printed block is the part an operator runs on the host.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

scripts/web-build.sh "$@"

echo
echo "Built: $repo_root/dist/web"
echo
echo "Publish it (run these as root on the host):"
cat <<'CMDS'
  install -d -m 0755 /srv/rast-static/revamp
  rsync -a --delete dist/web/ /srv/rast-static/revamp/
  install -d -m 0755 /etc/nginx/snippets
  install -m 0644 deploy/nginx-revamp.conf /etc/nginx/snippets/revamp.conf
  # one-time: add  include /etc/nginx/snippets/revamp.conf;  inside the
  # `server { ... }` block of /etc/nginx/sites-available/rast, before `location / {`
  nginx -t && systemctl reload nginx
CMDS
echo
echo "Then open: https://desktop.lufria.tech/revamp/"
echo "The existing desktop at https://desktop.lufria.tech/ is not affected."
