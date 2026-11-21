//! `/api` routes. Errors are `{"error": "<sentence for the user>"}`.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Signed};
use crate::db::{self, NewUser, User, UserChange};
use crate::{AppState, Role};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/password", post(change_password))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", patch(update_user).delete(delete_user))
        .layer(middleware::from_fn(guard))
        .with_state(state)
}

/// Require `X-RAST: 1` on every request that changes state (see the crate docs)
/// and keep API answers out of caches.
async fn guard(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    let marked = request
        .headers()
        .get("x-rast")
        .is_some_and(|value| value == "1");
    if !safe && !marked {
        return ApiError::forbidden("This request is missing the RAST header.").into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// --- errors -------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "That account no longer exists.")
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    fn too_many(seconds: u64) -> Self {
        Self {
            retry_after: Some(seconds),
            ..Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many attempts. Try again in {}.", wait_text(seconds)),
            )
        }
    }
    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        eprintln!("rast-server: {}", detail.into());
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The RAST server hit an error. Try again.",
        )
    }
}

fn wait_text(seconds: u64) -> String {
    if seconds < 90 {
        format!("{seconds} seconds")
    } else {
        format!("{} minutes", seconds.div_ceil(60))
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::internal(format!("database: {error}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.message }))).into_response();
        if let Some(seconds) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

type ApiResult<T> = Result<T, ApiError>;

// --- shapes -------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserDto {
    id: i64,
    username: String,
    display_name: String,
    role: Role,
    disabled: bool,
    created_at: i64,
    last_login_at: Option<i64>,
}

impl From<&User> for UserDto {
    fn from(user: &User) -> Self {
        Self {
            id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            role: user.role,
            disabled: user.disabled,
            created_at: user.created_at,
            last_login_at: user.last_login_at,
        }
    }
}

fn with_cookie(cookie: String, body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// Rate-limit keys: the account name and, behind nginx, the client address.
fn limit_keys(headers: &HeaderMap, username: &str) -> Vec<String> {
    let mut keys = vec![format!("user:{username}")];
    if let Some(ip) = headers
        .get("x-client-ip")
        .and_then(|value| value.to_str().ok())
        .filter(|ip| !ip.is_empty())
    {
        keys.push(format!("ip:{ip}"));
    }
    keys
}

// --- sign-in --------------------------------------------------------------------

async fn auth_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let needs_setup = db::user_count(&state.pool).await? == 0;
    let user = auth::signed_in(&state, &headers)
        .await?
        .map(|signed| UserDto::from(&signed.user));
    Ok(Json(json!({ "needsSetup": needs_setup, "user": user })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupBody {
    username: String,
    password: String,
    token: String,
    #[serde(default)]
    display_name: String,
}

/// Create the first account, a Super Admin, with the one-time setup token.
async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetupBody>,
) -> ApiResult<Response> {
    if db::user_count(&state.pool).await? > 0 {
        return Err(ApiError::conflict(
            "RAST is already set up. Sign in instead.",
        ));
    }
    let keys = limit_keys(&headers, "setup");
    if let Some(wait) = state.limiter.wait(&keys) {
        return Err(ApiError::too_many(wait));
    }
    if !state.setup.matches(&body.token) {
        state.limiter.fail(&keys);
        return Err(ApiError::forbidden(
            "That setup token is not right. Copy it again from the server.",
        ));
    }
    let username = auth::validate_username(&body.username)?;
    auth::validate_password(&body.password)?;
    let display_name = auth::validate_display_name(&body.display_name)?;
    let hash = auth::hash_password(body.password).await?;
    let Some(id) = db::insert_first_user(
        &state.pool,
        NewUser {
            username: &username,
            display_name: &display_name,
            role: Role::SuperAdmin,
            password_hash: &hash,
        },
    )
    .await?
    else {
        return Err(ApiError::conflict(
            "RAST is already set up. Sign in instead.",
        ));
    };
    state.setup.finish();
    db::audit(&state.pool, Some(id), "setup", &username).await;
    start_session(&state, id).await
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_lowercase();
    if body.password.len() > auth::MAX_PASSWORD || username.len() > 64 {
        return Err(ApiError::unauthorized(
            "The username or password is incorrect.",
        ));
    }
    let keys = limit_keys(&headers, &username);
    if let Some(wait) = state.limiter.wait(&keys) {
        return Err(ApiError::too_many(wait));
    }
    let user = db::user_by_name(&state.pool, &username).await?;
    let ok = auth::verify_password(
        body.password,
        user.as_ref().map(|u| u.password_hash.clone()),
    )
    .await?;
    let Some(user) = user.filter(|_| ok) else {
        state.limiter.fail(&keys);
        db::audit(&state.pool, None, "login.failed", &username).await;
        return Err(ApiError::unauthorized(
            "The username or password is incorrect.",
        ));
    };
    if user.disabled {
        return Err(ApiError::forbidden(
            "This account is disabled. Ask an administrator to enable it.",
        ));
    }
    state.limiter.succeed(&keys[0]);
    db::touch_login(&state.pool, user.id).await?;
    db::audit(&state.pool, Some(user.id), "login", &user.username).await;
    start_session(&state, user.id).await
}

async fn start_session(state: &AppState, user_id: i64) -> ApiResult<Response> {
    let token = auth::random_token(32);
    db::insert_session(&state.pool, &auth::token_hash(&token), user_id).await?;
    let user = db::user_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(with_cookie(
        auth::session_cookie(&token),
        Json(json!({ "user": UserDto::from(&user) })),
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = auth::cookie_token(&headers) {
        db::delete_session(&state.pool, &auth::token_hash(&token)).await?;
    }
    Ok(with_cookie(auth::cleared_cookie(), StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

/// Change your own password; other sessions of the account end.
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
    let Signed { user, session } = auth::require(&state, &headers).await?;
    let keys = limit_keys(&headers, &user.username);
    if let Some(wait) = state.limiter.wait(&keys) {
        return Err(ApiError::too_many(wait));
    }
    if !auth::verify_password(body.current, Some(user.password_hash.clone())).await? {
        state.limiter.fail(&keys);
        return Err(ApiError::forbidden("The current password is not right."));
    }
    auth::validate_password(&body.new)?;
    let change = UserChange {
        password_hash: Some(auth::hash_password(body.new).await?),
        ..UserChange::default()
    };
    db::update_user(&state.pool, user.id, &change).await?;
    db::delete_sessions_of(&state.pool, user.id, Some(&session)).await?;
    db::audit(
        &state.pool,
        Some(user.id),
        "password.changed",
        &user.username,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// --- accounts -------------------------------------------------------------------

async fn admin(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let signed = auth::require(state, headers).await?;
    if !signed.user.role.is_admin() {
        return Err(ApiError::forbidden(
            "Only administrators can manage accounts.",
        ));
    }
    Ok(signed.user)
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    admin(&state, &headers).await?;
    let users: Vec<UserDto> = db::users(&state.pool)
        .await?
        .iter()
        .map(UserDto::from)
        .collect();
    Ok(Json(json!({ "users": users })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    username: String,
    password: String,
    role: Role,
    #[serde(default)]
    display_name: String,
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let actor = admin(&state, &headers).await?;
    if !actor.role.may_manage(body.role) {
        return Err(ApiError::forbidden(
            "Only a Super Admin can create administrators.",
        ));
    }
    let username = auth::validate_username(&body.username)?;
    auth::validate_password(&body.password)?;
    let display_name = auth::validate_display_name(&body.display_name)?;
    let hash = auth::hash_password(body.password).await?;
    let Some(id) = db::insert_user(
        &state.pool,
        NewUser {
            username: &username,
            display_name: &display_name,
            role: body.role,
            password_hash: &hash,
        },
    )
    .await?
    else {
        return Err(ApiError::conflict(format!(
            "The username {username} is taken."
        )));
    };
    db::audit(
        &state.pool,
        Some(actor.id),
        "user.created",
        &format!("{username} as {}", body.role.as_str()),
    )
    .await;
    let user = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user": UserDto::from(&user) })),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    display_name: Option<String>,
    role: Option<Role>,
    disabled: Option<bool>,
    password: Option<String>,
}

async fn update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let actor = admin(&state, &headers).await?;
    let target = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if target.id == actor.id && (body.role.is_some() || body.disabled.is_some()) {
        return Err(ApiError::forbidden(
            "You can't change your own role or disable yourself. Ask another Super Admin.",
        ));
    }
    if target.id != actor.id && !actor.role.may_manage(target.role) {
        return Err(ApiError::forbidden(
            "Only a Super Admin can change administrators.",
        ));
    }
    if let Some(role) = body.role
        && !actor.role.may_manage(role)
    {
        return Err(ApiError::forbidden(
            "Only a Super Admin can make someone an administrator.",
        ));
    }
    let loses_super = target.role == Role::SuperAdmin
        && (body.role.is_some_and(|role| role != Role::SuperAdmin) || body.disabled == Some(true));
    if loses_super && db::other_super_admins(&state.pool, target.id).await? == 0 {
        return Err(ApiError::conflict(
            "RAST needs at least one active Super Admin. Make someone else Super Admin first.",
        ));
    }
    let mut change = UserChange {
        role: body.role,
        disabled: body.disabled,
        ..UserChange::default()
    };
    if let Some(name) = &body.display_name {
        change.display_name = Some(auth::validate_display_name(name)?);
    }
    if let Some(password) = body.password {
        auth::validate_password(&password)?;
        change.password_hash = Some(auth::hash_password(password).await?);
    }
    db::update_user(&state.pool, target.id, &change).await?;
    // A new password or a disabled account ends that account's sessions.
    if target.id != actor.id && (change.password_hash.is_some() || body.disabled == Some(true)) {
        db::delete_sessions_of(&state.pool, target.id, None).await?;
    }
    let mut what = Vec::new();
    if let Some(role) = body.role {
        what.push(format!("role {}", role.as_str()));
    }
    if let Some(disabled) = body.disabled {
        what.push(if disabled { "disabled" } else { "enabled" }.to_owned());
    }
    if change.password_hash.is_some() {
        what.push("password reset".to_owned());
    }
    if change.display_name.is_some() {
        what.push("display name".to_owned());
    }
    db::audit(
        &state.pool,
        Some(actor.id),
        "user.updated",
        &format!("{}: {}", target.username, what.join(", ")),
    )
    .await;
    let user = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "user": UserDto::from(&user) })))
}

async fn delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let actor = admin(&state, &headers).await?;
    let target = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if target.id == actor.id {
        return Err(ApiError::forbidden("You can't delete your own account."));
    }
    if !actor.role.may_manage(target.role) {
        return Err(ApiError::forbidden(
            "Only a Super Admin can delete administrators.",
        ));
    }
    if target.role == Role::SuperAdmin
        && !target.disabled
        && db::other_super_admins(&state.pool, target.id).await? == 0
    {
        return Err(ApiError::conflict(
            "RAST needs at least one active Super Admin.",
        ));
    }
    db::delete_user(&state.pool, target.id).await?;
    db::audit(
        &state.pool,
        Some(actor.id),
        "user.deleted",
        &target.username,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
