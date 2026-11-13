//! App Center API.
//!
//! App Center is for administrators and the members of groups an
//! administrator granted the `packages` application; everyone else is
//! refused. Those package managers may check the catalog, install, update,
//! change the default, remove, uninstall, cancel and edit consumers.
//! Approving a new GitHub source stays administrator-only. Every mutation
//! is audited by [`crate::packages::service`].

use axum::{
    extract::{Path, Query, State},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    access,
    auth::AuthUser,
    error::{AppError, AppResult},
    state::AppState,
};

/// Whether `user` may change packages (see the module docs).
pub async fn can_manage(state: &AppState, user: &AuthUser) -> AppResult<bool> {
    if user.is_admin() {
        return Ok(true);
    }
    let access = access::app_access(&state.db, user, "packages").await?;
    Ok(access.allowed && !access.read_only)
}

/// An account allowed to change packages: an administrator or a member of a
/// group granted App Center.
pub struct PackageManager(pub AuthUser);

impl axum::extract::FromRequestParts<AppState> for PackageManager {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if can_manage(state, &user).await? {
            Ok(Self(user))
        } else {
            Err(AppError::Forbidden(
                "App Center is for administrators and the groups they grant it to",
            ))
        }
    }
}

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
        .route("/api/packages/apps/{id}/install", post(install_app))
        .route("/api/packages/apps/{id}", delete(uninstall_app))
        .route("/api/packages/{id}", get(package).delete(uninstall))
        .route("/api/packages/{id}/run", post(run_package_app))
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
    let manage = can_manage(&state, &user).await?;
    let overview = state.packages.overview(manage).await?;
    let mut body = serde_json::to_value(overview).map_err(|e| AppError::internal(e.to_string()))?;
    body["apps"] = json!(apps_state(&state).await);
    Ok(Json(body))
}

/// RAST's own apps as App Center lists them: built-in ones are always
/// installed; removable ones can be uninstalled and installed again.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPackage {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed: bool,
    pub removable: bool,
}

pub async fn apps_state(state: &AppState) -> Vec<AppPackage> {
    let removed = access::removed_apps(&state.db).await;
    std::iter::once(access::LAUNCHER_APP)
        .chain(access::APP_IDS.iter().copied())
        .map(|id| AppPackage {
            id: id.to_string(),
            name: access::app_label(id).to_string(),
            description: access::app_description(id).to_string(),
            installed: !removed.iter().any(|r| r == id),
            removable: access::REMOVABLE_APPS.contains(&id),
        })
        .collect()
}

/// POST /api/packages/apps/{id}/install
pub async fn install_app(
    State(state): State<AppState>,
    PackageManager(user): PackageManager,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    set_app(&state, &user, &id, true).await
}

/// DELETE /api/packages/apps/{id}?confirm=<id> — hide the app and close its
/// API for everyone; its data, schedules and running containers stay.
pub async fn uninstall_app(
    State(state): State<AppState>,
    PackageManager(user): PackageManager,
    Path(id): Path<String>,
    Query(q): Query<UninstallQuery>,
) -> AppResult<Json<Value>> {
    if q.confirm.as_deref() != Some(id.as_str()) {
        return Err(AppError::bad_request(
            "Uninstalling must be confirmed: pass confirm=<app id>.",
        ));
    }
    set_app(&state, &user, &id, false).await
}

async fn set_app(
    state: &AppState,
    user: &AuthUser,
    id: &str,
    installed: bool,
) -> AppResult<Json<Value>> {
    if !access::is_desktop_app(id) {
        return Err(AppError::NotFound("unknown application"));
    }
    let label = access::app_label(id);
    let action = if installed {
        "package.app_install"
    } else {
        "package.app_uninstall"
    };
    let log = crate::audit::AuditLog::new(action)
        .actor(user)
        .object("app", id.to_string())
        .path(label.to_string());
    let changed = match access::set_app_installed(&state.db, id, installed).await {
        Ok(changed) => changed,
        Err(e) => {
            crate::audit::record_with_events(
                &state.db,
                &state.events,
                log.failure().detail(e.public_message()),
            )
            .await;
            return Err(e);
        }
    };
    if changed {
        crate::audit::record_with_events(
            &state.db,
            &state.events,
            log.success().detail(if installed {
                "installed"
            } else {
                "uninstalled"
            }),
        )
        .await;
        // Every open desktop reloads its permissions and app list.
        state.events.publish(
            "access.changed",
            json!({"reason": if installed { "app_installed" } else { "app_uninstalled" }, "appId": id}),
        );
        state
            .events
            .publish("package.changed", json!({"reason": "app", "appId": id}));
    }
    Ok(Json(
        json!({ "ok": true, "changed": changed, "apps": apps_state(state).await }),
    ))
}

/// GET /api/packages/{id}
pub async fn package(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    access::app_surface(&state.db, &state, &user, "packages").await?;
    let manage = can_manage(&state, &user).await?;
    let (view, jobs) = state.packages.package(&id, manage).await?;
    Ok(Json(
        json!({ "package": view, "jobs": jobs, "host": state.packages.host }),
    ))
}

/// POST /api/packages/check — fetch and verify the catalog now.
pub async fn check(
    State(state): State<AppState>,
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
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
pub struct UninstallQuery {
    pub confirm: Option<String>,
}

/// DELETE /api/packages/{id}?confirm=<id> — remove every installed version
/// and its commands; the system's own copy (if any) is used again.
pub async fn uninstall(
    State(state): State<AppState>,
    PackageManager(user): PackageManager,
    Path(id): Path<String>,
    Query(q): Query<UninstallQuery>,
) -> AppResult<Json<Value>> {
    if q.confirm.as_deref() != Some(id.as_str()) {
        return Err(AppError::bad_request(
            "Uninstalling must be confirmed: pass confirm=<package id>.",
        ));
    }
    state.packages.uninstall(&user, &id).await?;
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    state.packages.delete_consumer(&user, id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunAppRequest {
    #[serde(default)]
    pub args: Vec<String>,
}

/// POST /api/packages/{id}/run — run an installed app-type package's command
/// and return its output (App Center → Apps → Open).
pub async fn run_package_app(
    State(state): State<AppState>,
    PackageManager(user): PackageManager,
    Path(id): Path<String>,
    Json(body): Json<RunAppRequest>,
) -> AppResult<Json<Value>> {
    access::app_surface(&state.db, &state, &user, "packages").await?;
    let output = state.packages.run_package_app(&id, body.args).await?;
    Ok(Json(
        serde_json::to_value(output).map_err(|e| AppError::internal(e.to_string()))?,
    ))
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
    PackageManager(user): PackageManager,
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
    PackageManager(user): PackageManager,
    Json(body): Json<GithubInstallRequest>,
) -> AppResult<Json<Value>> {
    if body.approve_source && !user.is_admin() {
        return Err(AppError::Forbidden(
            "Only an administrator can approve a new package source.",
        ));
    }
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
