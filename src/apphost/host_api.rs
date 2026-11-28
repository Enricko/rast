//! The host API: what an app's server may ask RAST for, over its own
//! `host.sock` (0600 in a 0700 folder), each request carrying
//! `Authorization: Bearer <RAST_APP_TOKEN>`. Version 1 (`/v1/…`):
//!
//! | Method | Path | Answer |
//! | --- | --- | --- |
//! | GET | `/v1/info` | `{hostApi, rastVersion, appId, version, dataDir, publicUrl, dockerEnabled, homesRoot, privatePaths}` (`privatePaths`: RAST's data, storage and settings folders, for an app to keep out of what it shows or runs) |
//! | GET | `/v1/users` | `{users:[{id, username, displayName, role, active, quotaBytes, groups:[id]}]}` |
//! | GET | `/v1/users/{id}` | `{user}` (404 when gone) |
//! | GET | `/v1/groups` | `{groups:[{id, name}]}` |
//! | GET | `/v1/users/{id}/apps/{appId}` | `{allowed, readOnly, installed}`: may the account open that RAST app |
//! | GET | `/v1/users/{id}/apps` | `{apps:[{id, name, description, opens}]}`: the apps the account may open now (built in and installed), in the desktop's order; `opens` as in an app window's manifest |
//! | POST | `/v1/events` | `{kind, scope, data}` or `{events:[{kind, scope, data}, …]}` (published in order) → `{delivered}`; each `kind` starts with the app's `eventPrefix`; `scope` is `{to:"all"\|"admins"}`, `{to:"user", userId}`, `{to:"userAndAdmins", userId}` or `{to:"users", userIds}` |
//! | POST | `/v1/audit` | `{action, userId?, username?, objectType?, objectId?, objectPath?, result?, detail?, ip?}`; `action` starts with the app's `eventPrefix` |
//! | GET | `/v1/terminal/status` | the root helper's status, or 503 `terminal_off` |
//! | POST | `/v1/terminal/run` | `{userId, command, timeoutSecs}` → `RunOutput` (`{exitCode, stdout, stderr, truncated, timedOut}`): one command as root through the terminal helper; 503 `terminal_off`, 409 `refused` (`detail`: the helper's code) |
//! | GET | `/v1/sandbox/{userId}` | `{allowed}`: whether the account has a Terminal sandbox |
//! | POST | `/v1/sandbox/run` | `{userId, command, timeoutSecs}` → `RunOutput`, in the account's sandbox (its home made ready first) |
//! | POST | `/v1/files/list` | `{userId, path}` → File Station's listing (first 500 by name) |
//! | POST | `/v1/files/read` | `{userId, path, maxBytes}` → `{encoding, content, truncated, …}` |
//! | POST | `/v1/files/search` | `{userId, path, query}` → `{items, total, truncated, …}` (first 100) |
//! | POST | `/v1/files/write` | `{userId, path, content}` → `{entry, existed}`; File Station's checks and quota, and live updates for open windows |
//! | POST | `/v1/files/mkdir` | `{userId, path}` → `{entry}` |
//! | GET, POST | `/v1/migrate/ai…` | AI Agent only: its data from RAST, once (see [`super::migrate`]) |
//!
//! File and command routes act as the account `userId`, with its rights.
//!
//! Later versions add routes; a route of a version never changes meaning.

use std::{os::unix::fs::PermissionsExt, sync::Arc};

use axum::{
    extract::{Path, Request, State},
    http::header,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use tokio::{net::UnixListener, sync::watch, task::JoinHandle};

use super::App;
use crate::{
    access,
    audit::{self, AuditLog},
    auth::{AuthUser, Role},
    error::{AppError, AppResult},
    events::EventScope,
    state::AppState,
};

/// Listen on the app's `host.sock` until the app stops.
pub async fn serve(
    state: AppState,
    app: Arc<App>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<JoinHandle<()>> {
    let path = app.host_socket();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let router = routes()
        .layer(middleware::from_fn(require_token))
        .layer(Extension(app))
        .with_state(state);
    Ok(tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        };
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!(%error, "app host API stopped");
        }
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/users", get(users))
        .route("/v1/users/{id}", get(user))
        .route("/v1/users/{id}/apps/{app_id}", get(user_app))
        .route("/v1/users/{id}/apps", get(user_apps))
        .route("/v1/groups", get(groups))
        .route("/v1/events", post(events))
        .route("/v1/audit", post(audit_entry))
        .route("/v1/terminal/status", get(terminal_status))
        .route("/v1/terminal/run", post(terminal_run))
        .route("/v1/sandbox/{user_id}", get(sandbox_allowed))
        .route("/v1/sandbox/run", post(sandbox_run))
        .route("/v1/files/list", post(files_list))
        .route("/v1/files/read", post(files_read))
        .route("/v1/files/search", post(files_search))
        .route("/v1/files/write", post(files_write))
        .route("/v1/files/mkdir", post(files_mkdir))
        .route("/v1/migrate/ai", get(super::migrate::dump))
        .route("/v1/migrate/ai/decrypt", post(super::migrate::decrypt))
        .route("/v1/migrate/ai/done", post(super::migrate::done))
}

async fn require_token(request: Request, next: Next) -> Response {
    let Some(app) = request.extensions().get::<Arc<App>>().cloned() else {
        return AppError::Unauthorized("no app").into_response();
    };
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(given.as_bytes(), app.token.as_bytes()) {
        return AppError::Unauthorized("the app token is missing or wrong").into_response();
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn info(State(state): State<AppState>, Extension(app): Extension<Arc<App>>) -> Json<Value> {
    let config = &state.config;
    Json(json!({
        "hostApi": 1,
        "rastVersion": crate::VERSION,
        "appId": app.app_id,
        "version": app.version,
        "dataDir": app.data_dir(),
        "publicUrl": config.public_url,
        "dockerEnabled": config.docker_enabled,
        "homesRoot": state.storage.homes_root,
        "privatePaths": private_paths(config),
    }))
}

/// Places on the server an app must keep out of what it shows or runs:
/// RAST's data and storage folders and its settings folder (real paths too).
pub fn private_paths(config: &crate::config::Config) -> Vec<std::path::PathBuf> {
    let mut out = vec![config.data_dir.clone(), config.storage_dir.clone()];
    if let Some(dir) = config
        .loaded_from
        .as_deref()
        .and_then(std::path::Path::parent)
    {
        if dir != std::path::Path::new("/") {
            out.push(dir.to_path_buf());
        }
    }
    for path in [&config.data_dir, &config.storage_dir] {
        if let Ok(real) = std::fs::canonicalize(path) {
            out.push(real);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn user_json(row: &sqlx::sqlite::SqliteRow, groups: Vec<i64>) -> Value {
    json!({
        "id": row.get::<i64, _>("id"),
        "username": row.get::<String, _>("username"),
        "displayName": row.get::<String, _>("display_name"),
        "role": row.get::<String, _>("role"),
        "active": row.get::<i64, _>("active") != 0,
        "quotaBytes": row.get::<i64, _>("quota_bytes"),
        "groups": groups,
    })
}

async fn memberships(state: &AppState) -> AppResult<Vec<(i64, i64)>> {
    Ok(
        sqlx::query_as::<_, (i64, i64)>("SELECT user_id, group_id FROM group_members")
            .fetch_all(&state.db)
            .await?,
    )
}

async fn users(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT id, username, display_name, role, active, quota_bytes FROM users
         ORDER BY username COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await?;
    let members = memberships(&state).await?;
    let users: Vec<Value> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get("id");
            let groups = members
                .iter()
                .filter(|(user, _)| *user == id)
                .map(|(_, group)| *group)
                .collect();
            user_json(row, groups)
        })
        .collect();
    Ok(Json(json!({ "users": users })))
}

async fn load_user(state: &AppState, id: i64) -> AppResult<sqlx::sqlite::SqliteRow> {
    sqlx::query(
        "SELECT id, username, display_name, role, active, quota_bytes FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("user not found"))
}

async fn user(State(state): State<AppState>, Path(id): Path<i64>) -> AppResult<Json<Value>> {
    let row = load_user(&state, id).await?;
    let groups = memberships(&state)
        .await?
        .into_iter()
        .filter(|(user, _)| *user == id)
        .map(|(_, group)| group)
        .collect();
    Ok(Json(json!({ "user": user_json(&row, groups) })))
}

async fn groups(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, name FROM groups ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await?;
    let groups: Vec<Value> = rows
        .into_iter()
        .map(|(id, name)| json!({"id": id, "name": name}))
        .collect();
    Ok(Json(json!({ "groups": groups })))
}

/// The account, as RAST's own checks see it (active accounts only).
pub async fn acting_user(state: &AppState, id: i64) -> AppResult<AuthUser> {
    let row = load_user(state, id).await?;
    if row.get::<i64, _>("active") == 0 {
        return Err(AppError::Forbidden("this account is turned off"));
    }
    Ok(AuthUser {
        id,
        username: row.get("username"),
        display_name: row.get("display_name"),
        role: Role::parse(&row.get::<String, _>("role")).unwrap_or(Role::User),
        session_id: String::new(),
        quota_bytes: row.get("quota_bytes"),
        must_change_password: false,
    })
}

/// The account behind a host API call (no session of its own).
fn host_user(id: i64, row: &sqlx::sqlite::SqliteRow) -> AuthUser {
    AuthUser {
        id,
        username: row.get("username"),
        display_name: row.get("display_name"),
        role: Role::parse(&row.get::<String, _>("role")).unwrap_or(Role::User),
        session_id: String::new(),
        quota_bytes: row.get("quota_bytes"),
        must_change_password: false,
    }
}

async fn user_apps(State(state): State<AppState>, Path(id): Path<i64>) -> AppResult<Json<Value>> {
    let row = load_user(&state, id).await?;
    let user = host_user(id, &row);
    let mut apps = Vec::new();
    if row.get::<i64, _>("active") != 0 {
        for app_id in access::app_ids() {
            if !access::app_installed(&state.db, &app_id).await {
                continue;
            }
            let window = state.apps.get(&app_id).map(|app| app.spec.window.clone());
            if window.as_ref().is_some_and(|w| w.admin_only) && !user.is_admin() {
                continue;
            }
            if !user.is_admin() && !access::app_access(&state.db, &user, &app_id).await?.allowed {
                continue;
            }
            apps.push(json!({
                "id": app_id,
                "name": access::app_label(&app_id),
                "description": access::app_description(&app_id),
                "opens": window.map(|w| w.opens).unwrap_or_default(),
            }));
        }
    }
    Ok(Json(json!({ "apps": apps })))
}

async fn user_app(
    State(state): State<AppState>,
    Path((id, app_id)): Path<(i64, String)>,
) -> AppResult<Json<Value>> {
    let row = load_user(&state, id).await?;
    let user = host_user(id, &row);
    let installed = access::app_installed(&state.db, &app_id).await;
    let active = row.get::<i64, _>("active") != 0;
    if !active || !installed {
        return Ok(Json(
            json!({"allowed": false, "readOnly": false, "installed": installed}),
        ));
    }
    if user.is_admin() {
        return Ok(Json(
            json!({"allowed": true, "readOnly": false, "installed": true}),
        ));
    }
    let grant = access::app_access(&state.db, &user, &app_id).await?;
    Ok(Json(json!({
        "allowed": grant.allowed,
        "readOnly": grant.read_only,
        "installed": true,
    })))
}

fn event_prefix(app: &App) -> AppResult<&str> {
    app.spec
        .server
        .as_ref()
        .map(|s| s.event_prefix.as_str())
        .ok_or(AppError::Forbidden("this app has no server"))
}

#[derive(Deserialize)]
#[serde(rename_all_fields = "camelCase", tag = "to")]
enum Scope {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "admins")]
    Admins,
    #[serde(rename = "user")]
    User { user_id: i64 },
    #[serde(rename = "userAndAdmins")]
    UserAndAdmins { user_id: i64 },
    #[serde(rename = "users")]
    Users { user_ids: Vec<i64> },
}

#[derive(Deserialize)]
struct EventBody {
    kind: String,
    scope: Scope,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Events {
    Many { events: Vec<EventBody> },
    One(EventBody),
}

async fn events(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
    Json(body): Json<Events>,
) -> AppResult<Json<Value>> {
    let prefix = event_prefix(&app)?;
    let list = match body {
        Events::Many { events } => events,
        Events::One(one) => vec![one],
    };
    if let Some(bad) = list
        .iter()
        .find(|e| !e.kind.starts_with(prefix) || e.kind.len() > 64)
    {
        return Err(AppError::bad_request(format!(
            "event kinds of this app start with {prefix:?} (not {:?})",
            bad.kind
        )));
    }
    let mut delivered = 0;
    for body in list {
        let scope = match body.scope {
            Scope::All => EventScope::Broadcast,
            Scope::Admins => EventScope::Admin,
            Scope::User { user_id } => EventScope::User(user_id),
            Scope::UserAndAdmins { user_id } => EventScope::UserAndAdmin(user_id),
            Scope::Users { user_ids } => EventScope::Users(user_ids),
        };
        delivered += state.events.publish_scoped(&body.kind, scope, body.data);
    }
    Ok(Json(json!({ "delivered": delivered })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct AuditBody {
    action: String,
    user_id: Option<i64>,
    username: String,
    object_type: String,
    object_id: String,
    object_path: String,
    result: String,
    detail: String,
    ip: String,
}

async fn audit_entry(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
    Json(body): Json<AuditBody>,
) -> AppResult<Json<Value>> {
    let prefix = event_prefix(&app)?;
    if !body.action.starts_with(prefix) || body.action.len() > 64 {
        return Err(AppError::bad_request(format!(
            "audit actions of this app start with {prefix:?}"
        )));
    }
    let mut log = AuditLog::new(body.action);
    log.user_id = body.user_id;
    log.username = body.username;
    log.object_type = body.object_type;
    log.object_id = body.object_id;
    log.object_path = body.object_path;
    log.detail = body.detail;
    log.ip = body.ip;
    if !body.result.is_empty() {
        log.result = body.result;
    }
    audit::record_with_events(&state.db, &state.events, log).await;
    Ok(Json(json!({ "recorded": true })))
}

// ------------------------------------------------------------- commands

fn terminal_error(error: crate::terminal::TerminalError) -> Response {
    use crate::terminal::TerminalError;
    let (status, code, detail, message) = match error {
        TerminalError::Off => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "terminal_off",
            String::new(),
            TerminalError::Off.to_string(),
        ),
        TerminalError::Refused { code, message } => {
            (axum::http::StatusCode::CONFLICT, "refused", code, message)
        }
        TerminalError::Io(message) => (
            axum::http::StatusCode::BAD_GATEWAY,
            "terminal_io",
            String::new(),
            message,
        ),
    };
    (
        status,
        Json(json!({"error": {"code": code, "detail": detail, "message": message}})),
    )
        .into_response()
}

async fn terminal_status() -> Response {
    match crate::terminal::TerminalClient::default().status().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => terminal_error(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunBody {
    user_id: i64,
    command: String,
    timeout_secs: u32,
}

fn checked_run(body: &RunBody) -> AppResult<u32> {
    if body.command.trim().is_empty() || body.command.len() > crate::terminal::MAX_RUN_COMMAND {
        return Err(AppError::bad_request("the command is empty or too long"));
    }
    Ok(body.timeout_secs.clamp(1, crate::terminal::MAX_RUN_SECS))
}

async fn terminal_run(State(state): State<AppState>, Json(body): Json<RunBody>) -> Response {
    let secs = match checked_run(&body) {
        Ok(secs) => secs,
        Err(error) => return error.into_response(),
    };
    let user = match acting_user(&state, body.user_id).await {
        Ok(user) => user,
        Err(error) => return error.into_response(),
    };
    let name = user.username.to_ascii_lowercase();
    match crate::terminal::TerminalClient::default()
        .run(user.id, &name, &body.command, secs)
        .await
    {
        Ok(output) => Json(output).into_response(),
        Err(error) => terminal_error(error),
    }
}

async fn sandbox_allowed(State(state): State<AppState>, Path(user_id): Path<i64>) -> Json<Value> {
    let allowed = state.config.docker_enabled && crate::sandbox::allowed(&state.db, user_id).await;
    Json(json!({ "allowed": allowed }))
}

async fn sandbox_run(State(state): State<AppState>, Json(body): Json<RunBody>) -> Response {
    let secs = match checked_run(&body) {
        Ok(secs) => secs,
        Err(error) => return error.into_response(),
    };
    let user = match acting_user(&state, body.user_id).await {
        Ok(user) => user,
        Err(error) => return error.into_response(),
    };
    if !(state.config.docker_enabled && crate::sandbox::allowed(&state.db, user.id).await) {
        return AppError::Forbidden("this account has no Terminal sandbox").into_response();
    }
    if let Err(error) = crate::sandbox::ready_home(&state.db, &state.storage, &user).await {
        return error.into_response();
    }
    let name = user.username.to_ascii_lowercase();
    match crate::sandbox::client(&state.config)
        .run(user.id, &name, &body.command, secs)
        .await
    {
        Ok(output) => Json(output).into_response(),
        Err(error) => terminal_error(error),
    }
}

// ---------------------------------------------------------------- files

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileBody {
    user_id: i64,
    #[serde(default)]
    path: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    max_bytes: Option<u64>,
}

async fn files_list(
    State(state): State<AppState>,
    Json(body): Json<FileBody>,
) -> AppResult<Json<Value>> {
    use crate::storage::ops;
    let user = acting_user(&state, body.user_id).await?;
    let listing = ops::list(
        &state.db,
        &state.storage,
        &user,
        &body.path,
        None,
        "name",
        "asc",
        1,
        500,
    )
    .await?;
    Ok(Json(json!(listing)))
}

async fn files_read(
    State(state): State<AppState>,
    Json(body): Json<FileBody>,
) -> AppResult<Json<Value>> {
    use crate::storage::ops;
    let user = acting_user(&state, body.user_id).await?;
    let preview =
        ops::text_preview(&state.db, &state.storage, &user, &body.path, body.max_bytes).await?;
    Ok(Json(json!(preview)))
}

async fn files_search(
    State(state): State<AppState>,
    Json(body): Json<FileBody>,
) -> AppResult<Json<Value>> {
    use crate::storage::ops;
    let user = acting_user(&state, body.user_id).await?;
    let path = if body.path.is_empty() {
        "/"
    } else {
        &body.path
    };
    let found = ops::search(
        &state.db,
        &state.storage,
        &user,
        path,
        &body.query,
        None,
        1,
        100,
    )
    .await?;
    Ok(Json(json!(found)))
}

/// The most an app writes in one file through the host.
const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;

async fn files_write(
    State(state): State<AppState>,
    Json(body): Json<FileBody>,
) -> AppResult<Json<Value>> {
    use crate::storage::ops;
    use tokio::io::AsyncWriteExt;
    let user = acting_user(&state, body.user_id).await?;
    if body.content.len() > MAX_WRITE_BYTES {
        return Err(AppError::PayloadTooLarge(
            "files written by apps are at most 10 MB",
        ));
    }
    let path = body.path.trim_end_matches('/');
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) if !name.is_empty() => (if dir.is_empty() { "/" } else { dir }, name),
        _ => {
            return Err(AppError::bad_request(
                "give the file's full path, like /notes/todo.txt",
            ))
        }
    };
    let existed = ops::stat(&state.db, &state.storage, &user, path)
        .await
        .is_ok();
    let size = body.content.len() as u64;
    let mut plan =
        ops::plan_upload(&state.db, &state.storage, &user, dir, name, true, size).await?;
    if let Err(error) = plan.file.write_all(body.content.as_bytes()).await {
        ops::abort_upload(&state.db, &user, plan).await;
        return Err(AppError::internal(format!(
            "cannot write the file: {error}"
        )));
    }
    let entry = ops::commit_upload(&state.db, &state.storage, &user, plan, size).await?;
    crate::realtime::publish_commit(
        &state.events,
        &state.db,
        &user,
        "upload",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        true,
    )
    .await;
    Ok(Json(json!({ "entry": entry, "existed": existed })))
}

async fn files_mkdir(
    State(state): State<AppState>,
    Json(body): Json<FileBody>,
) -> AppResult<Json<Value>> {
    use crate::storage::ops;
    let user = acting_user(&state, body.user_id).await?;
    let entry = ops::mkdir(&state.db, &state.storage, &user, &body.path).await?;
    crate::realtime::publish_commit(
        &state.events,
        &state.db,
        &user,
        "mkdir",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        false,
    )
    .await;
    Ok(Json(json!({ "entry": entry })))
}
