//! App Center: runtime catalog, GitHub packages, package install and sandbox.

pub mod packages;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_base::{audit, auth, config, db, error, events, jobs};
// Crate-level constants of the product.
use rast_base::VERSION;
