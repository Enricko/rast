# Session pre-warm re-enabled — deferred-viewer + Terminal focus validation

Builds: pre-warm deferred-viewer frontend (`3d3f8f9`, docs `6a86cc3`), GPU
display backend (`4cc60dc`), discovery polish (`41c1027`/`ce77cb9`/`413ad2c`).
Production config at validation time: `linux_prewarm = true`,
`linux_display_backend = "auto"` (GPU verified sessions).

## Why it is safe now

Reliable pre-warming used to connect the hidden Xpra transport iframe right
after sign-in, whose clipboard autofocus could steal keyboard focus from local
RAST apps (Terminal ignored typing until clicked). The first fix (blurring the
viewer) was reverted because it broke synthetic key delivery. The deployed fix
defers the hidden transport connection until the first native application
launch; a session that exists only because of pre-warming has **no iframe at
all**, so it cannot take focus.

## Evidence (production, t-admin / t-bob)

| Check | Result |
| --- | --- |
| Pre-warmed session becomes ready while the desktop is open | **1991 ms** after desktop load (t-admin, GPU display, no application started; earlier t-bob measurement 4.7 s) |
| Hidden transport iframe present while pre-warmed session exists | **0 iframes** (`document.querySelectorAll("iframe.linux-native-session-frame").length === 0`) |
| Session state | `status=ready`, `displayBackend=gpu`, `apps=[]`, `viewers=0` |
| Focus after sign-in | desktop document, never the off-screen viewer |
| Terminal opened from the start menu with a pre-warmed GPU session | opens; **0 iframes**, active element not in the viewer |
| Typing into the Terminal **without clicking its body** | `echo PREWARM_TERM_*` executed and its output appeared (PASS) — the regression that disabled pre-warm is gone |
| First app launch after pre-warm | viewer is created on demand (`prepareForLaunch`) then the existing launch API is called; bounded 15 s connect wait with a clear warning on timeout |

The rejected 93585ba focus-blurring approach stays reverted; the shim's
synthetic key path is untouched.

## Enablement

`linux_prewarm = true` is now set in production `/etc/rast/rast.toml` and the
server-side pre-warm (app-list trigger, 10-minute per-person throttle, explicit
close suppression) is unchanged. Rollback: set it back to `false` and restart
`rast.service` (the deferred-viewer frontend is inert without pre-warm).
