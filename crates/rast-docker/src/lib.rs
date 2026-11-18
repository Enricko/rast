//! Docker Compose apps, the privileged worker and the legacy Docker Linux-app backend.

pub mod docker;
pub mod worker;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_base::{access, config, db, error, events};
// Crate-level constants of the product.
use rast_base::VERSION;
