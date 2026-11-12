//! Terminal sessions into containers (`container-exec`). The wire format is
//! described in [`crate::docker::exec`].
//!
//! The worker re-inspects the container and re-applies
//! [`terminal_policy`] right before it starts the shell, so a client cannot
//! open a terminal the API would refuse. The shell runs as
//! `docker exec -i -t <id> sh -c <fixed script>` on a pseudo terminal owned by
//! the worker; only keyboard bytes and window sizes come from the client.

use std::{
    collections::VecDeque,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::ExitStatusExt,
    },
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    io::{unix::AsyncFd, BufReader},
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    process::Child,
    sync::{mpsc, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use super::{
    docker_command, os_args, run_docker, stderr_summary, write_response, WorkerError,
    WorkerOptions, MAX_CMD_OUTPUT,
};
use crate::docker::{
    containers::parse_inspect_json,
    exec::{
        clamp_size, read_frame, terminal_policy, write_frame, Frame, EXEC_IDLE_TIMEOUT,
        MAX_EXEC_SESSIONS, SHELL_SCRIPT,
    },
    protocol::{
        validate_container_id, validate_request, ValidatedAction, WorkerRequest, WorkerResponse,
    },
};

/// Bytes read from the terminal at a time.
const READ_CHUNK: usize = 16 * 1024;
/// Keyboard bytes buffered while the shell is not reading; the client is not
/// read further until the shell catches up.
const MAX_PENDING_INPUT: usize = 256 * 1024;
/// After the shell exits, its last output is forwarded for at most this long.
const EXIT_DRAIN: Duration = Duration::from_millis(500);
/// After a hang-up, `docker exec` gets this long to end before it is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(3);

/// A shell that is running and connected to the worker's pseudo terminal.
struct Session {
    _permit: OwnedSemaphorePermit,
    child: Child,
    master: AsyncFd<OwnedFd>,
    container: String,
    cols: u16,
    rows: u16,
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The shell exited with this status.
    Exited(i32),
    /// The server closed the session or went away.
    ClientClosed,
    /// Nobody typed for [`EXEC_IDLE_TIMEOUT`].
    Idle,
    /// The terminal itself failed.
    Failed,
}

/// Serve one `container-exec` connection: answer the request line, then
/// bridge frames until the shell exits or the client closes.
pub(super) async fn serve(
    request: WorkerRequest,
    reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    options: &WorkerOptions,
    sessions: &Arc<Semaphore>,
) {
    let id = request.id.clone();
    let mut session = match prepare(&request, options, sessions).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(
                code = error.code,
                "terminal request refused: {}",
                error.message
            );
            let response = WorkerResponse::failure(id, error.code, error.message);
            let _ = write_response(&mut writer, &response).await;
            return;
        }
    };
    let accepted = WorkerResponse::success(
        id,
        json!({"container": session.container, "cols": session.cols, "rows": session.rows}),
    );
    let short = session.container.chars().take(12).collect::<String>();
    if write_response(&mut writer, &accepted).await.is_err() {
        hang_up(session.child, session.master).await;
        return;
    }
    tracing::info!(container = %short, "terminal opened");

    let end = bridge(reader, &mut writer, &session.master, &mut session.child).await;
    match end {
        End::Exited(code) => {
            let _ = write_frame(&mut writer, &Frame::Exit(code)).await;
        }
        End::Idle => {
            let notice = format!(
                "\r\n[Terminal closed after {} minutes without input]\r\n",
                EXEC_IDLE_TIMEOUT.as_secs() / 60
            );
            let _ = write_frame(&mut writer, &Frame::Data(notice.into_bytes())).await;
            let _ = write_frame(&mut writer, &Frame::Exit(-1)).await;
        }
        End::Failed => {
            let _ = write_frame(&mut writer, &Frame::Exit(-1)).await;
        }
        End::ClientClosed => {}
    }
    hang_up(session.child, session.master).await;
    tracing::info!(container = %short, ?end, "terminal closed");
}

/// Validate, check the container and start the shell.
async fn prepare(
    request: &WorkerRequest,
    options: &WorkerOptions,
    sessions: &Arc<Semaphore>,
) -> Result<Session, WorkerError> {
    let (id, cols, rows) = match validate_request(request, &options.apps_dir) {
        Ok(ValidatedAction::Exec { id, cols, rows }) => (id, cols, rows),
        Ok(_) => return Err(WorkerError::invalid("not a terminal request")),
        Err(message) => return Err(WorkerError::invalid(message)),
    };
    let permit = sessions.clone().try_acquire_owned().map_err(|_| {
        WorkerError::new(
            "busy",
            format!("{MAX_EXEC_SESSIONS} terminals are already open; close one and try again"),
        )
    })?;

    let inspect = inspect_container(options, &id).await?;
    if inspect.get("state").and_then(Value::as_str) != Some("running") {
        return Err(WorkerError::new(
            "not_running",
            "the container is not running",
        ));
    }
    let (allowed, reason) = terminal_policy(&inspect);
    if !allowed {
        return Err(WorkerError::forbidden(reason));
    }
    // Address the shell by the full id Docker reported, never by a prefix.
    let container = inspect
        .get("id")
        .and_then(Value::as_str)
        .filter(|full| full.starts_with(id.as_str()) && validate_container_id(full).is_ok())
        .ok_or_else(|| WorkerError::command_failed("docker inspect returned another container"))?
        .to_string();

    let (master, slave) = open_pty(cols, rows)
        .map_err(|error| WorkerError::unavailable(format!("cannot open a terminal: {error}")))?;
    let child = spawn_shell(options, &container, slave)?;
    set_nonblocking(master.as_raw_fd()).map_err(|error| {
        WorkerError::unavailable(format!("cannot configure the terminal: {error}"))
    })?;
    // SAFETY: an OwnedFd stays open and keeps returning the same descriptor
    // until the AsyncFd that owns it is dropped.
    let master = unsafe { AsyncFd::register(master) }.map_err(|error| {
        let (_, error) = error.into_parts();
        WorkerError::unavailable(format!("cannot watch the terminal: {error}"))
    })?;
    Ok(Session {
        _permit: permit,
        child,
        master,
        container,
        cols,
        rows,
    })
}

async fn inspect_container(options: &WorkerOptions, id: &str) -> Result<Value, WorkerError> {
    let output = run_docker(
        options,
        os_args(&["container", "inspect", id]),
        None,
        options.quick_timeout,
        MAX_CMD_OUTPUT,
    )
    .await?;
    if !output.success {
        return Err(WorkerError::not_found(format!(
            "container not found: {}",
            stderr_summary(&output)
        )));
    }
    parse_inspect_json(&output.stdout)
        .map_err(|error| {
            WorkerError::command_failed(format!("cannot parse docker inspect: {error}"))
        })?
        .into_iter()
        .next()
        .ok_or_else(|| WorkerError::not_found("container not found"))
}

/// `docker exec -i -t` with the pseudo terminal as its controlling terminal.
fn spawn_shell(
    options: &WorkerOptions,
    container: &str,
    slave: OwnedFd,
) -> Result<Child, WorkerError> {
    let clone = |fd: &OwnedFd| {
        fd.try_clone()
            .map_err(|error| WorkerError::unavailable(format!("cannot open a terminal: {error}")))
    };
    let stdin = clone(&slave)?;
    let stdout = clone(&slave)?;
    let mut command = docker_command(options);
    command
        .args([
            "exec",
            "-i",
            "-t",
            "-e",
            "TERM=xterm-256color",
            "-e",
            "COLORTERM=truecolor",
            container,
            "sh",
            "-c",
            SHELL_SCRIPT,
        ])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls (setsid, ioctl) run between fork
    // and exec. They make the terminal the child's controlling terminal, so
    // window size changes reach `docker exec` as SIGWINCH.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| {
        WorkerError::unavailable(format!(
            "cannot execute docker ({}): {error}",
            options.docker_bin.display()
        ))
    })?;
    // The command holds copies of the terminal's other end; close them so the
    // terminal reports the end once the shell is gone.
    drop(command);
    Ok(child)
}

/// Move bytes between the client and the terminal until something ends.
async fn bridge(
    mut reader: BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    master: &AsyncFd<OwnedFd>,
    child: &mut Child,
) -> End {
    // Frames are read in their own task: reading a frame is not cancel safe,
    // a channel receive is.
    let (frames_tx, mut frames) = mpsc::channel::<Frame>(8);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(frame @ (Frame::Data(_) | Frame::Resize { .. }))) => {
                    if frames_tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::Close)) | Ok(None) => break,
                Ok(Some(Frame::Exit(_))) => {
                    tracing::warn!("terminal client sent an exit frame; closing");
                    break;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "terminal client sent a malformed frame");
                    break;
                }
            }
        }
    });

    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    let mut idle_deadline = Instant::now() + EXEC_IDLE_TIMEOUT;
    let mut exit: Option<i32> = None;
    let mut drain_deadline: Option<Instant> = None;
    let mut terminal_open = true;

    let end = loop {
        if let (Some(code), false) = (exit, terminal_open) {
            break End::Exited(code);
        }
        let deadline = drain_deadline.unwrap_or(idle_deadline);
        tokio::select! {
            frame = frames.recv(), if exit.is_none() && pending.len() < MAX_PENDING_INPUT => {
                match frame {
                    Some(Frame::Data(bytes)) => {
                        idle_deadline = Instant::now() + EXEC_IDLE_TIMEOUT;
                        pending.extend(bytes);
                    }
                    Some(Frame::Resize { cols, rows }) => {
                        let (cols, rows) = clamp_size(Some(cols), Some(rows));
                        if let Err(error) = set_window_size(master.as_raw_fd(), cols, rows) {
                            tracing::debug!(error = %error, "cannot resize the terminal");
                        }
                    }
                    Some(_) => {}
                    None => break End::ClientClosed,
                }
            }
            ready = master.writable(), if terminal_open && !pending.is_empty() => {
                let Ok(mut guard) = ready else { break End::Failed };
                let chunk: &[u8] = pending.make_contiguous();
                match guard.try_io(|fd| write_fd(fd.as_raw_fd(), chunk)) {
                    Ok(Ok(written)) => {
                        pending.drain(..written);
                    }
                    // The shell is gone; its exit status follows.
                    Ok(Err(_)) => pending.clear(),
                    Err(_would_block) => {}
                }
            }
            ready = master.readable(), if terminal_open => {
                let Ok(mut guard) = ready else { break End::Failed };
                match guard.try_io(|fd| read_fd(fd.as_raw_fd(), &mut buffer)) {
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        // EIO: every process has left the terminal.
                        terminal_open = false;
                    }
                    Ok(Ok(read)) => {
                        let frame = Frame::Data(buffer[..read].to_vec());
                        if write_frame(writer, &frame).await.is_err() {
                            break End::ClientClosed;
                        }
                    }
                    Err(_would_block) => {}
                }
            }
            status = child.wait(), if exit.is_none() => {
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
                    None => break End::Idle,
                }
            }
        }
    };
    reader_task.abort();
    end
}

/// Close the terminal (a hang-up for `docker exec`), then make sure the
/// process is gone.
async fn hang_up(mut child: Child, master: AsyncFd<OwnedFd>) {
    drop(master);
    if tokio::time::timeout(HANGUP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

// ---------------------------------------------------------------------------
// Pseudo terminal helpers

fn open_pty(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = window_size(cols, rows);
    // SAFETY: valid out pointers; the name and termios arguments are optional.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two new descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;
    Ok((master, slave))
}

fn window_size(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn set_window_size(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = window_size(cols, rows);
    // SAFETY: TIOCSWINSZ reads one winsize from the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain descriptor flag calls on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain status flag calls on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn read_fd(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the buffer is valid for `len` bytes.
    let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn write_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: the slice is valid for `len` bytes.
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}
