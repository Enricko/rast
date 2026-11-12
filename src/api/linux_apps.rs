//! Linux apps API (administrators only).
//!
//! | Method | Path | |
//! |---|---|---|
//! | GET | `/api/linux-apps` | installed apps and the caller's running copies |
//! | POST | `/api/linux-apps/install` | `{path}` (File Station file) or `{url}` (https link): install (job) |
//! | GET | `/api/linux-apps/{id}` | one app |
//! | PATCH | `/api/linux-apps/{id}` | `{memoryMb?, cpus?}` |
//! | DELETE | `/api/linux-apps/{id}?removeData=true` | uninstall |
//! | GET | `/api/linux-apps/{id}/icon` | the app's icon |
//! | POST | `/api/linux-apps/{id}/check` | ask the official source for a newer version |
//! | POST | `/api/linux-apps/{id}/update` | install the newer version (job) |
//! | POST | `/api/linux-apps/{id}/open` | start (or find) the caller's copy |
//! | GET | `/api/linux-apps/sessions/{sid}` | state of one of the caller's copies |
//! | POST | `/api/linux-apps/sessions/{sid}/close` | stop it now |

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{auth::AdminUser, error::AppResult, state::AppState};

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
        .route("/api/linux-apps/{id}/open", post(open))
        .route("/api/linux-apps/sessions/{sid}", get(session))
        .route("/api/linux-apps/sessions/{sid}/close", post(close))
}

async fn list(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
    Ok(Json(json!({
        "apps": state.linux_apps.list().await?,
        "sessions": state.linux_apps.my_sessions(&admin.0),
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
    _admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.get(&id).await?))
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
    _admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let (kind, bytes) = state.linux_apps.icon(&id).await?;
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
    admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.open(&admin.0, &id).await?))
}

async fn session(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(sid): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.session(&admin.0, &sid)?))
}

async fn close(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(sid): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(state.linux_apps.close(&admin.0, &sid)?))
}
