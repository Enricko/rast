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
