# Input and clipboard for native Linux apps

This document describes how RAST routes keyboard, pointer and clipboard input
to native Linux applications (the shared Xpra seamless session), which browser
limitations are real, and how to diagnose a problem.

The implementation lives in two files:

* `frontend/src/shell/xpra-native.ts` — the desktop side: focus ownership, key
  policy, pointer glue, and the two-way clipboard manager.
* `src/linux_apps/xpra_shim.js` — the bridge inside the hidden Xpra HTML5
  viewer iframe: client capture, window snapshots/adoption, synthetic key
  delivery, mouse coordinate rebasing, and the clipboard token/request patches.

The transport architecture is unchanged: **one** shared Xpra session, **one**
hidden same-origin viewer iframe, and each remote top-level window adopted into
its own RAST window body. Input is never implemented with per-window
connections or extra viewers.

## Where focus lives (the root cause of the old bugs)

The Xpra HTML5 client (pinned v21 with Xpra 6.5.5) focuses a `#pasteboard`
element inside its own document and re-focuses it on blur (its clipboard
"autofocus"). In RAST the viewer iframe is off-screen, so that behaviour:

* pulled every keystroke into the hidden viewer document, bypassing the
  desktop's key handling (`forwardKey` never ran);
* left the browser clipboard APIs running in an *unfocused* document, where
  browsers reject `navigator.clipboard.readText()/writeText()` — and the old
  code swallowed the rejection, so copy/paste failed silently;
* competed with the RAST window focus, which looked like delayed typing.

The shim now releases that autofocus (it replaces the node, which drops the
viewer's re-focus handler) and asks the desktop to focus the adopted window
body. The RAST desktop document is the single keyboard/clipboard owner:

* clicking an application surface focuses its `.win-body.linux-native-window`;
* the hidden viewer iframe is `tabindex="-1"`, `aria-hidden`, off-screen and
  never focused; if anything does focus it, the desktop takes focus back;
* the shim pins `client.capture_keyboard = true`, because only the desktop
  sends keys to the client.

## Keyboard

Every `keydown`/`keyup` in the desktop document is classified once by
`decideNativeKey()`:

| Decision | Meaning |
| --- | --- |
| `shell` | RAST keeps the key: no native window focused, focus is on a RAST control (form field, taskbar, start menu, panel, tray, dialog/modals), an IME composition is running, or the session is disconnected. |
| `reserved` | The browser/OS owns the combination. It is never forwarded and never cancelled. |
| `forward` | Exactly one synthetic key event is sent to the focused native window through the shim. |

Forwarding details:

* **No duplicate delivery.** The same physical event can only take one path:
  if the desktop document has focus the parent forwards it; if the hidden
  viewer somehow has focus the client handles it natively. There is no path
  where both run. Unit tests assert one remote keydown and one keyup per event.
* **Modifier presses are forwarded** (Shift/Control/Alt/Meta) so remote chords
  and modifier state (`Control+click`, Shift+drag, …) work.
* **Repeat is preserved**: auto-repeat keydowns are forwarded one-for-one with
  `repeat=true`.
* **Focus loss never leaves keys stuck**: `window.blur`, `visibilitychange`
  to hidden, viewer disconnect/reload, session end and frame teardown all send
  the missing keyups (`flushNativeInput`), and `keyup` is still forwarded when
  focus moved to a RAST control while the key was held.
* **IME/composition is not supported** by the pinned HTML5 client v21 (it has
  no composition/commit packet path). Composition events are kept by the
  browser (`shell` decision) and never produce garbage key events. Use
  clipboard paste for CJK/emoji/accented text that the keymap cannot produce.
* **Pointer lock, media keys and OS-level shortcuts** are not intercepted.

### macOS: Command → Linux Control

macOS users expect Command; Linux applications expect Control. The pinned Xpra
HTML5 client already implements exactly this translation in its `swap_keys`
mode: it maps `Meta` to `Control` (and `Control` to `Meta`) for **key and mouse
modifiers**, and it delays a Ctrl/Cmd+V key packet by 100 ms so the browser
paste event can push the clipboard token first. The shim pins
`client.swap_keys` from the detected desktop platform (`navigator.platform` /
`navigator.userAgentData.platform`, generic — never Chrome-specific).

RAST therefore forwards the **real Command event** (Meta stays Meta) and only
decides *which* combinations are forwarded. Rewriting Meta to Control in the
frontend would be swapped a second time by the client and reach the server as
Meta. On a macOS client, Control itself is swapped to Meta — xpra's documented
behaviour; this is why Cmd (not Ctrl) is the shortcut modifier for native apps.

Forwarded Command combinations (Linux applications receive their Control
equivalent):

| macOS | Linux app receives | Notes |
| --- | --- | --- |
| `Cmd+A` | Ctrl+A | select all |
| `Cmd+C` | Ctrl+C | copy; browser copy action is cancelled (the app copies) |
| `Cmd+X` | Ctrl+X | cut |
| `Cmd+V` | Ctrl+V | paste; the browser's paste default action is kept so the desktop receives the paste event (shim delays the key 100 ms so the token arrives first) |
| `Cmd+Shift+V` | Ctrl+Shift+V | paste special in terminals/editors |
| `Cmd+Z` | Ctrl+Z | undo |
| `Cmd+Shift+Z` | Ctrl+Shift+Z | redo |
| `Cmd+Y` | Ctrl+Y | redo (Linux convention) |
| `Cmd+S` | Ctrl+S | save (browser save dialog is cancelled) |

Deliberately **left to the browser** (never forwarded, never cancelled; a web
page cannot reliably override most of them anyway):

| Reserved | Browser action |
| --- | --- |
| `Cmd+W`, `Cmd+Shift+W` | close tab/window |
| `Cmd+T`, `Cmd+Shift+T` | new tab / reopen tab |
| `Cmd+N`, `Cmd+Shift+N` | new window / incognito |
| `Cmd+L` | address bar |
| `Cmd+R`, `Cmd+Shift+R` | reload / hard reload |
| `Cmd+P` | print |
| `Cmd+M` | minimise window |
| `Cmd+H` | hide browser |
| `Cmd+F` | find bar (kept browser-side by RAST policy) |
| `Cmd+Q` | quit browser |
| `Cmd+O`, `Cmd+,` | open file / settings |
| `Cmd+1…9`, `Cmd+[`, `Cmd+]`, `` Cmd+` `` | tab/window navigation |
| `Cmd+←/→/↑/↓`, `Cmd+Backspace`, `Cmd+Delete`, `Cmd+Tab`, `Cmd+Escape` | navigation / OS |
| `Cmd+Shift+C` | DevTools inspect element |
| `Cmd+Opt+I/J/C/K`, `Cmd+Ctrl+*` | DevTools / OS combinations |
| any other `Cmd+…` | left to the browser rather than guessing a Linux shortcut |

`F5`, `F11`, `F12` stay with the browser on every platform.

### Windows/Linux clients

| Combination | Behaviour |
| --- | --- |
| `Ctrl+C` / `Ctrl+X` | forwarded (copy/cut gesture; double-click selection etc. work) |
| `Ctrl+V`, `Ctrl+Shift+V`, `Shift+Insert` | forwarded as a paste gesture; the browser paste event delivers the clipboard text (see [Clipboard](#clipboard)) |
| `Ctrl+A`, `Ctrl+Z`, `Ctrl+S`, `Ctrl+F`, other `Ctrl+<key>` (not reserved) | forwarded to the application |
| `Ctrl+W/T/N/L/R/P`, `Ctrl+1…9`, `Ctrl+Shift+W/T/N/R/P` | browser-reserved: not forwarded |
| `Ctrl+Shift+I/J/C/K` | DevTools: not forwarded |
| `Alt+<key>`, `Alt+Tab`, `Shift+…` | forwarded (Alt+Tab is normally intercepted by the OS before the page) |
| Meta/Windows/Super key | not forwarded (OS) |

When a browser does not allow a page to cancel a reserved shortcut (tab
management, navigation), the application simply does not see it. Do not
configure a native app expecting to receive browser shortcuts.

## Pointer and wheel

The remote Xpra window's canvas is adopted into the RAST window body and
rendered 1:1 (Xpra session DPI 96, `client.scale = 1`). Coordinates are rebased
by the shim's `getMouse()` patch:

```
x = win.x + (event.clientX - contentRect.left) * win.w / contentRect.width
y = win.y + (event.clientY - contentRect.top)  * win.h / contentRect.height
```

`win.x/win.y` are the remote window's server-side position, so the absolute
coordinates sent to the X server land inside the remote window; the scale
factor makes the mapping correct under browser zoom and HiDPI (CSS pixels vs
canvas pixels). Motion, button and wheel events all use this single path, so a
click never drifts from the point where the cursor is drawn.

* **Buttons**: left/middle/right keep their X11 numbers (1/2/3, wheel 4–7);
  the jQuery/native event wrappers are both handled.
* **Drags** are released on the visible desktop document (`pointerup`,
  `pointercancel`, `window.blur`) so a drag that ends outside the adopted
  window, on the title bar, or in another application cannot leave a remote
  button pressed.
* **Wheel**: vertical/horizontal deltas are passed through the Xpra client's
  normalizer. Some browsers report Shift+wheel as a vertical delta only; the
  shim converts that to the horizontal component (Linux applications expect a
  horizontal scroll while Shift is held). Real horizontal trackpad deltas are
  untouched (Shift+wheel → scroll-left/right).
* **Double-click** is generated natively by the browser from two forwarded
  clicks; no extra handling is needed.
* **Context menu**: right-click inside the adopted surface no longer opens the
  *browser* page menu; the application receives the click and draws its own
  X11 context/pop-up menu (adopted as a frameless RAST popup). Middle-click
  auto-scroll and HTML5 drag ghosting are suppressed for the same reason.
* **Cursor changes** come from the Xpra client (`cursor` CSS on the adopted
  node) and are unaffected by the RAST window border.
* Focus for pointer input is moved to the RAST window body on `pointerdown`
  (cheap, no round trip); no per-event network round trips are added to the
  input path — key/mouse packets still go straight to the existing connection.

## Clipboard

Two-way clipboard is implemented through the Xpra shim's clipboard API. The
desktop's focused document — not the hidden viewer — performs every browser
clipboard API call:

| Direction | Path |
| --- | --- |
| remote app copy → browser | server clipboard token → shim `_process_clipboard_token` patch → shim `clipboard` event → desktop `navigator.clipboard.writeText(text)`. Text targets only (see formats). |
| browser copy → remote app paste (keyboard) | desktop `paste` event (trusted user gesture, `clipboardData`) → shim `sendClipboardText` → server clipboard token; the forwarded Ctrl/Cmd+V key is delayed 100 ms by the client so the token is there before the app asks. |
| browser copy → remote app paste (app asks) | server clipboard request → shim defers it to the desktop (750 ms timeout) → desktop answers from the last RAST copy or a quiet `readText`, or declines so the client's own fallback runs; the application's paste never hangs. |

Rules:

* **Never silent.** If `navigator.clipboard.writeText()` is refused (page not
  focused, permission denied), RAST shows a warning toast with a **Copy now**
  action (retry inside a real user gesture), and if that still fails an
  explicit fallback dialog shows the copied text in a read-only area that can
  be selected and copied by hand.
* **Explicit paste fallback.** If a paste gesture produces no paste event, or
  the application's clipboard request cannot be answered, RAST shows a
  **Paste into the native application** dialog: the person pastes the text
  into a text area (a normal trusted paste) and presses *Send to application*;
  RAST pushes it to the session clipboard and sends a Ctrl+V (Cmd+V on macOS)
  to the focused window.
* **No loops / no stale overwrites.** Identical tokens are de-duplicated on
  both sides (`clipboardState.lastText` in the shim, `lastWritten` in the
  desktop manager); rapid bursts are coalesced and the newest text always
  wins; text the desktop itself pushed is never written back over the browser
  clipboard when the server echoes it.
* **Reconnect-aware.** After a viewer/session reconnect the desktop re-pushes
  the browser clipboard to the session and asks the Xpra client to re-poll, so
  pasting into applications keeps working. The server's own clipboard is
  synced on the next copy inside the application; there is no client-side API
  in Xpra to pull it back proactively.
* **Best effort per browser.** Text is transferred; rich content is not (see
  below).

### Formats: text only (honest limitation)

The negotiated path in this deployment is text-first. RAST transfers
`text/plain` (and Xpra's `UTF8_STRING`/`STRING` text targets) only. HTML and
images are **not** transferred by the desktop manager:

* if an application copies an image/HTML, the shim reports the datatype and
  RAST shows one honest notice per session ("plain text only, it was not placed
  on your clipboard") instead of pretending the copy worked;
* the pinned Xpra HTML5 client has a best-effort image path of its own, but it
  runs in the hidden/unfocused viewer document and is not reliable — do not
  rely on copying images between the browser and a native app. Save the file
  and move it with File Station instead;
* formatting is lost: copying rich text from a browser page into a native app
  pastes plain text;
* the X11 `PRIMARY` selection (middle-click paste) is served from the browser
  clipboard as a best effort; browsers do not expose it.

### Browser permission limits (important)

Browser clipboard APIs are intentionally restricted, and RAST cannot override
that:

* **Write (remote copy → browser):** usually allowed for the focused, active
  tab in Chromium-based browsers; other browsers may require a user gesture.
  When it is refused, RAST surfaces it (toast + fallback dialog) — the copy is
  never silently lost.
* **Read (browser copy → remote):** `navigator.clipboard.readText()` requires
  the `clipboard-read` permission and may show a permission prompt; on some
  browsers it only works from a user gesture. RAST only performs a *quiet*
  read when the permission is already granted (never pops a prompt from a
  random event). The normal browser→remote path does not need a read
  permission because it uses the trusted `paste` event's `clipboardData`.
* **Page focus:** clipboard API calls are rejected when the document is not
  focused. If the browser window is in the background, a remote copy is queued
  and written when the window regains focus (with the toast/action if it still
  fails).
* **Insecure contexts** (plain HTTP, non-loopback) have no Clipboard API.
  Deploy RAST over HTTPS (or localhost) for clipboard support; the explicit
  fallbacks still work.
* **Cross-account/other tabs:** the clipboard belongs to the operating system
  user session; RAST does not try to isolate it between accounts.

## Keyboard / mouse limitations you should know

* Browser-reserved shortcuts (above) cannot be delivered to native apps.
* `Cmd+F` is deliberately kept as the browser find bar (see table). A native
  app's own find shortcut is not reachable with that combination.
* IME composition / language input methods are not supported by the pinned
  HTML5 client (v21). Use clipboard paste.
* Some macOS Option-letter combinations produce composed characters; the
  client may not reproduce every dead-key/accent sequence. Use clipboard
  paste for exotic characters.
* A browser tab that is not the active tab receives no input, by definition.
* When the remote application grabs the pointer (e.g. drag-and-drop or a
  slider), moving outside the RAST window may end the capture; releasing the
  button always releases the remote button.
* Nothing here disables the browser's own UI: tab management, DevTools and the
  address bar still work exactly as in any page.

## Diagnostics

Start with the shim snapshot (from the desktop page's DevTools console):

```js
const frame = document.querySelector(".linux-native-session-frame");
frame.contentWindow.RASTNative.snapshot();
// { version, connected, clipboardEnabled, clipboardDirection, clipboardPending,
//   clipboardLastEventAt, keyboardSwap, captureKeyboard, windows: [...] }
```

Expected values on a healthy macOS session: `version >= 1.3.0`,
`connected: true`, `clipboardEnabled: true`, `keyboardSwap: true`,
`captureKeyboard: true`.

Focus check (before typing):

```js
document.activeElement;                 // should be .win-body.linux-native-window
document.activeElement.tagName;         // never IFRAME while an app is focused
frame.contentDocument.activeElement;    // viewer focus should not be #pasteboard
```

Other signals:

* The shim logs under `[RAST native shim]`. Its version is also exposed as
  `window.RASTNative.version` / `window.__RAST_SHIM_VERSION`.
* Clipboard failures in the desktop are visible as toasts and in the console
  (the manager never suppresses a refusal).
* Enable the Xpra client's clipboard debug category to see token/request
  packets: `frame.contentWindow.client.debug_categories.push("clipboard")`.
* The transport viewer's screenshot/logs are not displayed by design; input
  goes through the desktop document.

## End-to-end verification (production, 2026-11-24)

The end-to-end harness was rebuilt around a **controlled remote page** so the
clipboard matrix is asserted on exact content instead of screenshot heuristics
or blind address-bar typing. It runs against the real public deployment
(`https://desktop.lufria.tech`) with the dedicated test account.

Harness files:

| Path | Purpose |
|---|---|
| `tests/e2e/clipboard/clipboard-page.html` | Controlled remote page: a full-window `<textarea id="field">`, a NEW WINDOW button and a pure-black 8 px top strip. It reports every state change in `document.title` (see grammar below). |
| `tests/e2e/clipboard/verify.mjs` | The matrix runner (Playwright). Serves the page on `127.0.0.1`, logs in through the real form, launches Google Chrome from Applications and drives the real clipboard paths. |
| `tests/e2e/clipboard/lib.mjs` | Shared helpers: login, start menu, adopted-window inspection, hit-tested clicks (overlapping adopted windows), page-origin calibration, title observer, browser clipboard, read-only X11/`xpra info` evidence. |
| `tests/e2e/clipboard/results/<run>/report.{json,md}` | Committed evidence of each matrix run. |
| `tests/e2e/clipboard/screenshots/<run>/*.png` | Screenshots (git-ignored run artifacts). |

### Assertion channel (why `document.title`)

The remote application is an Xpra window rendered into a canvas inside the
RAST window: remote DOM is not inspectable from the client. The controlled page
therefore reports its state through `document.title`, which RAST mirrors into
the adopted RAST window's `.win-title` (plus a `" - Google Chrome"` suffix that
the harness tolerates). Title grammar:

| Title | Meaning |
|---|---|
| `READY:<role>` / `FOCUS:<role>` / `BLUR:<role>` | page loaded / remote window focus change |
| `PASTED:<text>` | a paste (paste event or `insertFromPaste` input) produced exactly this text |
| `INPUT:<text>` | typed/deleted text (no clipboard involved) |
| `COPIED:<sha8>|<len>` | a copy inside the application; SHA-256 prefix + length of the copied text |
| `CUT:<sha8>|<remaining>` | a cut; SHA-256 prefix of the cut text + remaining field length |
| `SEL:<len>` | non-empty selection (debounced; used for select-all) |
| `UNDO:<text>` / `REDO:<text>` | the text after an undo/redo input event |
| `WINDOW-OPENED:B` / `POPUP-BLOCKED` | result of the page's second-window request |

Navigation never uses browser-reserved chords: **Alt+D** (primary) or a pointer
click on the omnibox, both validated on the shipped input policy (`Ctrl+L`,
`Ctrl+N`, `Ctrl+T`, … are deliberately browser-reserved and never reach the
remote app). Navigation success is decided by the page's *pixels* (dark
textarea + black origin strip + coloured toolbar), not by the window title
alone, because Chrome restores tabs and a title can be stale.

### Matrix result (run `2026-11-24T06-18-48-986`, RAST 0.18.3 `ab9b821d18cb`)

| # | Direction / behaviour | Result | Evidence |
|---|---|---|---|
| 1 | local browser → remote app, ASCII | **PASS** | window title `PASTED:RAST-LOCAL-TO-REMOTE-48-986 - Google Chrome`, 313.8 ms from Ctrl+V to the title update |
| 2 | local browser → remote app, unicode | **PASS** | `PASTED:RAST ⇄ Linux ✓ — ünïcødé テスト 日本語 🚀 Ω≈ç√∫ -986 - Google Chrome`, 345.5 ms |
| 3 | remote app → local browser clipboard | **PASS** | remote Ctrl+A/Ctrl+C produced `COPIED:de568410\|46`; `navigator.clipboard.readText()` returned the exact unicode string; independent host evidence: `xclip -selection clipboard -o` on `:101` returned the same text |
| 4 | select-all | **PASS** | title `SEL:46 - Google Chrome` |
| 5 | cut (Ctrl+X) | **PASS** | `CUT:de568410\|0 - Google Chrome` and the browser clipboard received the cut text |
| 6 | typing exactly once | **PASS** | `INPUT:typed-marker-48-986 - Google Chrome` (exact; no duplicated/lost characters) |
| 7 | undo / redo | **PASS** | `UNDO:typed-marker-48-98`, `REDO:typed-marker-48-986` |
| 8 | second window adopted 1:1, focus switching | **PASS** | 1 → 2 adopted RAST windows (`win-2`, `win-3`), focus class moves with clicks (`A.focused=true`, then `B.focused=true`) |
| 9 | remote window A → remote window B | **PASS** | A `COPIED:98f3504e\|23`, browser clipboard synced, B `PASTED:remote-to-remote-48-986 - Google Chrome`, 316.1 ms |
| 10 | desktop reload (viewer reconnect) → paste | **PASS** | after `page.reload()` both windows re-adopted and the new browser clipboard pasted into the app: `PASTED:after-reconnect-48-986`, 351.6 ms |
| 11 | macOS `keyboardSwap` emulation | **PARTIAL** | emulated platform ⇒ `keyboardSwap=true`; Cmd+A/Cmd+C proven end-to-end; the automated Cmd+V pass did not surface the new text (see limitations) |
| 12 | image/HTML clipboard | **NOT TESTED** | unsupported by design: this deployment transfers plain text only |

Plain text, exact round trip, both directions, unicode included, across two
top-level windows and across a viewer reconnect. Latency values above are
"Ctrl+V/Ctrl+C down → application state observed" measured on the title
observer, so they include the client round trip and Xpra metadata propagation,
not only the clipboard transfer.

### Honest limitations of this harness

* **The trusted browser paste event cannot be produced by automation.**
  Playwright/CDP key injection does not run browser editing commands, so the
  local browser never fires a trusted `paste` event with `clipboardData`. The
  browser→application direction is therefore proven through the product's own
  fallbacks: the forwarded paste gesture leads to a quiet `navigator.clipboard
  .readText()` and a push to the session, and the application's X11 clipboard
  request is answered by the desktop manager. A real user Ctrl/Cmd+V also
  delivers the trusted paste event first (the primary path, covered by the
  frontend unit tests); the fallback is what makes the gesture still work when
  the event is missing, which is why this automation is representative but not
  identical to a human gesture.
* **Real macOS hardware is not available here.** The macOS pass emulates
  `navigator.platform` / `navigator.userAgentData.platform` on a Linux
  Chromium. It proves the `keyboardSwap` policy and transport path, and the
  Cmd+A/Cmd+C forwarding, but not physical Apple-keyboard behaviour; the Cmd+V
  assertion remains partial (possible stale-clipboard/cached-text race in the
  automation; not a confirmed product bug). A real macOS client is required for
  the final sign-off of the Command path.
* **Text only.** HTML/image clipboard is intentionally unsupported and reported
  not-tested; the desktop manager shows an honest notice instead.
* The remote DOM is a canvas; everything remote is asserted through the
  title channel plus read-only host-side evidence (`xclip`, `xdotool`,
  `xpra info` on the test identity's own display).
* One host, one Xpra version (v6.5.5-r0), Chrome 158, software Xvfb, no GPU.

### How to run

```bash
cd tests/e2e

# Full clipboard/input matrix on production (writes report + screenshots):
node clipboard/verify.mjs

# Navigation/focus probe only (fast, useful when changing the harness):
node clipboard/verify.mjs --nav-only

# Watch it in a real browser window:
node clipboard/verify.mjs --headless=false
```

Options: `--url=<base>`, `--creds=<file>`, `--user=<name>`, `--admin=<name>`,
`--nav-only`, `--keep-session` (leave the test session for a follow-up run).
Credentials come from `/home/lufria/.config/rast-test/creds` and are never
printed or committed. The runner only opens/closes the test account's own Chrome
windows and ends only that account's own native session through the RAST API;
the live user session is never touched. Results are written under
`tests/e2e/clipboard/results/<run>/`.

### Benchmark/environment note

The matrix runs against the deployed production build; the `report.json`
records the `/api/system/info` facts and the session state at the time of the
run. Latency stage numbers for the same path (rAF rates, Xpra ping/damage,
tunnel RTT, estimated floor) are in
[BENCHMARKS.md §8](BENCHMARKS.md#8-input-and-clipboard-latency-stage-profile-2026-11-24-production).

### Focus-ownership incident during the deploy window (product bug, fixed)

While the prewarm feature was being deployed, the hidden Xpra transport could
steal keyboard focus after the prewarmed session connected: the desktop
document lost keyboard ownership, so a user's keystrokes could stop reaching
the native application. This is exactly the failure class the clipboard/input
matrix is built to catch (focus probe: `document.activeElement` must be
`.win-body.linux-native-window`, never the hidden viewer iframe). It was found
and first patched during the deploy window (build `93585ba2`). That patch
(blurring the hidden viewer document) was **reverted in `951c15a`**: the shim
forwards synthetic key events into the viewer, and blurring its document made
them stop reaching the Xpra client (the automated focus handshake correctly
caught this).

The product fix is instead to never have a hidden transport iframe while a
session is only pre-warmed: the desktop defers the viewer connection until the
first native app launch (`NativeLinuxHost.viewerDeferred` /
`prepareForLaunch()`), then connects the transport (bounded 15 s wait for the
shim), and only then calls the launch API. Consequently:

* after sign-in with pre-warming enabled there is no hidden iframe at all, so
  `document.activeElement` stays in the desktop document and local RAST apps
  (Terminal, Editors, File Station) keep keyboard ownership — the original
  "Terminal opens but ignores typing until a click" symptom cannot occur;
* during a native launch the iframe connects before the app starts, and
  `ensureNativeKeyboardFocus()` keeps routing keys to the focused adopted
  window afterwards; synthetic keyboard forwarding is untouched (the reverted
  blurring patch stays reverted);
* if the transport cannot connect inside the bounded wait, the launch still
  proceeds and the person sees a clear warning instead of a silent failure.

Focus probe to re-run on the deployed build with pre-warm enabled: sign in,
open Terminal without clicking a native app, type — `document.activeElement`
must never be the hidden `.linux-native-session-frame` document; then launch a
native app and verify the adopted window is the active element
(`.win-body.linux-native-window`). The latency profiler keeps its focus
handshake that aborts a measurement when a forwarded key does not provably
reach the controlled remote page, instead of reporting a wrong number.

### Matrix classification addendum

The machine report of the canonical matrix run marks the macOS-emulation check
`fail` because the exact Cmd+V content assertion did not hold; the reviewed
classification is **partial / not proven** (Cmd+A/Cmd+C and
`keyboardSwap=true` are proven, Cmd+V is not). The evidence and reasoning are in
`tests/e2e/clipboard/results/2026-11-24T06-18-48-986/classification-note.md`.
No other check failed and no product bug was found by the clipboard probes.

## Frontend unit tests

`frontend/tests/input-clipboard.test.ts` covers the pure policy:

* the complete macOS Command forwarding table (including `Cmd+W/T/L/N/R/P/M/H/F`,
  `Cmd+Shift+W/N/T` and `Cmd+Opt+I/J/C/K` staying browser-side);
* that the parent does not rewrite Command to Control (the client swap does it);
* one keydown / one keyup, repeat preserved, release after focus loss;
* the clipboard manager: remote→browser writes, coalescing, blocked writes
  surfaced, copy/paste fallbacks, unknown-format notices, loop prevention,
  reconnect, stale-cache expiry.

`frontend/tests/xpra-input.test.ts` runs the actual injected shim and covers
adopted pointer mapping (including browser zoom), button numbers, wheel
normalization, clipboard token filtering, deferred clipboard requests with the
timeout fallback, echoed-token loop prevention, and the pinned
`swap_keys`/`capture_keyboard` contract.
