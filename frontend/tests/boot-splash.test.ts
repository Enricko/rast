// The loading screen shows the signed-in account's wallpaper: the app stores
// it (splashWallpaper) and public/boot-splash.js applies it before the app
// loads, accepting only safe values.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";
import { runInNewContext } from "node:vm";

import { DEFAULT_APPEARANCE, nextSplash, normalizeAppearance, splashWallpaper } from "../src/core/appearance";

const script = readFileSync(join(process.cwd(), "public", "boot-splash.js"), "utf8");

/** Run boot-splash.js against a stored value; returns the CSS it set. */
function boot(stored: string | null): { props: Record<string, string>; flagged: boolean } {
  const props: Record<string, string> = {};
  let flagged = false;
  const window = {
    localStorage: {
      getItem: (key: string) => (key === "rast.splash.v1" ? stored : null),
    },
  };
  const document = {
    documentElement: {
      style: { setProperty: (name: string, value: string) => (props[name] = value) },
      setAttribute: (name: string) => {
        if (name === "data-boot-wallpaper") flagged = true;
      },
    },
  };
  runInNewContext(script, { window, document });
  return { props, flagged };
}

test("the stored loading-screen wallpaper follows the account's wallpaper", () => {
  const builtin = normalizeAppearance({
    ...DEFAULT_APPEARANCE,
    wallpaper: { ...DEFAULT_APPEARANCE.wallpaper, kind: "builtin", builtinId: "forest-mist", fit: "contain", position: "top-left" },
  });
  assert.deepEqual(splashWallpaper(builtin), {
    image: "/wallpapers/forest-mist.jpg",
    color: builtin.wallpaper.color,
    size: "contain",
    position: "left top",
  });
  const upload = normalizeAppearance({
    ...DEFAULT_APPEARANCE,
    wallpaper: { ...DEFAULT_APPEARANCE.wallpaper, kind: "upload", uploadId: "3f2a9c" },
  });
  assert.equal(splashWallpaper(upload).image, "/api/appearance/wallpapers/3f2a9c");
  const color = normalizeAppearance({
    ...DEFAULT_APPEARANCE,
    wallpaper: { ...DEFAULT_APPEARANCE.wallpaper, kind: "color", color: "#224466" },
  });
  assert.deepEqual(splashWallpaper(color), { image: null, color: "#224466", size: "cover", position: "center" });
});

test("boot-splash.js paints the stored wallpaper before the app loads", () => {
  const stored = { image: "/api/appearance/wallpapers/3f2a9c", color: "#224466", size: "contain", position: "left top" };
  const { props, flagged } = boot(JSON.stringify(stored));
  assert.equal(flagged, true);
  assert.deepEqual(props, {
    "--boot-wallpaper-image": 'url("/api/appearance/wallpapers/3f2a9c")',
    "--boot-wallpaper-color": "#224466",
    "--boot-wallpaper-size": "contain",
    "--boot-wallpaper-position": "left top",
  });
  const plain = boot(JSON.stringify({ image: null, color: "#224466", size: "cover", position: "center" }));
  assert.equal(plain.props["--boot-wallpaper-image"], "none");
});

test("an uploaded wallpaper keeps its reduced copy until the upload changes", () => {
  const upload = (uploadId: string, fit: "cover" | "center" = "cover") =>
    normalizeAppearance({ ...DEFAULT_APPEARANCE, wallpaper: { ...DEFAULT_APPEARANCE.wallpaper, kind: "upload", uploadId, fit } });
  // No copy yet: the upload's address (works while signed in) + which upload.
  const first = nextSplash(upload("aa11"), null);
  assert.equal(first.image, "/api/appearance/wallpapers/aa11");
  assert.equal(first.uploadId, "aa11");
  // Once the copy exists it is kept on every later load of the same upload…
  const copy = { ...first, image: "data:image/jpeg;base64,QUJD", uploadSize: "3840px 2160px" };
  assert.equal(nextSplash(upload("aa11"), copy).image, copy.image);
  // …the "center" fit shows it at the upload's own size…
  assert.equal(nextSplash(upload("aa11", "center"), copy).size, "3840px 2160px");
  assert.equal(nextSplash(upload("aa11"), copy).size, "cover");
  // …and another upload or a built-in wallpaper drops it.
  assert.equal(nextSplash(upload("bb22"), copy).image, "/api/appearance/wallpapers/bb22");
  const builtin = nextSplash(normalizeAppearance(DEFAULT_APPEARANCE), copy);
  assert.equal(builtin.image, "/wallpapers/alpine-dawn.jpg");
  assert.equal(builtin.uploadId, undefined);
});

test("boot-splash.js accepts the reduced copy and its size", () => {
  const { props } = boot(JSON.stringify({ image: "data:image/jpeg;base64,QUJD+/9=", color: "#224466", size: "3840px 2160px", position: "center" }));
  assert.equal(props["--boot-wallpaper-image"], 'url("data:image/jpeg;base64,QUJD+/9=")');
  assert.equal(props["--boot-wallpaper-size"], "3840px 2160px");
  const svg = boot(JSON.stringify({ image: 'data:image/svg+xml;base64,QUJD") ; x', size: "1px 1px; color: red" }));
  assert.equal(svg.props["--boot-wallpaper-image"], "none");
  assert.equal(svg.props["--boot-wallpaper-size"], "cover");
});

test("boot-splash.js keeps the default without a stored value and rejects unsafe ones", () => {
  assert.deepEqual(boot(null), { props: {}, flagged: false });
  assert.deepEqual(boot("{not json"), { props: {}, flagged: false });
  const hostile = boot(
    JSON.stringify({
      image: 'https://evil.example/x.jpg") ; background: red',
      color: "red; content: 'x'",
      size: "999px",
      position: "center; color: red",
    }),
  );
  assert.deepEqual(hostile.props, {
    "--boot-wallpaper-image": "none",
    "--boot-wallpaper-color": "#10161d",
    "--boot-wallpaper-size": "cover",
    "--boot-wallpaper-position": "center",
  });
});
