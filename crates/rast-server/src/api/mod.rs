//! HTTP API router.
//!
//! Public endpoints (login/setup/health) are mounted unauthenticated. Every
//! other endpoint passes through the session + CSRF middleware and must check
//! object-level permissions in its handler.

use axum::{http::StatusCode, middleware, response::IntoResponse, routing::get, Router};

use crate::{auth::middleware::require_auth, server, state::AppState};

pub mod appearance;
pub mod apps;
pub mod auth;
pub mod backup;
pub mod browser;
pub mod docker;
pub mod events;
pub mod files;
pub mod groups;
pub mod homefs;
pub mod jobs;
pub mod linux_apps;
pub mod logs;
pub mod monitor;
pub mod packages;
pub mod rootfs;
pub mod settings;
pub mod system;
pub mod terminal;
pub mod terminal_native;
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
        .merge(
            files::routes()
                // Runs second: one-HOME accounts go to their file agent.
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    homefs::dispatch,
                ))
                // Runs first: `/@root` goes to the root file agent.
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    rootfs::dispatch,
                )),
        )
        .merge(groups::routes())
        .merge(appearance::routes())
        .merge(apps::routes())
        .merge(docker::routes())
        .merge(terminal::routes())
        .merge(terminal_native::routes())
        .merge(monitor::routes())
        .merge(backup::routes())
        .merge(packages::routes())
        .merge(browser::routes())
        .merge(linux_apps::routes())
        .route("/api/events", get(events::stream))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::access::middleware::enforce_app_policy::<AppState>,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth::<AppState>,
        ));

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
        // A running Linux app's page; the key in the path is its credential
        // (see `linux_apps::proxy`).
        .route(
            "/linux-apps/session/{*rest}",
            axum::routing::any(crate::native_proxy::handle),
        )
        .route(
            "/linux-apps/session/",
            axum::routing::any(crate::native_proxy::handle),
        )
        .route(
            "/linux-apps/{*rest}",
            axum::routing::any(crate::linux_apps::proxy::handle::<AppState>),
        )
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

pub use crate::storage::safe_path::validate_logical_path;
