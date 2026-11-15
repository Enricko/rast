//! The Axum application: REST and SSE API, backups, monitoring, shared state and the native session proxy.

pub mod api;
pub mod backup;
pub mod monitor;
pub mod native_proxy;
pub mod server;
pub mod state;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_agent::{rootfs, terminal};
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
