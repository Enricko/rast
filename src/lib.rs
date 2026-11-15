//! RAST — Resource Administration & Storage Toolkit.
//!
//! A self-hosted server administration toolkit with a desktop-like web UI:
//! file station, application deployment, resource monitoring, control panel,
//! backups and audit logs — shipped as a single Rust binary.

pub mod api;
pub mod backup;
pub mod docker;
pub mod linux_apps;
pub mod monitor;
pub mod native;
pub mod native_proxy;
pub mod packages;
pub mod rootfs;
pub mod server;
pub mod state;
pub mod terminal;
pub mod update;
pub mod worker;

pub use rast_base::{
    access, appearance, audit, auth, config, db, error, events, jobs, realtime, secrets, storage,
    users,
};

/// Product name.
pub const NAME: &str = "RAST";
/// Semantic version of this build (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Product description used in UI/metadata.
pub const DESCRIPTION: &str = "Resource Administration & Storage Toolkit";
