//! The host API: what an app's server may ask RAST for, over its own
//! `host.sock` (0600 in a 0700 folder), each request carrying
//! `Authorization: Bearer <RAST_APP_TOKEN>`. Version 1 (`/v1/…`):
//!
//! | Method | Path | Answer |
//! | --- | --- | --- |
//! | GET | `/v1/info` | `{hostApi, rastVersion, appId, version, dataDir}` |
//! | GET | `/v1/users` | `{users:[{id, username, displayName, role, active, quotaBytes, groups:[id]}]}` |
//! | GET | `/v1/users/{id}` | `{user}` (404 when gone) |
//! | GET | `/v1/groups` | `{groups:[{id, name}]}` |
//! | GET | `/v1/users/{id}/apps/{appId}` | `{allowed, readOnly, installed}`: may the account open that RAST app |
//! | POST | `/v1/events` | `{kind, scope, data}` → `{delivered}`; `kind` starts with the app's `eventPrefix`; `scope` is `{to:"all"\|"admins"}`, `{to:"user", userId}`, `{to:"userAndAdmins", userId}` or `{to:"users", userIds}` |
//! | POST | `/v1/audit` | `{action, userId?, username?, objectType?, objectId?, objectPath?, result?, detail?, ip?}`; `action` starts with the app's `eventPrefix` |
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
        .route("/v1/groups", get(groups))
        .route("/v1/events", post(events))
        .route("/v1/audit", post(audit_entry))
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

async fn info(Extension(app): Extension<Arc<App>>) -> Json<Value> {
    Json(json!({
        "hostApi": 1,
        "rastVersion": crate::VERSION,
        "appId": app.app_id,
        "version": app.version,
        "dataDir": app.data_dir(),
    }))
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

async fn user_app(
    State(state): State<AppState>,
    Path((id, app_id)): Path<(i64, String)>,
) -> AppResult<Json<Value>> {
    let row = load_user(&state, id).await?;
    let user = AuthUser {
        id,
        username: row.get("username"),
        display_name: row.get("display_name"),
        role: Role::parse(&row.get::<String, _>("role")).unwrap_or(Role::User),
        session_id: String::new(),
        quota_bytes: row.get("quota_bytes"),
        must_change_password: false,
    };
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

async fn events(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
    Json(body): Json<EventBody>,
) -> AppResult<Json<Value>> {
    let prefix = event_prefix(&app)?;
    if !body.kind.starts_with(prefix) || body.kind.len() > 64 {
        return Err(AppError::bad_request(format!(
            "event kinds of this app start with {prefix:?}"
        )));
    }
    let scope = match body.scope {
        Scope::All => EventScope::Broadcast,
        Scope::Admins => EventScope::Admin,
        Scope::User { user_id } => EventScope::User(user_id),
        Scope::UserAndAdmins { user_id } => EventScope::UserAndAdmin(user_id),
        Scope::Users { user_ids } => EventScope::Users(user_ids),
    };
    let delivered = state.events.publish_scoped(&body.kind, scope, body.data);
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
