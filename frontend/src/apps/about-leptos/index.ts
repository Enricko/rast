// About RAST (Leptos): the first window whose body is rendered by the
// Rust/WASM UI (crates/rast-ui). The WASM module is loaded on first open.

import type { AppContext, AppDefinition } from "../../core/app";
import { h } from "../../core/dom";
import { loadRastUi, type MountHandle } from "../../core/wasm-ui";

export const aboutLeptosApp: AppDefinition = {
  id: "about-leptos",
  name: "About RAST (Leptos)",
  icon: "info",
  description: "Version information, rendered by the Leptos/WASM UI",
  singleton: true,
  defaultSize: { w: 420, h: 220 },
  minSize: { w: 280, h: 160 },
  mount(ctx) {
    mountAbout(ctx);
  },
};

function mountAbout(ctx: AppContext): void {
  ctx.setTitle("About RAST (Leptos)");
  const body = h("div", { class: "about-leptos" });
  ctx.root.append(body);
  let handle: MountHandle | undefined;
  let disposed = false;

  ctx.onDispose(() => {
    disposed = true;
    handle?.dispose();
    handle?.free?.();
    handle = undefined;
  });

  loadRastUi()
    .then((mod) => {
      if (disposed) return;
      handle = mod.mount_about(body);
    })
    .catch((error: unknown) => {
      console.error("Leptos UI failed to load", error);
      if (disposed) return;
      const reason = error instanceof Error ? error.message : String(error);
      body.append(h("p", { class: "error-title", role: "alert" }, `The Leptos UI could not be loaded: ${reason}`));
    });
}
