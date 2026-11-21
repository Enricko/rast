// The sign-in is a real browser form backed by the RAST server. The server
// sets an HttpOnly session cookie, so this script never holds the session; it
// only learns who signed in and hands that to Rune. Rune creates no canvas and
// installs no viewport handlers until then.
const $ = (id) => document.getElementById(id);
const screen = $('login-screen');
const form = $('login-form');
const fields = $('login-fields');
const username = $('username');
const password = $('password');
const confirm = $('confirm');
const token = $('setup-token');
const submit = $('submit-login');
const reveal = $('password-toggle');
const theme = $('theme-toggle');
const note = $('login-note');
const status = $('status');
const error = $('login-error');
const cover = $('workspace-cover');
const startup = $('startup-cover');

const LAST_USER = 'rast.last-user';
const UNREACHABLE = "The RAST server can't be reached. Check your connection and try again.";

let wasm;
let mode = 'login';
let opening = false;
// The password just used to sign in, while its replacement is chosen.
let current = '';

// Every call carries X-RAST: the server refuses changes without it, and
// browsers only send it cross-site after a preflight the server never allows.
async function api(path, body) {
  const init = { method: body === undefined ? 'GET' : 'POST', headers: { 'x-rast': '1' }, credentials: 'same-origin', cache: 'no-store' };
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch {
    throw UNREACHABLE;
  }
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = null; }
  if (response.ok) return data;
  if (data && typeof data.error === 'string') throw data.error;
  if (response.status === 404 || (response.status >= 502 && response.status <= 504)) {
    throw 'The RAST server is not responding. Try again in a moment.';
  }
  throw `The RAST server answered ${response.status}.`;
}

function remembered() {
  try { return localStorage.getItem(LAST_USER) ?? ''; } catch { return ''; }
}

function remember(name) {
  try { localStorage.setItem(LAST_USER, name); } catch { /* private mode */ }
}

function setRevealed(show) {
  password.type = confirm.type = show ? 'text' : 'password';
  reveal.textContent = show ? 'Hide' : 'Show';
  reveal.setAttribute('aria-label', show ? 'Hide password' : 'Show password');
}

// "setup": no accounts yet, create the first Super Admin with the token.
// "login": sign in; accounts are made by administrators, never here.
// "change": signed in with a password an administrator set; choose your own
// before the workspace opens. The username stays in the (hidden) field so
// password managers save the new password for the right account.
function setMode(next) {
  mode = next;
  const setup = mode === 'setup';
  const change = mode === 'change';
  const choosing = setup || change;
  $('login-title').textContent = setup ? 'Set up RAST' : change ? 'Choose a new password' : 'Welcome back';
  $('login-subtitle').textContent = setup
    ? 'Create the first Super Admin for this server.'
    : change
      ? 'An administrator set your password. Choose your own to continue.'
      : 'Sign in to your server workspace.';
  $('token-row').hidden = !setup;
  token.required = setup;
  $('username-row').hidden = change;
  username.required = !change;
  $('password-label').textContent = change ? 'New password' : 'Password';
  $('confirm-row').hidden = !choosing;
  confirm.required = choosing;
  $('confirm-label').textContent = change ? 'Confirm new password' : 'Confirm password';
  password.autocomplete = choosing ? 'new-password' : 'current-password';
  password.minLength = choosing ? 8 : 0;
  password.placeholder = choosing ? 'At least 8 characters' : 'password';
  username.placeholder = setup ? 'e.g. admin' : 'username';
  if (!change) username.value = setup ? '' : remembered();
  if (!change) current = '';
  password.value = confirm.value = token.value = '';
  setRevealed(false);
  submit.textContent = setup ? 'Create Super Admin' : change ? 'Save and continue' : 'Sign in';
  note.textContent = setup
    ? 'The setup token is on the server: sudo cat /var/lib/rast-revamp/setup-token'
    : change
      ? `Signed in as ${username.value}. Your other sessions will be signed out.`
      : 'No public sign-up. Ask an administrator for an account.';
  $('change-cancel').hidden = !change;
  error.textContent = '';
  status.hidden = true;
  fields.disabled = false;
}

function reportStartupError(cause) {
  fields.disabled = true;
  status.hidden = false;
  status.textContent = cause instanceof WebAssembly.LinkError
    ? 'RAST could not load this release. Reload the page and try again.'
    : message(cause, 'RAST could not start. Reload the page and try again.');
}

function message(cause, fallback) {
  return typeof cause === 'string' ? cause : cause instanceof Error ? cause.message : fallback;
}

// The starter returns before Rune presents anything. Keep a plain browser
// cover up until the first frame; remove it even when the GPU cannot start.
function startWorkspace(start) {
  return new Promise((resolve, reject) => {
    let timer;
    const clean = () => {
      document.removeEventListener('runepresent', presented);
      clearTimeout(timer);
    };
    const presented = (event) => {
      clean();
      resolve(event.target);
    };
    const began = performance.now();
    function check() {
      let failure;
      try {
        failure = wasm.workspace_error();
      } catch (cause) {
        clean();
        reject(cause);
        return;
      }
      if (failure || performance.now() - began >= 15000) {
        clean();
        reject(failure || 'The workspace did not start. Please try again.');
      } else {
        timer = setTimeout(check, 200);
      }
    }
    document.addEventListener('runepresent', presented, { once: true });
    try {
      start();
      timer = setTimeout(check, 200);
    } catch (cause) {
      clean();
      reject(cause);
    }
  });
}

// Open the desktop for `user`. If it cannot start, end the server session so
// the next attempt starts clean, and leave the error for the caller.
async function openWorkspace(user) {
  let started = false;
  try {
    const canvas = await startWorkspace(() => {
      // The HTML sign-in already supplies the fast first paint. Avoid Rune's
      // temporary CPU canvas and a second compositor handover.
      globalThis.RUNE_CPU_FIRST = 'off';
      wasm.open_workspace(JSON.stringify(user));
      started = true;
      cover.hidden = false;
      screen.hidden = true;
    });
    status.hidden = true;
    cover.hidden = true;
    canvas.focus({ preventScroll: true });
  } catch (cause) {
    if (started) {
      try { wasm.close_workspace(); } catch (closeError) { console.error(closeError); }
    }
    try { await api('/api/auth/logout', {}); } catch { /* already signed out */ }
    cover.hidden = true;
    screen.hidden = false;
    throw cause;
  }
}

function focusLogin() {
  const input = mode === 'setup' ? token : mode === 'change' || username.value ? password : username;
  input.focus({ preventScroll: true });
}

// Back to the sign-in form, ending the session that still owes a new password.
async function leaveChange(reason) {
  fields.disabled = true;
  try { await api('/api/auth/logout', {}); } catch { /* already signed out */ }
  setMode('login');
  error.textContent = reason;
  focusLogin();
}

// Replace the password an administrator set, then open the workspace.
async function choosePassword() {
  if (password.value !== confirm.value) {
    error.textContent = 'The passwords do not match.';
    confirm.focus();
    return;
  }
  opening = true;
  fields.disabled = true;
  error.textContent = '';
  status.textContent = 'Saving your new password…';
  status.hidden = false;
  try {
    try {
      await api('/api/auth/password', { current, new: password.value });
    } finally {
      password.value = confirm.value = '';
      setRevealed(false);
    }
    current = '';
    const state = await api('/api/auth/state');
    if (!state.user) throw 'Your session ended. Sign in with your new password.';
    status.textContent = 'Opening your workspace…';
    await openWorkspace(state.user);
  } catch (cause) {
    status.hidden = true;
    fields.disabled = false;
    error.textContent = message(cause, 'RAST could not save your new password.');
    // A session that ended meanwhile can't finish the change.
    try {
      const state = await api('/api/auth/state');
      if (!state.user) {
        const shown = error.textContent;
        setMode('login');
        error.textContent = shown;
      } else if (!state.user.mustChangePassword) {
        await openWorkspace(state.user);
        return;
      }
    } catch { /* keep the form as it is */ }
    focusLogin();
  } finally {
    opening = false;
  }
}

reveal.addEventListener('click', () => setRevealed(password.type === 'password'));
theme.addEventListener('click', () => {
  const light = screen.dataset.theme !== 'light';
  screen.dataset.theme = light ? 'light' : 'dark';
  theme.setAttribute('aria-label', light ? 'Switch to dark mode' : 'Switch to light mode');
});

$('change-cancel').addEventListener('click', () => {
  if (!opening) leaveChange('');
});

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!wasm || fields.disabled || opening) return;
  if (mode === 'change') {
    await choosePassword();
    return;
  }
  const setup = mode === 'setup';
  if (setup && password.value !== confirm.value) {
    error.textContent = 'The passwords do not match.';
    confirm.focus();
    return;
  }
  error.textContent = '';
  // Check before signing in on a device that cannot open the workspace.
  if (!navigator.gpu) {
    error.textContent = 'The RAST workspace needs a browser with WebGPU enabled.';
    return;
  }
  opening = true;
  fields.disabled = true;
  status.textContent = setup ? 'Creating the Super Admin…' : 'Signing in…';
  status.hidden = false;
  const name = username.value.trim().toLowerCase();
  const typed = password.value;
  try {
    let reply;
    try {
      reply = setup
        ? await api('/api/auth/setup', { username: name, password: typed, token: token.value.trim() })
        : await api('/api/auth/login', { username: name, password: typed });
    } finally {
      password.value = confirm.value = '';
      setRevealed(false);
    }
    token.value = '';
    remember(reply.user.username);
    if (reply.user.mustChangePassword) {
      username.value = reply.user.username;
      current = typed;
      setMode('change');
      focusLogin();
      return;
    }
    status.textContent = 'Opening your workspace…';
    await openWorkspace(reply.user);
  } catch (cause) {
    status.hidden = true;
    fields.disabled = false;
    error.textContent = message(cause, 'RAST could not open the workspace.');
    // Someone finished setup meanwhile: switch to sign-in.
    if (setup) {
      try {
        const state = await api('/api/auth/state');
        if (!state.needsSetup) {
          const shown = error.textContent;
          setMode('login');
          error.textContent = shown;
        }
      } catch { /* keep the form as it is */ }
    }
    focusLogin();
  } finally {
    opening = false;
  }
});

window.addEventListener('rastsignout', async () => {
  const reason = globalThis.rastSignOutReason || '';
  globalThis.rastSignOutReason = '';
  cover.hidden = true;
  screen.hidden = false;
  // Wait for the server to end the session: a sign-in racing this call could
  // otherwise have its new cookie cleared by the late logout reply.
  fields.disabled = true;
  status.textContent = 'Signing out…';
  status.hidden = false;
  try { await api('/api/auth/logout', {}); } catch { /* the session is gone already */ }
  setMode('login');
  error.textContent = reason;
  focusLogin();
});

async function loadBindings() {
  const bindings = await import('./app.js');
  await bindings.default();
  return bindings;
}

let startupTimer;
let session = null;
try {
  wasm = await Promise.race([
    loadBindings(),
    new Promise((_, reject) => {
      startupTimer = setTimeout(() => reject('RAST took too long to load. Reload the page and try again.'), 30000);
    }),
  ]);
  try {
    const state = await api('/api/auth/state');
    setMode(state.needsSetup ? 'setup' : 'login');
    session = state.user;
  } catch (cause) {
    setMode('login');
    error.textContent = message(cause, UNREACHABLE);
  }
} catch (cause) {
  reportStartupError(cause);
} finally {
  clearTimeout(startupTimer);
}

// Still signed in from before (the cookie is HttpOnly; the server says who).
// A session that still owes a new password can't finish here: the password it
// signed in with is gone, so sign in again.
if (session && session.mustChangePassword) {
  screen.hidden = false;
  startup.hidden = true;
  await leaveChange('Sign in again to choose your new password.');
} else if (session && navigator.gpu) {
  startup.hidden = true;
  cover.hidden = false;
  opening = true;
  fields.disabled = true;
  try {
    await openWorkspace(session);
  } catch (cause) {
    fields.disabled = false;
    error.textContent = message(cause, 'RAST could not open the workspace.');
  } finally {
    opening = false;
  }
} else {
  if (session) error.textContent = 'The RAST workspace needs a browser with WebGPU enabled.';
  screen.hidden = false;
  startup.hidden = true;
}
