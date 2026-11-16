//! The server side of the per-user file agents: which accounts are in
//! one-HOME mode, starting their agents through the helper, sending them
//! requests, the quota reservations of uploads they are receiving, and the
//! realtime stream of each agent.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, HttpBody};
use axum::http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode};
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
use crate::storage::chunked::{MAX_UPLOADS_GLOBAL, MAX_UPLOADS_PER_USER};
use crate::storage::safe_path::validate_component;
use crate::storage::LogicalPath;

use super::watch::MAX_PATHS_PER_LINE;

/// How the agent is named in error messages.
pub const AGENT: &str = "file agent";
/// Longest line accepted from a watch stream.
const MAX_WATCH_LINE: usize = 256 * 1024;

/// Bytes reserved in `storage_usage` for a chunked upload an agent is
/// receiving (0 for a shared folder); committed or released when it
/// completes, is cancelled or expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub user_id: i64,
    pub bytes: i64,
    pub expires: Instant,
}

/// The chunked uploads the agents are receiving, and those being started.
#[derive(Debug, Default)]
struct Uploads {
    reservations: HashMap<String, Reservation>,
    /// Uploads being started, per account, until they are registered.
    starting: HashMap<i64, usize>,
}

/// A chunked upload being started (see [`HomeFs::begin_upload`]). It counts
/// against the limits until it is registered or dropped.
pub struct UploadSlot<'a> {
    uploads: &'a std::sync::Mutex<Uploads>,
    user_id: i64,
}

impl UploadSlot<'_> {
    /// Record the upload the agent accepted, with the quota bytes reserved
    /// for it.
    pub fn register(self, upload_id: &str, bytes: i64) {
        if let Ok(mut uploads) = self.uploads.lock() {
            uploads.reservations.insert(
                upload_id.to_string(),
                Reservation {
                    user_id: self.user_id,
                    bytes,
                    expires: Instant::now() + UPLOAD_TTL + Duration::from_secs(60),
                },
            );
        }
    }
}

impl Drop for UploadSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut uploads) = self.uploads.lock() {
            if let Some(count) = uploads.starting.get_mut(&self.user_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    uploads.starting.remove(&self.user_id);
                }
            }
        }
    }
}

/// Where one account's agent listens and its token, once known. Each
/// account has its own lock, so starting one account's agent through the
/// helper never holds up the requests of another account.
type AgentSlot = Arc<Mutex<Option<(PathBuf, String)>>>;

/// Connections to the per-user file agents.
pub struct HomeFs {
    /// `native_enabled` on Linux: without the helper every account is legacy.
    enabled: bool,
    native: NativeClient,
    homes_root: PathBuf,
    events: EventBus,
    agents: std::sync::Mutex<HashMap<i64, AgentSlot>>,
    watching: Arc<std::sync::Mutex<HashSet<i64>>>,
    uploads: std::sync::Mutex<Uploads>,
}

impl HomeFs {
    pub fn new(enabled: bool, native: NativeClient, homes_root: PathBuf, events: EventBus) -> Self {
        Self {
            enabled: enabled && cfg!(target_os = "linux"),
            native,
            homes_root,
            events,
            agents: std::sync::Mutex::new(HashMap::new()),
            watching: Arc::new(std::sync::Mutex::new(HashSet::new())),
            uploads: std::sync::Mutex::new(Uploads::default()),
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

    /// The agent slot of one account.
    fn slot(&self, user_id: i64) -> AgentSlot {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(user_id)
            .or_default()
            .clone()
    }

    /// Connect to the agent of `user_id`, starting it through the helper
    /// when it is not known or not listening. `refused` is a token that the
    /// agent refused: it is not used again, the helper is asked for the
    /// current one (unless another request already did).
    async fn connect(
        &self,
        user_id: i64,
        refused: Option<&str>,
    ) -> AppResult<(tokio::net::UnixStream, String)> {
        let slot = self.slot(user_id);
        let mut agent = slot.lock().await;
        if let Some((socket, token)) = agent.as_ref() {
            if Some(token.as_str()) != refused {
                if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
                    let token = token.clone();
                    self.ensure_watch(user_id, socket.clone(), token.clone());
                    return Ok((stream, token));
                }
            }
        }
        *agent = None;
        let (socket, token) = self.start(user_id).await?;
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|error| {
                AppError::unavailable(format!("cannot reach your file agent: {error}"))
            })?;
        *agent = Some((socket.clone(), token.clone()));
        self.ensure_watch(user_id, socket, token.clone());
        Ok((stream, token))
    }

    /// Use an agent that is already listening (tests).
    pub async fn set_agent(&self, user_id: i64, socket: PathBuf, token: String) {
        *self.slot(user_id).lock().await = Some((socket, token));
    }

    /// Stop the agent of a deactivated account through the helper (ADR
    /// 0005, section 7) and forget it. The helper derives the unit from the
    /// user id and tolerates a missing one, so this is safe for accounts
    /// that never had an agent. A failure is logged: the account can no
    /// longer sign in either way.
    pub async fn stop_agent(&self, user_id: i64) {
        if !self.enabled {
            return;
        }
        if let Err(error) = self
            .native
            .call_json("files_agent_stop", json!({ "userId": user_id }))
            .await
        {
            tracing::warn!(
                user_id,
                error = %error,
                "could not stop the file agent of a deactivated account"
            );
        }
        self.forget(user_id).await;
    }

    /// Forget a user's agent (after `files_agent_stop`). It does not wait
    /// for a start that is under way.
    pub async fn forget(&self, user_id: i64) {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&user_id);
    }

    /// Send one request to the user's agent and stream its response back.
    /// A request without a body is sent again once when the agent refuses
    /// the token; one with a streamed body cannot be, and gets 503.
    pub async fn send(
        &self,
        user_id: i64,
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> AppResult<Response<Body>> {
        let replay = (body.size_hint().exact() == Some(0)).then(Bytes::new);
        self.exchange(user_id, method, path_and_query, headers, body, replay)
            .await
    }

    /// One request, and once more with `replay` as its body when the agent
    /// refused the token: its socket unit was restarted (the agent exited
    /// or was stopped), and the helper wrote a new token. Only a 403 reply
    /// is read here (it is small); others stream through.
    async fn exchange(
        &self,
        user_id: i64,
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
        body: Body,
        replay: Option<Bytes>,
    ) -> AppResult<Response<Body>> {
        let (stream, token) = self.connect(user_id, None).await?;
        let response = client::send(
            AGENT,
            stream,
            &token,
            method.clone(),
            path_and_query,
            headers,
            body,
        )
        .await?;
        if response.status() != StatusCode::FORBIDDEN {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, client::MAX_JSON_REPLY)
            .await
            .map_err(|error| AppError::unavailable(format!("{AGENT} reply: {error}")))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !client::token_refused(parts.status, &value) {
            return Ok(Response::from_parts(parts, Body::from(bytes)));
        }
        let (stream, token) = self.connect(user_id, Some(&token)).await?;
        let Some(replay) = replay else {
            drop(stream);
            return Err(AppError::unavailable(
                "your file agent was restarted; please try again",
            ));
        };
        client::send(
            AGENT,
            stream,
            &token,
            method,
            path_and_query,
            headers,
            Body::from(replay),
        )
        .await
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
        let body = Bytes::from(body.to_string());
        let response = self
            .exchange(
                user_id,
                Method::POST,
                path,
                &headers,
                Body::from(body.clone()),
                Some(body),
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
    // Chunked uploads: limits and quota reservations
    // -----------------------------------------------------------------------

    /// Start a chunked upload of `user_id` within the limits of in-process
    /// uploads (`storage::chunked`): at most [`MAX_UPLOADS_PER_USER`] per
    /// account and [`MAX_UPLOADS_GLOBAL`] in all, counting uploads into
    /// shared folders too. `429` when they are reached.
    pub fn begin_upload(&self, user_id: i64) -> AppResult<UploadSlot<'_>> {
        let mut uploads = self
            .uploads
            .lock()
            .map_err(|_| AppError::internal("upload state is unavailable"))?;
        let mine = uploads
            .reservations
            .values()
            .filter(|reservation| reservation.user_id == user_id)
            .count()
            + uploads.starting.get(&user_id).copied().unwrap_or(0);
        let all = uploads.reservations.len() + uploads.starting.values().sum::<usize>();
        if mine >= MAX_UPLOADS_PER_USER || all >= MAX_UPLOADS_GLOBAL {
            return Err(AppError::TooManyRequests {
                retry_after_secs: 60,
                message: "too many concurrent uploads, finish or cancel one first",
            });
        }
        *uploads.starting.entry(user_id).or_default() += 1;
        Ok(UploadSlot {
            uploads: &self.uploads,
            user_id,
        })
    }

    /// Take the reservation of an upload of `user_id` (another user's id
    /// finds nothing).
    pub fn take(&self, upload_id: &str, user_id: i64) -> Option<Reservation> {
        let mut uploads = self.uploads.lock().ok()?;
        let map = &mut uploads.reservations;
        if map.get(upload_id)?.user_id != user_id {
            return None;
        }
        map.remove(upload_id)
    }

    /// Remove and return the reservations whose uploads have expired.
    pub fn expired(&self) -> Vec<Reservation> {
        let Ok(mut uploads) = self.uploads.lock() else {
            return Vec::new();
        };
        let map = &mut uploads.reservations;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn homefs() -> HomeFs {
        HomeFs::new(
            true,
            NativeClient::new(PathBuf::from("/nonexistent/native.sock")),
            PathBuf::from("/nonexistent/homes"),
            EventBus::new(),
        )
    }

    /// A helper that answers `files_agent_start` for user 2 at once and
    /// never for user 1.
    fn slow_helper(dir: &std::path::Path) -> PathBuf {
        let helper = dir.join("h.sock");
        let listener = tokio::net::UnixListener::bind(&helper).unwrap();
        let agent = dir.join("u2.sock");
        // Listening is enough for `connect`; requests are not needed.
        let agent_listener = std::os::unix::net::UnixListener::bind(&agent).unwrap();
        tokio::spawn(async move {
            let _agent_listener = agent_listener;
            while let Ok((stream, _)) = listener.accept().await {
                let agent = agent.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    let mut reader = tokio::io::BufReader::new(read);
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if request["userId"] == 1 {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    }
                    let reply = json!({"id": request["id"], "ok": true, "data": {
                        "unit": "rast-files-u2", "socket": agent, "token": "k".repeat(64),
                    }});
                    let mut bytes = serde_json::to_vec(&reply).unwrap();
                    bytes.push(b'\n');
                    let _ = write.write_all(&bytes).await;
                });
            }
        });
        helper
    }

    #[tokio::test]
    async fn a_slow_agent_start_holds_up_only_its_own_account() {
        let dir = crate::test_support::socket_tempdir();
        let fs = Arc::new(HomeFs::new(
            true,
            NativeClient::new(slow_helper(dir.path())),
            dir.path().join("homes"),
            EventBus::new(),
        ));
        let slow = {
            let fs = fs.clone();
            tokio::spawn(async move { fs.connect(1, None).await.map(|_| ()) })
        };
        // Let user 1 take its lock and wait for the helper.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!slow.is_finished());
        let (_, token) = tokio::time::timeout(Duration::from_secs(5), fs.connect(2, None))
            .await
            .expect("user 2 waited for user 1's agent start")
            .unwrap();
        assert_eq!(token, "k".repeat(64));
        // Forgetting an account does not wait for its start either.
        tokio::time::timeout(Duration::from_secs(5), fs.forget(1))
            .await
            .unwrap();
        slow.abort();
    }

    /// A helper that answers every `files_agent_start` with `socket` and
    /// `token`, counting the calls.
    fn counting_helper(
        dir: &std::path::Path,
        socket: PathBuf,
        token: String,
    ) -> (PathBuf, Arc<std::sync::atomic::AtomicUsize>) {
        let helper = dir.join("h.sock");
        let listener = tokio::net::UnixListener::bind(&helper).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let mut reader = tokio::io::BufReader::new(read);
                if reader.read_line(&mut line).await.is_err() {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let reply = json!({"id": request["id"], "ok": true, "data": {
                    "unit": "rast-files-u1", "socket": socket, "token": token,
                }});
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            }
        });
        (helper, calls)
    }

    #[tokio::test]
    async fn a_refused_token_is_read_again_from_the_helper_once() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = crate::test_support::socket_tempdir();
        let home = dir.path().join("homes/alice");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        // The agent's socket unit was restarted: it has a new token.
        let socket = dir.path().join("a.sock");
        let current = "n".repeat(64);
        tokio::spawn(super::super::agent::run(super::super::agent::RunOptions {
            home: home.clone(),
            shared_root: shared,
            listen: super::super::agent::Listen::Path(socket.clone()),
            token: current.clone(),
            peer_uid: None,
            idle: Duration::from_secs(600),
            stay_while_user_runs: false,
        }));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (helper, calls) = counting_helper(dir.path(), socket.clone(), current);
        let fs = HomeFs::new(
            true,
            NativeClient::new(helper),
            dir.path().join("homes"),
            EventBus::new(),
        );
        let stale = "o".repeat(64);

        // A JSON call is sent again with the new token.
        fs.set_agent(1, socket.clone(), stale.clone()).await;
        let entry = fs
            .call(
                1,
                "/api/files/mkdir",
                &json!({"path": "/docs"}),
                HeaderMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(entry["path"], "/docs");
        assert!(home.join("docs").is_dir());
        assert_eq!(calls.load(SeqCst), 1);

        // So is a request without a body.
        fs.set_agent(1, socket.clone(), stale.clone()).await;
        let response = fs
            .send(
                1,
                Method::GET,
                "/api/files?path=/",
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(calls.load(SeqCst), 2);

        // A streamed body cannot be sent again: 503, and the next request
        // has the new token without asking the helper.
        fs.set_agent(1, socket.clone(), stale.clone()).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(3));
        let body = Body::from_stream(futures::stream::iter([Ok::<_, std::io::Error>(
            Bytes::from_static(b"abc"),
        )]));
        let error = fs
            .send(
                1,
                Method::POST,
                "/api/files/upload?dir=/&name=a.txt",
                &headers,
                body,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Unavailable(_)), "{error:?}");
        assert!(!home.join("a.txt").exists());
        assert_eq!(calls.load(SeqCst), 3);
        let response = fs
            .send(
                1,
                Method::GET,
                "/api/files?path=/",
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(calls.load(SeqCst), 3);

        // Other refusals pass through untouched, without asking the helper.
        std::os::unix::fs::symlink("/", home.join("up")).unwrap();
        let response = fs
            .send(
                1,
                Method::GET,
                "/api/files?path=/up",
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let value = client::read_json(AGENT, response).await.unwrap();
        assert_eq!(
            value["error"]["message"],
            "symbolic links are not supported"
        );
        // A token the helper still hands out but the agent refuses is
        // tried once, not in a loop.
        let (lying, lies) = counting_helper(&dir.path().join("homes"), socket, stale);
        let liar = HomeFs::new(
            true,
            NativeClient::new(lying),
            dir.path().join("homes"),
            EventBus::new(),
        );
        let error = liar
            .call(
                1,
                "/api/files/mkdir",
                &json!({"path": "/x"}),
                HeaderMap::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Forbidden(_)), "{error:?}");
        assert_eq!(lies.load(SeqCst), 2);
        assert!(!home.join("x").exists());
    }

    #[test]
    fn chunked_uploads_have_the_in_process_limits() {
        let fs = homefs();
        // Per account: slots being started count as well as registered ones.
        let starting = fs.begin_upload(1).unwrap();
        for index in 1..MAX_UPLOADS_PER_USER {
            fs.begin_upload(1)
                .unwrap()
                .register(&format!("usr-a{index}"), 0);
        }
        let error = fs.begin_upload(1).err().unwrap();
        assert!(
            matches!(error, AppError::TooManyRequests { .. }),
            "{error:?}"
        );
        // A slot dropped without registering frees its place.
        drop(starting);
        fs.begin_upload(1).unwrap().register("usr-a0", 0);
        assert!(fs.begin_upload(1).is_err());
        // Taking an upload (complete or cancel) frees its place.
        assert!(fs.take("usr-a0", 1).is_some());
        assert!(fs.begin_upload(1).is_ok());

        // In all: the other accounts fill the rest of the global limit.
        let mut user = 2;
        let mut registered = MAX_UPLOADS_PER_USER - 1;
        while registered < MAX_UPLOADS_GLOBAL {
            for _ in 0..MAX_UPLOADS_PER_USER.min(MAX_UPLOADS_GLOBAL - registered) {
                fs.begin_upload(user)
                    .unwrap()
                    .register(&format!("usr-{user}-{registered}"), 0);
                registered += 1;
            }
            user += 1;
        }
        let error = fs.begin_upload(user + 1).err().unwrap();
        assert!(
            matches!(error, AppError::TooManyRequests { .. }),
            "{error:?}"
        );
    }
}
