//! The terminal helper: `rast terminal helper`, run as root by
//! `rast-terminal.service`. See [`crate::terminal`] for the design.
//!
//! Each shell is a login shell of the account the helper runs as (root on a
//! server) on its own pseudo terminal, owned by a task that keeps its recent
//! output and forwards it to at most one viewer at a time.

use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use rand::RngCore;
use serde_json::{json, Value};
use tokio::{
    io::{unix::AsyncFd, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::Instant,
};

use super::{
    socket_path, valid_session_id, Request, SessionInfo, DEFAULT_PEER_USER, DETACHED_IDLE,
    MAX_LINE, MAX_SESSIONS, SCROLLBACK,
};
use crate::{
    docker::exec::{clamp_size, read_frame, read_line_capped, write_frame, Frame},
    pty::{open_pty, read_fd, set_nonblocking, set_window_size, spawn_on_pty, write_fd},
};

/// Bytes read from a terminal at a time.
const READ_CHUNK: usize = 16 * 1024;
/// Keyboard bytes waiting for the shell; more is dropped (a huge paste).
const MAX_PENDING_INPUT: usize = 1024 * 1024;
/// After the shell exits, its last output is forwarded for at most this long.
const EXIT_DRAIN: Duration = Duration::from_millis(500);
/// After a hang-up, the shell gets this long to end before it is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(3);
/// A viewer that does not take output for this long is detached.
const VIEWER_STALL: Duration = Duration::from_secs(10);
/// The first request line must arrive within this time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin";

/// How shells are started: the helper's own account's login shell.
#[derive(Debug, Clone)]
pub struct ShellSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub user: String,
    pub home: PathBuf,
    pub path: String,
    pub lang: String,
}

impl ShellSpec {
    /// A login shell of the account this process runs as.
    pub fn for_current_user() -> Result<Self, String> {
        // SAFETY: geteuid never fails.
        let uid = unsafe { libc::geteuid() };
        let account = passwd_by_uid(uid).ok_or_else(|| format!("no passwd entry for uid {uid}"))?;
        let usable = |shell: &Path| {
            shell.is_absolute()
                && shell.exists()
                && !matches!(
                    shell.file_name().and_then(|n| n.to_str()),
                    Some("nologin" | "false")
                )
        };
        let program = [account.shell.clone(), "/bin/bash".into(), "/bin/sh".into()]
            .into_iter()
            .find(|shell| usable(shell))
            .ok_or("no usable shell (bash or sh) on this system")?;
        let lang = std::env::var("LANG")
            .ok()
            .filter(|lang| !lang.is_empty() && lang.chars().all(|c| c.is_ascii_graphic()))
            .unwrap_or_else(|| "C.UTF-8".into());
        Ok(Self {
            program,
            args: vec!["-l".into()],
            user: account.name,
            home: account.home,
            path: DEFAULT_PATH.into(),
            lang,
        })
    }

    fn command(&self, rast_user: &str) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .env("HOME", &self.home)
            .env("USER", &self.user)
            .env("LOGNAME", &self.user)
            .env("SHELL", &self.program)
            .env("PATH", &self.path)
            .env("LANG", &self.lang)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            // Who opened it, for prompts and scripts (audit is the server's).
            .env("RAST_USER", rast_user)
            .current_dir(if self.home.is_dir() {
                self.home.as_path()
            } else {
                Path::new("/")
            });
        command
    }
}

// ---------------------------------------------------------------------------
// Sessions

enum Cmd {
    Attach {
        viewer: Viewer,
        cols: u16,
        rows: u16,
        reply: oneshot::Sender<Vec<u8>>,
    },
    Input {
        viewer: u64,
        bytes: Vec<u8>,
    },
    Resize {
        viewer: u64,
        cols: u16,
        rows: u16,
    },
    Detach {
        viewer: u64,
    },
    Kill,
}

enum Out {
    Data(Vec<u8>),
    Exit(i32),
    Detached,
}

struct Viewer {
    id: u64,
    tx: mpsc::Sender<Out>,
}

struct SessionHandle {
    id: String,
    owner: i64,
    number: u32,
    created_at: i64,
    pid: u32,
    attached: AtomicBool,
    cmd: mpsc::Sender<Cmd>,
}

impl SessionHandle {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            number: self.number,
            created_at: self.created_at,
            attached: self.attached.load(Ordering::Relaxed),
            program: foreground_program(self.pid).unwrap_or_default(),
        }
    }
}

/// Recent output, trimmed from the front.
struct Scrollback {
    bytes: VecDeque<u8>,
    limit: usize,
    trimmed: bool,
}

impl Scrollback {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            trimmed: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        if self.bytes.len() > self.limit {
            let excess = self.bytes.len() - self.limit;
            self.bytes.drain(..excess);
            self.trimmed = true;
        }
    }

    /// What a new viewer is shown: once trimmed, from the first full line
    /// (never from the middle of a character or an escape sequence).
    fn snapshot(&mut self) -> Vec<u8> {
        let bytes = self.bytes.make_contiguous();
        let start = if self.trimmed {
            bytes.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1)
        } else {
            0
        };
        bytes[start..].to_vec()
    }
}

/// The shells of this helper.
pub struct Helper {
    shell: ShellSpec,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    viewers: AtomicU64,
}

#[derive(Debug)]
struct Refusal {
    code: &'static str,
    message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Helper {
    pub fn new(shell: ShellSpec) -> Arc<Self> {
        Arc::new(Self {
            shell,
            sessions: Mutex::new(HashMap::new()),
            viewers: AtomicU64::new(1),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionHandle>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn list(&self, owner: i64) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self
            .lock()
            .values()
            .filter(|s| s.owner == owner)
            .map(|s| s.info())
            .collect();
        sessions.sort_by_key(|s| (s.number, s.created_at));
        sessions
    }

    fn find(&self, owner: i64, id: &str) -> Option<Arc<SessionHandle>> {
        self.lock().get(id).filter(|s| s.owner == owner).cloned()
    }

    async fn kill(&self, owner: i64, id: &str) -> bool {
        let Some(session) = self.find(owner, id) else {
            return false;
        };
        let _ = session.cmd.send(Cmd::Kill).await;
        true
    }

    async fn kill_user(&self, owner: i64) -> usize {
        let sessions: Vec<_> = self
            .lock()
            .values()
            .filter(|s| s.owner == owner)
            .cloned()
            .collect();
        for session in &sessions {
            let _ = session.cmd.send(Cmd::Kill).await;
        }
        sessions.len()
    }

    async fn kill_all(&self) {
        let sessions: Vec<_> = self.lock().values().cloned().collect();
        for session in sessions {
            let _ = session.cmd.send(Cmd::Kill).await;
        }
    }

    /// Start a shell for `owner`.
    fn spawn(
        self: &Arc<Self>,
        owner: i64,
        owner_name: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<SessionHandle>, Refusal> {
        let mut sessions = self.lock();
        if sessions.len() >= MAX_SESSIONS {
            return Err(Refusal::new(
                "busy",
                format!(
                    "{MAX_SESSIONS} terminals are already open on this server; close one first"
                ),
            ));
        }
        let used: Vec<u32> = sessions
            .values()
            .filter(|s| s.owner == owner)
            .map(|s| s.number)
            .collect();
        let number = (1..).find(|n| !used.contains(n)).unwrap_or(1);
        let id = loop {
            let mut raw = [0u8; 8];
            rand::rng().fill_bytes(&mut raw);
            let id = hex::encode(raw);
            if !sessions.contains_key(&id) {
                break id;
            }
        };
        let unavailable = |what: &str, error: std::io::Error| {
            Refusal::new("unavailable", format!("cannot {what}: {error}"))
        };
        let (master, slave) =
            open_pty(cols, rows).map_err(|e| unavailable("open a terminal", e))?;
        let child = spawn_on_pty(self.shell.command(owner_name), slave)
            .map_err(|e| unavailable(&format!("start {}", self.shell.program.display()), e))?;
        let pid = child.id().unwrap_or(0);
        set_nonblocking(master.as_raw_fd())
            .map_err(|e| unavailable("configure the terminal", e))?;
        // SAFETY: an OwnedFd stays open and keeps returning the same descriptor
        // until the AsyncFd that owns it is dropped.
        let master = unsafe { AsyncFd::register(master) }.map_err(|error| {
            let (_, error) = error.into_parts();
            unavailable("watch the terminal", error)
        })?;
        let (cmd, cmds) = mpsc::channel(64);
        let session = Arc::new(SessionHandle {
            id: id.clone(),
            owner,
            number,
            created_at: crate::db::now_ms(),
            pid,
            attached: AtomicBool::new(false),
            cmd,
        });
        sessions.insert(id.clone(), session.clone());
        drop(sessions);
        tracing::info!(session = %id, user = owner_name, "terminal opened");
        let helper = self.clone();
        let run = SessionRun {
            master,
            child,
            cmds,
            size: (cols, rows),
        };
        let handle = session.clone();
        tokio::spawn(async move {
            let end = run.run(&handle).await;
            helper.lock().remove(&handle.id);
            tracing::info!(session = %handle.id, ?end, "terminal closed");
        });
        Ok(session)
    }

    fn next_viewer(&self) -> u64 {
        self.viewers.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug)]
enum End {
    Exited(i32),
    Killed,
    Idle,
    Failed,
}

struct SessionRun {
    master: AsyncFd<OwnedFd>,
    child: Child,
    cmds: mpsc::Receiver<Cmd>,
    size: (u16, u16),
}

impl SessionRun {
    async fn run(mut self, handle: &SessionHandle) -> End {
        let mut scrollback = Scrollback::new(SCROLLBACK);
        let mut viewer: Option<Viewer> = None;
        let mut pending: VecDeque<u8> = VecDeque::new();
        let mut buffer = vec![0u8; READ_CHUNK];
        let mut last_activity = Instant::now();
        let mut exit: Option<i32> = None;
        let mut drain_deadline: Option<Instant> = None;
        let mut terminal_open = true;
        let fd = self.master.as_raw_fd();

        let end = loop {
            if let (Some(code), false) = (exit, terminal_open) {
                break End::Exited(code);
            }
            // Only a shell nobody looks at ends for being idle.
            let deadline = drain_deadline.unwrap_or(if viewer.is_some() {
                Instant::now() + DETACHED_IDLE
            } else {
                last_activity + DETACHED_IDLE
            });
            tokio::select! {
                cmd = self.cmds.recv() => match cmd {
                    None | Some(Cmd::Kill) => break End::Killed,
                    Some(Cmd::Attach { viewer: next, cols, rows, reply }) => {
                        if let Some(old) = viewer.take() {
                            let _ = old.tx.try_send(Out::Detached);
                        }
                        let _ = reply.send(scrollback.snapshot());
                        let (cols, rows) = clamp_size(Some(cols), Some(rows));
                        if (cols, rows) == self.size {
                            // Same size: nudge full-screen programs to redraw.
                            let _ = set_window_size(fd, cols, rows.saturating_sub(1).max(2));
                        }
                        let _ = set_window_size(fd, cols, rows);
                        self.size = (cols, rows);
                        viewer = Some(next);
                        handle.attached.store(true, Ordering::Relaxed);
                        last_activity = Instant::now();
                    }
                    Some(Cmd::Input { viewer: id, bytes }) => {
                        if viewer.as_ref().is_some_and(|v| v.id == id)
                            && exit.is_none()
                            && pending.len() + bytes.len() <= MAX_PENDING_INPUT
                        {
                            pending.extend(bytes);
                        }
                    }
                    Some(Cmd::Resize { viewer: id, cols, rows }) => {
                        if viewer.as_ref().is_some_and(|v| v.id == id) {
                            let (cols, rows) = clamp_size(Some(cols), Some(rows));
                            if set_window_size(fd, cols, rows).is_ok() {
                                self.size = (cols, rows);
                            }
                        }
                    }
                    Some(Cmd::Detach { viewer: id }) => {
                        if viewer.as_ref().is_some_and(|v| v.id == id) {
                            viewer = None;
                            handle.attached.store(false, Ordering::Relaxed);
                            last_activity = Instant::now();
                        }
                    }
                },
                ready = self.master.writable(), if terminal_open && !pending.is_empty() => {
                    let Ok(mut guard) = ready else { break End::Failed };
                    let chunk: &[u8] = pending.make_contiguous();
                    match guard.try_io(|fd| write_fd(fd.as_raw_fd(), chunk)) {
                        Ok(Ok(written)) => {
                            pending.drain(..written);
                        }
                        Ok(Err(_)) => pending.clear(),
                        Err(_would_block) => {}
                    }
                }
                ready = self.master.readable(), if terminal_open => {
                    let Ok(mut guard) = ready else { break End::Failed };
                    match guard.try_io(|fd| read_fd(fd.as_raw_fd(), &mut buffer)) {
                        Ok(Ok(0)) | Ok(Err(_)) => terminal_open = false, // EIO: everyone left
                        Ok(Ok(read)) => {
                            let data = &buffer[..read];
                            scrollback.push(data);
                            last_activity = Instant::now();
                            if let Some(current) = &viewer {
                                let sent = tokio::time::timeout(
                                    VIEWER_STALL,
                                    current.tx.send(Out::Data(data.to_vec())),
                                )
                                .await;
                                if !matches!(sent, Ok(Ok(()))) {
                                    viewer = None;
                                    handle.attached.store(false, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(_would_block) => {}
                    }
                }
                status = self.child.wait(), if exit.is_none() => {
                    exit = Some(match status {
                        Ok(status) => status
                            .code()
                            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                        Err(_) => -1,
                    });
                    drain_deadline = Some(Instant::now() + EXIT_DRAIN);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    match exit {
                        Some(code) => break End::Exited(code),
                        None if viewer.is_none() => break End::Idle,
                        None => {}
                    }
                }
            }
        };
        handle.attached.store(false, Ordering::Relaxed);
        if let Some(viewer) = viewer {
            let code = match end {
                End::Exited(code) => code,
                _ => -1,
            };
            let _ = tokio::time::timeout(VIEWER_STALL, viewer.tx.send(Out::Exit(code))).await;
        }
        // Hang up: the shell (and its foreground jobs) get SIGHUP, like a
        // closed SSH connection.
        drop(self.master);
        if exit.is_none()
            && tokio::time::timeout(HANGUP_GRACE, self.child.wait())
                .await
                .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        end
    }
}

/// The name of the program in the shell's foreground (`bash`, `htop`, …).
fn foreground_program(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 8 (tpgid) comes after the parenthesised command name.
    let after = &stat[stat.rfind(')')? + 1..];
    let tpgid: i64 = after.split_whitespace().nth(5)?.parse().ok()?;
    let target = if tpgid > 0 { tpgid as u32 } else { pid };
    let comm = std::fs::read_to_string(format!("/proc/{target}/comm")).ok()?;
    let name = comm.trim();
    (!name.is_empty()).then(|| name.chars().take(32).collect())
}

// ---------------------------------------------------------------------------
// Connections

async fn respond(writer: &mut OwnedWriteHalf, value: Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

async fn refuse(writer: &mut OwnedWriteHalf, refusal: Refusal) {
    let _ = respond(
        writer,
        json!({"ok": false, "code": refusal.code, "message": refusal.message}),
    )
    .await;
}

/// Serve one connection from the web server.
pub async fn serve_connection(helper: Arc<Helper>, stream: UnixStream) {
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let line = match tokio::time::timeout(REQUEST_TIMEOUT, read_line_capped(&mut reader, MAX_LINE))
        .await
    {
        Ok(Ok(Some(line))) => line,
        _ => return,
    };
    let request: Request = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(_) => {
            refuse(&mut writer, Refusal::new("invalid", "malformed request")).await;
            return;
        }
    };
    match request {
        Request::Status => {
            let open = helper.lock().len();
            let _ = respond(
                &mut writer,
                json!({"ok": true, "version": crate::VERSION, "sessions": open, "max": MAX_SESSIONS, "user": helper.shell.user}),
            )
            .await;
        }
        Request::List { user } => {
            let _ = respond(
                &mut writer,
                json!({"ok": true, "sessions": helper.list(user)}),
            )
            .await;
        }
        Request::Kill { user, session } => {
            if valid_session_id(&session) && helper.kill(user, &session).await {
                let _ = respond(&mut writer, json!({"ok": true})).await;
            } else {
                refuse(
                    &mut writer,
                    Refusal::new("not_found", "this terminal is not open"),
                )
                .await;
            }
        }
        Request::KillUser { user } => {
            let killed = helper.kill_user(user).await;
            let _ = respond(&mut writer, json!({"ok": true, "killed": killed})).await;
        }
        Request::Open {
            user,
            user_name,
            cols,
            rows,
        } => {
            let name: String = user_name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .take(64)
                .collect();
            let (cols, rows) = clamp_size(Some(cols), Some(rows));
            match helper.spawn(user, &name, cols, rows) {
                Ok(session) => attach(helper, session, cols, rows, reader, writer).await,
                Err(refusal) => refuse(&mut writer, refusal).await,
            }
        }
        Request::Attach {
            user,
            session,
            cols,
            rows,
        } => {
            let found = valid_session_id(&session)
                .then(|| helper.find(user, &session))
                .flatten();
            match found {
                Some(session) => {
                    let (cols, rows) = clamp_size(Some(cols), Some(rows));
                    attach(helper, session, cols, rows, reader, writer).await;
                }
                None => {
                    refuse(
                        &mut writer,
                        Refusal::new("not_found", "this terminal is no longer open"),
                    )
                    .await;
                }
            }
        }
    }
}

/// Show `session` on this connection until it ends, is closed, or is taken
/// over by another window.
async fn attach(
    helper: Arc<Helper>,
    session: Arc<SessionHandle>,
    cols: u16,
    rows: u16,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
) {
    let viewer = helper.next_viewer();
    let (tx, mut output) = mpsc::channel::<Out>(64);
    let (reply, scrollback) = oneshot::channel();
    let attached = session
        .cmd
        .send(Cmd::Attach {
            viewer: Viewer { id: viewer, tx },
            cols,
            rows,
            reply,
        })
        .await;
    let Ok(scrollback) = (match attached {
        Ok(()) => scrollback.await.map_err(|_| ()),
        Err(_) => Err(()),
    }) else {
        refuse(
            &mut writer,
            Refusal::new("not_found", "this terminal is no longer open"),
        )
        .await;
        return;
    };
    if respond(&mut writer, json!({"ok": true, "session": session.info()}))
        .await
        .is_err()
    {
        let _ = session.cmd.send(Cmd::Detach { viewer }).await;
        return;
    }
    if !scrollback.is_empty()
        && write_frame(&mut writer, &Frame::Data(scrollback))
            .await
            .is_err()
    {
        let _ = session.cmd.send(Cmd::Detach { viewer }).await;
        return;
    }

    // Frames from the web server, in their own task (not cancel safe).
    let cmd = session.cmd.clone();
    let input = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(Frame::Data(bytes))) => {
                    if cmd.send(Cmd::Input { viewer, bytes }).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::Resize { cols, rows })) => {
                    if cmd.send(Cmd::Resize { viewer, cols, rows }).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::Close)) => {
                    let _ = cmd.send(Cmd::Kill).await;
                    break;
                }
                // The page went away (or sent nonsense): detach, keep the shell.
                _ => {
                    let _ = cmd.send(Cmd::Detach { viewer }).await;
                    break;
                }
            }
        }
    });
    while let Some(next) = output.recv().await {
        let (frame, last) = match next {
            Out::Data(bytes) => (Frame::Data(bytes), false),
            Out::Exit(code) => (Frame::Exit(code), true),
            Out::Detached => (Frame::Detached, true),
        };
        if write_frame(&mut writer, &frame).await.is_err() || last {
            break;
        }
    }
    input.abort();
    let _ = session.cmd.try_send(Cmd::Detach { viewer });
}

/// Accept connections from `allowed` uids (and root) until `shutdown`.
pub async fn serve(
    helper: Arc<Helper>,
    listener: UnixListener,
    allowed_uid: u32,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == allowed_uid || cred.uid() == 0 => {
                        tokio::spawn(serve_connection(helper.clone(), stream));
                    }
                    Ok(cred) => {
                        tracing::warn!(uid = cred.uid(), "refused a terminal connection from another account");
                    }
                    Err(error) => tracing::warn!(error = %error, "cannot read the peer of a terminal connection"),
                }
            }
            _ = &mut shutdown => break,
        }
    }
    helper.kill_all().await;
    // Give the sessions a moment to hang up their shells.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// `rast terminal helper`: run the helper (as root, from rast-terminal.service).
pub async fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let socket = socket_path();
    let peer_name = std::env::var("RAST_TERMINAL_PEER_USER")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_PEER_USER.into());
    let peer = passwd_by_name(&peer_name)
        .ok_or_else(|| format!("account {peer_name} (RAST_TERMINAL_PEER_USER) does not exist"))?;
    let shell = ShellSpec::for_current_user()?;
    // SAFETY: geteuid never fails.
    let root = unsafe { libc::geteuid() } == 0;

    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        if root {
            std::os::unix::fs::chown(dir, Some(0), Some(peer.gid))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(&socket) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(format!("{} exists and is not a socket", socket.display()).into());
        }
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    if root {
        std::os::unix::fs::chown(&socket, Some(0), Some(peer.gid))?;
    }
    tracing::info!(
        socket = %socket.display(),
        shell = %shell.program.display(),
        user = %shell.user,
        peer = %peer_name,
        "terminal helper listening"
    );

    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    };
    serve(Helper::new(shell), listener, peer.uid, shutdown).await;
    let _ = std::fs::remove_file(&socket);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Accounts

#[derive(Debug, Clone)]
pub struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

fn passwd_from(entry: &libc::passwd) -> Passwd {
    // SAFETY: getpw*_r filled these with NUL terminated strings in `buffer`.
    let text = |ptr: *const libc::c_char| {
        if ptr.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    };
    Passwd {
        name: text(entry.pw_name),
        uid: entry.pw_uid,
        gid: entry.pw_gid,
        home: PathBuf::from(text(entry.pw_dir)),
        shell: PathBuf::from(text(entry.pw_shell)),
    }
}

pub fn passwd_by_uid(uid: u32) -> Option<Passwd> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid pointers to an entry, a buffer of the given size and a result slot.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    (rc == 0 && !result.is_null()).then(|| passwd_from(&entry))
}

pub fn passwd_by_name(name: &str) -> Option<Passwd> {
    let name = CString::new(name).ok()?;
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: as above, with a NUL terminated name.
    let rc = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    (rc == 0 && !result.is_null()).then(|| passwd_from(&entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{TerminalClient, TerminalError};

    #[test]
    fn scrollback_keeps_the_tail_from_a_line_start() {
        let mut scrollback = Scrollback::new(10);
        scrollback.push(b"abc\n");
        assert_eq!(scrollback.snapshot(), b"abc\n");
        // 12 bytes > 10: "ab" goes, and the partial line "c" is not shown.
        scrollback.push(b"defghij\n");
        assert_eq!(scrollback.snapshot(), b"defghij\n");
    }

    #[test]
    fn foreground_program_of_this_process() {
        // Not in a terminal here, so it falls back to the process itself.
        let name = foreground_program(std::process::id()).unwrap_or_default();
        assert!(!name.is_empty());
        assert_eq!(foreground_program(0), None);
    }

    fn test_shell() -> ShellSpec {
        let mut shell = ShellSpec::for_current_user().expect("current account");
        shell.program = PathBuf::from("/bin/sh");
        shell.args = Vec::new();
        shell
    }

    async fn read_until(session: &mut crate::terminal::HostSession, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains(needle) {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut session.reader))
                .await
                .expect("output in time")
                .expect("frame");
            match frame {
                Some(Frame::Data(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                other => panic!("unexpected {other:?} while waiting for {needle:?}; saw {seen:?}"),
            }
        }
        seen
    }

    #[tokio::test]
    async fn shells_survive_their_viewer_and_belong_to_their_owner() {
        let dir = std::env::temp_dir().join(format!("rast-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("h.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        // SAFETY: getuid never fails.
        let uid = unsafe { libc::getuid() };
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(Helper::new(test_shell()), listener, uid, async {
            let _ = stopped.await;
        }));
        let client = TerminalClient::new(&socket);

        let status = client.status().await.unwrap();
        assert_eq!(status["sessions"], 0);

        // Open, type, see the output.
        let mut first = client.open(1, "admin", 80, 24).await.unwrap();
        assert_eq!(first.info.number, 1);
        let id = first.info.id.clone();
        write_frame(
            &mut first.writer,
            &Frame::Data(b"echo mark-$((40+2))\n".to_vec()),
        )
        .await
        .unwrap();
        read_until(&mut first, "mark-42").await;

        // Close the page: the shell keeps running, detached.
        drop(first);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let listed = client.list(1).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].attached);
        // Another account sees nothing and cannot attach or end it.
        assert!(client.list(2).await.unwrap().is_empty());
        assert!(matches!(
            client.attach(2, &id, 80, 24).await,
            Err(TerminalError::Refused { .. })
        ));
        assert!(client.kill(2, &id).await.is_err());

        // Attach again: the earlier output comes first.
        let mut second = client.attach(1, &id, 100, 30).await.unwrap();
        read_until(&mut second, "mark-42").await;
        // A third window takes it over; the second is told so.
        let mut third = client.attach(1, &id, 100, 30).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match read_frame(&mut second.reader).await.unwrap() {
                    Some(Frame::Data(_)) => continue,
                    other => break other,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(frame, Some(Frame::Detached));

        // `exit` ends the shell and the viewer gets its status.
        write_frame(&mut third.writer, &Frame::Data(b"exit 3\n".to_vec()))
            .await
            .unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match read_frame(&mut third.reader).await.unwrap() {
                    Some(Frame::Data(_)) => continue,
                    other => break other,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(code, Some(Frame::Exit(3)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(client.list(1).await.unwrap().is_empty());

        // Ending every shell of an account.
        let a = client.open(1, "admin", 80, 24).await.unwrap();
        let b = client.open(1, "admin", 80, 24).await.unwrap();
        assert_eq!((a.info.number, b.info.number), (1, 2));
        drop((a, b));
        assert_eq!(client.kill_user(1).await.unwrap(), 2);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(client.list(1).await.unwrap().is_empty());

        let _ = stop.send(());
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_absent_helper_reads_as_off() {
        let client = TerminalClient::new("/nonexistent/rast-terminal.sock");
        assert!(matches!(client.status().await, Err(TerminalError::Off)));
    }
}
