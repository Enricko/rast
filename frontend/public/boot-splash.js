// Loading screen wallpaper: show the account's own wallpaper while the app
// loads, instead of the default one (the sign-in screen uses the same values).
// Runs before <body> is parsed (a classic, blocking script: CSP allows only
// same-origin scripts, no inline ones). The app keeps `rast.splash.v1` up to
// date (core/appearance.ts, writeSplashWallpaper); it stays after sign-out,
// like a lock screen. An uploaded wallpaper is stored as a reduced JPEG copy
// (data: URL) because the upload itself is served to its owner only. Every
// value is checked here so a tampered entry cannot inject CSS.
(function () {
  try {
    var raw = window.localStorage.getItem("rast.splash.v1");
    if (!raw) return;
    var saved = JSON.parse(raw);
    if (!saved || typeof saved !== "object") return;
    var image =
      typeof saved.image === "string" &&
      (/^\/(wallpapers\/[A-Za-z0-9._-]+|api\/appearance\/wallpapers\/[A-Za-z0-9._%-]+)$/.test(saved.image) ||
        /^data:image\/jpeg;base64,[A-Za-z0-9+\/]+={0,2}$/.test(saved.image))
        ? saved.image
        : "";
    var color = typeof saved.color === "string" && /^#[0-9a-fA-F]{6}$/.test(saved.color) ? saved.color : "#10161d";
    var size =
      saved.size === "contain" ||
      saved.size === "auto" ||
      (typeof saved.size === "string" && /^[0-9]{1,5}px [0-9]{1,5}px$/.test(saved.size))
        ? saved.size
        : "cover";
    var position =
      typeof saved.position === "string" && /^(center|top|bottom|left|right)( (top|bottom))?$/.test(saved.position)
        ? saved.position
        : "center";
    var root = document.documentElement;
    root.style.setProperty("--boot-wallpaper-image", image ? 'url("' + image + '")' : "none");
    root.style.setProperty("--boot-wallpaper-color", color);
    root.style.setProperty("--boot-wallpaper-size", size);
    root.style.setProperty("--boot-wallpaper-position", position);
    root.setAttribute("data-boot-wallpaper", "");
  } catch (error) {
    /* storage unavailable: the default loading screen stays */
  }
})();
