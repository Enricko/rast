//! Native Linux apps API: authorization, request validation, schema and
//! lifecycle decisions that do not need a real Xpra session or the privileged
//! helper.
//!
//! Everything runs through the real Axum router with a temporary database and
//! storage tree (same harness pattern as `tests/api_e2e.rs`). The privileged
//! helper socket is deliberately missing, so session-starting paths are only
//! checked for their *authorization* and *graceful unavailability* — the live
//! Xpra behaviour is covered by the Playwright E2E harness in `tests/e2e/`
//! (Docker/native scenarios and performance measurements).
//!
//! Run with:
//!     cargo test --test native_linux_e2e
#![cfg(target_os = "linux")]

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
    state::AppState,
    users,
};

const ADMIN_PASSWORD: &str = "Admin-pass-123";
const USER_PASSWORD: &str = "Secret-pass-123";

struct TestServer {
    _dir: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(|file| file).await
    }

    /// Start the server with a native-enabled configuration; `customize` can
    /// change fields (for example to disable the native backend).
    async fn start_with(customize: impl FnOnce(FileConfig) -> FileConfig) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = customize(FileConfig {
            data_dir: Some(dir.path().join("data")),
            storage_dir: Some(dir.path().join("storage")),
            public_url: Some("https://rast.test".into()),
            secure_cookies: Some(false),
            setup_token_required: Some(true),
            trusted_proxies: Some(vec!["127.0.0.1/32".into()]),
            packages_check_interval_minutes: Some(0),
            // No Docker worker and no native helper are available in tests.
            docker_enabled: Some(false),
            native_enabled: Some(true),
            native_helper_socket: Some(dir.path().join("native.sock")),
            linux_uid_base: Some(300_000),
            linux_idle_minutes: Some(2),
            linux_session_memory_mb: Some(4096),
            linux_session_cpus: Some(2),
            linux_app_memory_mb: Some(2048),
            linux_app_cpus: Some(1),
            ..Default::default()
        });
        let cfg = Config::from_parts(None, file, |_| None).expect("config");
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
            .and_then(|value| value.to_str().ok())
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

    /// Seed one `linux_apps` row (the columns the native backend reads).
    async fn seed_app(&self, id: &str, backend: &str, status: &str, owner: i64) {
        sqlx::query(
            "INSERT INTO linux_apps \
             (id, name, format, file_name, status, build, visibility, backend, memory_mb, cpus, \
              launcher, created_by, created_at, updated_at) \
             VALUES (?, ?, 'deb', 'app.deb', ?, 1, 'users', ?, 2048, 1, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(format!("App {id}"))
        .bind(status)
        .bind(backend)
        .bind(json!({"argv": ["/usr/bin/xterm"], "workingDir": "/tmp"}).to_string())
        .bind(owner)
        .execute(&self.state.db)
        .await
        .expect("seed a Linux app");
    }
}

struct Session {
    cookie: String,
    csrf: String,
    #[allow(dead_code)]
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
        headers.insert(
            "x-csrf-token",
            self.csrf.parse().expect("valid csrf header"),
        );
        headers
    }
}

const NATIVE_ENDPOINTS: &[(&str, &str)] = &[
    ("GET", "/api/linux-apps/session"),
    ("POST", "/api/linux-apps/session/close"),
    ("POST", "/api/linux-apps/session/runs/0123456789abcdef"),
    ("POST", "/api/linux-apps/preview"),
    ("POST", "/api/linux-apps/demo/launch"),
    ("POST", "/api/linux-apps/demo/migrate"),
    ("POST", "/api/linux-apps/demo/migration/rollback"),
    ("POST", "/api/linux-apps/demo/migration/cleanup"),
];

#[tokio::test]
async fn native_endpoints_reject_unauthenticated_requests() {
    let server = TestServer::start().await;
    for (method, path) in NATIVE_ENDPOINTS {
        let (status, _, body) = server
            .request(method, path, Some(json!({})), HeaderMap::new())
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
    }
    // The Xpra proxy page is public at the router level and authenticates the
    // cookie itself. An unknown key is not resolved to an owner at all, so it
    // answers 404; it must never serve a page.
    let (status, _, _) = server
        .request("GET", "/linux-apps/session/abc123/", None, HeaderMap::new())
        .await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::UNAUTHORIZED,
        "unknown key answered {status}"
    );
}

#[tokio::test]
async fn native_admin_endpoints_reject_regular_users() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let alice = server.login("alice", USER_PASSWORD).await;
    for (method, path) in [
        ("POST", "/api/linux-apps/preview"),
        ("POST", "/api/linux-apps/demo/migrate"),
        ("POST", "/api/linux-apps/demo/migration/rollback"),
        ("POST", "/api/linux-apps/demo/migration/cleanup"),
    ] {
        let (status, _, body) = server
            .request(method, path, Some(json!({})), alice.headers_with_csrf())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    // Session queries are allowed for every signed-in user.
    let (status, _, body) = server
        .request("GET", "/api/linux-apps/session", None, alice.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["session"].is_null(),
        "no session should be running: {body}"
    );
}

#[tokio::test]
async fn native_session_state_is_reported_with_limits_and_no_helper() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let (status, _, body) = server
        .request("GET", "/api/linux-apps/session", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["session"].is_null(), "{body}");
    let limits = &body["limits"];
    assert_eq!(limits["idleMinutes"], 2);
    assert_eq!(limits["sessionMemoryMb"], 4096);
    assert_eq!(limits["sessionCpus"], 2);
    assert_eq!(limits["appMemoryMb"], 2048);
    assert_eq!(limits["appCpus"], 1);

    // No session: closing one and closing an unknown run are clean 404s, not
    // 500s (the helper is absent too, and must not be needed for this).
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/session/close",
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/session/runs/0123456789abcdef",
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn launch_validates_backend_and_app_state() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let alice = server.login("alice", USER_PASSWORD).await;

    // Unknown app → 404 (authz/lookup happens before any helper call).
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/missing/launch",
            Some(json!({})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A Docker-backed app must tell the caller to migrate first.
    server
        .seed_app("legacy", "docker", "ready", alice.user_id)
        .await;
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/legacy/launch",
            Some(json!({})),
            alice.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("migrate"),
        "the error should tell the admin to migrate: {body}"
    );

    // An installing app cannot be launched yet. Non-admins do not even see it
    // (404), so this check runs as the administrator.
    server
        .seed_app("native-pending", "native", "installing", admin.user_id)
        .await;
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/native-pending/launch",
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The administrator sees both seeded apps in the list.
    let (status, _, body) = server
        .request("GET", "/api/linux-apps", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["apps"]
        .as_array()
        .expect("apps")
        .iter()
        .map(|app| app["id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&"legacy"), "{ids:?}");
    assert!(ids.contains(&"native-pending"), "{ids:?}");
}

#[tokio::test]
async fn preview_validation_happens_before_the_helper_is_called() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;

    for body in [
        json!({}),
        json!({"path": "  "}),
        json!({"url": "http://example.com/app.deb"}),
    ] {
        let (status, _, response) = server
            .request(
                "POST",
                "/api/linux-apps/preview",
                Some(body.clone()),
                admin.headers_with_csrf(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {response}");
        assert_eq!(response["error"]["code"], "bad_request");
    }
    // A link without a usable file name is refused too.
    let (status, _, response) = server
        .request(
            "POST",
            "/api/linux-apps/preview",
            Some(json!({"url": "https://example.com/"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");

    // A File Station path that does not exist is a 404, not a helper failure.
    let (status, _, response) = server
        .request(
            "POST",
            "/api/linux-apps/preview",
            Some(json!({"path": "/missing.deb"})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{response}");
}

#[tokio::test]
async fn migrate_rollback_and_cleanup_are_admin_only_and_state_aware() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    server
        .seed_app("native-app", "native", "ready", admin.user_id)
        .await;

    // Migrating an app that already runs natively is a conflict, no helper
    // operation is attempted.
    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/native-app/migrate",
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("natively"),
        "{body}"
    );

    // Rollback/cleanup of an app with no migration to undo reports a client
    // error (precise) and never a 500.
    for path in [
        "/api/linux-apps/native-app/migration/rollback",
        "/api/linux-apps/native-app/migration/cleanup",
    ] {
        let (status, _, body) = server
            .request("POST", path, Some(json!({})), admin.headers_with_csrf())
            .await;
        assert!(
            status.is_client_error() || status == StatusCode::SERVICE_UNAVAILABLE,
            "{path}: expected a precise error, got {status}: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{path}: error has no message: {body}"
        );
    }
}

#[tokio::test]
async fn list_reports_the_native_contract_and_limits() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;
    let (status, _, body) = server
        .request("GET", "/api/linux-apps", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for key in [
        "apps",
        "sessions",
        "session",
        "limits",
        "dockerEnabled",
        "nativeEnabled",
    ] {
        assert!(body.get(key).is_some(), "list is missing {key}: {body}");
    }
    assert_eq!(body["nativeEnabled"], true);
    assert_eq!(body["dockerEnabled"], false);
    for key in [
        "minFreeMemoryBytes",
        "idleMinutes",
        "maxCpus",
        "admissionReserveMb",
        "sessionMemoryMb",
        "sessionCpus",
        "sessionTasks",
        "appMemoryMb",
        "appCpus",
        "appTasks",
    ] {
        assert!(
            body["limits"].get(key).is_some(),
            "limits is missing {key}: {body}"
        );
    }
}

#[tokio::test]
async fn native_disabled_reports_unavailable_for_session_starting_endpoints() {
    let server = TestServer::start_with(|mut file| {
        file.native_enabled = Some(false);
        file
    })
    .await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.seed_app("native-app", "native", "ready", 1).await;
    let admin = server.login("admin", ADMIN_PASSWORD).await;

    let (status, _, body) = server
        .request("GET", "/api/linux-apps/session", None, admin.headers())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["session"].is_null());

    let (status, _, body) = server
        .request(
            "POST",
            "/api/linux-apps/native-app/launch",
            Some(json!({})),
            admin.headers_with_csrf(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("native_enabled"),
        "{body}"
    );
}

#[tokio::test]
async fn native_schema_and_migration_are_applied() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    let pool = &server.state.db;

    for table in ["linux_identities", "linux_sessions", "linux_runs"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("schema query");
        assert_eq!(count, 1, "table {table} is missing");
    }
    let mut columns = Vec::new();
    let rows = sqlx::query("PRAGMA table_info(linux_apps)")
        .fetch_all(pool)
        .await
        .expect("table_info");
    for row in rows {
        use sqlx::Row;
        columns.push(row.get::<String, _>("name"));
    }
    for column in [
        "backend",
        "install_path",
        "previous_path",
        "launcher",
        "architecture",
        "package_version",
        "package_preexisting",
        "dependencies",
        "packages_added",
        "compatibility",
        "install_source",
        "migration_state",
        "migration_error",
        "migration_backup",
        "migrated_at",
        "docker_volume",
    ] {
        assert!(
            columns.iter().any(|name| name == column),
            "linux_apps is missing the native column {column}: {columns:?}"
        );
    }
    // Existing apps default to the Docker backend.
    server.seed_app("defaulted", "docker", "ready", 1).await;
    let backend: String =
        sqlx::query_scalar("SELECT backend FROM linux_apps WHERE id = 'defaulted'")
            .fetch_one(pool)
            .await
            .expect("backend");
    assert_eq!(backend, "docker");
    // The backend CHECK constraint refuses unknown values.
    let result = sqlx::query("UPDATE linux_apps SET backend = 'bogus' WHERE id = 'defaulted'")
        .execute(pool)
        .await;
    assert!(result.is_err(), "the backend CHECK constraint is missing");
}

#[tokio::test]
async fn native_session_rows_are_per_user_and_keys_are_unique() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    server
        .create_user("bob", "Secret-pass-456", Role::User)
        .await;
    let pool = &server.state.db;
    let alice_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'alice'")
        .fetch_one(pool)
        .await
        .expect("alice id");
    let bob_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'bob'")
        .fetch_one(pool)
        .await
        .expect("bob id");
    let now = 1_i64;
    sqlx::query(
        "INSERT INTO linux_sessions (user_id, key, display, port, unit, status, started_at, last_input, updated_at) \
         VALUES (?, ?, 100, 14500, 'rast-xpra-u1', 'ready', ?, ?, ?)",
    )
    .bind(alice_id)
    .bind("a".repeat(32))
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert alice session");
    // A second user cannot have the same key, and a duplicate user row is
    // refused (one shared session per person).
    let duplicate_key = sqlx::query(
        "INSERT INTO linux_sessions (user_id, key, display, port, unit, status, started_at, last_input, updated_at) \
         VALUES (?, ?, 101, 14501, 'rast-xpra-u2', 'ready', ?, ?, ?)",
    )
    .bind(bob_id)
    .bind("a".repeat(32))
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await;
    assert!(duplicate_key.is_err(), "session keys must be unique");
    let duplicate_user = sqlx::query(
        "INSERT INTO linux_sessions (user_id, key, display, port, unit, status, started_at, last_input, updated_at) \
         VALUES (?, ?, 102, 14502, 'rast-xpra-u1', 'ready', ?, ?, ?)",
    )
    .bind(alice_id)
    .bind("b".repeat(32))
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await;
    assert!(
        duplicate_user.is_err(),
        "one session per user is enforced by the primary key"
    );
}

#[tokio::test]
async fn another_users_session_cookie_cannot_open_an_xpra_key() {
    let server = TestServer::start().await;
    server
        .create_user("admin", ADMIN_PASSWORD, Role::Admin)
        .await;
    server.create_user("alice", USER_PASSWORD, Role::User).await;
    server
        .create_user("bob", "Secret-pass-456", Role::User)
        .await;
    let alice = server.login("alice", USER_PASSWORD).await;
    let bob = server.login("bob", "Secret-pass-456").await;
    // Authentication happens before the key is resolved: no cookie is a 401
    // (and does not reveal whether the key exists); a valid cookie for an
    // unknown key is a 404. A valid cookie for somebody else's live session is
    // a 403 — that branch needs a live NativeSessions entry, so it is covered
    // by the proxy unit tests and by the Playwright multi-user scenarios.
    let (status, _, _) = server
        .request(
            "GET",
            "/linux-apps/session/0123456789abcdef0123456789abcdef/",
            None,
            HeaderMap::new(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for headers in [bob.headers(), alice.headers()] {
        let (status, _, _) = server
            .request(
                "GET",
                "/linux-apps/session/0123456789abcdef0123456789abcdef/",
                None,
                headers,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // A malformed key is refused as well.
    let (status, _, _) = server
        .request("GET", "/linux-apps/session/not-a-key/", None, bob.headers())
        .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
        "malformed key answered {status}"
    );
    // An unauthenticated WebSocket upgrade attempt is refused the same way.
    let mut headers = HeaderMap::new();
    headers.insert(header::UPGRADE, "websocket".parse().unwrap());
    let (status, _, _) = server
        .request(
            "GET",
            "/linux-apps/session/0123456789abcdef0123456789abcdef/",
            None,
            headers,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Both accounts are valid server-side (sanity for the assertions above).
    assert_ne!(alice.user_id, bob.user_id);
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn state_is_send_sync_with_the_native_backend() {
    assert_send_sync::<AppState>();
    let _ = std::mem::size_of::<SocketAddr>();
}
