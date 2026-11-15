//! Root helper: JSON-lines protocol, identities, transient units, APT and nftables.

pub mod native;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_apps::packages;
use rast_base::config;
// Crate-level constants of the product.
use rast_base::VERSION;
