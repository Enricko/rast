//! Build script: guarantees that the frontend asset directory exists so
//! `rust-embed` (see `src/server.rs`) can always compile.
//!
//! Debug builds (clippy, tests) may embed a stand-in page when the frontend
//! was not built. A release build must never do that: it fails instead, so a
//! binary without the UI cannot be shipped by accident.

use std::path::Path;

/// Marker inside the stand-in page; CI also greps the release binary for it.
const PLACEHOLDER_MARKER: &str = "rast-placeholder-frontend";

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
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
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        let index = std::fs::read_to_string(dist.join("index.html")).unwrap_or_default();
        assert!(
            !index.is_empty() && !index.contains(PLACEHOLDER_MARKER),
            "frontend/dist/index.html is missing or is the build placeholder; run \
             `npm --prefix frontend run build` before a release build"
        );
    }
    println!("cargo:rerun-if-changed=../../frontend/dist");
}
