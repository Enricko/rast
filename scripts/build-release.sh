#!/usr/bin/env bash
# Build a complete, release-ready RAST artifact set into dist/.
#
# Steps:
#   1. Build the web frontend with Node.js + npm (embedded by rust-embed).
#   2. Build the release binary with cargo.
#   3. Check that the binary reports the version in Cargo.toml.
#   4. Stage release assets under dist/: rast-linux-<arch>, SHA256SUMS,
#      install.sh (pinned to this release), uninstall.sh, rast.toml.example.
#
# The artifact names and the layout of SHA256SUMS are exactly what
# installer/install.sh expects on GitHub Releases.
#
# Usage:
#   scripts/build-release.sh [--skip-frontend] [--out DIR] [--no-locked]
#
# Requirements: Rust (rust-toolchain.toml), Node.js >= 22, npm, sha256sum.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT_DIR="$ROOT/dist"
SKIP_FRONTEND=0
CARGO_LOCKED="--locked"

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-frontend) SKIP_FRONTEND=1 ;;
        --out)
            shift
            [[ $# -gt 0 ]] || {
                echo "build-release: --out requires a directory" >&2
                exit 2
            }
            OUT_DIR=$1
            ;;
        --out=*) OUT_DIR=${1#*=} ;;
        --no-locked) CARGO_LOCKED="" ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "build-release: unknown argument: $1 (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

cd "$ROOT"

command -v cargo >/dev/null 2>&1 || {
    echo "build-release: cargo not found; install Rust (see rust-toolchain.toml)" >&2
    exit 1
}
command -v sha256sum >/dev/null 2>&1 || {
    echo "build-release: sha256sum not found" >&2
    exit 1
}

# Never build a release from a dirty tree without telling the user.
if [[ -d .git ]] && command -v git >/dev/null 2>&1; then
    if ! git diff --quiet --ignore-submodules HEAD 2>/dev/null; then
        echo "build-release: WARNING: the working tree has uncommitted changes; the binary will report the last commit SHA" >&2
    fi
fi

VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[[ -n "$VERSION" ]] || {
    echo "build-release: cannot read the package version from Cargo.toml" >&2
    exit 1
}

case "$(uname -s)" in
    Linux) ;;
    *)
        echo "build-release: releases are built for Linux only (this is $(uname -s))" >&2
        exit 1
        ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    aarch64 | arm64) ARCH=aarch64 ;;
    *)
        echo "build-release: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

echo "build-release: RAST $VERSION for linux/$ARCH"

# ---------------------------------------------------------------------------
# 1. Frontend
# ---------------------------------------------------------------------------
if [[ "$SKIP_FRONTEND" -eq 1 ]]; then
    echo "build-release: skipping the frontend build (--skip-frontend)"
    if [[ ! -f frontend/dist/index.html ]]; then
        echo "build-release: frontend/dist is missing; refusing to build a release without UI assets" >&2
        exit 1
    fi
else
    command -v node >/dev/null 2>&1 || {
        echo "build-release: node not found; install Node.js >= 22" >&2
        exit 1
    }
    command -v npm >/dev/null 2>&1 || {
        echo "build-release: npm not found; install Node.js >= 22" >&2
        exit 1
    }
    node_major=$(node -p 'process.versions.node.split(".")[0]')
    if ((node_major < 22)); then
        echo "build-release: Node.js >= 22 required (found $(node --version))" >&2
        exit 1
    fi
    echo "build-release: building frontend with Node.js $(node --version) / npm $(npm --version)"
    (
        cd frontend
        if [[ -f package-lock.json ]]; then
            npm ci
        else
            echo "build-release: package-lock.json missing, falling back to npm install" >&2
            npm install
        fi
        npm run build
    )
fi

[[ -f frontend/dist/index.html ]] || {
    echo "build-release: frontend/dist/index.html is missing after the build" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# 2. Rust release binary
# ---------------------------------------------------------------------------
echo "build-release: building cargo release binary"
# shellcheck disable=SC2086 # CARGO_LOCKED is empty or "--locked"
cargo build --release $CARGO_LOCKED

BIN="target/release/rast"
[[ -x "$BIN" ]] || {
    echo "build-release: $BIN not found after the build" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# 3. Version self-check
# ---------------------------------------------------------------------------
reported=$("$BIN" version | head -n 1 | awk '{print $2}')
if [[ "$reported" != "$VERSION" ]]; then
    echo "build-release: binary reports version '$reported' but Cargo.toml says '$VERSION'" >&2
    exit 1
fi
echo "build-release: version self-check passed ($reported)"

# ---------------------------------------------------------------------------
# 4. Stage release assets
# ---------------------------------------------------------------------------
mkdir -p "$OUT_DIR"
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/rast-stage.XXXXXX")
cleanup() { rm -rf -- "$STAGE"; }
trap cleanup EXIT

ASSET="rast-linux-$ARCH"
cp -- "$BIN" "$STAGE/$ASSET"
chmod 0755 "$STAGE/$ASSET"

# Pin the installer's default version so that the install.sh attached to a
# release always installs that release (and not whatever is newest by then).
sed "s|^RAST_DEFAULT_VERSION=.*|RAST_DEFAULT_VERSION=\"v$VERSION\"|" \
    installer/install.sh >"$STAGE/install.sh"
chmod 0755 "$STAGE/install.sh"
cp -- installer/uninstall.sh "$STAGE/uninstall.sh"
chmod 0755 "$STAGE/uninstall.sh"
cp -- deploy/rast.toml.example "$STAGE/rast.toml.example"

# Sanity-check the pinned installer and POSIX syntax before shipping it.
if ! sh -n "$STAGE/install.sh" || ! sh -n "$STAGE/uninstall.sh"; then
    echo "build-release: generated installers failed the sh -n syntax check" >&2
    exit 1
fi

(
    cd "$STAGE"
    sha256sum rast-linux-* install.sh uninstall.sh rast.toml.example >SHA256SUMS
)

cp -f -- "$STAGE"/* "$OUT_DIR"/
echo
echo "build-release: release assets written to $OUT_DIR"
(
    cd "$OUT_DIR"
    sha256sum --check SHA256SUMS
    ls -l rast-linux-* install.sh uninstall.sh SHA256SUMS rast.toml.example
)
echo
echo "build-release: done. Verify with scripts/verify-release.sh --dir $OUT_DIR"
