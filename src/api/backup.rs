//! Backup Center API.
//!
//! OWNER: lead. Placeholder until the backup engine lands.

use axum::{routing::get, Router};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/backups", get(not_ready))
        .route("/api/backups/{*rest}", get(not_ready))
}

async fn not_ready() -> axum::response::Response {
    crate::api::not_implemented("Backup Center")
}
