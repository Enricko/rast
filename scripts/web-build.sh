#!/usr/bin/env bash
# Build the RAST shell for the browser (Rune's experimental WASM + WebGPU target).
#
#   scripts/web-build.sh [--dev] [--out DIR]
#
# Requirements:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.129 --locked
#
# The output folder is a plain static site. Serve it over HTTPS (plain HTTP works
# on localhost) and serve `.wasm` as `application/wasm`. The page needs WebGPU:
# Rune's web target renders one canvas through it.
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
            [[ $# -ge 2 ]] || { echo "--out needs a directory" >&2; exit 2; }
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

rm -rf "$out"
mkdir -p "$out"

# wasm-bindgen writes app.js + app_bg.wasm (+ snippets/) for a `--target web`
# module. The page imports app.js and awaits its default export. A release build
# drops the wasm function-name section (over 40% of an unoptimized build).
bindgen_args=(--target web --out-dir "$out" --out-name app)
$dev || bindgen_args+=(--remove-name-section)
wasm-bindgen "${bindgen_args[@]}" "$artifact"

cp "crates/$package/index.html" "$out/index.html"
cp "crates/$package/boot.js" "$out/boot.js"

# Rune embeds a Latin/Greek/Cyrillic cut of Inter and fetches the full faces from
# beside app.js after the first frame. Copy the ones the build embedded, if any;
# without them text outside the cut draws in fallback fonts.
cut=$(tr ' ' '\n' <"${artifact%.wasm}.d" 2>/dev/null | grep -m1 '/assets/web/first/Inter-Regular\.ttf$' || true)
if [[ -n $cut ]]; then
    cp "${cut%/first/*}"/Inter-*.ttf "$out/"
fi

python3 - "$out/app_bg.wasm" <<'PY'
import gzip
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = path.read_bytes()
print(f"{path}: {len(data):,} bytes raw, {len(gzip.compress(data, mtime=0)):,} bytes gzip")
PY

echo
echo "Built $profile_dir -> $out"
echo "Serve it, for example: python3 -m http.server 8000 -d $out"
