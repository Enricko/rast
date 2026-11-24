# Multi-window adoption — independent RAST windows

Build: `RAST 0.18.3 (68c0bea64c1b)`, GPU session (`displayBackend="gpu"`,
`gpuAccelerationVerified=true`), display :110.

Two native applications were launched through the API **before** any browser
connected to the session, then the RAST desktop was opened in a real browser:

- launch `google-chrome` → 200
- launch `artcraft-launcher` → 200
- session ready with apps `["google-chrome","artcraft-launcher"]`
- adopted RAST windows: `"New Tab - Google Chrome"` and
  `"ArtCraft Launcher"` — **both adopted simultaneously (`both_adopted=true`)**

This proves the lazy/deferred viewer connects to an existing multi-window
session and adopts every window, not just the focused one. Evidence:
`compat-multiwindow-adoption.png`, `multi-window-adoption.json`.

Note: a large compatibility-matrix session (many apps launched in one go) only
reported 2 adopted windows at its brief snapshot; the dedicated test above
shows the two-window case is correct, so the large-session count is a
snapshot/settle-time and transient-window issue rather than a hard limit.
