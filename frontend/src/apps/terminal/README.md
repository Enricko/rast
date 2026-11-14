# Terminal app (frontend)

A first-class RAST app (`id: "terminal"`) that runs xterm.js in the browser and
a real PTY on the server over one WebSocket per tab.

## Files

| File | Purpose |
| --- | --- |
| `index.ts` | `terminalApp` definition, window UI: toolbar, tabs, overlays, status bar, search, copy/paste/font size, runtime-panel glue. |
| `session.ts` | One tab: lazy xterm + FitAddon + SearchAddon, WebSocket lifecycle, reconnect, resize, clipboard, search. |
| `model.ts` | Pure helpers (no DOM): WS URL, start/ready/exit/error messages, phase machine, tab model, runtime DTO normalisation, font-size storage, File Station folder mapping. |
| `runtime-panel.ts` | Modal listing runtimes, per-user defaults (`PATCH /api/terminal/env`) and per-tab overrides for new tabs. |
| `../../tests/terminal-app.test.ts` | Unit tests for `model.ts`. |

xterm and its addons are bundled in `apps/applications/xterm-bundle.ts` and
imported dynamically, so they stay out of the desktop's main chunk.

## Protocol

REST (cookie auth, CSRF via `ctx.api`):

* `GET /api/terminal/env` → runtime environment + panel data.
* `PATCH /api/terminal/env` `{"defaults":{"rust":"1.90.0"}}` → same shape.
* `GET /api/terminal/sessions` → live sessions (used by the runtime panel).
* `POST /api/terminal/sessions/{id}/close` → close a session by id (not used by
  the UI: closing a tab sends the WebSocket `close` message instead).

WebSocket `GET /api/terminal/ws` (same host, `wss:` on HTTPS):

1. Client text `{"type":"start","csrf":"…","cols":N,"rows":N,"folder":{"path":"/shared/team/docs"},"runtimes":{"rust":"1.89.0"},"session":"<reattach id>"}`.
   `folder` and `runtimes` are only sent for a **new** session; `session` only
   when reattaching after a drop. The folder is a RAST *logical* path — exactly
   what File Station uses (`/` home, `/sub/dir` home subfolder,
   `/shared/<share>/…`) — never a POSIX path. Without it the shell opens in the
   private per-user HOME.
2. Client binary frames are keyboard bytes (UTF-8); `{"type":"resize","cols":N,"rows":N}`
   changes the size (debounced 100 ms); `{"type":"close"}` is sent before a tab
   is disposed so the backend closes the PTY immediately.
3. Server text `ready` (`sessionId`, `shell`, `cwd`, `reconnected`, `runtimes`),
   binary output, text `exit` (`code: N|null`) or `error` (`message`).

After an unexpected socket drop the tab reconnects **once** after 1.5 s and
sends the previous `sessionId`. If the server answers with an expiry-style
error ("That terminal is no longer running.", "session expired", …) the tab
becomes `exited` and offers "New terminal"; other errors show "Terminal error"
with Retry/New terminal.

## Keyboard policy

The app takes only Ctrl+Shift / Cmd+Shift shortcuts: Ctrl+Shift+C (copy),
Ctrl+Shift+V (paste), Ctrl+Shift+F (find), Ctrl+Shift+T (new tab). Plain
Ctrl+C/D/Z, Ctrl+V (readline), Tab completion, arrows and Unicode go to the
shell untouched.

Browser-reserved chords stay browser-controlled and are deliberately not
intercepted: Ctrl+W / Ctrl+Shift+W (close browser tab/window), Ctrl+T /
Ctrl+Shift+N, Ctrl+Shift+I/J/C (devtools), Ctrl+L, F5/F11. Every action also
has a toolbar button, so nothing becomes unreachable.

## Notes for backend integration

* The runtime panel shows `enabled:false` as "Terminal is disabled by the
  administrator"; a 503 "disabled …" from `/api/terminal/env` (current
  backend behaviour) and a 403 (app permission revoked) are also rendered as
  states instead of errors.
* A reattached `ready` currently carries an empty `runtimes` map (the manager
  does not persist the selection in `attach()`); the status bar then falls
  back to the "next tabs" runtime summary.
* `GET /api/terminal/env` has no `graceSeconds` field yet; the normaliser
  defaults it to 30.
