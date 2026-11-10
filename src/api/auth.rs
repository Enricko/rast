//! Authentication, session and first-run setup endpoints.

use std::sync::OnceLock;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{
    audit::{self, AuditLog},
    auth::{self, csrf, password, session, AuthUser, PeerAddr, Role},
    db,
    error::{AppError, AppResult},
    state::AppState,
    users::{self, UserDto},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/password", post(change_password))
        .route(
            "/api/auth/sessions",
            get(list_sessions).delete(revoke_all_sessions),
        )
        .route("/api/auth/sessions/{id}", delete(revoke_session))
}

fn peer_ip(peer: PeerAddr) -> std::net::IpAddr {
    peer.0.ip()
}

/// Constant-time-ish dummy hash so unknown usernames cost the same as known
/// ones (mitigates username enumeration by timing).
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        password::hash_password("Dummy-Password-For-Timing-1!")
            .unwrap_or_else(|_| "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into())
    })
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub user: UserDto,
    pub csrf_token: String,
    pub expires_at: i64,
}

/// POST /api/auth/login
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<LoginRequest>,
) -> AppResult<Response> {
    let ip = auth::client_ip(&headers, peer_ip(peer), &state.config);
    let ip_str = ip.to_string();
    let username_raw = body.username.trim().to_string();

    // Cheap input sanity checks before any expensive work.
    if username_raw.is_empty() || username_raw.len() > 64 || body.password.len() > 512 {
        return Err(AppError::Unauthorized("invalid username or password"));
    }

    // IP-based throttle first: protects against password spraying.
    match state.login_ip_limiter.check(&ip_str) {
        auth::rate_limit::Decision::Allowed => {}
        auth::rate_limit::Decision::Limited { retry_after_secs } => {
            return Err(AppError::TooManyRequests {
                retry_after_secs,
                message: "too many login attempts, try again later",
            })
        }
    }
    let user_key = username_raw.to_ascii_lowercase();
    match state.login_user_limiter.check(&user_key) {
        auth::rate_limit::Decision::Allowed => {}
        auth::rate_limit::Decision::Limited { retry_after_secs } => {
            return Err(AppError::TooManyRequests {
                retry_after_secs,
                message: "too many login attempts for this account, try again later",
            })
        }
    }

    // Persistent per-account lockout after repeated failures.
    let row = sqlx::query(
        "SELECT id, username, display_name, password_hash, role, active, quota_bytes,
                must_change_password, failed_attempts, locked_until
         FROM users WHERE username = ?",
    )
    .bind(&username_raw)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        // Burn comparable CPU for unknown users.
        let _ =
            password::verify_password_async(body.password.clone(), dummy_hash().to_string()).await;
        audit::record(
            &state.db,
            AuditLog::new("auth.login")
                .anonymous(&username_raw)
                .failure()
                .detail("unknown username")
                .ip(&ip_str),
        )
        .await;
        return Err(AppError::Unauthorized("invalid username or password"));
    };

    let user_id: i64 = row.get("id");
    let username: String = row.get("username");
    let active: bool = row.get("active");
    let locked_until: i64 = row.get("locked_until");
    let failed_attempts: i64 = row.get("failed_attempts");
    let stored_hash: String = row.get("password_hash");
    let now = db::now_ms();

    if !active {
        audit::record(
            &state.db,
            AuditLog::new("auth.login")
                .object("user", user_id.to_string())
                .anonymous(&username)
                .denied()
                .detail("account disabled")
                .ip(&ip_str),
        )
        .await;
        return Err(AppError::Unauthorized("account is disabled"));
    }
    if locked_until > now {
        let retry = ((locked_until - now) as u64).div_ceil(1000).max(1);
        return Err(AppError::TooManyRequests {
            retry_after_secs: retry,
            message: "account temporarily locked after too many failed attempts",
        });
    }

    let ok = password::verify_password_async(body.password.clone(), stored_hash.clone()).await?;
    if !ok {
        let attempts = failed_attempts + 1;
        let lock_until = if attempts >= 5 { now + 15 * 60_000 } else { 0 };
        sqlx::query("UPDATE users SET failed_attempts = ?, locked_until = ? WHERE id = ?")
            .bind(if lock_until > 0 { 0 } else { attempts })
            .bind(lock_until)
            .bind(user_id)
            .execute(&state.db)
            .await?;
        sqlx::query(
            "INSERT INTO login_attempts (username, ip, success, created_at) VALUES (?, ?, 0, ?)",
        )
        .bind(&username)
        .bind(&ip_str)
        .bind(now)
        .execute(&state.db)
        .await?;
        audit::record(
            &state.db,
            AuditLog::new("auth.login")
                .object("user", user_id.to_string())
                .anonymous(&username)
                .failure()
                .detail(if lock_until > 0 {
                    "invalid password; account locked for 15 minutes"
                } else {
                    "invalid password"
                })
                .ip(&ip_str),
        )
        .await;
        if lock_until > 0 {
            return Err(AppError::TooManyRequests {
                retry_after_secs: 900,
                message: "account temporarily locked after too many failed attempts",
            });
        }
        return Err(AppError::Unauthorized("invalid username or password"));
    }

    // Success: reset counters, transparently upgrade weak hashes.
    let new_hash = if password::needs_rehash(&stored_hash) {
        password::hash_password_async(body.password.clone())
            .await
            .ok()
    } else {
        None
    };
    sqlx::query(
        "UPDATE users SET failed_attempts = 0, locked_until = 0, last_login_at = ?,
                password_hash = COALESCE(?, password_hash) WHERE id = ?",
    )
    .bind(now)
    .bind(new_hash)
    .bind(user_id)
    .execute(&state.db)
    .await?;
    sqlx::query(
        "INSERT INTO login_attempts (username, ip, success, created_at) VALUES (?, ?, 1, ?)",
    )
    .bind(&username)
    .bind(&ip_str)
    .bind(now)
    .execute(&state.db)
    .await?;
    state.login_user_limiter.reset(&user_key);
    state.login_ip_limiter.reset(&ip_str);

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let new_session = session::create(
        &state.db,
        user_id,
        state.config.session_ttl_hours,
        &ip_str,
        &user_agent,
    )
    .await?;
    let csrf_token = csrf::token(&state.secrets, &new_session.session_id);

    audit::record(
        &state.db,
        AuditLog::new("auth.login")
            .object("user", user_id.to_string())
            .anonymous(&username)
            .success()
            .ip(&ip_str),
    )
    .await;

    let record = users::find_by_id(&state.db, user_id)
        .await?
        .ok_or_else(|| AppError::internal("user vanished during login"))?;
    let cookie = auth::session_cookie(
        &new_session.token,
        state.config.session_ttl_hours,
        state.config.secure_cookies,
    );
    let body = LoginResponse {
        user: UserDto::from(record),
        csrf_token,
        expires_at: new_session.expires_at,
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| AppError::internal("invalid cookie"))?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeResponse {
    pub user: UserDto,
    pub csrf_token: String,
    pub session_expires_at: i64,
    pub features: Features,
    pub server_time: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    pub docker: bool,
    pub apps: bool,
    pub backup: bool,
    pub monitor: bool,
    pub activity_logs: bool,
    pub file_station: bool,
}

/// GET /api/auth/me
pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<MeResponse>> {
    let record = users::find_by_id(&state.db, user.id)
        .await?
        .ok_or(AppError::Unauthorized("account no longer exists"))?;
    let expires_at: i64 =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id = ? AND revoked_at IS NULL")
            .bind(&user.session_id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_else(|| db::now_ms() + state.config.session_ttl_hours * 3_600_000);
    Ok(Json(MeResponse {
        csrf_token: csrf::token(&state.secrets, &user.session_id),
        session_expires_at: expires_at,
        features: Features {
            docker: state.docker.enabled,
            apps: state.docker.enabled,
            backup: true,
            monitor: true,
            activity_logs: true,
            file_station: true,
        },
        server_time: db::now_ms(),
        user: UserDto::from(record),
    }))
}

/// POST /api/auth/logout
pub async fn logout(State(state): State<AppState>, user: AuthUser) -> AppResult<Response> {
    session::revoke(&state.db, &user.session_id).await?;
    audit::record(
        &state.db,
        AuditLog::new("auth.logout").actor(&user).success(),
    )
    .await;
    let mut response = Json(serde_json::json!({"ok": true})).into_response();
    for cookie in auth::clear_cookies(state.config.secure_cookies) {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// POST /api/auth/password
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let ok = users::verify_password(&state.db, user.id, &body.current_password).await?;
    if !ok {
        audit::record(
            &state.db,
            AuditLog::new("auth.password_change").actor(&user).denied(),
        )
        .await;
        return Err(AppError::Unauthorized("current password is incorrect"));
    }
    if body.new_password == body.current_password {
        return Err(AppError::bad_request(
            "new password must differ from the current password",
        ));
    }
    users::set_password(
        &state.db,
        user.id,
        &body.new_password,
        false,
        Some(&user.session_id),
    )
    .await?;
    audit::record(
        &state.db,
        AuditLog::new("auth.password_change").actor(&user).success(),
    )
    .await;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    pub id: String,
    pub ip: String,
    pub user_agent: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub current: bool,
}

/// GET /api/auth/sessions
pub async fn list_sessions(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Vec<SessionDto>>> {
    let rows = sqlx::query(
        "SELECT id, ip, user_agent, created_at, last_seen_at, expires_at
         FROM sessions WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
         ORDER BY last_seen_at DESC",
    )
    .bind(user.id)
    .bind(db::now_ms())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let id: String = r.get("id");
                SessionDto {
                    current: id == user.session_id,
                    id,
                    ip: r.get("ip"),
                    user_agent: r.get("user_agent"),
                    created_at: r.get("created_at"),
                    last_seen_at: r.get("last_seen_at"),
                    expires_at: r.get("expires_at"),
                }
            })
            .collect(),
    ))
}

/// DELETE /api/auth/sessions/{id}
pub async fn revoke_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_optional(&state.db)
        .await?;
    match owner {
        Some(owner_id) if owner_id == user.id || user.is_admin() => {}
        _ => return Err(AppError::NotFound("session not found")),
    }
    session::revoke(&state.db, &session_id).await?;
    audit::record(
        &state.db,
        AuditLog::new("auth.session_revoke")
            .actor(&user)
            .object("session", session_id)
            .success(),
    )
    .await;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/auth/sessions — revoke every other session of the current user.
pub async fn revoke_all_sessions(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    let n = session::revoke_all_for_user(&state.db, user.id, Some(&user.session_id)).await?;
    audit::record(
        &state.db,
        AuditLog::new("auth.session_revoke_all")
            .actor(&user)
            .success()
            .detail(format!("{n} sessions revoked")),
    )
    .await;
    Ok(Json(serde_json::json!({"ok": true, "revoked": n})))
}

// ---------------------------------------------------------------------------
// First-run setup
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    pub setup_required: bool,
    pub token_required: bool,
    pub version: &'static str,
    pub product: &'static str,
}

/// GET /api/setup/status
pub async fn setup_status(State(state): State<AppState>) -> Json<SetupStatus> {
    Json(SetupStatus {
        setup_required: !db::has_admin(&state.db).await,
        token_required: state.config.setup_token_required,
        version: crate::VERSION,
        product: crate::NAME,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
    #[serde(default)]
    pub token: Option<String>,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// POST /api/setup — create the first administrator exactly once.
///
/// This endpoint is disabled as soon as an active admin exists. When
/// `setup_token_required` is set (default) the operator must first run
/// `rast setup-token` on the server and paste the printed token.
pub async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<SetupRequest>,
) -> AppResult<Response> {
    if db::has_admin(&state.db).await {
        return Err(AppError::Forbidden("setup has already been completed"));
    }
    let ip = auth::client_ip(&headers, peer_ip(peer), &state.config).to_string();
    if state.config.setup_token_required {
        let provided = body.token.as_deref().unwrap_or("").trim().to_string();
        if provided.is_empty() {
            return Err(AppError::Forbidden(
                "a setup token is required; run `rast setup-token` on the server",
            ));
        }
        let expires: Option<i64> = db::get_setting(&state.db, "setup_token_expires")
            .await
            .and_then(|v| v.parse().ok());
        let stored_hash = db::get_setting(&state.db, "setup_token_hash").await;
        let valid = match (stored_hash, expires) {
            (Some(hash), Some(exp)) if exp > db::now_ms() => {
                let provided_hash = hex::encode(Sha256::digest(provided.as_bytes()));
                use subtle::ConstantTimeEq;
                provided_hash.as_bytes().ct_eq(hash.as_bytes()).into()
            }
            _ => false,
        };
        if !valid {
            audit::record(
                &state.db,
                AuditLog::new("setup")
                    .anonymous(&body.username)
                    .denied()
                    .detail("invalid or expired setup token")
                    .ip(&ip),
            )
            .await;
            return Err(AppError::Forbidden(
                "invalid or expired setup token; generate a new one with `rast setup-token`",
            ));
        }
    }
    let user = users::create(
        &state.db,
        &state.config,
        &body.username,
        &body.password,
        Role::Admin,
        body.display_name.as_deref(),
        Some(0),
    )
    .await?;
    // Invalidate the token: one-shot.
    let _ = db::set_setting(&state.db, "setup_token_hash", "").await;
    let _ = db::set_setting(&state.db, "setup_token_expires", "0").await;
    audit::record(
        &state.db,
        AuditLog::new("setup")
            .object("user", user.id.to_string())
            .anonymous(&user.username)
            .success()
            .detail("first administrator created via setup endpoint")
            .ip(&ip),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "user": UserDto::from(user),
            "message": "administrator created; sign in to continue"
        })),
    )
        .into_response())
}

/// Issue a one-shot setup token (used by `rast setup-token`).
pub async fn issue_setup_token(pool: &sqlx::SqlitePool) -> AppResult<String> {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    let token = format!("rast_setup_{}", hex::encode(bytes));
    let hash = hex::encode(Sha256::digest(token.as_bytes()));
    db::set_setting(pool, "setup_token_hash", &hash).await?;
    db::set_setting(
        pool,
        "setup_token_expires",
        &(db::now_ms() + 3_600_000).to_string(),
    )
    .await?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_token_hash_is_not_reversible() {
        let token = "rast_setup_abcdef";
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains(token));
    }
}
