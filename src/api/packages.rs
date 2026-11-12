//! Package Center API.
//!
//! Reading the catalog, installed versions and job history requires the
//! `packages` application (administrators, or a read-only group grant).
//! Every mutation — checking the catalog, approving a source, installing,
//! updating, activating, removing and editing consumers — is
//! administrator-only and audited by [`crate::packages::service`].

use axum::{
    extract::{Path, Query, State},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    access,
    auth::{AdminUser, AuthUser},
    error::{AppError, AppResult},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/packages", get(overview))
        .route("/api/packages/check", post(check))
        .route("/api/packages/github/inspect", post(github_inspect))
        .route("/api/packages/github/install", post(github_install))
        .route("/api/packages/consumers", post(create_consumer))
        .route(
            "/api/packages/consumers/{id}",
            put(update_consumer).delete(delete_consumer),
        )
        .route("/api/packages/{id}", get(package))
        .route("/api/packages/{id}/install", post(install))
        .route("/api/packages/{id}/update", post(update))
        .route("/api/packages/{id}/default", put(set_default))
        .route(
            "/api/packages/{id}/versions/{version}",
            delete(remove_version),
        )
}

/// GET /api/packages
pub async fn overview(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    access::app_surface(&state.db, &state, &user, "packages").await?;
    let overview = state.packages.overview(user.is_admin()).await?;
    Ok(Json(
        serde_json::to_value(overview).map_err(|e| AppError::internal(e.to_string()))?,
    ))
}

/// GET /api/packages/{id}
pub async fn package(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    access::app_surface(&state.db, &state, &user, "packages").await?;
    let (view, jobs) = state.packages.package(&id, user.is_admin()).await?;
    Ok(Json(
        json!({ "package": view, "jobs": jobs, "host": state.packages.host }),
    ))
}

/// POST /api/packages/check — fetch and verify the catalog now.
pub async fn check(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> AppResult<Json<Value>> {
    let status = state.packages.check_now(Some(&user)).await?;
    Ok(Json(json!({ "catalog": status })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallRequest {
    pub version: Option<String>,
    pub make_default: Option<bool>,
}

/// POST /api/packages/{id}/install — install a specific catalog version.
pub async fn install(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<InstallRequest>,
) -> AppResult<Json<Value>> {
    let version = body
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::bad_request("choose a version to install"))?;
    let job_id = state
        .packages
        .install_official(&user, &id, Some(version), body.make_default)
        .await?;
    Ok(Json(json!({ "jobId": job_id })))
}

/// POST /api/packages/{id}/update — install the newest compatible version
/// (of the given version's channel, or the default's channel) alongside the
/// current ones. The previous version stays installed for rollback.
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<InstallRequest>,
) -> AppResult<Json<Value>> {
    let version = body
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let job_id = state
        .packages
        .install_official(&user, &id, version, body.make_default)
        .await?;
    Ok(Json(json!({ "jobId": job_id })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefaultRequest {
    pub version: Option<String>,
    /// `true` goes back to the system's own runtime instead of a version.
    #[serde(default)]
    pub system: bool,
}

/// PUT /api/packages/{id}/default — select the RAST default version
/// (`{"version": "1.27.4"}`) or the system's runtime (`{"system": true}`).
pub async fn set_default(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<DefaultRequest>,
) -> AppResult<Json<Value>> {
    match (body.version.as_deref().map(str::trim), body.system) {
        (Some(version), false) if !version.is_empty() => {
            let activation = state.packages.set_default(&user, &id, version).await?;
            Ok(Json(json!({ "ok": true, "activation": activation })))
        }
        (None, true) => {
            state.packages.use_system(&user, &id).await?;
            Ok(Json(
                json!({ "ok": true, "activation": { "shims": [], "conflicts": [] } }),
            ))
        }
        _ => Err(AppError::bad_request(
            "Send either a version or \"system\": true.",
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct RemoveQuery {
    pub confirm: Option<String>,
}

/// DELETE /api/packages/{id}/versions/{version}?confirm=<version>
pub async fn remove_version(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, version)): Path<(String, String)>,
    Query(q): Query<RemoveQuery>,
) -> AppResult<Json<Value>> {
    if q.confirm.as_deref() != Some(version.as_str()) {
        return Err(AppError::bad_request(
            "Removal must be confirmed: pass confirm=<version>.",
        ));
    }
    state.packages.remove_version(&user, &id, &version).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumerRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub package_id: Option<String>,
    /// `null` follows the RAST default; a version pins it.
    pub version: Option<String>,
}

/// POST /api/packages/consumers
pub async fn create_consumer(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Json(body): Json<ConsumerRequest>,
) -> AppResult<Json<Value>> {
    let package_id = body
        .package_id
        .as_deref()
        .ok_or_else(|| AppError::bad_request("packageId is required"))?;
    let version = body
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let id = state
        .packages
        .create_consumer(&user, &body.name, &body.description, package_id, version)
        .await?;
    Ok(Json(json!({ "id": id })))
}

/// PUT /api/packages/consumers/{id}
pub async fn update_consumer(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    Json(body): Json<ConsumerRequest>,
) -> AppResult<Json<Value>> {
    let version = body
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    state
        .packages
        .update_consumer(&user, id, &body.name, &body.description, version)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// DELETE /api/packages/consumers/{id}
pub async fn delete_consumer(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    state.packages.delete_consumer(&user, id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectRequest {
    pub url: String,
}

/// POST /api/packages/github/inspect — step 1: resolve the link to a release,
/// validate its manifest and return a preview pinned to that release.
pub async fn github_inspect(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Json(body): Json<InspectRequest>,
) -> AppResult<Json<Value>> {
    if body.url.len() > 500 {
        return Err(AppError::bad_request("link is too long"));
    }
    let preview = state.packages.inspect_github(&user, &body.url).await?;
    Ok(Json(
        serde_json::to_value(preview).map_err(|e| AppError::internal(e.to_string()))?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubInstallRequest {
    pub inspection_id: String,
    #[serde(default)]
    pub approve_source: bool,
    pub make_default: Option<bool>,
}

/// POST /api/packages/github/install — step 2: approve the source (first time)
/// and install exactly the inspected release.
pub async fn github_install(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Json(body): Json<GithubInstallRequest>,
) -> AppResult<Json<Value>> {
    let job_id = state
        .packages
        .install_inspection(
            &user,
            &body.inspection_id,
            body.approve_source,
            body.make_default,
        )
        .await?;
    Ok(Json(json!({ "jobId": job_id })))
}
