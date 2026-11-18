// Loader for the Leptos/WASM UI (crates/rast-ui), served by rast-server under
// /ui/. The glue file has a stable name (Trunk `filehash = false`), so no
// manifest is needed. The module is fetched and instantiated once per page;
// a failed load is not cached, so reopening the window retries.

/** Handle returned by every `mount_*` export; `dispose` unmounts the component. */
export interface MountHandle {
  dispose(): void;
  free?(): void;
}

/** The subset of the wasm-bindgen glue module that the shell uses. */
export interface RastUiModule {
  /** wasm-bindgen `init`: fetches and instantiates the .wasm next to the glue. */
  default(input?: unknown): Promise<unknown>;
  mount_about(el: HTMLElement): MountHandle;
}

export const RAST_UI_GLUE_URL = "/ui/rast-ui.js";

type Importer = (url: string) => Promise<RastUiModule>;

// The URL is a runtime value on purpose: Vite must not try to bundle it.
const browserImport: Importer = (url) => import(/* @vite-ignore */ url) as Promise<RastUiModule>;

export interface RastUiLoader {
  load(): Promise<RastUiModule>;
}

export function createRastUiLoader(importer: Importer = browserImport, url: string = RAST_UI_GLUE_URL): RastUiLoader {
  let cached: Promise<RastUiModule> | undefined;
  return {
    load() {
      if (!cached) {
        const attempt = (async () => {
          const mod = await importer(url);
          await mod.default();
          return mod;
        })();
        cached = attempt;
        attempt.catch(() => {
          if (cached === attempt) cached = undefined;
        });
      }
      return cached;
    },
  };
}

const shared = createRastUiLoader();

/** Load and initialise the Leptos UI module (once per page). */
export function loadRastUi(): Promise<RastUiModule> {
  return shared.load();
}
