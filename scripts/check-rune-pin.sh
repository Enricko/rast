#!/usr/bin/env bash
# Report whether the pinned Rune revision is still the head of Rune's `dev`.
#
#   scripts/check-rune-pin.sh
#
# Exit codes:
#   0   the pin is up to date
#   10  a newer Rune revision exists (CI's scheduled job turns this into a build
#       failure, which is how the team hears about it)
#   1   the check could not run (network, malformed Cargo.toml)
#
# Rune has no releases, tags or crates.io package: every commit on `dev` is a new
# pre-release, so "is there a new patch?" means "did `dev` move?".
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

rune_repo=${RUNE_REPO:-https://github.com/anomalyco/rune}

pinned=$(python3 - <<'PY'
import tomllib

with open("Cargo.toml", "rb") as handle:
    manifest = tomllib.load(handle)
rune = manifest["workspace"]["dependencies"]["rune"]
rev = rune.get("rev")
if not isinstance(rev, str) or len(rev) != 40:
    raise SystemExit("Cargo.toml does not pin rune to a full 40-character rev")
print(rev)
PY
)

latest=$(git ls-remote "$rune_repo" refs/heads/dev | cut -f1)
if [[ -z $latest ]]; then
    echo "could not read refs/heads/dev from $rune_repo" >&2
    exit 1
fi

echo "pinned rev: $pinned"
echo "dev head:   $latest"
echo

if [[ $pinned == "$latest" ]]; then
    echo "Rune is up to date."
    exit 0
fi

echo "A newer Rune revision is available on \`dev\`."
echo
echo "Update in a review branch with:"
echo "  scripts/update-rune.sh            # latest dev"
echo "  scripts/update-rune.sh $latest    # this revision"
echo
echo "Read docs/UPDATING-RUNE.md first: Rune is pre-release, breaking changes are"
echo "expected between revisions."
exit 10
