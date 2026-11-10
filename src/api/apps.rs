//! Applications / Deployment API (Docker, admin only).
//!
//! OWNER: teammate `backend-docker-monitor`. Replace this placeholder; keep
//! `pub fn routes() -> Router<AppState>` as the integration point.

use axum::{routing::get, Router};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps", get(not_ready))
        .route("/api/apps/{*rest}", get(not_ready))
        .route("/api/docker/status", get(not_ready))
}

async fn not_ready() -> axum::response::Response {
    crate::api::not_implemented("Applications")
}
