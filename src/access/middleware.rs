//! Application-policy middleware.
//!
//! Runs after session authentication (the router mounts it so `require_auth`
//! executes first) and enforces the group application rules for non-admin
//! requests. Folder-level checks remain in the storage layer; this middleware
//! only blocks an application surface entirely (or write methods for a
//! read-only grant). Administrators bypass it and keep their existing role
//! checks.

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{access, auth::AuthUser, error::AppError, state::AppState};

/// API path prefixes mapped to the stable application ids.
pub fn app_for_path(path: &str) -> Option<&'static str> {
    if path == "/api/files" || path.starts_with("/api/files/") {
        Some("files")
    } else if path == "/api/monitor" || path.starts_with("/api/monitor/") {
        Some("monitor")
    } else if path == "/api/logs" || path.starts_with("/api/logs/") {
        Some("logs")
    } else if path == "/api/users"
        || path.starts_with("/api/users/")
        || path == "/api/shares"
        || path.starts_with("/api/shares/")
        || path == "/api/settings"
        || path.starts_with("/api/settings/")
        || path == "/api/groups"
        || path.starts_with("/api/groups/")
    {
        Some("controlpanel")
    } else if path == "/api/apps"
        || path.starts_with("/api/apps/")
        || path == "/api/docker"
        || path.starts_with("/api/docker/")
    {
        Some("applications")
    } else if path == "/api/backups" || path.starts_with("/api/backups/") {
        Some("backup")
    } else {
        None
    }
}

/// Reject the request when group policy denies the application (or when a
/// read-only grant attempts a mutation).
pub async fn enforce_app_policy(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(user) = request.extensions().get::<AuthUser>().cloned() else {
        // Authentication middleware has not run (misconfigured stack); leave
        // the request to the existing auth layer which returns a 401.
        return next.run(request).await;
    };
    let Some(app_id) = app_for_path(request.uri().path()) else {
        return next.run(request).await;
    };
    // An app uninstalled in App Center is closed for everyone.
    if !access::app_installed(&state.db, app_id).await {
        return AppError::Forbidden(
            "this application is not installed; install it from App Center",
        )
        .into_response();
    }
    if user.is_admin() {
        return next.run(request).await;
    }
    match access::app_access(&state.db, &user, app_id).await {
        Ok(grant) if grant.allowed => {
            if grant.read_only && !matches!(request.method().as_str(), "GET" | "HEAD" | "OPTIONS") {
                return AppError::Forbidden(
                    "read-only access: this action requires an administrator",
                )
                .into_response();
            }
            next.run(request).await
        }
        Ok(_) => AppError::Forbidden("access denied by group policy").into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::app_for_path;

    #[test]
    fn maps_api_prefixes_to_apps() {
        assert_eq!(app_for_path("/api/files"), Some("files"));
        assert_eq!(app_for_path("/api/files/upload/init"), Some("files"));
        assert_eq!(app_for_path("/api/monitor/snapshot"), Some("monitor"));
        assert_eq!(app_for_path("/api/logs"), Some("logs"));
        assert_eq!(
            app_for_path("/api/users/3/effective-access"),
            Some("controlpanel")
        );
        assert_eq!(app_for_path("/api/groups"), Some("controlpanel"));
        assert_eq!(app_for_path("/api/shares/team"), Some("controlpanel"));
        assert_eq!(
            app_for_path("/api/apps/123/logs/stream"),
            Some("applications")
        );
        assert_eq!(app_for_path("/api/docker/status"), Some("applications"));
        assert_eq!(app_for_path("/api/backups/1/download"), Some("backup"));
        // Own profile, appearance, events and access helpers are not gated here.
        assert_eq!(app_for_path("/api/auth/me"), None);
        assert_eq!(app_for_path("/api/appearance"), None);
        assert_eq!(app_for_path("/api/events"), None);
        assert_eq!(app_for_path("/api/access/me"), None);
        assert_eq!(app_for_path("/healthz"), None);
    }
}
