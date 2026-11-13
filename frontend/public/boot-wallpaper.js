// Paint the account's last-used wallpaper on the boot splash before the app
// bundle loads. CSP forbids inline scripts, so this tiny file is loaded as a
// normal same-origin script. Best effort: every step is optional.
(() => {
  try {
    const raw = window.localStorage.getItem("rast.wallpaper.splash.v1");
    if (!raw) return;
    const saved = JSON.parse(raw);
    if (!saved || typeof saved.url !== "string" || !saved.url) return;
    const splash = document.getElementById("boot-splash");
    if (!splash) return;
    const overlay = Math.max(0, Math.min(60, Number(saved.overlay) || 0)) / 100;
    const size = saved.fit === "contain" ? "contain" : saved.fit === "auto" ? "auto" : "cover";
    const position = typeof saved.position === "string" && saved.position ? saved.position : "center";
    splash.style.backgroundImage = `linear-gradient(rgba(12,18,26,${overlay}),rgba(12,18,26,${overlay})),url("${saved.url}")`;
    splash.style.backgroundSize = `auto,${size}`;
    splash.style.backgroundPosition = `center,${position}`;
    splash.style.backgroundRepeat = "no-repeat,no-repeat";
    if (typeof saved.color === "string" && /^#[0-9a-fA-F]{6}$/.test(saved.color)) {
      splash.style.backgroundColor = saved.color;
    }
  } catch {
    /* keep the built-in splash */
  }
})();
