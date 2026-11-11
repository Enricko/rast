//! HTTP middleware: session authentication, CSRF enforcement, client IP
//! resolution and baseline security headers.

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::net::SocketAddr;

use crate::{
    auth::{self, csrf, session, AuthUser, ClientIp},
    error::AppError,
    state::AppState,
};

const SAFE_METHODS: [Method; 3] = [Method::GET, Method::HEAD, Method::OPTIONS];

/// Endpoints an account flagged with `must_change_password` may still use:
/// identify itself, change the password, sign out and keep its live
/// connection (so the UI can show the forced-change dialog).
const PASSWORD_CHANGE_ALLOWED: [&str; 4] = [
    "/api/auth/me",
    "/api/auth/password",
    "/api/auth/logout",
    "/api/events",
];

/// Is this request permitted while a password change is pending?
pub fn allowed_during_forced_password_change(path: &str) -> bool {
    PASSWORD_CHANGE_ALLOWED.contains(&path)
}

/// Authentication + CSRF layer applied to all protected routes.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let peer_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let client_ip = auth::client_ip(req.headers(), peer_ip, &state.config);
    req.extensions_mut().insert(ClientIp(client_ip));

    let token = auth::cookie_value(req.headers(), session::SESSION_COOKIE)
        .ok_or(AppError::Unauthorized("authentication required"))?;

    let record = session::validate(&state.db, &token, state.config.session_ttl_hours)
        .await?
        .ok_or(AppError::Unauthorized("session expired or revoked"))?;

    // Load the user on every request so deactivation takes effect immediately.
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT id, username, display_name, role, active, quota_bytes, must_change_password
         FROM users WHERE id = ?",
    )
    .bind(record.user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        session::revoke(&state.db, &record.session_id).await?;
        return Err(AppError::Unauthorized("account no longer exists"));
    };
    if !row.active {
        session::revoke(&state.db, &record.session_id).await?;
        return Err(AppError::Unauthorized("account is disabled"));
    }

    if !SAFE_METHODS.contains(req.method()) {
        if !csrf::verify(req.headers(), &state.secrets, &record.session_id) {
            return Err(AppError::Forbidden("invalid or missing CSRF token"));
        }
        if !origin_allowed(req.headers(), &state.config) {
            return Err(AppError::Forbidden("cross-origin request rejected"));
        }
    }

    // A password reset by an administrator (or the first sign-in with a
    // temporary password) must be completed before the account can use any
    // other API — enforced server-side, not only by the UI dialog.
    if row.must_change_password && !allowed_during_forced_password_change(req.uri().path()) {
        return Err(AppError::Forbidden(
            "password change required before using RAST",
        ));
    }

    let user = AuthUser {
        id: row.id,
        username: row.username,
        display_name: row.display_name,
        role: row.role,
        session_id: record.session_id,
        quota_bytes: row.quota_bytes,
        must_change_password: row.must_change_password,
    };
    req.extensions_mut().insert(user);

    let mut response = next.run(req).await;
    apply_security_headers(response.headers_mut());
    Ok(response)
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    username: String,
    display_name: String,
    role: auth::Role,
    active: bool,
    quota_bytes: i64,
    must_change_password: bool,
}

/// Baseline headers added to every response (public or protected).
pub async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let mut response = next.run(req).await;
    apply_security_headers(response.headers_mut());
    response
}

pub fn apply_security_headers(headers: &mut HeaderMap) {
    let defaults: [(&'static str, HeaderValue); 5] = [
        (
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        ),
        ("x-frame-options", HeaderValue::from_static("DENY")),
        ("referrer-policy", HeaderValue::from_static("no-referrer")),
        (
            "permissions-policy",
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ),
        (
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        ),
    ];
    for (name, value) in defaults {
        if !headers.contains_key(name) {
            headers.insert(header::HeaderName::from_static(name), value);
        }
    }
}

/// Validate `Origin`/`Sec-Fetch-Site` for state-changing requests.
///
/// The CSRF token is the primary defence; this is defence in depth against
/// misconfigured proxies.
pub fn origin_allowed(headers: &HeaderMap, cfg: &crate::config::Config) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin == "null" {
            return false;
        }
        if let Some(expected) = cfg.public_url.as_deref() {
            let expected = origin_of(expected);
            let actual = origin_of(origin);
            return match (expected, actual) {
                (Some(e), Some(a)) => e.eq_ignore_ascii_case(&a),
                _ => false,
            };
        }
    }
    true
}

/// Extract scheme://host[:port] from a URL-ish string.
pub fn origin_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}").to_ascii_lowercase())
}

/// Final fallback for unmatched routes: JSON for API paths, SPA for the rest is
/// handled by the static file router.
pub async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": {"code": "not_found", "message": "endpoint not found"}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_parsing() {
        assert_eq!(
            origin_of("https://desktop.example.com/app?x=1").as_deref(),
            Some("https://desktop.example.com")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8720").as_deref(),
            Some("http://127.0.0.1:8720")
        );
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn origin_checks() {
        let cfg = crate::config::Config::from_parts(
            None,
            crate::config::FileConfig {
                public_url: Some("https://rast.example.com".into()),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://rast.example.com"),
        );
        assert!(origin_allowed(&headers, &cfg));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example.com"),
        );
        assert!(!origin_allowed(&headers, &cfg));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!origin_allowed(&headers, &cfg));
    }
}

#[cfg(test)]
mod forced_password_change_tests {
    use super::allowed_during_forced_password_change;

    #[test]
    fn only_identity_password_logout_and_events_are_allowed() {
        for allowed in [
            "/api/auth/me",
            "/api/auth/password",
            "/api/auth/logout",
            "/api/events",
        ] {
            assert!(allowed_during_forced_password_change(allowed), "{allowed}");
        }
        for denied in [
            "/api/files",
            "/api/files/download",
            "/api/users",
            "/api/appearance",
            "/api/auth/sessions",
            "/api/auth/me/extra",
        ] {
            assert!(!allowed_during_forced_password_change(denied), "{denied}");
        }
    }
}
