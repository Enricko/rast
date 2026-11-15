//! RAST — Resource Administration & Storage Toolkit.
//!
//! A self-hosted server administration toolkit with a desktop-like web UI:
//! file station, application deployment, resource monitoring, control panel,
//! backups and audit logs — shipped as a single Rust binary.

pub mod api;
pub mod backup;
pub mod linux_apps;
pub mod monitor;
pub mod native_proxy;
pub mod server;
pub mod state;
pub mod update;

pub use rast_base::{
    access, appearance, audit, auth, config, db, error, events, jobs, realtime, secrets, storage,
    users,
};

pub use rast_base::{DESCRIPTION, NAME, VERSION};

pub use rast_apps::packages;

pub use rast_docker::{docker, worker};

pub use rast_helper::native;

pub use rast_agent::{rootfs, terminal};
