//! RAST — Resource Administration & Storage Toolkit.
//!
//! A self-hosted server administration toolkit with a desktop-like web UI:
//! file station, application deployment, resource monitoring, control panel,
//! backups and audit logs — shipped as a single Rust binary.

pub mod access;
pub mod api;
pub mod appearance;
pub mod audit;
pub mod auth;
pub mod backup;
pub mod config;
pub mod db;
pub mod docker;
pub mod error;
pub mod events;
pub mod jobs;
pub mod linux_apps;
pub mod monitor;
pub mod native;
pub mod packages;
pub mod realtime;
pub mod secrets;
pub mod server;
pub mod state;
pub mod storage;
pub mod update;
pub mod users;
pub mod worker;

/// Product name.
pub const NAME: &str = "RAST";
/// Semantic version of this build (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Product description used in UI/metadata.
pub const DESCRIPTION: &str = "Resource Administration & Storage Toolkit";
