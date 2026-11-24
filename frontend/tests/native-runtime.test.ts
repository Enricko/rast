import assert from "node:assert/strict";
import test from "node:test";

import {
  nativeRuntimeSummary,
  type NativeRuntimeReport,
} from "../src/apps/linuxapp/catalog";

function report(patch: Partial<NativeRuntimeReport> = {}): NativeRuntimeReport {
  return {
    profile: "balanced",
    quality: null,
    minQuality: null,
    speed: null,
    minSpeed: null,
    videoScaling: null,
    avSync: null,
    encoding: null,
    display: {
      xvfb: true,
      xvfbPath: "/usr/bin/Xvfb",
      customConfigured: false,
      customUsable: false,
      active: "xvfb",
      customCommand: null,
      nvidiaXorgDriver: false,
      xorg: false,
      gpuDisplayConfigured: false,
    },
    gpu: {
      vendor: "nvidia",
      model: "NVIDIA GeForce GTX 1660 Ti",
      driverVersion: "595.99.02",
      nvidiaDeviceNodes: true,
      nvidiaUvm: true,
      nvidiaSmi: true,
      nvencLibrary: false,
      nvdecLibrary: false,
      xpraNvidiaCodecs: false,
      mesaEgl: true,
      renderNodes: ["renderD128"],
    },
    gpuAccelerationVerified: false,
    notes: [],
    ...patch,
  };
}

const value = (rows: Array<{ label: string; value: string }>, label: string): string | undefined =>
  rows.find((row) => row.label === label)?.value;

test("no runtime report renders no rows", () => {
  assert.deepEqual(nativeRuntimeSummary(null), []);
  assert.deepEqual(nativeRuntimeSummary(undefined), []);
});

test("a software runtime never claims GPU acceleration", () => {
  const rows = nativeRuntimeSummary(report());
  assert.equal(value(rows, "Runtime profile"), "balanced");
  assert.match(value(rows, "Display backend") ?? "", /Xvfb \(software rendering\)/);
  assert.match(value(rows, "Stream encoding") ?? "", /software/);
  assert.match(value(rows, "GPU acceleration") ?? "", /not verified/);
  assert.equal(value(rows, "GPU"), "NVIDIA GeForce GTX 1660 Ti (driver 595.99.02)");
});

test("a configured GPU backend with NVENC and a verified test is reported", () => {
  const rows = nativeRuntimeSummary(
    report({
      encoding: null,
      display: {
        xvfb: true,
        xvfbPath: "/usr/bin/Xvfb",
        customConfigured: true,
        customUsable: true,
        active: "custom",
        customCommand: "/usr/local/sbin/rast-xorg-nvidia",
        nvidiaXorgDriver: true,
        xorg: true,
        gpuDisplayConfigured: true,
      },
      gpu: {
        ...report().gpu,
        nvencLibrary: true,
        nvdecLibrary: true,
        xpraNvidiaCodecs: true,
      },
      gpuAccelerationVerified: true,
    }),
  );
  assert.match(value(rows, "Display backend") ?? "", /custom GPU X server/);
  assert.match(value(rows, "Stream encoding") ?? "", /NVENC/);
  assert.match(value(rows, "GPU acceleration") ?? "", /verified/);
});

test("a forced encoding and at most two notes are shown", () => {
  const rows = nativeRuntimeSummary(
    report({ encoding: "webp", notes: ["first", "second", "third"] }),
  );
  assert.match(value(rows, "Stream encoding") ?? "", /forced webp/);
  const notes = rows.filter((row) => row.label === "Runtime note");
  assert.equal(notes.length, 2);
  assert.equal(notes[0].value, "first");
});
