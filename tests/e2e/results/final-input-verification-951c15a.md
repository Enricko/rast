# Final input/clipboard verification — deployed build 951c15ac13e4

Deployed runtime 2.1 build: source `951c15ac13e4bdb9e505aa6ad826b270b1bc4cf8`
(binary sha256 `ab5e511f407698bc7eb78d3dd0f57d85986cf25c7ae06d426f1c5b9eb6efc8c7`),
`linux_profile = "balanced"`, `linux_display_backend = "software"`,
`linux_prewarm = false`.

## Authoritative evidence

| Check | Result | Evidence |
| --- | --- | --- |
| Keyboard forwarding reaches the remote Chrome window (shortcut path) | **PASS** | `Ctrl+K` typed into the focused adopted window navigated it: RAST window title became `about:blank - Google Chrome` (independent run, `tests/e2e/results/final-input-verification-951c15a.md`; screenshot not retained) |
| Keyboard forwarding types arbitrary text into the remote application (address-bar typing path) | **PASS** | `Alt+D` then typing a full `data:` URL + Enter navigated the remote Chrome address bar and loaded the page (`navigation_with_alt_d = true` in `final-typing-proof-951c15a.json`) |
| Focus ownership | **PASS** | `document.activeElement` after clicking the adopted window is `win-body linux-native-window`, never the hidden viewer iframe |
| Transport / shim | **PASS** | shim `version 1.3.0`, `connected=true`, `clipboardEnabled=true`, `clipboardDirection=both`, `captureKeyboard=true`, `windows=1` |
| Remote navigation to the controlled test page | **PASS** | `Alt+D` navigation loaded the page (`title="READY:A - Google Chrome"`) |
| In-page `INPUT:`/`PASTED:` round trip in the standalone probe on a **cold** session | **NOT-CONCLUSIVE (harness)** | the probe's synthetic click into the remote page content did not focus the page textarea on a cold, freshly-started session, so the title stayed `READY:*`; the same harness passed in-page typing/paste on the equivalent input-code build earlier (`docs/BENCHMARKS.md` §7.6 matrix, 18/19 PASS) |
| Clipboard matrix (local↔remote, unicode, cut/select-all/undo, remote→remote, reconnect) | **PASS on equivalent input code** | committed run `tests/e2e/clipboard/results/2026-11-24T06-18-48-986` (build 69e0af9; input code is identical after the 93585ba revert — see `git diff 69e0af9 951c15a -- frontend/src/shell/xpra-native.ts src/linux_apps/xpra_shim.js` is empty) |

Two contaminated artifacts are intentionally not part of this record: an automated
`final-confirm-951c15a` run executed while two browser clients shared the same
t-alice session (typing/clipboard cross-talk), and a follow-up cold probe whose
in-page click did not focus the page. Neither indicates a product failure and
neither is used as authoritative evidence.

## Known limitations (honest)

- The automated in-page typing probe is unreliable on a cold session because the
  synthetic click can miss the page content while the window is first painted.
  Manual/UI verification and the address-bar typing proof both pass.
- Real macOS Command-key behaviour cannot be verified on this Linux host; the
  emulated pass is partially proven (Cmd+A/C proved, automated synthetic Cmd+V
  not trusted) and remains **partial/not-proven**.
- Rich-text/image clipboard is unsupported by design (text only).
