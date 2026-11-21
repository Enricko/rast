// The login is a real browser form. Fetch the local Argon2id verifier here, but
// don't let Rune create a canvas or install viewport handlers until unlock.
const $ = (id) => document.getElementById(id);
const screen = $('login-screen');
const form = $('login-form');
const fields = $('login-fields');
const username = $('username');
const password = $('password');
const confirm = $('confirm');
const submit = $('submit-login');
const reveal = $('password-toggle');
const theme = $('theme-toggle');
const status = $('status');
const error = $('login-error');
const cover = $('workspace-cover');
const startup = $('startup-cover');

let wasm;
let creating = false;
let opening = false;

function setRevealed(show) {
  password.type = confirm.type = show ? 'text' : 'password';
  reveal.textContent = show ? 'Hide' : 'Show';
  reveal.setAttribute('aria-label', show ? 'Hide password' : 'Show password');
}

function resetLogin() {
  const saved = wasm.device_username();
  creating = saved == null;
  $('login-title').textContent = creating ? 'Welcome to RAST' : 'Welcome back';
  $('login-subtitle').textContent = creating
    ? 'Set up a local login for this browser.' : 'Sign in to your server workspace.';
  $('confirm-row').hidden = !creating;
  confirm.required = creating;
  password.autocomplete = creating ? 'new-password' : 'current-password';
  password.minLength = creating ? 8 : 0;
  password.placeholder = creating ? 'At least 8 characters' : 'password';
  username.value = saved ?? '';
  password.value = confirm.value = '';
  setRevealed(false);
  submit.textContent = creating ? 'Create local login' : 'Sign in';
  error.textContent = '';
  status.hidden = true;
  fields.disabled = false;
}

function reportStartupError(cause) {
  fields.disabled = true;
  status.hidden = false;
  status.textContent = cause instanceof WebAssembly.LinkError
    ? 'RAST could not load this release. Reload the page and try again. Your local login is unchanged.'
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

function focusLogin() {
  const input = creating ? username : password;
  input.focus({ preventScroll: true });
}

reveal.addEventListener('click', () => setRevealed(password.type === 'password'));
theme.addEventListener('click', () => {
  const light = screen.dataset.theme !== 'light';
  screen.dataset.theme = light ? 'light' : 'dark';
  theme.setAttribute('aria-label', light ? 'Switch to dark mode' : 'Switch to light mode');
});

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!wasm || fields.disabled || opening) return;
  if (creating && password.value !== confirm.value) {
    error.textContent = 'The passwords do not match.';
    confirm.focus();
    return;
  }
  error.textContent = '';
  // Check before creating an account on a device that cannot open the workspace.
  if (!navigator.gpu) {
    error.textContent = 'The RAST workspace needs a browser with WebGPU enabled.';
    return;
  }
  opening = true;
  fields.disabled = true;
  status.textContent = 'Checking local login…';
  status.hidden = false;
  let started = false;
  try {
    // Give the browser a chance to show feedback before local password hashing.
    await new Promise((resolve) => requestAnimationFrame(() => setTimeout(resolve, 0)));
    const canvas = await startWorkspace(() => {
      // The HTML login already supplies the fast first paint. Avoid Rune's
      // temporary CPU canvas and a second compositor handover on unlock.
      globalThis.RUNE_CPU_FIRST = 'off';
      try {
        wasm.unlock_workspace(username.value, password.value, creating);
        started = true;
      } finally {
        password.value = confirm.value = '';
        setRevealed(false);
      }
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
    cover.hidden = true;
    screen.hidden = false;
    status.hidden = true;
    fields.disabled = false;
    // Storage may now contain the just-created account if workspace startup failed.
    if (started) {
      try { resetLogin(); } catch (readError) { reportStartupError(readError); }
    }
    error.textContent = message(cause, 'RAST could not open the workspace.');
    focusLogin();
  } finally {
    opening = false;
  }
});

window.addEventListener('rastsignout', () => {
  cover.hidden = true;
  screen.hidden = false;
  try {
    resetLogin();
    focusLogin();
  } catch (cause) {
    reportStartupError(cause);
  }
});

async function loadBindings() {
  const bindings = await import('./app.js');
  await bindings.default();
  return bindings;
}

let startupTimer;
try {
  wasm = await Promise.race([
    loadBindings(),
    new Promise((_, reject) => {
      startupTimer = setTimeout(() => reject('RAST took too long to load. Reload the page and try again.'), 30000);
    }),
  ]);
  // A full reload discards the in-memory unlock. Never restore it from a
  // browser storage flag: the local password verifier must still be checked.
  resetLogin();
} catch (cause) {
  reportStartupError(cause);
} finally {
  clearTimeout(startupTimer);
  screen.hidden = false;
  startup.hidden = true;
}
