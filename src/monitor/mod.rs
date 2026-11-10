//! Resource monitor: host CPU/memory/swap/uptime, disks, network and container
//! stats. One sampler runs per process; UI tabs subscribe via SSE/HTTP.
//!
//! OWNER: teammate `backend-docker-monitor`. Replace this placeholder while
//! keeping the signatures used by `AppState` and the monitor API.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{config::Config, error::AppResult};

pub struct Monitor {
    interval_secs: u64,
}

impl Monitor {
    pub fn new(cfg: &Config) -> AppResult<Arc<Self>> {
        Ok(Arc::new(Self {
            interval_secs: cfg.monitor_interval_secs,
        }))
    }

    pub async fn sample(&self) -> Value {
        json!({
            "ts": crate::db::now_ms(),
            "intervalSecs": self.interval_secs,
            "host": Value::Null,
            "unavailable": true,
            "reason": "monitor module not implemented yet",
        })
    }

    pub fn is_fresh(&self) -> bool {
        false
    }

    pub fn latest(&self) -> Option<Value> {
        None
    }
}
