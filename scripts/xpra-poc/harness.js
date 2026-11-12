/* RAST xpra PoC harness: a miniature RAST window manager.
 *
 * Proves the integration contract of src/linux_apps/xpra_shim.js:
 *  - ONE Xpra HTML5 client connection for the whole desktop (one iframe);
 *  - every remote top-level Xpra window becomes one RAST-managed window
 *    (title bar, taskbar entry, focus/raise, resize, minimize/maximize,
 *    close, transient dialogs attached to their parent);
 *  - keyboard/mouse/clipboard forwarding;
 *  - reconnect after a page refresh without duplicating remote apps.
 *
 * Plain browser JS on purpose (the real frontend re-implements this in TS).
 */
(function () {
  "use strict";

  var windowsLayer = document.getElementById("windows-layer");
  var taskbar = document.getElementById("taskbar");
  var statusDot = document.getElementById("status-dot");
  var statusText = document.getElementById("status-text");
  var logEl = document.getElementById("log");
  var sessionFrame = document.getElementById("session-frame");

  var logLines = [];
  var frames = new Map(); // wid -> Frame
  var zCounter = 10;
  var focusedWid = null;
  var closing = new Set();
  var adoptFailures = [];
  var lastState = null;
  var lastEvent = null;
  var ready = false;
  var cascade = 0;
  var TASKBAR_H = 52;

  function log(message) {
    var line = new Date().toISOString().slice(11, 23) + " " + message;
    logLines.push(line);
    if (logLines.length > 300) logLines.shift();
    if (logEl) logEl.textContent = logLines.slice(-14).join("\n");
  }

  /* ------------------------------------------------------------- shim access */

  function shim() {
    try {
      var win = sessionFrame.contentWindow;
      return win && win.RASTNative ? win.RASTNative : null;
    } catch (error) {
      return null;
    }
  }

  function onShimEvent(name, payload) {
    lastEvent = { event: name, at: Date.now() };
    if (payload && payload.windows) lastState = payload;
    if (name !== "state") log("shim event: " + name + (payload && payload.windows ? " (" + payload.windows.length + " windows)" : ""));
    if (name === "ready" || name === "windows" || name === "state" || name === "connected" || name === "disconnected") {
      if (payload) applyState(payload);
    }
    if (name === "clipboard") {
      window.__POC_CLIPBOARD = payload && payload.text;
    }
  }

  function attach() {
    var api = shim();
    if (!api) {
      window.setTimeout(attach, 120);
      return;
    }
    if (api === attachedApi) return;
    attachedApi = api;
    log("shim attached: version=" + api.version + " key=" + (api.key || "none"));
    api.on("ready", function (state) { onShimEvent("ready", state); });
    api.on("state", function (state) { onShimEvent("state", state); });
    api.on("windows", function (state) { onShimEvent("windows", state); });
    api.on("connected", function (state) { onShimEvent("connected", state); });
    api.on("disconnected", function (state) { onShimEvent("disconnected", state); });
    api.on("error", function (state) { onShimEvent("error", state); });
    api.on("clipboard", function (payload) { onShimEvent("clipboard", payload); });
    applyState(api.snapshot());
  }

  var attachedApi = null;

  function resetFrames() {
    log("session page (re)loaded: resetting RAST windows");
    for (var key of Array.from(frames.keys())) destroyFrame(key);
    attachedApi = null;
    lastState = null;
  }

  /* -------------------------------------------------------------- window mgmt */

  function applyState(state) {
    if (!state || !state.ready) return;
    lastState = state;
    ready = true;
    statusDot.className = "dot " + (state.connected ? "ready" : "down");
    statusText.textContent = (state.connected ? "connected" : "connecting") + " · " + state.windows.length + " windows";
    if (state.separate === false) fallbackWholeSession(state);
    var seen = new Set();
    state.windows.forEach(function (w) {
      seen.add(w.wid);
      if (closing.has(w.wid)) return;
      var frame = frames.get(w.wid);
      if (!frame) {
        createFrame(w);
        return;
      }
      updateFrame(frame, w);
    });
    Array.from(frames.keys()).forEach(function (wid) {
      if (seen.has(wid)) return;
      log("remote window closed: " + wid);
      destroyFrame(wid);
    });
    updateFocusFromState(state.windows);
    renderTaskbar();
  }

  function fallbackWholeSession(state) {
    if (fallbackFrame) return;
    log("server is in desktop/shadow mode: single whole-session window fallback");
    var el = document.createElement("section");
    el.className = "window is-focused";
    el.style.cssText = "left:8px;top:8px;right:8px;bottom:8px;width:auto;height:auto;";
    var body = document.createElement("div");
    body.className = "window-body";
    body.style.cssText = "position:absolute;inset:0;";
    sessionFrame.classList.remove("session-frame");
    sessionFrame.style.cssText = "position:absolute;inset:0;width:100%;height:100%;border:0;";
    body.appendChild(sessionFrame);
    el.appendChild(body);
    windowsLayer.appendChild(el);
    fallbackFrame = el;
  }

  var fallbackFrame = null;

  function createFrame(w) {
    var dialog = !!w.transientFor && frames.has(w.transientFor);
    var owner = dialog ? frames.get(w.transientFor) : null;
    var el = document.createElement("section");
    el.className = "window" + (dialog ? " is-dialog" : "");
    el.dataset.wid = w.wid;
    el.dataset.title = w.title;

    var titlebar = document.createElement("header");
    titlebar.className = "window-titlebar";
    var title = document.createElement("span");
    title.className = "window-title";
    title.textContent = w.title;
    var task = document.createElement("span");
    task.className = "window-task";
    task.textContent = (w.classInstance && w.classInstance[0]) || "";
    var controls = document.createElement("div");
    controls.className = "window-controls";
    var minBtn = button("min", "–", function () { minimize(w.wid); });
    var maxBtn = button("max", "□", function () { maximize(w.wid); });
    var closeBtn = button("close", "×", function () { close(w.wid); });
    controls.appendChild(minBtn);
    controls.appendChild(maxBtn);
    controls.appendChild(closeBtn);
    titlebar.appendChild(title);
    titlebar.appendChild(task);
    titlebar.appendChild(controls);

    var body = document.createElement("div");
    body.className = "window-body";
    body.tabIndex = -1;

    el.appendChild(titlebar);
    el.appendChild(body);

    var size = dialog ? { w: 420, h: 300 } : { w: 620, h: 420 };
    var area = desktopArea();
    var offset = (cascade % 6) * 24;
    cascade += 1;
    var x = Math.min(24 + offset, Math.max(8, area.w - size.w - 8));
    var y = Math.min(24 + offset, Math.max(8, area.h - size.h - 8));
    if (owner) {
      var parentRect = owner.el.getBoundingClientRect();
      var layerRect = windowsLayer.getBoundingClientRect();
      size = { w: Math.min(420, area.w - 24), h: Math.min(300, area.h - 24) };
      x = Math.max(8, parentRect.left - layerRect.left + 40);
      y = Math.max(8, parentRect.top - layerRect.top + 40);
    }
    el.style.left = x + "px";
    el.style.top = y + "px";
    el.style.width = size.w + "px";
    el.style.height = size.h + "px";
    windowsLayer.appendChild(el);

    var frame = {
      wid: w.wid,
      el: el,
      titleEl: title,
      bodyEl: body,
      info: w,
      owner: w.transientFor || null,
      minimized: !!w.minimized,
      maximized: !!w.maximized,
      resizeTimer: 0,
      observer: null,
    };
    frames.set(w.wid, frame);

    if (w.minimized) el.classList.add("is-minimized");

    var api = shim();
    var adopted = false;
    try {
      adopted = api ? api.adoptWindow(w.wid, body) : false;
    } catch (error) {
      log("adopt threw: " + error);
    }
    if (!adopted) {
      adoptFailures.push({ wid: w.wid, title: w.title });
      log("ADOPT FAILED for " + w.wid + " (" + w.title + ")");
      body.textContent = "adoption failed";
    }

    frame.observer = new ResizeObserver(function () { scheduleResize(frame); });
    frame.observer.observe(body);
    scheduleResize(frame, 0);

    el.addEventListener("pointerdown", function (event) {
      if (event.target && event.target.closest && event.target.closest(".window-controls")) return;
      focus(w.wid);
    }, true);

    focus(w.wid, true);
    renderTaskbar();
    log("window created: " + w.wid + " · " + w.title + (dialog ? " (dialog of " + w.transientFor + ")" : ""));
  }

  function button(kind, label, onClick) {
    var el = document.createElement("button");
    el.type = "button";
    el.className = "window-control window-control-" + kind;
    el.textContent = label;
    el.addEventListener("pointerdown", function (event) { event.stopPropagation(); });
    el.addEventListener("click", function (event) {
      event.stopPropagation();
      onClick();
    });
    return el;
  }

  function updateFrame(frame, w) {
    frame.info = w;
    if (frame.titleEl.textContent !== w.title) {
      frame.titleEl.textContent = w.title;
      frame.el.dataset.title = w.title;
      renderTaskbar();
    }
    if (!!w.minimized !== frame.minimized) {
      frame.minimized = !!w.minimized;
      frame.el.classList.toggle("is-minimized", frame.minimized);
    }
    if (!!w.maximized !== frame.maximized) {
      frame.maximized = !!w.maximized;
      applyMaximized(frame);
    }
  }

  function scheduleResize(frame, delay) {
    if (frame.resizeTimer) window.clearTimeout(frame.resizeTimer);
    frame.resizeTimer = window.setTimeout(function () {
      frame.resizeTimer = 0;
      var api = shim();
      if (!api || !frames.has(frame.wid)) return;
      var rect = frame.bodyEl.getBoundingClientRect();
      var width = Math.round(rect.width);
      var height = Math.round(rect.height);
      if (width < 8 || height < 8) return;
      api.resizeWindow(frame.wid, { width: width, height: height });
    }, delay == null ? 90 : delay);
  }

  function desktopArea() {
    var layer = windowsLayer.getBoundingClientRect();
    return { w: layer.width, h: layer.height };
  }

  function focus(wid, silent) {
    var frame = frames.get(wid);
    if (!frame) return;
    focusedWid = wid;
    zCounter += 1;
    frame.el.style.zIndex = String(zCounter);
    frames.forEach(function (other) {
      other.el.classList.toggle("is-focused", other.wid === wid);
    });
    var api = shim();
    if (api) {
      if (!silent) api.focusWindow(wid);
      else if (frame.info && frame.info.focused === false) api.focusWindow(wid);
    }
    try {
      frame.bodyEl.focus({ preventScroll: true });
    } catch (error) {
      /* ignore */
    }
    renderTaskbar();
  }

  function updateFocusFromState(windows) {
    for (var i = 0; i < windows.length; i++) {
      if (windows[i].focused) {
        var frame = frames.get(windows[i].wid);
        if (frame && focusedWid !== windows[i].wid) {
          focusedWid = windows[i].wid;
          frames.forEach(function (other) {
            other.el.classList.toggle("is-focused", other.wid === windows[i].wid);
          });
          renderTaskbar();
        }
        break;
      }
    }
  }

  function minimize(wid) {
    var frame = frames.get(wid);
    var api = shim();
    if (!frame || !api) return;
    frame.minimized = true;
    frame.el.classList.add("is-minimized");
    api.setMinimized(wid, true);
    log("minimized " + wid);
    renderTaskbar();
  }

  function restore(wid, silentFocus) {
    var frame = frames.get(wid);
    var api = shim();
    if (!frame || !api) return;
    frame.minimized = false;
    frame.el.classList.remove("is-minimized");
    api.setMinimized(wid, false);
    log("restored " + wid);
    if (!silentFocus) focus(wid);
    renderTaskbar();
  }

  function maximize(wid) {
    var frame = frames.get(wid);
    if (!frame) return;
    frame.maximized = !frame.maximized;
    frame.info = Object.assign({}, frame.info, { maximized: frame.maximized });
    applyMaximized(frame);
    scheduleResize(frame, 10);
    log((frame.maximized ? "maximized " : "unmaximized ") + wid);
  }

  function applyMaximized(frame) {
    var area = desktopArea();
    if (frame.maximized) {
      if (!frame.prevRect) {
        frame.prevRect = { left: frame.el.style.left, top: frame.el.style.top, width: frame.el.style.width, height: frame.el.style.height };
      }
      frame.el.style.left = "0px";
      frame.el.style.top = "0px";
      frame.el.style.width = area.w + "px";
      frame.el.style.height = area.h + "px";
    } else if (frame.prevRect) {
      frame.el.style.left = frame.prevRect.left;
      frame.el.style.top = frame.prevRect.top;
      frame.el.style.width = frame.prevRect.width;
      frame.el.style.height = frame.prevRect.height;
      frame.prevRect = null;
    }
    scheduleResize(frame, 10);
  }

  function close(wid) {
    var api = shim();
    var frame = frames.get(wid);
    if (!frame || !api) return;
    closing.add(wid);
    log("closing " + wid + " (WM_DELETE)");
    api.closeWindow(wid);
    window.setTimeout(function () {
      if (frames.has(wid)) {
        log("remote window " + wid + " did not close in time");
        closing.delete(wid);
      }
    }, 6000);
  }

  function destroyFrame(wid) {
    var frame = frames.get(wid);
    if (!frame) return;
    if (frame.observer) frame.observer.disconnect();
    if (frame.resizeTimer) window.clearTimeout(frame.resizeTimer);
    frame.el.remove();
    frames.delete(wid);
    closing.delete(wid);
    if (focusedWid === wid) focusedWid = null;
  }

  function renderTaskbar() {
    taskbar.replaceChildren();
    frames.forEach(function (frame) {
      if (frame.owner) return; // transient dialogs have no taskbar entry
      var entry = document.createElement("button");
      entry.type = "button";
      entry.className = "taskbar-entry" + (frame.wid === focusedWid ? " is-focused" : "");
      entry.textContent = (frame.info && frame.info.title) || frame.wid;
      entry.addEventListener("click", function () {
        if (frame.minimized) restore(frame.wid);
        else focus(frame.wid);
      });
      taskbar.appendChild(entry);
    });
    if (frames.size === 0) {
      var empty = document.createElement("span");
      empty.style.cssText = "opacity:.5;font-size:12px";
      empty.textContent = ready ? "No remote windows" : "Connecting to native session…";
      taskbar.appendChild(empty);
    }
  }

  /* ---------------------------------------------------------------- keyboard */

  function forwardKey(event, pressed) {
    if (!focusedWid) return;
    var api = shim();
    if (!api) return;
    if (event.key === "F5" || event.key === "F12" || (event.ctrlKey && event.shiftKey && event.key === "I")) return;
    var ok = api.sendKey(pressed, {
      key: event.key,
      code: event.code,
      keyCode: event.keyCode || event.which || 0,
      ctrlKey: event.ctrlKey,
      shiftKey: event.shiftKey,
      altKey: event.altKey,
      metaKey: event.metaKey,
      repeat: event.repeat,
    });
    if (ok) {
      event.preventDefault();
      event.stopPropagation();
    }
  }

  document.addEventListener("keydown", function (event) { forwardKey(event, true); }, true);
  document.addEventListener("keyup", function (event) { forwardKey(event, false); }, true);

  /* ------------------------------------------------------------------ wiring */

  sessionFrame.addEventListener("load", function () {
    resetFrames();
    attachedApi = null;
    attach();
    window.setTimeout(attach, 400);
    window.setTimeout(attach, 1500);
  });

  attach();
  window.setTimeout(attach, 300);

  /* --------------------------------------------------------------- test API */

  window.__POC = {
    desktopState: function () {
      var api = shim();
      return {
        ready: ready,
        connected: !!(api && api.state.connected),
        separate: api ? api.state.separate : null,
        shimVersion: api ? api.version : null,
        shimKey: api ? api.key : null,
        adoptFailures: adoptFailures.slice(),
        frames: Array.from(frames.keys()),
        focusedWid: focusedWid,
        taskbar: Array.from(taskbar.querySelectorAll(".taskbar-entry")).map(function (el) { return el.textContent; }),
        log: logLines.slice(-40),
        lastEvent: lastEvent,
        windows: api ? api.windows() : [],
        framesInfo: Array.from(frames.entries()).map(function (pair) {
          var frame = pair[1];
          var canvas = frame.bodyEl.querySelector("canvas");
          var rect = frame.bodyEl.getBoundingClientRect();
          return {
            wid: frame.wid,
            title: frame.info.title,
            dialogOf: frame.owner,
            minimized: frame.minimized,
            maximized: frame.maximized,
            body: { width: Math.round(rect.width), height: Math.round(rect.height) },
            canvas: canvas ? { width: canvas.width, height: canvas.height, cssWidth: canvas.style.width } : null,
            adopted: !!frame.bodyEl.querySelector('[id="' + frame.wid + '"]'),
          };
        }),
      };
    },
    frameBodySize: function (wid) {
      var frame = frames.get(wid);
      if (!frame) return null;
      var rect = frame.bodyEl.getBoundingClientRect();
      return { width: Math.round(rect.width), height: Math.round(rect.height) };
    },
    canvasDistinctColors: function (wid) {
      var frame = frames.get(wid);
      if (!frame) return -1;
      var canvas = frame.bodyEl.querySelector("canvas");
      if (!canvas || typeof canvas.getContext !== "function") return -2;
      try {
        var tmp = document.createElement("canvas");
        tmp.width = Math.min(160, canvas.width || 1);
        tmp.height = Math.min(100, canvas.height || 1);
        var ctx = tmp.getContext("2d");
        ctx.drawImage(canvas, 0, 0, canvas.width, canvas.height, 0, 0, tmp.width, tmp.height);
        var data = ctx.getImageData(0, 0, tmp.width, tmp.height).data;
        var seen = new Set();
        for (var i = 0; i < data.length; i += 4 * 13) seen.add(data[i] + "," + data[i + 1] + "," + data[i + 2]);
        return seen.size;
      } catch (error) {
        return -3;
      }
    },
    frameCanvasSize: function (wid) {
      var frame = frames.get(wid);
      if (!frame) return null;
      var canvas = frame.bodyEl.querySelector("canvas");
      return canvas ? { width: canvas.width, height: canvas.height } : null;
    },
    shimWindow: function (wid) {
      var api = shim();
      if (!api) return null;
      var windows = api.windows();
      for (var i = 0; i < windows.length; i++) if (windows[i].wid === wid) return windows[i];
      return null;
    },
    clipboardText: function () {
      var api = shim();
      return api ? api.clipboardText() : "";
    },
    sendClipboard: function (text) {
      var api = shim();
      return api ? api.sendClipboardText(text) : false;
    },
    focusFrame: function (wid) { focus(wid); },
    minimizeFrame: function (wid) { minimize(wid); },
    restoreFrame: function (wid) { restore(wid); },
    maximizeFrame: function (wid) { maximize(wid); },
    closeWindow: function (wid) { close(wid); },
    resizeFrame: function (wid, width, height) {
      var frame = frames.get(wid);
      if (!frame) return false;
      frame.el.style.width = width + "px";
      frame.el.style.height = height + "px";
      scheduleResize(frame, 0);
      return true;
    },
    reloadSession: function () {
      sessionFrame.src = "/xpra/?reconnect=" + Date.now();
      return true;
    },
    stage: function (name) {
      log("STAGE: " + name);
      window.__POC_STAGE = name;
    },
  };

  log("harness ready");
})();
