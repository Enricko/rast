//! One HOME per user (ADR 0005): File Station requests of a one-HOME account
//! go to that account's file agent; legacy accounts stay in-process.
//!
//! The real router runs with a temporary database and storage tree. A fake
//! privileged helper answers `files_agent_start` with the socket of a real
//! `rast file-agent` started in this process (as the test user, so the
//! kernel-level isolation between `rast-u<id>` accounts is not exercised
//! here: that needs root and runs on the separate test host, see
//! `docs/remake/PROGRESS.md`).
//!
//! An account is in one-HOME mode when its HOME belongs to the uid of its
//! Linux identity. The tests give one account an identity with the test
//! process's own uid, which owns every HOME in the temporary tree.
//!
//! Run with:
//!     cargo test --test homefs_e2e
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

use rast::{
    api,
    auth::Role,
    config::{Config, FileConfig},
    state::AppState,
    users,
};

const PASSWORD: &str = "Secret-pass-123";
const TOKEN_CHAR: char = 'k';

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// What the fake helper saw.
#[derive(Default)]
struct HelperLog {
    starts: AtomicUsize,
    users: Mutex<Vec<i64>>,
    /// The user ids of `files_agent_stop` requests.
    stops: Mutex<Vec<i64>>,
    /// The members of each share's groups, as `share_members_set` set them:
    /// share id → (write, read).
    groups: Mutex<std::collections::HashMap<String, Members>>,
    /// Kernel quota as the fake helper reports it (`None`: the quota
    /// operations fail, like an older helper).
    kernel: Mutex<Option<Kernel>>,
}

/// What the fake kernel keeps: user quota state, used bytes and limits per
/// user id, and every `quota_set`.
#[derive(Default, Clone)]
struct Kernel {
    user: &'static str,
    group: &'static str,
    used: std::collections::HashMap<i64, u64>,
    limits: std::collections::HashMap<i64, (u64, u64)>,
    sets: Vec<(i64, u64, u64)>,
    /// Group limits per share id, and every `share_quota_set`.
    share_limits: std::collections::HashMap<String, (u64, u64)>,
    share_sets: Vec<(String, u64, u64)>,
}

/// Size of the fake filesystem.
const KERNEL_TOTAL: u64 = 1 << 40;

/// The (write, read) members of one share's groups.
type Members = (Vec<i64>, Vec<i64>);

impl HelperLog {
    /// Answer a quota operation from the fake kernel.
    fn quota(&self, request: &Value) -> Option<Value> {
        let mut kernel = self.kernel.lock().unwrap();
        let kernel = kernel.as_mut()?;
        let root = |group: &str| {
            json!({"device": "8:2", "fsType": "ext4", "totalBytes": KERNEL_TOTAL,
                   "user": kernel.user, "group": group, "project": "off",
                   "userGraceSecs": 604_800})
        };
        Some(match request["op"].as_str()? {
            "quota_status" => json!({"homes": root(kernel.group), "shared": root(kernel.group),
                                     "sameFilesystem": true}),
            "quota_report" => {
                let no_ids = Vec::new();
                let rows: Vec<Value> = request["userIds"]
                    .as_array()
                    .unwrap_or(&no_ids)
                    .iter()
                    .filter_map(Value::as_i64)
                    .map(|id| {
                        let (soft, hard) = kernel.limits.get(&id).copied().unwrap_or_default();
                        json!([
                            id,
                            kernel.used.get(&id).copied().unwrap_or(0),
                            soft,
                            hard,
                            0
                        ])
                    })
                    .collect();
                let shares: Vec<Value> = request["shareIds"]
                    .as_array()
                    .map(|ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(|id| {
                                let (soft, hard) =
                                    kernel.share_limits.get(id).copied().unwrap_or_default();
                                json!([id, 0, soft, hard, 0])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"users": rows, "shares": shares, "userQuota": kernel.user,
                       "groupQuota": kernel.group})
            }
            "share_quota_set" => {
                let id = request["shareId"].as_str()?.to_string();
                let soft = request["softBytes"].as_u64()?;
                let hard = request["hardBytes"].as_u64()?;
                kernel.share_limits.insert(id.clone(), (soft, hard));
                kernel.share_sets.push((id.clone(), soft, hard));
                json!({"shareId": id, "backend": "group"})
            }
            "quota_set" => {
                let id = request["userId"].as_i64()?;
                let soft = request["softBytes"].as_u64()?;
                let hard = request["hardBytes"].as_u64()?;
                kernel.limits.insert(id, (soft, hard));
                kernel.sets.push((id, soft, hard));
                json!({"userId": id})
            }
            _ => return None,
        })
    }

    /// Set the groups of a share and answer like the helper does.
    fn members_set(&self, request: &Value) -> Value {
        let ids = |key: &str| -> Vec<i64> {
            request[key]
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default()
        };
        let (write, read) = (ids("write"), ids("read"));
        let share = request["shareId"].as_str().unwrap_or_default().to_string();
        let mut groups = self.groups.lock().unwrap();
        let (old_write, old_read) = groups.get(&share).cloned().unwrap_or_default();
        let diff = |old: &[i64], new: &[i64]| {
            json!({
                "added": new.iter().filter(|id| !old.contains(id)).collect::<Vec<_>>(),
                "removed": old.iter().filter(|id| !new.contains(id)).collect::<Vec<_>>(),
                "removedOther": 0,
            })
        };
        let reply = json!({
            "shareId": share,
            "write": diff(&old_write, &write),
            "read": diff(&old_read, &read),
            "skipped": [],
        });
        groups.insert(share, (write, read));
        reply
    }
}

struct TestServer {
    dir: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
    helper: Arc<HelperLog>,
}

struct Session {
    cookie: String,
    csrf: String,
    id: i64,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("hf")
            .tempdir()
            .expect("temp dir");
        let file = FileConfig {
            data_dir: Some(dir.path().join("data")),
            storage_dir: Some(dir.path().join("storage")),
            public_url: Some("https://rast.test".into()),
            secure_cookies: Some(false),
            setup_token_required: Some(true),
            trusted_proxies: Some(vec!["127.0.0.1/32".into()]),
            packages_check_interval_minutes: Some(0),
            docker_enabled: Some(false),
            native_enabled: Some(true),
            native_helper_socket: Some(dir.path().join("h.sock")),
            linux_uid_base: Some(300_000),
            ..Default::default()
        };
        let cfg = Config::from_parts(None, file, |_| None).expect("config");
        let state = AppState::new(cfg).await.expect("state");
        let app = api::router(state.clone());
        Self {
            dir,
            state,
            app,
            helper: Arc::new(HelperLog::default()),
        }
    }

    fn socket(&self, user_id: i64) -> PathBuf {
        self.dir.path().join(format!("u{user_id}.sock"))
    }

    fn home(&self, username: &str) -> PathBuf {
        self.state.storage.home_dir(username)
    }

    /// Answer `files_agent_start` with the socket of the user's agent.
    fn start_helper(&self) {
        let listener =
            tokio::net::UnixListener::bind(self.dir.path().join("h.sock")).expect("helper socket");
        let log = self.helper.clone();
        let dir = self.dir.path().to_path_buf();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = log.clone();
                let dir = dir.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    if BufReader::new(read).read_line(&mut line).await.is_err() {
                        return;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    let reply = if request["op"] == "files_agent_start" {
                        let user_id = request["userId"].as_i64().unwrap_or(0);
                        log.starts.fetch_add(1, Ordering::SeqCst);
                        log.users.lock().unwrap().push(user_id);
                        json!({"id": request["id"], "ok": true, "data": {
                            "unit": format!("rast-files-u{user_id}"),
                            "socket": dir.join(format!("u{user_id}.sock")),
                            "token": TOKEN_CHAR.to_string().repeat(64),
                        }})
                    } else if request["op"] == "files_agent_stop" {
                        let user_id = request["userId"].as_i64().unwrap_or(0);
                        log.stops.lock().unwrap().push(user_id);
                        json!({"id": request["id"], "ok": true, "data": {
                            "unit": format!("rast-files-u{user_id}"),
                        }})
                    } else if let Some(data) = log.quota(&request) {
                        json!({"id": request["id"], "ok": true, "data": data})
                    } else if request["op"] == "share_members_set" {
                        json!({"id": request["id"], "ok": true, "data": log.members_set(&request)})
                    } else if ["share_group_ensure", "share_dir_prepare"]
                        .contains(&request["op"].as_str().unwrap_or_default())
                    {
                        json!({"id": request["id"], "ok": true, "data": {}})
                    } else {
                        json!({"id": request["id"], "ok": false, "error": {
                            "code": "invalid_request", "message": "not served by the test helper"
                        }})
                    };
                    let mut bytes = serde_json::to_vec(&reply).unwrap();
                    bytes.push(b'\n');
                    let _ = write.write_all(&bytes).await;
                });
            }
        });
    }

    /// Start the file agent of `username` (HOME and shared root of storage).
    async fn start_agent(&self, user_id: i64, username: &str) {
        let socket = self.socket(user_id);
        tokio::spawn(rast::homefs::agent::run(rast::homefs::agent::RunOptions {
            home: self.home(username),
            shared_root: self.state.config.shared_dir(),
            listen: rast::homefs::agent::Listen::Path(socket.clone()),
            token: TOKEN_CHAR.to_string().repeat(64),
            peer_uid: Some(uid()),
            idle: Duration::from_secs(600),
            stay_while_user_runs: false,
        }));
        for _ in 0..200 {
            if socket.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the file agent did not start");
    }

    async fn user(&self, username: &str, role: Role, quota: i64) -> Session {
        users::create(
            &self.state.db,
            &self.state.config,
            username,
            PASSWORD,
            role,
            None,
            Some(quota),
        )
        .await
        .expect("create user");
        std::fs::create_dir_all(self.home(username)).unwrap();
        self.login(username).await
    }

    /// Give `user_id` a Linux identity with this process's uid: its HOME
    /// (owned by this process) makes it a one-HOME account.
    async fn make_one_home(&self, session: &Session, username: &str) {
        sqlx::query(
            "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(session.id)
        .bind(format!("rast-u{}", session.id))
        .bind(i64::from(uid()))
        .bind(i64::from(uid()))
        .bind(self.home(username).to_string_lossy().to_string())
        .execute(&self.state.db)
        .await
        .expect("identity");
    }

    async fn login(&self, username: &str) -> Session {
        let (status, headers, body) = self
            .json(
                "POST",
                "/api/auth/login",
                None,
                Some(json!({"username": username, "password": PASSWORD})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "login failed: {body}");
        let cookie = headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("session cookie")
            .split(';')
            .next()
            .unwrap()
            .to_string();
        Session {
            cookie,
            csrf: body["csrfToken"].as_str().unwrap().to_string(),
            id: body["user"]["id"].as_i64().unwrap(),
        }
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        session: Option<&Session>,
        headers: HeaderMap,
        body: Body,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(session) = session {
            builder = builder
                .header(header::COOKIE, &session.cookie)
                .header("x-csrf-token", &session.csrf);
        }
        for (key, value) in headers.iter() {
            builder = builder.header(key, value);
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    async fn json(
        &self,
        method: &str,
        path: &str,
        session: Option<&Session>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut headers = HeaderMap::new();
        let body = match body {
            Some(value) => {
                headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let (status, headers, bytes) = self.raw(method, path, session, headers, body).await;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, value)
    }

    async fn get(&self, path: &str, session: &Session) -> (StatusCode, Value) {
        let (status, _, value) = self.json("GET", path, Some(session), None).await;
        (status, value)
    }

    async fn post(&self, path: &str, session: &Session, body: Value) -> (StatusCode, Value) {
        let (status, _, value) = self.json("POST", path, Some(session), Some(body)).await;
        (status, value)
    }

    async fn upload(
        &self,
        session: &Session,
        query: &str,
        bytes: &'static [u8],
    ) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, bytes.len().into());
        let (status, _, body) = self
            .raw(
                "POST",
                &format!("/api/files/upload?{query}"),
                Some(session),
                headers,
                Body::from(bytes),
            )
            .await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn used(&self, user_id: i64) -> (i64, i64) {
        let info = rast::storage::quota::info(&self.state.db, user_id)
            .await
            .unwrap();
        (info.used_bytes, info.reserved_bytes)
    }

    async fn wait_job(&self, session: &Session, job: &Value) -> Value {
        let id = job["jobId"].as_str().expect("job id");
        for _ in 0..200 {
            let (_, job) = self.get(&format!("/api/jobs/{id}"), session).await;
            if job["status"] == "done" || job["status"] == "failed" {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("job {id} did not finish");
    }

    fn starts(&self) -> usize {
        self.helper.starts.load(Ordering::SeqCst)
    }
}

/// One one-HOME account (alice, quota 100 bytes) and one legacy account
/// (bob), with alice's agent and the fake helper running.
async fn setup() -> (TestServer, Session, Session) {
    let server = TestServer::start().await;
    let alice = server.user("alice", Role::User, 100).await;
    let bob = server.user("bob", Role::User, 100).await;
    server.make_one_home(&alice, "alice").await;
    server.start_agent(alice.id, "alice").await;
    server.start_helper();
    (server, alice, bob)
}

fn names(listing: &Value) -> Vec<String> {
    listing["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn one_home_accounts_are_served_by_their_agent_and_legacy_ones_in_process() {
    let (server, alice, bob) = setup().await;
    std::fs::write(server.home("alice").join("note.txt"), b"hello").unwrap();
    std::fs::write(server.home("bob").join("bob.txt"), b"bob").unwrap();

    let (status, listing) = server.get("/api/files?path=/", &alice).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert_eq!(names(&listing), vec!["note.txt"]);
    // Same shapes: breadcrumbs, pagination and the server's quota.
    assert_eq!(listing["path"], "/");
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["quota"]["quotaBytes"], 100);
    assert_eq!(server.starts(), 1);

    // The legacy account never reaches the helper.
    let (status, listing) = server.get("/api/files?path=/", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&listing), vec!["bob.txt"]);
    let (status, _) = server
        .post("/api/files/delete", &bob, json!({"paths": ["/bob.txt"]}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server.starts(), 1);
    assert_eq!(*server.helper.users.lock().unwrap(), vec![alice.id]);

    // Reads go through the agent with their headers and bodies streamed.
    let (status, entry) = server.get("/api/files/stat?path=/note.txt", &alice).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["size"], 5);
    let (status, text) = server.get("/api/files/text?path=/note.txt", &alice).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let mut range = HeaderMap::new();
    range.insert(header::RANGE, "bytes=1-".parse().unwrap());
    let (status, headers, bytes) = server
        .raw(
            "GET",
            "/api/files/download?path=/note.txt",
            Some(&alice),
            range,
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, b"ello");
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 1-4/5");
    // The agent's file headers pass; the security headers are the server's.
    assert!(headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment"));
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        rast::storage::serve::PREVIEW_CSP
    );
    let (status, found) = server.get("/api/files/search?q=note", &alice).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["total"], 1);

    // Folders and names, audited like in-process.
    let (status, made) = server
        .post("/api/files/mkdir", &alice, json!({"path": "/docs"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    assert_eq!(made["kind"], "dir");
    let (status, renamed) = server
        .post(
            "/api/files/rename",
            &alice,
            json!({"path": "/note.txt", "newName": "hello.txt"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert!(server.home("alice").join("hello.txt").exists());
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_logs WHERE user_id = ? AND action LIKE 'file.%' ORDER BY id",
    )
    .bind(alice.id)
    .fetch_all(&server.state.db)
    .await
    .unwrap();
    // A ranged download that does not start at 0 is not a new download.
    assert_eq!(actions, vec!["file.mkdir", "file.rename"]);

    // Deleting, the recycle bin and the usage view are answered by the
    // agent too (task 1.4).
    let (status, trash) = server.get("/api/files/trash", &alice).await;
    assert_eq!(status, StatusCode::OK, "{trash}");
    assert_eq!(trash["total"], 0);
    // Other routes keep their in-process handlers.
    let (status, roots) = server.get("/api/files/roots", &alice).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(roots[0]["path"], "/");
}

#[tokio::test]
async fn paths_are_validated_and_the_root_file_system_is_not_forwarded() {
    let (server, alice, _bob) = setup().await;
    let outside = server.dir.path().join("secret.txt");
    std::fs::write(&outside, b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, server.home("alice").join("link.txt")).unwrap();
    for path in [
        "/api/files?path=/../storage",
        "/api/files?path=/%2e%2e/storage",
        "/api/files/stat?path=/a%00b",
    ] {
        let (status, _) = server.get(path, &alice).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    let (status, _) = server
        .get("/api/files/download?path=/link.txt", &alice)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post("/api/files/mkdir", &alice, json!({"path": "/../escape"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // `/@root` stays with the root file agent (super administrators only),
    // and mixing it with HOME is refused before anything is forwarded.
    let before = server.starts();
    let (status, _) = server.get("/api/files?path=/@root/etc", &alice).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post(
            "/api/files/copy",
            &alice,
            json!({"paths": ["/@root/etc/hosts"], "destDir": "/"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(server.starts(), before);
    // Mutations still need the CSRF token.
    let mut session = server.login("alice").await;
    session.csrf = "wrong".into();
    let (status, _) = server
        .post("/api/files/mkdir", &session, json!({"path": "/x"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!server.home("alice").join("x").exists());
}

#[tokio::test]
async fn uploads_reserve_and_account_the_quota() {
    let (server, alice, _bob) = setup().await;
    let (status, entry) = server.upload(&alice, "dir=/&name=a.bin", &[1u8; 60]).await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    assert_eq!(entry["path"], "/a.bin");
    assert_eq!(server.used(alice.id).await, (60, 0));
    // Over the quota: refused before a byte reaches the agent.
    let (status, _) = server.upload(&alice, "dir=/&name=b.bin", &[2u8; 60]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!server.home("alice").join("b.bin").exists());
    assert_eq!(server.used(alice.id).await, (60, 0));
    // Overwriting accounts the difference.
    let (status, _) = server
        .upload(&alice, "dir=/&name=a.bin&overwrite=true", &[3u8; 30])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server.used(alice.id).await, (30, 0));
    // A failed upload releases its reservation.
    let (status, _) = server
        .upload(&alice, "dir=/missing&name=c.bin", &[4u8; 10])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(server.used(alice.id).await, (30, 0));
}

#[tokio::test]
async fn chunked_uploads_are_limited_like_in_process_ones() {
    let (server, alice, _bob) = setup().await;
    let limit = rast::storage::chunked::MAX_UPLOADS_PER_USER;
    let mut ids = Vec::new();
    for index in 0..limit {
        let (status, init) = server
            .post(
                "/api/files/upload/init",
                &alice,
                json!({"dir": "/", "name": format!("f{index}.bin"), "size": 1}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{init}");
        ids.push(init["uploadId"].as_str().unwrap().to_string());
    }
    let one_more = json!({"dir": "/", "name": "one-more.bin", "size": 1});
    let (status, body) = server
        .post("/api/files/upload/init", &alice, one_more.clone())
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    // Refused before any quota is reserved.
    assert_eq!(server.used(alice.id).await, (0, limit as i64));
    // Cancelling one frees its place.
    let (status, _, _) = server
        .json(
            "DELETE",
            &format!("/api/files/upload?uploadId={}", ids[0]),
            Some(&alice),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    let (status, init) = server
        .post("/api/files/upload/init", &alice, one_more)
        .await;
    assert_eq!(status, StatusCode::OK, "{init}");
}

#[tokio::test]
async fn chunked_uploads_belong_to_their_account() {
    let (server, alice, bob) = setup().await;
    let (status, init) = server
        .post(
            "/api/files/upload/init",
            &alice,
            json!({"dir": "/", "name": "big.bin", "size": 40}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{init}");
    let id = init["uploadId"].as_str().unwrap().to_string();
    assert!(id.starts_with("usr-"), "{id}");
    assert_eq!(server.used(alice.id).await, (0, 40));
    // Too large for what is left: refused at init.
    let (status, _) = server
        .post(
            "/api/files/upload/init",
            &alice,
            json!({"dir": "/", "name": "huge.bin", "size": 61}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let chunk = |offset: usize, bytes: &'static [u8]| {
        let id = id.clone();
        let server = &server;
        let alice = &alice;
        async move {
            server
                .raw(
                    "POST",
                    &format!("/api/files/upload/chunk?uploadId={id}&offset={offset}"),
                    Some(alice),
                    HeaderMap::new(),
                    Body::from(bytes),
                )
                .await
                .0
        }
    };
    assert_eq!(chunk(0, &[7u8; 25]).await, StatusCode::OK);
    // Another account cannot write into it or complete it.
    let mut length = HeaderMap::new();
    length.insert(header::CONTENT_LENGTH, 15.into());
    let (status, _, _) = server
        .raw(
            "POST",
            &format!("/api/files/upload/chunk?uploadId={id}&offset=25"),
            Some(&bob),
            length,
            Body::from(&[0u8; 15][..]),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .post(
            &format!("/api/files/upload/complete?uploadId={id}"),
            &bob,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(chunk(25, &[7u8; 15]).await, StatusCode::OK);
    let (status, entry) = server
        .post(
            &format!("/api/files/upload/complete?uploadId={id}"),
            &alice,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    assert_eq!(entry["size"], 40);
    assert_eq!(server.used(alice.id).await, (40, 0));

    // A cancelled upload gives its reservation back.
    let (_, init) = server
        .post(
            "/api/files/upload/init",
            &alice,
            json!({"dir": "/", "name": "gone.bin", "size": 20}),
        )
        .await;
    let gone = init["uploadId"].as_str().unwrap().to_string();
    assert_eq!(server.used(alice.id).await, (40, 20));
    let (status, _, _) = server
        .json(
            "DELETE",
            &format!("/api/files/upload?uploadId={gone}"),
            Some(&alice),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(server.used(alice.id).await, (40, 0));
}

#[tokio::test]
async fn copy_and_move_run_as_jobs_and_account_home_bytes() {
    let (server, alice, _bob) = setup().await;
    let home = server.home("alice");
    std::fs::create_dir(home.join("docs")).unwrap();
    std::fs::write(home.join("a.bin"), [1u8; 30]).unwrap();
    rast::storage::quota::adjust_used(&server.state.db, alice.id, 30)
        .await
        .unwrap();

    let (status, job) = server
        .post(
            "/api/files/copy",
            &alice,
            json!({"paths": ["/a.bin"], "destDir": "/docs"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["items"], 1);
    let done = server.wait_job(&alice, &job).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["result"]["copied"], 1);
    assert!(done["result"].get("homeBytes").is_none(), "{done}");
    assert_eq!(server.used(alice.id).await, (60, 0));

    // A second copy does not fit in the quota (100 bytes), even with a
    // budget header from the browser: nothing is written.
    std::fs::create_dir(home.join("more")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-rast-quota-free", "999999".parse().unwrap());
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    let (status, _, job) = server
        .raw(
            "POST",
            "/api/files/copy",
            Some(&alice),
            headers,
            Body::from(json!({"paths": ["/a.bin", "/docs/a.bin"], "destDir": "/more"}).to_string()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let job: Value = serde_json::from_slice(&job).unwrap();
    let failed = server.wait_job(&alice, &job).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(std::fs::read_dir(home.join("more")).unwrap().count(), 0);
    assert_eq!(server.used(alice.id).await, (60, 0));

    // A move inside HOME changes nothing.
    let (_, job) = server
        .post(
            "/api/files/move",
            &alice,
            json!({"paths": ["/docs/a.bin"], "destDir": "/"}),
        )
        .await;
    let moved = server.wait_job(&alice, &job).await;
    assert_eq!(moved["status"], "failed", "a.bin already exists: {moved}");
    std::fs::create_dir(home.join("archive")).unwrap();
    let (_, job) = server
        .post(
            "/api/files/move",
            &alice,
            json!({"paths": ["/docs/a.bin"], "destDir": "/archive"}),
        )
        .await;
    let moved = server.wait_job(&alice, &job).await;
    assert_eq!(moved["status"], "done", "{moved}");
    assert!(home.join("archive/a.bin").exists());
}

#[tokio::test]
async fn the_recycle_bin_is_the_xdg_trash_of_home_and_of_each_shared_folder() {
    let (server, alice, bob) = setup().await;
    let home = server.home("alice");
    std::fs::create_dir(home.join("docs")).unwrap();
    std::fs::write(home.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(home.join("b.txt"), b"beta").unwrap();

    let (status, job) = server
        .post(
            "/api/files/delete",
            &alice,
            json!({"paths": ["/docs/a.txt", "/b.txt"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["items"], 2);
    let done = server.wait_job(&alice, &job).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["result"]["deleted"], 2);
    assert_eq!(done["result"]["trashed"], true);
    let trash = home.join(".local/share/Trash");
    assert_eq!(std::fs::read(trash.join("files/a.txt")).unwrap(), b"alpha");
    assert!(trash.join("info/b.txt.trashinfo").is_file());
    // Nothing lands in the legacy recycle bin.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trash")
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // Storage roots are refused before a job starts.
    let (status, _) = server
        .post("/api/files/delete", &alice, json!({"paths": ["/"]}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, list) = server.get("/api/files/trash?perPage=1", &alice).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["total"], 2);
    assert_eq!(list["perPage"], 1);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let (_, list) = server.get("/api/files/trash", &alice).await;
    let id_of = |path: &str| {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["originalPath"] == path)
            .unwrap_or_else(|| panic!("{path} not in {list}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let a = id_of("/docs/a.txt");
    let b = id_of("/b.txt");
    assert!(a.starts_with("h:"), "{a}");

    let (status, restored) = server
        .post("/api/files/trash/restore", &alice, json!({"ids": [a]}))
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["restored"][0]["path"], "/docs/a.txt");
    assert_eq!(std::fs::read(home.join("docs/a.txt")).unwrap(), b"alpha");
    let (status, removed) = server
        .post("/api/files/trash/delete", &alice, json!({"ids": [b]}))
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["deleted"], 1);
    assert_eq!(removed["bytes"], 4);
    assert!(!trash.join("files/b.txt").exists());
    for id in ["../x", "h:zz"] {
        let (status, _) = server
            .post("/api/files/trash/restore", &alice, json!({"ids": [id]}))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
    }
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_logs WHERE user_id = ? AND action LIKE 'file.%' ORDER BY id",
    )
    .bind(alice.id)
    .fetch_all(&server.state.db)
    .await
    .unwrap();
    assert_eq!(
        actions,
        vec!["file.delete", "file.restore", "file.trash_delete"]
    );

    // Shared folders: the entry stays in the folder's own trash, which is
    // hidden; a read-only folder refuses, a hidden one is 404.
    let admin = server.user("root-admin", Role::Admin, 0).await;
    for (name, perms) in [("Team", "write"), ("Board", "read")] {
        let (status, share) = server
            .post(
                "/api/shares",
                &admin,
                json!({"name": name, "members": [{"userId": alice.id, "perms": perms}]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{share}");
    }
    let shared = server.state.config.shared_dir();
    for name in ["Team", "Board"] {
        std::fs::write(shared.join(name).join("r.txt"), name).unwrap();
    }
    let (status, _) = server
        .post(
            "/api/files/delete",
            &alice,
            json!({"paths": ["/shared/Board/r.txt"]}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post(
            "/api/files/delete",
            &alice,
            json!({"paths": ["/shared/Team"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, job) = server
        .post(
            "/api/files/delete",
            &alice,
            json!({"paths": ["/shared/Team/r.txt"]}),
        )
        .await;
    let done = server.wait_job(&alice, &job).await;
    assert_eq!(done["status"], "done", "{done}");
    let share_trash = shared.join("Team").join(format!(".Trash-{}", uid()));
    assert_eq!(
        std::fs::read(share_trash.join("files/r.txt")).unwrap(),
        b"Team"
    );
    for viewer in [&alice, &admin] {
        let (status, listing) = server.get("/api/files?path=/shared/Team", viewer).await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert!(names(&listing).is_empty(), "{listing}");
    }
    // In process (the admin is a legacy account) the folder is no path
    // either, and its name cannot be taken.
    let trash_path = format!("/api/files?path=/shared/Team/.Trash-{}", uid());
    let (status, _) = server.get(&trash_path, &admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .post(
            "/api/files/mkdir",
            &admin,
            json!({"path": "/shared/Team/.Trash-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .post(
            "/api/files/mkdir",
            &alice,
            json!({"path": "/shared/Team/.Trash"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, list) = server.get("/api/files/trash", &alice).await;
    assert_eq!(list["total"], 1, "{list}");
    let item = &list["items"][0];
    assert_eq!(item["originalPath"], "/shared/Team/r.txt");
    let id = item["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("s:"), "{id}");
    // Without write access to the folder, the entry cannot be restored.
    let team = sqlx::query_scalar::<_, String>("SELECT id FROM shares WHERE name = 'Team'")
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    let (status, _, _) = server
        .json(
            "PATCH",
            &format!("/api/shares/{team}"),
            Some(&admin),
            Some(json!({"members": [{"userId": alice.id, "perms": "read"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .post("/api/files/trash/restore", &alice, json!({"ids": [id]}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Emptying still removes it.
    let (status, _, emptied) = server
        .json("DELETE", "/api/files/trash", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{emptied}");
    assert_eq!(emptied["deleted"], 1);
    assert!(!share_trash.join("files/r.txt").exists());

    // A legacy account keeps the in-process recycle bin.
    std::fs::write(server.home("bob").join("c.txt"), b"c").unwrap();
    let (_, job) = server
        .post("/api/files/delete", &bob, json!({"paths": ["/c.txt"]}))
        .await;
    assert_eq!(server.wait_job(&bob, &job).await["status"], "done");
    let (_, list) = server.get("/api/files/trash", &bob).await;
    assert_eq!(list["total"], 1);
    assert!(!server.home("bob").join(".local/share/Trash").exists());
}

#[tokio::test]
async fn the_usage_view_and_the_fallback_quota_follow_a_home_scan() {
    let (server, alice, bob) = setup().await;
    let home = server.home("alice");
    std::fs::create_dir_all(home.join(".cache")).unwrap();
    std::fs::write(home.join(".cache/c"), vec![1u8; 8192]).unwrap();
    std::fs::write(home.join("notes.txt"), b"hello").unwrap();
    let (status, usage) = server.get("/api/files/usage", &alice).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["quotaBackend"], "rast");
    assert_eq!(usage["quotaBytes"], 100);
    assert_eq!(usage["byType"]["documents"], 5);
    let locations = usage["byLocation"].as_object().unwrap();
    let total: i64 = locations.values().map(|v| v.as_i64().unwrap()).sum();
    assert_eq!(usage["usedBytes"].as_i64().unwrap(), total, "{usage}");
    assert!(locations["caches"].as_i64().unwrap() >= 0);
    // The scan is the account's usage now: apps in HOME count (D2).
    assert_eq!(server.used(alice.id).await.0, total);
    if total > 100 {
        let (status, _) = server.upload(&alice, "dir=/&name=x.txt", b"x").await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
    // A legacy account: RAST's accounting, without the new locations.
    let (status, usage) = server.get("/api/files/usage", &bob).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["quotaBackend"], "rast");
    assert!(usage["byLocation"]["files"].is_i64(), "{usage}");
}

#[tokio::test]
async fn kernel_user_quota_sets_d4_limits_and_counts_everything_the_identity_owns() {
    let (server, alice, bob) = setup().await;
    let admin = server.user("root-admin", Role::Admin, 0).await;
    let native = rast::native::client::NativeClient::from_config(&server.state.config);
    // An older helper (no quota operations): RAST accounting applies.
    assert!(rast::quota_sync::sync_once(&server.state, &native)
        .await
        .is_err());
    let (_, usage) = server.get("/api/files/usage", &alice).await;
    assert_eq!(usage["quotaBackend"], "rast");

    *server.helper.kernel.lock().unwrap() = Some(Kernel {
        user: "enforced",
        group: "enforced",
        used: [(alice.id, 10 << 20)].into_iter().collect(),
        ..Default::default()
    });
    let summary = rast::quota_sync::sync_once(&server.state, &native)
        .await
        .unwrap();
    assert_eq!(summary.accounts, 1, "only the one-HOME account");
    let sets = server
        .helper
        .kernel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .sets
        .clone();
    // D4: soft = quota, hard = soft + max(5 %, 1 GiB), in whole KiB as the
    // kernel keeps them (a quota of 100 bytes is one block).
    assert_eq!(sets, vec![(alice.id, 1024, 1024 + (1 << 30))]);
    assert_eq!(server.used(alice.id).await.0, 10 << 20);
    // Nothing changes: no second set.
    rast::quota_sync::sync_once(&server.state, &native)
        .await
        .unwrap();
    assert_eq!(
        server
            .helper
            .kernel
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .sets
            .len(),
        1
    );

    // The usage view: the kernel's number, and what HOME does not hold is
    // in shared folders.
    let (status, usage) = server.get("/api/files/usage", &alice).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["quotaBackend"], "filesystem");
    assert_eq!(usage["usedBytes"], 10 << 20);
    let home: i64 = ["files", "apps", "caches", "trash"]
        .iter()
        .map(|key| usage["byLocation"][key].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(
        usage["byLocation"]["shared"].as_i64().unwrap(),
        (10 << 20) - home
    );
    // Over quota: File Station refuses.
    let (status, _) = server.upload(&alice, "dir=/&name=x.txt", b"x").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // A legacy account keeps RAST's accounting.
    let (_, usage) = server.get("/api/files/usage", &bob).await;
    assert_eq!(usage["quotaBackend"], "rast");

    // A quota change reaches the kernel; an administrator is unlimited.
    let (status, _, _) = server
        .json(
            "PATCH",
            &format!("/api/users/{}", alice.id),
            Some(&admin),
            Some(json!({"quotaBytes": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    rast::quota_sync::sync_once(&server.state, &native)
        .await
        .unwrap();
    let sets = server
        .helper
        .kernel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .sets
        .clone();
    assert_eq!(sets.last(), Some(&(alice.id, 0, 0)));

    let (status, storage) = server.get("/api/settings/storage", &admin).await;
    assert_eq!(status, StatusCode::OK, "{storage}");
    assert_eq!(storage["quota"]["backend"], "filesystem");
    assert_eq!(storage["quota"]["userQuota"], "enforced");
    assert_eq!(storage["quota"]["shareLimits"], "filesystem, best effort");
}

#[tokio::test]
async fn shared_folder_size_limits_apply_to_file_station_writes() {
    let (server, alice, _bob) = setup().await;
    let admin = server.user("root-admin", Role::Admin, 0).await;
    let (status, share) = server
        .post(
            "/api/shares",
            &admin,
            json!({"name": "Team", "quotaBytes": 10,
                   "members": [{"userId": alice.id, "perms": "write"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{share}");
    assert_eq!(share["quotaBytes"], 10);
    assert_eq!(share["quotaEnforcedBy"], "RAST only");
    let (status, _) = server
        .post(
            "/api/shares",
            &admin,
            json!({"name": "Bad", "quotaBytes": -1}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A one-HOME account (agent) and a legacy one (in process).
    let (status, _) = server
        .upload(&alice, "dir=/shared/Team&name=big.txt", b"0123456789ab")
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, _) = server
        .upload(&admin, "dir=/shared/Team&name=big.txt", b"0123456789ab")
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, entry) = server
        .upload(&alice, "dir=/shared/Team&name=a.txt", b"012345")
        .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    // 6 of 10 bytes used: a chunked upload of 5 does not fit any more.
    let (status, _) = server
        .post(
            "/api/files/upload/init",
            &alice,
            json!({"dir": "/shared/Team", "name": "c.txt", "size": 5}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // A copy from HOME: the whole batch must fit, nothing is written.
    std::fs::write(server.home("alice").join("five.txt"), b"01234").unwrap();
    let (status, job) = server
        .post(
            "/api/files/copy",
            &alice,
            json!({"paths": ["/five.txt"], "destDir": "/shared/Team"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let failed = server.wait_job(&alice, &job).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    // The agent's reason reaches the browser.
    assert!(
        failed
            .to_string()
            .contains(rast::storage::share_quota::LIMIT_EXCEEDED),
        "{failed}"
    );
    let shared = server.state.config.shared_dir();
    assert!(!shared.join("Team/five.txt").exists());
    // Removing the limit lets it through.
    let (status, _, updated) = server
        .json(
            "PATCH",
            &format!("/api/shares/{}", share["id"].as_str().unwrap()),
            Some(&admin),
            Some(json!({"quotaBytes": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["quotaBytes"], Value::Null);
    assert!(updated.get("quotaEnforcedBy").is_none());
    let (_, job) = server
        .post(
            "/api/files/copy",
            &alice,
            json!({"paths": ["/five.txt"], "destDir": "/shared/Team"}),
        )
        .await;
    assert_eq!(server.wait_job(&alice, &job).await["status"], "done");

    // Group quota on the shared filesystem: the rw group gets the limit,
    // and the label says "filesystem, best effort".
    let (_, _, _) = server
        .json(
            "PATCH",
            &format!("/api/shares/{}", share["id"].as_str().unwrap()),
            Some(&admin),
            Some(json!({"quotaBytes": 1 << 30})),
        )
        .await;
    *server.helper.kernel.lock().unwrap() = Some(Kernel {
        user: "off",
        group: "enforced",
        ..Default::default()
    });
    let native = rast::native::client::NativeClient::from_config(&server.state.config);
    let summary = rast::quota_sync::sync_once(&server.state, &native)
        .await
        .unwrap();
    assert_eq!(summary.share_limits_set, 1);
    let sets = server
        .helper
        .kernel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .share_sets
        .clone();
    assert_eq!(
        sets,
        vec![(share["id"].as_str().unwrap().to_string(), 1 << 30, 2 << 30)]
    );
    let (_, list) = server.get("/api/shares", &admin).await;
    assert_eq!(list[0]["quotaEnforcedBy"], "filesystem, best effort");
    // The accounts keep RAST's accounting: user quota is off.
    let (_, usage) = server.get("/api/files/usage", &alice).await;
    assert_eq!(usage["quotaBackend"], "rast");
}

#[tokio::test]
async fn shared_folders_keep_the_rast_access_rules() {
    let (server, alice, _bob) = setup().await;
    let admin = server.user("root-admin", Role::Admin, 0).await;
    for (name, perms) in [("Team", "write"), ("Board", "read")] {
        let (status, share) = server
            .post(
                "/api/shares",
                &admin,
                json!({"name": name, "members": [{"userId": alice.id, "perms": perms}]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{share}");
    }
    let (status, _) = server
        .post(
            "/api/shares",
            &admin,
            json!({"name": "Secret", "members": []}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let shared = server.state.config.shared_dir();
    for name in ["Team", "Board", "Secret"] {
        std::fs::write(shared.join(name).join("readme.txt"), name).unwrap();
    }
    // `~/Shared` links are for apps: the agent never follows them, so a
    // link that alice points at another share changes nothing.
    let links = server.home("alice").join("Shared");
    std::fs::create_dir(&links).unwrap();
    for name in ["Team", "Board", "Secret"] {
        std::os::unix::fs::symlink(shared.join("Secret"), links.join(name)).unwrap();
    }

    let (status, listing) = server.get("/api/files?path=/shared/Team", &alice).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert_eq!(names(&listing), vec!["readme.txt"]);
    let (status, _, bytes) = server
        .raw(
            "GET",
            "/api/files/download?path=/shared/Team/readme.txt",
            Some(&alice),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"Team");
    // Only the server names containers: a header from the browser that
    // maps Team to Secret's container is dropped.
    let mut planted = rast::homefs::ShareRoots::default();
    planted
        .insert(
            "Team",
            rast::homefs::ShareRoot {
                id: "00000000-0000-4000-8000-000000000001".to_string(),
                name: "Team".to_string(),
                container: "Secret".to_string(),
            },
        )
        .unwrap();
    let mut headers = HeaderMap::new();
    planted.apply(&mut headers).unwrap();
    let (status, _, bytes) = server
        .raw(
            "GET",
            "/api/files/download?path=/shared/Team/readme.txt",
            Some(&alice),
            headers,
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"Team");
    // Listings start a HOME scan in the background (the RAST fallback);
    // the usage view scans on demand.
    let (_, before) = server.get("/api/files/usage", &alice).await;
    let (status, entry) = server
        .upload(&alice, "dir=/shared/Team&name=new.txt", b"team")
        .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    assert_eq!(entry["path"], "/shared/Team/new.txt");
    // Shared folders do not count against the quota.
    assert_eq!(server.used(alice.id).await.1, 0);
    let (_, after) = server.get("/api/files/usage", &alice).await;
    assert_eq!(before["usedBytes"], after["usedBytes"]);
    assert_eq!(
        server.used(alice.id).await.0,
        after["usedBytes"].as_i64().unwrap()
    );

    // Read-only: reads work, writes are refused by the server.
    let (status, _) = server.get("/api/files?path=/shared/Board", &alice).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .upload(&alice, "dir=/shared/Board&name=new.txt", b"x")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post(
            "/api/files/mkdir",
            &alice,
            json!({"path": "/shared/Board/x"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post(
            "/api/files/move",
            &alice,
            json!({"paths": ["/shared/Board/readme.txt"], "destDir": "/"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!shared.join("Board/new.txt").exists());

    // Hidden: 404 even though a link exists, like a missing folder.
    for path in [
        "/api/files?path=/shared/Secret",
        "/api/files/download?path=/shared/Secret/readme.txt",
        "/api/files?path=/shared/Nothing",
    ] {
        let (status, _) = server.get(path, &alice).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

/// Wait (bounded) until `check` holds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn share_groups_and_links_follow_access_and_revocations_restart_the_session() {
    let (server, alice, bob) = setup().await;
    let admin = server.user("root-admin", Role::Admin, 0).await;
    let (status, share) = server
        .post(
            "/api/shares",
            &admin,
            json!({"name": "Team", "members": [{"userId": alice.id, "perms": "write"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{share}");
    let share_id = share["id"].as_str().unwrap().to_string();
    let container: String = sqlx::query_scalar("SELECT container_path FROM shares WHERE id = ?")
        .bind(&share_id)
        .fetch_one(&server.state.db)
        .await
        .unwrap();
    let groups = || server.helper.groups.lock().unwrap().get(&share_id).cloned();
    let link = server.home("alice").join("Shared/Team");

    // Write access: the rw group, and a `~/Shared` link written by the
    // agent (bob has no Linux identity: in no group).
    eventually("alice in the rw group", || {
        groups() == Some((vec![alice.id], vec![]))
    })
    .await;
    eventually("the ~/Shared link", || std::fs::read_link(&link).is_ok()).await;
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::fs::canonicalize(server.state.config.shared_dir())
            .unwrap()
            .join(&container)
    );
    assert!(server.state.session_restarts.pending(bob.id).is_none());

    // Write → read is a revocation: the agent restarts at once, the
    // session after the warning; the link stays.
    let stops_before = server.helper.stops.lock().unwrap().len();
    let (status, _, body) = server
        .json(
            "PATCH",
            &format!("/api/shares/{share_id}"),
            Some(&admin),
            Some(json!({"members": [{"userId": alice.id, "perms": "read"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    eventually("alice in the ro group", || {
        groups() == Some((vec![], vec![alice.id]))
    })
    .await;
    eventually("the agent restart", || {
        server.helper.stops.lock().unwrap()[stops_before..].contains(&alice.id)
    })
    .await;
    let pending = server.state.session_restarts.pending(alice.id).unwrap();
    assert!(pending.forced, "{pending:?}");
    assert_eq!(pending.reason, "access_revoked");
    let (status, body) = server.get("/api/session/restart", &alice).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pending"]["forced"], true);
    assert!(std::fs::read_link(&link).is_ok());

    // No access: out of both groups, and the link goes.
    let (status, _, body) = server
        .json(
            "PATCH",
            &format!("/api/shares/{share_id}"),
            Some(&admin),
            Some(json!({"members": []})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    eventually("alice in no group", || groups() == Some((vec![], vec![]))).await;
    eventually("the link removed", || {
        std::fs::symlink_metadata(&link).is_err()
    })
    .await;

    // "Restart session now".
    let (status, body) = server.post("/api/session/restart", &alice, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(server.state.session_restarts.pending(alice.id).is_none());
}

#[tokio::test]
async fn deactivating_an_account_stops_its_file_agent() {
    let (server, alice, bob) = setup().await;
    let admin = server.user("root-admin", Role::Admin, 0).await;
    let carol = server.user("carol", Role::User, 0).await;
    let stops = || server.helper.stops.lock().unwrap().clone();
    let (status, _) = server.get("/api/files?path=/", &alice).await;
    assert_eq!(status, StatusCode::OK);

    // An edit that keeps the account active stops nothing.
    let (status, _, body) = server
        .json(
            "PATCH",
            &format!("/api/users/{}", carol.id),
            Some(&admin),
            Some(json!({"displayName": "Carol"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(stops().is_empty());

    // PATCH with active=false (ADR 0005, section 7).
    let (status, _, body) = server
        .json(
            "PATCH",
            &format!("/api/users/{}", alice.id),
            Some(&admin),
            Some(json!({"active": false})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stops(), vec![alice.id]);
    let (status, _) = server.get("/api/files?path=/", &alice).await;
    assert_ne!(status, StatusCode::OK);

    // DELETE deactivates too.
    let (status, _, body) = server
        .json(
            "DELETE",
            &format!("/api/users/{}", bob.id),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stops(), vec![alice.id, bob.id]);
}

#[tokio::test]
async fn without_the_helper_one_home_accounts_get_503_and_no_fallback() {
    let server = TestServer::start().await;
    let alice = server.user("alice", Role::User, 0).await;
    let bob = server.user("bob", Role::User, 0).await;
    server.make_one_home(&alice, "alice").await;
    std::fs::write(server.home("alice").join("note.txt"), b"x").unwrap();
    // No helper socket: the agent cannot be started.
    let (status, body) = server.get("/api/files?path=/", &alice).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, _) = server.upload(&alice, "dir=/&name=a.bin", b"abc").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!server.home("alice").join("a.bin").exists());
    assert_eq!(server.used(alice.id).await, (0, 0));
    // Legacy accounts do not need the helper.
    let (status, _) = server.get("/api/files?path=/", &bob).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_home_owned_by_another_uid_stays_legacy() {
    let server = TestServer::start().await;
    let carol = server.user("carol", Role::User, 0).await;
    // An identity whose uid does not own the HOME (the service owns it).
    sqlx::query(
        "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
         VALUES (?, 'rast-u9', 300009, 300009, ?, 0)",
    )
    .bind(carol.id)
    .bind(server.home("carol").to_string_lossy().to_string())
    .execute(&server.state.db)
    .await
    .unwrap();
    let (status, _) = server.get("/api/files?path=/", &carol).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .post("/api/files/mkdir", &carol, json!({"path": "/docs"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server.starts(), 0);
    assert!(Path::new(&server.home("carol")).join("docs").exists());
}

/// Wait up to `limit` for `file.changed` events of an external change, and
/// return the paths of those addressed to `user_id`.
async fn external_paths(
    events: &mut tokio::sync::broadcast::Receiver<rast::events::Event>,
    user_id: i64,
    limit: Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + limit;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        let mine = matches!(event.scope, rast::events::EventScope::User(id) if id == user_id);
        if event.kind == "file.changed" && mine && event.data["source"] == "external" {
            out.push(event.data["path"].as_str().unwrap_or_default().to_string());
        }
    }
    out
}

#[tokio::test]
async fn external_changes_reach_their_account_once() {
    let (server, alice, bob) = setup().await;
    let mut events = server.state.events.subscribe();
    // A first request starts the agent and the server follows its stream.
    let (status, _) = server.get("/api/files?path=/", &alice).await;
    assert_eq!(status, StatusCode::OK);
    for _ in 0..200 {
        if server.state.homefs.is_watching(alice.id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.state.homefs.is_watching(alice.id));

    // The server's watcher re-checks an account's mode after this long; the
    // setup made alice one-HOME only just now.
    tokio::time::sleep(rast::realtime::watch::ONE_HOME_TTL + Duration::from_millis(500)).await;

    // Hidden folders are reported by nobody: the agent does not watch them
    // and the server's watcher skips the whole one-HOME home.
    std::fs::create_dir_all(server.home("alice").join(".cache")).unwrap();
    std::fs::write(server.home("alice").join(".cache/blob"), b"x").unwrap();
    let paths = external_paths(&mut events, alice.id, Duration::from_millis(1500)).await;
    assert!(paths.is_empty(), "{paths:?}");

    // The agent sets its watches up after it answers: write until one shows.
    let mut seen = None;
    for attempt in 0..20 {
        let name = format!("/note-{attempt}.txt");
        std::fs::write(server.home("alice").join(&name[1..]), b"x").unwrap();
        let paths = external_paths(&mut events, alice.id, Duration::from_millis(1500)).await;
        if paths.contains(&name) {
            // Exactly once: the server's own watcher skips one-HOME homes.
            assert_eq!(
                paths.iter().filter(|path| **path == name).count(),
                1,
                "{paths:?}"
            );
            seen = Some(name);
            break;
        }
    }
    assert!(seen.is_some(), "no event for a change in a one-HOME home");

    // A legacy home is still watched by the server.
    let mut seen_bob = false;
    for attempt in 0..10 {
        let name = format!("/bob-{attempt}.txt");
        std::fs::write(server.home("bob").join(&name[1..]), b"x").unwrap();
        if external_paths(&mut events, bob.id, Duration::from_millis(1500))
            .await
            .contains(&name)
        {
            seen_bob = true;
            break;
        }
    }
    assert!(seen_bob, "no event for a change in a legacy home");
}
