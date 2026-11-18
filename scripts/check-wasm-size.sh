#!/usr/bin/env bash
# Measure the Leptos UI build (crates/rast-ui/dist: the .wasm and glue .js;
# Trunk's index.html is a build entry and is not served or counted) and fail
# when its gzip -9 size exceeds the budget in crates/rast-ui/wasm-budget.txt.
# Prints the Markdown table of scripts/measure-bundle.sh (also for CI logs and
# the job summary).
#
# Usage: scripts/check-wasm-size.sh [dist-dir]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist="${1:-$root/crates/rast-ui/dist}"
budget_file="$root/crates/rast-ui/wasm-budget.txt"

[ -d "$dist" ] || { echo "$dist does not exist; run scripts/build-ui.sh first" >&2; exit 1; }
budget="$(grep -E '^[0-9]+$' "$budget_file" | head -n 1)"
[ -n "$budget" ] || { echo "no budget found in $budget_file" >&2; exit 1; }

table="$("$root/scripts/measure-bundle.sh" "$dist" --only-ext wasm,js)"
echo "$table"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "### rast-ui size"; echo; echo "$table"; } >> "$GITHUB_STEP_SUMMARY"
fi

gzip_total="$("$root/scripts/measure-bundle.sh" "$dist" --only-ext wasm,js --tsv | awk -F'\t' '$1 == "TOTAL" { print $3 }')"
echo
echo "gzip total: $gzip_total B, budget: $budget B"
if [ "$gzip_total" -gt "$budget" ]; then
  echo "FAIL: the WASM UI exceeds the gzip budget by $((gzip_total - budget)) B" >&2
  exit 1
fi
echo "OK: $((budget - gzip_total)) B under budget"
