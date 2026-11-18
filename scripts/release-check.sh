#!/usr/bin/env bash
# Resolve and check a release version before the Release workflow builds it.
#
# Used by .github/workflows/release.yml (prepare job) for both a pushed tag
# and a manual run (workflow_dispatch). Prints GitHub output lines on stdout:
#
#   version=1.0.1-alpha.2
#   tag=v1.0.1-alpha.2
#   prerelease=true
#
# Rules:
#   - The version must be X.Y.Z, optionally with a -pre-release suffix and/or
#     +build metadata; a leading "v" is ignored.
#   - A version with a -suffix (1.0.1-alpha.2, 1.0.1-rc.1) is always published
#     as a pre-release, even from a pushed tag or with the box unticked.
#     GitHub never reports a pre-release as /releases/latest, so `rast update`
#     never offers it to other servers.
#   - A version without a -suffix (a final release) is refused unless its
#     commit is on the main branch, so a development branch such as Remake is
#     never published as /releases/latest. This also holds when the
#     pre-release box is ticked.
#
# Usage:
#   scripts/release-check.sh [--prerelease true|false] [--commit REV]
#                            [--main-ref REF] VERSION
#
#   --prerelease   the manual run's "prerelease" input (default false)
#   --commit       the commit being released (default HEAD)
#   --main-ref     the main branch ref (default origin/main)
#
# Exits 1 when the release must not be published, 2 on a usage error.

set -euo pipefail

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

die() {
    echo "release-check: $*" >&2
    exit 1
}

PRERELEASE_INPUT=false
COMMIT=HEAD
MAIN_REF=origin/main
RAW=""

need_value() {
    [[ $# -ge 2 && -n "$2" ]] || {
        echo "release-check: $1 requires a value" >&2
        exit 2
    }
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        --prerelease)
            need_value "$@"
            PRERELEASE_INPUT=$2
            shift 2
            ;;
        --commit)
            need_value "$@"
            COMMIT=$2
            shift 2
            ;;
        --main-ref)
            need_value "$@"
            MAIN_REF=$2
            shift 2
            ;;
        -*)
            echo "release-check: unknown option: $1 (try --help)" >&2
            exit 2
            ;;
        *)
            [[ -z "$RAW" ]] || {
                echo "release-check: more than one version given" >&2
                exit 2
            }
            RAW=$1
            shift
            ;;
    esac
done

[[ -n "$RAW" ]] || {
    echo "release-check: missing version argument (try --help)" >&2
    exit 2
}

case "$PRERELEASE_INPUT" in
    true | false) ;;
    *)
        echo "release-check: --prerelease must be true or false, got '$PRERELEASE_INPUT'" >&2
        exit 2
        ;;
esac

VERSION=${RAW#v}
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
    die "invalid release version: '$RAW' (expected vX.Y.Z or X.Y.Z, optionally with -suffix)"
fi

# The pre-release part is a "-" right after X.Y.Z; "+build" metadata alone
# does not make a pre-release.
if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+- ]]; then
    HAS_SUFFIX=true
else
    HAS_SUFFIX=false
fi

if [[ "$HAS_SUFFIX" == true ]]; then
    PRERELEASE=true
    if [[ "$PRERELEASE_INPUT" != true ]]; then
        echo "release-check: $VERSION has a pre-release suffix; publishing it as a pre-release" >&2
    fi
else
    PRERELEASE=$PRERELEASE_INPUT
    commit_sha=$(git rev-parse --verify --quiet "${COMMIT}^{commit}") ||
        die "cannot resolve the release commit '$COMMIT'"
    main_sha=$(git rev-parse --verify --quiet "${MAIN_REF}^{commit}") ||
        die "cannot resolve the main branch '$MAIN_REF' (fetch the full history)"
    if ! git merge-base --is-ancestor "$commit_sha" "$main_sha"; then
        die "refusing to release $VERSION: commit $commit_sha is not on $MAIN_REF. A version without a -suffix is a final release; release it from main, or use a pre-release version such as ${VERSION%%+*}-alpha.1"
    fi
fi

echo "version=$VERSION"
echo "tag=v$VERSION"
echo "prerelease=$PRERELEASE"
