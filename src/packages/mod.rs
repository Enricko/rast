//! Package Center: verified, side-by-side runtime packages.
//!
//! * [`manifest`] — the versioned `rast-package.json` format.
//! * [`catalog`] — the signed official catalog (Ed25519, anti-rollback).
//! * [`github`] — resolving and cross-checking third-party GitHub releases.
//! * [`net`] / [`archive`] — origin-restricted downloads and safe extraction.
//! * [`install`] — staged installs under `<data_dir>/packages/runtimes`.
//! * [`sandbox`] — confinement (process group, seccomp, Landlock) for checks.
//! * [`system`] — runtimes the operating system already has (apt, manual).
//! * [`shell`] — the defaults on every login shell's PATH (`/etc/profile.d`).
//! * [`service`] — the orchestration used by the HTTP API.

pub mod archive;
pub mod catalog;
pub mod github;
pub mod host;
pub mod install;
pub mod manifest;
pub mod net;
pub mod sandbox;
pub mod service;
pub mod shell;
pub mod store;
pub mod system;
pub mod version;

pub use service::PackageManager;
