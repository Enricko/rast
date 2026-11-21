//! Native entry point: `cargo run -p rast-ui --bin rast`.
//!
//! The browser build links the library and opens Rune after local unlock.

#![forbid(unsafe_code)]

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    rast_ui::start();
}

// The browser runs the library, not the native binary.
#[cfg(target_arch = "wasm32")]
fn main() {}
