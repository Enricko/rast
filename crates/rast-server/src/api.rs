//! `/api` routes. Errors are `{"error": "<sentence for the user>"}`.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Signed};
use crate::db::{self, NewUser, User, UserChange};
use crate::{AppState, Role, access, appearance, groups, sessions};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/password", post(change_password))
        .route("/api/auth/profile", patch(update_profile))
        .route(
            "/api/auth/sessions",
            get(sessions::mine).delete(sessions::end_my_others),
        )
        .route("/api/auth/sessions/{id}", delete(sessions::end_mine))
        .route("/api/access/me", get(groups::my_access))
        .route(
            "/api/appearance",
            get(appearance::mine).put(appearance::update),
        )
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", patch(update_user).delete(delete_user))
        .route(
            "/api/users/{id}/sessions",
            get(sessions::of_user).delete(sessions::end_all_of_user),
        )
        .route(
            "/api/users/{id}/access",
            get(groups::user_access).patch(groups::update_user_access),
        )
        .route("/api/groups", get(groups::list).post(groups::create))
        .route(
            "/api/groups/{id}",
            patch(groups::update).delete(groups::delete),
        )
        .fallback(no_route)
        .layer(middleware::from_fn_with_state(state.clone(), app_gate))
        .layer(middleware::from_fn(guard))
        .with_state(state)
}

async fn no_route() -> ApiError {
    ApiError::not_found_with("There is nothing at this address.")
}

/// Close an app's routes to accounts its rules deny (see `access`).
async fn app_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(app) = access::app_for_path(request.uri().path()) else {
        return next.run(request).await;
    };
    let user = match auth::require(&state, request.headers()).await {
        Ok(signed) => signed.user,
        Err(error) => return error.into_response(),
    };
    let ip = auth::client_ip(request.headers());
    match access::allowed(&state.pool, &user, app).await {
        Ok(true) => next.run(request).await,
        Ok(false) => {
            db::audit_from(
                &state.pool,
                Some(user.id),
                "access.denied",
                &format!("{} {}", user.username, app.id),
                &ip,
            )
            .await;
            ApiError::forbidden(format!(
                "{} isn't available to your account. Ask a Super Admin.",
                app.label
            ))
            .into_response()
        }
        Err(error) => ApiError::from(error).into_response(),
    }
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
        Self::not_found_with("That account no longer exists.")
    }
    pub(crate) fn not_found_with(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    pub(crate) fn too_many(seconds: u64) -> Self {
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

pub(crate) type ApiResult<T> = Result<T, ApiError>;

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
    must_change_password: bool,
    /// Sign-in is refused until then; `null` when not locked.
    locked_until: Option<i64>,
    /// Wrong passwords since the last sign-in, within the lockout window.
    failed_attempts: i64,
    /// Group names.
    groups: Vec<String>,
}

impl UserDto {
    fn new(user: &User, groups: Vec<String>) -> Self {
        let now = db::now();
        Self {
            id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            role: user.role,
            disabled: user.disabled,
            created_at: user.created_at,
            last_login_at: user.last_login_at,
            must_change_password: user.must_change_password,
            locked_until: user.locked_at(now),
            failed_attempts: user.recent_failures(now),
            groups,
        }
    }
}

/// One account with its group names.
pub(crate) async fn user_dto(state: &AppState, user: &User) -> ApiResult<UserDto> {
    let groups = db::user_groups(&state.pool, user.id)
        .await?
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    Ok(UserDto::new(user, groups))
}

fn with_cookie(cookie: String, body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// The rate-limit key of an account name.
pub(crate) fn user_key(username: &str) -> String {
    format!("user:{username}")
}

/// Rate-limit keys: the account name and, behind nginx, the client address.
fn limit_keys(ip: &str, username: &str) -> Vec<String> {
    let mut keys = vec![user_key(username)];
    if !ip.is_empty() {
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
    let user = match auth::signed_in(&state, &headers).await? {
        Some(signed) => Some(user_dto(&state, &signed.user).await?),
        None => None,
    };
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
    let ip = auth::client_ip(&headers);
    let keys = limit_keys(&ip, "setup");
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
            must_change_password: false,
        },
    )
    .await?
    else {
        return Err(ApiError::conflict(
            "RAST is already set up. Sign in instead.",
        ));
    };
    state.setup.finish();
    db::audit_from(&state.pool, Some(id), "setup", &username, &ip).await;
    start_session(&state, id, &headers).await
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
    let ip = auth::client_ip(&headers);
    let keys = limit_keys(&ip, &username);
    if let Some(wait) = state.limiter.wait(&keys) {
        return Err(ApiError::too_many(wait));
    }
    let account = db::user_by_name(&state.pool, &username).await?;
    let ok = auth::verify_password(
        body.password,
        account.as_ref().map(|u| u.password_hash.clone()),
    )
    .await?;
    // A locked account answers like the throttle, right password or not, so
    // the answer tells nothing about the password. Each try still counts
    // towards the throttle: after a restart forgets it, a locked account must
    // not become a target that can be hammered at full speed.
    let now = db::now();
    if let Some(until) = account.as_ref().and_then(|u| u.locked_at(now)) {
        state.limiter.fail(&keys);
        db::audit_from(&state.pool, None, "login.locked", &username, &ip).await;
        return Err(ApiError::too_many((until - now).max(1).unsigned_abs()));
    }
    let user = match account {
        Some(user) if ok => user,
        account => {
            state.limiter.fail(&keys);
            db::audit_from(&state.pool, None, "login.failed", &username, &ip).await;
            if let Some(account) = account
                && db::record_failure(&state.pool, account.id).await?.is_some()
            {
                db::audit_from(
                    &state.pool,
                    None,
                    "user.locked",
                    &format!("{username} after {} wrong passwords", db::LOCK_AFTER),
                    &ip,
                )
                .await;
            }
            return Err(ApiError::unauthorized(
                "The username or password is incorrect.",
            ));
        }
    };
    if user.disabled {
        return Err(ApiError::forbidden(
            "This account is disabled. Ask an administrator to enable it.",
        ));
    }
    state.limiter.succeed(&keys[0]);
    db::touch_login(&state.pool, user.id).await?;
    db::audit_from(&state.pool, Some(user.id), "login", &user.username, &ip).await;
    start_session(&state, user.id, &headers).await
}

async fn start_session(state: &AppState, user_id: i64, headers: &HeaderMap) -> ApiResult<Response> {
    let token = auth::random_token(32);
    db::insert_session(
        &state.pool,
        &auth::random_id(16),
        &auth::token_hash(&token),
        user_id,
        &auth::origin(headers),
    )
    .await?;
    let user = db::user_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(with_cookie(
        auth::session_cookie(&token),
        Json(json!({ "user": user_dto(state, &user).await? })),
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = auth::cookie_token(&headers) {
        if let Some(signed) = auth::signed_in(&state, &headers).await? {
            db::audit_from(
                &state.pool,
                Some(signed.user.id),
                "logout",
                &signed.user.username,
                &auth::client_ip(&headers),
            )
            .await;
        }
        db::delete_session(&state.pool, &auth::token_hash(&token)).await?;
    }
    Ok(with_cookie(auth::cleared_cookie(), StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

/// Change your own password; other sessions of the account end. This is the
/// one change an account must make first when an administrator set its
/// password.
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
    let Signed { user, session, .. } = auth::require_any(&state, &headers).await?;
    let ip = auth::client_ip(&headers);
    let keys = limit_keys(&ip, &user.username);
    if let Some(wait) = state.limiter.wait(&keys) {
        return Err(ApiError::too_many(wait));
    }
    let same = body.new == body.current;
    if !auth::verify_password(body.current, Some(user.password_hash.clone())).await? {
        state.limiter.fail(&keys);
        return Err(ApiError::forbidden("The current password is not right."));
    }
    auth::validate_password(&body.new)?;
    if same {
        return Err(ApiError::bad_request(
            "Choose a password that is different from the current one.",
        ));
    }
    let change = UserChange {
        password_hash: Some(auth::hash_password(body.new).await?),
        must_change_password: Some(false),
        ..UserChange::default()
    };
    db::update_user(&state.pool, user.id, &change).await?;
    db::delete_sessions_of(&state.pool, user.id, Some(&session)).await?;
    db::audit_from(
        &state.pool,
        Some(user.id),
        "password.changed",
        &user.username,
        &ip,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileBody {
    display_name: String,
}

/// Change your own display name.
async fn update_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProfileBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let Signed { user, .. } = auth::require(&state, &headers).await?;
    let change = UserChange {
        display_name: Some(auth::validate_display_name(&body.display_name)?),
        ..UserChange::default()
    };
    db::update_user(&state.pool, user.id, &change).await?;
    db::audit_from(
        &state.pool,
        Some(user.id),
        "profile.updated",
        &user.username,
        &auth::client_ip(&headers),
    )
    .await;
    let user = db::user_by_id(&state.pool, user.id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "user": user_dto(&state, &user).await? })))
}

// --- accounts -------------------------------------------------------------------

pub(crate) async fn admin(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    admin_signed(state, headers).await.map(|signed| signed.user)
}

/// A signed-in Super Admin or Admin, with the session.
pub(crate) async fn admin_signed(state: &AppState, headers: &HeaderMap) -> ApiResult<Signed> {
    let signed = auth::require(state, headers).await?;
    if !signed.user.role.is_admin() {
        return Err(ApiError::forbidden(
            "Only administrators can manage accounts.",
        ));
    }
    Ok(signed)
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    admin(&state, &headers).await?;
    let mut groups = db::group_names_by_user(&state.pool).await?;
    let users: Vec<UserDto> = db::users(&state.pool)
        .await?
        .iter()
        .map(|user| UserDto::new(user, groups.remove(&user.id).unwrap_or_default()))
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
    /// The person chooses their own password at the first sign-in.
    #[serde(default)]
    must_change_password: bool,
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
            must_change_password: body.must_change_password,
        },
    )
    .await?
    else {
        return Err(ApiError::conflict(format!(
            "The username {username} is taken."
        )));
    };
    db::audit_from(
        &state.pool,
        Some(actor.id),
        "user.created",
        &format!(
            "{username} as {}{}",
            body.role.as_str(),
            if body.must_change_password {
                ", new password at first sign-in"
            } else {
                ""
            }
        ),
        &auth::client_ip(&headers),
    )
    .await;
    let user = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user": user_dto(&state, &user).await? })),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    display_name: Option<String>,
    role: Option<Role>,
    disabled: Option<bool>,
    /// A new password. The person must choose their own at the next sign-in
    /// unless `mustChangePassword` is `false`.
    password: Option<String>,
    must_change_password: Option<bool>,
    /// Lift a lockout and forget failed sign-ins.
    #[serde(default)]
    unlock: bool,
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
    if target.id == actor.id
        && (body.role.is_some()
            || body.disabled.is_some()
            || body.must_change_password.is_some()
            || body.unlock)
    {
        return Err(ApiError::forbidden(
            "You can't change your own role or sign-in. Ask another Super Admin.",
        ));
    }
    if target.id == actor.id && body.password.is_some() {
        return Err(ApiError::forbidden(
            "Change your own password under Profile, with your current password.",
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
        must_change_password: body
            .must_change_password
            .or(body.password.is_some().then_some(true)),
        unlock: body.unlock,
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
    if change.unlock {
        state.limiter.succeed(&user_key(&target.username));
    }
    // A new password, a disabled account or a password the person must
    // replace ends that account's sessions: it signs in again, and chooses.
    if change.password_hash.is_some()
        || body.disabled == Some(true)
        || change.must_change_password == Some(true)
    {
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
    match change.must_change_password {
        Some(true) => what.push("new password at next sign-in".to_owned()),
        Some(false) => what.push("keeps the password".to_owned()),
        None => {}
    }
    if change.unlock {
        what.push("unlocked".to_owned());
    }
    db::audit_from(
        &state.pool,
        Some(actor.id),
        "user.updated",
        &format!("{}: {}", target.username, what.join(", ")),
        &auth::client_ip(&headers),
    )
    .await;
    let user = db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "user": user_dto(&state, &user).await? })))
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
    db::audit_from(
        &state.pool,
        Some(actor.id),
        "user.deleted",
        &target.username,
        &auth::client_ip(&headers),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
