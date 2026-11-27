// The window SDK RAST lends Package Center apps (`app.window.sdk` in their
// manifest). An app's window module is built against these modules (its
// `../../core/<name>` imports become `globalThis.__RAST_SDK__.<name>`), so it
// uses RAST's own helpers: one live-event connection, one dialog layer, one
// look. Version 1 keeps every export listed in `sdk-v1.json`
// (tests/sdk.test.ts); a later version may add, never remove or change.

import * as api from "./api";
import * as components from "./components";
import * as dom from "./dom";
import * as format from "./format";
import * as icons from "./icons";
import * as realtime from "./realtime";
import * as ui from "./ui";

export const SDK_VERSION = 1;

export const SDK_MODULES = { api, components, dom, format, icons, realtime, ui } as const;

export type Sdk = typeof SDK_MODULES & { version: number };

let installed: Sdk | null = null;

/** The SDK, also published as `globalThis.__RAST_SDK__` for app modules. */
export function sdk(): Sdk {
  if (!installed) {
    installed = Object.freeze({ ...SDK_MODULES, version: SDK_VERSION });
    (globalThis as { __RAST_SDK__?: Sdk }).__RAST_SDK__ = installed;
  }
  return installed;
}
