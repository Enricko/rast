#!/usr/bin/env bash
# Regenerate docs/THIRD-PARTY-LICENSES.md from Cargo.lock.
#
# Uses `cargo metadata` (no network needed once dependencies are vendored/cached)
# and python3 for JSON processing. Called manually when dependencies change;
# CI only checks that the file exists and is current for release tags.
#
# Usage:
#   scripts/generate-third-party-licenses.sh [--check] [--out FILE]
#
# --check exits 1 if the file on disk differs from what would be generated.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT="$ROOT/docs/THIRD-PARTY-LICENSES.md"
MODE="write"

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check) MODE="check" ;;
        --out)
            shift
            [[ $# -gt 0 ]] || {
                echo "generate-third-party-licenses: --out requires a file" >&2
                exit 2
            }
            OUT=$1
            ;;
        --out=*) OUT=${1#*=} ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "generate-third-party-licenses: unknown argument: $1 (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

# Make the toolchain usable when the script is started from a non-login shell.
if ! command -v cargo >/dev/null 2>&1; then
    for _cargo_dir in "$HOME/.cargo/bin" /usr/local/cargo/bin; do
        if [[ -x "$_cargo_dir/cargo" ]]; then
            PATH="$_cargo_dir:$PATH"
            export PATH
            break
        fi
    done
fi

command -v cargo >/dev/null 2>&1 || {
    echo "generate-third-party-licenses: cargo not found" >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    echo "generate-third-party-licenses: python3 not found" >&2
    exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/rast-licenses.XXXXXX")
cleanup() { rm -rf -- "$work"; }
trap cleanup EXIT

echo "generate-third-party-licenses: running cargo metadata"
(
    cd "$ROOT"
    cargo metadata --locked --format-version 1 >"$work/metadata.json"
)

python3 - "$work/metadata.json" "$work/licenses.md" "$ROOT" <<'PY'
import json
import pathlib
import sys

metadata_path, output_path, root = sys.argv[1], sys.argv[2], pathlib.Path(sys.argv[3])
with open(metadata_path, encoding="utf-8") as fh:
    metadata = json.load(fh)


def crate_url(pkg):
    repo = pkg.get("repository") or ""
    if repo.startswith("https://github.com/"):
        return repo.rstrip("/")
    home = pkg.get("homepage") or ""
    if home.startswith("https://"):
        return home.rstrip("/")
    return f"https://crates.io/crates/{pkg['name']}/{pkg['version']}"


rows = []
for pkg in metadata["packages"]:
    if pkg.get("source") is None:
        # Workspace package (RAST itself, first-party, MIT).
        continue
    license_id = (pkg.get("license") or "").strip()
    if not license_id:
        license_id = "NOASSERTION (check upstream)"
    rows.append((pkg["name"], pkg["version"], license_id, crate_url(pkg)))

rows.sort(key=lambda row: (row[0].lower(), row[1]))

license_counts = {}
for _, _, license_id, _ in rows:
    license_counts[license_id] = license_counts.get(license_id, 0) + 1

lines = []
lines.append("# Third-party licenses")
lines.append("")
lines.append(
    "RAST itself is MIT licensed (see [LICENSE](../LICENSE)). This file lists the "
    "third-party Rust crates compiled into the `rast` binary."
)
lines.append("")
lines.append(
    "**Generated file — do not edit manually.** Regenerate with "
    "`scripts/generate-third-party-licenses.sh` after changing `Cargo.lock`."
)
lines.append("")
lines.append(f"The locked dependency graph (`Cargo.lock`) contains {len(rows)} third-party "
             "Rust crates, including crates used only for specific platforms or for "
             "tests; only the subset required by the Linux release build is linked "
             "into the binary. Each crate remains under the license declared by its "
             "authors; the full license texts are available in the upstream crate "
             "packages. Package-level license metadata follows the SPDX expressions "
             "reported by each crate.")
lines.append("")
lines.append("Frontend build tooling (Vite, TypeScript and their transitive "
             "dependencies) is used only to build `frontend/dist` and is not "
             "distributed inside the binary; their exact versions and licenses are "
             "pinned in `frontend/package-lock.json`.")
lines.append("")
lines.append("The web interface embedded in the binary bundles these frontend "
             "libraries (loaded only when a container terminal is opened); their "
             "license notice is kept in the bundled file:")
lines.append("")
lines.append("| Package | Version | License | Copyright |")
lines.append("| --- | --- | --- | --- |")
lines.append("| `@xterm/xterm` | 6.0.0 | MIT | The xterm.js authors; SourceLair Private Company; Christopher Jeffrey |")
lines.append("| `@xterm/addon-fit` | 0.11.0 | MIT | The xterm.js authors |")
lines.append("")
lines.append("## License summary")
lines.append("")
lines.append("| License expression | Packages |")
lines.append("| --- | ---: |")
for license_id in sorted(license_counts, key=lambda value: (-license_counts[value], value)):
    lines.append(f"| {license_id.replace('|', '\\|')} | {license_counts[license_id]} |")
lines.append("")
lines.append("## Packages")
lines.append("")
lines.append("| Package | Version | License | Source |")
lines.append("| --- | --- | --- | --- |")
for name, version, license_id, url in rows:
    safe_name = name.replace("|", "\\|")
    safe_license = license_id.replace("|", "\\|")
    lines.append(f"| `{safe_name}` | {version} | {safe_license} | <{url}> |")
lines.append("")


lines.append("")
lines.append("## External programs invoked at runtime")
lines.append("")
lines.append("These programs are **not** linked into or distributed with RAST. They are "
             "installed by the operator (or by their distribution) and RAST invokes "
             "them as separate processes; their own license terms and copyright "
             "notices apply and their source/license files remain in their "
             "distribution packages.")
lines.append("")
lines.append("| Program | Role | Version requirement | License |")
lines.append("| --- | --- | --- | --- |")
lines.append("| Xpra server (`xpra`) | Seamless graphical session per RAST user; HTML5 transport, window and audio forwarding. Invoked as an external program by the native helper. | 6.5+ (validated with 6.5.5) | GPL-2.0-or-later |")
lines.append("| Xpra HTML5 client (`xpra-html5`, served from `/usr/share/xpra/www`) | Browser client loaded inside the authenticated same-origin session page and adapted by RAST's shim. Served by the installed Xpra package, not embedded in the RAST binary. | shipped with the pinned Xpra (HTML5 client v21 with 6.5.5) | MPL-2.0 |")
lines.append("| Xvfb | Virtual X server started by Xpra for the seamless session | distribution version | GPL-2.0-or-later / MIT-style X11 terms as packaged |")
lines.append("| nftables (`nft`) | Display-port firewall rule restricting the loopback Xpra port to the RAST service uid | distribution version | GPL-2.0-or-later |")
lines.append("| PulseAudio + GStreamer plugins | Playback-only audio transport for a native session when enabled | distribution version | LGPL-2.1-or-later / GPL-2.0-or-later as packaged |")
lines.append("| Applications installed by an administrator | The actual desktop applications started for people | as packaged | determined by each package; RAST does not relicense or redistribute them |")
lines.append("")
lines.append("Installing or upgrading Xpra, Xvfb, nftables, PulseAudio/GStreamer or a "
             "native application does not change RAST's MIT license, and RAST does "
             "not claim authorship of those programs. Operators who redistribute a "
             "server image containing them are responsible for including the "
             "corresponding notices and source offers required by those licenses.")
lines.append("")
pathlib.Path(output_path).write_text("\n".join(lines), encoding="utf-8")
print(f"generate-third-party-licenses: rendered {len(rows)} crates")
PY

if [[ "$MODE" == "check" ]]; then
    if cmp -s -- "$work/licenses.md" "$OUT"; then
        echo "generate-third-party-licenses: $OUT is up to date"
        exit 0
    fi
    echo "generate-third-party-licenses: ERROR: $OUT is out of date" >&2
    diff -u -- "$OUT" "$work/licenses.md" | head -n 60 >&2 || true
    exit 1
fi

mkdir -p -- "$(dirname -- "$OUT")"
cp -- "$work/licenses.md" "$OUT"
echo "generate-third-party-licenses: updated $OUT"
