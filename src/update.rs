//! Self-update: verify a downloaded release, back up the database, replace the
//! binary atomically, restart the service and check health.
//!
//! OWNER: lead. Placeholder until the release/update flow is implemented.

use std::process::ExitCode;

use crate::config::Config;

pub async fn run(
    _cfg: Config,
    _version: Option<&str>,
    _check_only: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    Err("rast update is not implemented in this build yet".into())
}
