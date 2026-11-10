//! Build script: embeds Git revision metadata and guarantees that the frontend
//! asset directory exists so `rust-embed` can always compile.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dist = Path::new(&manifest).join("frontend/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(
            dist.join("index.html"),
            "<!doctype html><title>RAST</title><p>Frontend assets are built with \
             <code>npm --prefix frontend run build</code>.</p>",
        );
    }

    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RAST_GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=frontend/dist");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
