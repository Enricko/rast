//! Native entry point: `cargo run -p rast-ui --bin rast`.
//!
//! The browser build does not use this file; it links the crate as a cdylib and
//! runs `rast_ui::start` through `wasm_bindgen(start)`.

#![forbid(unsafe_code)]

fn main() {
    rast_ui::start();
}
