//! Linux apps API.
//!
//! Every signed-in user can list and open the apps shared with them; managing
//! apps (installing, updating, removing, limits and visibility) is
//! administrator-only.
//!
//! | Method | Path | Who |
//! |---|---|---|
//! | GET | `/api/linux-apps` | any signed-in user: installed apps they may see and their running copies |
//! | POST | `/api/linux-apps/install` | administrator: `{path}` (File Station file) or `{url}` (https link): install (job) |
//! | GET | `/api/linux-apps/{id}` | any signed-in user: one app they may see |
//! | PATCH | `/api/linux-apps/{id}` | administrator: `{memoryMb?, cpus?}` |
//! | DELETE | `/api/linux-apps/{id}?removeData=true` | administrator: uninstall |
//! | GET | `/api/linux-apps/{id}/icon` | any signed-in user: the app's icon |
//! | POST | `/api/linux-apps/{id}/check` | administrator: ask the official source for a newer version |
//! | POST | `/api/linux-apps/{id}/update` | administrator: install the newer version (job) |
//! | POST | `/api/linux-apps/{id}/visibility` | administrator: `{visibility:"users"|"admins"}` |
//! | POST | `/api/linux-apps/{id}/open` | any signed-in user: start (or find) their own copy |
//! | GET | `/api/linux-apps/sessions/{sid}` | any signed-in user: state of one of their own copies |
//! | POST | `/api/linux-apps/sessions/{sid}/close` | any signed-in user: stop their own copy now |
//! | GET | `/api/linux-apps/sessions/{sid}/windows` | any signed-in user: the app's windows (separate windows) |
//! | POST | `/api/linux-apps/sessions/{sid}/windows/{window}/stream` | any signed-in user: start (or find) that window's stream → `{url}` |
//! | DELETE | `/api/linux-apps/sessions/{sid}/windows/{window}/stream` | any signed-in user: stop that window's stream |
//! | POST | `/api/linux-apps/sessions/{sid}/windows/{window}/close` | any signed-in user: close the window inside the app |

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{AdminUser, AuthUser},
    error::AppResult,
    linux_apps::Visibility,
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/linux-apps", get(list))
        .route("/api/linux-apps/install", post(install))
        .route(
            "/api/linux-apps/{id}",
            get(one).patch(set_limits).delete(uninstall),
        )
        .route("/api/linux-apps/{id}/icon", get(icon))
        .route("/api/linux-apps/{id}/check", post(check))
        .route("/api/linux-apps/{id}/update", post(update))
        .route("/api/linux-apps/{id}/visibility", post(set_visibility))
        .route("/api/linux-apps/{id}/open", post(open))
        .route("/api/linux-apps/sessions/{sid}", get(session))
        .route("/api/linux-apps/sessions/{sid}/close", post(close))
        // Separate windows (per-window streaming).
        .route("/api/linux-apps/sessions/{sid}/windows", get(windows))
        .route(
            "/api/linux-apps/sessions/{sid}/windows/{window}/stream",
            post(stream_window).delete(stop_window_stream),
        )
        .route(
            "/api/linux-apps/sessions/{sid}/windows/{window}/close",
            post(close_window),
        )
}

async fn list(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    Ok(Json(json!({
        "apps": state.linux_apps.list(&user).await?,
        "sessions": state.linux_apps.my_sessions(&user),
        "limits": {
            "minFreeMemoryBytes": crate::linux_apps::MIN_FREE_MEMORY_BYTES,
            "idleMinutes": crate::linux_apps::IDLE_LIMIT.as_secs() / 60,
            "maxCpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        },
        "dockerEnabled": state.config.docker_enabled && crate::linux_apps::SUPPORTED,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    /// A File Station path…
    path: Option<String>,
    /// …or an `https://` link the server downloads.
    url: Option<String>,
}

async fn install(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<InstallRequest>,
) -> AppResult<Json<Value>> {
    let apps = &state.linux_apps;
    Ok(Json(match (body.path.as_deref(), body.url.as_deref()) {
        (Some(path), None) => apps.install(&admin.0, &state.storage, path.trim()).await?,
        (None, Some(url)) => apps.install_url(&admin.0, url.trim()).await?,
        _ => {
            return Err(crate::error::AppError::bad_request(
                "send either `path` or `url`",
            ))
        }
    }))
}

async fn one(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.get(&user, &id).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VisibilityRequest {
    visibility: String,
}

async fn set_visibility(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
    Json(body): Json<VisibilityRequest>,
) -> AppResult<Json<Value>> {
    let visibility = Visibility::parse(body.visibility.trim()).ok_or_else(|| {
        crate::error::AppError::bad_request("visibility must be \"users\" or \"admins\"")
    })?;
    Ok(Json(
        state
            .linux_apps
            .set_visibility(&admin.0, &id, visibility)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LimitsRequest {
    memory_mb: Option<u32>,
    cpus: Option<u32>,
}

async fn set_limits(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
    Json(body): Json<LimitsRequest>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        state
            .linux_apps
            .set_limits(&admin.0, &id, body.memory_mb, body.cpus)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UninstallQuery {
    #[serde(default)]
    remove_data: bool,
}

async fn uninstall(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
    Query(query): Query<UninstallQuery>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        state
            .linux_apps
            .uninstall(&admin.0, &id, query.remove_data)
            .await?,
    ))
}

async fn icon(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let (kind, bytes) = state.linux_apps.icon(&user, &id).await?;
    let content_type = if kind == "svg" {
        "image/svg+xml"
    } else {
        "image/png"
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // The URL changes with every update (`?v=`), so it may be cached.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    Ok(response)
}

async fn check(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.check_now(&id).await?))
}

async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.update(&admin.0, &id).await?))
}

async fn open(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.open(&user, &id).await?))
}

async fn session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sid): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.session(&user, &sid)?))
}

async fn close(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sid): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.close(&user, &sid)?))
}

async fn windows(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sid): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.windows(&user, &sid).await?))
}

async fn stream_window(
    State(state): State<AppState>,
    user: AuthUser,
    Path((sid, window)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        state
            .linux_apps
            .start_window_stream(&user, &sid, &window)
            .await?,
    ))
}

async fn stop_window_stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path((sid, window)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        state
            .linux_apps
            .stop_window_stream(&user, &sid, &window)
            .await?,
    ))
}

async fn close_window(
    State(state): State<AppState>,
    user: AuthUser,
    Path((sid, window)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        state.linux_apps.close_window(&user, &sid, &window).await?,
    ))
}
