//! Server-side lifecycle of native terminal sessions.
//!
//! One [`Session`] per tab: the manager starts the privileged helper (which
//! provisions the per-user identity and a transient systemd unit running the
//! terminal agent), connects to the agent socket and relays frames. Sessions
//! are in memory: they survive browser disconnects for the configured grace
//! period (re-attach by id) and die with the server process (systemd stops
//! the transient units on shutdown).
//!
//! Security boundaries live here and in the helper: the user id is always
//! derived from the authenticated session, ownership is checked for every
//! attach/close, and the helper re-validates every field it receives.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, Mutex},
};

use crate::{
    auth::AuthUser,
    config::Config,
    db,
    error::{AppError, AppResult},
    native::client::NativeClient,
    terminal::{
        env::{self, TerminalEnvironment},
        protocol::{write_frame, AgentFrame, Handshake, HandshakeReply, ServerFrame, MAX_FRAME},
    },
};

/// How much recent output is kept so a reconnecting browser can be caught up.
const REPLAY_LIMIT: usize = 256 * 1024;
/// Bounded viewer queue (backpressure: the agent is not read while this is full).
const VIEWER_QUEUE: usize = 64;
/// Bounded command queue towards the agent.
const AGENT_QUEUE: usize = 64;
/// How long to wait for the agent socket after the unit started.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Sweeper interval.
const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Everything needed to open one terminal tab.
#[derive(Debug, Clone)]
pub struct TerminalSpec {
    /// Absolute directory resolved by the API from a RAST folder reference.
    pub cwd: Option<PathBuf>,
    /// Whether the RAST folder permission allows writing.
    pub write_access: bool,
    /// Per-terminal runtime version overrides (validated against installed
    /// versions by the environment builder).
    pub overrides: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

/// Events the WebSocket handler forwards to the browser.
#[derive(Debug)]
pub enum ViewerEvent {
    Output(Vec<u8>),
    Exit(Option<i32>),
    Error(String),
    Closed,
}

/// A viewer handle returned by start/attach.
pub struct TerminalViewer {
    pub session_id: String,
    pub shell: String,
    pub cwd: String,
    pub runtimes: BTreeMap<String, String>,
    /// Recent output to replay before live frames.
    pub replay: Vec<u8>,
    pub events: mpsc::Receiver<ViewerEvent>,
}

/// Commands sent to the agent actor task.
enum AgentCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

struct SessionInner {
    viewer: Option<mpsc::Sender<ViewerEvent>>,
    ring: VecDeque<u8>,
    detach_deadline: Option<Instant>,
    last_activity: Instant,
    closed: bool,
}

pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub shell: String,
    pub cwd: String,
    pub started_at: i64,
    /// Runtime versions this shell was started with (shown on reconnect).
    pub runtimes: BTreeMap<String, String>,
    inner: Mutex<SessionInner>,
    commands: mpsc::Sender<AgentCommand>,
}

impl Session {
    fn new(
        id: String,
        user_id: i64,
        shell: String,
        cwd: String,
        runtimes: BTreeMap<String, String>,
        commands: mpsc::Sender<AgentCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            user_id,
            shell,
            cwd,
            started_at: db::now_ms(),
            runtimes,
            inner: Mutex::new(SessionInner {
                viewer: None,
                ring: VecDeque::with_capacity(REPLAY_LIMIT.min(64 * 1024)),
                detach_deadline: None,
                last_activity: Instant::now(),
                closed: false,
            }),
            commands,
        })
    }

    async fn push_output(&self, bytes: &[u8]) {
        let mut inner = self.inner.lock().await;
        let capacity = REPLAY_LIMIT.max(bytes.len());
        for byte in bytes {
            if inner.ring.len() >= capacity {
                inner.ring.pop_front();
            }
            inner.ring.push_back(*byte);
        }
        inner.last_activity = Instant::now();
        if let Some(viewer) = inner.viewer.clone() {
            if viewer
                .send(ViewerEvent::Output(bytes.to_vec()))
                .await
                .is_err()
            {
                inner.viewer = None;
            }
        }
    }

    async fn attach(&self) -> Option<TerminalViewer> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return None;
        }
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE);
        inner.viewer = Some(tx);
        inner.detach_deadline = None;
        inner.last_activity = Instant::now();
        let replay: Vec<u8> = inner.ring.iter().copied().collect();
        Some(TerminalViewer {
            session_id: self.id.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            runtimes: self.runtimes.clone(),
            replay,
            events: rx,
        })
    }

    async fn detach(&self) {
        let mut inner = self.inner.lock().await;
        inner.viewer = None;
        inner.detach_deadline = Some(Instant::now());
    }

    async fn mark_closed(&self) {
        let mut inner = self.inner.lock().await;
        inner.closed = true;
        if let Some(viewer) = inner.viewer.take() {
            let _ = viewer.send(ViewerEvent::Closed).await;
        }
    }
}

/// Terminal manager held in [`crate::state::AppState`].
pub struct TerminalManager {
    config: Arc<Config>,
    db: sqlx::SqlitePool,
    helper: NativeClient,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    system_cache: Mutex<
        Option<(
            Instant,
            HashMap<String, crate::packages::system::SystemInstall>,
        )>,
    >,
}

impl TerminalManager {
    pub fn new(config: Arc<Config>, db: sqlx::SqlitePool, helper: NativeClient) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            helper,
            sessions: Mutex::new(HashMap::new()),
            system_cache: Mutex::new(None),
        })
    }

    /// Background sweeper: reaps detached sessions after the grace period and
    /// enforces the idle timeout.
    pub fn spawn_sweeper(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                ticker.tick().await;
                manager.sweep().await;
            }
        });
    }

    pub fn enabled(&self) -> bool {
        self.config.terminal.enabled
    }

    async fn sweep(&self) {
        let now = Instant::now();
        let grace = Duration::from_secs(self.config.terminal.grace_seconds as u64);
        let idle = if self.config.terminal.idle_minutes == 0 {
            None
        } else {
            Some(Duration::from_secs(
                self.config.terminal.idle_minutes as u64 * 60,
            ))
        };
        let sessions: Vec<Arc<Session>> = {
            let map = self.sessions.lock().await;
            map.values().cloned().collect()
        };
        for session in sessions {
            let close = {
                let inner = session.inner.lock().await;
                if inner.closed {
                    true
                } else if inner.viewer.is_none() {
                    match inner.detach_deadline {
                        Some(deadline) => now.duration_since(deadline) >= grace,
                        None => false,
                    }
                } else {
                    idle.is_some_and(|limit| now.duration_since(inner.last_activity) >= limit)
                }
            };
            if close {
                self.close_session(&session).await;
            }
        }
    }

    async fn close_session(&self, session: &Arc<Session>) {
        let _ = session.commands.send(AgentCommand::Close).await;
        session.mark_closed().await;
        self.sessions.lock().await.remove(&session.id);
        let session_id = session.id.clone();
        let user_id = session.user_id;
        let helper = self.helper.clone();
        tokio::spawn(async move {
            let _ = helper
                .call_json(
                    "terminal_stop",
                    json!({"userId": user_id, "sessionId": session_id}),
                )
                .await;
        });
    }

    async fn system_probe(&self) -> HashMap<String, crate::packages::system::SystemInstall> {
        {
            let cache = self.system_cache.lock().await;
            if let Some((at, map)) = cache.as_ref() {
                if at.elapsed() < Duration::from_secs(300) {
                    return map.clone();
                }
            }
        }
        let cfg = self.config.clone();
        let probe = tokio::task::spawn_blocking(move || {
            let scratch = cfg.data_dir.join("tmp").join("terminal-probe");
            let _ = std::fs::create_dir_all(&scratch);
            let found = crate::packages::system::detect_all(&cfg.packages_dir(), &scratch);
            let _ = std::fs::remove_dir_all(&scratch);
            found
        })
        .await
        .unwrap_or_default();
        *self.system_cache.lock().await = Some((Instant::now(), probe.clone()));
        probe
    }

    async fn environment(&self, user_id: i64) -> AppResult<TerminalEnvironment> {
        let probe = self.system_probe().await;
        env::environment(&self.config, &self.db, user_id, &probe).await
    }

    /// Runtime panel data for the Terminal app.
    pub async fn env_dto(&self, user: &AuthUser) -> AppResult<Value> {
        let environment = self.environment(user.id).await?;
        Ok(environment.to_json(&self.config.terminal, self.enabled()))
    }

    /// Persist per-user default runtime versions.
    pub async fn set_defaults(
        &self,
        user: &AuthUser,
        defaults: &BTreeMap<String, String>,
    ) -> AppResult<Value> {
        env::set_defaults(&self.db, user.id, defaults).await?;
        self.env_dto(user).await
    }

    /// Sessions of one user (metadata only).
    pub async fn list(&self, user: &AuthUser) -> Value {
        let map = self.sessions.lock().await;
        let mut list = Vec::new();
        for session in map.values().filter(|session| session.user_id == user.id) {
            let inner = session.inner.lock().await;
            if inner.closed {
                continue;
            }
            list.push(json!({
                "id": session.id,
                "shell": session.shell,
                "cwd": session.cwd,
                "state": "running",
                "startedAt": session.started_at,
                "viewers": if inner.viewer.is_some() { 1 } else { 0 },
            }));
        }
        list.sort_by(|a, b| a["startedAt"].as_i64().cmp(&b["startedAt"].as_i64()));
        json!({"sessions": list, "maxSessions": self.config.terminal.max_sessions})
    }

    async fn session_count(&self, user_id: i64) -> usize {
        let map = self.sessions.lock().await;
        map.values()
            .filter(|session| session.user_id == user_id)
            .count()
    }

    /// Start a new terminal and return a viewer attached to it.
    pub async fn start(
        self: &Arc<Self>,
        user: &AuthUser,
        spec: TerminalSpec,
    ) -> AppResult<TerminalViewer> {
        if !self.enabled() {
            return Err(AppError::unavailable(
                "The terminal is disabled on this server (terminal_enabled = false).",
            ));
        }
        if self.session_count(user.id).await >= self.config.terminal.max_sessions as usize {
            return Err(AppError::conflict(format!(
                "You already have {} terminal sessions open. Close one to open another.",
                self.config.terminal.max_sessions
            )));
        }
        let shell = if Path::new("/bin/bash").is_file() {
            "/bin/bash".to_string()
        } else {
            "/bin/sh".to_string()
        };
        let environment = self.environment(user.id).await?;
        let selected = environment.selected_overrides(&spec.overrides);
        let env_map = environment.shell_env(&selected);
        let session_id = random_hex(16);
        let cwd = spec
            .cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let params = json!({
            "userId": user.id,
            "sessionId": session_id,
            "shell": shell,
            "cwd": cwd,
            "writeAccess": spec.write_access,
            "env": env_map,
            "memoryMaxMb": self.config.terminal.memory_mb,
            "cpuQuotaPercent": self.config.terminal.cpus.saturating_mul(100),
            "tasksMax": self.config.terminal.tasks,
        });
        let started = self
            .helper
            .call_json("terminal_start", params)
            .await
            .map_err(|error| AppError::unavailable(error.message))?;
        let socket = started
            .get("socket")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::internal("terminal helper returned no socket"))?
            .to_string();
        let token = started
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::internal("terminal helper returned no token"))?
            .to_string();
        let final_cwd = started
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or(&cwd)
            .to_string();

        let (cols, rows) = crate::terminal::protocol::clamp_size(Some(spec.cols), Some(spec.rows));
        let stream = match self.connect_agent(&socket).await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = self
                    .helper
                    .call_json(
                        "terminal_stop",
                        json!({"userId": user.id, "sessionId": session_id}),
                    )
                    .await;
                return Err(error);
            }
        };
        let (stream, cols, rows) = match handshake(stream, &token, cols, rows).await {
            Ok(parts) => parts,
            Err(message) => {
                let _ = self
                    .helper
                    .call_json(
                        "terminal_stop",
                        json!({"userId": user.id, "sessionId": session_id}),
                    )
                    .await;
                return Err(AppError::unavailable(message));
            }
        };

        let (commands_tx, commands_rx) = mpsc::channel(AGENT_QUEUE);
        let session = Session::new(
            session_id.clone(),
            user.id,
            shell.clone(),
            final_cwd.clone(),
            selected.clone(),
            commands_tx,
        );
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session.clone());
        let manager = self.clone();
        let actor_session = session.clone();
        tokio::spawn(async move {
            manager
                .run_actor(actor_session, stream, cols, rows, commands_rx)
                .await;
        });

        let viewer = session
            .attach()
            .await
            .ok_or_else(|| AppError::internal("the terminal closed before it could be attached"))?;
        Ok(viewer)
    }

    /// Re-attach an existing session after a temporary network interruption.
    pub async fn attach(
        self: &Arc<Self>,
        user: &AuthUser,
        session_id: &str,
    ) -> AppResult<TerminalViewer> {
        let session = {
            let map = self.sessions.lock().await;
            map.get(session_id).cloned()
        };
        let Some(session) = session else {
            return Err(AppError::NotFound("that terminal is no longer running"));
        };
        if session.user_id != user.id {
            return Err(AppError::Forbidden(
                "this terminal belongs to another account",
            ));
        }
        session
            .attach()
            .await
            .ok_or(AppError::NotFound("that terminal has ended"))
    }

    /// Mark a viewer detached; the sweeper reaps the session after the grace.
    pub async fn detach(&self, session_id: &str) {
        let session = {
            let map = self.sessions.lock().await;
            map.get(session_id).cloned()
        };
        if let Some(session) = session {
            session.detach().await;
        }
    }

    /// Close one terminal (explicit close / tab close).
    pub async fn close(&self, user: &AuthUser, session_id: &str) -> AppResult<()> {
        let session = {
            let map = self.sessions.lock().await;
            map.get(session_id).cloned()
        };
        let Some(session) = session else {
            return Ok(());
        };
        if session.user_id != user.id {
            return Err(AppError::Forbidden(
                "this terminal belongs to another account",
            ));
        }
        self.close_session(&session).await;
        Ok(())
    }

    /// Close every session of one account (account disabled, app permission
    /// revoked, sign-out policy). Backend-initiated, no user context.
    pub async fn close_user(&self, user_id: i64) {
        let sessions: Vec<Arc<Session>> = {
            let map = self.sessions.lock().await;
            map.values()
                .filter(|session| session.user_id == user_id)
                .cloned()
                .collect()
        };
        for session in sessions {
            self.close_session(&session).await;
        }
    }

    /// Forward keyboard bytes to the session's PTY (owner-checked).
    pub async fn input(&self, user: &AuthUser, session_id: &str, bytes: Vec<u8>) -> bool {
        let Some(session) = self.owned_session(user, session_id).await else {
            return false;
        };
        session
            .commands
            .send(AgentCommand::Input(bytes))
            .await
            .is_ok()
    }

    /// Resize a session's PTY (owner-checked).
    pub async fn resize(&self, user: &AuthUser, session_id: &str, cols: u16, rows: u16) {
        if let Some(session) = self.owned_session(user, session_id).await {
            let _ = session
                .commands
                .send(AgentCommand::Resize { cols, rows })
                .await;
        }
    }

    async fn owned_session(&self, user: &AuthUser, session_id: &str) -> Option<Arc<Session>> {
        let session = {
            let map = self.sessions.lock().await;
            map.get(session_id).cloned()
        };
        match session {
            Some(session) if session.user_id == user.id => Some(session),
            _ => None,
        }
    }

    async fn connect_agent(&self, socket: &str) -> AppResult<UnixStream> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => return Ok(stream),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(AppError::unavailable(format!(
                        "The terminal agent did not accept a connection: {error}"
                    )))
                }
            }
        }
    }

    /// The per-session actor: owns the agent connection, streams output to the
    /// current viewer (bounded queue = backpressure), records replay history
    /// and cleans up on exit.
    async fn run_actor(
        self: Arc<Self>,
        session: Arc<Session>,
        stream: UnixStream,
        cols: u16,
        rows: u16,
        mut commands: mpsc::Receiver<AgentCommand>,
    ) {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        // Apply the handshake size to the PTY right away.
        let initial = ServerFrame::Resize { cols, rows };
        let _ = self.write_agent(&mut write_half, &initial).await;
        let mut buffer = vec![0u8; MAX_FRAME];
        #[allow(unused_assignments)]
        let mut ended: Option<(Option<i32>, Option<String>)> = None;
        loop {
            tokio::select! {
                read = read_agent_line(&mut reader, &mut buffer) => {
                    match read {
                        Ok(Some(AgentFrame::Output(bytes))) => session.push_output(&bytes).await,
                        Ok(Some(AgentFrame::Exit { code })) => {
                            ended = Some((code, None));
                            break;
                        }
                        Ok(Some(AgentFrame::Error(message))) => {
                            ended = Some((None, Some(message)));
                            break;
                        }
                        Ok(None) | Err(_) => {
                            ended = Some((None, None));
                            break;
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        ended = Some((None, None));
                        break;
                    };
                    let frame = match command {
                        AgentCommand::Input(bytes) => ServerFrame::Input(bytes),
                        AgentCommand::Resize { cols, rows } => ServerFrame::Resize { cols, rows },
                        AgentCommand::Close => {
                            let _ = self.write_agent(&mut write_half, &ServerFrame::Close).await;
                            ended = Some((None, None));
                            break;
                        }
                    };
                    if self.write_agent(&mut write_half, &frame).await.is_err() {
                        ended = Some((None, None));
                        break;
                    }
                }
            }
        }
        // Report the final event before closing the viewer: the browser must
        // see "exit" (code) or "error" (message), not just "closed".
        match ended {
            Some((code, error)) => {
                let mut inner = session.inner.lock().await;
                inner.closed = true;
                if let Some(viewer) = inner.viewer.take() {
                    let event = match error {
                        Some(message) => ViewerEvent::Error(message),
                        None => ViewerEvent::Exit(code),
                    };
                    let _ = viewer.send(event).await;
                }
            }
            None => session.mark_closed().await,
        }
        self.sessions.lock().await.remove(&session.id);
        let user_id = session.user_id;
        let session_id = session.id.clone();
        let helper = self.helper.clone();
        tokio::spawn(async move {
            let _ = helper
                .call_json(
                    "terminal_stop",
                    json!({"userId": user_id, "sessionId": session_id}),
                )
                .await;
        });
    }

    async fn write_agent(
        &self,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        frame: &ServerFrame,
    ) -> std::io::Result<()> {
        let encoded = frame
            .encode()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        write_frame(writer, &encoded).await
    }
}

/// Read one agent frame without allocating a fresh buffer every time.
async fn read_agent_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    _buffer: &mut [u8],
) -> std::io::Result<Option<AgentFrame>> {
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal frame length out of range",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    AgentFrame::decode(header[0], payload).map(Some)
}

async fn handshake(
    stream: UnixStream,
    token: &str,
    cols: u16,
    rows: u16,
) -> Result<(UnixStream, u16, u16), String> {
    let (read_half, mut write_half) = stream.into_split();
    let handshake = Handshake {
        token: token.to_string(),
        cols,
        rows,
    };
    let mut line = serde_json::to_vec(&handshake).map_err(|error| error.to_string())?;
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .map_err(|error| format!("cannot send the terminal handshake: {error}"))?;
    write_half
        .flush()
        .await
        .map_err(|error| format!("cannot flush the terminal handshake: {error}"))?;
    let mut reader = BufReader::new(read_half);
    let mut reply = String::new();
    let read = tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut reply)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {}
        _ => return Err("The terminal agent did not answer the handshake.".into()),
    }
    let reply: HandshakeReply = serde_json::from_str(reply.trim())
        .map_err(|_| "The terminal handshake reply was malformed.")?;
    if !reply.ok {
        return Err(reply
            .error
            .unwrap_or_else(|| "The terminal agent rejected the connection.".into()));
    }
    let stream = reader.into_inner().reunite(write_half).map_err(|_| {
        "The terminal connection could not be recombined after the handshake.".to_string()
    })?;
    Ok((stream, cols, rows))
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    use rand::RngCore;
    rand::rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

/// Platform guard used by the API layer: terminals are Linux-only (PTY,
/// systemd and the identity helper).
pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_and_tokens_are_hex() {
        let id = random_hex(16);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn supported_on_linux() {
        assert!(supported());
    }

    #[test]
    fn replay_ring_is_bounded() {
        // Pure logic mirrored from Session::push_output.
        let mut ring: VecDeque<u8> = VecDeque::new();
        let bytes = vec![b'a'; REPLAY_LIMIT + 100];
        for byte in &bytes {
            if ring.len() >= REPLAY_LIMIT {
                ring.pop_front();
            }
            ring.push_back(*byte);
        }
        assert_eq!(ring.len(), REPLAY_LIMIT);
    }
}
