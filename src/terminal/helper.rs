//! The terminal helper: shells that outlive their browser tab. See
//! [`crate::terminal`] for the design.
//!
//! Each shell runs on its own pseudo terminal, owned by a task that keeps its
//! recent output and forwards it to at most one viewer at a time. What a
//! shell *is* comes from a [`Launcher`]: `rast terminal helper` (root, from
//! `rast-terminal.service`) starts login shells of its own account
//! ([`HostLauncher`]); the Docker worker starts shells in per-account
//! sandbox containers (`crate::worker::sandbox`).

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

use async_trait::async_trait;
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
    socket_path, valid_session_id, Request, RunOutput, SessionInfo, DEFAULT_PEER_USER,
    DETACHED_IDLE, MAX_LINE, MAX_RUN_COMMAND, MAX_RUN_SECS, MAX_SESSIONS, RUN_OUTPUT, SCROLLBACK,
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

/// What the helper's shells are, and how they start.
#[async_trait]
pub trait Launcher: Send + Sync + 'static {
    /// Fields added to the `status` answer (`user`: who the shells run as).
    async fn status(&self) -> Value;
    /// Get ready to start shells for `owner` without waiting (sandboxes:
    /// build the image in the background). Answers like [`Self::status`].
    async fn prepare(&self, owner: i64, owner_name: &str) -> Result<Value, Refusal> {
        let _ = (owner, owner_name);
        Ok(self.status().await)
    }
    /// The command of a new shell for `owner`, run on the shell's terminal.
    async fn command(&self, owner: i64, owner_name: &str) -> Result<Command, Refusal>;
    /// The command that runs `script` once for `owner` in a login shell
    /// without a terminal (stdin empty), within `timeout` (the helper also
    /// stops it then; launchers whose processes the helper cannot stop by
    /// itself, such as containers, stop it inside too).
    async fn run_command(
        &self,
        owner: i64,
        owner_name: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<Command, Refusal>;
    /// Whether the shell's foreground program can be read from `/proc`
    /// (not for shells inside containers).
    fn shows_program(&self) -> bool {
        true
    }
    /// `owner` lost access: their shells were ended, clean up after them.
    async fn forget(&self, owner: i64) {
        let _ = owner;
    }
}

/// Login shells of the helper's own account (root on a server).
pub struct HostLauncher(pub ShellSpec);

#[async_trait]
impl Launcher for HostLauncher {
    async fn status(&self) -> Value {
        json!({"user": self.0.user})
    }

    async fn command(&self, _owner: i64, owner_name: &str) -> Result<Command, Refusal> {
        Ok(self.0.command(owner_name))
    }

    async fn run_command(
        &self,
        _owner: i64,
        owner_name: &str,
        script: &str,
        _timeout: Duration,
    ) -> Result<Command, Refusal> {
        let mut command = self.0.command(owner_name);
        command.arg("-c").arg(script);
        quiet_env(&mut command);
        Ok(command)
    }
}

/// No colours, pagers or prompts in output nobody watches live.
pub fn quiet_env(command: &mut Command) {
    command
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("SYSTEMD_PAGER", "")
        .env("NO_COLOR", "1")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env_remove("COLORTERM");
}

/// Run a launcher's command to the end (or until `timeout`, then its whole
/// process group is killed) and keep up to [`RUN_OUTPUT`] of each stream.
pub async fn run_once(mut command: Command, timeout: Duration) -> RunOutput {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunOutput {
                stderr: format!("cannot start the shell: {error}"),
                ..RunOutput::default()
            }
        }
    };
    let pid = child.id();
    async fn keep(stream: Option<impl tokio::io::AsyncRead + Unpin>) -> (Vec<u8>, bool) {
        let mut kept = Vec::new();
        let mut truncated = false;
        let Some(mut stream) = stream else {
            return (kept, truncated);
        };
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = RUN_OUTPUT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                    truncated |= n > room;
                }
            }
        }
        (kept, truncated)
    }
    let out = tokio::spawn(keep(child.stdout.take()));
    let err = tokio::spawn(keep(child.stderr.take()));
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status.ok(), false),
        Err(_) => {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                // SAFETY: signalling the process group this helper started.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            (None, true)
        }
    };
    // Programs that left the group may hold the pipes open: stop waiting.
    let collect = |task: tokio::task::JoinHandle<(Vec<u8>, bool)>| async move {
        match tokio::time::timeout(Duration::from_secs(2), task).await {
            Ok(Ok(kept)) => kept,
            _ => (Vec::new(), true),
        }
    };
    let (stdout, cut_out) = collect(out).await;
    let (stderr, cut_err) = collect(err).await;
    RunOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout: clean_output(&stdout),
        stderr: clean_output(&stderr),
        truncated: cut_out || cut_err,
        timed_out,
    }
}

/// Text of a command's output: escape sequences and control characters
/// removed, a line redrawn with carriage returns kept as last drawn.
pub fn clean_output(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI (`ESC [ … final`), OSC (`ESC ] … BEL/ST`) or a two-character escape.
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c == '\n' || c == '\t' || c == '\r' || !c.is_control() {
            plain.push(c);
        }
    }
    plain
        .split('\n')
        .map(|line| {
            line.trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many shells a helper keeps open.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// On the whole server.
    pub total: usize,
    /// Per account.
    pub per_owner: usize,
}

impl Limits {
    /// The root helper's limits.
    pub const HOST: Self = Self {
        total: MAX_SESSIONS,
        per_owner: MAX_SESSIONS,
    };
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
    launcher: Arc<dyn Launcher>,
    limits: Limits,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    viewers: AtomicU64,
}

/// Why a request was refused: a stable `code` and a message for people.
#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Helper {
    pub fn new(launcher: Arc<dyn Launcher>, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            launcher,
            limits,
            sessions: Mutex::new(HashMap::new()),
            viewers: AtomicU64::new(1),
        })
    }

    /// Accounts that have shells open.
    pub fn owners(&self) -> Vec<i64> {
        let mut owners: Vec<i64> = self.lock().values().map(|s| s.owner).collect();
        owners.sort_unstable();
        owners.dedup();
        owners
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
        // Let the shells hang up before cleaning up after them.
        for _ in 0..40 {
            if !self.lock().values().any(|s| s.owner == owner) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.launcher.forget(owner).await;
        sessions.len()
    }

    /// Refuse a new shell for `owner` when a limit is reached.
    fn check_limits(&self, owner: i64) -> Result<(), Refusal> {
        let sessions = self.lock();
        if sessions.len() >= self.limits.total {
            return Err(Refusal::new(
                "busy",
                format!(
                    "{} terminals are already open on this server; close one first",
                    self.limits.total
                ),
            ));
        }
        if sessions.values().filter(|s| s.owner == owner).count() >= self.limits.per_owner {
            return Err(Refusal::new(
                "busy",
                format!(
                    "You already have {} terminals open; close one first",
                    self.limits.per_owner
                ),
            ));
        }
        Ok(())
    }

    async fn kill_all(&self) {
        let sessions: Vec<_> = self.lock().values().cloned().collect();
        for session in sessions {
            let _ = session.cmd.send(Cmd::Kill).await;
        }
    }

    /// Start a shell for `owner`.
    async fn spawn(
        self: &Arc<Self>,
        owner: i64,
        owner_name: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<SessionHandle>, Refusal> {
        self.check_limits(owner)?;
        let command = self.launcher.command(owner, owner_name).await?;
        self.check_limits(owner)?;
        let mut sessions = self.lock();
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
        let program = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        let child = spawn_on_pty(command, slave)
            .map_err(|e| unavailable(&format!("start {program}"), e))?;
        let pid = if self.launcher.shows_program() {
            child.id().unwrap_or(0)
        } else {
            0
        };
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
/// Linux only (`/proc`); `None` elsewhere.
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
            let mut answer = json!({
                "ok": true,
                "version": crate::VERSION,
                "sessions": open,
                "max": helper.limits.total,
                "perUser": helper.limits.per_owner,
            });
            merge(&mut answer, helper.launcher.status().await);
            let _ = respond(&mut writer, answer).await;
        }
        Request::Owners => {
            let _ = respond(&mut writer, json!({"ok": true, "owners": helper.owners()})).await;
        }
        Request::Prepare { user, user_name } => {
            match helper.launcher.prepare(user, &clean_name(&user_name)).await {
                Ok(state) => {
                    let mut answer = json!({"ok": true});
                    merge(&mut answer, state);
                    let _ = respond(&mut writer, answer).await;
                }
                Err(refusal) => refuse(&mut writer, refusal).await,
            }
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
        Request::Run {
            user,
            user_name,
            command,
            timeout_secs,
        } => {
            if command.trim().is_empty()
                || command.len() > MAX_RUN_COMMAND
                || command.contains('\0')
            {
                refuse(
                    &mut writer,
                    Refusal::new("invalid", "no command, or too long"),
                )
                .await;
                return;
            }
            let timeout = Duration::from_secs(u64::from(timeout_secs.clamp(1, MAX_RUN_SECS)));
            let name = clean_name(&user_name);
            match helper
                .launcher
                .run_command(user, &name, &command, timeout)
                .await
            {
                Ok(launch) => {
                    let output = run_once(launch, timeout).await;
                    let mut answer = serde_json::to_value(&output).unwrap_or_else(|_| json!({}));
                    merge(&mut answer, json!({"ok": true}));
                    let _ = respond(&mut writer, answer).await;
                }
                Err(refusal) => refuse(&mut writer, refusal).await,
            }
        }
        Request::Open {
            user,
            user_name,
            cols,
            rows,
        } => {
            let name = clean_name(&user_name);
            let (cols, rows) = clamp_size(Some(cols), Some(rows));
            match helper.spawn(user, &name, cols, rows).await {
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

/// An account name as it may appear in an environment variable.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect()
}

/// Add `extra`'s fields to the object `answer`.
fn merge(answer: &mut Value, extra: Value) {
    if let (Some(answer), Value::Object(extra)) = (answer.as_object_mut(), extra) {
        for (key, value) in extra {
            answer.entry(key).or_insert(value);
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
    serve(
        Helper::new(Arc::new(HostLauncher(shell)), Limits::HOST),
        listener,
        peer.uid,
        shutdown,
    )
    .await;
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

    // Reads /proc: on other systems tabs show only the shell number.
    #[cfg(target_os = "linux")]
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

    #[test]
    fn command_output_is_plain_text() {
        let raw = b"\x1b[1;32mok\x1b[0m done\r\nprogress 10%\rprogress 100%\n\x1b]0;title\x07tab\there\x00";
        assert_eq!(clean_output(raw), "ok done\nprogress 100%\ntab\there");
    }

    #[tokio::test]
    async fn commands_run_once_with_their_output_and_time_limit() {
        let dir = std::env::temp_dir().join(format!("rast-term-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("h.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        // SAFETY: getuid never fails.
        let uid = unsafe { libc::getuid() };
        let (stop, stopped) = oneshot::channel::<()>();
        let helper = Helper::new(
            Arc::new(HostLauncher(test_shell())),
            Limits {
                total: 8,
                per_owner: 3,
            },
        );
        let server = tokio::spawn(serve(helper.clone(), listener, uid, async {
            let _ = stopped.await;
        }));
        let client = TerminalClient::new(&socket);
        let out = client
            .run(
                1,
                "alice",
                "echo hello; echo oops >&2; printf 'TERM=%s' \"$TERM\"; exit 3",
                10,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.stdout, "hello\nTERM=dumb");
        assert_eq!(out.stderr, "oops\n");
        assert!(!out.timed_out && !out.truncated);
        // stdin is empty: `cat` ends at once.
        assert_eq!(
            client
                .run(1, "alice", "cat; echo end", 10)
                .await
                .unwrap()
                .stdout,
            "end\n"
        );
        let started = Instant::now();
        let out = client
            .run(1, "alice", "echo before; sleep 30; echo after", 1)
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert_eq!(out.stdout, "before\n");
        assert!(started.elapsed() < Duration::from_secs(10));
        let big = client
            .run(1, "alice", "head -c 200000 /dev/zero | tr '\\0' a", 10)
            .await
            .unwrap();
        assert!(big.truncated);
        assert_eq!(big.stdout.len(), RUN_OUTPUT);
        assert!(matches!(
            client.run(1, "alice", "   ", 10).await,
            Err(TerminalError::Refused { .. })
        ));
        assert!(helper.list(1).is_empty(), "no shell is left open");
        let _ = stop.send(());
        let _ = server.await;
        let _ = std::fs::remove_dir_all(&dir);
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
        let helper = Helper::new(
            Arc::new(HostLauncher(test_shell())),
            Limits {
                total: 8,
                per_owner: 3,
            },
        );
        let server = tokio::spawn(serve(helper.clone(), listener, uid, async {
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
        assert_eq!(helper.owners(), vec![1]);
        // The per-account limit (3 here).
        let c = client.open(1, "admin", 80, 24).await.unwrap();
        assert!(matches!(
            client.open(1, "admin", 80, 24).await,
            Err(TerminalError::Refused { code, .. }) if code == "busy"
        ));
        drop((a, b, c));
        assert_eq!(client.kill_user(1).await.unwrap(), 3);
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
