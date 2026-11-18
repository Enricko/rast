//! Per-user terminal agent: owns the PTY and the shell for one terminal.
//!
//! Started by the privileged helper as a transient systemd unit running as the
//! mapped `rast-u<userId>` identity. It listens on a Unix socket in a
//! per-session directory (owned by the identity, group `rast`, so only the
//! unprivileged RAST server can connect), verifies the random handshake token
//! the helper wrote, spawns the shell on a fresh PTY and then relays bytes
//! until the shell exits or the server closes the connection.
//!
//! The agent deliberately does not inherit anything sensitive: the helper
//! starts it with a minimal, explicitly constructed environment (no RAST
//! database paths, secrets, helper/worker sockets or Docker socket are
//! readable by the identity anyway). The shell environment consists of that
//! environment plus the runtime PATH selected by the server.

use std::{
    ffi::{CStr, CString},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::PermissionsExt,
    },
    path::Path,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::terminal::protocol::{
    clamp_size, read_server_frame, tokens_match, write_frame, AgentFrame, Handshake,
    HandshakeReply, ServerFrame, MAX_HANDSHAKE, VERSION,
};

/// How long the agent waits for the server to connect after it is ready.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(45);
/// Terminal bytes read from the PTY at a time.
const READ_CHUNK: usize = 16 * 1024;
/// How long a closed connection gets to terminate the shell before SIGKILL.
const TERMINATE_GRACE: Duration = Duration::from_millis(700);

/// Run one terminal agent. Returns when the shell/agent is finished.
pub async fn run(
    socket_path: std::path::PathBuf,
    token_file: std::path::PathBuf,
    shell: std::path::PathBuf,
    cwd: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let token = read_token(&token_file)?;
    // The token is single-use: remove it before accepting a connection.
    let _ = std::fs::remove_file(&token_file);
    if !cwd.is_dir() {
        return Err(format!(
            "terminal working directory is not a directory: {}",
            cwd.display()
        )
        .into());
    }
    if !shell.is_absolute() || !is_executable(&shell) {
        return Err(format!("terminal shell is not executable: {}", shell.display()).into());
    }
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;

    let (mut stream, _peer) = match tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => return Err(format!("terminal socket accept failed: {error}").into()),
        Err(_) => return Err("the RAST server did not connect to the terminal in time".into()),
    };
    // Only one viewer can own a terminal.
    drop(listener);

    let (cols, rows) = handshake(&mut stream, &token).await?;
    session(stream, &shell, &cwd, cols, rows).await?;
    Ok(())
}

fn read_token(path: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let raw = std::fs::read_to_string(path)?;
    let token = raw.trim().to_string();
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("terminal token file is invalid".into());
    }
    Ok(token)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

async fn handshake(
    stream: &mut UnixStream,
    token: &str,
) -> Result<(u16, u16), Box<dyn std::error::Error + Send + Sync>> {
    let mut line = Vec::with_capacity(256);
    loop {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(15), stream.read_exact(&mut byte)).await {
            Ok(Ok(_)) => {}
            _ => return Err("the RAST server did not send a valid terminal handshake".into()),
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_HANDSHAKE {
            return Err("the terminal handshake was too long".into());
        }
    }
    let raw = String::from_utf8(line)?;
    let handshake: Handshake =
        serde_json::from_str(raw.trim()).map_err(|_| "the terminal handshake was malformed")?;
    if !tokens_match(token, &handshake.token) {
        let reply = HandshakeReply {
            ok: false,
            version: VERSION,
            pid: 0,
            error: Some("terminal handshake token mismatch".into()),
        };
        let mut bytes = serde_json::to_vec(&reply)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        return Err("terminal handshake token mismatch".into());
    }
    let (cols, rows) = clamp_size(Some(handshake.cols), Some(handshake.rows));
    let reply = HandshakeReply {
        ok: true,
        version: VERSION,
        pid: std::process::id(),
        error: None,
    };
    let mut bytes = serde_json::to_vec(&reply)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok((cols, rows))
}

fn agent_debug(message: &str) {
    if std::env::var_os("RAST_TERM_DEBUG").is_some() {
        eprintln!("[terminal-agent {}] {message}", std::process::id());
    }
}

/// PTY master + slave pair.
struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn open_pty() -> io::Result<Pty> {
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(master_fd);
        if libc::grantpt(master_fd) != 0 || libc::unlockpt(master_fd) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = [0 as libc::c_char; 256];
        if libc::ptsname_r(master_fd, buffer.as_mut_ptr(), buffer.len()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let name = CStr::from_ptr(buffer.as_ptr())
            .to_str()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid PTY name"))?;
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid PTY name"))?;
        let slave_fd = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if slave_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pty {
            master,
            slave: OwnedFd::from_raw_fd(slave_fd),
        })
    }
}

fn set_window_size(fd: RawFd, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &size);
    }
}

pub use crate::native::LOGIN_SHELL_ENV;

/// Environment for the shell: the agent's (deliberate) environment, with the
/// terminal identity forced and secrets from the service environment dropped.
fn shell_environment(shell: &Path) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            // The helper starts the agent with an explicit environment; never
            // let the service account's secrets reach the user shell.
            !matches!(
                key.as_str(),
                "RAST_CONFIG"
                    | "RAST_DATA_DIR"
                    | "RAST_SECRET_KEY"
                    | "RAST_MASTER_KEY"
                    | LOGIN_SHELL_ENV
            ) && !key.starts_with("RAST_PACKAGES_")
        })
        .collect();
    let upsert = |entries: &mut Vec<(String, String)>, key: &str, value: &str| {
        if let Some(existing) = entries.iter_mut().find(|(name, _)| name == key) {
            existing.1 = value.to_string();
        } else {
            entries.push((key.to_string(), value.to_string()));
        }
    };
    upsert(&mut entries, "TERM", "xterm-256color");
    upsert(&mut entries, "SHELL", &shell.display().to_string());
    if !entries.iter().any(|(key, _)| key == "PATH") {
        upsert(
            &mut entries,
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }
    entries
}

fn c_strings(entries: &[(String, String)]) -> io::Result<(Vec<CString>, Vec<*const libc::c_char>)> {
    let owned: Vec<CString> = entries
        .iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")))
        .collect::<Result<_, _>>()?;
    let mut pointers: Vec<*const libc::c_char> = owned.iter().map(|value| value.as_ptr()).collect();
    pointers.push(std::ptr::null());
    Ok((owned, pointers))
}

/// Fork and exec the shell on `slave`, in a new session with the PTY as
/// controlling terminal. Only async-signal-safe calls happen after `fork`.
unsafe fn spawn_shell(
    shell: &CStr,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    cwd: &CStr,
    slave: RawFd,
    master: RawFd,
) -> io::Result<libc::pid_t> {
    let pid = libc::fork();
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Child. No allocation or locks here: only libc calls until execve().
        libc::setsid();
        libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
        // Rust ignores SIGPIPE and tokio installs handlers; a shell must get
        // default dispositions (pipelines rely on SIGPIPE, Ctrl+C on SIGINT).
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
        libc::dup2(slave, 0);
        libc::dup2(slave, 1);
        libc::dup2(slave, 2);
        if slave > 2 {
            libc::close(slave);
        }
        // Make the shell's process group the terminal's foreground group;
        // otherwise an interactive shell gets SIGTTIN/SIGTTOU and stops.
        libc::tcsetpgrp(0, libc::getpid());
        libc::close(master);
        libc::chdir(cwd.as_ptr());
        libc::execve(shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
        libc::_exit(127);
    }
    Ok(pid)
}

fn kill_group(pid: libc::pid_t, signal: libc::c_int) {
    // The shell is a session/process-group leader, so the negative pid targets
    // the whole terminal process tree (jobs, compilers, TUI programs).
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Prepare argv/env and fork/exec the shell. Kept in a synchronous helper so
/// no raw pointers (non-Send) are held across an await in the async session.
fn spawn_shell_process(
    shell: &Path,
    cwd: &Path,
    slave: RawFd,
    master: RawFd,
) -> io::Result<libc::pid_t> {
    let cwd_c = CString::new(cwd.display().to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid working directory"))?;
    let shell_c = CString::new(shell.display().to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid shell path"))?;
    let shell_arg = CString::new(shell.display().to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid shell path"))?;
    let interactive = CString::new("-i").expect("static argument");
    let login = CString::new("-l").expect("static argument");
    // An account shell (`terminal_admin_account`) is a login shell, so the
    // account's own profile builds the environment, as over SSH.
    let argv_ptrs = if std::env::var_os(LOGIN_SHELL_ENV).is_some() {
        vec![
            shell_arg.as_ptr(),
            login.as_ptr(),
            interactive.as_ptr(),
            std::ptr::null(),
        ]
    } else {
        vec![shell_arg.as_ptr(), interactive.as_ptr(), std::ptr::null()]
    };
    let env = shell_environment(shell);
    let (owned_env, env_ptrs) = c_strings(&env)?;
    let pid = unsafe { spawn_shell(&shell_c, &argv_ptrs, &env_ptrs, &cwd_c, slave, master) }?;
    // Keep the owning CStrings alive until after the child inherited them.
    drop(owned_env);
    Ok(pid)
}

enum End {
    /// Shell exited by itself.
    Exited(Option<i32>),
    /// Server closed the socket.
    Closed,
    /// Agent received SIGTERM/SIGINT (systemd stop).
    Signalled,
}

async fn session(
    stream: UnixStream,
    shell: &Path,
    cwd: &Path,
    cols: u16,
    rows: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pty = open_pty()?;
    let master_fd = pty.master.as_raw_fd();
    set_window_size(master_fd, cols, rows);
    let pid = spawn_shell_process(shell, cwd, pty.slave.as_raw_fd(), pty.master.as_raw_fd())?;
    agent_debug(&format!("spawned shell pid {pid}"));
    // Parent does not use the slave end; the child streams through it.
    drop(pty.slave);

    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<Option<i32>>();
    std::thread::spawn(move || {
        let mut status: libc::c_int = 0;
        loop {
            let result = unsafe { libc::waitpid(pid, &mut status, 0) };
            if result == pid || result < 0 {
                break;
            }
        }
        let code = if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Some(128 + libc::WTERMSIG(status))
        } else {
            None
        };
        let _ = exit_tx.send(code);
    });

    let master_file = unsafe { std::fs::File::from_raw_fd(pty.master.as_raw_fd()) };
    std::mem::forget(pty.master);
    let master_fd = master_file.as_raw_fd();
    // Blocking reader thread: PTY output -> bounded async channel (backpressure).
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<io::Result<Vec<u8>>>(32);
    let mut reader_file = master_file
        .try_clone()
        .map_err(|error| format!("cannot duplicate the PTY master: {error}"))?;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match reader_file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if output_tx.blocking_send(Ok(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = output_tx.blocking_send(Err(error));
                    break;
                }
            }
        }
    });
    // Blocking writer thread: keyboard bytes -> PTY master.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut writer_file = master_file;
    std::thread::spawn(move || {
        use std::io::Write;
        while let Ok(bytes) = input_rx.recv() {
            if writer_file.write_all(&bytes).is_err() || writer_file.flush().is_err() {
                break;
            }
        }
    });
    let (socket_read, mut socket_write) = stream.into_split();
    let mut socket_read = BufReader::new(socket_read);

    #[cfg(unix)]
    let terminate_signal = {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        async move {
            tokio::select! {
                _ = term.recv() => {},
                _ = interrupt.recv() => {},
            }
        }
    };
    #[cfg(unix)]
    tokio::pin!(terminate_signal);

    let end = loop {
        tokio::select! {
            frame = read_server_frame(&mut socket_read) => {
                match frame {
                    Ok(Some(ServerFrame::Input(bytes))) => {
                        agent_debug(&format!("input {} bytes", bytes.len()));
                        if input_tx.send(bytes).is_err() {
                            break End::Closed;
                        }
                    }
                    Ok(Some(ServerFrame::Resize { cols, rows })) => {
                        agent_debug(&format!("resize {cols}x{rows}"));
                        set_window_size(master_fd, cols, rows);
                    }
                    Ok(Some(ServerFrame::Signal { signal })) => {
                        kill_group(pid, signal as libc::c_int);
                    }
                    Ok(Some(ServerFrame::Close)) | Ok(None) | Err(_) => break End::Closed,
                }
            }
            output = output_rx.recv() => {
                match output {
                    Some(Ok(bytes)) => {
                        agent_debug(&format!(
                            "pty output {} bytes",
                            bytes.len()
                        ));
                        let frame = AgentFrame::Output(bytes).encode()?;
                        if write_frame(&mut socket_write, &frame).await.is_err() {
                            break End::Closed;
                        }
                    }
                    Some(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                        break End::Exited(None);
                    }
                    Some(Err(_)) | None => break End::Exited(None),
                }
            }
            code = &mut exit_rx => {
                break End::Exited(code.ok().flatten());
            }
            _ = &mut terminate_signal => {
                break End::Signalled;
            }
        }
    };

    agent_debug("session loop ended");
    // End the whole terminal process tree deterministically.
    kill_group(pid, libc::SIGTERM);
    tokio::time::sleep(TERMINATE_GRACE).await;
    kill_group(pid, libc::SIGKILL);

    agent_debug(&format!(
        "sending exit frame (ended={})",
        match end {
            End::Exited(_) => "exited",
            End::Closed => "closed",
            End::Signalled => "signalled",
        }
    ));
    match end {
        End::Exited(code) => {
            let code = match code {
                Some(code) => Some(code),
                None => tokio::time::timeout(Duration::from_secs(2), &mut exit_rx)
                    .await
                    .ok()
                    .and_then(|result| result.ok())
                    .flatten(),
            };
            let frame = AgentFrame::Exit { code }.encode()?;
            let _ = socket_write.write_all(&frame).await;
            let _ = socket_write.flush().await;
        }
        End::Closed | End::Signalled => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::protocol::{read_agent_frame, write_frame, Handshake, HandshakeReply};
    use std::path::PathBuf;

    fn test_token() -> String {
        "a".repeat(64)
    }

    fn test_socket_path(dir: &Path) -> std::path::PathBuf {
        dir.join("agent.sock")
    }

    async fn wait_for_socket(path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("agent socket did not appear at {}", path.display());
    }

    async fn connect_agent(
        socket: &Path,
        token: &str,
        cols: u16,
        rows: u16,
    ) -> (UnixStream, HandshakeReply) {
        let mut stream = UnixStream::connect(socket).await.expect("connect agent");
        let handshake = Handshake {
            token: token.to_string(),
            cols,
            rows,
        };
        let mut line = serde_json::to_vec(&handshake).unwrap();
        line.push(b'\n');
        stream.write_all(&line).await.unwrap();
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' {
                break;
            }
            reply.push(byte[0]);
        }
        let reply: HandshakeReply = serde_json::from_slice(&reply).unwrap();
        (stream, reply)
    }

    /// A real PTY agent: start `sh`, type commands, resize and close it.
    #[tokio::test]
    async fn agent_runs_a_shell_on_a_pty() {
        let temp = crate::test_support::socket_tempdir();
        let socket = test_socket_path(temp.path());
        let token_file = temp.path().join("token");
        std::fs::write(&token_file, test_token()).unwrap();
        let shell = PathBuf::from("/bin/sh");
        let cwd = temp.path().to_path_buf();
        let handle = tokio::spawn(run(socket.clone(), token_file, shell, cwd));
        wait_for_socket(&socket).await;
        let (mut stream, reply) = connect_agent(&socket, &test_token(), 100, 30).await;
        assert!(reply.ok);
        assert_eq!(reply.version, VERSION);

        // Type a command; the PTY echoes it and the shell prints its output.
        let input = ServerFrame::Input(b"echo agent-ok-12345\n".to_vec())
            .encode()
            .unwrap();
        write_frame(&mut stream, &input).await.unwrap();
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let frame =
                tokio::time::timeout(Duration::from_millis(500), read_agent_frame(&mut stream))
                    .await;
            match frame {
                Ok(Ok(Some(AgentFrame::Output(bytes)))) => {
                    output.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&output).contains("agent-ok-12345") {
                        break;
                    }
                }
                Ok(Ok(Some(AgentFrame::Exit { .. }))) => panic!("shell exited early"),
                Ok(Ok(None)) | Ok(Err(_)) => panic!("agent connection closed early"),
                Ok(Ok(Some(AgentFrame::Error(message)))) => panic!("agent error: {message}"),
                Err(_) => {}
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("agent-ok-12345"),
            "shell output not observed: {:?}",
            String::from_utf8_lossy(&output)
        );

        // Resize must not kill the shell (TIOCSWINSZ).
        let resize = ServerFrame::Resize {
            cols: 132,
            rows: 43,
        }
        .encode()
        .unwrap();
        write_frame(&mut stream, &resize).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Ctrl+D (end of input) ends an interactive sh and the agent reports exit.
        let eof = ServerFrame::Input(vec![0x04]).encode().unwrap();
        write_frame(&mut stream, &eof).await.unwrap();
        let mut exited = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), read_agent_frame(&mut stream))
                .await
            {
                Ok(Ok(Some(AgentFrame::Exit { code }))) => {
                    exited = Some(code);
                    break;
                }
                Ok(Ok(Some(AgentFrame::Output(_)))) => continue,
                Ok(Ok(Some(AgentFrame::Error(message)))) => panic!("agent error: {message}"),
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {}
            }
        }
        let code = exited.expect("no exit frame");
        assert_eq!(code, Some(0));
        let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(joined.is_ok(), "agent task did not finish");
    }

    /// A wrong token must be rejected and the shell never started.
    #[tokio::test]
    async fn agent_rejects_a_wrong_token() {
        let temp = crate::test_support::socket_tempdir();
        let socket = test_socket_path(temp.path());
        let token_file = temp.path().join("token");
        std::fs::write(&token_file, test_token()).unwrap();
        let handle = tokio::spawn(run(
            socket.clone(),
            token_file,
            PathBuf::from("/bin/sh"),
            temp.path().to_path_buf(),
        ));
        wait_for_socket(&socket).await;
        let (mut stream, reply) = connect_agent(&socket, &"b".repeat(64), 80, 24).await;
        assert!(!reply.ok);
        // Agent closes the socket; reading yields EOF rather than shell output.
        let mut buffer = [0u8; 8];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await;
        assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))));
        let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(joined.is_ok());
    }

    /// A service secret in the inherited environment must not reach the shell.
    #[tokio::test]
    async fn agent_strips_service_secrets_from_the_shell_environment() {
        std::env::set_var("RAST_CONFIG", "/etc/rast/secret-config");
        let temp = crate::test_support::socket_tempdir();
        let socket = test_socket_path(temp.path());
        let token_file = temp.path().join("token");
        std::fs::write(&token_file, test_token()).unwrap();
        let handle = tokio::spawn(run(
            socket.clone(),
            token_file,
            PathBuf::from("/bin/sh"),
            temp.path().to_path_buf(),
        ));
        wait_for_socket(&socket).await;
        let (mut stream, _reply) = connect_agent(&socket, &test_token(), 100, 30).await;
        write_frame(
            &mut stream,
            &ServerFrame::Input(b"echo config=[$RAST_CONFIG]\n".to_vec())
                .encode()
                .unwrap(),
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(Some(AgentFrame::Output(bytes)))) =
                tokio::time::timeout(Duration::from_millis(500), read_agent_frame(&mut stream))
                    .await
            {
                output.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&output).contains("config=[]") {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("config=[]"),
            "secret leaked into shell: {text}"
        );
        let _ = write_frame(&mut stream, &ServerFrame::Close.encode().unwrap()).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        std::env::remove_var("RAST_CONFIG");
    }
}
