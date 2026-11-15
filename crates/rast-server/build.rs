//! Build script: guarantees that the asset directories exist so `rust-embed`
//! (see `src/server.rs`) can always compile.
//!
//! Debug builds (clippy, tests) may embed stand-ins when the frontend or the
//! WASM UI was not built. A release build must never do that: it fails
//! instead, so a binary without the UI cannot be shipped by accident.

use std::path::Path;

/// Marker inside the stand-in page; CI also greps the release binary for it.
const PLACEHOLDER_MARKER: &str = "rast-placeholder-frontend";
/// Marker inside the stand-in WASM glue (`rast-ui.js`); CI greps for it too.
const UI_PLACEHOLDER_MARKER: &str = "rast-placeholder-ui";
/// An empty but valid WebAssembly module (magic number and version).
const EMPTY_WASM: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let release = std::env::var("PROFILE").as_deref() == Ok("release");

    let dist = Path::new(&manifest).join("../../frontend/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(
            dist.join("index.html"),
            format!(
                "<!doctype html><title>RAST</title><!-- {PLACEHOLDER_MARKER} --><p>Frontend \
                 assets are built with <code>npm --prefix frontend run build</code>.</p>"
            ),
        );
    }
    if release {
        let index = std::fs::read_to_string(dist.join("index.html")).unwrap_or_default();
        assert!(
            !index.is_empty() && !index.contains(PLACEHOLDER_MARKER),
            "frontend/dist/index.html is missing or is the build placeholder; run \
             `npm --prefix frontend run build` before a release build"
        );
    }
    println!("cargo:rerun-if-changed=../../frontend/dist");

    let ui = Path::new(&manifest).join("../../crates/rast-ui/dist");
    if !ui.exists() {
        let _ = std::fs::create_dir_all(&ui);
        let _ = std::fs::write(
            ui.join("rast-ui.js"),
            format!(
                "// {UI_PLACEHOLDER_MARKER}\nthrow new Error(\"the Leptos UI is not built; run \
                 scripts/build-ui.sh\");\n"
            ),
        );
        let _ = std::fs::write(ui.join("rast-ui_bg.wasm"), EMPTY_WASM);
    }
    if release {
        let glue = std::fs::read_to_string(ui.join("rast-ui.js")).unwrap_or_default();
        let wasm = std::fs::metadata(ui.join("rast-ui_bg.wasm")).map_or(0, |m| m.len());
        assert!(
            !glue.is_empty()
                && !glue.contains(UI_PLACEHOLDER_MARKER)
                && wasm > EMPTY_WASM.len() as u64,
            "crates/rast-ui/dist is missing or is the build placeholder; run \
             `scripts/build-ui.sh` (Trunk, `trunk build --release` in crates/rast-ui) before a \
             release build"
        );
    }
    println!("cargo:rerun-if-changed=../../crates/rast-ui/dist");
}
