/*!
 * RAST native Xpra bridge shim (rast-shim.js)
 * -------------------------------------------
 * Injected by the RAST server at the end of every proxied Xpra HTML5 page
 * (<script src="/linux-apps/session/<key>/rast-shim.js"></script> immediately
 * before </body>). The page is served same-origin, inside a non-sandboxed RAST
 * iframe; this shim is the only contract between the Xpra HTML5 client and the
 * RAST desktop window manager.
 *
 * Responsibilities:
 *  - capture the live XpraClient instance (the page creates it synchronously
 *    before this script runs: `var client = new XpraClient("screen")`);
 *  - hide the Xpra page chrome (the iframe is kept off-screen but laid out):
 *    the RAST desktop is the window manager, every remote top-level window
 *    becomes a first-class RAST window;
 *  - let the parent ADOPT each remote window's DOM node into its RAST window
 *    body (same-origin `ownerDocument.adoptNode`), switch the node to "fill"
 *    mode, and keep the server's X window in sync with the RAST window size;
 *  - forward keyboard/mouse/clipboard and window commands from the parent;
 *  - publish the remote window list (title, transient-for, modal, iconic,
 *    maximized, class-instance, ...) to the parent through a direct same-origin
 *    object (`window.RASTNative`) and, redundantly, window.postMessage.
 *
 * Nothing here is required for a plain Xpra page to work: every block is
 * guarded, failures only log to the console. No dependencies, idempotent.
 *
 * ES2017-compatible, classic script. Never throws at load.
 */
(function () {
  "use strict";

  var VERSION = "1.3.0";
  var ROOT = window;

  /* ------------------------------------------------------------------ state */

  var state = {
    client: null,
    ready: false,
    error: null,
  };
  var listeners = Object.create(null);
  var lastConnected = null;
  var lastWindowsSignature = "";
  var notifyTimer = 0;
  var pollTimer = 0;
  var patchedClient = null;
  var platformCache = null;

  /* Clipboard bookkeeping shared by the token/request patches below. */
  var clipboardState = {
    lastText: "",
    lastDatatype: "",
    lastEventAt: 0,
    direction: "both",
    requestCount: 0,
    responseCount: 0,
  };
  var pendingClipboardRequests = Object.create(null);
  var clipboardRequestSeq = 0;
  /* The desktop answers a clipboard request synchronously from its cached
   * clipboard in the normal case; this timeout only exists so an application
   * paste never hangs when the desktop cannot read the clipboard at all. */
  var CLIPBOARD_REQUEST_TIMEOUT_MS = 750;

  function log() {
    try {
      var args = ["[RAST native shim]"].concat(Array.prototype.slice.call(arguments));
      console.info.apply(console, args);
    } catch (e) {
      /* ignore */
    }
  }
  function warn() {
    try {
      var args = ["[RAST native shim]"].concat(Array.prototype.slice.call(arguments));
      console.warn.apply(console, args);
    } catch (e) {
      /* ignore */
    }
  }

  /* -------------------------------------------------------------- event bus */

  function on(event, callback) {
    if (typeof callback !== "function") return function () {};
    var list = listeners[event] || (listeners[event] = []);
    list.push(callback);
    return function () {
      off(event, callback);
    };
  }
  function off(event, callback) {
    var list = listeners[event];
    if (!list) return;
    var index = list.indexOf(callback);
    if (index >= 0) list.splice(index, 1);
  }
  function emit(event, payload) {
    var list = listeners[event];
    if (list) {
      var copy = list.slice();
      for (var i = 0; i < copy.length; i++) {
        try {
          copy[i](payload);
        } catch (error) {
          warn("listener failed for", event, error);
        }
      }
    }
    postToParent(event, payload);
  }

  /* ------------------------------------------------- parent <-> iframe bridge */

  function parentWindow() {
    try {
      return window.parent && window.parent !== window ? window.parent : null;
    } catch (e) {
      return null;
    }
  }

  function postToParent(event, payload) {
    var parent = parentWindow();
    if (!parent) return;
    try {
      var message = { type: "rast:native", event: event };
      if (payload && typeof payload === "object") {
        message.state = payload;
      }
      parent.postMessage(message, window.location.origin || "*");
    } catch (e) {
      /* cross-origin or detached parent: ignore */
    }
  }

  var MESSAGE_TYPE = "rast:native";

  function onParentMessage(event) {
    var parent = parentWindow();
    if (!parent || event.source !== parent) return;
    if (event.origin && event.origin !== window.location.origin) return;
    var data = event.data;
    if (!data || data.type !== MESSAGE_TYPE) return;
    var command = data.cmd;
    try {
      switch (command) {
        case "focus":
          focusWindow(data.wid);
          break;
        case "close-window":
          closeWindow(data.wid);
          break;
        case "resize":
          resizeWindow(data.wid, { width: data.width, height: data.height });
          break;
        case "adopt":
          if (typeof data.wid === "string" || typeof data.wid === "number") {
            /* The parent can only pass a node reference through the direct
             * API; a postMessage adopt is meaningless. Ignored. */
          }
          break;
        case "minimize":
          setMinimized(data.wid, data.minimized !== false);
          break;
        case "reconnect":
          reconnect();
          break;
        case "set-scale":
          setScale(data.scale);
          break;
        case "clipboard":
          sendClipboardText(data.text == null ? "" : String(data.text));
          break;
        case "request-clipboard":
          requestClipboard();
          break;
        case "get-state":
          emit("state", snapshot());
          break;
        default:
          warn("unknown parent command", command);
      }
    } catch (error) {
      warn("parent command failed", command, error);
      state.error = String(error);
      emit("error", snapshot());
    }
  }
  try {
    window.addEventListener("message", onParentMessage, false);
  } catch (e) {
    warn("could not listen for parent messages", e);
  }

  /* ---------------------------------------------------------- client capture */

  function whenClient(client) {
    if (!client || patchedClient === client) return;
    patchedClient = client;
    state.client = client;
    try {
      /* The RAST desktop maps sourceless pointer coordinates itself; 1:1
       * scaling keeps canvas pixels == CSS pixels. */
      client.scale = 1;
    } catch (e) {
      /* ignore */
    }
    // The viewer is intentionally offscreen; Chromium may throttle that
    // document's animation frames even though its adopted canvases are visible.
    // Schedule its paints on the visible desktop's clock instead.
    var parent = parentWindow();
    if (parent && parent.requestAnimationFrame) {
      window.requestAnimationFrame = parent.requestAnimationFrame.bind(parent);
      window.cancelAnimationFrame = parent.cancelAnimationFrame.bind(parent);
    }
    patchClient(client);
    // The desktop may capture the client after the first hello already ran:
    // pin the keyboard/clipboard contract and release the viewer's clipboard
    // autofocus as soon as we own the instance.
    applyKeyboardPolicy(client);
    neutralizeClipboardAutofocus();
    requestDesktopKeyboardFocus();
    state.ready = true;
    log("captured XpraClient, version", VERSION);
    emit("ready", snapshot());
    scheduleNotify();
  }

  /**
   * Try to capture the client as early as possible:
   *  1. the page's own `var client = new XpraClient("screen")` already ran
   *     before the shim (injection is right before </body>) -> window.client;
   *  2. if the shim is injected earlier (head), subclass XpraClient so the
   *     later `new XpraClient(...)` hands us the instance;
   *  3. otherwise poll for window.client (e.g. a different page layout).
   */
  function findClient() {
    try {
      if (window.client && window.client.container) return window.client;
      if (window.__RAST_XPRA_CAPTURED_CLIENT) return window.__RAST_XPRA_CAPTURED_CLIENT;
    } catch (e) {
      /* ignore */
    }
    return null;
  }

  function tryCapture() {
    var client = findClient();
    if (client) {
      whenClient(client);
      return true;
    }
    return false;
  }

  if (!tryCapture()) {
    try {
      var OriginalClient = typeof ROOT.XpraClient === "function" ? ROOT.XpraClient : null;
      if (OriginalClient) {
        var WrappedClient = class extends OriginalClient {
          constructor(...args) {
            super(...args);
            try {
              window.__RAST_XPRA_CAPTURED_CLIENT = this;
              whenClient(this);
            } catch (error) {
              warn("could not capture the XpraClient instance", error);
            }
          }
        };
        try {
          ROOT.XpraClient = WrappedClient;
          log("patched XpraClient constructor for capture");
        } catch (error) {
          warn("could not patch XpraClient", error);
        }
      }
    } catch (error) {
      warn("client capture setup failed", error);
    }
    pollTimer = window.setInterval(function () {
      if (tryCapture()) {
        window.clearInterval(pollTimer);
        pollTimer = 0;
      }
    }, 200);
  }

  /* ------------------------------------------------------- window snapshots */

  var DESKTOP_WINDOW_TYPES = {
    DESKTOP: true,
    DOCK: true,
    TOOLTIP: true,
    DROPDOWN: true,
    POPUP_MENU: true,
    COMBO: true,
    MENU: true,
    NOTIFICATION: true,
  };

  function normalizedList(value) {
    if (!value) return [];
    if (typeof value === "string") return [value];
    var out = [];
    try {
      for (var i = 0; i < value.length; i++) out.push(String(value[i]));
    } catch (e) {
      /* ignore */
    }
    return out;
  }

  function windowIconUrl(win) {
    try {
      var img = win.div.querySelector("img.windowicon");
      if (img && img.src && img.src.indexOf("data:") === 0) return img.src;
    } catch (e) {
      /* ignore */
    }
    return null;
  }

  function describeWindow(win) {
    var metadata = win.metadata || {};
    var types = normalizedList(metadata["window-type"]);
    if (!types.length) types = normalizedList(win.windowtype);
    var classInstance = normalizedList(metadata["class-instance"]);
    var transientFor = metadata["transient-for"];
    var minimized = false;
    try {
      minimized = !!win.minimized;
    } catch (e) {
      /* ignore */
    }
    var maximized = false;
    try {
      maximized = !!win.maximized;
    } catch (e) {
      /* ignore */
    }
    return {
      wid: String(win.wid),
      title: rawWindowTitles[String(win.wid)] || win.title || metadata.title || "Window",
      x: Math.round(win.x || 0),
      y: Math.round(win.y || 0),
      width: Math.max(1, Math.round(win.w || 1)),
      height: Math.max(1, Math.round(win.h || 1)),
      minimized: minimized,
      maximized: maximized,
      focused: !!win.focused,
      stacking: typeof win.stacking_layer === "number" ? win.stacking_layer : 0,
      modal: !!metadata.modal,
      transientFor: transientFor == null ? null : String(transientFor),
      windowType: types,
      classInstance: classInstance,
      overrideRedirect: !!win.override_redirect,
      resizable: !!win.resizable,
      iconUrl: windowIconUrl(win),
      adopted: !!win.__rastContentEl,
    };
  }

  function snapshot() {
    var client = state.client;
    var windows = [];
    var separate = true;
    var connected = false;
    var desktopWidth = 0;
    var desktopHeight = 0;
    var error = state.error;
    var disconnectedReason = null;
    if (client) {
      connected = !!client.connected;
      separate = !client.server_is_desktop && !client.server_is_shadow;
      desktopWidth = client.desktop_width || 0;
      desktopHeight = client.desktop_height || 0;
      if (client.disconnect_reason != null && !connected) {
        disconnectedReason = String(client.disconnect_reason);
      }
      var map = client.id_to_window || {};
      for (var wid in map) {
        if (!Object.prototype.hasOwnProperty.call(map, wid)) continue;
        var win = map[wid];
        if (!win || !win.div) continue;
        if (win.tray) continue;
        var info = describeWindow(win);
        if (!separate && info.windowType.indexOf("DESKTOP") < 0) {
          /* desktop/shadow mode: a single full-desktop window is expected */
        }
        windows.push(info);
      }
      windows.sort(function (a, b) {
        return a.stacking - b.stacking || Number(a.wid) - Number(b.wid);
      });
    }
    return {
      version: VERSION,
      key: sessionKey(),
      ready: state.ready,
      connected: connected,
      separate: separate,
      error: error,
      disconnectReason: disconnectedReason,
      size: [desktopWidth, desktopHeight],
      desktopWidth: desktopWidth,
      desktopHeight: desktopHeight,
      clipboardEnabled: !!(client && client.clipboard_enabled),
      clipboardDirection: client ? String(client.clipboard_direction || "") : "",
      clipboardPending: clipboardState.requestCount - clipboardState.responseCount,
      clipboardLastEventAt: clipboardState.lastEventAt,
      keyboardSwap: !!(client && client.swap_keys),
      captureKeyboard: !!(client && client.capture_keyboard),
      audio: {
        enabled: !!(client && client.audio_enabled),
        state: client ? client.audio_state : "",
        codec: client ? client.audio_codec : null,
      },
      windows: windows,
    };
  }

  function sessionKey() {
    try {
      var match = /\/linux-apps\/session\/([0-9a-fA-F]{32})\//.exec(window.location.pathname || "");
      return match ? match[1] : null;
    } catch (e) {
      return null;
    }
  }

  function windowsSignature(windows) {
    var parts = [];
    for (var i = 0; i < windows.length; i++) {
      var w = windows[i];
      parts.push(
        [w.wid, w.title, w.width, w.height, w.minimized ? 1 : 0, w.maximized ? 1 : 0,
          w.transientFor || "", w.modal ? 1 : 0].join(":"),
      );
    }
    return parts.join("|");
  }

  function scheduleNotify() {
    if (notifyTimer) return;
    notifyTimer = window.setTimeout(function () {
      notifyTimer = 0;
      notify();
    }, 24);
  }

  function notify() {
    var current = snapshot();
    emit("state", current);
    var signature = windowsSignature(current.windows);
    if (signature !== lastWindowsSignature) {
      lastWindowsSignature = signature;
      emit("windows", current);
    }
    if (lastConnected !== current.connected) {
      lastConnected = current.connected;
      if (current.connected) emit("connected", current);
      else emit("disconnected", current);
    }
    /* Mirror a compact state for debuggability / late-joining parents. */
    try {
      window.__RAST_NATIVE_STATE = {
        connected: current.connected,
        separate: current.separate,
        windows: current.windows.map(function (w) {
          return {
            wid: w.wid, title: w.title, w: w.width, h: w.height,
            x: w.x, y: w.y, focused: w.focused, minimized: w.minimized,
            maximized: w.maximized, modal: w.modal, transientFor: w.transientFor,
          };
        }),
        size: current.size,
        error: current.error,
      };
    } catch (e) {
      /* ignore */
    }
  }

  /* --------------------------------------------------- platform + focus */

  /** navigator of the top-level RAST document (same-origin), or ours. */
  function desktopNavigator() {
    try {
      var parent = parentWindow();
      if (parent && parent.navigator) return parent.navigator;
    } catch (e) {
      /* cross-origin: use our own */
    }
    try {
      return window.navigator;
    } catch (e) {
      return null;
    }
  }

  /**
   * The desktop client's platform. RAST and the viewer share an origin, so the
   * parent navigator is the authoritative one (and is not Chrome-specific).
   */
  function isMacPlatform() {
    if (platformCache !== null) return platformCache;
    var result = false;
    try {
      var nav = desktopNavigator();
      if (nav) {
        var platform = (nav.userAgentData && nav.userAgentData.platform) || nav.platform || "";
        result =
          /mac/i.test(platform) ||
          /Macintosh|Mac OS X/i.test(nav.userAgent || "");
      }
    } catch (e) {
      result = false;
    }
    platformCache = result;
    return result;
  }

  /**
   * The RAST desktop is the only keyboard owner. The hidden viewer must not
   * keep DOM focus: the Xpra page focuses its #pasteboard element and re-focuses
   * it on blur, which pulls every keystroke into the off-screen iframe and
   * bypasses the desktop's key routing (and leaves the browser clipboard APIs
   * running in an unfocused document, where they are rejected). Replacing the
   * node drops the viewer's jQuery blur handler; the desktop then focuses the
   * adopted window body after connecting.
   */
  function neutralizeClipboardAutofocus() {
    var changed = false;
    try {
      var pasteboard = document.getElementById("pasteboard");
      if (pasteboard && pasteboard.parentNode) {
        var clone = pasteboard.cloneNode(true);
        try {
          clone.removeAttribute("autofocus");
        } catch (e) {
          /* ignore */
        }
        try {
          clone.setAttribute("data-rast-neutralized", "1");
        } catch (e) {
          /* ignore */
        }
        pasteboard.parentNode.replaceChild(clone, pasteboard);
        changed = true;
      }
      var active = document.activeElement;
      if (active && active !== document.body && typeof active.blur === "function") {
        try {
          active.blur();
        } catch (e) {
          /* ignore */
        }
        changed = true;
      }
    } catch (error) {
      warn("could not release the viewer's clipboard focus", error);
    }
    return changed;
  }

  /** Ask the desktop to put keyboard focus back on the adopted window body. */
  function requestDesktopKeyboardFocus() {
    try {
      emit("focus-request", {});
    } catch (e) {
      /* ignore */
    }
  }

  /**
   * Pin the keyboard/clipboard contract of the pinned Xpra HTML5 client:
   *  - macOS clients get xpra-html5's Meta<->Control swap, so the desktop can
   *    forward the real Command events and Linux applications receive their
   *    usual Control shortcuts (and Control+click behaves like Ctrl+click);
   *  - every other platform keeps its native Control mapping;
   *  - key capture is enforced because only the RAST desktop sends keys here.
   */
  function applyKeyboardPolicy(client) {
    if (!client) return;
    try {
      client.swap_keys = isMacPlatform();
    } catch (e) {
      warn("could not set the keyboard swap mode", e);
    }
    try {
      client.capture_keyboard = true;
    } catch (e) {
      warn("could not enable keyboard capture", e);
    }
    try {
      clipboardState.direction = String(client.clipboard_direction || "both");
    } catch (e) {
      /* ignore */
    }
  }

  /* --------------------------------------------------- clipboard patches */

  function replaceClientMethod(client, name, wrapped, original) {
    try {
      client[name] = wrapped;
    } catch (e) {
      warn("could not patch", name, e);
      return false;
    }
    try {
      var table = client.packet_handlers;
      if (table && original) {
        for (var key in table) {
          if (table[key] === original) table[key] = wrapped;
        }
      }
    } catch (e) {
      /* ignore */
    }
    return true;
  }

  /**
   * Remote -> browser copy. The pinned client plays the token itself (writing
   * navigator.clipboard from its hidden, unfocused document, which browsers
   * reject), so the desktop must receive the token and write the clipboard from
   * its focused document instead. Only text targets are forwarded; image/HTML
   * data is reported as unsupported instead of pretending it was copied.
   */
  function patchClipboardTokens(client) {
    var original = client._process_clipboard_token;
    if (typeof original !== "function" || original.__rastClipboardToken) return;
    var wrapped = function (packet) {
      var result;
      try {
        result = original.apply(this, arguments);
      } finally {
        try {
          var datatype = packet && packet.length > 4 ? String(packet[4] == null ? "" : packet[4]) : "";
          var selection = packet && packet.length > 1 ? String(packet[1] == null ? "CLIPBOARD" : packet[1]) : "CLIPBOARD";
          var lower = datatype.toLowerCase();
          var isText = lower === "" || lower.indexOf("text") >= 0 || lower.indexOf("string") >= 0;
          if (isText) {
            var text = client.clipboard_buffer == null ? "" : String(client.clipboard_buffer);
            if (text && text !== clipboardState.lastText) {
              clipboardState.lastText = text;
              clipboardState.lastDatatype = datatype || "text/plain";
              clipboardState.lastEventAt = Date.now();
              emit("clipboard", {
                text: text,
                datatype: clipboardState.lastDatatype,
                selection: selection,
              });
            }
          } else {
            emit("clipboard-format", { datatype: datatype || "unknown", selection: selection, supported: false });
          }
        } catch (error) {
          warn("clipboard token handling failed", error);
        }
      }
      return result;
    };
    wrapped.__rastClipboardToken = true;
    replaceClientMethod(client, "_process_clipboard_token", wrapped, original);
  }

  /**
   * Browser -> application paste. The X11 application asks the Xpra server for
   * the clipboard; the server asks this client, and the pinned client would read
   * navigator.clipboard from its unfocused document (refused). Defer the request
   * to the focused desktop document and fall back to the client's own path if
   * the desktop cannot answer in time, so an application paste never hangs.
   */
  function patchClipboardRequests(client) {
    var original = client._process_clipboard_request;
    if (typeof original !== "function" || original.__rastClipboardRequest) return;
    var wrapped = function (packet) {
      var requestId = packet && packet.length > 1 ? packet[1] : 0;
      var selection = packet && packet.length > 2 ? String(packet[2] == null ? "CLIPBOARD" : packet[2]) : "CLIPBOARD";
      clipboardRequestSeq += 1;
      var key = selection + ":" + String(requestId) + ":" + clipboardRequestSeq;
      var entry = {
        key: key,
        requestId: requestId,
        selection: selection,
        timer: 0,
        done: false,
        createdAt: Date.now(),
      };
      entry.finish = function (text, provide) {
        if (entry.done) return false;
        entry.done = true;
        if (entry.timer) {
          try {
            window.clearTimeout(entry.timer);
          } catch (e) {
            /* ignore */
          }
          entry.timer = 0;
        }
        delete pendingClipboardRequests[key];
        try {
          if (provide) {
            client.send_clipboard_string(requestId, selection, text == null ? "" : String(text));
          } else {
            original.call(client, packet);
          }
          clipboardState.responseCount += 1;
          return true;
        } catch (error) {
          warn("clipboard request answer failed", error);
          return false;
        }
      };
      pendingClipboardRequests[key] = entry;
      clipboardState.requestCount += 1;
      entry.timer = window.setTimeout(function () {
        entry.timer = 0;
        if (entry.done) return;
        entry.finish(null, false);
      }, CLIPBOARD_REQUEST_TIMEOUT_MS);
      emit("clipboard-request", {
        key: key,
        requestId: String(requestId),
        selection: selection,
      });
      return true;
    };
    wrapped.__rastClipboardRequest = true;
    replaceClientMethod(client, "_process_clipboard_request", wrapped, original);
  }

  function clearClipboardRequests() {
    for (var key in pendingClipboardRequests) {
      if (!Object.prototype.hasOwnProperty.call(pendingClipboardRequests, key)) continue;
      var entry = pendingClipboardRequests[key];
      if (!entry) continue;
      entry.done = true;
      if (entry.timer) {
        try {
          window.clearTimeout(entry.timer);
        } catch (e) {
          /* ignore */
        }
        entry.timer = 0;
      }
      delete pendingClipboardRequests[key];
    }
  }

  /** Called after the connection is replaced: no stale requests/buffers. */
  function onConnectionLost() {
    clearClipboardRequests();
    clipboardState.lastText = "";
    clipboardState.lastDatatype = "";
  }

  /** Called after every hello: pin the contract and the desktop focus. */
  function onSessionConnected() {
    applyKeyboardPolicy(state.client);
    var released = neutralizeClipboardAutofocus();
    requestDesktopKeyboardFocus();
    if (released) log("released the viewer clipboard autofocus");
  }

  /* ------------------------------------------------ wheel normalization */

  /**
   * Shift+wheel: browsers disagree (some report only a vertical delta). Linux
   * applications expect a horizontal scroll while Shift is held, so convert an
   * untranslated vertical delta into the horizontal component. Real horizontal
   * trackpad/wheel deltas are left untouched.
   */
  function patchWheelNormalization() {
    try {
      var utils = window.Utilities;
      if (!utils || typeof utils.normalizeWheel !== "function" || utils.normalizeWheel.__rastWheel) return;
      var original = utils.normalizeWheel;
      var wrapped = function (event) {
        var normalized = original.call(utils, event);
        try {
          if (event && event.shiftKey && normalized && normalized.pixelY && !normalized.pixelX) {
            var pixelX = normalized.pixelY;
            return {
              spinX: pixelX < 1 ? -1 : 1,
              spinY: 0,
              pixelX: pixelX,
              pixelY: 0,
              deltaMode: normalized.deltaMode || 0,
            };
          }
        } catch (e) {
          /* keep the original result */
        }
        return normalized;
      };
      wrapped.__rastWheel = true;
      utils.normalizeWheel = wrapped;
    } catch (error) {
      warn("could not normalize shift-wheel scrolling", error);
    }
  }

  /* ------------------------------------------------------- client patching */

  function patchHandler(client, name, after) {
    var original = client[name];
    if (typeof original !== "function" || original.__rastWrapped) return;
    var wrapped = function () {
      var result;
      try {
        result = original.apply(this, arguments);
      } finally {
        try {
          if (after) after(arguments);
        } catch (e) {
          /* ignore */
        }
        scheduleNotify();
      }
      return result;
    };
    wrapped.__rastWrapped = true;
    try {
      client[name] = wrapped;
    } catch (e) {
      warn("could not patch", name, e);
      return;
    }
    /* If xpra already built its packet handler table, re-point it. */
    var handlers = client.packet_handlers;
    if (handlers && typeof handlers === "object") {
      for (var key in handlers) {
        if (handlers[key] === original) handlers[key] = wrapped;
      }
    }
  }

  /**
   * Wrap a client packet handler so `before` can sanitize its arguments before
   * the pinned Xpra HTML5 client sees them. The client renders remote window
   * titles with jQuery `.html()` (xpra-html5 v21, Window.js), so application
   * metadata is untrusted HTML from the app's point of view: it is escaped
   * before rendering. The raw title is kept for the RAST desktop, which uses
   * text nodes only.
   */
  function patchBefore(client, name, before) {
    var original = client[name];
    if (typeof original !== "function" || original.__rastBefore) return;
    var wrapped = function () {
      try {
        before(arguments);
      } catch (e) {
        /* never let sanitizing break the client */
      }
      return original.apply(this, arguments);
    };
    wrapped.__rastBefore = true;
    try {
      client[name] = wrapped;
    } catch (e) {
      warn("could not patch", name, e);
      return;
    }
    var handlers = client.packet_handlers;
    if (handlers && typeof handlers === "object") {
      for (var key in handlers) {
        if (handlers[key] === original) handlers[key] = wrapped;
      }
    }
  }

  /* Remote window titles reach the Xpra client's own DOM through `.html()`;
   * they are untrusted text from the application (a web page title, a file
   * name, a credential prompt). Escape them before the client renders them and
   * keep the original string for the RAST window manager. */
  var rawWindowTitles = Object.create(null);

  function escapeTitleHtml(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function sanitizeTitleMetadata(wid, metadata) {
    if (!metadata || typeof metadata !== "object" || metadata.title == null) return;
    var raw = String(metadata.title);
    if (wid != null) rawWindowTitles[String(wid)] = raw;
    metadata.title = escapeTitleHtml(raw);
  }

  function sanitizeWindowPackets(client) {
    patchBefore(client, "_process_window_metadata", function (args) {
      var packet = args[0];
      if (!packet || packet.length < 3) return;
      sanitizeTitleMetadata(packet[1], packet[2]);
    });
    var newWindow = function (args) {
      var packet = args[0];
      if (!packet || packet.length < 7) return;
      sanitizeTitleMetadata(packet[1], packet[6]);
    };
    patchBefore(client, "_process_new_window", newWindow);
    patchBefore(client, "_process_new_override_redirect", newWindow);
    patchBefore(client, "_process_lost_window", function (args) {
      var packet = args[0];
      if (packet && packet.length > 1) delete rawWindowTitles[String(packet[1])];
    });
  }

  function patchClient(client) {
    if (!client) return;
    var handlers = [
      "_process_hello",
      "_process_close",
      "_process_disconnect",
      "_process_new_window",
      "_process_new_override_redirect",
      "_process_new_tray",
      "_process_lost_window",
      "_process_window_metadata",
      "_process_window_icon",
      "_process_raise_window",
      "_process_window_move_resize",
      "_process_window_resized",
      "_process_setting_change",
    ];
    var afterHandlers = {
      _process_hello: onSessionConnected,
      _process_close: onConnectionLost,
      _process_disconnect: onConnectionLost,
    };
    for (var i = 0; i < handlers.length; i++) {
      var handlerName = handlers[i];
      patchHandler(client, handlerName, afterHandlers[handlerName] || null);
    }
    patchClipboardTokens(client);
    patchClipboardRequests(client);
    patchGetMouse(client);
    patchWheelNormalization();
    sanitizeWindowPackets(client);
    /* Some builds assign handlers directly on the instance; make sure the
     * packet table references our wrappers even if it was built earlier. */
    try {
      var handlersTable = client.packet_handlers;
      if (handlersTable) {
        for (var key in handlersTable) {
          var fn = handlersTable[key];
          if (typeof fn === "function" && !fn.__rastWrapped) {
            /* leave untouched: only our known names are wrapped above */
          }
        }
      }
    } catch (e) {
      /* ignore */
    }
    lastConnected = null;
    /* Start watching connection state even if no packets flow. */
    startPolling();
  }

  /**
   * Remote windows live in the iframe document, but their input handlers send
   * coordinates relative to the *remote* window position (win.x/win.y). After
   * adoption the DOM node is in the parent document, so pointer events carry
   * the parent's viewport coordinates. Rebase them on the RAST window body and
   * pin win.x/win.y to 0, so the client computes the correct window-relative
   * coordinates for the X server.
   */
  function patchGetMouse(client) {
    var original = client.getMouse;
    if (typeof original !== "function" || original.__rastWrapped) return;
    var wrapped = function (event, win) {
      try {
        // Xpra passes win for button/wheel events, but NOT pointer motion or
        // stale-button release. Resolve those from the adopted event target.
        var nativeEvent = event && (event.originalEvent || event);
        if (!win && nativeEvent && nativeEvent.target) {
          var target = nativeEvent.target;
          var adopted = target.closest && target.closest("[data-rast-native-wid]");
          if (adopted) win = getWindow(adopted.getAttribute("data-rast-native-wid"));
        }
        var content = win && win.__rastContentEl;
        if (content && nativeEvent) {
          if (nativeEvent && typeof nativeEvent.clientX === "number") {
            var rect = content.getBoundingClientRect();
            var button = 0;
            if (typeof nativeEvent.which === "number") {
              button = Math.max(0, nativeEvent.which);
            } else if (typeof nativeEvent.button === "number") {
              button = Math.max(0, nativeEvent.button) + 1;
            }
            win.__rastLastPointerEvent = nativeEvent;
            return {
              // Keep absolute X11 desktop coordinates AND consistent relative
              // coordinates (the client subtracts get_internal_geometry).
              // During a resize the backing canvas may still have the old size.
              x: win.x + (nativeEvent.clientX - rect.left) * win.w / (rect.width || win.w),
              y: win.y + (nativeEvent.clientY - rect.top) * win.h / (rect.height || win.h),
              button: button,
            };
          }
        }
      } catch (e) {
        /* fall through */
      }
      return original.call(this, event, win);
    };
    wrapped.__rastWrapped = true;
    try {
      client.getMouse = wrapped;
    } catch (e) {
      warn("could not patch getMouse", e);
    }
  }

  /** Preserve button-up while dragging tabs beyond an adopted canvas. The
   * iframe's document/screen handlers cannot see events in the parent page. */
  function installInputRecovery(win) {
    if (!win.canvas || win.__rastInputRecovery) return;
    var canvas = win.canvas;
    var parent = canvas.ownerDocument.defaultView;
    var markDown = function (event) {
      win.__rastPointerDown = true;
      if (event && event.pointerType && event.pointerType !== "mouse") {
        // Mouse capture changes compatibility mouse-event delivery in some
        // browsers; only capture touch/pen, where it is required for dragging.
        try { canvas.setPointerCapture(event.pointerId); } catch (e) { /* unsupported */ }
      }
      unlockAudio();
    };
    var release = function () {
      if (!win.__rastPointerDown) return;
      win.__rastPointerDown = false;
      var client = state.client;
      var lastEvent = win.__rastLastPointerEvent;
      win.__rastLastPointerEvent = null;
      // Without a recorded adopted pointer event Xpra's release_buttons would
      // call getMouse(undefined) and throw inside the viewer; the button state
      // is then cleared directly below instead.
      if (!lastEvent) {
        try {
          if (client && client.buttons_pressed && typeof client.buttons_pressed.clear === "function") {
            client.buttons_pressed.clear();
          }
        } catch (e) {
          /* ignore */
        }
        return;
      }
      if (client && typeof client.release_buttons === "function") client.release_buttons(lastEvent, win);
    };
    canvas.addEventListener("pointerdown", markDown);
    canvas.addEventListener("mousedown", markDown);
    canvas.addEventListener("pointercancel", release);
    // The transport iframe cannot see a button release over the RAST document
    // (drag out of the adopted window, a title-bar press, focus loss): release
    // on the visible desktop page instead so the remote drag never sticks.
    var releaseAll = function () {
      release();
    };
    if (parent) {
      parent.addEventListener("pointerup", releaseAll);
      parent.addEventListener("pointercancel", releaseAll);
      parent.addEventListener("blur", releaseAll);
    }
    var destroy = win.destroy;
    win.destroy = function () {
      if (win.__rastInputRecovery) win.__rastInputRecovery();
      win.__rastInputRecovery = null;
      return destroy.apply(this, arguments);
    };
    win.__rastInputRecovery = function () {
      canvas.removeEventListener("pointerdown", markDown);
      canvas.removeEventListener("mousedown", markDown);
      canvas.removeEventListener("pointercancel", release);
      if (parent) {
        parent.removeEventListener("pointerup", releaseAll);
        parent.removeEventListener("pointercancel", releaseAll);
        parent.removeEventListener("blur", releaseAll);
      }
      win.destroy = destroy;
    };
  }

  /** Release every mouse button held by an adopted remote window. */
  function releaseInput() {
    var client = state.client;
    if (!client) return false;
    try {
      var map = client.id_to_window || {};
      for (var wid in map) {
        if (!Object.prototype.hasOwnProperty.call(map, wid)) continue;
        var win = map[wid];
        if (!win || !win.__rastContentEl) continue;
        win.__rastPointerDown = false;
        var lastEvent = win.__rastLastPointerEvent;
        win.__rastLastPointerEvent = null;
        if (!lastEvent) continue;
        if (typeof client.release_buttons === "function") client.release_buttons(lastEvent, win);
      }
      if (client.buttons_pressed && typeof client.buttons_pressed.clear === "function") {
        client.buttons_pressed.clear();
      }
      return true;
    } catch (error) {
      warn("could not release the remote mouse buttons", error);
      return false;
    }
  }

  /* ------------------------------------------------------- polling fallback */

  function startPolling() {
    if (pollTimer) window.clearInterval(pollTimer);
    pollTimer = window.setInterval(function () {
      if (!state.client) {
        tryCapture();
        return;
      }
      try {
        var connected = !!state.client.connected;
        if (connected !== lastConnected) scheduleNotify();
        /* Catch windows created by a re-connect before handlers are ready. */
        scheduleNotify();
      } catch (e) {
        /* ignore */
      }
    }, 1000);
  }

  /* --------------------------------------------------------------- commands */

  function getWindow(wid) {
    var client = state.client;
    if (!client || wid == null) return null;
    var map = client.id_to_window || {};
    var key = String(wid);
    if (map[key]) return map[key];
    /* numeric lookup in case the map keys are numbers in some builds */
    var numeric = Number(key);
    if (!isNaN(numeric) && map[numeric]) return map[numeric];
    return null;
  }

  function focusWindow(wid) {
    var client = state.client;
    var win = getWindow(wid);
    if (!client || !win) return false;
    try {
      if (win.minimized) {
        setMinimized(wid, false);
        return true;
      }
      client.set_focus(win);
      focusFrame(win);
      scheduleNotify();
      return true;
    } catch (error) {
      warn("focus failed", wid, error);
      return false;
    }
  }

  /** Keep DOM focus on the adopted window body so RAST key handling routes. */
  function focusFrame(win) {
    try {
      var content = win.__rastContentEl;
      if (content && content.focus) {
        if (!content.hasAttribute("tabindex")) content.setAttribute("tabindex", "-1");
        content.focus({ preventScroll: true });
      }
    } catch (e) {
      /* ignore */
    }
  }

  function closeWindow(wid) {
    var client = state.client;
    var win = getWindow(wid);
    if (!client || !win) return false;
    try {
      /* WM_DELETE: the application's own close behaviour (may show prompts). */
      client.send_close_window(win);
      return true;
    } catch (error) {
      warn("close failed", wid, error);
      return false;
    }
  }

  /**
   * Tell the X server the window's new content size (RAST window resize).
   * The RAST window manager owns the position; remote x/y are irrelevant, so
   * adopted windows are pinned to (0,0) and only the size is negotiated.
   */
  function resizeWindow(wid, geometry) {
    var win = getWindow(wid);
    if (!win || !geometry) return false;
    try {
      var width = Math.max(1, Math.round(Number(geometry.width) || win.w));
      var height = Math.max(1, Math.round(Number(geometry.height) || win.h));
      if (win.w !== width || win.h !== height) {
        win.w = width;
        win.h = height;
        try {
          if (typeof win.updateCSSGeometry === "function") win.updateCSSGeometry();
        } catch (e) {
          /* ignore */
        }
        try {
          win.geometry_cb(win);
        } catch (e) {
          warn("configure failed", e);
        }
        scheduleRefresh(wid);
        scheduleNotify();
      }
      return true;
    } catch (error) {
      warn("resize failed", wid, error);
      return false;
    }
  }

  /** Ask the server for a full window buffer refresh (canvas was resized). */
  function requestRefresh(wid) {
    var client = state.client;
    var win = getWindow(wid);
    if (!client || !win || !client.connected) return false;
    try {
      if (typeof client.request_refresh === "function") client.request_refresh(win.wid);
      return true;
    } catch (error) {
      return false;
    }
  }

  /** The browser clears a canvas when its backing store is resized; ask the
   * X server to repaint the whole window shortly after (same trick xpra uses
   * after its own resize handles). */
  function scheduleRefresh(wid) {
    var delays = [160, 520, 1300];
    for (var i = 0; i < delays.length; i++) {
      (function (delay) {
        window.setTimeout(function () {
          requestRefresh(wid);
        }, delay);
      })(delays[i]);
    }
  }

  function setMinimized(wid, minimized) {
    var client = state.client;
    var win = getWindow(wid);
    if (!client || !win) return false;
    var next = !!minimized;
    try {
      if (!!win.minimized === next) return true;
      // Hide/show only. An X11 window-unmap/window-map pair (Xpra's own
      // toggle_minimized does this) stops the application from painting while
      // hidden, but it also loses the X input focus: after a restore the app
      // kept painting yet ignored keyboard input until it was clicked or the
      // viewer reconnected. Correct focus is more important than the saved
      // frames, so the remote window stays mapped and only the RAST window and
      // the adopted DOM node are hidden. See NATIVE-LINUX-APPS.md.
      win.set_minimized(next);
      if (!next) {
        focusFrame(win);
        scheduleRefresh(wid);
      }
      scheduleNotify();
      return true;
    } catch (error) {
      warn("minimize failed", wid, error);
      return false;
    }
  }

  /* -------------------------------------------------------------- adoption */

  /**
   * Move a remote Xpra window's DOM node into a RAST-owned container and make
   * it fill the container. The Xpra client keeps drawing into its own canvas
   * and its own input handlers keep working (the node object is the same).
   */
  function adoptWindow(wid, container) {
    var win = getWindow(wid);
    if (!win || !container || !win.div) return false;
    try {
      var doc = container.ownerDocument || document;
      hideXpraChrome(win);
      // Xpra names its window nodes `.window`: after adoption they live in the
      // RAST document and RAST's own `.window` rules would apply to them.
      // Rename the node so both window managers keep their own styles.
      try {
        if (win.__rastOriginalClassName == null) win.__rastOriginalClassName = win.div.className;
        win.div.className = "rast-native-xpra-window";
      } catch (e) {
        /* ignore */
      }
      win.__rastContentEl = container;
      win.div.setAttribute("data-rast-native-wid", String(wid));
      patchWindowForFill(win);
      if (win.div.parentNode !== container) {
        var adopted = doc.adoptNode(win.div);
        container.appendChild(adopted);
      }
      installInputRecovery(win);
      try {
        win.updateCSSGeometry();
      } catch (e) {
        /* ignore */
      }
      scheduleRefresh(wid);
      focusFrame(win);
      scheduleNotify();
      return true;
    } catch (error) {
      warn("adopt failed", wid, error);
      return false;
    }
  }

  function hideXpraChrome(win) {
    try {
      var div = win.div;
      var head = div.querySelector(".windowhead");
      if (head) head.style.display = "none";
      var icons = div.querySelectorAll(".ui-resizable-handle");
      for (var i = 0; i < icons.length; i++) icons[i].style.display = "none";
      var spinner = div.querySelector(".spinneroverlay");
      if (spinner) {
        // Xpra's connection spinner belongs to the viewer document. Without
        // that document's stylesheet it is laid out here as an in-flow block,
        // and RAST's global `.spinner` styling turns it into a permanently
        // visible, animated loading ring at the top-left of the window. The
        // adopted window paints from its canvas and RAST shows its own
        // reconnect state, so drop the element entirely (this also stops the
        // spinning animation from costing paint work).
        spinner.remove();
      }
      win.decorations = false;
    } catch (e) {
      /* ignore */
    }
  }

  function patchWindowForFill(win) {
    if (win.__rastFillPatched) return;
    win.__rastFillPatched = true;
    win.__rastOriginal = {
      updateCSSGeometry: win.updateCSSGeometry,
      update_offsets: win.update_offsets,
      _set_decorated: win._set_decorated,
      update_zindex: win.update_zindex,
      updateFocus: win.updateFocus,
      set_minimized: win.set_minimized,
      destroy: win.destroy,
    };

    win.update_offsets = function () {
      this.leftoffset = 0;
      this.rightoffset = 0;
      this.topoffset = 0;
      this.bottomoffset = 0;
    };
    win._set_decorated = function () {
      this.decorated = false;
      hideXpraChrome(this);
    };
    win.updateCSSGeometry = function () {
      var resized = this.w !== this.__rastCanvasW || this.h !== this.__rastCanvasH;
      this.__rastCanvasW = this.w;
      this.__rastCanvasH = this.h;
      try {
        this.updateCanvasGeometry();
      } catch (e) {
        /* ignore */
      }
      var div = this.div;
      if (!div) return;
      var style = div.style;
      style.position = "absolute";
      style.left = "0px";
      style.top = "0px";
      style.margin = "0px";
      style.width = "100%";
      style.height = "100%";
      style.transform = "none";
      style.zIndex = "";
      var head = div.querySelector(".windowhead");
      if (head) head.style.display = "none";
      if (this.canvas) {
        var canvasStyle = this.canvas.style;
        canvasStyle.position = "absolute";
        canvasStyle.left = "0px";
        canvasStyle.top = "0px";
        canvasStyle.margin = "0px";
        canvasStyle.width = "100%";
        canvasStyle.height = "100%";
        canvasStyle.maxWidth = "none";
        canvasStyle.maxHeight = "none";
        canvasStyle.display = "block";
      }
      // Server-initiated resize also clears the canvas backing store.
      if (resized) scheduleRefresh(this.wid);
    };
    win.update_zindex = function () {
      if (!this.div) return;
      this.div.style.zIndex = this.tray ? "0" : "";
    };
    win.updateFocus = function () {
      // The RAST window manager draws the focus state; keep Xpra's node clean.
      if (this.focused) focusFrame(this);
    };
    win.set_minimized = function (minimized) {
      this.minimized = !!minimized;
      if (this.div) this.div.style.display = minimized ? "none" : "";
    };
  }

  /** Undo adoption (e.g. the parent falls back to a whole-session window). */
  function detachWindow(wid) {
    var win = getWindow(wid);
    if (!win || !win.__rastContentEl) return false;
    try {
      var screen = document.querySelector("#screen");
      var original = win.__rastOriginal;
      if (win.__rastInputRecovery) win.__rastInputRecovery();
      win.__rastInputRecovery = null;
      win.div.removeAttribute("data-rast-native-wid");
      win.__rastContentEl = null;
      win.__rastFillPatched = false;
      win._set_decorated = original._set_decorated;
      win.update_offsets = original.update_offsets;
      win.updateCSSGeometry = original.updateCSSGeometry;
      win.update_zindex = original.update_zindex;
      win.updateFocus = original.updateFocus;
      win.set_minimized = original.set_minimized;
      win.div.removeAttribute("tabindex");
      if (win.__rastOriginalClassName != null) {
        win.div.className = win.__rastOriginalClassName;
        win.__rastOriginalClassName = null;
      }
      if (screen) {
        screen.appendChild(document.adoptNode(win.div));
      }
      try {
        win.updateCSSGeometry();
      } catch (e) {
        /* ignore */
      }
      scheduleNotify();
      return true;
    } catch (error) {
      warn("detach failed", wid, error);
      return false;
    }
  }

  /* --------------------------------------------------------------- keyboard */

  /**
   * Forward one key event. `init` mirrors KeyboardEventInit plus legacy
   * keyCode/which (Chromium only fills keyCode for trusted events; we
   * redefine it explicitly so xpra's mapping works for synthetic events).
   */
  function sendKey(pressed, init) {
    init = init || {};
    var keyCode = typeof init.keyCode === "number" ? init.keyCode : 0;
    try {
      var event = new window.KeyboardEvent(pressed ? "keydown" : "keyup", {
        key: init.key || "",
        code: init.code || "",
        location: typeof init.location === "number" ? init.location : 0,
        ctrlKey: !!init.ctrlKey,
        shiftKey: !!init.shiftKey,
        altKey: !!init.altKey,
        metaKey: !!init.metaKey,
        repeat: !!init.repeat,
        bubbles: true,
        cancelable: true,
        composed: true,
      });
      if (keyCode) {
        try {
          Object.defineProperty(event, "keyCode", { get: function () { return keyCode; } });
        } catch (e) {
          /* ignore */
        }
        try {
          Object.defineProperty(event, "which", { get: function () { return keyCode; } });
        } catch (e) {
          /* ignore */
        }
        try {
          Object.defineProperty(event, "charCode", { get: function () { return 0; } });
        } catch (e) {
          /* ignore */
        }
      }
      document.dispatchEvent(event);
      return true;
    } catch (error) {
      /* Fallback: call the client's key processor directly. */
      try {
        var client = state.client;
        if (client && typeof client._keyb_process === "function") {
          client._keyb_process(!!pressed, {
            key: init.key || "",
            code: init.code || "",
            which: keyCode,
            keyCode: keyCode,
            ctrlKey: !!init.ctrlKey,
            shiftKey: !!init.shiftKey,
            altKey: !!init.altKey,
            metaKey: !!init.metaKey,
            preventDefault: function () {},
            stopPropagation: function () {},
          });
          return true;
        }
      } catch (error2) {
        warn("key forwarding failed", error2);
      }
      return false;
    }
  }

  /** Ignore the iframe's own document key handling for keys the parent owns. */
  function setKeyboardCapture(enabled) {
    try {
      if (state.client) state.client.capture_keyboard = !!enabled;
      return true;
    } catch (e) {
      return false;
    }
  }

  /* -------------------------------------------------------------- clipboard */

  function sendClipboardText(text) {
    var client = state.client;
    if (!client || !client.connected) return false;
    if (client.clipboard_enabled === false) return false;
    var value = text == null ? "" : String(text);
    if (!value) return false;
    try {
      var bytes = value;
      try {
        if (window.Utilities && typeof window.Utilities.StringToUint8 === "function") {
          bytes = window.Utilities.StringToUint8(value);
        }
      } catch (e) {
        /* send the string as-is */
      }
      client.clipboard_buffer = value;
      try {
        client.clipboard_datatype = "text/plain";
      } catch (e) {
        /* ignore */
      }
      client.send_clipboard_token(bytes);
      // Remember what the desktop pushed: a token echoed back by the server for
      // identical content must not overwrite the browser clipboard again.
      clipboardState.lastText = value;
      clipboardState.lastDatatype = "text/plain";
      clipboardState.lastEventAt = Date.now();
      return true;
    } catch (error) {
      warn("clipboard send failed", error);
      return false;
    }
  }

  function clipboardText() {
    var client = state.client;
    return client && client.clipboard_buffer != null ? String(client.clipboard_buffer) : "";
  }

  function requestClipboard() {
    var client = state.client;
    if (!client || !client.connected) return false;
    try {
      if (typeof client._poll_clipboard === "function") client._poll_clipboard();
      return true;
    } catch (e) {
      return false;
    }
  }

  /**
   * Answer a deferred clipboard request (`clipboard-request` event). `text`
   * null declines and lets the Xpra client run its own fallback path.
   */
  function respondClipboardRequest(key, text) {
    if (key == null) return false;
    var entry = pendingClipboardRequests[String(key)];
    if (!entry) return false;
    if (text === null || typeof text === "undefined") return entry.finish(null, false);
    return entry.finish(String(text), true);
  }

  /* -------------------------------------------------------------- playback */

  /** Called synchronously from a trusted parent gesture. Microphone capture is
   * never requested: this only unlocks output from the user's private session. */
  function unlockAudio() {
    var client = state.client;
    if (!client) return false;
    try {
      if (client.audio_context && client.audio_context.state === "suspended") {
        client.audio_context.resume().catch(function () { /* next gesture can retry */ });
      }
      if (client.audio && client.audio.paused) {
        var playing = client.audio.play();
        if (playing && playing.catch) playing.catch(function () { /* autoplay blocked */ });
      } else if (client.connected && client.audio_enabled && client.audio_codec &&
          !client.audio && !client.audio_aurora_ctx && !client.__rastAudioStarting &&
          typeof client._sound_start_receiving === "function") {
        client.__rastAudioStarting = true;
        try { client._sound_start_receiving(); }
        finally { client.__rastAudioStarting = false; }
      }
      return true;
    } catch (error) {
      warn("could not unlock audio", error);
      return false;
    }
  }

  /* ------------------------------------------------------------ page commands */

  function reconnect() {
    try {
      window.location.reload();
      return true;
    } catch (e) {
      return false;
    }
  }

  function disconnect() {
    try {
      if (state.client) state.client.close();
      return true;
    } catch (error) {
      return false;
    }
  }

  function setScale(scale) {
    var value = Number(scale);
    if (!isFinite(value) || value <= 0) return false;
    try {
      if (state.client) {
        state.client.scale = value;
        var map = state.client.id_to_window || {};
        for (var wid in map) {
          if (map[wid]) map[wid].scale = value;
        }
      }
      return true;
    } catch (e) {
      return false;
    }
  }

  /* ------------------------------------------------------------- public API */

  var api = {
    version: VERSION,
    key: sessionKey(),
    get client() {
      return state.client;
    },
    get ready() {
      return state.ready;
    },
    get state() {
      return snapshot();
    },
    windows: function () {
      return snapshot().windows;
    },
    snapshot: snapshot,
    focusWindow: focusWindow,
    closeWindow: closeWindow,
    resizeWindow: resizeWindow,
    adoptWindow: adoptWindow,
    detachWindow: detachWindow,
    setMinimized: setMinimized,
    sendKey: sendKey,
    setKeyboardCapture: setKeyboardCapture,
    requestRefresh: requestRefresh,
    unlockAudio: unlockAudio,
    sendClipboardText: sendClipboardText,
    clipboardText: clipboardText,
    requestClipboard: requestClipboard,
    respondClipboardRequest: respondClipboardRequest,
    releaseInput: releaseInput,
    reconnect: reconnect,
    disconnect: disconnect,
    setScale: setScale,
    on: on,
    off: off,
    emit: emit,
  };

  try {
    window.RASTNative = api;
    window.__RAST_XPRA_SHIM = api;
    window.__RAST_SHIM_VERSION = VERSION;
  } catch (error) {
    warn("could not publish the RAST bridge", error);
  }

  log("loaded v" + VERSION + " (session key " + api.key + ")");
  /* A capture may have happened synchronously above; publish the first state
   * snapshot so a parent that attaches later still sees windows. */
  if (state.ready) scheduleNotify();
})();
