//! End-to-end HTTP tests: authentication, sessions, CSRF, roles and the
//! administrative API, exercised through the real Axum router with a temporary
//! database and storage tree (no mocks of the business logic).

use std::net::SocketAddr;

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use rast::{
    api,
    auth::Role,
    config::{Config, FileConfig},
    db,
    state::AppState,
    users,
};

struct TestServer {
    _dir: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg = Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.path().join("data")),
                storage_dir: Some(dir.path().join("storage")),
                public_url: Some("https://rast.test".into()),
                secure_cookies: Some(false),
                setup_token_required: Some(true),
                trusted_proxies: Some(vec!["127.0.0.1/32".into()]),
                ..Default::default()
            },
            |_| None,
        )
        .expect("config");
        let state = AppState::new(cfg).await.expect("state");
        let app = api::router(state.clone());
        Self {
            _dir: dir,
            state,
            app,
        }
    }

    async fn create_user(&self, username: &str, password: &str, role: Role) {
        users::create(
            &self.state.db,
            &self.state.config,
            username,
            password,
            role,
            None,
            None,
        )
        .await
        .expect("create user");
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: HeaderMap,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&value).expect("json"))
            }
            None => Body::empty(),
        };
        let request = builder.body(body).expect("request");
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
        };
        (status, headers, value)
    }

    async fn login(&self, username: &str, password: &str) -> Session {
        let (status, headers, body) = self
            .request(
                "POST",
                "/api/auth/login",
                Some(json!({"username": username, "password": password})),
                HeaderMap::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "login failed: {body}");
        let cookie = headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("session cookie")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_string();
        let csrf = body["csrfToken"].as_str().expect("csrf token").to_string();
        let user_id = body["user"]["id"].as_i64().expect("user id");
        Session {
            cookie,
            csrf,
            user_id,
        }
    }

    /// Send a raw request body (uploads/downloads) through the real router.
    async fn request_raw(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        headers: HeaderMap,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, body.len());
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let request = builder.body(Body::from(body)).expect("request");
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        (status, bytes)
    }
}

struct Session {
    cookie: String,
    csrf: String,
    user_id: i64,
}

impl Session {
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            self.cookie.parse().expect("valid cookie header"),
        );
        headers
    }

    fn headers_with_csrf(&self) -> HeaderMap {
        let mut headers = self.headers();
        headers.insert("x-csrf-token", self.csrf.parse().expect("csrf header"));
        headers
    }
}

const ADMIN_PASSWORD: &str = "Admin-Password-123!";
const USER_PASSWORD: &str = "User-Password-123!";

#[tokio::test]
async fn public_endpoints_work_without_auth() {
    let server = TestServer::start().await;
    let (status, _, _body) = server
        .request("GET", "/healthz", None, HeaderMap::new())
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = server
        .request("GET", "/api/bootstrap", None, HeaderMap::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["product"], "RAST");
    assert_eq!(body["setupRequired"], true);
    assert_eq!(body["registrationEnabled"], false);

    // Setup is token-gated by default.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/setup",
            Some(json!({"username": "first", "password": "First-Admin-123!"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "setup must require a token: {body}"
    );
}

#[tokio::test]
async fn setup_token_flow_creates_admin_once() {
    let server = TestServer::start().await;
    let token = api::auth::issue_setup_token(&server.state.db)
        .await
        .expect("setup token");

    let (status, _, body) = server
        .request(
            "POST",
            "/api/setup",
            Some(json!({"token": token, "username": "boss", "password": "Boss-Admin-123!"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "setup failed: {body}");
    assert_eq!(body["user"]["role"], "admin");

    // One-shot: a second attempt is rejected even with the same token.
    let (status, _, _) = server
        .request(
            "POST",
            "/api/setup",
            Some(json!({"token": token, "username": "boss2", "password": "Boss-Admin-123!"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_sets_cookie_and_requires_csrf_for_mutations() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;

    let session = server.login("admin", ADMIN_PASSWORD).await;

    // Session cookie works for reads.
    let (status, _, body) = server
        .request("GET", "/api/auth/me", None, session.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["username"], "admin");
    assert_eq!(body["user"]["role"], "admin");
    assert_eq!(body["csrfToken"], session.csrf);

    // The public bootstrap endpoint tells the SPA whether the cookie is valid,
    // so the boot path never has to probe a protected endpoint.
    let (status, _, body) = server
        .request("GET", "/api/bootstrap", None, session.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["authenticated"], true,
        "bootstrap should see the session"
    );
    let (_, _, anon) = server
        .request("GET", "/api/bootstrap", None, HeaderMap::new())
        .await;
    assert_eq!(anon["authenticated"], false);

    // Mutating without the CSRF header is rejected.
    let (status, _, _) = server
        .request(
            "POST",
            "/api/files/mkdir",
            Some(json!({"path": "/demo"})),
            session.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Mutating with the wrong token is rejected.
    let mut wrong = session.headers();
    wrong.insert("x-csrf-token", "deadbeef".parse().expect("header"));
    let (status, _, _) = server
        .request(
            "POST",
            "/api/files/mkdir",
            Some(json!({"path": "/demo"})),
            wrong,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_api_requests_are_rejected() {
    let server = TestServer::start().await;
    for (method, path) in [
        ("GET", "/api/auth/me"),
        ("GET", "/api/users"),
        ("GET", "/api/logs"),
        ("GET", "/api/settings"),
        ("GET", "/api/system/info"),
        ("GET", "/api/files"),
    ] {
        let (status, _, _) = server.request(method, path, None, HeaderMap::new()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} should require authentication"
        );
    }
}

#[tokio::test]
async fn login_rate_limiting_and_bad_credentials() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;

    // Wrong password returns the same generic error as a missing user.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/auth/login",
            Some(json!({"username": "admin", "password": "wrong-password-1"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["message"], "invalid username or password");

    let (status, _, _) = server
        .request(
            "POST",
            "/api/auth/login",
            Some(json!({"username": "nobody", "password": "wrong-password-1"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Repeated failures trip the per-account lockout / IP limiter.
    let mut saw_rate_limit = false;
    for _ in 0..12 {
        let (status, _, _) = server
            .request(
                "POST",
                "/api/auth/login",
                Some(json!({"username": "admin", "password": "wrong-password-1"})),
                HeaderMap::new(),
            )
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_rate_limit = true;
            break;
        }
    }
    assert!(saw_rate_limit, "login attempts were not rate limited");
}

#[tokio::test]
async fn logout_revokes_the_session() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let session = server.login("admin", ADMIN_PASSWORD).await;

    let (status, headers, _) = server
        .request(
            "POST",
            "/api/auth/logout",
            Some(json!({})),
            session.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let cookies: Vec<String> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    assert!(
        cookies.iter().any(|c| c.contains("Max-Age=0")),
        "logout must clear the cookie"
    );

    let (status, _, _) = server
        .request("GET", "/api/auth/me", None, session.headers())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn role_enforcement_and_user_management() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;

    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;

    // Regular users cannot access admin endpoints.
    for (method, path, body) in [
        ("GET", "/api/users", None),
        ("GET", "/api/settings", None),
        ("GET", "/api/backups", None),
        ("GET", "/api/system/info", None),
    ] {
        let (status, _, _) = server.request(method, path, body, alice.headers()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "non-admin must be forbidden from {method} {path}"
        );
    }

    // Admin can list users.
    let (status, _, body) = server
        .request("GET", "/api/users?page=1&perPage=10", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 2);

    // Create a user through the API.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/users",
            Some(json!({
                "username": "bob",
                "password": "Bob-Password-123!",
                "role": "user",
                "quotaBytes": 1024 * 1024 * 100
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create user failed: {body}");
    let bob_id = body["id"].as_i64().expect("bob id");
    assert_eq!(body["username"], "bob");
    assert_eq!(body["quotaBytes"], 1024 * 1024 * 100);
    assert!(server.state.config.home_for("bob").is_dir());

    // Duplicate usernames are rejected.
    let (status, _, _) = server
        .request(
            "POST",
            "/api/users",
            Some(json!({"username": "bob", "password": "Bob-Password-123!", "role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Weak password is rejected.
    let (status, _, _) = server
        .request(
            "POST",
            "/api/users",
            Some(json!({"username": "weak", "password": "short", "role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The last administrator cannot be demoted or disabled.
    let (status, _, _) = server
        .request(
            "PATCH",
            &format!("/api/users/{}", admin.user_id),
            Some(json!({"role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, _) = server
        .request(
            "DELETE",
            &format!("/api/users/{bob_id}"),
            None,
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Disabled user can no longer sign in.
    let (status, _, _) = server
        .request(
            "POST",
            "/api/auth/login",
            Some(json!({"username": "bob", "password": "Bob-Password-123!"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn password_change_revokes_other_sessions() {
    let server = TestServer::start().await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let first = server.login("alice", USER_PASSWORD).await;
    let second = server.login("alice", USER_PASSWORD).await;

    let (status, _, body) = server
        .request(
            "POST",
            "/api/auth/password",
            Some(json!({"currentPassword": USER_PASSWORD, "newPassword": "New-Password-456!"})),
            first.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The current session survives; the other one is revoked.
    let (status, _, _) = server
        .request("GET", "/api/auth/me", None, first.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = server
        .request("GET", "/api/auth/me", None, second.headers())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn activity_log_records_actions_without_secrets() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let _ = server
        .request(
            "POST",
            "/api/users",
            Some(json!({"username": "carol", "password": "Carol-Password-123!", "role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;

    let (status, _, body) = server
        .request("GET", "/api/logs?page=1&perPage=50", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("items");
    assert!(body["total"].as_i64().unwrap_or(0) >= 2);
    let actions: Vec<&str> = items
        .iter()
        .filter_map(|item| item["action"].as_str())
        .collect();
    assert!(actions.contains(&"user.create"), "actions: {actions:?}");
    let serialized = serde_json::to_string(&body).expect("json");
    assert!(!serialized.contains(ADMIN_PASSWORD));
    assert!(!serialized.contains("Carol-Password-123!"));
}

#[tokio::test]
async fn system_info_and_components_report_real_data() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;

    let (status, _, body) = server
        .request("GET", "/api/system/info", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["product"], "RAST");
    assert_eq!(body["counts"]["admins"], 1);
    assert!(body["host"]["hostname"]
        .as_str()
        .is_some_and(|h| !h.is_empty()));
    assert!(body["storage"]["available"].as_bool().is_some());

    let (status, _, body) = server
        .request("GET", "/api/system/components", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let components = body["components"].as_array().expect("components");
    assert!(components
        .iter()
        .any(|c| c["id"] == "database" && c["status"] == "ok"));
}

#[tokio::test]
async fn backup_roundtrip_through_api() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;

    let (status, _, body) = server
        .request(
            "POST",
            "/api/backups",
            Some(json!({"scope": ["db", "config"], "note": "e2e test"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "backup start failed: {body}");
    let job_id = body["jobId"].as_str().expect("job id").to_string();

    // Wait for the background job.
    let mut job = Value::Null;
    for _ in 0..100 {
        let (_, _, value) = server
            .request("GET", &format!("/api/jobs/{job_id}"), None, admin.headers())
            .await;
        job = value;
        match job["status"].as_str() {
            Some("done") => break,
            Some("failed") | Some("cancelled") => panic!("backup job failed: {job}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    assert_eq!(job["status"], "done", "backup job did not finish: {job}");

    let (status, _, list) = server
        .request("GET", "/api/backups", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "backup not listed: {list}");
    let backup_id = items[0]["id"].as_str().expect("backup id").to_string();
    assert!(items[0]["sha256"].as_str().is_some_and(|s| s.len() == 64));

    let (status, _, verify) = server
        .request(
            "POST",
            &format!("/api/backups/{backup_id}/verify"),
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{verify}");
    assert_eq!(verify["ok"], true, "verification failed: {verify}");

    let (status, _, guide) = server
        .request(
            "GET",
            &format!("/api/backups/restore-guide?backupId={backup_id}"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(guide["steps"].as_array().is_some_and(|s| s.len() >= 4));

    // Delete removes the row.
    let (status, _, _) = server
        .request(
            "DELETE",
            &format!("/api/backups/{backup_id}"),
            None,
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, list) = server
        .request("GET", "/api/backups", None, admin.headers())
        .await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn setup_is_disabled_after_first_admin() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let (_, _, body) = server
        .request("GET", "/api/bootstrap", None, HeaderMap::new())
        .await;
    assert_eq!(body["setupRequired"], false);
    let (status, _, _) = server
        .request(
            "POST",
            "/api/setup",
            Some(json!({"username": "evil", "password": "Evil-Admin-123!"})),
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn metrics_is_not_public() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    // Internal metrics endpoints must never be reachable unauthenticated.
    for path in [
        "/api/monitor/snapshot",
        "/api/monitor/history",
        "/api/monitor/containers",
        "/api/events",
        "/api/system/components",
    ] {
        let (status, _, _) = server.request("GET", path, None, HeaderMap::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} leaked data");
    }
}

#[tokio::test]
async fn tls_reverse_proxy_client_ip_is_trusted_only_from_proxy() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;

    // Trusted peer (127.0.0.1) + CF-Connecting-IP: the audit log records the real IP.
    let mut headers = HeaderMap::new();
    headers.insert("cf-connecting-ip", "203.0.113.50".parse().expect("header"));
    let (status, _, _) = server
        .request(
            "POST",
            "/api/auth/login",
            Some(json!({"username": "admin", "password": ADMIN_PASSWORD})),
            headers,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let (_, _, body) = server
        .request("GET", "/api/logs?action=auth.login", None, admin.headers())
        .await;
    let ips: Vec<&str> = body["items"]
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["ip"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        ips.contains(&"203.0.113.50"),
        "forwarded client IP not honoured: {ips:?}"
    );
}

#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn state_is_send_sync() {
    assert_send_sync::<AppState>();
    assert_send_sync::<SocketAddr>();
    let _ = db::now_ms();
}

// ---------------------------------------------------------------------------
// Appearance axes + realtime event authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn appearance_theme_accent_and_wallpaper_are_independent() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;

    // Defaults: light mode, Ocean Blue accent, bundled wallpaper.
    let (status, _, body) = server
        .request("GET", "/api/appearance", None, alice.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["theme"], "light");
    assert_eq!(body["accent"], "ocean");
    assert_eq!(body["wallpaper"]["kind"], "builtin");

    // Change only the accent; theme and wallpaper must be preserved.
    let (status, _, body) = server
        .request(
            "PUT",
            "/api/appearance",
            Some(json!({"accent": "forest"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accent"], "forest");
    assert_eq!(body["theme"], "light");
    assert_eq!(body["wallpaper"]["builtinId"], "alpine-dawn");

    // Change only the theme; accent stays.
    let (status, _, body) = server
        .request(
            "PUT",
            "/api/appearance",
            Some(json!({"theme": "dark"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["theme"], "dark");
    assert_eq!(body["accent"], "forest");

    // Change only the wallpaper; both axes stay.
    let (status, _, body) = server
        .request(
            "PUT",
            "/api/appearance",
            Some(json!({"wallpaper": {"kind": "builtin", "builtinId": "coastal-calm", "overlay": 35}})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["wallpaper"]["builtinId"], "coastal-calm");
    assert_eq!(body["theme"], "dark");
    assert_eq!(body["accent"], "forest");

    // Invalid accent is rejected.
    let (status, _, _) = server
        .request(
            "PUT",
            "/api/appearance",
            Some(json!({"accent": "neon"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Another account keeps its own defaults (isolation).
    let (_, _, other) = server
        .request("GET", "/api/appearance", None, admin.headers())
        .await;
    assert_eq!(other["accent"], "ocean");
    assert_eq!(other["theme"], "light");
    assert_eq!(other["wallpaper"]["builtinId"], "alpine-dawn");

    // Reset restores the wallpaper but keeps the chosen theme/accent.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/appearance/reset",
            Some(json!({})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["wallpaper"]["builtinId"], "alpine-dawn");
    assert_eq!(body["theme"], "dark");
    assert_eq!(body["accent"], "forest");
}

#[tokio::test]
async fn realtime_events_are_authorized_and_ordered() {
    use std::time::Duration;

    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    server
        .create_user("bob", "Bob-Password-123!", Role::User)
        .await;
    let alice = server.login("alice", USER_PASSWORD).await;
    let bob = server.login("bob", "Bob-Password-123!").await;

    // Subscribe an SSE stream for Alice and collect frames for a short window.
    let request = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            header::COOKIE,
            alice.cookie.parse::<axum::http::HeaderValue>().unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let collector = tokio::spawn(collect_sse(
        response.into_body(),
        Duration::from_millis(1600),
    ));
    tokio::time::sleep(Duration::from_millis(150)).await;

    // One event for Bob only, one global event.
    server
        .state
        .events
        .publish_user("test.private", bob.user_id, json!({"secret": "bob-only"}));
    server
        .state
        .events
        .publish("test.broadcast", json!({"hello": "everyone"}));

    let joined = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("SSE collector timed out")
        .expect("SSE collector panicked");
    assert!(
        joined.contains("event: hello"),
        "missing hello frame: {joined}"
    );
    assert!(
        joined.contains("test.broadcast"),
        "missing broadcast frame: {joined}"
    );
    assert!(
        !joined.contains("bob-only"),
        "private event leaked to another user: {joined}"
    );

    // Every frame carries a strictly increasing sequence id.
    let seq_ids: Vec<u64> = joined
        .lines()
        .filter(|line| line.starts_with("id: "))
        .filter_map(|line| line.trim_start_matches("id: ").trim().parse::<u64>().ok())
        .collect();
    assert!(seq_ids.len() >= 2, "expected seq ids: {joined}");
    assert!(
        seq_ids.windows(2).all(|pair| pair[0] <= pair[1]),
        "sequence ids are not monotonic: {seq_ids:?}"
    );
}

/// Read an SSE body for a bounded window and return the raw concatenated text.
async fn collect_sse(body: Body, duration: std::time::Duration) -> String {
    use http_body_util::BodyExt;
    use std::time::Instant;

    let deadline = Instant::now() + duration;
    let mut text = String::new();
    tokio::pin!(body);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
    text
}

#[tokio::test]
async fn domain_mutations_publish_scoped_realtime_events() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;
    let mut rx = server.state.events.subscribe();

    // Appearance changes are user-scoped and carry the new accent.
    let (status, _, body) = server
        .request(
            "PUT",
            "/api/appearance",
            Some(json!({"accent": "teal"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = drain_events(&mut rx, std::time::Duration::from_millis(400)).await;
    let appearance = events
        .iter()
        .find(|event| event.kind == "appearance.changed")
        .expect("no appearance.changed event");
    assert_eq!(
        appearance.scope,
        rast::events::EventScope::User(alice.user_id),
        "appearance event must be user-scoped"
    );
    assert_eq!(appearance.data["appearance"]["accent"], "teal");
    let alice_audit = events
        .iter()
        .find(|event| event.kind == "audit.created" && event.data["userId"] == alice.user_id)
        .expect("no audit event for alice's appearance change");
    assert_eq!(
        alice_audit.scope,
        rast::events::EventScope::UserAndAdmin(alice.user_id),
        "audit events must be visible to the actor and admins only"
    );

    // User management events are admin-scoped.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/users",
            Some(json!({"username": "carol", "password": "Carol-Password-123!", "role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = drain_events(&mut rx, std::time::Duration::from_millis(400)).await;
    let user_event = events
        .iter()
        .find(|event| event.kind == "user.changed")
        .expect("no user.changed event");
    assert_eq!(user_event.scope, rast::events::EventScope::Admin);
    assert_eq!(user_event.data["event"], "created");

    // Settings changes are admin-scoped.
    let (status, _, body) = server
        .request(
            "PATCH",
            "/api/settings",
            Some(json!({"siteName": "Realtime Test"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = drain_events(&mut rx, std::time::Duration::from_millis(400)).await;
    let settings = events
        .iter()
        .find(|event| event.kind == "settings.changed")
        .expect("no settings.changed event");
    assert_eq!(settings.scope, rast::events::EventScope::Admin);
    assert!(settings.data["keys"]
        .as_array()
        .is_some_and(|keys| !keys.is_empty()));
}

/// Collect every event published within a bounded window.
async fn drain_events(
    rx: &mut tokio::sync::broadcast::Receiver<rast::events::Event>,
    window: std::time::Duration,
) -> Vec<rast::events::Event> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => events.push(event),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    events
}

// ---------------------------------------------------------------------------
// User groups & access policy
// ---------------------------------------------------------------------------

async fn create_share(server: &TestServer, admin: &Session, name: &str) -> Value {
    let (status, _, body) = server
        .request(
            "POST",
            "/api/shares",
            Some(json!({"name": name, "description": format!("{name} share")})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create share {name}: {body}");
    body
}

#[tokio::test]
async fn groups_crud_membership_events_and_delete_retention() {
    use rast::events::EventScope;

    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    server
        .create_user("bob", "Bob-Password-123!", Role::User)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;
    let bob = server.login("bob", "Bob-Password-123!").await;
    let _projects = create_share(&server, &admin, "Projects").await;

    let mut rx = server.state.events.subscribe();
    let (status, _, body) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Editors",
                "description": "Writes into Projects",
                "members": [alice.user_id, bob.user_id],
                "folderRules": [{"shareName": "Projects", "rule": "write"}],
                "appRules": [{"appId": "files", "rule": "allow"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create group: {body}");
    let group_id = body["id"].as_i64().expect("group id");
    assert_eq!(body["memberCount"], 2);
    assert_eq!(body["folderRuleCount"], 1);
    assert_eq!(body["appRuleCount"], 1);
    assert_eq!(body["members"][0]["username"], "alice");
    assert_eq!(body["folderRules"][0]["shareName"], "Projects");
    assert_eq!(body["folderRules"][0]["rule"], "write");

    let events = drain_events(&mut rx, std::time::Duration::from_millis(400)).await;
    let group_event = events
        .iter()
        .find(|event| event.kind == "group.changed")
        .expect("group.changed event");
    assert_eq!(
        group_event.scope,
        EventScope::Admin,
        "group changes are admin-scoped"
    );
    let access_event = events
        .iter()
        .find(|event| event.kind == "access.changed" && event.data["userId"] == alice.user_id)
        .expect("access.changed event for alice");
    assert_eq!(
        access_event.scope,
        EventScope::User(alice.user_id),
        "access changes are user-scoped"
    );

    // Search + list.
    let (status, _, list) = server
        .request(
            "GET",
            "/api/groups?page=1&perPage=10&q=Editor",
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["id"], group_id);

    // Rename keeps the stable id and rules.
    let (status, _, renamed) = server
        .request(
            "PUT",
            &format!("/api/groups/{group_id}"),
            Some(json!({
                "name": "Editorial",
                "description": "Renamed group",
                "members": [alice.user_id],
                "folderRules": [{"shareName": "Projects", "rule": "read"}],
                "appRules": [{"appId": "files", "rule": "allow"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "rename group: {renamed}");
    assert_eq!(renamed["id"], group_id, "group id is stable across rename");
    assert_eq!(renamed["name"], "Editorial");
    assert_eq!(renamed["members"].as_array().unwrap().len(), 1);
    assert_eq!(renamed["folderRules"].as_array().unwrap().len(), 1);

    // Membership endpoints.
    let (status, _, memberships) = server
        .request(
            "GET",
            &format!("/api/users/{}/groups", alice.user_id),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(memberships["groupIds"][0], group_id);
    assert_eq!(memberships["groups"][0]["name"], "Editorial");
    let (status, _, updated) = server
        .request(
            "PUT",
            &format!("/api/users/{}/groups", bob.user_id),
            Some(json!({"groupIds": [group_id]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "set memberships: {updated}");
    assert_eq!(updated["groups"][0]["id"], group_id);
    let (status, _, removed) = server
        .request(
            "PUT",
            &format!("/api/users/{}/groups", bob.user_id),
            Some(json!({"groupIds": []})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["groups"].as_array().unwrap().len(), 0);

    // Direct user rules.
    let (status, _, direct) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", bob.user_id),
            Some(json!({
                "folderRules": [{"shareName": "Projects", "rule": "none"}],
                "appRules": [{"appId": "backup", "rule": "allow"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "direct rules: {direct}");
    assert_eq!(direct["folderRules"][0]["rule"], "none");
    assert_eq!(direct["appRules"][0]["appId"], "backup");

    // Invalid policy input is rejected.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/groups/{group_id}"),
            Some(json!({"name": "Broken", "folderRules": [{"shareName": "Projects", "rule": "read-write"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({"name": "Broken", "appRules": [{"appId": "admin", "rule": "allow"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Delete reports the impact and keeps users/shares/files.
    let (status, _, deleted) = server
        .request(
            "DELETE",
            &format!("/api/groups/{group_id}"),
            None,
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "delete group: {deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["affectedUsers"], 1);
    assert_eq!(deleted["affectedFolderRules"], 1);
    assert_eq!(deleted["affectedAppRules"], 1);
    let (status, _, _) = server
        .request(
            "GET",
            &format!("/api/groups/{group_id}"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, users) = server
        .request("GET", "/api/users?page=1&perPage=10", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(users["total"], 3, "users are retained after group deletion");
    let (status, _, shares) = server
        .request("GET", "/api/shares", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shares.as_array().unwrap().len(), 1);
    assert!(server
        .state
        .config
        .shared_dir()
        .join(
            shares[0]["path"]
                .as_str()
                .unwrap()
                .trim_start_matches("/shared/")
        )
        .is_dir());

    // Audit trail.
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_logs WHERE action LIKE 'group.%' OR action LIKE 'user.%'",
    )
    .fetch_all(&server.state.db)
    .await
    .expect("audit rows");
    for expected in [
        "group.create",
        "group.update",
        "group.members_update",
        "group.permissions_update",
        "group.delete",
        "user.groups_update",
        "user.access_update",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "missing audit action {expected}: {actions:?}"
        );
    }
}

/// Fetch the Effective Access explanation for a user as an administrator.
async fn effective_access_for(server: &TestServer, admin: &Session, user_id: i64) -> Value {
    let (status, _, body) = server
        .request(
            "GET",
            &format!("/api/users/{user_id}/effective-access"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn effective_access_precedence_and_legacy_fallback() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let _alice = server.login("alice", USER_PASSWORD).await;

    let _projects = create_share(&server, &admin, "Projects").await;
    let finance = create_share(&server, &admin, "Finance").await;
    let _archive = create_share(&server, &admin, "Archive").await;
    let finance_id = finance["id"].as_str().unwrap().to_string();

    // Legacy ACL: alice is a read member of Finance (pre-groups behaviour).
    let (status, _, _) = server
        .request(
            "PATCH",
            &format!("/api/shares/{finance_id}"),
            Some(json!({"members": [{"userId": _alice.user_id, "perms": "read"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let alice_id = _alice.user_id;
    // Viewers: read Projects + explicit deny on Finance. Editors: write Projects.
    let (status, _, viewers) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Viewers",
                "members": [alice_id],
                "folderRules": [
                    {"shareName": "Projects", "rule": "read"},
                    {"shareName": "Finance", "rule": "none"}
                ]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{viewers}");
    let (status, _, editors) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Editors",
                "members": [alice_id],
                "folderRules": [{"shareName": "Projects", "rule": "write"}],
                "appRules": [{"appId": "applications", "rule": "allow"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{editors}");

    let body = effective_access_for(&server, &admin, alice_id).await;
    let folders = body["folders"].as_array().unwrap();
    let find = |name: &str| {
        folders
            .iter()
            .find(|folder| folder["shareName"] == name)
            .unwrap_or_else(|| panic!("missing folder {name}: {folders:?}"))
    };
    // Strongest grant wins: group read + group write => write.
    let projects_row = find("Projects");
    assert_eq!(projects_row["level"], "write", "{projects_row}");
    let sources = projects_row["sources"].as_array().unwrap();
    assert!(sources.iter().any(|source| source["kind"] == "group"
        && source["groupName"] == "Project Editors"
        && source["rule"] == "write"));
    // Explicit No Access from a group beats the legacy read membership.
    let finance_row = find("Finance");
    assert_eq!(finance_row["level"], "none", "{finance_row}");
    assert!(finance_row["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["rule"] == "none" && source["groupName"] == "Project Viewers"));
    // No rules and no legacy membership on Archive: nothing grants access.
    let archive_row = find("Archive");
    assert_eq!(archive_row["level"], "none");
    assert_eq!(archive_row["fallback"], "none");

    // Direct explicit deny beats a stronger group grant.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{alice_id}/access-rules"),
            Some(json!({"folderRules": [{"shareName": "Projects", "rule": "none"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = effective_access_for(&server, &admin, alice_id).await;
    let projects_row = body["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["shareName"] == "Projects")
        .unwrap();
    assert_eq!(projects_row["level"], "none");
    assert!(projects_row["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["kind"] == "direct" && source["rule"] == "none"));

    // A direct read grant does not downgrade a stronger group write grant.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{alice_id}/access-rules"),
            Some(json!({"folderRules": [{"shareName": "Projects", "rule": "read"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = effective_access_for(&server, &admin, alice_id).await;
    let projects_row = body["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["shareName"] == "Projects")
        .unwrap();
    assert_eq!(projects_row["level"], "write");

    // Application defaults and group grants.
    let apps = body["apps"].as_array().unwrap();
    let app = |id: &str| {
        apps.iter()
            .find(|entry| entry["appId"] == id)
            .unwrap_or_else(|| panic!("missing app {id}: {apps:?}"))
    };
    assert_eq!(app("files")["allowed"], true);
    assert_eq!(app("applications")["allowed"], true);
    assert_eq!(app("applications")["readOnly"], true);
    assert_eq!(app("backup")["allowed"], false);

    // A direct application denial overrides the group allow.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{alice_id}/access-rules"),
            Some(json!({"appRules": [{"appId": "applications", "rule": "none"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = effective_access_for(&server, &admin, alice_id).await;
    let apps = body["apps"].as_array().unwrap();
    let applications = apps
        .iter()
        .find(|entry| entry["appId"] == "applications")
        .unwrap();
    assert_eq!(applications["allowed"], false);
}

#[tokio::test]
async fn migration_0004_applies_additively_and_preserves_legacy_access() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_dir = dir.path().join("data");
    std::fs::create_dir_all(&db_dir).expect("data dir");
    let db_path = db_dir.join("rast.db");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.expect("db");

    // Build a v0.1.8 database (migrations 1..3) through the real migrator so
    // the _sqlx_migrations checksums match what the current binary expects.
    let old_dir = dir.path().join("old-migrations");
    std::fs::create_dir_all(&old_dir).expect("old migrations dir");
    for (name, sql) in [
        ("0001_init.sql", include_str!("../migrations/0001_init.sql")),
        (
            "0002_appearance.sql",
            include_str!("../migrations/0002_appearance.sql"),
        ),
        (
            "0003_appearance_accent.sql",
            include_str!("../migrations/0003_appearance_accent.sql"),
        ),
    ] {
        std::fs::write(old_dir.join(name), sql).expect("write migration");
    }
    let old_migrator = sqlx::migrate::Migrator::new(old_dir)
        .await
        .expect("load old migrator");
    old_migrator.run(&pool).await.expect("run old migrations");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("versions");
    assert_eq!(applied, vec![1, 2, 3]);

    // Seed representative pre-upgrade data: a user, a shared folder, a legacy
    // membership and appearance settings.
    sqlx::query(
        "INSERT INTO users (username, display_name, password_hash, role, active,
             quota_bytes, must_change_password, created_at, updated_at)
         VALUES ('legacy', 'Legacy User', 'x', 'user', 1, 0, 0, 1, 1)",
    )
    .execute(&pool)
    .await
    .expect("user");
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'legacy'")
        .fetch_one(&pool)
        .await
        .expect("legacy user id");
    sqlx::query(
        "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
         VALUES ('share-legacy', 'Legacy Share', '', 'legacy', NULL, 1, 1)",
    )
    .execute(&pool)
    .await
    .expect("share");
    sqlx::query(
        "INSERT INTO share_members (share_id, user_id, perms) VALUES ('share-legacy', ?, 'write')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("member");
    sqlx::query("INSERT INTO user_appearance (user_id, theme, updated_at) VALUES (?, 'dark', 5)")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("appearance");

    // Upgrade with the current binary's embedded migrations.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("upgrade migrations");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("versions");
    assert_eq!(
        applied,
        vec![1, 2, 3, 4, 5],
        "0004 and 0005 are the new migrations"
    );
    // Migration 0005 runs on the same upgrade and records rule-mapping stats.
    for (key, expected) in [
        ("migration.0005.orphaned_rules", "0"),
        ("migration.0005.conflicts", "0"),
    ] {
        let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_one(&pool)
            .await
            .expect("migration 0005 stats");
        assert_eq!(value, expected, "{key}");
    }

    // Existing rows are intact; the new policy is empty.
    let theme: String = sqlx::query_scalar("SELECT theme FROM user_appearance WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("appearance row");
    assert_eq!(theme, "dark");
    let perms: String = sqlx::query_scalar("SELECT perms FROM share_members WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("member row");
    assert_eq!(perms, "write");
    let groups: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM groups")
        .fetch_one(&pool)
        .await
        .expect("groups");
    assert_eq!(groups, 0);

    // Effective access after the upgrade is exactly the legacy ACL.
    let user = rast::auth::AuthUser {
        id: user_id,
        username: "legacy".into(),
        display_name: "Legacy User".into(),
        role: Role::User,
        session_id: String::new(),
        quota_bytes: 0,
        must_change_password: false,
    };
    let access = rast::access::folder_access_by_name(&pool, &user, "Legacy Share")
        .await
        .expect("folder access");
    assert_eq!(access.level, Some(rast::storage::Perm::Write));
    assert_eq!(access.fallback.as_deref(), Some("legacy-acl"));

    // Invalid legacy share names are hidden, admins still bypass.
    let admin = rast::auth::AuthUser {
        id: 1,
        username: "admin".into(),
        display_name: "Admin".into(),
        role: Role::Admin,
        session_id: String::new(),
        quota_bytes: 0,
        must_change_password: false,
    };
    let admin_access = rast::access::folder_access_by_name(&pool, &admin, "Legacy Share")
        .await
        .expect("admin access");
    assert_eq!(admin_access.level, Some(rast::storage::Perm::Admin));
    pool.close().await;
}

#[tokio::test]
async fn group_folder_and_app_permissions_are_enforced() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    server
        .create_user("bob", "Bob-Password-123!", Role::User)
        .await;
    server
        .create_user("carol", "Carol-Password-123!", Role::User)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;
    let bob = server.login("bob", "Bob-Password-123!").await;
    let carol = server.login("carol", "Carol-Password-123!").await;

    create_share(&server, &admin, "Projects").await;
    create_share(&server, &admin, "Finance").await;

    // Project Editors: write on Projects, read on Finance (alice).
    let (status, _, editors) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Editors",
                "members": [alice.user_id],
                "folderRules": [
                    {"shareName": "Projects", "rule": "write"},
                    {"shareName": "Finance", "rule": "read"}
                ]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{editors}");
    // Project Viewers: read on Projects (bob).
    let (status, _, viewers) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Viewers",
                "members": [bob.user_id],
                "folderRules": [{"shareName": "Projects", "rule": "read"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{viewers}");
    let viewers_id = viewers["id"].as_i64().expect("viewers id");
    // Analytics Readers: read-only Applications + Backup Center (alice).
    let (status, _, analytics) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Analytics Readers",
                "members": [alice.user_id],
                "appRules": [
                    {"appId": "applications", "rule": "allow"},
                    {"appId": "backup", "rule": "allow"}
                ]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{analytics}");

    // Editor can upload into Projects.
    let (status, bytes) = server
        .request_raw(
            "POST",
            "/api/files/upload?dir=%2Fshared%2FProjects&name=notes.txt",
            b"group-policy-content".to_vec(),
            "text/plain",
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "editor upload: {}",
        String::from_utf8_lossy(&bytes)
    );

    // Viewer can list, search, preview/download.
    let (status, _, listing) = server
        .request(
            "GET",
            "/api/files?path=%2Fshared%2FProjects&page=1&perPage=50",
            None,
            bob.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "viewer listing: {listing}");
    assert!(
        listing["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "notes.txt"),
        "viewer sees the uploaded file: {listing}"
    );
    let (status, download) = server
        .request_raw(
            "GET",
            "/api/files/download?path=%2Fshared%2FProjects%2Fnotes.txt",
            Vec::new(),
            "application/octet-stream",
            bob.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(download, b"group-policy-content");
    let (status, _, search) = server
        .request(
            "GET",
            "/api/files/search?path=%2Fshared%2FProjects&q=notes",
            None,
            bob.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "viewer search: {search}");

    // Viewer cannot mutate.
    for (method, path, body) in [
        (
            "POST",
            "/api/files/rename",
            json!({"path": "/shared/Projects/notes.txt", "newName": "renamed.txt"}),
        ),
        (
            "POST",
            "/api/files/mkdir",
            json!({"path": "/shared/Projects/new"}),
        ),
        (
            "POST",
            "/api/files/delete",
            json!({"paths": ["/shared/Projects/notes.txt"]}),
        ),
    ] {
        let (status, _, denied) = server
            .request(method, path, Some(body), bob.headers_with_csrf())
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "viewer must not mutate via {path}: {denied}"
        );
    }
    let (status, _) = server
        .request_raw(
            "POST",
            "/api/files/upload?dir=%2Fshared%2FProjects&name=blocked.txt",
            b"x".to_vec(),
            "text/plain",
            bob.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "viewer upload denied");

    // Copy requires write on the destination (viewer only reads both sides).
    let (status, _, copy) = server
        .request(
            "POST",
            "/api/files/copy",
            Some(json!({"paths": ["/shared/Projects/notes.txt"], "destDir": "/shared/Finance"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert!(
        status == StatusCode::FORBIDDEN,
        "editor has read Finance but no write: {status} {copy}"
    );
    // Move requires write on both ends.
    let (status, _, mv) = server
        .request(
            "POST",
            "/api/files/move",
            Some(json!({"paths": ["/shared/Projects/notes.txt"], "destDir": "/shared/Finance"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert!(
        status == StatusCode::FORBIDDEN,
        "move needs write on both sides: {status} {mv}"
    );

    // Carol has no rules: Projects is hidden (roots + direct path 404).
    let (status, _, roots) = server
        .request("GET", "/api/files/roots", None, carol.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !roots
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root["path"] == "/shared/Projects"),
        "hidden share must not be listed: {roots}"
    );
    let (status, _, _) = server
        .request(
            "GET",
            "/api/files?path=%2Fshared%2FProjects&page=1&perPage=50",
            None,
            carol.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "hidden share returns 404");

    // Removing the membership revokes access immediately in the signed-in session.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/groups/{viewers_id}"),
            Some(json!({"name": "Project Viewers", "members": []})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = server
        .request(
            "GET",
            "/api/files?path=%2Fshared%2FProjects&page=1&perPage=50",
            None,
            bob.headers(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked membership applies immediately"
    );

    // Group app grants: read-only Application/Backup access.
    let (status, _, apps) = server
        .request("GET", "/api/apps?page=1&perPage=10", None, alice.headers())
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read-only application grant lists apps: {apps}"
    );
    let (status, _, apps_denied) = server
        .request("GET", "/api/apps?page=1&perPage=10", None, bob.headers())
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "default application access denied: {apps_denied}"
    );
    let (status, _, backups) = server
        .request(
            "GET",
            "/api/backups?page=1&perPage=10",
            None,
            alice.headers(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read-only backup grant lists backups: {backups}"
    );
    let (status, _, denied) = server
        .request(
            "POST",
            "/api/apps",
            Some(json!({"name": "probe", "compose": "services: {web: {image: nginx}}"})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read-only group cannot deploy: {denied}"
    );

    // Direct app denial blocks the API surface even though the default allows it.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", bob.user_id),
            Some(json!({"appRules": [{"appId": "files", "rule": "none"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = server
        .request("GET", "/api/files/roots", None, bob.headers())
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "denied File Station blocks the API"
    );

    // SSE filtering: a denied app receives no events for that app.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", carol.user_id),
            Some(json!({"appRules": [{"appId": "monitor", "rule": "none"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let request = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            header::COOKIE,
            carol.cookie.parse::<axum::http::HeaderValue>().unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        collect_sse(response.into_body(), std::time::Duration::from_millis(1200)),
    )
    .await
    .expect("sse timeout");
    assert!(seen.contains("event: hello"), "expected hello: {seen}");
    assert!(
        !seen.contains("event: monitor"),
        "denied app must not receive monitor events: {seen}"
    );

    // Allowing the app again re-enables deliveries.
    let (status, _, _) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", carol.user_id),
            Some(json!({"appRules": []})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let request = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            header::COOKIE,
            carol.cookie.parse::<axum::http::HeaderValue>().unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    let seen = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        collect_sse(response.into_body(), std::time::Duration::from_millis(1200)),
    )
    .await
    .expect("sse timeout");
    assert!(
        seen.contains("event: monitor"),
        "restored app receives monitor events: {seen}"
    );
}

// ---------------------------------------------------------------------------
// Audit hardening regressions (v0.2.0 → v0.3.0)
// ---------------------------------------------------------------------------

async fn sse_frames_for(server: &TestServer, session: &Session, window_ms: u64) -> String {
    let request = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            header::COOKIE,
            session.cookie.parse::<axum::http::HeaderValue>().unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    collect_sse(
        response.into_body(),
        std::time::Duration::from_millis(window_ms),
    )
    .await
}

async fn upload_status(
    server: &TestServer,
    session: &Session,
    dir: &str,
    name: &str,
) -> StatusCode {
    let path = format!(
        "/api/files/upload?dir={}&name={}",
        urlencode(dir),
        urlencode(name)
    );
    let (status, _) = server
        .request_raw(
            "POST",
            &path,
            b"fictional content".to_vec(),
            "text/plain",
            session.headers_with_csrf(),
        )
        .await;
    status
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Effective Access (admin view) must agree with what the API actually lets
/// each fictional account do: hidden ⇔ 404, read ⇔ list 200 + upload 403,
/// write ⇔ list 200 + upload 200 — for every (account, folder) pair, including
/// direct-deny conflicts, legacy ACL rows and a demoted share creator (F6).
#[tokio::test]
async fn effective_access_matches_api_decisions_for_every_account() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    for name in ["ada", "ben", "cara", "dina", "erin"] {
        server.create_user(name, USER_PASSWORD, Role::User).await;
    }
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let mut sessions = Vec::new();
    for name in ["ada", "ben", "cara", "dina", "erin"] {
        sessions.push((name, server.login(name, USER_PASSWORD).await));
    }
    let id = |name: &str| {
        sessions
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s.user_id)
            .unwrap()
    };

    let projects = create_share(&server, &admin, "Projects").await;
    let finance = create_share(&server, &admin, "Finance").await;
    let _archive = create_share(&server, &admin, "Archive").await;
    let finance_id = finance["id"].as_str().unwrap().to_string();
    let projects_id = projects["id"].as_str().unwrap().to_string();

    // Legacy ACL: erin is a write member of Finance (no group rules apply).
    let (status, _, body) = server
        .request(
            "PATCH",
            &format!("/api/shares/{finance_id}"),
            Some(json!({"members": [{"userId": id("erin"), "perms": "write"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    for (name, members, folder_rules) in [
        (
            "Project Editors",
            vec![id("ada"), id("dina")],
            json!([{"shareId": projects_id, "rule": "write"}, {"shareName": "Finance", "rule": "read"}]),
        ),
        (
            "Project Viewers",
            vec![id("ben")],
            json!([{"shareName": "Projects", "rule": "read"}, {"shareName": "Finance", "rule": "none"}]),
        ),
    ] {
        let (status, _, body) = server
            .request(
                "POST",
                "/api/groups",
                Some(json!({"name": name, "members": members, "folderRules": folder_rules})),
                admin.headers_with_csrf(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["folderRules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|rule| rule["shareId"].is_string() && rule["shareName"].is_string()),
            "rules return stable ids and current names: {body}"
        );
    }
    // Dina: direct No Access on Projects overrides her group write grant.
    let (status, _, body) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", id("dina")),
            Some(
                json!({"folderRules": [{"shareName": "Projects", "rule": "none"}], "appRules": []}),
            ),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut checked = 0;
    for (name, session) in &sessions {
        let review = effective_access_for(&server, &admin, session.user_id).await;
        assert_eq!(review["foldersAll"], 3, "{review}");
        for folder in review["folders"].as_array().unwrap() {
            let share = folder["shareName"].as_str().unwrap();
            let level = folder["level"].as_str().unwrap();
            let dir = format!("/shared/{share}");
            let (list_status, _, _) = server
                .request(
                    "GET",
                    &format!("/api/files?path={}&page=1&perPage=10", urlencode(&dir)),
                    None,
                    session.headers(),
                )
                .await;
            let upload = upload_status(&server, session, &dir, &format!("{name}.txt")).await;
            match level {
                "none" => {
                    assert_eq!(list_status, StatusCode::NOT_FOUND, "{name} {share}");
                    assert_eq!(upload, StatusCode::NOT_FOUND, "{name} {share}");
                }
                "read" => {
                    assert_eq!(list_status, StatusCode::OK, "{name} {share}");
                    assert_eq!(upload, StatusCode::FORBIDDEN, "{name} {share}");
                }
                "write" | "admin" => {
                    assert_eq!(list_status, StatusCode::OK, "{name} {share}");
                    assert_eq!(upload, StatusCode::OK, "{name} {share}");
                }
                other => panic!("unexpected level {other}"),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 15, "5 accounts × 3 folders cross-checked");

    // Spot-check the expected decisions and the conflict explanation.
    let dina = effective_access_for(&server, &admin, id("dina")).await;
    let dina_projects = dina["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["shareName"] == "Projects")
        .unwrap()
        .clone();
    assert_eq!(dina_projects["level"], "none");
    assert_eq!(dina_projects["conflict"]["deniedBy"][0]["kind"], "direct");
    assert_eq!(
        dina_projects["conflict"]["overriddenGrants"][0]["groupName"],
        "Project Editors"
    );
    let erin = effective_access_for(&server, &admin, id("erin")).await;
    let erin_finance = erin["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["shareName"] == "Finance")
        .unwrap()
        .clone();
    assert_eq!(erin_finance["level"], "write");
    assert_eq!(erin_finance["fallback"], "legacy-acl");

    // Search, level filter and pagination use the same evaluator.
    let (status, _, page) = server
        .request(
            "GET",
            &format!(
                "/api/users/{}/effective-access?q=proj&level=all&page=1&perPage=1",
                id("ada")
            ),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["foldersTotal"], 1);
    assert_eq!(page["folders"][0]["shareName"], "Projects");
    assert_eq!(page["folders"][0]["level"], "write");
    assert_eq!(
        page["apps"].as_array().unwrap().len(),
        6,
        "apps always complete"
    );
    let (_, _, none_only) = server
        .request(
            "GET",
            &format!(
                "/api/users/{}/effective-access?level=none&perPage=2",
                id("ben")
            ),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(
        none_only["foldersTotal"], 2,
        "Finance + Archive: {none_only}"
    );
    assert_eq!(none_only["levelCounts"]["read"], 1);

    // The caller's own view never lists hidden folders.
    let ben = &sessions.iter().find(|(n, _)| *n == "ben").unwrap().1;
    let (status, _, mine) = server
        .request("GET", "/api/access/me", None, ben.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let names: Vec<&str> = mine["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["shareName"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Projects"]);
}

/// F6: a former administrator who created a shared folder must not keep an
/// implicit owner grant after demotion (v0.1.8 never had one).
#[tokio::test]
async fn demoted_share_creator_has_no_implicit_access() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server
        .create_user("frank", USER_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let frank = server.login("frank", USER_PASSWORD).await;
    let _share = create_share(&server, &frank, "Frank Projects").await;

    let (status, _, body) = server
        .request(
            "PATCH",
            &format!("/api/users/{}", frank.user_id),
            Some(json!({"role": "user"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let frank = server.login("frank", USER_PASSWORD).await;
    let (status, _, _) = server
        .request(
            "GET",
            "/api/files?path=%2Fshared%2FFrank%20Projects&page=1&perPage=10",
            None,
            frank.headers(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "demoted creator keeps no access"
    );
    let review = effective_access_for(&server, &admin, frank.user_id).await;
    assert_eq!(review["folders"][0]["level"], "none", "{review}");
    assert_ne!(review["folders"][0]["fallback"], "owner");
}

/// F12/F13 support endpoints: searchable member pages (incl. deactivated
/// members) and a deletion impact computed with the real evaluator.
#[tokio::test]
async fn group_members_page_and_deletion_impact() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let mut ids = Vec::new();
    for index in 0..30 {
        let name = format!("member{index:02}");
        server.create_user(&name, USER_PASSWORD, Role::User).await;
        ids.push(
            users::find_by_username(&server.state.db, &name)
                .await
                .unwrap()
                .unwrap()
                .id,
        );
    }
    create_share(&server, &admin, "Projects").await;
    let (status, _, group) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Project Editors",
                "members": ids,
                "folderRules": [{"shareName": "Projects", "rule": "write"}],
                "appRules": [{"appId": "backup", "rule": "allow"}]
            })),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{group}");
    let group_id = group["id"].as_i64().unwrap();
    // Deactivate one member: it stays listed (flagged) for review/removal.
    let (status, _, _) = server
        .request(
            "PATCH",
            &format!("/api/users/{}", ids[3]),
            Some(json!({"active": false})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, page1) = server
        .request(
            "GET",
            &format!("/api/groups/{group_id}/members?page=1&perPage=25"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    assert_eq!(page1["total"], 30);
    assert_eq!(page1["inactiveTotal"], 1);
    assert_eq!(page1["items"].as_array().unwrap().len(), 25);
    let (_, _, page2) = server
        .request(
            "GET",
            &format!("/api/groups/{group_id}/members?page=2&perPage=25"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(page2["items"].as_array().unwrap().len(), 5);
    let inactive: Vec<&Value> = page1["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["items"].as_array().unwrap().iter())
        .filter(|m| m["active"] == false)
        .collect();
    assert_eq!(inactive.len(), 1);
    assert_eq!(inactive[0]["username"], "member03");
    let (_, _, search) = server
        .request(
            "GET",
            &format!("/api/groups/{group_id}/members?q=r0"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(search["total"], 10, "two-character search works: {search}");

    // Account search: short queries and the active filter.
    let (_, _, active_users) = server
        .request(
            "GET",
            "/api/users?q=member0&active=active&perPage=100",
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(
        active_users["total"], 9,
        "member00..09 minus the disabled one"
    );

    let (status, _, impact) = server
        .request(
            "GET",
            &format!("/api/groups/{group_id}/impact"),
            None,
            admin.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{impact}");
    assert_eq!(impact["members"], 30);
    assert_eq!(impact["folderRules"], 1);
    assert_eq!(impact["appRules"], 1);
    let first = &impact["users"][0];
    let changes = first["changes"].as_array().unwrap();
    assert!(changes.iter().any(|c| c["kind"] == "folder"
        && c["resource"] == "Projects"
        && c["before"] == "write"
        && c["after"] == "none"));
    assert!(changes.iter().any(|c| c["kind"] == "app"
        && c["resourceId"] == "backup"
        && c["before"] == "read-only"
        && c["after"] == "none"));
    // The preview never changes anything.
    let still: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = ?")
        .bind(group_id)
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    assert_eq!(still, 30, "impact preview is read-only");
}

/// F9: a pending forced password change is enforced by the server, not only
/// by the UI dialog.
#[tokio::test]
async fn forced_password_change_is_enforced_server_side() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("gwen", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let gwen = server.login("gwen", USER_PASSWORD).await;
    let (status, _, body) = server
        .request(
            "POST",
            &format!("/api/users/{}/password", gwen.user_id),
            Some(json!({"password": "Temporary-Password-1!", "mustChangePassword": true})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gwen = server.login("gwen", "Temporary-Password-1!").await;

    let (status, _, me) = server
        .request("GET", "/api/auth/me", None, gwen.headers())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user"]["mustChangePassword"], true);
    let (status, _, body) = server
        .request(
            "GET",
            "/api/files?path=%2F&page=1&perPage=5",
            None,
            gwen.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("password change required"));

    let (status, _, body) = server
        .request(
            "POST",
            "/api/auth/password",
            Some(json!({"currentPassword": "Temporary-Password-1!", "newPassword": "Gwen-New-Password-9!"})),
            gwen.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = server
        .request(
            "GET",
            "/api/files?path=%2F&page=1&perPage=5",
            None,
            gwen.headers(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "access restored after the change");
}

/// F8 + live revocation: monitor samples delivered over SSE carry container
/// inventory only for accounts allowed to use Applications, and a grant or
/// revocation applies to the next sample on an already-open stream.
#[tokio::test]
async fn sse_monitor_container_inventory_follows_current_access() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("hana", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let hana = server.login("hana", USER_PASSWORD).await;
    let sample = json!({
        "ts": 1,
        "containers": [{"id": "c0ffee", "name": "fictional-db", "cpuPercent": 1.0}],
        "components": []
    });

    // Open Hana's stream and read it frame by frame, waiting for specific
    // frames instead of sleeping (deterministic under a loaded test runner).
    let request = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            header::COOKIE,
            hana.cookie.parse::<axum::http::HeaderValue>().unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut buffer = String::new();
    async fn next_frame_matching(
        body: &mut Body,
        buffer: &mut String,
        predicate: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            while let Some(end) = buffer.find("\n\n") {
                let frame = buffer[..end].to_string();
                buffer.drain(..end + 2);
                if predicate(&frame) {
                    return frame;
                }
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for an SSE frame");
            match tokio::time::timeout(remaining, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        buffer.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                other => panic!("SSE stream ended early: {:?}", other.is_ok()),
            }
        }
    }
    let is_sample = |frame: &str| {
        frame.contains("event: monitor")
            && (frame.contains("\"ts\":1}") || frame.contains("\"ts\":1,"))
    };
    // The hello frame proves the subscription exists before publishing.
    next_frame_matching(&mut body, &mut buffer, |f| f.contains("event: hello")).await;

    // Restricted: no inventory.
    server.state.events.publish("monitor", sample.clone());
    let restricted = next_frame_matching(&mut body, &mut buffer, is_sample).await;
    assert!(
        !restricted.contains("fictional-db"),
        "restricted sample leaked inventory: {restricted}"
    );
    assert!(restricted.contains("containersRestricted"));

    // Grant Applications while the stream is open; the next sample includes it.
    let (status, _, response_body) = server
        .request(
            "PUT",
            &format!("/api/users/{}/access-rules", hana.user_id),
            Some(json!({"folderRules": [], "appRules": [{"appId": "applications", "rule": "allow"}]})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{response_body}");
    next_frame_matching(&mut body, &mut buffer, |f| {
        f.contains("event: access.changed")
    })
    .await;
    server.state.events.publish("monitor", sample.clone());
    let granted = next_frame_matching(&mut body, &mut buffer, is_sample).await;
    assert!(
        granted.contains("fictional-db"),
        "grant applies to the next sample: {granted}"
    );
    drop(body);

    // Admin streams are never redacted.
    let admin_frames = sse_frames_for(&server, &admin, 300).await;
    assert!(!admin_frames.contains("containersRestricted"));
}
