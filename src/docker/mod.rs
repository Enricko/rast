//! Docker administration. The web server never touches the Docker socket
//! directly; privileged container operations go through a separate worker
//! process (see `crate::worker`) reachable over a Unix socket.
//!
//! OWNER: teammate `backend-docker-monitor`. Replace this placeholder while
//! keeping the public interface used by the API layer.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::Config;

pub struct DockerManager {
    pub enabled: bool,
    pub socket_path: std::path::PathBuf,
}

impl DockerManager {
    pub fn new(cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            enabled: cfg.docker_enabled,
            socket_path: cfg.worker_socket.clone(),
        })
    }

    pub async fn status(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "available": false,
            "reason": "docker module not implemented yet"
        })
    }
}
