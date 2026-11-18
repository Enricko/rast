#!/usr/bin/env bash
# Print raw, gzip (-9) and brotli (quality 11) sizes of every file under a
# directory as a Markdown table, plus a total row. Used for the JS baseline
# (frontend/dist) and for the WASM budget check (crates/rast-ui/dist).
#
# Usage: scripts/measure-bundle.sh <dir> [--group-ext ext[,ext...]] [--tsv]
#   --group-ext  fold files with these extensions into one row (e.g. jpg,png)
#   --skip-ext   leave out files with these extensions (e.g. jpg,png,map)
#   --only-ext   measure only files with these extensions (e.g. wasm,js)
#   --tsv        machine-readable output: "name<TAB>raw<TAB>gzip<TAB>brotli"
#
# Brotli comes from node's built-in zlib, so nothing needs installing.
set -euo pipefail

dir=""
group=""
skip=""
only=""
tsv=0
while [ $# -gt 0 ]; do
  case "$1" in
    --group-ext) group="${2:?--group-ext needs a value}"; shift 2 ;;
    --skip-ext) skip="${2:?--skip-ext needs a value}"; shift 2 ;;
    --only-ext) only="${2:?--only-ext needs a value}"; shift 2 ;;
    --tsv) tsv=1; shift ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) dir="$1"; shift ;;
  esac
done
if [ -z "$dir" ] || [ ! -d "$dir" ]; then
  echo "usage: $0 <dir> [--group-ext jpg,png] [--tsv]" >&2
  exit 2
fi
command -v node >/dev/null || { echo "node is required (brotli via zlib)" >&2; exit 2; }

node - "$dir" "$group" "$tsv" "$skip" "$only" <<'JS'
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');
const [dir, group, tsv, skip, only] = process.argv.slice(2);
const onlyExt = new Set(only.split(',').filter(Boolean).map((e) => e.toLowerCase()));
const skipExt = new Set(skip.split(',').filter(Boolean).map((e) => e.toLowerCase()));
const groupExt = new Set(group.split(',').filter(Boolean).map((e) => e.toLowerCase()));
const files = [];
(function walk(d) {
  for (const e of fs.readdirSync(d, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.isFile()) files.push(p);
  }
})(dir);
const rows = new Map();
const add = (name, raw, gz, br) => {
  const r = rows.get(name) || { raw: 0, gz: 0, br: 0, n: 0 };
  r.raw += raw; r.gz += gz; r.br += br; r.n += 1;
  rows.set(name, r);
};
for (const f of files) {
  if (onlyExt.size && !onlyExt.has(path.extname(f).slice(1).toLowerCase())) continue;
  if (skipExt.has(path.extname(f).slice(1).toLowerCase())) continue;
  const buf = fs.readFileSync(f);
  const gz = zlib.gzipSync(buf, { level: 9 }).length;
  const br = zlib.brotliCompressSync(buf, {
    params: {
      [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
      [zlib.constants.BROTLI_PARAM_SIZE_HINT]: buf.length,
    },
  }).length;
  const ext = path.extname(f).slice(1).toLowerCase();
  const rel = path.relative(dir, f);
  add(groupExt.has(ext) ? `(${[...groupExt].join('/')} files)` : rel, buf.length, gz, br);
}
let t = { raw: 0, gz: 0, br: 0 };
for (const r of rows.values()) { t.raw += r.raw; t.gz += r.gz; t.br += r.br; }
if (tsv === '1') {
  for (const [n, r] of rows) console.log(`${n}\t${r.raw}\t${r.gz}\t${r.br}`);
  console.log(`TOTAL\t${t.raw}\t${t.gz}\t${t.br}`);
} else {
  console.log('| File | Raw (B) | Gzip -9 (B) | Brotli q11 (B) |');
  console.log('| --- | ---: | ---: | ---: |');
  for (const [n, r] of rows) {
    const label = r.n > 1 ? `${n} (${r.n})` : n;
    console.log(`| \`${label}\` | ${r.raw} | ${r.gz} | ${r.br} |`);
  }
  console.log(`| **Total** | **${t.raw}** | **${t.gz}** | **${t.br}** |`);
}
JS
