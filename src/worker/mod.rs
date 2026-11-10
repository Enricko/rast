//! Privileged administrative worker for Docker operations.
//!
//! The worker runs as a separate systemd unit (`rast-worker.service`) with
//! access to the Docker socket and listens on `worker_socket`. The web server
//! (unprivileged) sends a strict JSON protocol with structured arguments only.
//!
//! OWNER: teammate `backend-docker-monitor`. Replace this placeholder.

use std::process::ExitCode;

use crate::config::Config;

pub async fn run(_cfg: Config) -> Result<ExitCode, Box<dyn std::error::Error>> {
    Err("docker worker is not implemented in this build yet".into())
}
