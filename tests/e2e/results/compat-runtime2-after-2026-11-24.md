# RAST native app compatibility matrix — after

Generated 2026-11-24T05:22:53.339Z against `https://desktop.lufria.tech` as `t-alice`.
Raw JSON: `compat-runtime2-after-2026-11-24.json`.

## Summary

Apps found: 31. partial: 1, not-tested: 30.

Legend: **pass** = all required checks passed, **partial** = some checks passed but app-specific checks are n/a / not tested, **fail** = a tested behaviour is broken, **not-tested** = app is not installed / not managed by RAST, so it was not exercised.

## Matrix

| App | Installed | In App Center | Launch | 1 window / top-level | Typing | Scrolling | Clipboard | 2nd window | Close | Overall | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Google Chrome | yes | yes | pass | pass | pass | pass | partial | pass | pass | **partial** | live-tested through the desktop shell |
| Alacritty | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (alacritty) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Blender | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/snap/bin/blender) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (chromium-browser) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/snap/bin/chromium) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Chromium Web Browser | no | no | — | — | — | — | — | — | — | **not-tested** | desktop entry is a handler stub (/usr/bin/false) — not a launchable app; not tested |
| Claude Code URL Handler | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/home/lufria/.local/bin/claude) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Handler for snap:// URIs | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/bin/snap) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Htop | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (htop) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Openbox | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (openbox) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Openbox Configuration Manager | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (obconf) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Portal | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/libexec/xdg-desktop-portal-gtk) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Python (v3.14) | yes | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (/usr/bin/python3.14) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Snapd User Session Agent | no | no | — | — | — | — | — | — | — | **not-tested** | desktop entry is a handler stub (/bin/false) — not a launchable app; not tested |
| TeXInfo | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (info) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| UXTerm | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (uxterm) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Vim | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (vim) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Attach | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Launcher | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra_launcher) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| Xpra Shadow Server | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xpra) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| XTerm | no | no | — | — | — | — | — | — | — | **not-tested** | installed on the host (xterm) but not managed by the RAST App Center — cannot be launched through RAST; not tested |
| firefox | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| libreoffice | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| gimp | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| vlc | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| mpv | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| gedit | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| gnome-terminal | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| xclock | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |
| code | no | no | — | — | — | — | — | — | — | **not-tested** | not installed on the host → not tested |

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
| firefox | not installed → not tested |
| libreoffice | not installed → not tested |
| gimp | not installed → not tested |
| vlc | not installed → not tested |
| mpv | not installed → not tested |
| gedit | not installed → not tested |
| gnome-terminal | not installed → not tested |
| xclock | not installed → not tested |
| code | not installed → not tested |

## Check details

### Google Chrome (`google-chrome`)

* **launch**: pass
* **windowMapping**: pass — one RAST window per remote top-level window, adopted DOM node, 0 iframes/viewers
  ```json
  {"shellWindows":1,"remoteTopLevelWindows":1,"remoteWindows":[{"wid":"6291459","title":"New Tab - Google Chrome","type":["NORMAL"],"transientFor":null,"adopted":true}],"bodies":[{"id":"win-2","adoptedNodes":1,"iframes":0,"canvases":1}]}
  ```
* **typing**: pass — pixels changed 444 ms after typing started
  ```json
  {"changed":true,"elapsedMs":444}
  ```
* **scrolling**: pass — wheel scroll changes pixels on the deterministic fixture
  ```json
  {"changed":true,"elapsedMs":567}
  ```
* **clipboard**: partial — Ctrl+A/Ctrl+C → Backspace → Ctrl+V in the remote Chrome fixture
  ```json
  {"restored":false,"differedFromCleared":true,"detail":"paste changed pixels but did not restore the exact pre-clear frame"}
  ```
* **secondWindow**: pass — Ctrl+N created exactly one more adopted RAST window and one more remote top-level window
  ```json
  {"shellBefore":1,"shellAfter":2,"remoteWindowsAfter":["RAST bench - Google Chrome","RAST bench - Google Chrome"],"screenshots":["google-chrome-second-window"]}
  ```
* **close**: pass — closing the RAST window closed the remote window and removed it from the shell
  ```json
  {"beforeClose":1,"shellAfter":0,"remoteAfter":0}
  ```

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


### firefox


### libreoffice


### gimp


### vlc


### mpv


### gedit


### gnome-terminal


### xclock


### code


## Method

* Detection scans `/usr/share/applications`, `/usr/local/share/applications`, flatpak/snap export dirs, the App Center app list (`GET /api/linux-apps` as admin and as the user) and known command names.
* Live checks run in headless Playwright Chromium through the real login form and the real desktop shell: launch from the start menu, then inspect the DOM (`.window[data-window-id]`, adopted `.rast-native-xpra-window` nodes, iframes) and the native shim snapshot (`RASTNative.windows()`).
* Typing/scrolling/clipboard are verified against a deterministic local page loaded inside the remote Chrome through its address bar; other apps are marked not-applicable / not-tested for checks that need an app-specific fixture instead of pretending to test them.
* Screenshots for the second-window check: `tests/e2e/screenshots/compat/`.

## Limitations

* Only apps that the RAST App Center can launch are live-tested; system packages outside the App Center are reported as installed but not tested.
* The matrix ran against one environment (one host, one Xpra version, one Chrome build); GPU/accelerated paths are not present on this host.
* Flaky browser-level interactions are retried once by the harness; a `fail` therefore means the behaviour was reproduced.

