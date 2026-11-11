#!/usr/bin/env bash
# Verify a staged RAST release directory produced by scripts/build-release.sh
# or downloaded from a GitHub Release.
#
# Checks:
#   * every expected asset exists (both architectures, SHA256SUMS, install.sh,
#     uninstall.sh; rast.toml.example is optional),
#   * SHA256SUMS covers every asset and every checksum matches,
#   * installers are syntactically valid POSIX sh and ShellCheck-clean when
#     the ShellCheck tool is available,
#   * the binary for this machine's architecture runs and reports the expected
#     version (skip with --no-run; binaries for other architectures are only
#     checksum-verified),
#   * GitHub/Sigstore build-provenance attestations for both binaries and
#     SHA256SUMS when --attestations is given (`gh attestation verify`; fails
#     closed when gh is missing or a verification fails).
#
# Usage:
#   scripts/verify-release.sh [--dir DIR] [--version X.Y.Z] [--no-run]
#                             [--attestations] [--repo OWNER/REPO]

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
DIR="$ROOT/dist"
EXPECTED_VERSION=""
RUN_CHECK=1
VERIFY_ATTESTATIONS=0
REPO_SLUG="Enricko/rast"
FAILURES=0

usage() {
    sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir)
            shift
            [[ $# -gt 0 ]] || {
                echo "verify-release: --dir requires a directory" >&2
                exit 2
            }
            DIR=$1
            ;;
        --dir=*) DIR=${1#*=} ;;
        --version)
            shift
            [[ $# -gt 0 ]] || {
                echo "verify-release: --version requires a value" >&2
                exit 2
            }
            EXPECTED_VERSION=${1#v}
            ;;
        --version=*) EXPECTED_VERSION=${1#*=} ; EXPECTED_VERSION=${EXPECTED_VERSION#v} ;;
        --no-run) RUN_CHECK=0 ;;
        --attestations) VERIFY_ATTESTATIONS=1 ;;
        --repo)
            shift
            [[ $# -gt 0 ]] || {
                echo "verify-release: --repo requires OWNER/REPO" >&2
                exit 2
            }
            REPO_SLUG=$1
            ;;
        --repo=*) REPO_SLUG=${1#*=} ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "verify-release: unknown argument: $1 (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

fail() {
    echo "verify-release: FAIL: $*" >&2
    FAILURES=$((FAILURES + 1))
}
ok() { echo "verify-release: ok: $*"; }

[[ -d "$DIR" ]] || {
    echo "verify-release: directory not found: $DIR" >&2
    exit 1
}
DIR=$(cd -- "$DIR" && pwd)

if [[ -z "$EXPECTED_VERSION" ]]; then
    EXPECTED_VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
fi

echo "verify-release: checking $DIR (expected RAST version: $EXPECTED_VERSION)"

# ---------------------------------------------------------------------------
# Expected assets
# ---------------------------------------------------------------------------
REQUIRED=(rast-linux-x86_64 rast-linux-aarch64 SHA256SUMS install.sh uninstall.sh)
OPTIONAL=(rast.toml.example)
for asset in "${REQUIRED[@]}"; do
    if [[ -f "$DIR/$asset" ]]; then
        ok "asset present: $asset"
    else
        fail "missing required asset: $asset"
    fi
done
for asset in "${OPTIONAL[@]}"; do
    if [[ -f "$DIR/$asset" ]]; then
        ok "asset present (optional): $asset"
    fi
done

# ---------------------------------------------------------------------------
# Checksums
# ---------------------------------------------------------------------------
if [[ -f "$DIR/SHA256SUMS" ]]; then
    if (cd "$DIR" && sha256sum --check --strict SHA256SUMS >/dev/null); then
        ok "SHA256SUMS verifies ($(wc -l <"$DIR/SHA256SUMS") file(s))"
    else
        fail "SHA256SUMS does not match the staged files"
    fi
    for asset in rast-linux-x86_64 rast-linux-aarch64; do
        if ! grep -Eq "[[:space:]]\*?${asset//./\\.}$" "$DIR/SHA256SUMS"; then
            fail "SHA256SUMS has no entry for $asset"
        fi
    done
    # Every file except SHA256SUMS itself must be covered by the checksum list.
    while IFS= read -r file; do
        base=$(basename -- "$file")
        if [[ "$base" != "SHA256SUMS" ]] && ! grep -Eq "[[:space:]]\*?${base//./\\.}$" "$DIR/SHA256SUMS"; then
            fail "file not covered by SHA256SUMS: $base"
        fi
    done < <(find "$DIR" -maxdepth 1 -type f -printf '%p\n')
else
    fail "SHA256SUMS is missing; checksums cannot be verified"
fi

# ---------------------------------------------------------------------------
# Installer sanity
# ---------------------------------------------------------------------------
for script in install.sh uninstall.sh; do
    if [[ -f "$DIR/$script" ]]; then
        if sh -n "$DIR/$script" 2>/dev/null; then
            ok "$script passes sh -n syntax check"
        else
            fail "$script is not valid POSIX sh"
        fi
        if command -v shellcheck >/dev/null 2>&1; then
            if shellcheck -s sh -x "$DIR/$script" >/dev/null 2>&1; then
                ok "$script is shellcheck-clean"
            else
                fail "$script has shellcheck findings (run: shellcheck -s sh -x $DIR/$script)"
            fi
        fi
        if [[ -x "$DIR/$script" ]]; then
            ok "$script is executable"
        else
            fail "$script is not executable"
        fi
    fi
done
if [[ -f "$DIR/install.sh" ]] && ! grep -q '^RAST_DEFAULT_VERSION="v' "$DIR/install.sh"; then
    echo "verify-release: WARNING: install.sh is not pinned to a release tag; GitHub release assets should be pinned by the release workflow" >&2
fi

# ---------------------------------------------------------------------------
# Build-provenance attestations (optional, fail closed)
# ---------------------------------------------------------------------------
if [[ "$VERIFY_ATTESTATIONS" -eq 1 ]]; then
    GH_BIN=${GH_BIN:-gh}
    if ! command -v "$GH_BIN" >/dev/null 2>&1; then
        fail "--attestations requires the GitHub CLI ('gh'); install it or omit the flag"
    elif ! "$GH_BIN" attestation verify --help >/dev/null 2>&1; then
        fail "--attestations requires 'gh attestation verify' (GitHub CLI 2.49+); upgrade gh"
    else
        for subject in rast-linux-x86_64 rast-linux-aarch64 SHA256SUMS; do
            if [[ ! -f "$DIR/$subject" ]]; then
                fail "attestation subject missing: $subject"
                continue
            fi
            if "$GH_BIN" attestation verify "$DIR/$subject" --repo "$REPO_SLUG" >/dev/null 2>&1; then
                ok "build provenance attested: $subject ($REPO_SLUG)"
            else
                fail "build provenance verification failed: $subject"
            fi
        done
    fi
fi

# ---------------------------------------------------------------------------
# Binary smoke test (host architecture only)
# ---------------------------------------------------------------------------
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) HOST_ASSET=rast-linux-x86_64 ;;
    Linux-aarch64) HOST_ASSET=rast-linux-aarch64 ;;
    *) HOST_ASSET="" ;;
esac
if [[ "$RUN_CHECK" -eq 1 && -n "$HOST_ASSET" && -f "$DIR/$HOST_ASSET" ]]; then
    tmp_bin=$(mktemp "${TMPDIR:-/tmp}/rast-verify.XXXXXX")
    cp -- "$DIR/$HOST_ASSET" "$tmp_bin"
    chmod 0755 "$tmp_bin"
    if reported=$("$tmp_bin" version 2>/dev/null | head -n 1 | awk '{print $2}') && [[ -n "$reported" ]]; then
        if [[ "$reported" == "$EXPECTED_VERSION" ]]; then
            ok "$HOST_ASSET runs and reports version $reported"
        else
            fail "$HOST_ASSET reports version '$reported', expected '$EXPECTED_VERSION'"
        fi
    else
        fail "$HOST_ASSET does not run on this host"
    fi
    rm -f -- "$tmp_bin"
elif [[ "$RUN_CHECK" -eq 1 ]]; then
    echo "verify-release: skipping binary run check (host is $(uname -s)/$(uname -m))"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
if [[ "$FAILURES" -gt 0 ]]; then
    echo "verify-release: $FAILURES check(s) failed" >&2
    exit 1
fi
echo "verify-release: all checks passed"
