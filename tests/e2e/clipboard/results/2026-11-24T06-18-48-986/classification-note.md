# Classification note — clipboard/input matrix run 2026-11-24T06-18-48-986

Measured on production `https://desktop.lufria.tech` with
`tests/e2e/clipboard/verify.mjs` on the pre-final-deploy build
`RAST 0.18.3 (ab9b821d18cb)` (input/clipboard code unchanged by the later
prewarm/discovery/terminal deploys).

**18 of 19 checks pass.** This addendum records the human classification of the
only non-pass, because the machine report's `10.keyboard-swap-emulation` row has
to be `fail` (the exact-content assertion did not hold) while the evidence shows
a partial proof:

| Check | Machine status | Reviewed classification |
|---|---|---|
| 10.keyboard-swap-emulation | fail | **partial / not proven** |

Evidence for the partial classification (from `report.json`):

* the emulated macOS client platform makes the shim take its swap branch:
  `keyboardSwap = true` in the shim snapshot;
* forwarded Cmd+A / Cmd+C reach the Linux application: the remote page reports
  `SEL:22` and `COPIED:3d7b2d3e|22`, and the browser clipboard is synchronised;
* the automated Cmd+V pass did not surface the newly-written text (the window
  answered with its previous content), so Command-based paste is **not proven**.

Why this is not called a product bug:

* this host has no Apple hardware: the macOS client is emulated
  (`navigator.platform` / `navigator.userAgentData.platform` override);
* Playwright/CDP key injection cannot produce the browser's trusted `paste`
  event, which is the primary browser→application clipboard path for a real
  user gesture (the automated path exercises the fallback/request paths, which
  do work for Ctrl chords, see checks 1–9);
* the pass may answer the X11 clipboard request from the manager's cached
  browser text (a reconnect had cached the previous value in this window), which
  the automation cannot distinguish from "no paste".

Real proof needs a macOS client with a real Cmd+V gesture; a Linux/client test
with a real (non-automated) paste gesture would also exercise the trusted paste
event path. Everything else (local↔remote text, unicode, cut, select-all,
undo/redo, typing, two windows, focus switching, reconnect) is proven with
exact content evidence.
