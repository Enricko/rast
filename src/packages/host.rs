//! Host detection and package compatibility.
//!
//! The target is `<os>-<arch>` (`linux-x86_64`, `linux-aarch64`). For
//! dynamically linked artifacts the relevant dependency is the C library: the
//! glibc version is read from the running process (RAST itself is a glibc
//! build), and musl hosts are detected so glibc artifacts are refused there.

use serde::Serialize;

use super::{
    manifest::{Artifact, Manifest, KNOWN_TARGETS},
    version,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub target: String,
    /// `glibc`, `musl` or `unknown`.
    pub libc: String,
    pub glibc_version: Option<String>,
    pub kernel: Option<String>,
    pub rast_version: String,
}

impl HostInfo {
    pub fn detect() -> Self {
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let glibc_version = glibc_version();
        let libc = if glibc_version.is_some() {
            "glibc"
        } else if musl_present() {
            "musl"
        } else {
            "unknown"
        };
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            target: format!("{os}-{arch}"),
            os,
            arch,
            libc: libc.to_string(),
            glibc_version,
            kernel,
            rast_version: crate::VERSION.to_string(),
        }
    }

    pub fn supported_target(&self) -> bool {
        KNOWN_TARGETS.contains(&self.target.as_str())
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn glibc_version() -> Option<String> {
    extern "C" {
        fn gnu_get_libc_version() -> *const std::os::raw::c_char;
    }
    // SAFETY: glibc returns a pointer to a static, NUL-terminated string.
    let raw = unsafe { gnu_get_libc_version() };
    if raw.is_null() {
        return None;
    }
    let text = unsafe { std::ffi::CStr::from_ptr(raw) };
    text.to_str().ok().map(str::to_string)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn glibc_version() -> Option<String> {
    None
}

fn musl_present() -> bool {
    std::fs::read_dir("/lib")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
        })
        .unwrap_or(false)
}

/// Why a manifest (or one of its artifacts) cannot be installed on `host`.
pub fn incompatibility(manifest: &Manifest, host: &HostInfo) -> Option<String> {
    if !version::satisfies_minimum(&host.rast_version, manifest.min_rast_version.as_deref()) {
        return Some(format!(
            "Requires RAST {} or newer; this server runs {}.",
            manifest.min_rast_version.as_deref().unwrap_or("?"),
            host.rast_version
        ));
    }
    let Some(artifact) = manifest.artifact_for(&host.target) else {
        let offered = manifest.targets().join(", ");
        return Some(format!(
            "No build for this server ({}); this version is published for {}.",
            host.target, offered
        ));
    };
    artifact_incompatibility(artifact, host)
}

pub fn artifact_incompatibility(artifact: &Artifact, host: &HostInfo) -> Option<String> {
    if let Some(min) = &artifact.requires.glibc {
        match (&host.libc[..], &host.glibc_version) {
            ("glibc", Some(have)) => {
                if !version::satisfies_minimum(have, Some(min)) {
                    return Some(format!(
                        "Needs glibc {min} or newer; this server has glibc {have}."
                    ));
                }
            }
            ("musl", _) => return Some(format!("Needs glibc {min}; this server uses musl libc.")),
            _ => {
                return Some(format!(
                    "Needs glibc {min}; the C library of this server could not be detected."
                ))
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::manifest::{parse, tests::sample};

    fn host(target: &str, glibc: Option<&str>) -> HostInfo {
        HostInfo {
            os: "linux".into(),
            arch: target.trim_start_matches("linux-").into(),
            target: target.into(),
            libc: if glibc.is_some() { "glibc" } else { "musl" }.into(),
            glibc_version: glibc.map(str::to_string),
            kernel: None,
            rast_version: "0.5.0".into(),
        }
    }

    #[test]
    fn detects_this_host() {
        let info = HostInfo::detect();
        assert_eq!(info.os, std::env::consts::OS);
        assert!(info.target.contains(std::env::consts::ARCH));
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert!(info.glibc_version.is_some());
    }

    #[test]
    fn explains_incompatibilities() {
        let mut v = sample();
        v["artifacts"][0]["requires"] = serde_json::json!({"glibc": "2.28"});
        let m = parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(incompatibility(&m, &host("linux-x86_64", Some("2.39"))).is_none());
        let old = incompatibility(&m, &host("linux-x86_64", Some("2.17"))).unwrap();
        assert!(old.contains("glibc 2.28"), "{old}");
        let musl = incompatibility(&m, &host("linux-x86_64", None)).unwrap();
        assert!(musl.contains("musl"), "{musl}");
        let arm = incompatibility(&m, &host("linux-aarch64", Some("2.39"))).unwrap();
        assert!(arm.contains("linux-x86_64"), "{arm}");

        v["minRastVersion"] = "9.0.0".into();
        let future = parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        let msg = incompatibility(&future, &host("linux-x86_64", Some("2.39"))).unwrap();
        assert!(msg.contains("RAST 9.0.0"), "{msg}");
    }
}
