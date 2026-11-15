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

pub use rast_base::{DESCRIPTION, NAME, VERSION};

pub use rast_apps::packages;
