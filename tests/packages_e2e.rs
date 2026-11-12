//! App Center API: signed catalog checks (rollback/tamper refusal),
//! compatibility, permissions, default selection, consumer protection and
//! the audit trail. Uses a local `file://` catalog signed with a test key, so
//! no network access is needed. Real downloads are covered by
//! `packages::install` unit tests and the isolated verification run.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use http_body_util::BodyExt;
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{json, Value};
use tower::ServiceExt;

use rast::{
    api,
    auth::Role,
    config::{Config, FileConfig},
    packages::{install::Layout, manifest, shell, store, system::SystemInstall},
    state::AppState,
    users,
};

struct Signer {
    pair: Ed25519KeyPair,
}

impl Signer {
    fn new() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Self {
            pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
        }
    }
    fn config_key(&self) -> String {
        format!("e2e-test:{}", B64.encode(self.pair.public_key().as_ref()))
    }
    fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "keyId": "e2e-test",
            "algorithm": "ed25519",
            "signature": B64.encode(self.pair.sign(bytes).as_ref()),
        }))
        .unwrap()
    }
}

/// Fictional password of the throwaway accounts these tests create.
const TEST_PASSWORD: &str = "Secret-pass-123";

fn version(v: &str, min_rast: Option<&str>) -> Value {
    let artifact = |target: &str| {
        json!({
            "target": target,
            "url": format!("https://github.com/example/lua/releases/download/v{v}/lua-{v}-{target}.tar.gz"),
            "sha256": "a".repeat(64),
            "size": 1234,
            "format": "tar.gz",
            "stripComponents": 1,
            "provenance": {"kind": "built", "source": format!("https://www.lua.org/ftp/lua-{v}.tar.gz")}
        })
    };
    let mut m = json!({
        "schemaVersion": 1,
        "kind": "rast-package",
        "id": "lua",
        "type": "runtime",
        "name": "Lua",
        "description": "Lua scripting language",
        "publisher": {"id": "example", "name": "Example Publisher"},
        "version": v,
        "revision": 1,
        "runtime": {
            "executables": [{"name": "lua", "path": "bin/lua"}],
            "checks": [{"executable": "lua", "args": ["-v"], "expect": format!("Lua {v}")}]
        },
        "artifacts": [artifact("linux-x86_64"), artifact("linux-aarch64")]
    });
    if let Some(min) = min_rast {
        m["minRastVersion"] = min.into();
    }
    m
}

fn catalog(sequence: u64) -> Vec<u8> {
    catalog_expiring(sequence, "2099-01-01T00:00:00Z")
}

fn catalog_expiring(sequence: u64, expires_at: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "kind": "rast-catalog",
        "name": "E2E catalog",
        "publisher": {"id": "example", "name": "Example Publisher"},
        "sequence": sequence,
        "generatedAt": "2026-11-11T00:00:00Z",
        "expiresAt": expires_at,
        "allowedHosts": ["github.com"],
        "packages": [{
            "id": "lua", "type": "runtime", "name": "Lua",
            "description": "Lua scripting language",
            "channels": [{"id": "5.4", "label": "Lua 5.4", "status": "stable"}],
            "versions": [version("5.5.0", Some("99.0.0")), version("5.4.8", None), version("5.4.7", None)],
            "detected": [{"version": "5.4.9", "channel": "5.4", "state": "verifying",
                          "detectedAt": "2026-11-11T00:00:00Z"}]
        }]
    }))
    .unwrap()
}

struct Server {
    dir: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
    signer: Signer,
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new();
        let catalog_path = dir.path().join("mirror/catalog.json");
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        let cfg = Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.path().join("data")),
                storage_dir: Some(dir.path().join("storage")),
                public_url: Some("https://rast.test".into()),
                secure_cookies: Some(false),
                setup_token_required: Some(true),
                packages_catalog_url: Some(format!("file://{}", catalog_path.display())),
                packages_catalog_keys: Some(vec![signer.config_key()]),
                packages_check_interval_minutes: Some(0),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        let state = AppState::new(cfg).await.unwrap();
        let app = api::router(state.clone());
        Self {
            dir,
            state,
            app,
            signer,
        }
    }

    fn publish(&self, bytes: &[u8], signature: &[u8]) {
        let path = self.dir.path().join("mirror/catalog.json");
        std::fs::write(&path, bytes).unwrap();
        std::fs::write(path.with_extension("json.sig"), signature).unwrap();
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: HeaderMap,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        for (k, v) in headers.iter() {
            builder = builder.header(k, v);
        }
        let body = match body {
            Some(v) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn login(&self, username: &str, role: Role) -> HeaderMap {
        users::create(
            &self.state.db,
            &self.state.config,
            username,
            TEST_PASSWORD,
            role,
            None,
            None,
        )
        .await
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({"username": username, "password": TEST_PASSWORD}))
                    .unwrap(),
            ))
            .unwrap();
        let response = self.app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().unwrap());
        headers.insert(
            "x-csrf-token",
            body["csrfToken"].as_str().unwrap().parse().unwrap(),
        );
        headers
    }

    /// Simulate a finished install: files on disk + the database record.
    async fn record_installed(&self, v: &str) -> PathBuf {
        let m = manifest::parse(&serde_json::to_vec(&version(v, None)).unwrap()).unwrap();
        let root = self.state.packages.layout.version_dir("lua", v);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        write_script(&root.join("bin/lua"), &format!("Lua {v}"));
        let target = self.state.packages.host.target.clone();
        store::record_install(
            &self.state.db,
            store::NewVersion {
                manifest: &m,
                source: "official",
                source_repo: None,
                target: &target,
                artifact_url: &m.artifacts[0].url,
                artifact_sha256: &m.artifacts[0].sha256,
                release_tag: None,
                install_path: &root.display().to_string(),
                size_bytes: 10,
                check_output: &format!("lua -v: Lua {v}"),
                installed_by: "admin",
            },
        )
        .await
        .unwrap();
        root
    }

    async fn audit(&self, action: &str) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT result, object_path FROM audit_logs WHERE action = ? ORDER BY id",
        )
        .bind(action)
        .fetch_all(&self.state.db)
        .await
        .unwrap()
    }
}

fn write_script(path: &Path, prints: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\necho \"{prints}\"\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn lua(overview: &Value) -> &Value {
    overview["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "lua")
        .expect("lua package")
}

#[tokio::test]
async fn signed_catalog_is_verified_and_rollback_or_tampering_is_refused() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let viewer = server.login("viewer", Role::User).await;

    // Nothing loaded yet: stale, no packages.
    let (status, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["catalog"]["stale"], true);
    assert!(body["packages"].as_array().unwrap().is_empty());

    // Non-administrators are denied by default; mutations are admin-only.
    let (status, _) = server
        .request("GET", "/api/packages", None, viewer.clone())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .request("POST", "/api/packages/check", None, viewer.clone())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let bytes = catalog(5);
    server.publish(&bytes, &server.signer.sign(&bytes));
    let (status, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["catalog"]["sequence"], 5);
    assert_eq!(body["catalog"]["keyId"], "e2e-test");
    assert_eq!(body["catalog"]["officialKey"], false);
    assert_eq!(body["catalog"]["stale"], false);

    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert_eq!(pkg["official"], true);
    assert_eq!(pkg["verification"]["kind"], "signed-catalog");
    assert_eq!(pkg["latest"]["version"], "5.4.8");
    let newest = &pkg["versions"][0];
    assert_eq!(newest["version"], "5.5.0");
    assert_eq!(newest["compatible"], false);
    assert!(
        newest["reason"].as_str().unwrap().contains("99.0.0"),
        "{newest}"
    );
    assert_eq!(
        pkg["versions"][1]["artifact"]["target"],
        body["host"]["target"]
    );

    // Incompatible or unknown versions are refused before any download.
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/lua/install",
            Some(json!({"version": "5.5.0"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("99.0.0"),
        "{body}"
    );
    let (status, _) = server
        .request(
            "POST",
            "/api/packages/lua/install",
            Some(json!({"version": "1.0.0"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An older sequence (rollback) is refused; the verified catalog stays.
    let old = catalog(4);
    server.publish(&old, &server.signer.sign(&old));
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(body["catalog"]["sequence"], 5);
    assert!(
        body["catalog"]["lastError"]
            .as_str()
            .unwrap()
            .contains("rollback"),
        "{body}"
    );
    assert_eq!(body["catalog"]["stale"], true);

    // A tampered document does not verify.
    let newer = catalog(6);
    let mut tampered = newer.clone();
    let at = tampered.iter().position(|b| *b == b'E').unwrap();
    tampered[at] = b'X';
    server.publish(&tampered, &server.signer.sign(&newer));
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(body["catalog"]["sequence"], 5);
    assert!(
        body["catalog"]["lastError"]
            .as_str()
            .unwrap()
            .contains("does not verify"),
        "{body}"
    );

    // A good newer catalog clears the error.
    server.publish(&newer, &server.signer.sign(&newer));
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(body["catalog"]["sequence"], 6);
    assert!(body["catalog"]["lastError"].is_null());

    let checks = server.audit("package.catalog_check").await;
    assert_eq!(
        checks.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["success", "failure", "failure", "success"]
    );

    // GitHub links are validated before any request.
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/github/inspect",
            Some(json!({"url": "https://gitlab.com/a/b"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn an_expired_catalog_blocks_installs_and_is_refused_on_check() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    // Accepted while valid ...
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let bytes = catalog_expiring(1, &soon);
    server.publish(&bytes, &server.signer.sign(&bytes));
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(body["catalog"]["sequence"], 1, "{body}");
    assert!(body["catalog"]["lastError"].is_null(), "{body}");
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;

    // ... but once expired it cannot drive installs ...
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/lua/install",
            Some(json!({"version": "5.4.8"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "{body}"
    );
    // ... and a source still serving it is reported, not trusted again.
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert!(
        body["catalog"]["lastError"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "{body}"
    );
    assert_eq!(body["catalog"]["expired"], true, "{body}");
    assert_eq!(body["catalog"]["stale"], true, "{body}");

    // A fresh catalog recovers.
    let fresh = catalog(2);
    server.publish(&fresh, &server.signer.sign(&fresh));
    let (_, body) = server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    assert_eq!(body["catalog"]["sequence"], 2);
    assert!(body["catalog"]["lastError"].is_null(), "{body}");
}

/// Store a GitHub preview as `inspect` leaves it (no network needed).
async fn stored_preview(server: &Server, repo: &str, manifest: &Value) -> String {
    let id = format!("preview-{}", manifest["id"].as_str().unwrap());
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO package_inspections (id, input_url, repo, release_tag, release_id, manifest_json,
                                          manifest_sha256, created_by, created_at, expires_at)
         VALUES (?, ?, ?, 'v1', 1, ?, '00', NULL, ?, ?)",
    )
    .bind(&id)
    .bind(format!("https://github.com/{repo}"))
    .bind(repo)
    .bind(manifest.to_string())
    .bind(now)
    .bind(now + 600_000)
    .execute(&server.state.db)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn imported_packages_are_rechecked_when_installed() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let mut squatter = version("5.4.8", None);
    squatter["publisher"] = json!({"id": "someone", "name": "Someone"});
    let preview = stored_preview(&server, "someone/lua", &squatter).await;
    let install = |id: String| {
        let admin = admin.clone();
        let server = &server;
        async move {
            server
                .request(
                    "POST",
                    "/api/packages/github/install",
                    Some(json!({"inspectionId": id, "approveSource": true})),
                    admin,
                )
                .await
        }
    };
    let message = |body: &Value| {
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    // Without a verified catalog RAST cannot rule out an official id.
    let (status, body) = install(preview.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(message(&body).contains("catalog first"), "{body}");

    // With it, the official id is refused at install time ...
    let bytes = catalog(1);
    server.publish(&bytes, &server.signer.sign(&bytes));
    server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    let (status, body) = install(preview).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(message(&body).contains("official catalog"), "{body}");

    // ... and so is another id that would provide an official command.
    let mut lookalike = squatter.clone();
    lookalike["id"] = "my-lua".into();
    let preview = stored_preview(&server, "someone/my-lua", &lookalike).await;
    let (status, body) = install(preview).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        message(&body).contains("command of an official package"),
        "{body}"
    );

    // Refused installs approve nothing.
    let approved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM package_sources")
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    assert_eq!(approved, 0);
}

#[tokio::test]
async fn versions_side_by_side_default_switch_and_protected_removal() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let bytes = catalog(1);
    server.publish(&bytes, &server.signer.sign(&bytes));
    server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;

    // Consumers need an installed package.
    let (status, _) = server
        .request(
            "POST",
            "/api/packages/consumers",
            Some(json!({"name": "site", "packageId": "lua"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 5.4.7 installed: an update to 5.4.8 is ready to install.
    let old_root = server.record_installed("5.4.7").await;
    let (status, body) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"version": "5.4.7"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let layout = &server.state.packages.layout;
    assert_eq!(
        std::fs::read_link(layout.current_link("lua")).unwrap(),
        PathBuf::from("5.4.7")
    );
    let shim = layout.bin_dir().join("lua");
    assert_eq!(
        std::process::Command::new(&shim).output().unwrap().stdout,
        b"Lua 5.4.7\n"
    );
    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert_eq!(pkg["updateState"], "ready");
    assert_eq!(pkg["updates"][0]["from"], "5.4.7");
    assert_eq!(pkg["updates"][0]["to"], "5.4.8");
    assert_eq!(pkg["updates"][0]["replacesDefault"], true);
    assert_eq!(body["counts"]["updatesReady"], 1);
    assert_eq!(
        pkg["installed"][0]["executables"][0]["path"],
        format!("{}/bin/lua", old_root.display())
    );

    // A pinned consumer and one that follows the default.
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/consumers",
            Some(json!({"name": "legacy-site", "packageId": "lua", "version": "5.4.7"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pinned_id = body["id"].as_i64().unwrap();
    let (status, _) = server
        .request(
            "POST",
            "/api/packages/consumers",
            Some(json!({"name": "scripts", "packageId": "lua"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Update installed alongside → only "new upstream version detected" left.
    server.record_installed("5.4.8").await;
    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert_eq!(pkg["installed"].as_array().unwrap().len(), 2);
    assert_eq!(pkg["updateState"], "detected");
    assert_eq!(pkg["detected"][0]["version"], "5.4.9");
    assert_eq!(pkg["defaultVersion"], "5.4.7");

    // Switch the default; the pinned consumer keeps 5.4.7.
    let (status, _) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"version": "5.4.8"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::process::Command::new(&shim).output().unwrap().stdout,
        b"Lua 5.4.8\n"
    );
    let (_, body) = server
        .request("GET", "/api/packages/lua", None, admin.clone())
        .await;
    let consumers = body["package"]["consumers"].as_array().unwrap();
    let legacy = consumers
        .iter()
        .find(|c| c["name"] == "legacy-site")
        .unwrap();
    assert_eq!(legacy["effectiveVersion"], "5.4.7");
    let scripts = consumers.iter().find(|c| c["name"] == "scripts").unwrap();
    assert_eq!(scripts["effectiveVersion"], "5.4.8");

    // Removal: confirmation required, pinned and default versions protected.
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/lua/versions/5.4.7",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = server
        .request(
            "DELETE",
            "/api/packages/lua/versions/5.4.7?confirm=5.4.7",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("legacy-site"),
        "{body}"
    );
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/lua/versions/5.4.8?confirm=5.4.8",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Roll back to the previous version, then unpin and remove 5.4.8.
    let (status, _) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"version": "5.4.7"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::process::Command::new(&shim).output().unwrap().stdout,
        b"Lua 5.4.7\n"
    );
    let (status, _) = server
        .request(
            "DELETE",
            &format!("/api/packages/consumers/{pinned_id}"),
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .request(
            "DELETE",
            "/api/packages/lua/versions/5.4.8?confirm=5.4.8",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!layout.version_dir("lua", "5.4.8").exists());
    assert!(old_root.join("bin/lua").exists());

    let activations = server.audit("package.activate").await;
    assert_eq!(activations.len(), 3);
    assert!(activations.iter().all(|a| a.0 == "success"));
    let removals = server.audit("package.remove").await;
    assert_eq!(
        removals.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["failure", "failure", "success"]
    );
    assert_eq!(removals[2].1, "lua 5.4.8");
}

#[tokio::test]
async fn a_system_runtime_is_listed_updated_alongside_and_restored() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let bytes = catalog(1);
    server.publish(&bytes, &server.signer.sign(&bytes));
    server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;

    // The server already has Lua 5.4.7 from apt.
    let sys_bin = server.dir.path().join("sys/usr/bin");
    std::fs::create_dir_all(&sys_bin).unwrap();
    write_script(&sys_bin.join("lua5.4"), "Lua 5.4.7");
    let system = SystemInstall {
        command: "lua".into(),
        path: sys_bin.join("lua").display().to_string(),
        real_path: sys_bin.join("lua5.4").display().to_string(),
        version: Some("5.4.7".into()),
        version_command: "lua -v".into(),
        output: "Lua 5.4.7".into(),
        source: "apt".into(),
        source_package: Some("lua5.4".into()),
        on_path: true,
        displaced: false,
        detected_at: 0,
    };
    let profile = server.dir.path().join("profile.d/rast-packages.sh");
    std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
    let packages = &server.state.packages;
    packages.set_system_for_tests(
        [("lua".to_string(), system.clone())].into_iter().collect(),
        profile.clone(),
    );

    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert_eq!(pkg["system"]["version"], "5.4.7");
    assert_eq!(pkg["system"]["sourcePackage"], "lua5.4");
    assert_eq!(
        body["counts"]["installed"], 1,
        "a system runtime counts as installed"
    );
    assert_eq!(pkg["updateState"], "ready");
    assert_eq!(pkg["updates"][0]["from"], "5.4.7");
    assert_eq!(pkg["updates"][0]["to"], "5.4.8");
    assert_eq!(pkg["updates"][0]["fromSystem"], true);
    assert_eq!(pkg["updates"][0]["replacesDefault"], true);
    assert_eq!(pkg["terminal"]["source"], "system");
    assert_eq!(pkg["terminal"]["version"], "5.4.7");
    assert_eq!(body["shell"]["active"], false);

    // The update is installed next to it and made the default.
    server.record_installed("5.4.8").await;
    let (status, body) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"version": "5.4.8"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert_eq!(pkg["updates"].as_array().unwrap().len(), 0, "{pkg}");
    assert_eq!(
        pkg["system"]["version"], "5.4.7",
        "the system runtime stays"
    );
    // Without the profile script, a terminal still finds the system one.
    assert_eq!(pkg["terminal"]["source"], "system");

    // With the PATH entry in place, the default's shim comes first.
    let data = server.state.config.data_dir.clone();
    let layout = Layout::new(server.state.config.packages_dir());
    shell::sync(true, &data, &layout, &profile).unwrap();
    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    assert_eq!(body["shell"]["active"], true);
    let pkg = lua(&body);
    assert_eq!(pkg["terminal"]["source"], "package-center");
    assert_eq!(pkg["terminal"]["version"], "5.4.8");
    let shim = layout.bin_dir().join("lua");
    assert_eq!(pkg["terminal"]["path"], shim.display().to_string());
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(". '{}'; lua", profile.display()))
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", sys_bin.display()))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Lua 5.4.8\n");

    // Malformed requests are refused.
    for body in [json!({}), json!({"version": "5.4.8", "system": true})] {
        let (status, _) = server
            .request(
                "PUT",
                "/api/packages/lua/default",
                Some(body),
                admin.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Back to the system runtime: no shim, no default; 5.4.8 stays installed.
    let (status, body) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"system": true})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!shim.exists());
    assert!(std::fs::symlink_metadata(layout.current_link("lua")).is_err());
    let (_, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    let pkg = lua(&body);
    assert!(pkg["defaultVersion"].is_null());
    assert_eq!(pkg["installed"].as_array().unwrap().len(), 1);
    assert_eq!(pkg["terminal"]["source"], "system");
    let audit = server.audit("package.activate").await;
    assert_eq!(
        audit.last().unwrap(),
        &("success".to_string(), "lua system".to_string())
    );

    // Without a system runtime there is nothing to go back to.
    packages.set_system_for_tests(Default::default(), profile);
    let (status, _) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"system": true})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn granted_groups_manage_packages_and_uninstall_removes_everything() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let viewer = server.login("viewer", Role::User).await;
    let keeper = server.login("keeper", Role::User).await;
    let bytes = catalog(1);
    server.publish(&bytes, &server.signer.sign(&bytes));
    server
        .request("POST", "/api/packages/check", None, admin.clone())
        .await;
    let old_root = server.record_installed("5.4.7").await;
    let new_root = server.record_installed("5.4.8").await;
    let (status, body) = server
        .request(
            "PUT",
            "/api/packages/lua/default",
            Some(json!({"version": "5.4.8"})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let shim = server.state.packages.layout.bin_dir().join("lua");
    assert!(shim.is_symlink());

    // Without a grant App Center is closed, uninstall included.
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/lua?confirm=lua",
            None,
            viewer.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A group granted App Center manages packages.
    let keeper_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'keeper'")
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    let (status, body) = server
        .request(
            "POST",
            "/api/groups",
            Some(json!({
                "name": "Runtime Keepers",
                "members": [keeper_id],
                "appRules": [{"appId": "packages", "rule": "allow"}]
            })),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .request("GET", "/api/packages", None, keeper.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["canManage"], true);
    let (_, body) = server
        .request("GET", "/api/packages", None, viewer.clone())
        .await;
    assert_ne!(body["canManage"], true);

    // Uninstall must be confirmed and is refused while a consumer uses it.
    let (status, _) = server
        .request("DELETE", "/api/packages/lua", None, keeper.clone())
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/consumers",
            Some(json!({"name": "site", "packageId": "lua", "version": "5.4.7"})),
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let consumer = body["id"].as_i64().unwrap();
    let (status, body) = server
        .request(
            "DELETE",
            "/api/packages/lua?confirm=lua",
            None,
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(old_root.exists() && new_root.exists(), "nothing removed");
    server
        .request(
            "DELETE",
            &format!("/api/packages/consumers/{consumer}"),
            None,
            keeper.clone(),
        )
        .await;

    let (status, body) = server
        .request(
            "DELETE",
            "/api/packages/lua?confirm=lua",
            None,
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!old_root.exists() && !new_root.exists());
    assert!(!shim.exists() && !shim.is_symlink(), "commands removed");
    assert!(!server.state.packages.layout.package_dir("lua").exists());
    let (_, body) = server
        .request("GET", "/api/packages", None, keeper.clone())
        .await;
    let pkg = lua(&body);
    assert!(pkg["installed"].as_array().unwrap().is_empty(), "{pkg}");
    assert!(pkg["defaultVersion"].is_null());
    let audit = server.audit("package.uninstall").await;
    assert_eq!(
        audit.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
        ["failure", "success"]
    );
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/lua?confirm=lua",
            None,
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing left to uninstall");

    // Approving a new GitHub source stays administrator-only.
    let (status, _) = server
        .request(
            "POST",
            "/api/packages/github/install",
            Some(json!({"inspectionId": "x", "approveSource": true})),
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Any package manager may cancel a package job, others cannot see it.
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/lua/install",
            Some(json!({"version": "5.4.8", "makeDefault": true})),
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job = body["jobId"].as_str().unwrap().to_string();
    let (status, _) = server
        .request(
            "POST",
            &format!("/api/jobs/{job}/cancel"),
            None,
            viewer.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = server
        .request(
            "POST",
            &format!("/api/jobs/{job}/cancel"),
            None,
            keeper.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn optional_apps_are_uninstalled_and_installed_in_package_center() {
    let server = Server::start().await;
    let admin = server.login("admin", Role::Admin).await;
    let viewer = server.login("viewer", Role::User).await;

    let (status, body) = server
        .request("GET", "/api/packages", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::OK);
    let apps = body["apps"].as_array().unwrap();
    let app = |id: &str| apps.iter().find(|a| a["id"] == id).cloned().unwrap();
    assert_eq!(app("launcher")["name"], "Applications");
    assert_eq!(app("applications")["name"], "Docker");
    assert_eq!(app("applications")["removable"], true);
    assert_eq!(app("backup")["installed"], true);
    assert_eq!(app("files")["removable"], false);

    // Built-in apps cannot be uninstalled; the confirmation is required.
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/apps/files?confirm=files",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .request("DELETE", "/api/packages/apps/backup", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .request(
            "DELETE",
            "/api/packages/apps/backup?confirm=backup",
            None,
            viewer.clone(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "App Center is closed to viewers"
    );

    let (status, _) = server
        .request("GET", "/api/backups", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .request(
            "DELETE",
            "/api/packages/apps/backup?confirm=backup",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);

    // Uninstalled: closed for administrators too, and reported as such.
    let (status, _) = server
        .request("GET", "/api/backups", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, me) = server
        .request("GET", "/api/auth/me", None, admin.clone())
        .await;
    assert_eq!(me["permissions"]["apps"]["backup"]["installed"], false);
    assert_eq!(me["permissions"]["apps"]["backup"]["allowed"], false);
    assert_eq!(me["features"]["backup"], false);
    assert_eq!(me["permissions"]["apps"]["files"]["installed"], true);
    let audit = server.audit("package.app_uninstall").await;
    assert_eq!(
        audit,
        [
            ("failure".to_string(), "File Station".to_string()),
            ("success".to_string(), "Backup Center".to_string())
        ]
    );

    // Uninstalling again changes nothing; installing opens it again.
    let (_, body) = server
        .request(
            "DELETE",
            "/api/packages/apps/backup?confirm=backup",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(body["changed"], false);
    let (status, body) = server
        .request(
            "POST",
            "/api/packages/apps/backup/install",
            None,
            admin.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = server
        .request("GET", "/api/backups", None, admin.clone())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, me) = server
        .request("GET", "/api/auth/me", None, admin.clone())
        .await;
    assert_eq!(me["permissions"]["apps"]["backup"]["installed"], true);
}
