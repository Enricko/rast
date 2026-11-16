//! The server side of the per-user file agents: which accounts are in
//! one-HOME mode, starting their agents through the helper, sending them
//! requests, the quota reservations of uploads they are receiving, and the
//! realtime stream of each agent.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Response};
use futures::StreamExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventBus;
use crate::fileagent::{client, UPLOAD_TTL, WATCH_ROUTE};
use crate::native::client::NativeClient;
use crate::realtime::{self, Audience};
use crate::storage::safe_path::validate_component;
use crate::storage::LogicalPath;

use super::watch::MAX_PATHS_PER_LINE;

/// How the agent is named in error messages.
pub const AGENT: &str = "file agent";
/// Longest line accepted from a watch stream.
const MAX_WATCH_LINE: usize = 256 * 1024;

/// Bytes reserved in `storage_usage` for a chunked upload an agent is
/// receiving; committed or released when it completes, is cancelled or
/// expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub user_id: i64,
    pub bytes: i64,
    pub expires: Instant,
}

/// Connections to the per-user file agents.
pub struct HomeFs {
    /// `native_enabled` on Linux: without the helper every account is legacy.
    enabled: bool,
    native: NativeClient,
    homes_root: PathBuf,
    events: EventBus,
    agents: Mutex<HashMap<i64, (PathBuf, String)>>,
    watching: Arc<std::sync::Mutex<HashSet<i64>>>,
    reservations: std::sync::Mutex<HashMap<String, Reservation>>,
}

impl HomeFs {
    pub fn new(enabled: bool, native: NativeClient, homes_root: PathBuf, events: EventBus) -> Self {
        Self {
            enabled: enabled && cfg!(target_os = "linux"),
            native,
            homes_root,
            events,
            agents: Mutex::new(HashMap::new()),
            watching: Arc::new(std::sync::Mutex::new(HashSet::new())),
            reservations: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Whether the account is in one-HOME mode: its HOME directory belongs
    /// to the uid of its Linux identity. A missing identity, a missing HOME
    /// or a HOME owned by anyone else keeps the account in legacy mode.
    pub async fn is_one_home(&self, pool: &SqlitePool, user: &AuthUser) -> AppResult<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let uid: Option<i64> =
            sqlx::query_scalar("SELECT uid FROM linux_identities WHERE user_id = ?")
                .bind(user.id)
                .fetch_optional(pool)
                .await?;
        let Some(uid) = uid else {
            return Ok(false);
        };
        let name = user.username.to_ascii_lowercase();
        if validate_component(&name).is_err() {
            return Ok(false);
        }
        Ok(
            match tokio::fs::symlink_metadata(self.homes_root.join(name)).await {
                Ok(meta) => meta.is_dir() && i64::from(meta.uid()) == uid,
                Err(_) => false,
            },
        )
    }

    /// The agent of `user_id`: started (or found) through the helper.
    async fn start(&self, user_id: i64) -> AppResult<(PathBuf, String)> {
        let reply = self
            .native
            .call_json("files_agent_start", json!({ "userId": user_id }))
            .await
            .map_err(|error| {
                AppError::unavailable(format!("your files are unavailable: {error}"))
            })?;
        let field = |name: &str| reply.get(name).and_then(Value::as_str).map(str::to_string);
        match (field("socket"), field("token")) {
            (Some(socket), Some(token)) if token.len() >= 32 => Ok((PathBuf::from(socket), token)),
            _ => Err(AppError::unavailable(
                "your files are unavailable: unexpected helper reply",
            )),
        }
    }

    async fn connect(&self, user_id: i64) -> AppResult<(tokio::net::UnixStream, String)> {
        let mut agents = self.agents.lock().await;
        if let Some((socket, token)) = agents.get(&user_id) {
            if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
                let token = token.clone();
                self.ensure_watch(user_id, socket.clone(), token.clone());
                return Ok((stream, token));
            }
        }
        agents.remove(&user_id);
        let (socket, token) = self.start(user_id).await?;
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|error| {
                AppError::unavailable(format!("cannot reach your file agent: {error}"))
            })?;
        agents.insert(user_id, (socket.clone(), token.clone()));
        self.ensure_watch(user_id, socket, token.clone());
        Ok((stream, token))
    }

    /// Use an agent that is already listening (tests).
    pub async fn set_agent(&self, user_id: i64, socket: PathBuf, token: String) {
        self.agents.lock().await.insert(user_id, (socket, token));
    }

    /// Forget a user's agent (after `files_agent_stop`).
    pub async fn forget(&self, user_id: i64) {
        self.agents.lock().await.remove(&user_id);
    }

    /// Send one request to the user's agent and stream its response back.
    pub async fn send(
        &self,
        user_id: i64,
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> AppResult<Response<Body>> {
        let (stream, token) = self.connect(user_id).await?;
        client::send(AGENT, stream, &token, method, path_and_query, headers, body).await
    }

    /// A JSON request whose JSON reply is read completely. `headers` are
    /// sent as well (the content type is set here).
    pub async fn call(
        &self,
        user_id: i64,
        path: &str,
        body: &Value,
        mut headers: HeaderMap,
    ) -> AppResult<Value> {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = self
            .send(
                user_id,
                Method::POST,
                path,
                &headers,
                Body::from(body.to_string()),
            )
            .await?;
        let status = response.status();
        let value = client::read_json(AGENT, response).await?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(client::error_from(AGENT, status, &value))
        }
    }

    // -----------------------------------------------------------------------
    // Realtime
    // -----------------------------------------------------------------------

    /// Follow the agent's watch stream, once per user.
    fn ensure_watch(&self, user_id: i64, socket: PathBuf, token: String) {
        let Ok(mut watching) = self.watching.lock() else {
            return;
        };
        if !watching.insert(user_id) {
            return;
        }
        let watching = self.watching.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(error) = follow_watch(user_id, &socket, &token, &events).await {
                tracing::debug!(user_id, error = %error, "file agent watch stream ended");
            }
            if let Ok(mut watching) = watching.lock() {
                watching.remove(&user_id);
            }
        });
    }

    /// Whether a watch stream of `user_id` is being followed (tests).
    pub fn is_watching(&self, user_id: i64) -> bool {
        self.watching
            .lock()
            .is_ok_and(|watching| watching.contains(&user_id))
    }

    // -----------------------------------------------------------------------
    // Quota reservations of chunked uploads
    // -----------------------------------------------------------------------

    pub fn reserve(&self, upload_id: &str, user_id: i64, bytes: i64) {
        if let Ok(mut map) = self.reservations.lock() {
            map.insert(
                upload_id.to_string(),
                Reservation {
                    user_id,
                    bytes,
                    expires: Instant::now() + UPLOAD_TTL + Duration::from_secs(60),
                },
            );
        }
    }

    /// Take the reservation of an upload of `user_id` (another user's id
    /// finds nothing).
    pub fn take(&self, upload_id: &str, user_id: i64) -> Option<Reservation> {
        let mut map = self.reservations.lock().ok()?;
        if map.get(upload_id)?.user_id != user_id {
            return None;
        }
        map.remove(upload_id)
    }

    /// Remove and return the reservations whose uploads have expired.
    pub fn expired(&self) -> Vec<Reservation> {
        let Ok(mut map) = self.reservations.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, reservation)| reservation.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.iter().filter_map(|id| map.remove(id)).collect()
    }
}

/// Read one agent's watch stream and publish what it reports to that user.
async fn follow_watch(
    user_id: i64,
    socket: &std::path::Path,
    token: &str,
    events: &EventBus,
) -> AppResult<()> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let response = client::send(
        AGENT,
        stream,
        token,
        Method::GET,
        WATCH_ROUTE,
        &HeaderMap::new(),
        Body::empty(),
    )
    .await?;
    if !response.status().is_success() {
        return Err(AppError::unavailable(format!(
            "the {AGENT} refused the watch stream ({})",
            response.status()
        )));
    }
    let mut body = response.into_body().into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| AppError::unavailable(format!("{AGENT}: {error}")))?;
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            publish_watch_line(events, user_id, &line);
        }
        if buffer.len() > MAX_WATCH_LINE {
            return Err(AppError::unavailable(format!(
                "the {AGENT} sent an oversized watch line"
            )));
        }
    }
    Ok(())
}

/// Publish one watch line (untrusted input: only valid logical paths in
/// HOME are passed on, to the user alone).
pub fn publish_watch_line(events: &EventBus, user_id: i64, line: &[u8]) {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if value.get("resync") == Some(&Value::Bool(true)) {
        events.publish_user(
            realtime::FILES_RESYNC,
            user_id,
            json!({ "reason": "overflow" }),
        );
        return;
    }
    let Some(paths) = value.get("paths").and_then(Value::as_array) else {
        return;
    };
    let paths: Vec<String> = paths
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|path| LogicalPath::parse(path).ok())
        .filter(|logical| {
            !logical.is_root()
                && !matches!(
                    logical.segments().first().map(String::as_str),
                    Some("shared") | Some(crate::storage::safe_path::RESERVED_NAME)
                )
        })
        .map(|logical| logical.as_string())
        .take(MAX_PATHS_PER_LINE)
        .collect();
    if !paths.is_empty() {
        realtime::publish_file_changed(
            events,
            &Audience::User(user_id),
            None,
            "changed",
            &paths,
            None,
            "external",
        );
    }
}
