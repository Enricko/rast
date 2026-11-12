# Xpra seamless-session PoC (native Linux apps)

This directory proves the native-apps integration **before/independently of the
Rust backend**, against the live Xpra 6.5.5 + xpra-html5 v21 session on this
host (`127.0.0.1:14591`, display `:101`).

Two harnesses run against the **real** shim (`src/linux_apps/xpra_shim.js`):

| Harness | What it proves |
| --- | --- |
| `server.mjs` + `index.html` + `harness.js` (`run.mjs`) | The integration contract in isolation: one shared client, adoption of every remote window into a RAST-style window frame, typing, clipboard, resize, maximize, minimize, transient dialogs, close, reconnect, narrow layout. |
| `mock-rast.mjs` + `run-real.mjs` | The **real built frontend** (`frontend/dist`) with a mock RAST API and the real session proxy: launcher → native launch → RAST-managed windows → keyboard → resize → minimize/restore/maximize → close → refresh reconnect, plus the App Center native UI (migration notice, native install details, install preview). |

`dialog-app.py` is a small GTK app that opens a real `WM_TRANSIENT_FOR` modal
dialog (used by the transient-dialog step).

## Run

```bash
# Architecture PoC (uses the live xpra session; launches its own xterm)
node scripts/xpra-poc/run.mjs

# Real frontend end-to-end (build the frontend first)
(cd frontend && npm run build)
node scripts/xpra-poc/run-real.mjs
```

Both runners write `observations*.json`, `screenshots*/`, and exit non-zero if
an assertion fails. Playwright comes from `/tmp/pw` (`POC_PLAYWRIGHT_DIR`);
Chromium defaults to the local Playwright cache (`POC_CHROME`).

## Chosen architecture: adopt, not mirror

The shim captures the single `XpraClient` instance and the parent **adopts each
remote window's DOM node** (`ownerDocument.adoptNode`) into its RAST window
body, then switches the node to fill-mode:

- the node object is preserved, so Xpra keeps drawing into its own canvas and
  its own jQuery mouse handlers keep working — **one WebSocket connection for
  every window and every app**;
- the node's Xpra classes are renamed (`rast-native-xpra-window`) so RAST's
  `.window` CSS does not bleed into it, and Xpra's titlebar/handles are hidden
  (the RAST desktop is the window manager);
- `client.getMouse` is rebased on the RAST window body so pointer coordinates
  stay correct after adoption;
- RAST resize → `send_configure_window` (X server resize); the shim requests a
  full `buffer-refresh` after the canvas backing store changes (otherwise the
  canvas stays blank until the app repaints);
- keyboard is forwarded into the iframe document as synthetic
  `KeyboardEvent`s (`keyCode`/`which` redefined so xpra's mapping works);
- transient windows (`transient-for` / modal / menus) become attached RAST
  windows (no taskbar entry, follow their parent); `postMessage` fallback API
  is provided for future sandboxing.

Mirroring (drawing `win.canvas` into per-window canvases) was **not needed**:
adoption was proven to keep rendering and input working in both harnesses.
Evidence: `screenshots/02-typing.png` (typed command visible in the remote
xterm), `screenshots-real/03-typing.png` (same through the real frontend),
`observations*.json` assertions.

## Evidence summary (last good run)

`run.mjs` — all assertions pass:

- single shared client connects, 3 remote windows adopted (canvas sizes match
  window bodies);
- keyboard forwarding: typing `touch /tmp/rast-poc-typing-proof` in the
  adopted remote xterm creates the file;
- clipboard browser→X11 (`xclip -o` returns the text) and X11→browser (shim
  clipboard buffer updates);
- resize → remote window resized (484→778 px canvas, `stty size` shows the new
  terminal size) and a full repaint (no blank canvas);
- maximize fills the work area; minimize sends X11 iconify (unmap) and restore
  maps again;
- a real GTK modal dialog is attached to its parent (`transientFor`, `modal`,
  no taskbar entry) and closing it leaves the parent alive;
- closing a RAST window sends WM_DELETE: only that xterm exits, other windows
  stay;
- page refresh reconnects the one client and rebuilds every window, no
  duplicated xterm processes;
- 390×844 narrow layout keeps windows usable.

`run-real.mjs` — all assertions pass:

- the real RAST desktop boots, reconnects the shared session on load and
  re-creates every remote window as a RAST window with title + taskbar entry;
- launcher → native launch works; typing through the real RAST key handling
  reaches the remote app; RAST edge-resize reaches the X server; minimize /
  restore / maximize work; closing one RAST window closes only that remote
  window; refresh restores windows without duplicate apps; narrow layout fits;
- App Center shows the Docker→native migration notice, native install details
  and the pre-install preview (package transaction + compatibility).

Screenshots: `screenshots/` (architecture PoC) and `screenshots-real/`
(real frontend + mock API).

## Known limitations

- Keyboard forwarding uses synthetic events: IME/dead-key composition and
  browser-level shortcuts are limited (the shim exposes the client's
  `_keyb_process` fallback).
- Clipboard: server→browser uses `navigator.clipboard.writeText` from the
  parent (needs browser permission / user gesture); browser→browser paste is
  forwarded via the document `paste` event. Full clipboard-manager semantics
  (images, X11 selections) are the Xpra client's own.
- HiDPI: the canvas is 1 CSS pixel per remote pixel (`scale = 1`); it may look
  slightly soft on 2x screens. Browser zoom comes from xpra's own detect-zoom.
- Menus/tooltips (`override_redirect`) are mapped to attached frameless RAST
  popup windows; unusual override-redirect windows may need follow-up.
- A remote window that ignores WM_DELETE (e.g. an app's own "unsaved changes"
  prompt) leaves its RAST window closed until the prompt is handled and the
  remote window disappears; it then reappears as a normal RAST window.
