# RAST native app compatibility matrix — baseline

Generated 2026-11-24T04:47:43.855Z against `https://desktop.lufria.tech` as `t-alice`.
Raw JSON: `compat-runtime2-baseline-2026-11-24.json`.

## Summary

Apps found: 22. not-tested: 22.

Legend: **pass** = all required checks passed, **partial** = some checks passed but app-specific checks are n/a / not tested, **fail** = a tested behaviour is broken, **not-tested** = app is not installed / not managed by RAST, so it was not exercised.

## Matrix

| App | Installed | In App Center | Launch | 1 window / top-level | Typing | Scrolling | Clipboard | 2nd window | Close | Overall | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Google Chrome | yes | yes | — | — | — | — | — | — | — | **not-tested** | live test pending |
| Alacritty | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (alacritty) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Blender | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/snap/bin/blender) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (chromium-browser) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/snap/bin/chromium) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/bin/false) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Claude Code URL Handler | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/home/lufria/.local/bin/claude) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Handler for snap:// URIs | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/bin/snap) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Htop | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (htop) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Openbox | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (openbox) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Openbox Configuration Manager | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (obconf) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Portal | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/libexec/xdg-desktop-portal-gtk) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Python (v3.14) | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/bin/python3.14) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Snapd User Session Agent | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/bin/false) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| TeXInfo | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (info) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| UXTerm | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (uxterm) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Vim | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (vim) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Attach | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Launcher | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra_launcher) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Shadow Server | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| XTerm | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xterm) but not managed by the RAST App Center — cannot be launched through RAST; not tested |

## Detection

* RAST App Center Linux apps (admin view): 1; visible to users: 1.
* System `.desktop` applications: 23 (installed on the host; only apps managed by the RAST App Center can be launched through RAST).
* RAST built-in apps (File Station, App Center, ...): 9 — outside the native Linux-app matrix.

| System desktop app | Exec | Managed by RAST |
|---|---|---|
| Alacritty | `alacritty` | no |
| Blender | `/snap/bin/blender` | no |
| Chromium Web Browser | `chromium-browser` | no |
| Chromium Web Browser | `/snap/bin/chromium` | no |
| Chromium Web Browser | `/usr/bin/false` | no |
| Claude Code URL Handler | `"/home/lufria/.local/bin/claude" --handle-uri` | no |
| Google Chrome | `/usr/bin/google-chrome-stable` | yes (google-chrome) |
| Google Chrome | `/usr/bin/google-chrome-stable` | yes (google-chrome) |
| Handler for snap:// URIs | `/usr/bin/snap handle-link` | no |
| Htop | `htop` | no |
| Openbox | `openbox` | no |
| Openbox Configuration Manager | `obconf` | no |
| Portal | `/usr/libexec/xdg-desktop-portal-gtk` | no |
| Python (v3.14) | `/usr/bin/python3.14` | no |
| Snapd User Session Agent | `/bin/false` | no |
| TeXInfo | `info` | no |
| UXTerm | `uxterm` | no |
| Vim | `vim` | no |
| Xpra | `xpra` | no |
| Xpra Attach | `xpra attach` | no |
| Xpra Launcher | `xpra_launcher` | no |
| Xpra Shadow Server | `xpra shadow --bind-tcp=0.0.0.0:0` | no |
| XTerm | `xterm` | no |

| Not installed / not detected | Status |
|---|---|

## Check details

### Google Chrome (`google-chrome`)


### Alacritty


### Blender


### Chromium Web Browser


### Chromium Web Browser


### Chromium Web Browser


### Claude Code URL Handler


### Handler for snap:// URIs


### Htop


### Openbox


### Openbox Configuration Manager


### Portal


### Python (v3.14)


### Snapd User Session Agent


### TeXInfo


### UXTerm


### Vim


### Xpra


### Xpra Attach


### Xpra Launcher


### Xpra Shadow Server


### XTerm


## Method

* Detection scans `/usr/share/applications`, `/usr/local/share/applications`, flatpak/snap export dirs, the App Center app list (`GET /api/linux-apps` as admin and as the user) and known command names.
* Live checks run in headless Playwright Chromium through the real login form and the real desktop shell: launch from the start menu, then inspect the DOM (`.window[data-window-id]`, adopted `.rast-native-xpra-window` nodes, iframes) and the native shim snapshot (`RASTNative.windows()`).
* Typing/scrolling/clipboard are verified against a deterministic local page loaded inside the remote Chrome through its address bar; other apps are marked not-applicable / not-tested for checks that need an app-specific fixture instead of pretending to test them.
* Screenshots for the second-window check: `tests/e2e/screenshots/compat/`.

## Limitations

* Only apps that the RAST App Center can launch are live-tested; system packages outside the App Center are reported as installed but not tested.
* The matrix ran against one environment (one host, one Xpra version, one Chrome build); GPU/accelerated paths are not present on this host.
* Flaky browser-level interactions are retried once by the harness; a `fail` therefore means the behaviour was reproduced.

