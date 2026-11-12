(() => {
  // RAST: this page runs in a sandboxed frame without an origin (it cannot
  // reach RAST's cookies or pages). Give it what such a frame lacks.
  const memory = () => {
    const data = new Map();
    return {
      get length() { return data.size; },
      key: (i) => Array.from(data.keys())[i] ?? null,
      getItem: (k) => (data.has(String(k)) ? data.get(String(k)) : null),
      setItem: (k, v) => { data.set(String(k), String(v)); },
      removeItem: (k) => { data.delete(String(k)); },
      clear: () => { data.clear(); },
    };
  };
  for (const name of ['localStorage', 'sessionStorage']) {
    try { window[name].length; } catch {
      try { Object.defineProperty(window, name, { value: memory(), configurable: true }); } catch {}
    }
  }
  try { document.cookie; } catch {
    try { Object.defineProperty(document, 'cookie', { get: () => '', set: () => {}, configurable: true }); } catch {}
  }
  // Messages this window posts to itself: address them to '*' and show the
  // page's own URL origin to its listeners. Other senders are untouched.
  const own = location.origin;
  const post = window.postMessage.bind(window);
  window.postMessage = (message, target, transfer) =>
    post(message, target === own ? '*' : target, transfer);
  const add = window.addEventListener.bind(window);
  const remove = window.removeEventListener.bind(window);
  const wrapped = new WeakMap();
  window.addEventListener = (type, listener, options) => {
    if (type !== 'message' || typeof listener !== 'function') return add(type, listener, options);
    let w = wrapped.get(listener);
    if (!w) {
      w = (event) => listener.call(window, event.source === window && event.origin === 'null'
        ? new Proxy(event, { get: (t, p) => (p === 'origin' ? own : (typeof t[p] === 'function' ? t[p].bind(t) : t[p])) })
        : event);
      wrapped.set(listener, w);
    }
    return add(type, w, options);
  };
  window.removeEventListener = (type, listener, options) =>
    remove(type, (type === 'message' && wrapped.get(listener)) || listener, options);
  const style = document.createElement('style');
  style.textContent = '.toggle-handle{display:none!important}';
  document.documentElement.appendChild(style);
})();
