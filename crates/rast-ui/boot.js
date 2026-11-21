// RAST page bootstrap. Kept as a separate file (not inline) so the deployed page
// works under RAST's strict CSP: script-src 'self' 'wasm-unsafe-eval'.

const status = document.querySelector('#status');
try {
  if (!navigator.gpu) {
    throw new Error(
      'RAST needs a browser with WebGPU (Chrome/Edge 113+, Safari 26+, Firefox with WebGPU enabled) over HTTPS.',
    );
  }
  const { default: init } = await import('./app.js');
  await init();
  status.remove();
} catch (error) {
  status.textContent = error instanceof WebAssembly.LinkError
    ? 'RAST could not load this release. Reload this page to try again. Your local login is unchanged.'
    : error instanceof Error ? error.message : 'RAST could not start. Reload this page to try again.';
  console.error(error);
}
