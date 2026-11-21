#!/usr/bin/env bash
# Build the RAST shell for the browser (Rune's experimental WASM + WebGPU target).
#
#   scripts/web-build.sh [--dev] [--out DIR]
#
# Requirements:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.129 --locked
#
# The static site has a stable index.html and immutable assets/<sha256>/ trees.
# Serve it over HTTPS (plain HTTP works on localhost), serve `.wasm` as
# `application/wasm`, and don't cache index.html. The page needs WebGPU.
#
# WebGPU caveat: Chrome/Edge 113+, Safari 26+, Firefox behind its WebGPU flag.
# There is no WebGL fallback.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo_bin=${CARGO:-cargo}
package=rast-ui
lib=rast_ui
out=dist/web
dev=false

while (($#)); do
    case $1 in
        --dev) dev=true ;;
        --out)
            [[ $# -ge 2 && -n $2 ]] || { echo "--out needs a directory" >&2; exit 2; }
            out=$2
            shift
            ;;
        -h | --help)
            sed -n '2,18p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift
done

if [[ -f Cargo.lock ]]; then
    bindgen_version=$(python3 - <<'PY'
import tomllib
with open("Cargo.lock", "rb") as handle:
    lock = tomllib.load(handle)
versions = sorted(p["version"] for p in lock["package"] if p["name"] == "wasm-bindgen")
if not versions:
    raise SystemExit("wasm-bindgen is missing from Cargo.lock")
print(versions[0])
PY
)
else
    bindgen_version=0.2.129
fi

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    echo "wasm-bindgen-cli is missing. Install the matching version:" >&2
    echo "  cargo install wasm-bindgen-cli --version $bindgen_version --locked" >&2
    exit 1
fi
if [[ $(wasm-bindgen --version) != "wasm-bindgen $bindgen_version" ]]; then
    echo "wasm-bindgen-cli version mismatch (need $bindgen_version):" >&2
    echo "  cargo install wasm-bindgen-cli --version $bindgen_version --locked --force" >&2
    exit 1
fi

profile_dir=release
if $dev; then
    profile_dir=debug
fi

echo "Building $package for wasm32-unknown-unknown ($profile_dir)…"
if $dev; then
    "$cargo_bin" build -p "$package" --lib --target wasm32-unknown-unknown
else
    # Size-first release knobs, the same set Rune's own web-build.sh uses. They
    # override this workspace's [profile.release] (codegen-units 16, incremental)
    # which is tuned for native iteration, not wasm size.
    CARGO_PROFILE_RELEASE_OPT_LEVEL=s \
    CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_PROFILE_RELEASE_DEBUG=0 \
    CARGO_PROFILE_RELEASE_STRIP=debuginfo \
        "$cargo_bin" build -p "$package" --lib --target wasm32-unknown-unknown --release
fi

artifact="target/wasm32-unknown-unknown/$profile_dir/$lib.wasm"
if [[ ! -f $artifact ]]; then
    echo "cargo finished but $artifact is missing." >&2
    echo "If you build on another machine, point CARGO at this machine's cargo." >&2
    exit 1
fi

# Stage on the output filesystems, never deleting the requested --out. All
# wasm-bindgen outputs keep their relative paths (including snippets/). Old
# releases and any preexisting legacy files are not touched.
python3 - "$out" "crates/$package" "$artifact" "$profile_dir" <<'PY'
import errno
import gzip
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import unquote, urlsplit


def bundle_digest(root):
    """Hash sorted file paths and bytes. Keep in sync with install-revamp.sh."""
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


def render_index(template, bundle, digest):
    # This is a hand-authored template: use quoted ./ paths for local src/href
    # resources. Keep transitive CSS/JS references relative to their own files.
    entrypoints = 0

    def rewrite(match):
        nonlocal entrypoints
        attribute, quote, url = match.groups()
        path = unquote(urlsplit(url).path).removeprefix("./")
        if ".." in Path(path).parts or path.startswith("/") or not (bundle / path).is_file():
            raise SystemExit(f"Missing or invalid index.html asset: {url}")
        if attribute.strip().lower().startswith("src") and url == "./boot.js":
            entrypoints += 1
        return f"{attribute}{quote}./assets/{digest}/{url[2:]}{quote}"

    result = re.sub(r'''(?<![\w:-])((?:src|href)\s*=\s*)(["'])(\./[^"']+)\2''',
                    rewrite, template, flags=re.IGNORECASE)
    if entrypoints != 1:
        raise SystemExit('index.html must have one src="./boot.js" entrypoint')
    return result


def copy_fonts(artifact, bundle):
    # Rune fetches the full Inter faces beside app.js after the first frame.
    # These paths may appear only in rune-text's own rustc dependency record.
    depfiles = [artifact.with_suffix(".d")]
    depfiles.extend(sorted((artifact.parent / "deps").glob("rune_text-*.d"),
                           key=lambda path: path.stat().st_mtime_ns, reverse=True))
    for path in depfiles:
        if not path.is_file():
            continue
        cut = next((item for item in path.read_text().split()
                    if item.endswith("/assets/web/first/Inter-Regular.ttf") and Path(item).is_file()), None)
        if cut:
            for font in Path(cut).parent.parent.glob("Inter-*.ttf"):
                shutil.copyfile(font, bundle / font.name)
            return


out, package, artifact = map(Path, sys.argv[1:4])
profile = sys.argv[4]
out = out.resolve()
out.mkdir(parents=True, exist_ok=True)
assets = out / "assets"
assets.mkdir(exist_ok=True)
if assets.is_symlink() or not assets.is_dir():
    raise SystemExit(f"Expected a regular assets directory: {assets}")

with tempfile.TemporaryDirectory(prefix=".build-", dir=assets) as stage:
    bundle = Path(stage) / "bundle"
    bundle.mkdir()
    args = ["wasm-bindgen", "--target", "web", "--out-dir", bundle, "--out-name", "app"]
    if profile == "release":
        args.append("--remove-name-section")
    subprocess.run([*args, artifact], check=True)
    for name in ("app.js", "app_bg.wasm"):
        if not (bundle / name).is_file():
            raise SystemExit(f"wasm-bindgen did not produce {name}")
    shutil.copyfile(package / "boot.js", bundle / "boot.js")
    copy_fonts(artifact, bundle)
    if (package / "assets").is_dir():
        for source in sorted((package / "assets").iterdir()):
            target = bundle / source.name
            if target.exists():
                raise SystemExit(f"Static asset conflicts with generated output: {source}")
            if source.is_dir():
                shutil.copytree(source, target, symlinks=True)
            else:
                shutil.copyfile(source, target, follow_symlinks=False)

    digest = bundle_digest(bundle)
    index = render_index((package / "index.html").read_text(encoding="utf-8"), bundle, digest)
    release = assets / digest
    # Rename the complete asset tree into place. A concurrent build may have
    # published the same digest; verify it and use it without rewriting files.
    try:
        if not os.path.lexists(release):
            bundle.rename(release)
    except OSError as error:
        if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
            raise
    if bundle.exists() and bundle_digest(release) != digest:
        raise SystemExit(f"Existing release does not match its digest: {release}")

    with tempfile.TemporaryDirectory(prefix=".index-", dir=out) as stage_index:
        entry = Path(stage_index) / "index.html"
        entry.write_text(index, encoding="utf-8")
        entry.replace(out / "index.html")

wasm = release / "app_bg.wasm"
data = wasm.read_bytes()
print(f"{wasm}: {len(data):,} bytes raw, {len(gzip.compress(data, mtime=0)):,} bytes gzip")
PY

echo
echo "Built $profile_dir -> $out"
printf 'Serve it, for example: python3 scripts/serve-web.py %q 8000\n' "$out"
