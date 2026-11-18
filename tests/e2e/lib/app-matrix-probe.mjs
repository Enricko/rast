// In-page probe for the app matrix. `installProbe` is evaluated in the RAST
// desktop page (page.evaluate(installProbe)); it must stay self-contained.
//
// The probe samples inside the page, on the page's own clock, so the numbers do
// not include Playwright round trips:
//   * keydown / wheel / pointerup timestamps (capture listeners on window);
//   * on every animation frame: a hash of the app's canvas (the Xpra window
//     adopted into `.win-body`) and the RAST window's position;
//   * error toasts that appear while it runs.
// Resolution is one animation frame (about 16 ms at 60 Hz). A blinking caret
// also changes the canvas, so a latency can read lower than the real one when
// a blink lands inside the window; the keys are spaced to make that rare.
// Legacy (Docker/noVNC) windows have no canvas in the page: `start` reports
// `canvas: false` and the scenario falls back to screenshots.

export function installProbe() {
  if (window.__rastProbe) return true;
  const MAX_SAMPLES = 20000;
  const state = {
    running: false,
    windowId: null,
    useCanvas: false,
    canvasError: null,
    lastSig: null,
    raf: 0,
    frames: 0,
    changes: [],
    rects: [],
    keydowns: [],
    wheels: [],
    pointerups: [],
    toastErrors: [],
  };
  const scratch = document.createElement("canvas");

  function windowEl() {
    return document.querySelector('.window[data-window-id="' + state.windowId + '"]');
  }

  function pickCanvas() {
    const body = windowEl()?.querySelector(".win-body");
    if (!body) return null;
    let best = null;
    let area = 0;
    for (const canvas of body.querySelectorAll("canvas")) {
      const a = canvas.width * canvas.height;
      if (a > area) {
        best = canvas;
        area = a;
      }
    }
    return best;
  }

  function signature() {
    const canvas = pickCanvas();
    if (!canvas || canvas.width < 2 || canvas.height < 2) return null;
    if (scratch.width !== canvas.width || scratch.height !== canvas.height) {
      scratch.width = canvas.width;
      scratch.height = canvas.height;
    }
    const ctx = scratch.getContext("2d", { willReadFrequently: true });
    ctx.drawImage(canvas, 0, 0);
    const data = ctx.getImageData(0, 0, scratch.width, scratch.height).data;
    const words = new Uint32Array(data.buffer, data.byteOffset, data.byteLength >> 2);
    let hash = 2166136261;
    for (let i = 0; i < words.length; i += 1) hash = Math.imul(hash ^ words[i], 16777619);
    return hash >>> 0;
  }

  function push(list, value) {
    if (list.length < MAX_SAMPLES) list.push(value);
  }

  function tick(ts) {
    if (!state.running) return;
    state.frames += 1;
    const el = windowEl();
    if (el) {
      const rect = el.getBoundingClientRect();
      push(state.rects, { t: ts, x: Math.round(rect.x), y: Math.round(rect.y) });
    }
    if (state.useCanvas) {
      try {
        const sig = signature();
        if (sig !== null) {
          if (state.lastSig !== null && sig !== state.lastSig) push(state.changes, ts);
          state.lastSig = sig;
        }
      } catch (error) {
        state.canvasError = String(error);
        state.useCanvas = false;
      }
    }
    state.raf = requestAnimationFrame(tick);
  }

  window.addEventListener("keydown", (e) => state.running && push(state.keydowns, e.timeStamp), true);
  window.addEventListener("wheel", (e) => state.running && push(state.wheels, e.timeStamp), true);
  window.addEventListener("pointerup", (e) => state.running && push(state.pointerups, e.timeStamp), true);

  new MutationObserver((mutations) => {
    for (const mutation of mutations) {
      for (const node of mutation.addedNodes) {
        if (node.nodeType !== 1) continue;
        const toast = node.matches?.(".toast-error") ? node : node.querySelector?.(".toast-error");
        if (toast) push(state.toastErrors, String(toast.textContent || "").trim().slice(0, 200));
      }
    }
  }).observe(document.documentElement, { childList: true, subtree: true });

  window.__rastProbe = {
    /** Start sampling for one RAST window. Returns what could be sampled. */
    start(windowId, { canvas = true } = {}) {
      cancelAnimationFrame(state.raf);
      state.windowId = windowId;
      state.running = true;
      state.canvasError = null;
      state.lastSig = null;
      state.frames = 0;
      state.changes = [];
      state.rects = [];
      state.keydowns = [];
      state.wheels = [];
      state.pointerups = [];
      state.useCanvas = false;
      let canvasFound = false;
      if (canvas) {
        try {
          canvasFound = signature() !== null;
        } catch (error) {
          state.canvasError = String(error);
        }
        state.useCanvas = canvasFound;
      }
      state.raf = requestAnimationFrame(tick);
      return { canvas: canvasFound, canvasError: state.canvasError };
    },
    /** Stop and return everything sampled (timestamps in page ms). */
    stop() {
      state.running = false;
      cancelAnimationFrame(state.raf);
      return {
        canvas: state.useCanvas,
        canvasError: state.canvasError,
        frames: state.frames,
        changes: state.changes.slice(),
        rects: state.rects.slice(),
        keydowns: state.keydowns.slice(),
        wheels: state.wheels.slice(),
        pointerups: state.pointerups.slice(),
      };
    },
    now() {
      return performance.now();
    },
    toastErrors() {
      return state.toastErrors.slice();
    },
  };
  return true;
}

/** Xpra client facts through the shim, read-only (iframe at /linux-apps/session/). */
export function readXpraInfo() {
  const client = window.RASTNative?.client;
  if (!client) return null;
  const pick = (name) => {
    const value = client[name];
    return typeof value === "number" || typeof value === "string" || typeof value === "boolean" ? value : null;
  };
  return {
    encoding: pick("encoding"),
    dpi: pick("dpi"),
    scale: pick("scale"),
    desktopWidth: pick("desktop_width"),
    desktopHeight: pick("desktop_height"),
    serverVersion: pick("server_version"),
    clientVersion: pick("version"),
  };
}
