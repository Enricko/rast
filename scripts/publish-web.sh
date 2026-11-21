#!/usr/bin/env bash
# Build the browser bundle, then atomically publish it to the existing
# desktop-revamp.lufria.tech site. Requires write access to its static root.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

if (($#)); then
    printf 'Usage: scripts/publish-web.sh (no arguments)\n' >&2
    exit 2
fi

scripts/web-build.sh
deploy/install-revamp.sh --web-only
