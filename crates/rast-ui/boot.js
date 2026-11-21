// RAST page bootstrap. Kept as a separate file (not inline) so the deployed page
// works under RAST's strict CSP: script-src 'self' 'wasm-unsafe-eval'.
import init from './app.js';

const status = document.querySelector('#status');
try {
  if (!navigator.gpu) {
    throw new Error(
      'RAST needs a browser with WebGPU (Chrome/Edge 113+, Safari 26+, Firefox with WebGPU enabled) over HTTPS.',
    );
  }
  await init();
  status.remove();
} catch (error) {
  status.textContent = String(error);
  console.error(error);
}
