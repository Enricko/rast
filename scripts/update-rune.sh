#!/usr/bin/env bash
# Move the pinned Rune revision across this workspace.
#
#   scripts/update-rune.sh            # pin the current head of Rune's `dev`
#   scripts/update-rune.sh <rev>      # pin one specific 40-character revision
#   scripts/update-rune.sh --dry-run  # show the manifest changes, run nothing
#
# What it does:
#   1. resolves the revision on Rune's `dev` branch;
#   2. reads Rune's root Cargo.toml at that revision and rebuilds our
#      `[patch.crates-io]` table from it — Cargo does not inherit patches
#      through a git dependency, so the table must match Rune's exactly;
#   3. rewrites the pinned rev and refreshes Cargo.lock;
#   4. builds, runs the headless test and a snapshot, and prints what changed
#      between the old and new revisions;
#   5. leaves the tree dirty for review: it never commits, tags or pushes.
#
# Rune publishes no releases or tags, so "updating to a new patch" means moving
# this pin. See docs/UPDATING-RUNE.md for the full procedure and the gates.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

rune_repo=${RUNE_REPO:-https://github.com/anomalyco/rune}
rune_dir=${RUNE_DIR:-$repo_root/.rune-src}
manifest=$repo_root/Cargo.toml

for tool in python3 git cargo; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "$tool is required" >&2
        exit 1
    }
done

dry_run=false
rev=""
for arg in "$@"; do
    case $arg in
        --dry-run) dry_run=true ;;
        -h | --help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        -*)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
        *)
            [[ -z $rev ]] || { echo "only one revision may be given" >&2; exit 2; }
            rev=$arg
            ;;
    esac
done
if [[ -z $rev ]]; then
    rev=$(git ls-remote "$rune_repo" refs/heads/dev | cut -f1)
    echo "Latest Rune dev revision: $rev"
fi
if [[ ! $rev =~ ^[0-9a-f]{40}$ ]]; then
    echo "not a full 40-character revision: $rev" >&2
    exit 1
fi

old_rev=$(python3 - <<'PY'
import tomllib

with open("Cargo.toml", "rb") as handle:
    manifest = tomllib.load(handle)
print(manifest["workspace"]["dependencies"]["rune"]["rev"])
PY
)
echo "Currently pinned: $old_rev"
if [[ $old_rev == "$rev" ]]; then
    echo "Already pinned to $rev; nothing to do."
    exit 0
fi

# --- 1. Fetch Rune's root Cargo.toml at the target revision ------------------
rune_manifest=$(mktemp)
trap 'rm -f "$rune_manifest"' EXIT

if [[ -d $rune_dir/.git ]]; then
    echo "Reading Rune's Cargo.toml at $rev from $rune_dir…"
    if ! git -C "$rune_dir" cat-file -e "$rev^{commit}" 2>/dev/null; then
        git -C "$rune_dir" fetch --quiet origin "$rev" 2>/dev/null ||
            git -C "$rune_dir" fetch --quiet origin dev 2>/dev/null || true
    fi
    if ! git -C "$rune_dir" cat-file -e "$rev^{commit}" 2>/dev/null; then
        echo "revision $rev is not in $rune_dir; fetching it" >&2
        git -C "$rune_dir" fetch --quiet --no-tags "https://github.com/anomalyco/rune" "$rev" ||
            { echo "could not fetch $rev" >&2; exit 1; }
    fi
    git -C "$rune_dir" show "$rev:Cargo.toml" >"$rune_manifest"
else
    echo "Fetching Rune's Cargo.toml at $rev from GitHub…"
    curl -fsSL "https://raw.githubusercontent.com/anomalyco/rune/$rev/Cargo.toml" >"$rune_manifest" ||
        { echo "could not fetch Rune's Cargo.toml at $rev" >&2; exit 1; }
fi

# --- 2. Rewrite the pin and the patch table --------------------------------
python3 - "$manifest" "$old_rev" "$rev" "$rune_manifest" <<'PY'
import pathlib
import re
import sys
import tomllib

manifest_path, old_rev, new_rev, rune_manifest_path = sys.argv[1:5]

with open(rune_manifest_path, "rb") as handle:
    rune_manifest = tomllib.load(handle)
patches = rune_manifest.get("patch", {}).get("crates-io", {})
if not patches:
    raise SystemExit("Rune's Cargo.toml has no [patch.crates-io] table; check the revision")

text = pathlib.Path(manifest_path).read_text()
if old_rev not in text:
    raise SystemExit(f"pinned rev {old_rev} not found in {manifest_path}")

# Every `rev = "<old>"` in the file belongs to the same Rune pin: the rune
# dependency and every patched crate.
text = text.replace(f'rev = "{old_rev}"', f'rev = "{new_rev}"')

lines = text.splitlines(keepends=True)
try:
    header = next(i for i, line in enumerate(lines) if line.strip() == "[patch.crates-io]")
except StopIteration:
    raise SystemExit(f"{manifest_path} has no [patch.crates-io] section")

next_section = next(
    (i for i in range(header + 1, len(lines)) if lines[i].lstrip().startswith("[")),
    None,
)
if next_section is None:
    raise SystemExit("the [patch.crates-io] section is the last one; cannot rewrite it safely")

# Lines after the last entry are not ours: blank lines and comments that belong
# to the next section (the profile notes, for example) are kept verbatim.
entry = re.compile(r"^[A-Za-z0-9_-]+ = \{")
last_entry = header
for i in range(header + 1, next_section):
    if entry.match(lines[i]):
        last_entry = i
trailing = lines[last_entry + 1 : next_section]
while trailing and not trailing[0].strip():
    trailing.pop(0)

generated = [
    "[patch.crates-io]\n",
    "# Regenerated by scripts/update-rune.sh: the crates Rune patches in its own\n",
    f"# repository at rev {new_rev}. Every entry stays on the pinned rev.\n",
]
for name in sorted(patches):
    generated.append(
        f'{name} = {{ git = "https://github.com/anomalyco/rune", rev = "{new_rev}" }}\n'
    )
generated.append("\n")

lines[header:next_section] = generated + trailing
pathlib.Path(manifest_path).write_text("".join(lines))
print(f"Rewrote {manifest_path}: {len(patches)} patched crates on {new_rev}")
PY

if $dry_run; then
    echo
    echo "Dry run: Cargo.toml would change as follows. Nothing else ran."
    git diff -- Cargo.toml
    git checkout -- Cargo.toml
    echo "Restored Cargo.toml."
    exit 0
fi

# --- 3. Refresh the lockfile and build -------------------------------------
echo
echo "Refreshing Cargo.lock…"
cargo metadata --format-version 1 >/dev/null

echo "Building $package…"
cargo build -p rast-ui

echo "Running the headless tests…"
cargo test -p rast-ui

# A visual proof. It needs a working GPU adapter; a CI runner or a headless box
# uses the software Vulkan driver (lavapipe).
if [[ -f /usr/share/vulkan/icd.d/lvp_icd.json ]]; then
    export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
fi
echo
echo "Rendering a headless snapshot (Rune's --snap needs a GPU adapter)…"
if ! cargo run -q -p rast-ui --bin rast -- --snap target/rune-update.png --scene opened; then
    echo "warning: the headless snapshot failed; check the renderer before committing" >&2
fi

# --- 4. Show what changed between the two revisions ------------------------
echo
if [[ -d $rune_dir/.git ]]; then
    git -C "$rune_dir" fetch --quiet --deepen 200 origin dev 2>/dev/null || true
    if git -C "$rune_dir" rev-list --count "$old_rev..$rev" >/dev/null 2>&1; then
        total=$(git -C "$rune_dir" rev-list --count "$old_rev..$rev")
        echo "Rune commits in $old_rev..$rev ($total total, newest first):"
        git -C "$rune_dir" log --oneline "$old_rev..$rev" | head -40
    else
        echo "Rune commits up to $rev:"
        git -C "$rune_dir" log --oneline -20 "$rev" 2>/dev/null || true
    fi
fi

echo
echo "Done. Cargo.toml and Cargo.lock are modified but not committed."
echo "Next: run the full gates, read the Rune changes above, then commit with"
echo "  chore(deps): update rune to ${rev:0:10}"
echo "and mention breaking changes in the body. Rollback: git checkout -- Cargo.toml Cargo.lock"
