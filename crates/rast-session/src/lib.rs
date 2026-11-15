//! Native Xpra sessions, app launch and install, and the legacy Docker Linux-app backend.

pub mod linux_apps;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_apps::packages;
use rast_base::{audit, auth, config, db, error, events, jobs, storage};
use rast_docker::docker;
use rast_helper::native;
