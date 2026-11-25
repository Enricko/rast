import { test } from "node:test";
import assert from "node:assert/strict";
import {
  computeGeometry,
  defaultScaleFor,
  formatScale,
  geometryChanged,
  normalizeScale,
  parseStoredDisplay,
  scaleOptionsFor,
  screenKey,
} from "../src/core/display-math";

test("the desktop keeps its layout when the browser zooms", () => {
  // 1920 device-pixel-wide window on a 1x monitor, scale 100 %.
  const at100 = computeGeometry({ cssWidth: 1920, cssHeight: 960, devicePixelRatio: 1 }, 1);
  // Same window at 150 % and 50 % browser zoom.
  const at150 = computeGeometry({ cssWidth: 1280, cssHeight: 640, devicePixelRatio: 1.5 }, 1);
  const at50 = computeGeometry({ cssWidth: 3840, cssHeight: 1920, devicePixelRatio: 0.5 }, 1);
  for (const g of [at100, at150, at50]) {
    assert.equal(g.width, 1920);
    assert.equal(g.height, 960);
    assert.equal(g.resolutionWidth, 1920);
    assert.equal(g.resolutionHeight, 960);
  }
  assert.equal(at100.factor, 1);
  assert.equal(at150.factor, 1 / 1.5);
  assert.equal(at50.factor, 2);
  // Filling the window: layout size × factor is the viewport.
  assert.equal(at150.width * at150.factor, 1280);
});

test("scale works like an OS display scale", () => {
  const g = computeGeometry({ cssWidth: 1920, cssHeight: 1080, devicePixelRatio: 1 }, 1.25);
  assert.equal(g.width, 1536);
  assert.equal(g.height, 864);
  assert.equal(g.factor, 1.25);
  // Retina at its native 200 %: untransformed, exactly what the page showed before.
  const retina = computeGeometry({ cssWidth: 1512, cssHeight: 860, devicePixelRatio: 2 }, 2);
  assert.equal(retina.factor, 1);
  assert.equal(retina.width, 1512);
  assert.equal(retina.resolutionWidth, 3024);
});

test("the desktop follows a window resize", () => {
  const wide = computeGeometry({ cssWidth: 1600, cssHeight: 900, devicePixelRatio: 1 }, 1);
  const narrow = computeGeometry({ cssWidth: 1000, cssHeight: 900, devicePixelRatio: 1 }, 1);
  assert.equal(wide.width, 1600);
  assert.equal(narrow.width, 1000);
  assert.ok(geometryChanged(wide, narrow));
});

test("near-identical factors are not resampled and sub-pixel noise is not a change", () => {
  const g = computeGeometry({ cssWidth: 1000, cssHeight: 800, devicePixelRatio: 1.0001 }, 1);
  assert.equal(g.factor, 1);
  const a = computeGeometry({ cssWidth: 1280, cssHeight: 640, devicePixelRatio: 1.5 }, 1);
  const b = computeGeometry({ cssWidth: 1280.2, cssHeight: 640, devicePixelRatio: 1.5 }, 1);
  assert.equal(geometryChanged(a, { ...b, resolutionWidth: a.resolutionWidth }), false);
});

test("bad metrics fall back to safe values", () => {
  const g = computeGeometry({ cssWidth: Number.NaN, cssHeight: 0, devicePixelRatio: -1 }, Number.NaN);
  assert.equal(g.factor, 1);
  assert.equal(g.scale, 1);
  assert.ok(g.width > 0 && g.height > 0);
});

test("scales are clamped and rounded to whole percent", () => {
  assert.equal(normalizeScale(1.254), 1.25);
  assert.equal(normalizeScale(10), 4);
  assert.equal(normalizeScale(0.1), 0.5);
  assert.equal(normalizeScale(0), null);
  assert.equal(normalizeScale("2"), null);
  assert.equal(defaultScaleFor(2.625), 2.63);
  assert.equal(defaultScaleFor(Number.NaN), 1);
  assert.equal(formatScale(1.25), "125%");
});

test("screen keys ignore orientation", () => {
  assert.equal(screenKey(1080, 1920), "1920x1080");
  assert.equal(screenKey(1920, 1080), "1920x1080");
  assert.equal(screenKey(Number.NaN, 5), "5x0");
});

test("stored settings are validated", () => {
  assert.deepEqual(parseStoredDisplay(null).screens, {});
  assert.deepEqual(parseStoredDisplay("not json").screens, {});
  const parsed = parseStoredDisplay(
    JSON.stringify({
      version: 1,
      screens: {
        "1920x1080": { scale: 1.25, recommended: 1 },
        "2560x1600": { scale: 2 },
        "bad key": { scale: 1 },
        "1280x800": { scale: "big" },
      },
    }),
  );
  assert.deepEqual(parsed.screens, {
    "1920x1080": { scale: 1.25, recommended: 1 },
    "2560x1600": { scale: 2, recommended: 2 },
  });
});

test("the scale list includes the recommended and current scale", () => {
  const options = scaleOptionsFor({ scale: 1.1, recommended: 2.63 });
  assert.ok(options.includes(1.1));
  assert.ok(options.includes(2.63));
  assert.ok(options.includes(1));
  assert.deepEqual(options, [...options].sort((a, b) => a - b));
});
