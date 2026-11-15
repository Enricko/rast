//! Build script: embeds Git revision metadata (`RAST_GIT_SHA`).
//!
//! The frontend embed and its release-build check live in
//! `crates/rast-server/build.rs`.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RAST_GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
