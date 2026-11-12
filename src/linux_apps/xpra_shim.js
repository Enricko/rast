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

  var VERSION = "1.0.0";
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
  var lastClipboard = "";
  var notifyTimer = 0;
  var pollTimer = 0;
  var patchedClient = null;

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
    patchClient(client);
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
          constructor() {
            super.apply(null, arguments);
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
      title: win.title || metadata.title || "Window",
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
      captureKeyboard: !!(client && client.capture_keyboard),
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
    for (var i = 0; i < handlers.length; i++) patchHandler(client, handlers[i]);
    patchHandler(client, "_process_clipboard_token", function () {
      try {
        var text = client.clipboard_buffer == null ? "" : String(client.clipboard_buffer);
        if (text !== lastClipboard) {
          lastClipboard = text;
          emit("clipboard", { text: text });
        }
      } catch (e) {
        /* ignore */
      }
    });
    patchGetMouse(client);
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
        var content = win && win.__rastContentEl;
        if (content && event) {
          var nativeEvent = event.originalEvent || event;
          if (nativeEvent && typeof nativeEvent.clientX === "number") {
            var rect = content.getBoundingClientRect();
            var button = 0;
            if (typeof nativeEvent.which === "number") {
              button = Math.max(0, nativeEvent.which);
            } else if (typeof nativeEvent.button === "number") {
              button = Math.max(0, nativeEvent.button) + 1;
            }
            return {
              x: nativeEvent.clientX - rect.left + (content.scrollLeft || 0),
              y: nativeEvent.clientY - rect.top + (content.scrollTop || 0),
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
        if (win.__rastContentEl) {
          win.x = 0;
          win.y = 0;
        }
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
      win.set_minimized(next);
      if (next) {
        if (client.focused_wid === win.wid) {
          try {
            client.auto_focus();
          } catch (e) {
            /* ignore */
          }
        }
      } else {
        var geometry = win.get_internal_geometry();
        if (!win.__rastContentEl) geometry = { x: win.x, y: win.y, w: win.w, h: win.h };
        client.send([
          ROOT.PACKET_TYPES.map_window,
          win.wid,
          geometry.x || 0,
          geometry.y || 0,
          geometry.w || win.w,
          geometry.h || win.h,
          win.client_properties,
        ]);
        try {
          client.focused_wid = 0;
          client.set_focus(win);
        } catch (e) {
          /* ignore */
        }
        scheduleRefresh(wid);
      }
      if (!next) focusFrame(win);
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
      win.x = 0;
      win.y = 0;
      patchWindowForFill(win);
      if (win.div.parentNode !== container) {
        var adopted = doc.adoptNode(win.div);
        container.appendChild(adopted);
      }
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
      if (spinner) spinner.style.pointerEvents = "none";
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
    var value = text == null ? "" : String(text);
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
      client.send_clipboard_token(bytes);
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
    if (!client) return false;
    try {
      if (typeof client._poll_clipboard === "function") client._poll_clipboard();
      return true;
    } catch (e) {
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
    sendClipboardText: sendClipboardText,
    clipboardText: clipboardText,
    requestClipboard: requestClipboard,
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
