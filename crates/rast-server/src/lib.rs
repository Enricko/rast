//! The Axum application: REST and SSE API, backups, monitoring, shared state and the native session proxy.

pub mod api;
pub mod backup;
pub mod migrate_home;
pub mod migrate_shares;
pub mod monitor;
pub mod native_proxy;
pub mod quota_sync;
pub mod server;
pub mod share_groups;
pub mod state;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_agent::{fileagent, homefs, rootfs, terminal};
use rast_apps::packages;
use rast_base::{
    access, appearance, audit, auth, config, db, error, events, jobs, realtime, secrets, storage,
    users,
};
use rast_docker::docker;
use rast_helper::native;
use rast_session::linux_apps;
// Crate-level constants of the product.
use rast_base::{DESCRIPTION, NAME, VERSION};
