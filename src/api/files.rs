//! File Station API: listings, upload/download streaming, filesystem
//! operations, recycle bin, previews, quota and search.
//!
//! OWNER: teammate `backend-storage` (tambahan: shared-folder ACL endpoints).
//! Replace this placeholder; keep `pub fn routes() -> Router<AppState>` as the
//! integration point.

use axum::{routing::get, Router};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/files", get(not_ready))
        .route("/api/files/{*path}", get(not_ready))
        .route("/api/shares", get(not_ready))
}

async fn not_ready() -> axum::response::Response {
    crate::api::not_implemented("File Station")
}
