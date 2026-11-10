//! User administration endpoints (admin only).

use axum::{
    extract::{Path, Query, State},
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{
    audit::{self, AuditLog},
    auth::{AdminUser, Role},
    db,
    error::{AppError, AppResult},
    state::AppState,
    users::{self, CreateUserRequest, UserDto},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route(
            "/api/users/{id}",
            patch(update_user).delete(deactivate_user),
        )
        .route("/api/users/{id}/password", post(admin_set_password))
        .route(
            "/api/users/{id}/sessions",
            get(user_sessions).delete(revoke_user_sessions),
        )
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page_camel: Option<i64>,
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPage {
    pub items: Vec<UserDto>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
}

/// GET /api/users
pub async fn list_users(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<UserPage>> {
    let page = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.or(q.per_page_camel).unwrap_or(25).clamp(1, 100);
    let (rows, total) = users::list(&state.db, q.q.as_deref(), page, per_page).await?;
    Ok(Json(UserPage {
        items: rows.into_iter().map(UserDto::from).collect(),
        page,
        per_page,
        total,
    }))
}

/// POST /api/users
pub async fn create_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<CreateUserRequest>,
) -> AppResult<Json<UserDto>> {
    let role = match body.role.as_deref() {
        None | Some("user") => Role::User,
        Some("admin") => Role::Admin,
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "invalid role {other:?}; expected admin or user"
            )))
        }
    };
    let user = users::create(
        &state.db,
        &state.config,
        &body.username,
        &body.password,
        role,
        body.display_name.as_deref(),
        body.quota_bytes,
    )
    .await?;
    audit::record(
        &state.db,
        AuditLog::new("user.create")
            .actor(&admin.0)
            .object("user", user.id.to_string())
            .path(&user.username)
            .success()
            .detail(format!("role={}, quota={}", user.role, user.quota_bytes)),
    )
    .await;
    Ok(Json(UserDto::from(user)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub active: Option<bool>,
    pub quota_bytes: Option<i64>,
    pub must_change_password: Option<bool>,
}

/// PATCH /api/users/{id}
pub async fn update_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> AppResult<Json<UserDto>> {
    let role = match body.role.as_deref() {
        None => None,
        Some("admin") => Some(Role::Admin),
        Some("user") => Some(Role::User),
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "invalid role {other:?}; expected admin or user"
            )))
        }
    };
    if body.active == Some(false) && id == admin.0.id {
        return Err(AppError::bad_request(
            "you cannot deactivate your own account",
        ));
    }
    if role == Some(Role::User) && id == admin.0.id {
        return Err(AppError::bad_request("you cannot demote your own account"));
    }
    let updated = users::update(
        &state.db,
        id,
        body.display_name.as_deref(),
        role,
        body.active,
        body.quota_bytes,
        body.must_change_password,
    )
    .await?;
    audit::record(
        &state.db,
        AuditLog::new("user.update")
            .actor(&admin.0)
            .object("user", id.to_string())
            .path(&updated.username)
            .success()
            .detail(format!(
                "role={}, active={}, quota={}",
                updated.role, updated.active, updated.quota_bytes
            )),
    )
    .await;
    Ok(Json(UserDto::from(updated)))
}

/// DELETE /api/users/{id} — deactivates the account, never deletes data.
pub async fn deactivate_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    if id == admin.0.id {
        return Err(AppError::bad_request(
            "you cannot deactivate your own account",
        ));
    }
    let target = users::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound("user not found"))?;
    users::deactivate(&state.db, id).await?;
    audit::record(
        &state.db,
        AuditLog::new("user.deactivate")
            .actor(&admin.0)
            .object("user", id.to_string())
            .path(&target.username)
            .success(),
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "account deactivated; data has been preserved"
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminPasswordRequest {
    pub password: String,
    #[serde(default)]
    pub must_change_password: Option<bool>,
}

/// POST /api/users/{id}/password
pub async fn admin_set_password(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    Json(body): Json<AdminPasswordRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let target = users::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound("user not found"))?;
    users::set_password(
        &state.db,
        id,
        &body.password,
        body.must_change_password.unwrap_or(true),
        None,
    )
    .await?;
    audit::record(
        &state.db,
        AuditLog::new("user.password_reset")
            .actor(&admin.0)
            .object("user", id.to_string())
            .path(&target.username)
            .success(),
    )
    .await;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/users/{id}/sessions
pub async fn user_sessions(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, ip, user_agent, created_at, last_seen_at, expires_at
         FROM sessions WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
         ORDER BY last_seen_at DESC",
    )
    .bind(id)
    .bind(db::now_ms())
    .fetch_all(&state.db)
    .await?;
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "ip": r.get::<String, _>("ip"),
                "userAgent": r.get::<String, _>("user_agent"),
                "createdAt": r.get::<i64, _>("created_at"),
                "lastSeenAt": r.get::<i64, _>("last_seen_at"),
                "expiresAt": r.get::<i64, _>("expires_at"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"items": items})))
}

/// DELETE /api/users/{id}/sessions
pub async fn revoke_user_sessions(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let target = users::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound("user not found"))?;
    let n = crate::auth::session::revoke_all_for_user(&state.db, id, None).await?;
    audit::record(
        &state.db,
        AuditLog::new("user.sessions_revoke")
            .actor(&admin.0)
            .object("user", id.to_string())
            .path(&target.username)
            .success()
            .detail(format!("{n} sessions revoked")),
    )
    .await;
    Ok(Json(serde_json::json!({"ok": true, "revoked": n})))
}
