#!/bin/bash
# chrome-gpu-check.sh — prove which GL renderer Chrome actually uses on a display.
#
# Starts a private Chrome (own --user-data-dir, own debug port) on the given X
# display, opens chrome://gpu, and prints the GPU feature summary + GL_RENDERER
# through the DevTools protocol. Nothing is installed and the production Chrome
# profile is never touched.
#
# Usage:
#   scripts/gpu/chrome-gpu-check.sh :110
#   DISPLAY=:110 scripts/gpu/chrome-gpu-check.sh        # defaults to $DISPLAY
#
# Exit status: 0 when chrome://gpu reports an accelerated compositor/rasterizer
# with an ANGLE/NVIDIA or ANGLE/Intel renderer, 1 otherwise (printed plainly).
set -u

display="${1:-${DISPLAY:-}}"
[ -n "$display" ] || { echo "error: pass a display, e.g. $0 :110" >&2; exit 2; }
chrome="${CHROME_BIN:-$(command -v google-chrome || command -v google-chrome-stable || command -v chromium)}"
[ -n "$chrome" ] || { echo "error: no Chrome/Chromium binary found" >&2; exit 2; }
command -v node >/dev/null 2>&1 || { echo "error: node is required for the DevTools probe" >&2; exit 2; }

port="${CHROME_DEBUG_PORT:-9333}"
profile="$(mktemp -d "${TMPDIR:-/tmp}/rast-chrome-gpu.XXXXXX")"
log="$profile/chrome.log"
node_script="$profile/read-gpu.mjs"

cat >"$node_script" <<'JS'
const port = Number(process.env.CHROME_DEBUG_PORT || 9333);
const base = `http://127.0.0.1:${port}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitForChrome() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`${base}/json/version`);
      if (r.ok) return await r.json();
    } catch {}
    await sleep(200);
  }
  throw new Error("Chrome DevTools endpoint did not come up");
}
await waitForChrome();
let target;
for (let i = 0; i < 20 && !target; i++) {
  try {
    const r = await fetch(`${base}/json/new`, { method: "PUT" });
    if (r.ok) target = await r.json();
  } catch {}
  if (!target) await sleep(250);
}
if (!target) throw new Error("could not create a DevTools target");
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let id = 0;
const pending = new Map();
const send = (method, params = {}) => new Promise((res, rej) => {
  const mid = ++id;
  pending.set(mid, { res, rej });
  ws.send(JSON.stringify({ id: mid, method, params }));
});
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  const p = pending.get(m.id);
  if (!p) return;
  pending.delete(m.id);
  m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result);
};
await send("Page.enable");
await send("Runtime.enable");
await send("Page.navigate", { url: "chrome://gpu" });
await sleep(9000);
const expr = `(() => {
  const parts = [];
  const visit = (root) => {
    for (const n of root.children || []) {
      if (n.shadowRoot) { parts.push(n.shadowRoot.textContent || ""); visit(n.shadowRoot); }
      visit(n);
    }
    if (root === document && document.body) parts.push(document.body.textContent || "");
  };
  visit(document);
  return parts.join("\\n");
})()`;
const out = await send("Runtime.evaluate", { expression: expr, returnByValue: true });
ws.close();
console.log(out?.result?.value || "");
JS

cleanup() {
  [ -n "${chrome_pid:-}" ] && kill "$chrome_pid" 2>/dev/null
    wait "$chrome_pid" 2>/dev/null
  rm -rf "$profile"
}
trap cleanup EXIT

echo "display: $display"
echo "chrome:  $chrome ($("$chrome" --version 2>/dev/null))"
DISPLAY="$display" "$chrome" \
    --no-sandbox --disable-dev-shm-usage \
    --user-data-dir="$profile/data" \
    --remote-debugging-port="$port" --remote-allow-origins='*' \
    --no-first-run --disable-features=Translate \
    about:blank >"$log" 2>&1 &
chrome_pid=$!

CHROME_DEBUG_PORT="$port" node "$node_script" >"$profile/gpu.txt" 2>"$profile/node.err"
node_rc=$?
if [ "$node_rc" != 0 ]; then
    echo "DevTools probe failed:" >&2
    cat "$profile/node.err" >&2
    tail -20 "$log" >&2
    exit 1
fi

# chrome://gpu renders its sections inside shadow DOM, so the text has to be
# read recursively (done in the node helper). Fault the fields out with sed:
# join everything on one line, then take the value between two known fields.
text="$(tr -d '\n' <"$profile/gpu.txt")"
compact="$(printf '%s' "$text" | tr -d ' \t')"
show() { printf '  %s\n' "$1"; }
field() { # field <name> <next-field-name> <compacted-text>
    printf '%s' "$3" | sed -n "s/.*${1}:\(.*\)${2}.*/\1/p" | head -1
}
echo
echo "chrome://gpu highlights:"
renderer="$(field GL_RENDERER GL_VERSION "$compact")"
vendor="$(field GL_VENDOR GL_RENDERER "$compact")"
gpu0="$(field GPU0 GPU1 "$compact")"
[ -n "$renderer" ] && show "GL_RENDERER: $renderer"
[ -n "$vendor" ] && show "GL_VENDOR:   $vendor"
[ -n "$gpu0" ] && show "GPU0:        $gpu0"
for feature in "Canvas" "Compositing" "Rasterization" "WebGL" "WebGPU" "Video Decode" "Video Encode" "Vulkan"; do
    value="$(printf '%s' "$text" | grep -oE "${feature}:[[:space:]]*(Hardware accelerated|Software only[^A-Z]*|Disabled)" | head -1)"
    [ -n "$value" ] && show "* $value"
done

if [ -z "$renderer" ]; then
    echo
    echo "RESULT: chrome://gpu did not report a GL_RENDERER (Chrome may have failed on this display)"
    echo "--- chrome log tail ---"
    tail -15 "$log" 2>/dev/null
    exit 1
fi
echo
case "$renderer" in
    *NVIDIA*|*Intel*) show "RESULT: hardware GPU renderer reported by Chrome"; exit 0 ;;
    *llvmpipe*|*SwiftShader*|*swiftshader*)
        show "RESULT: software renderer ($renderer) - GPU acceleration NOT active"; exit 1 ;;
    *) [ -n "$renderer" ] && show "RESULT: renderer reported: $renderer" || show "RESULT: no GL renderer reported"; exit 1 ;;
esac
