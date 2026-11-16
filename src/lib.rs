//! RAST — Resource Administration & Storage Toolkit.
//!
//! A self-hosted server administration toolkit with a desktop-like web UI:
//! file station, application deployment, resource monitoring, control panel,
//! backups and audit logs — shipped as a single Rust binary.

pub mod update;

pub use rast_base::{
    access, appearance, audit, auth, config, db, error, events, jobs, realtime, secrets, storage,
    users,
};

pub use rast_base::{DESCRIPTION, NAME, VERSION};

pub use rast_apps::packages;

pub use rast_docker::{docker, worker};

pub use rast_helper::native;

pub use rast_agent::{fileagent, homefs, rootfs, terminal};

pub use rast_session::linux_apps;

pub use rast_server::{
    api, backup, migrate_shares, monitor, native_proxy, server, share_groups, state,
};
