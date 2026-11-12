//! HTTP API router.
//!
//! Public endpoints (login/setup/health) are mounted unauthenticated. Every
//! other endpoint passes through the session + CSRF middleware and must check
//! object-level permissions in its handler.

use axum::{http::StatusCode, middleware, response::IntoResponse, routing::get, Router};

use crate::{auth::middleware::require_auth, error::AppError, server, state::AppState};

pub mod appearance;
pub mod apps;
pub mod auth;
pub mod backup;
pub mod docker;
pub mod events;
pub mod files;
pub mod groups;
pub mod jobs;
pub mod logs;
pub mod monitor;
pub mod packages;
pub mod settings;
pub mod system;
pub mod terminal;
pub mod users;

/// Build the complete application router (API + embedded frontend).
pub fn router(state: AppState) -> Router {
    let protected: Router<AppState> = Router::new()
        .merge(auth::routes())
        .merge(users::routes())
        .merge(settings::routes())
        .merge(logs::routes())
        .merge(jobs::routes())
        .merge(system::protected_routes())
        .merge(files::routes())
        .merge(groups::routes())
        .merge(appearance::routes())
        .merge(apps::routes())
        .merge(docker::routes())
        .merge(terminal::routes())
        .merge(monitor::routes())
        .merge(backup::routes())
        .merge(packages::routes())
        .route("/api/events", get(events::stream))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::access::middleware::enforce_app_policy,
        ))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .merge(public_routes())
        .merge(protected)
        .fallback(server::static_files)
        .with_state(state)
}

fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/bootstrap", get(system::bootstrap))
        .route("/api/setup/status", get(auth::setup_status))
        .route("/api/setup", axum::routing::post(auth::setup))
        .route("/api/auth/login", axum::routing::post(auth::login))
}

/// Shared helper: JSON 501 for features not yet available on this host.
pub fn not_implemented(what: &'static str) -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(serde_json::json!({
            "error": {"code": "not_implemented", "message": format!("{what} is not implemented")}
        })),
    )
        .into_response()
}

/// Convert an optional query parameter into a bounded usize.
pub fn parse_bounded(value: Option<&str>, default: usize, max: usize) -> usize {
    value
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// Reject paths containing traversal or control characters before any
/// filesystem call. Backend handlers still re-check after resolution.
pub fn validate_logical_path(path: &str) -> Result<(), AppError> {
    if path.contains('\0') || path.contains('\\') {
        return Err(AppError::bad_request("invalid path"));
    }
    for segment in path.split('/') {
        if segment == ".." {
            return Err(AppError::bad_request("path traversal is not allowed"));
        }
        if segment.chars().any(|c| c.is_control()) {
            return Err(AppError::bad_request("invalid path"));
        }
    }
    Ok(())
}
