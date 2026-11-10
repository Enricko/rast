//! Resource monitor API.
//!
//! OWNER: teammate `backend-docker-monitor`. Replace this placeholder; keep
//! `pub fn routes() -> Router<AppState>` as the integration point.

use axum::{routing::get, Router};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/monitor/snapshot", get(not_ready))
        .route("/api/monitor/history", get(not_ready))
        .route("/api/monitor/containers", get(not_ready))
}

async fn not_ready() -> axum::response::Response {
    crate::api::not_implemented("Resource monitor")
}
