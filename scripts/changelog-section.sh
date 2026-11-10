#!/usr/bin/env bash
# Extract one release section from CHANGELOG.md (Keep a Changelog format).
#
# Used by .github/workflows/release.yml to build GitHub Release notes from the
# `## [VERSION] - DATE` section, and by maintainers to preview them.
#
# Usage:
#   scripts/changelog-section.sh 0.1.0            # section for a version
#   scripts/changelog-section.sh v0.1.0           # leading "v" is ignored
#   scripts/changelog-section.sh unreleased       # the [Unreleased] section
#   scripts/changelog-section.sh 0.1.0 FILE       # custom changelog path
#
# Exits 1 when the section does not exist, so a release cannot be published
# with empty notes.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
    "")
        echo "changelog-section: missing version argument (try --help)" >&2
        exit 2
        ;;
esac

TARGET=${1#v}
FILE=${2:-"$ROOT/CHANGELOG.md"}

[[ -f "$FILE" ]] || {
    echo "changelog-section: changelog not found: $FILE" >&2
    exit 1
}

awk -v target="$TARGET" '
    function section_version(line,   inner) {
        if (line !~ /^##[[:space:]]+\[/) return ""
        inner = line
        sub(/^##[[:space:]]+\[/, "", inner)
        sub(/\].*$/, "", inner)
        return inner
    }
    function normalize(v) {
        if (tolower(v) == "unreleased") return "unreleased"
        sub(/^[vV]/, "", v)
        return v
    }
    BEGIN { print_version = normalize(target); in_section = 0; found = 0; n = 0 }
    {
        if ($0 ~ /^##[[:space:]]/) {
            if (in_section) { in_section = 0 }
            version = section_version($0)
            if (version != "" && normalize(version) == print_version) {
                in_section = 1
                found = 1
            }
            next
        }
        if (in_section) {
            body[++n] = $0
        }
    }
    END {
        if (!found) {
            printf "changelog-section: no section for \"%s\" in the changelog\n", target > "/dev/stderr"
            exit 1
        }
        # Trim leading/trailing blank lines so the release body is tidy.
        first = 1
        while (first <= n && body[first] ~ /^[[:space:]]*$/) first++
        last = n
        while (last >= first && body[last] ~ /^[[:space:]]*$/) last--
        for (i = first; i <= last; i++) print body[i]
    }
' "$FILE"
