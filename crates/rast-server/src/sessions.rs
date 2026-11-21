//! Sessions: the devices an account is signed in on. Everyone sees and ends
//! their own (`/api/auth/sessions`); administrators see the sessions of
//! accounts they manage and sign them out everywhere (`/api/users/{id}/sessions`).
//! A session is named by a random public id; its token's hash never leaves the
//! server.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::api::{ApiError, ApiResult, admin_signed};
use crate::auth::{self, Signed};
use crate::db::{self, User};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDto {
    id: String,
    /// The session this request came with.
    current: bool,
    /// "Chrome on Windows".
    device: String,
    ip: String,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
}

/// `user_id`'s sessions; `current` is the token hash of the caller's session.
async fn list(state: &AppState, user_id: i64, current: Option<&str>) -> ApiResult<Json<Value>> {
    let sessions: Vec<SessionDto> = db::sessions_of(&state.pool, user_id)
        .await?
        .into_iter()
        .map(|session| SessionDto {
            current: current == Some(session.token_hash.as_str()),
            device: auth::device(&session.user_agent),
            id: session.id,
            ip: session.ip,
            created_at: session.created_at,
            last_seen_at: session.last_seen_at,
            expires_at: session.expires_at,
        })
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

// --- your own -----------------------------------------------------------------------

pub(crate) async fn mine(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let signed = auth::require(&state, &headers).await?;
    list(&state, signed.user.id, Some(&signed.session)).await
}

/// End one of your sessions, such as a browser left signed in elsewhere.
pub(crate) async fn end_mine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let Signed {
        user,
        session,
        session_id,
    } = auth::require(&state, &headers).await?;
    if id == session_id {
        return Err(ApiError::bad_request(
            "This is the session you are using. Sign out to end it.",
        ));
    }
    if !db::delete_session_by_id(&state.pool, user.id, &id).await? {
        return Err(ApiError::not_found_with("That session has already ended."));
    }
    db::audit_from(
        &state.pool,
        Some(user.id),
        "session.ended",
        &user.username,
        &auth::client_ip(&headers),
    )
    .await;
    list(&state, user.id, Some(&session)).await
}

/// Sign out everywhere except here.
pub(crate) async fn end_my_others(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let Signed { user, session, .. } = auth::require(&state, &headers).await?;
    db::delete_sessions_of(&state.pool, user.id, Some(&session)).await?;
    db::audit_from(
        &state.pool,
        Some(user.id),
        "session.ended_others",
        &user.username,
        &auth::client_ip(&headers),
    )
    .await;
    list(&state, user.id, Some(&session)).await
}

// --- an account's, for administrators -------------------------------------------------

/// The account administrator `signed` may look at: themselves or one they manage.
async fn target(state: &AppState, signed: &Signed, id: i64) -> ApiResult<User> {
    let target = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found_with("That account no longer exists."))?;
    if target.id != signed.user.id && !signed.user.role.may_manage(target.role) {
        return Err(ApiError::forbidden(
            "Only a Super Admin can see the sessions of administrators.",
        ));
    }
    Ok(target)
}

pub(crate) async fn of_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let signed = admin_signed(&state, &headers).await?;
    let target = target(&state, &signed, id).await?;
    list(&state, target.id, Some(&signed.session)).await
}

/// Sign an account out everywhere.
pub(crate) async fn end_all_of_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let signed = admin_signed(&state, &headers).await?;
    let target = target(&state, &signed, id).await?;
    if target.id == signed.user.id {
        return Err(ApiError::forbidden(
            "To sign out your other sessions, use Profile.",
        ));
    }
    db::delete_sessions_of(&state.pool, target.id, None).await?;
    db::audit_from(
        &state.pool,
        Some(signed.user.id),
        "user.signed_out",
        &target.username,
        &auth::client_ip(&headers),
    )
    .await;
    list(&state, target.id, None).await
}
