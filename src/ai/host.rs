//! What AI Agent asks of RAST: accounts and groups, File Station, commands,
//! live events and the audit log. Built into RAST, it calls RAST directly
//! ([`Host::BuiltIn`]); run as a Package Center app, it asks RAST's host API
//! over `host.sock` ([`Host::Remote`], see `crate::apphost::host_api`).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Body,
    http::{header, Method, StatusCode},
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::{net::UnixStream, sync::mpsc};

use super::run::Runs;
use crate::{
    audit::AuditLog,
    auth::AuthUser,
    error::{AppError, AppResult},
    events::EventScope,
    secrets::Secrets,
    state::AppState,
    storage::ops::{EntryDto, ListingDto, SearchDto, TextPreviewDto},
    terminal::{RunOutput, TerminalError},
};

/// AI Agent's own state: its database, key and running replies, and the
/// RAST it runs in.
#[derive(Clone)]
pub struct Agent {
    pub db: SqlitePool,
    pub secrets: Arc<Secrets>,
    pub ai: Arc<Runs>,
    pub host: Host,
    data_dir: PathBuf,
}

impl Agent {
    /// AI Agent inside RAST: RAST's database, key and data folder.
    pub fn built_in(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            secrets: state.secrets.clone(),
            ai: state.ai.clone(),
            data_dir: state.data_dir().to_path_buf(),
            host: Host::BuiltIn(Box::new(state.clone())),
        }
    }

    /// AI Agent as its own program: its own database, key and folder.
    pub fn remote(db: SqlitePool, secrets: Arc<Secrets>, data_dir: PathBuf, host: Remote) -> Self {
        Self {
            db,
            secrets,
            ai: Runs::new(),
            data_dir,
            host: Host::Remote(Arc::new(host)),
        }
    }

    /// Where AI Agent keeps its files (chat files, the browser's sockets).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// An account, as AI Agent sees it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub active: bool,
    #[serde(default)]
    pub groups: Vec<i64>,
}

impl Account {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
}

/// RAST, as AI Agent reaches it.
#[derive(Clone)]
pub enum Host {
    BuiltIn(Box<AppState>),
    Remote(Arc<Remote>),
}

impl Host {
    // ------------------------------------------------------------ events

    pub fn publish_scoped(&self, kind: &str, scope: EventScope, data: Value) {
        match self {
            Host::BuiltIn(state) => {
                state.events.publish_scoped(kind, scope, data);
            }
            Host::Remote(remote) => remote.publish(kind, scope, data),
        }
    }

    pub fn publish(&self, kind: &str, data: Value) {
        self.publish_scoped(kind, EventScope::Broadcast, data);
    }

    pub fn publish_user(&self, kind: &str, user_id: i64, data: Value) {
        self.publish_scoped(kind, EventScope::User(user_id), data);
    }

    pub fn publish_user_and_admin(&self, kind: &str, user_id: i64, data: Value) {
        self.publish_scoped(kind, EventScope::UserAndAdmin(user_id), data);
    }

    pub async fn audit(&self, log: AuditLog) {
        match self {
            Host::BuiltIn(state) => {
                crate::audit::record_with_events(&state.db, &state.events, log).await
            }
            Host::Remote(remote) => {
                let body = json!({
                    "action": log.action,
                    "userId": log.user_id,
                    "username": log.username,
                    "objectType": log.object_type,
                    "objectId": log.object_id,
                    "objectPath": log.object_path,
                    "result": log.result,
                    "detail": log.detail,
                    "ip": log.ip,
                });
                if let Err(error) = remote.call(Method::POST, "/v1/audit", Some(&body)).await {
                    tracing::warn!(%error, "the audit entry was not recorded");
                }
            }
        }
    }

    // ---------------------------------------------------------- accounts

    pub async fn accounts(&self) -> AppResult<Vec<Account>> {
        match self {
            Host::BuiltIn(state) => {
                let rows = sqlx::query_as::<_, (i64, String, String, String, i64)>(
                    "SELECT id, username, display_name, role, active FROM users
                     ORDER BY username COLLATE NOCASE",
                )
                .fetch_all(&state.db)
                .await?;
                let members =
                    sqlx::query_as::<_, (i64, i64)>("SELECT user_id, group_id FROM group_members")
                        .fetch_all(&state.db)
                        .await?;
                Ok(rows
                    .into_iter()
                    .map(|(id, username, display_name, role, active)| Account {
                        id,
                        username,
                        display_name,
                        role,
                        active: active != 0,
                        groups: members
                            .iter()
                            .filter(|(user, _)| *user == id)
                            .map(|(_, group)| *group)
                            .collect(),
                    })
                    .collect())
            }
            Host::Remote(remote) => {
                let answer = remote.get("/v1/users").await?;
                Ok(serde_json::from_value(answer["users"].clone()).unwrap_or_default())
            }
        }
    }

    pub async fn account(&self, id: i64) -> AppResult<Option<Account>> {
        match self {
            Host::BuiltIn(_) => Ok(self.accounts().await?.into_iter().find(|a| a.id == id)),
            Host::Remote(remote) => {
                match remote
                    .call(Method::GET, &format!("/v1/users/{id}"), None)
                    .await
                {
                    Ok((StatusCode::NOT_FOUND, _)) => Ok(None),
                    Ok((status, answer)) if status.is_success() => {
                        Ok(serde_json::from_value(answer["user"].clone()).ok())
                    }
                    Ok((_, answer)) => Err(AppError::unavailable(error_text(&answer))),
                    Err(error) => Err(AppError::unavailable(error)),
                }
            }
        }
    }

    pub async fn groups(&self) -> AppResult<Vec<Group>> {
        match self {
            Host::BuiltIn(state) => Ok(sqlx::query_as::<_, (i64, String)>(
                "SELECT id, name FROM groups ORDER BY name COLLATE NOCASE",
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|(id, name)| Group { id, name })
            .collect()),
            Host::Remote(remote) => {
                let answer = remote.get("/v1/groups").await?;
                Ok(serde_json::from_value(answer["groups"].clone()).unwrap_or_default())
            }
        }
    }

    /// The groups an account is in.
    pub async fn group_ids(&self, user_id: i64) -> AppResult<Vec<i64>> {
        Ok(self
            .account(user_id)
            .await?
            .map(|a| a.groups)
            .unwrap_or_default())
    }

    /// Whether the account may use another RAST app (such as `files`).
    pub async fn may_use_app(&self, user: &AuthUser, app_id: &str) -> bool {
        if user.is_admin() {
            return true;
        }
        match self {
            Host::BuiltIn(state) => crate::access::app_access(&state.db, user, app_id)
                .await
                .is_ok_and(|a| a.allowed),
            Host::Remote(remote) => remote
                .get(&format!("/v1/users/{}/apps/{app_id}", user.id))
                .await
                .is_ok_and(|a| a["allowed"] == true),
        }
    }

    // ------------------------------------------------------------ server

    /// RAST's own places on the server (data, storage, settings).
    pub fn private_paths(&self) -> Vec<PathBuf> {
        match self {
            Host::BuiltIn(state) => crate::apphost::host_api::private_paths(&state.config),
            Host::Remote(remote) => remote.info.private_paths.clone(),
        }
    }

    pub fn docker_enabled(&self) -> bool {
        match self {
            Host::BuiltIn(state) => state.config.docker_enabled,
            Host::Remote(remote) => remote.info.docker_enabled,
        }
    }

    /// An account's home folder on the server's disk.
    pub fn home_dir(&self, username: &str) -> PathBuf {
        let name = username.to_ascii_lowercase();
        match self {
            Host::BuiltIn(state) => state.storage.home_dir(&name),
            Host::Remote(remote) => remote.info.homes_root.join(name),
        }
    }

    /// Whether the terminal helper answers.
    pub async fn terminal_ready(&self) -> bool {
        match self {
            Host::BuiltIn(_) => crate::terminal::TerminalClient::default()
                .status()
                .await
                .is_ok(),
            Host::Remote(remote) => remote
                .call(Method::GET, "/v1/terminal/status", None)
                .await
                .is_ok_and(|(status, _)| status.is_success()),
        }
    }

    /// One command as root, through the terminal helper.
    pub async fn terminal_run(
        &self,
        user: &AuthUser,
        command: &str,
        secs: u32,
    ) -> Result<RunOutput, TerminalError> {
        match self {
            Host::BuiltIn(_) => {
                let name = user.username.to_ascii_lowercase();
                crate::terminal::TerminalClient::default()
                    .run(user.id, &name, command, secs)
                    .await
            }
            Host::Remote(remote) => remote.run("/v1/terminal/run", user, command, secs).await,
        }
    }

    pub async fn sandbox_allowed(&self, user_id: i64) -> bool {
        match self {
            Host::BuiltIn(state) => {
                state.config.docker_enabled && crate::sandbox::allowed(&state.db, user_id).await
            }
            Host::Remote(remote) => remote
                .get(&format!("/v1/sandbox/{user_id}"))
                .await
                .is_ok_and(|a| a["allowed"] == true),
        }
    }

    /// One command in the account's Terminal sandbox.
    pub async fn sandbox_run(
        &self,
        user: &AuthUser,
        command: &str,
        secs: u32,
    ) -> Result<RunOutput, TerminalError> {
        match self {
            Host::BuiltIn(state) => {
                crate::sandbox::ready_home(&state.db, &state.storage, user)
                    .await
                    .map_err(|e| TerminalError::Refused {
                        code: "home".into(),
                        message: e.public_message(),
                    })?;
                let name = user.username.to_ascii_lowercase();
                crate::sandbox::client(&state.config)
                    .run(user.id, &name, command, secs)
                    .await
            }
            Host::Remote(remote) => remote.run("/v1/sandbox/run", user, command, secs).await,
        }
    }

    // ------------------------------------------------------------- files

    pub async fn files_list(&self, user: &AuthUser, path: &str) -> Result<ListingDto, String> {
        use crate::storage::ops;
        match self {
            Host::BuiltIn(state) => ops::list(
                &state.db,
                &state.storage,
                user,
                path,
                None,
                "name",
                "asc",
                1,
                500,
            )
            .await
            .map_err(|e| e.public_message()),
            Host::Remote(remote) => {
                remote
                    .files("list", json!({"userId": user.id, "path": path}))
                    .await
            }
        }
    }

    pub async fn files_read(
        &self,
        user: &AuthUser,
        path: &str,
        max_bytes: u64,
    ) -> Result<TextPreviewDto, String> {
        use crate::storage::ops;
        match self {
            Host::BuiltIn(state) => {
                ops::text_preview(&state.db, &state.storage, user, path, Some(max_bytes))
                    .await
                    .map_err(|e| e.public_message())
            }
            Host::Remote(remote) => {
                remote
                    .files(
                        "read",
                        json!({"userId": user.id, "path": path, "maxBytes": max_bytes}),
                    )
                    .await
            }
        }
    }

    pub async fn files_search(
        &self,
        user: &AuthUser,
        path: &str,
        query: &str,
    ) -> Result<SearchDto, String> {
        use crate::storage::ops;
        match self {
            Host::BuiltIn(state) => {
                ops::search(&state.db, &state.storage, user, path, query, None, 1, 100)
                    .await
                    .map_err(|e| e.public_message())
            }
            Host::Remote(remote) => {
                remote
                    .files(
                        "search",
                        json!({"userId": user.id, "path": path, "query": query}),
                    )
                    .await
            }
        }
    }

    /// Write a text file (replacing one there); the entry and whether a
    /// file was there before.
    pub async fn files_write(
        &self,
        user: &AuthUser,
        path: &str,
        content: &str,
    ) -> Result<(EntryDto, bool), String> {
        use crate::storage::ops;
        match self {
            Host::BuiltIn(state) => {
                let public = |e: AppError| e.public_message();
                let (dir, name) = match path.trim_end_matches('/').rsplit_once('/') {
                    Some((dir, name)) if !name.is_empty() => {
                        (if dir.is_empty() { "/" } else { dir }, name)
                    }
                    _ => return Err("give the file's full path, like /notes/todo.txt".into()),
                };
                let existed = ops::stat(&state.db, &state.storage, user, path)
                    .await
                    .is_ok();
                let size = content.len() as u64;
                let mut plan =
                    ops::plan_upload(&state.db, &state.storage, user, dir, name, true, size)
                        .await
                        .map_err(public)?;
                use tokio::io::AsyncWriteExt;
                if let Err(error) = plan.file.write_all(content.as_bytes()).await {
                    ops::abort_upload(&state.db, user, plan).await;
                    return Err(format!("cannot write the file: {error}"));
                }
                let entry = ops::commit_upload(&state.db, &state.storage, user, plan, size)
                    .await
                    .map_err(public)?;
                crate::realtime::publish_commit(
                    &state.events,
                    &state.db,
                    user,
                    "upload",
                    std::slice::from_ref(&entry.path),
                    Some(&entry),
                    true,
                )
                .await;
                Ok((entry, existed))
            }
            Host::Remote(remote) => {
                let answer: Value = remote
                    .files(
                        "write",
                        json!({"userId": user.id, "path": path, "content": content}),
                    )
                    .await?;
                let entry = serde_json::from_value(answer["entry"].clone())
                    .map_err(|e| format!("RAST answered oddly: {e}"))?;
                Ok((entry, answer["existed"] == true))
            }
        }
    }

    pub async fn files_mkdir(&self, user: &AuthUser, path: &str) -> Result<EntryDto, String> {
        use crate::storage::ops;
        match self {
            Host::BuiltIn(state) => {
                let entry = ops::mkdir(&state.db, &state.storage, user, path)
                    .await
                    .map_err(|e| e.public_message())?;
                crate::realtime::publish_commit(
                    &state.events,
                    &state.db,
                    user,
                    "mkdir",
                    std::slice::from_ref(&entry.path),
                    Some(&entry),
                    false,
                )
                .await;
                Ok(entry)
            }
            Host::Remote(remote) => {
                let answer: Value = remote
                    .files("mkdir", json!({"userId": user.id, "path": path}))
                    .await?;
                serde_json::from_value(answer["entry"].clone())
                    .map_err(|e| format!("RAST answered oddly: {e}"))
            }
        }
    }
}

/// What RAST said when the app started (`GET /v1/info`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    #[serde(default)]
    pub rast_version: String,
    #[serde(default)]
    pub private_paths: Vec<PathBuf>,
    #[serde(default)]
    pub homes_root: PathBuf,
    #[serde(default)]
    pub docker_enabled: bool,
    #[serde(default)]
    pub public_url: Option<String>,
}

/// RAST's host API, over `host.sock`.
pub struct Remote {
    socket: PathBuf,
    token: String,
    pub info: HostInfo,
    events: mpsc::UnboundedSender<Value>,
}

/// The most RAST's answers may hold.
const ANSWER_LIMIT: usize = 64 * 1024 * 1024;

impl Remote {
    /// Connect to RAST: read its info, and start sending events in order.
    pub async fn connect(socket: PathBuf, token: String) -> Result<Self, String> {
        let (events, queue) = mpsc::unbounded_channel();
        let mut remote = Self {
            socket,
            token,
            info: HostInfo::default(),
            events,
        };
        let mut last = String::new();
        for _ in 0..50 {
            match remote.call(Method::GET, "/v1/info", None).await {
                Ok((status, answer)) if status.is_success() => {
                    remote.info = serde_json::from_value(answer)
                        .map_err(|e| format!("RAST's info could not be read: {e}"))?;
                    let sender = Sender {
                        socket: remote.socket.clone(),
                        token: remote.token.clone(),
                    };
                    tokio::spawn(sender.run(queue));
                    return Ok(remote);
                }
                Ok((_, answer)) => last = error_text(&answer),
                Err(error) => last = error,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!("RAST's host API did not answer: {last}"))
    }

    fn publish(&self, kind: &str, scope: EventScope, data: Value) {
        let scope = match scope {
            EventScope::Broadcast => json!({"to": "all"}),
            EventScope::Admin => json!({"to": "admins"}),
            EventScope::User(id) => json!({"to": "user", "userId": id}),
            EventScope::UserAndAdmin(id) => json!({"to": "userAndAdmins", "userId": id}),
            EventScope::Users(ids) => json!({"to": "users", "userIds": ids}),
            // Shares are File Station's.
            _ => return,
        };
        let _ = self
            .events
            .send(json!({"kind": kind, "scope": scope, "data": data}));
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        call(&self.socket, &self.token, method, path, body).await
    }

    /// RAST's AI Agent data (`GET /v1/migrate/ai`).
    pub async fn dump(&self) -> Result<Value, String> {
        match self.call(Method::GET, "/v1/migrate/ai", None).await? {
            (status, answer) if status.is_success() => Ok(answer),
            (_, answer) => Err(error_text(&answer)),
        }
    }

    /// Values sealed with RAST's key, opened (null where RAST cannot).
    pub async fn open_sealed(&self, values: &[String]) -> Result<Vec<Value>, String> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({ "values": values });
        match self
            .call(Method::POST, "/v1/migrate/ai/decrypt", Some(&body))
            .await?
        {
            (status, answer) if status.is_success() => {
                let opened = answer["values"].as_array().cloned().unwrap_or_default();
                if opened.len() == values.len() {
                    Ok(opened)
                } else {
                    Err("RAST opened a different number of values".into())
                }
            }
            (_, answer) => Err(error_text(&answer)),
        }
    }

    /// Tell RAST the data was taken.
    pub async fn handed_over(&self) -> Result<(), String> {
        match self.call(Method::POST, "/v1/migrate/ai/done", None).await? {
            (status, _) if status.is_success() => Ok(()),
            (_, answer) => Err(error_text(&answer)),
        }
    }

    async fn get(&self, path: &str) -> AppResult<Value> {
        match self.call(Method::GET, path, None).await {
            Ok((status, answer)) if status.is_success() => Ok(answer),
            Ok((status, answer)) if status == StatusCode::NOT_FOUND => {
                Err(AppError::NotFound("not found on RAST")).inspect_err(
                    |_| tracing::debug!(path, answer = %error_text(&answer), "host API: not found"),
                )
            }
            Ok((_, answer)) => Err(AppError::unavailable(error_text(&answer))),
            Err(error) => Err(AppError::unavailable(error)),
        }
    }

    async fn files<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        body: Value,
    ) -> Result<T, String> {
        let (status, answer) = self
            .call(Method::POST, &format!("/v1/files/{op}"), Some(&body))
            .await?;
        if !status.is_success() {
            return Err(error_text(&answer));
        }
        serde_json::from_value(answer).map_err(|e| format!("RAST answered oddly: {e}"))
    }

    async fn run(
        &self,
        path: &str,
        user: &AuthUser,
        command: &str,
        secs: u32,
    ) -> Result<RunOutput, TerminalError> {
        let body = json!({"userId": user.id, "command": command, "timeoutSecs": secs});
        let (status, answer) = self
            .call(Method::POST, path, Some(&body))
            .await
            .map_err(TerminalError::Io)?;
        if status.is_success() {
            return serde_json::from_value(answer)
                .map_err(|e| TerminalError::Io(format!("RAST answered oddly: {e}")));
        }
        let code = answer["error"]["code"].as_str().unwrap_or("");
        Err(match code {
            "terminal_off" => TerminalError::Off,
            "refused" => TerminalError::Refused {
                code: answer["error"]["detail"].as_str().unwrap_or("").to_string(),
                message: error_text(&answer),
            },
            _ if status == StatusCode::FORBIDDEN => TerminalError::Refused {
                code: "forbidden".into(),
                message: error_text(&answer),
            },
            _ => TerminalError::Io(error_text(&answer)),
        })
    }
}

/// Sends events to RAST in the order they were published, several at once
/// when they queue up (a streaming reply's pieces).
struct Sender {
    socket: PathBuf,
    token: String,
}

impl Sender {
    async fn run(self, mut queue: mpsc::UnboundedReceiver<Value>) {
        while let Some(first) = queue.recv().await {
            let mut batch = vec![first];
            while batch.len() < 200 {
                match queue.try_recv() {
                    Ok(next) => batch.push(next),
                    Err(_) => break,
                }
            }
            let body = json!({ "events": batch });
            for attempt in 0..3 {
                match call(
                    &self.socket,
                    &self.token,
                    Method::POST,
                    "/v1/events",
                    Some(&body),
                )
                .await
                {
                    Ok((status, _)) if status.is_success() => break,
                    Ok((_, answer)) => {
                        tracing::warn!(error = %error_text(&answer), "RAST refused events");
                        break;
                    }
                    Err(error) if attempt == 2 => {
                        tracing::warn!(%error, "events could not reach RAST")
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        }
    }
}

/// The message in a RAST error answer.
fn error_text(answer: &Value) -> String {
    answer["error"]["message"]
        .as_str()
        .or_else(|| answer["message"].as_str())
        .unwrap_or("RAST did not say why")
        .to_string()
}

async fn call(
    socket: &Path,
    token: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<(StatusCode, Value), String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot reach RAST: {e}"))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("cannot reach RAST: {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "rast")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .map_err(|e| e.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("RAST did not answer: {e}"))?;
    let status = response.status();
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), ANSWER_LIMIT)
        .await
        .map_err(|e| format!("RAST's answer could not be read: {e}"))?;
    let answer = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, answer))
}
