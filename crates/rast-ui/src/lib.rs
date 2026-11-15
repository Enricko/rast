//! RAST web UI in Leptos (CSR), compiled to WebAssembly.
//!
//! The TypeScript shell loads this module on demand and mounts one component
//! per RAST window body through the exported `mount_*` functions. Each mount
//! returns a handle; calling `dispose` (or dropping the handle on the
//! JavaScript side with `free`) unmounts the component and releases its
//! reactive state, so closing a window leaks nothing.

use std::any::Any;

use leptos::mount::mount_to;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Product name shown in the About window.
const PRODUCT: &str = "RAST";
/// Version of this build (the workspace version).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// "About RAST (Leptos)": name, version and where it is rendered.
///
/// Styling uses the shell's existing classes (`card`, `card-head`, ...), so it
/// follows the current theme without inline styles.
#[component]
pub fn About() -> impl IntoView {
    view! {
        <div class="card" data-rast-ui="about">
            <div class="card-head">
                <div class="card-title">{PRODUCT}</div>
                <div class="card-subtitle">{format!("Version {VERSION}")}</div>
            </div>
            <p class="card-subtitle">
                "This window is rendered by Leptos, compiled to WebAssembly."
            </p>
        </div>
    }
}

/// Keeps a mounted component alive until `dispose` is called.
#[wasm_bindgen]
pub struct MountHandle {
    // Dropping a Leptos `UnmountHandle` removes the view and its reactive owner.
    inner: Option<Box<dyn Any>>,
}

#[wasm_bindgen]
impl MountHandle {
    /// Unmount the component. Safe to call more than once.
    pub fn dispose(&mut self) {
        self.inner.take();
    }
}

/// Runs once when the module is instantiated.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Mount the About component inside `el` and return its handle.
#[wasm_bindgen]
pub fn mount_about(el: web_sys::HtmlElement) -> MountHandle {
    let handle = mount_to(el, About);
    MountHandle {
        inner: Some(Box::new(handle)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_workspace_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn dispose_is_idempotent() {
        let mut handle = MountHandle {
            inner: Some(Box::new(1_u8)),
        };
        handle.dispose();
        handle.dispose();
        assert!(handle.inner.is_none());
    }
}
