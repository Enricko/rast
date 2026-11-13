//! Root daemon of the native helper.
//!
//! [`serve`] binds the Unix socket, drops stale socket files, enforces the
//! socket permissions and the optional peer-uid allow-list, and processes
//! JSON-lines requests one at a time per connection. All mutating work goes
//! through a bounded semaphore; APT transactions are serialized with one mutex
//! so two installs never interleave and apt's own lock is never contended.
//!
//! Every handler re-validates its input with [`super::protocol`], constructs
//! argument vectors (never shell strings), pins absolute binaries, applies
//! command deadlines and caps captured output. Long operations are logged with
//! the op name, user id/unit and duration — never with environment values.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
};

use crate::native::{
    identity::{self, Identity},
    protocol::{
        identity_ids, is_managed_unit, last_lines, session_unit, validate_absolute_path,
        validate_acl_perms, validate_app_id, validate_app_unit, validate_argv, validate_argv0,
        validate_cwd, validate_deb_path, validate_deb_version, validate_display, validate_env,
        validate_limits, validate_managed_path, validate_managed_uid, validate_managed_unit,
        validate_mode, validate_package_name, validate_packages, validate_port, validate_run_id,
        validate_runtime_dir, validate_session_args, validate_sha256, validate_symlink_target,
        validate_unit, validate_xpra_bin, AppStartOp, AppStopOp, DebDownloadOp, DebInstallOp,
        DebPreviewOp, DebRemoveOp, IdentityEnsureOp, IdentityRemoveOp, NativeError, NativeOp,
        NativeRequest, NativeResponse, PackageFilesOp, PackageInstalledListOp, PackagePolicyOp,
        PathAclOp, PathChmodOp, PathChownOp, PathMkdirOp, PathRemoveOp, PathSymlinkOp,
        ProfileCleanLocksOp, ProfileInstallOp, ProfilePrepareOp, ProfileRemoveOp, ResourceLimits,
        SessionStartOp, SessionStopOp, UnitStatusOp, MAX_OUTPUT_BYTES, MAX_REQUEST_LINE_BYTES,
        MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
    },
    run_command, run_command_in, summarize_command, CommandOutput, NativeOptions, LINUX_SLICE,
};

/// Deadline of cheap helper commands (kept under the client's 15 s deadline).
const QUICK_CMD_TIMEOUT: Duration = Duration::from_secs(13);
/// Deadline of `systemd-run` (unit start) commands.
const SYSTEMD_RUN_TIMEOUT: Duration = Duration::from_secs(13);
/// Deadline of `systemctl` queries/actions.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(13);
/// Deadline of an `apt-get -s` simulation (client allows 120 s).
const APT_PREVIEW_TIMEOUT: Duration = Duration::from_secs(110);
/// Deadline of `apt-get install` (client allows 900 s).
const APT_INSTALL_TIMEOUT: Duration = Duration::from_secs(890);
/// Deadline of `apt-get remove/purge` (client allows 300 s).
const APT_REMOVE_TIMEOUT: Duration = Duration::from_secs(290);
/// Deadline of `apt-get download` (client allows 600 s).
const APT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(590);
/// Deadline of `apt-get update` (documented 180 s).
const APT_UPDATE_TIMEOUT: Duration = Duration::from_secs(175);
/// Bytes of APT/dpkg output kept per command.
const MAX_APT_OUTPUT: usize = 256 * 1024;
/// Bytes of metadata output kept per `dpkg-deb`/`apt-cache` call.
const MAX_METADATA_OUTPUT: usize = 256 * 1024;
/// Bytes of `dpkg-query` output kept.
const MAX_DPKG_QUERY_OUTPUT: usize = 1024 * 1024;
/// Maximum number of files returned by `package_files`.
const MAX_PACKAGE_FILES: usize = 20_000;
/// Maximum bytes of file paths returned by `package_files` (well under 1 MiB).
const MAX_PACKAGE_FILES_BYTES: usize = 512 * 1024;
/// Maximum number of packages accepted by `package_installed_list`.
const MAX_INSTALLED_QUERY: usize = 64;
/// Maximum number of `.deb` files a `deb_download` may produce.
const MAX_DOWNLOADED_DEBS: usize = 4;

/// Environment forced on every APT invocation.
const APT_ENV: [(&str, &str); 3] = [
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("LC_ALL", "C"),
    ("APT_LISTCHANGES_FRONTEND", "none"),
];

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Run the helper until `shutdown` resolves.
pub async fn serve(options: NativeOptions, shutdown: impl Future<Output = ()>) -> io::Result<()> {
    if options.require_root && !is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the native helper must run as root (start rast-native.service)",
        ));
    }
    let rast_gid = options
        .effective_rast_gid()
        .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error.message))?;
    crate::native::ensure_managed_roots(&options, rast_gid)
        .map_err(|error| io::Error::other(error.message))?;
    // Prepare the immutable final Xpra audio config before accepting requests:
    // concurrent session starts must never read a partially rewritten file.
    super::audio::prepare_config(&options.run_dir)
        .map_err(|error| io::Error::other(error.message))?;
    prepare_socket_path(&options.socket_path)?;
    let listener = UnixListener::bind(&options.socket_path)?;
    apply_socket_permissions(&options.socket_path, rast_gid, options.is_test_mode())?;
    tracing::info!(
        socket = %options.socket_path.display(),
        run_dir = %options.run_dir.display(),
        "native helper listening"
    );

    let options = Arc::new(options);
    let semaphore = Arc::new(Semaphore::new(options.max_concurrent.max(1)));
    let apt_lock = Arc::new(Mutex::new(()));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _address)) => {
                        let options = options.clone();
                        let semaphore = semaphore.clone();
                        let apt_lock = apt_lock.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, options, semaphore, apt_lock).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "native helper accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(&options.socket_path);
    Ok(())
}

/// Create the socket parent directory (if needed) and clear a stale socket.
/// A non-socket file in the socket's place is never removed.
fn prepare_socket_path(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_socket() {
                std::fs::remove_file(path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a socket; refusing to remove it",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Socket mode 0660, owned by root:<group> so only group `rast` can connect.
fn apply_socket_permissions(path: &Path, gid: u32, test_mode: bool) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if !test_mode {
        std::os::unix::fs::chown(path, Some(0), Some(gid))?;
    }
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    options: Arc<NativeOptions>,
    semaphore: Arc<Semaphore>,
    apt_lock: Arc<Mutex<()>>,
) {
    if let Some(required_uid) = options.allowed_peer_uid {
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == required_uid => {}
            Ok(cred) => {
                tracing::warn!(
                    uid = cred.uid(),
                    "rejecting native helper client with unexpected uid"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot read native helper peer credentials");
                return;
            }
        }
    }
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let read = tokio::time::timeout(
            options.idle_timeout,
            read_line_capped(&mut reader, MAX_REQUEST_LINE_BYTES),
        )
        .await;
        let line = match read {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                let response = NativeResponse::failure(
                    None,
                    &NativeError::invalid("request exceeds the protocol size limit"),
                );
                let _ = write_response(&mut write_half, response).await;
                break;
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        let response = match crate::native::protocol::parse_request_line(&line) {
            Ok(request) => process(request, &options, &semaphore, &apt_lock).await,
            Err(error) => unreadable_response(&line, &error),
        };
        if write_response(&mut write_half, response).await.is_err() {
            break;
        }
    }
}

/// Answer a request this helper cannot decode. The id is echoed when readable
/// so the caller sees this error instead of a mismatched reply.
fn unreadable_response(
    line: &[u8],
    error: &crate::native::protocol::RequestParseError,
) -> NativeResponse {
    let id = serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_u64));
    NativeResponse::failure(
        id,
        &NativeError::new(error.code(), error.message().to_string()),
    )
}

async fn write_response(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    response: NativeResponse,
) -> io::Result<()> {
    let mut line = match response.to_line() {
        Ok(line) => line,
        Err(error) => {
            tracing::error!(error = %error, "cannot encode native helper response");
            let fallback =
                NativeResponse::failure(None, &NativeError::protocol("cannot encode the response"));
            fallback.to_line().unwrap_or_default()
        }
    };
    if line.len() + 1 > MAX_RESPONSE_BYTES {
        let too_large = NativeResponse::failure(
            response.id,
            &NativeError::new("response_too_large", "the response exceeds 1 MiB"),
        );
        line = too_large.to_line().unwrap_or_default();
    }
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await
}

/// Read one newline-terminated line with a hard byte cap.
async fn read_line_capped<R>(reader: &mut R, max: usize) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            out.extend_from_slice(&buffer[..position]);
            reader.consume(position + 1);
            if out.len() > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request exceeds the protocol size limit",
                ));
            }
            return Ok(Some(out));
        }
        let consumed = buffer.len();
        out.extend_from_slice(buffer);
        reader.consume(consumed);
        if out.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request exceeds the protocol size limit",
            ));
        }
    }
}

fn log_context(op: &NativeOp) -> (Option<i64>, Option<String>) {
    match op {
        NativeOp::IdentityEnsure(IdentityEnsureOp { user_id, .. }) => (Some(*user_id), None),
        NativeOp::IdentityRemove(IdentityRemoveOp { user_id, .. }) => (Some(*user_id), None),
        NativeOp::SessionStart(SessionStartOp { user_id, .. })
        | NativeOp::SessionStop(SessionStopOp { user_id, .. }) => {
            (Some(*user_id), session_unit(*user_id).ok())
        }
        NativeOp::AppStart(AppStartOp { user_id, unit, .. }) => {
            (Some(*user_id), Some(unit.clone()))
        }
        NativeOp::AppStop(AppStopOp { unit }) | NativeOp::UnitStatus(UnitStatusOp { unit }) => {
            (None, Some(unit.clone()))
        }
        _ => (None, None),
    }
}

async fn process(
    request: NativeRequest,
    options: &NativeOptions,
    semaphore: &Semaphore,
    apt_lock: &Mutex<()>,
) -> NativeResponse {
    let id = request.id;
    let name = request.name();
    let (user_id, unit) = log_context(&request.op);
    let started = std::time::Instant::now();
    let _permit = if request.op.is_long_running() {
        match semaphore.acquire().await {
            Ok(permit) => Some(permit),
            Err(_) => {
                return NativeResponse::failure(
                    Some(id),
                    &NativeError::unavailable("the native helper is shutting down"),
                )
            }
        }
    } else {
        None
    };
    let _apt = if request.op.is_apt_op() {
        Some(apt_lock.lock().await)
    } else {
        None
    };
    let outcome = dispatch(request.op, options).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(data) => {
            tracing::info!(
                op = name,
                user_id = user_id,
                unit = unit.as_deref(),
                duration_ms,
                "native helper request"
            );
            NativeResponse::success(Some(id), data)
        }
        Err(error) => {
            tracing::warn!(
                op = name,
                user_id = user_id,
                unit = unit.as_deref(),
                duration_ms,
                code = error.code,
                message = %crate::native::protocol::truncate_message(&error.message, 300),
                "native helper request failed"
            );
            NativeResponse::failure(Some(id), &error)
        }
    }
}

async fn dispatch(op: NativeOp, options: &NativeOptions) -> Result<Value, NativeError> {
    match op {
        NativeOp::Ping => Ok(ping(options)),
        NativeOp::IdentityEnsure(op) => identity_ensure(options, op).await,
        NativeOp::IdentityRemove(op) => identity_remove(options, op).await,
        NativeOp::SessionStart(op) => session_start(options, op).await,
        NativeOp::SessionStop(op) => session_stop(options, op).await,
        NativeOp::UnitStatus(op) => unit_status(options, op).await,
        NativeOp::UnitsList => units_list(options).await,
        NativeOp::AppStart(op) => app_start(options, op).await,
        NativeOp::AppStop(op) => app_stop(options, op).await,
        NativeOp::DebPreview(op) => deb_preview(options, op).await,
        NativeOp::DebInstall(op) => deb_install(options, op).await,
        NativeOp::DebRemove(op) => deb_remove(options, op).await,
        NativeOp::DebDownload(op) => deb_download(options, op).await,
        NativeOp::PackagePolicy(op) => package_policy(options, op).await,
        NativeOp::PackageInstalledList(op) => package_installed_list(options, op).await,
        NativeOp::PackageFiles(op) => package_files(options, op).await,
        NativeOp::PathMkdir(op) => path_mkdir(options, op).await,
        NativeOp::PathChown(op) => path_chown(options, op).await,
        NativeOp::PathRemove(op) => path_remove(options, op).await,
        NativeOp::PathChmod(op) => path_chmod(options, op).await,
        NativeOp::PathAcl(op) => path_acl(options, op).await,
        NativeOp::PathSymlink(op) => path_symlink(options, op).await,
        NativeOp::ProfilePrepare(op) => profile_prepare(options, op).await,
        NativeOp::ProfileCleanLocks(op) => profile_clean_locks(options, op).await,
        NativeOp::ProfileInstall(op) => profile_install(options, op).await,
        NativeOp::ProfileRemove(op) => profile_remove(options, op).await,
        NativeOp::AptUpdate => apt_update(options).await,
    }
}

// ---------------------------------------------------------------------------
// Simple probes and identity
// ---------------------------------------------------------------------------

fn ping(options: &NativeOptions) -> Value {
    let xpra = std::fs::metadata(&options.xpra_bin)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    json!({
        "version": PROTOCOL_VERSION,
        "root": is_root() || options.is_test_mode(),
        "xpra": xpra,
        "dockerFallback": false,
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn identity_ensure(
    options: &NativeOptions,
    op: IdentityEnsureOp,
) -> Result<Value, NativeError> {
    if let Some(username) = &op.username {
        let expected = crate::native::protocol::username_for(op.user_id);
        if *username != expected {
            return Err(NativeError::invalid(format!(
                "username must be {expected:?}"
            )));
        }
    }
    let (identity, created) = identity::ensure_identity(options, op.user_id).await?;
    Ok(json!({
        "userId": identity.user_id,
        "username": identity.username,
        "uid": identity.uid,
        "gid": identity.gid,
        "home": path_string(&identity.home),
        "created": created,
    }))
}

async fn identity_remove(
    options: &NativeOptions,
    op: IdentityRemoveOp,
) -> Result<Value, NativeError> {
    identity::remove_identity(options, op.user_id, op.remove_home).await?;
    Ok(json!({"removed": true}))
}

async fn require_identity(options: &NativeOptions, user_id: i64) -> Result<Identity, NativeError> {
    let identity = Identity::derive(options, user_id)?;
    let passwd = identity::getent_passwd(options, &identity.username).await?;
    let Some(entry) = passwd else {
        return Err(NativeError::new(
            "identity_missing",
            format!("no Linux identity for user {user_id} yet; call identity_ensure first"),
        ));
    };
    identity::check_existing_identity(&identity, Some(&entry), None)?;
    Ok(identity)
}

// ---------------------------------------------------------------------------
// Sessions and applications
// ---------------------------------------------------------------------------

fn effective_session_env(
    options: &NativeOptions,
    identity: &Identity,
    requested: &BTreeMap<String, String>,
    runtime_dir: &Path,
) -> BTreeMap<String, String> {
    let mut env = requested.clone();
    let home = path_string(&identity.home);
    env.insert("HOME".to_string(), home.clone());
    env.insert(
        "XAUTHORITY".to_string(),
        path_string(&identity.home.join(".Xauthority")),
    );
    env.insert("XDG_RUNTIME_DIR".to_string(), path_string(runtime_dir));
    env.extend(super::audio::environment(runtime_dir));
    env.insert("XPRA_PRIVATE_PULSEAUDIO".to_string(), "0".to_string());
    env.insert(
        "XPRA_USER_CONF_DIRS".to_string(),
        path_string(&super::audio::config_dir(&options.run_dir)),
    );
    env.insert("USER".to_string(), identity.username.clone());
    env.insert("LOGNAME".to_string(), identity.username.clone());
    env.insert("RAST_USER_ID".to_string(), identity.user_id.to_string());
    let _ = options;
    env
}

fn effective_app_env(
    options: &NativeOptions,
    identity: &Identity,
    requested: &BTreeMap<String, String>,
    runtime_dir: &Path,
    session_auth: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = requested.clone();
    // Caller-provided HOME / XDG_* values win: native apps run with a per-app
    // profile home. XAUTHORITY always points at the session's auth file.
    env.entry("HOME".to_string())
        .or_insert_with(|| path_string(&identity.home));
    env.entry("XDG_RUNTIME_DIR".to_string())
        .or_insert_with(|| path_string(runtime_dir));
    // The session runs Xpra with `XAUTHORITY=<identity home>/.Xauthority`
    // (pinned by `effective_session_env`), so Xvfb and every application must
    // use that same file. Prefer a caller-provided path when it really exists
    // and lives in the identity's runtime or home; never guess from the
    // runtime directory, where `xpra/Xauthority-<old display>` files from
    // earlier sessions may linger and point at a different cookie.
    // The Xvfb instance of the session may use either `$HOME/.Xauthority` or
    // `$XDG_RUNTIME_DIR/xpra/Xauthority-<display>` depending on whether Xpra
    // could write the home file. Discover the file from the running Xvfb
    // command line (root can see it); only fall back to a caller-provided or
    // home path when discovery fails.
    let provided = requested
        .get("XAUTHORITY")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .filter(|path| path.starts_with(runtime_dir) || path.starts_with(&identity.home));
    let auth = session_auth
        .map(Path::to_path_buf)
        .filter(|path| path.is_file())
        .or(provided)
        .unwrap_or_else(|| identity.home.join(".Xauthority"));
    env.insert("XAUTHORITY".to_string(), path_string(&auth));
    env.extend(super::audio::environment(runtime_dir));
    env.insert("USER".to_string(), identity.username.clone());
    env.insert("LOGNAME".to_string(), identity.username.clone());
    env.insert("RAST_USER_ID".to_string(), identity.user_id.to_string());
    let _ = options;
    env
}

/// The X11 auth file the person's running Xvfb uses, discovered from the
/// session unit's process tree (the file path depends on the Xpra version and
/// on whether the identity home was writable when the session started).
fn discover_session_xauthority(options: &NativeOptions, user_id: i64) -> Option<PathBuf> {
    let unit = format!("rast-xpra-u{user_id}.service");
    let output = std::process::Command::new(&options.systemctl_bin)
        .args(["show", "--value", "--property", "MainPID", &unit])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let main_pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if main_pid == 0 {
        return None;
    }
    // Walk the unit's process tree (bounded) and find the Xvfb child, then read
    // the file after `-auth` from its command line.
    let mut queue = std::collections::VecDeque::from([main_pid]);
    let mut visited = 0usize;
    while let Some(pid) = queue.pop_front() {
        visited += 1;
        if visited > 256 {
            break;
        }
        if let Some(auth) = xvfb_auth_argument(pid) {
            return Some(auth);
        }
        if let Ok(children) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in children.flatten() {
                let path = task.path().join("children");
                if let Ok(text) = std::fs::read_to_string(path) {
                    for child in text.split_whitespace() {
                        if let Ok(child) = child.parse::<u32>() {
                            queue.push_back(child);
                        }
                    }
                }
            }
        }
    }
    None
}

/// The `-auth <file>` argument of this process when it is an Xvfb server.
fn xvfb_auth_argument(pid: u32) -> Option<PathBuf> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let program = args.first()?;
    if !program.contains("Xvfb") {
        return None;
    }
    let index = args.iter().position(|arg| arg == "-auth")?;
    let file = PathBuf::from(args.get(index + 1)?);
    file.is_file().then_some(file)
}

/// Ensure the user runtime directory and (optionally) a deeper directory under
/// it exist with 0700 uid:gid, and validate the requested path.
fn prepare_runtime_dir(
    options: &NativeOptions,
    identity: &Identity,
    requested: Option<&str>,
) -> Result<PathBuf, NativeError> {
    let base = identity::ensure_runtime_dir(options, identity)?;
    let target = match requested {
        Some(raw) => {
            let ctx = options.validation_context();
            validate_runtime_dir(&ctx, raw)?
        }
        None => base.clone(),
    };
    if !target.starts_with(&base) {
        return Err(NativeError::forbidden(
            "the session runtime path must belong to this native identity",
        ));
    }
    std::fs::create_dir_all(&target).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot create runtime directory {}: {error}",
            target.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&target).map_err(|error| {
        NativeError::unavailable(format!("cannot inspect {}: {error}", target.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(
            "the session runtime path must be a real directory",
        ));
    }
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        NativeError::command_failed(format!("cannot chmod {}: {error}", target.display()))
    })?;
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&target, Some(identity.uid), Some(identity.gid)).map_err(
            |error| {
                NativeError::command_failed(format!("cannot chown {}: {error}", target.display()))
            },
        )?;
    }
    Ok(target)
}

/// Make every ancestor directory between `path` and the RAST roots traversable
/// by the unprivileged identities (add the "others execute" bit, nothing
/// else).
///
/// The per-user profile/app directories are 0700 and owned by the identity
/// (correct), but a path such as `<data_dir>/linux-apps/profiles/<app>/<user>`
/// also needs `linux-apps`, `profiles` and `profiles/<app>` to be *searchable*
/// by the identity. Without this, systemd fails with `200/CHDIR: Changing to
/// the requested working directory failed: Permission denied`. The helper
/// never changes ownership, read/write bits or the data directory itself; a
/// non-traversable data_dir is reported by `rast doctor` / `--check`.
fn ensure_identity_traversal(options: &NativeOptions, path: &Path) -> Result<(), NativeError> {
    use std::os::unix::fs::PermissionsExt;
    if options.is_test_mode() {
        // Tests run as an ordinary user; the fixture tree is already theirs.
        // Still exercise the logic (chmod on own files succeeds).
    }
    let mut current = path.to_path_buf();
    loop {
        if current == options.data_dir || current == options.run_dir {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
        if current == options.data_dir || current == options.run_dir {
            break;
        }
        if !current.starts_with(&options.data_dir) && !current.starts_with(&options.run_dir) {
            break;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    break;
                }
                let mode = metadata.permissions().mode();
                if mode & 0o001 == 0 {
                    std::fs::set_permissions(
                        &current,
                        std::fs::Permissions::from_mode(mode | 0o001),
                    )
                    .map_err(|error| {
                        NativeError::command_failed(format!(
                            "cannot make {} traversable for native apps: {error}",
                            current.display()
                        ))
                    })?;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Whether `metadata` grants the execute (search) permission to `uid`, using
/// the identity's single primary group `gid` (RAST identities have no
/// supplementary groups).
fn directory_traversable_by(metadata: &std::fs::Metadata, uid: u32, gid: u32) -> bool {
    let mode = metadata.permissions().mode();
    if metadata.uid() == uid {
        mode & 0o100 != 0
    } else if metadata.gid() == gid {
        mode & 0o010 != 0
    } else {
        mode & 0o001 != 0
    }
}

/// Validate that the identity can traverse (chdir through) `cwd` before any
/// transient unit is started. Every directory component — including
/// `<data_dir>/linux-apps`, `profiles/<appId>` and the profile dir itself —
/// must grant execute to the identity. Returns `cwd_inaccessible` naming the
/// exact offending component instead of letting systemd fail with 200/CHDIR.
fn validate_cwd_traversable(
    options: &NativeOptions,
    cwd: &Path,
    identity: &Identity,
) -> Result<(), NativeError> {
    // Tests run as the fixture owner, not as the future identity; check the
    // permission logic against the current euid/egid in that case so the
    // diagnostics are still exercised.
    let (check_uid, check_gid) = if options.is_test_mode() {
        // SAFETY: geteuid/getegid have no preconditions.
        unsafe { (libc::geteuid(), libc::getegid()) }
    } else {
        (identity.uid, identity.gid)
    };
    let mut current = cwd.to_path_buf();
    loop {
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            NativeError::cwd_inaccessible(format!(
                "working directory {} cannot be inspected: {error}",
                current.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(NativeError::cwd_inaccessible(format!(
                "working directory component {} is not a real directory",
                current.display()
            )));
        }
        if !directory_traversable_by(&metadata, check_uid, check_gid) {
            return Err(NativeError::cwd_inaccessible(format!(
                "directory component {} (mode {:04o} owner {}:{}) is not traversable by the \
                 native identity rast-u{} (uid {}); every component of the profile/cwd path needs \
                 the execute bit for it — the per-user profile directory must be owned by the \
                 identity and the shared parents need path_chmod/path_acl",
                current.display(),
                metadata.permissions().mode() & 0o7777,
                metadata.uid(),
                metadata.gid(),
                identity.user_id,
                identity.uid
            )));
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                current = parent.to_path_buf();
            }
            _ => break,
        }
    }
    Ok(())
}

/// Validate the session auth file an adopted session would use.
///
/// With `--auth=none` Xpra may not create `$HOME/.Xauthority` at all, so a
/// missing file is allowed. When it does exist it must be a regular file the
/// identity can read; otherwise apps started against that display fail with
/// confusing "Authorization required"/"Missing X server" errors. Return a
/// stable, actionable `xauthority_unavailable` instead.
fn validate_session_xauthority(
    options: &NativeOptions,
    identity: &Identity,
) -> Result<(), NativeError> {
    let path = identity.home.join(".Xauthority");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(NativeError::xauthority_unavailable(format!(
                "cannot inspect the session auth file {}: {error}",
                path.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(NativeError::xauthority_unavailable(format!(
            "the session auth file {} is not a regular file; stop and restart the user's \
             native session",
            path.display()
        )));
    }
    let (check_uid, check_gid) = if options.is_test_mode() {
        // SAFETY: geteuid/getegid have no preconditions.
        unsafe { (libc::geteuid(), libc::getegid()) }
    } else {
        (identity.uid, identity.gid)
    };
    let mode = metadata.permissions().mode();
    let readable = if metadata.uid() == check_uid {
        mode & 0o400 != 0
    } else if metadata.gid() == check_gid {
        mode & 0o040 != 0
    } else {
        mode & 0o004 != 0
    };
    if !readable {
        return Err(NativeError::xauthority_unavailable(format!(
            "the session auth file {} (mode {:04o} owner {}:{}) is not readable by the native \
             identity rast-u{} (uid {}); the home directory was probably repaired after the \
             session started — stop the session and start it again so Xpra regenerates the auth \
             file",
            path.display(),
            mode & 0o7777,
            metadata.uid(),
            metadata.gid(),
            identity.user_id,
            identity.uid
        )));
    }
    Ok(())
}

/// Reject admission when the host cannot spare the requested memory plus the
/// configured reserve. Missing /proc information is not fatal.
fn check_memory_admission(
    options: &NativeOptions,
    requested_mb: u32,
    what: &str,
) -> Result<(), NativeError> {
    if options.is_test_mode() {
        return Ok(());
    }
    let Some(available) = memory_available_mb() else {
        return Ok(());
    };
    let needed = u64::from(requested_mb) + u64::from(options.admission_reserve_mb);
    if available < needed {
        return Err(NativeError::insufficient_memory(format!(
            "not enough free memory to start the {what}: {available} MiB available, \
             {requested_mb} MiB requested plus a {} MiB reserve",
            options.admission_reserve_mb
        )));
    }
    Ok(())
}

fn memory_available_mb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kib / 1024)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Session,
    App,
}

fn build_systemd_run_args(
    kind: UnitKind,
    unit: &str,
    identity: &Identity,
    limits: ResourceLimits,
    workdir: &Path,
    env: &BTreeMap<String, String>,
    argv: &[String],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from(format!("--unit={unit}")),
        OsString::from(format!("--slice={LINUX_SLICE}")),
        OsString::from(format!("--uid={}", identity.uid)),
        OsString::from(format!("--gid={}", identity.gid)),
        OsString::from("--collect"),
        OsString::from("--quiet"),
        OsString::from("--property=Type=exec"),
        OsString::from(format!("--property=MemoryMax={}M", limits.memory_max_mb)),
        OsString::from(format!("--property=CPUQuota={}%", limits.cpu_quota_percent)),
        OsString::from(format!("--property=TasksMax={}", limits.tasks_max)),
        OsString::from("--property=KillMode=control-group"),
        OsString::from(format!(
            "--property=WorkingDirectory={}",
            path_string(workdir)
        )),
    ];
    if kind == UnitKind::Session {
        args.push(OsString::from("--property=Restart=no"));
    }
    // BTreeMap keeps the command line deterministic (and easy to test).
    for (key, value) in env {
        args.push(OsString::from(format!("--setenv={key}={value}")));
    }
    args.push(OsString::from("--"));
    args.extend(argv.iter().map(OsString::from));
    args
}

async fn run_systemd_unit(
    options: &NativeOptions,
    unit: &str,
    args: Vec<OsString>,
) -> Result<(), NativeError> {
    let output = run_command(
        &options.systemd_run_bin,
        args,
        &[],
        SYSTEMD_RUN_TIMEOUT,
        MAX_APT_OUTPUT,
        MAX_APT_OUTPUT,
    )
    .await?;
    if output.success {
        Ok(())
    } else {
        Err(NativeError::command_failed(format!(
            "cannot start {unit}: {}",
            summarize_command(&output)
        )))
    }
}

/// State of one managed unit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitInfo {
    unit: String,
    active: bool,
    state: String,
    main_pid: Option<u32>,
}

impl UnitInfo {
    fn inactive(unit: &str) -> Self {
        Self {
            unit: unit.to_string(),
            active: false,
            state: "inactive".to_string(),
            main_pid: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawUnit {
    id: Option<String>,
    active_state: Option<String>,
    load_state: Option<String>,
    main_pid: Option<u32>,
}

impl RawUnit {
    fn to_info(&self, fallback_unit: &str) -> UnitInfo {
        let unit = self.id.clone().unwrap_or_else(|| fallback_unit.to_string());
        let active_state = self.active_state.as_deref().unwrap_or("inactive");
        let load_state = self.load_state.as_deref().unwrap_or("loaded");
        let active = active_state == "active";
        let state = if active_state == "failed" || load_state == "failed" || load_state == "error" {
            "failed"
        } else if active {
            "active"
        } else {
            "inactive"
        };
        UnitInfo {
            unit,
            active,
            state: state.to_string(),
            main_pid: self.main_pid.filter(|pid| *pid > 0),
        }
    }
}

fn parse_systemctl_show(text: &str) -> Vec<RawUnit> {
    let mut units = Vec::new();
    let mut current = RawUnit::default();
    let mut any = false;
    let flush = |current: &mut RawUnit, units: &mut Vec<RawUnit>, any: &mut bool| {
        if *any {
            units.push(std::mem::take(current));
            *any = false;
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut current, &mut units, &mut any);
            continue;
        }
        if let Some(value) = line.strip_prefix("Id=") {
            if any && current.id.is_some() {
                flush(&mut current, &mut units, &mut any);
            }
            current.id = Some(value.trim_end_matches(".service").to_string());
            any = true;
        } else if let Some(value) = line.strip_prefix("ActiveState=") {
            current.active_state = Some(value.to_string());
            any = true;
        } else if let Some(value) = line.strip_prefix("LoadState=") {
            current.load_state = Some(value.to_string());
            any = true;
        } else if let Some(value) = line.strip_prefix("MainPID=") {
            current.main_pid = value.parse::<u32>().ok();
            any = true;
        }
    }
    flush(&mut current, &mut units, &mut any);
    units
}

async fn systemctl_show(
    options: &NativeOptions,
    selectors: &[&str],
) -> Result<Vec<RawUnit>, NativeError> {
    let mut args: Vec<OsString> = vec![OsString::from("show")];
    args.extend(selectors.iter().map(OsString::from));
    args.extend([
        OsString::from("--property=Id"),
        OsString::from("--property=ActiveState"),
        OsString::from("--property=LoadState"),
        OsString::from("--property=MainPID"),
        OsString::from("--no-pager"),
    ]);
    let output = run_command(
        &options.systemctl_bin,
        args,
        &[],
        SYSTEMCTL_TIMEOUT,
        MAX_APT_OUTPUT,
        MAX_APT_OUTPUT,
    )
    .await?;
    if !output.success {
        let summary = summarize_command(&output).to_ascii_lowercase();
        if summary.contains("not found")
            || summary.contains("not loaded")
            || summary.contains("no units")
        {
            return Ok(Vec::new());
        }
        return Err(NativeError::command_failed(format!(
            "cannot query systemd units: {}",
            summarize_command(&output)
        )));
    }
    Ok(parse_systemctl_show(&output.stdout))
}

async fn unit_state(options: &NativeOptions, unit: &str) -> Result<UnitInfo, NativeError> {
    let selector = format!("{unit}.service");
    let units = systemctl_show(options, &[selector.as_str()]).await?;
    Ok(units
        .first()
        .map(|raw| raw.to_info(unit))
        .unwrap_or_else(|| UnitInfo::inactive(unit)))
}

async fn reset_failed(options: &NativeOptions, unit: &str) {
    let _ = run_command(
        &options.systemctl_bin,
        vec![OsString::from("reset-failed"), OsString::from(unit)],
        &[],
        SYSTEMCTL_TIMEOUT,
        64 * 1024,
        64 * 1024,
    )
    .await;
}

async fn stop_unit(options: &NativeOptions, unit: &str) -> Result<(), NativeError> {
    let output = run_command(
        &options.systemctl_bin,
        vec![OsString::from("stop"), OsString::from(unit)],
        &[],
        SYSTEMCTL_TIMEOUT,
        64 * 1024,
        64 * 1024,
    )
    .await?;
    if output.success {
        reset_failed(options, unit).await;
        return Ok(());
    }
    let summary = summarize_command(&output);
    let lower = summary.to_ascii_lowercase();
    if lower.contains("not loaded")
        || lower.contains("not found")
        || lower.contains("does not exist")
    {
        return Ok(());
    }
    Err(NativeError::command_failed(format!(
        "cannot stop {unit}: {summary}"
    )))
}

async fn session_start(options: &NativeOptions, op: SessionStartOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_display(op.display)?;
    validate_port(op.port)?;
    validate_session_args(&op.args)?;
    validate_env(&op.env)?;
    let limits = ResourceLimits {
        memory_max_mb: op.memory_max_mb,
        cpu_quota_percent: op.cpu_quota_percent,
        tasks_max: op.tasks_max,
    };
    validate_limits(limits, ctx.session_limits, "session")?;
    validate_xpra_bin(&ctx, &op.xpra_bin)?;
    let identity = require_identity(options, op.user_id).await?;
    // The session's HOME is the identity home: xpra writes its X11 auth file
    // there and child apps read it. Repair the ownership/mode whenever a
    // session starts so a home chowned by an operator script (or an older
    // layout) heals before Xvfb starts.
    crate::native::identity::ensure_home(options, &identity).await?;
    let runtime_dir = prepare_runtime_dir(options, &identity, Some(&op.runtime_dir))?;
    // PulseAudio's socket/cookie/state must be writable by this identity, not
    // root or the web service. Reuse the checked 0700 directory provisioning;
    // never chown or attach to a host /run/user/<uid>/pulse directory.
    prepare_runtime_dir(
        options,
        &identity,
        Some(&path_string(&runtime_dir.join("pulse"))),
    )?;
    ensure_identity_traversal(options, &runtime_dir)?;
    check_memory_admission(options, limits.memory_max_mb, "graphical session")?;

    let unit = session_unit(op.user_id)?;
    let current = unit_state(options, &unit).await?;
    if current.active {
        // Idempotent: a session that is already running is left untouched.
        return Ok(json!({"unit": unit, "active": true}));
    }
    if current.state == "failed" {
        // A previous attempt left a failed transient unit behind; clear it so
        // the same unit name can be started again.
        stop_unit(options, &unit).await?;
    }
    reset_failed(options, &unit).await;
    let env = effective_session_env(options, &identity, &op.env, &runtime_dir);
    let mut argv = Vec::with_capacity(op.args.len() + 1);
    argv.push(path_string(&options.xpra_bin));
    argv.extend(op.args.iter().cloned());
    let args = build_systemd_run_args(
        UnitKind::Session,
        &unit,
        &identity,
        limits,
        &runtime_dir,
        &env,
        &argv,
    );
    run_systemd_unit(options, &unit, args).await?;
    Ok(json!({"unit": unit, "active": true}))
}

async fn session_stop(options: &NativeOptions, op: SessionStopOp) -> Result<Value, NativeError> {
    let unit = session_unit(op.user_id)?;
    stop_unit(options, &unit).await?;
    Ok(json!({"stopped": true}))
}

async fn unit_status(options: &NativeOptions, op: UnitStatusOp) -> Result<Value, NativeError> {
    validate_managed_unit(&op.unit)?;
    let unit = op.unit;
    let info = unit_state(options, &unit).await?;
    Ok(json!({
        "unit": info.unit,
        "active": info.active,
        "state": info.state,
        "mainPid": info.main_pid,
    }))
}

/// Does this unit's main process run Xpra with playback audio enabled?
/// `None` when the command line cannot be read.
fn unit_plays_audio(main_pid: Option<u32>) -> Option<bool> {
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", main_pid?)).ok()?;
    let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    Some(args.contains(&&b"--audio=yes"[..]) && args.contains(&&b"--speaker=on"[..]))
}

async fn units_list(options: &NativeOptions) -> Result<Value, NativeError> {
    let units = systemctl_show(options, &["rast-xpra-*.service", "rast-app-*.service"]).await?;
    let mut items: Vec<Value> = Vec::new();
    for raw in units {
        let Some(id) = raw.id.as_deref() else {
            continue;
        };
        if !is_managed_unit(id) || validate_unit(id).is_err() {
            continue;
        }
        let info = raw.to_info(id);
        items.push(json!({
            "unit": info.unit,
            "active": info.active,
            "mainPid": info.main_pid,
            // Root can read another identity's /proc entry; the server runs
            // with ProtectProc=invisible and cannot. Used to recognise Xpra
            // sessions started before playback audio existed.
            "audio": unit_plays_audio(info.main_pid),
        }));
    }
    items.sort_by(|left, right| left["unit"].as_str().cmp(&right["unit"].as_str()));
    Ok(json!({"units": items}))
}

async fn app_start(options: &NativeOptions, op: AppStartOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_app_id(&op.app_id)?;
    validate_run_id(&op.run_id)?;
    validate_app_unit(&op.unit)?;
    validate_argv(&op.argv)?;
    validate_argv0(&ctx, &op.argv[0])?;
    let cwd = validate_cwd(&ctx, &op.cwd)?;
    validate_env(&op.env)?;
    let limits = ResourceLimits {
        memory_max_mb: op.memory_max_mb,
        cpu_quota_percent: op.cpu_quota_percent,
        tasks_max: op.tasks_max,
    };
    validate_limits(limits, ctx.app_limits, "application")?;
    let identity = require_identity(options, op.user_id).await?;
    identity::ensure_home(options, &identity).await?;
    validate_session_xauthority(options, &identity)?;
    let runtime_dir = prepare_runtime_dir(options, &identity, None)?;
    ensure_identity_traversal(options, &runtime_dir)?;
    // The app identity must be able to chdir to `cwd` and exec `argv[0]`.
    ensure_identity_traversal(options, &cwd)?;
    ensure_identity_traversal(options, Path::new(&op.argv[0]))?;
    validate_cwd_traversable(options, &cwd, &identity)?;
    check_memory_admission(options, limits.memory_max_mb, "application")?;

    let current = unit_state(options, &op.unit).await?;
    if current.active {
        return Err(NativeError::unit_conflict(format!(
            "unit {} is already active; use a new run id",
            op.unit
        )));
    }
    if current.state == "failed" {
        stop_unit(options, &op.unit).await?;
    }
    reset_failed(options, &op.unit).await;
    let session_auth = discover_session_xauthority(options, op.user_id);
    let env = effective_app_env(
        options,
        &identity,
        &op.env,
        &runtime_dir,
        session_auth.as_deref(),
    );
    let args = build_systemd_run_args(
        UnitKind::App,
        &op.unit,
        &identity,
        limits,
        &cwd,
        &env,
        &op.argv,
    );
    if let Err(error) = run_systemd_unit(options, &op.unit, args).await {
        // systemd reports a non-traversable/absent working directory as
        // `status=200/CHDIR`; surface the actionable helper error instead.
        if error.message.contains("200/CHDIR")
            || error.message.contains("requested working directory")
        {
            return Err(NativeError::cwd_inaccessible(format!(
                "systemd could not chdir to {} for application {}: {} — the working directory \
                 and its parents must be traversable by rast-u{} (uid {}); recreate the profile \
                 with profile_prepare or fix the directory ownership",
                cwd.display(),
                op.app_id,
                op.user_id,
                identity.uid,
                error.message
            )));
        }
        return Err(error);
    }
    Ok(json!({"unit": op.unit, "active": true}))
}

async fn app_stop(options: &NativeOptions, op: AppStopOp) -> Result<Value, NativeError> {
    validate_app_unit(&op.unit)?;
    stop_unit(options, &op.unit).await?;
    Ok(json!({"stopped": true}))
}

// ---------------------------------------------------------------------------
// APT / package operations
// ---------------------------------------------------------------------------

/// Streaming SHA-256 of a file (never loads it into memory).
fn sha256_file_blocking(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn verify_checksum(path: &Path, expected: &str) -> Result<(), NativeError> {
    let path = path.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || sha256_file_blocking(&path))
        .await
        .map_err(|error| NativeError::command_failed(format!("cannot hash package: {error}")))?
        .map_err(|error| {
            NativeError::invalid_deb(format!("cannot read the package file: {error}"))
        })?;
    if actual != expected {
        return Err(NativeError::checksum_mismatch(
            "the package file changed after upload (SHA-256 mismatch); upload it again",
        ));
    }
    Ok(())
}

fn parse_control_file(text: &str) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            if let Some(name) = &current {
                if let Some(value) = fields.get_mut(name) {
                    value.push('\n');
                    value.push_str(rest);
                }
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some(name.trim().to_string());
            fields.insert(name.trim().to_string(), value.trim().to_string());
        }
    }
    fields
}

fn split_dpkg_field(value: Option<&String>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn deb_control(
    options: &NativeOptions,
    path: &Path,
) -> Result<BTreeMap<String, String>, NativeError> {
    let output = run_command(
        &options.dpkg_deb_bin,
        vec![OsString::from("--field"), path.as_os_str().to_os_string()],
        &[],
        QUICK_CMD_TIMEOUT,
        MAX_METADATA_OUTPUT,
        MAX_METADATA_OUTPUT,
    )
    .await?;
    if !output.success {
        return Err(NativeError::invalid_deb(format!(
            "cannot read the package metadata: {}",
            summarize_command(&output)
        )));
    }
    let control = parse_control_file(&output.stdout);
    if !control.contains_key("Package") {
        return Err(NativeError::invalid_deb(
            "the file has no Package control field; is it a valid .deb?",
        ));
    }
    Ok(control)
}

fn native_dpkg_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armhf",
        "x86" => "i386",
        "powerpc64" => "ppc64el",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        other => other,
    }
}

/// One `apt-get -s` transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AptSimulation {
    installs: Vec<(String, String)>,
    removals: Vec<String>,
    upgrades: Vec<String>,
    download_bytes: Option<u64>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AptSection {
    New,
    Remove,
    Upgrade,
}

fn classify_apt_header(line: &str) -> Option<AptSection> {
    if line.starts_with("The following NEW packages will be installed") {
        Some(AptSection::New)
    } else if line.starts_with("The following packages will be REMOVED") {
        Some(AptSection::Remove)
    } else if line.starts_with("The following packages will be upgraded") {
        Some(AptSection::Upgrade)
    } else {
        None
    }
}

fn parse_inst_line(rest: &str) -> Option<(String, String)> {
    let mut tokens = rest.split_whitespace();
    let name = tokens.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    let mut version = String::new();
    for token in tokens {
        if let Some(value) = token.strip_prefix('(') {
            version = value.trim_end_matches(')').to_string();
            break;
        }
    }
    if version.is_empty() {
        version = "unknown".to_string();
    }
    Some((name, version))
}

/// Parse the output of an `apt-get -s` simulation.
fn parse_apt_simulation(text: &str) -> AptSimulation {
    let mut sim = AptSimulation::default();
    let mut section: Option<AptSection> = None;
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            section = None;
            continue;
        }
        let indented = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        if indented {
            if let Some(section) = section {
                for token in trimmed.split_whitespace() {
                    if token.is_empty() {
                        continue;
                    }
                    match section {
                        AptSection::New => push_unique_pair(&mut sim.installs, token),
                        AptSection::Remove => push_unique(&mut sim.removals, token),
                        AptSection::Upgrade => push_unique(&mut sim.upgrades, token),
                    }
                }
            }
            continue;
        }
        if let Some(header) = classify_apt_header(trimmed) {
            section = Some(header);
            continue;
        }
        section = None;
        if let Some(rest) = trimmed.strip_prefix("Inst ") {
            if let Some(pair) = parse_inst_line(rest) {
                push_unique_pair(&mut sim.installs, &pair.0);
                if !pair.1.is_empty() {
                    sim.installs.retain(|(name, _)| name != &pair.0);
                    sim.installs.push(pair);
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("Remv ") {
            if let Some(name) = rest.split_whitespace().next() {
                push_unique(&mut sim.removals, name);
            }
        } else if trimmed.starts_with("Need to get ") && sim.download_bytes.is_none() {
            sim.download_bytes = parse_apt_size(trimmed);
        }
        if trimmed.starts_with("WARNING:") || trimmed.starts_with("NOTE:") {
            sim.warnings
                .push(crate::native::protocol::truncate_message(trimmed, 300));
        }
    }
    sim
}

fn push_unique(items: &mut Vec<String>, value: &str) {
    if !items.iter().any(|existing| existing == value) {
        items.push(value.to_string());
    }
}

fn push_unique_pair(items: &mut Vec<(String, String)>, name: &str) {
    if !items.iter().any(|(existing, _)| existing == name) {
        items.push((name.to_string(), "unknown".to_string()));
    }
}

/// `Need to get 3,166 kB of archives.` -> bytes.
fn parse_apt_size(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("Need to get ")?;
    let rest = rest.split(" of archives").next()?;
    let cleaned = rest.replace(',', "");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let window = tokens.windows(2).next()?;
    let value = window[0].parse::<f64>().ok()?;
    let multiplier = match window[1] {
        "B" => 1f64,
        "kB" => 1024f64,
        "MB" => 1024f64 * 1024f64,
        "GB" => 1024f64 * 1024f64 * 1024f64,
        "TB" => 1024f64 * 1024f64 * 1024f64 * 1024f64,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

fn apt_common_args() -> Vec<OsString> {
    vec![OsString::from("-o"), OsString::from("Dpkg::Use-Pty=0")]
}

async fn apt_get(
    options: &NativeOptions,
    args: Vec<OsString>,
    timeout: Duration,
) -> Result<CommandOutput, NativeError> {
    run_command(
        &options.apt_get_bin,
        args,
        &APT_ENV,
        timeout,
        MAX_APT_OUTPUT,
        MAX_APT_OUTPUT,
    )
    .await
}

fn last_output_words(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    last_lines(&combined, MAX_OUTPUT_BYTES)
}

fn parse_setup_packages(output: &str) -> Vec<String> {
    let mut packages = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Setting up ") {
            if let Some(name) = rest.split_whitespace().next() {
                push_unique(&mut packages, name);
            }
        }
    }
    packages
}

fn apt_sources_snapshot() -> BTreeSet<PathBuf> {
    let mut entries = BTreeSet::new();
    if let Ok(dir) = std::fs::read_dir("/etc/apt/sources.list.d") {
        for entry in dir.flatten() {
            let path = entry.path();
            if path
                .extension()
                .map(|ext| ext == "list" || ext == "sources")
                == Some(true)
            {
                entries.insert(path);
            }
        }
    }
    entries
}

/// Outcome of `apt-cache policy`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PolicyInfo {
    installed: Option<String>,
    candidate: Option<String>,
    source: Option<String>,
    repository: Option<String>,
    architecture: Option<String>,
}

fn parse_apt_cache_policy(text: &str) -> PolicyInfo {
    let mut info = PolicyInfo::default();
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Installed:") {
            let value = value.trim();
            if value != "(none)" && !value.is_empty() {
                info.installed = Some(value.to_string());
            } else {
                info.installed = None;
            }
        } else if let Some(value) = trimmed.strip_prefix("Candidate:") {
            let value = value.trim();
            if value != "(none)" && !value.is_empty() {
                info.candidate = Some(value.to_string());
            }
        }
    }
    // Find the candidate version block and the repository line below it.
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("*** ") {
            continue;
        }
        for follow in lines.iter().skip(index + 1) {
            let follow = follow.trim();
            if follow.is_empty() || follow.starts_with("*** ") {
                break;
            }
            let tokens: Vec<&str> = follow.split_whitespace().collect();
            if tokens.len() < 2 || !tokens[0].bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let url = tokens[1];
            let suite = tokens.get(2).copied().unwrap_or("");
            if url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("file:")
                || url.starts_with("cdrom:")
                || url.starts_with("mirror:")
            {
                info.source = Some(format!("{url} {suite}").trim().to_string());
                let host = url
                    .split("://")
                    .nth(1)
                    .unwrap_or(url)
                    .split('/')
                    .next()
                    .unwrap_or(url);
                info.repository = Some(format!("{host} {suite}").trim().to_string());
                if let Some(position) = tokens.iter().position(|token| *token == "Packages") {
                    if let Some(arch) = position.checked_sub(1).map(|index| tokens[index]) {
                        if !arch.is_empty() && !arch.starts_with('/') {
                            info.architecture = Some(arch.to_string());
                        }
                    }
                }
                break;
            }
            if (url.ends_with("/var/lib/dpkg/status") || url == "/var/lib/dpkg/status")
                && info.source.is_none()
            {
                info.source = Some("/var/lib/dpkg/status".to_string());
            }
        }
        if info.repository.is_some() {
            break;
        }
    }
    info
}

async fn query_package_policy(
    options: &NativeOptions,
    package: &str,
) -> Result<PolicyInfo, NativeError> {
    let output = run_command(
        &options.apt_cache_bin,
        vec![OsString::from("policy"), OsString::from(package)],
        &APT_ENV,
        QUICK_CMD_TIMEOUT,
        MAX_METADATA_OUTPUT,
        MAX_METADATA_OUTPUT,
    )
    .await?;
    if !output.success {
        return Err(NativeError::command_failed(format!(
            "cannot query APT policy for {package}: {}",
            summarize_command(&output)
        )));
    }
    Ok(parse_apt_cache_policy(&output.stdout))
}

async fn package_policy(
    options: &NativeOptions,
    op: PackagePolicyOp,
) -> Result<Value, NativeError> {
    validate_package_name(&op.package)?;
    let info = query_package_policy(options, &op.package).await?;
    Ok(json!({
        "installed": info.installed,
        "candidate": info.candidate,
        "architecture": info.architecture,
        "repository": info.repository,
        "source": info.source,
    }))
}

fn validate_deb_sha256(sha: &str) -> Result<(), NativeError> {
    validate_sha256(sha)
}

async fn deb_preview(options: &NativeOptions, op: DebPreviewOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_deb_path(&ctx, &op.path)?;
    validate_deb_sha256(&op.sha256)?;
    verify_checksum(&path, &op.sha256).await?;
    let control = deb_control(options, &path).await?;
    let package = control.get("Package").cloned().unwrap_or_default();
    let version = control.get("Version").cloned().unwrap_or_default();
    let architecture = control.get("Architecture").cloned().unwrap_or_default();
    let native_arch = native_dpkg_arch().to_string();
    let mut reasons: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    if architecture != "all" && architecture != native_arch {
        reasons.push(format!(
            "package architecture {architecture} does not match the host ({native_arch})"
        ));
    }
    let policy = query_package_policy(options, &package)
        .await
        .unwrap_or_default();
    if policy.installed.as_deref() == Some(version.as_str()) && !version.is_empty() {
        warnings.push(format!("version {version} is already installed"));
    }
    let mut args = apt_common_args();
    args.extend([
        OsString::from("-s"),
        OsString::from("-y"),
        OsString::from("--no-install-recommends"),
        OsString::from("install"),
        path.as_os_str().to_os_string(),
    ]);
    let output = apt_get(options, args, APT_PREVIEW_TIMEOUT).await?;
    let mut simulation = parse_apt_simulation(&output.stdout);
    let mut error: Option<String> = None;
    if !output.success {
        let raw = if output.stderr.trim().is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        error = Some(last_lines(raw.trim(), 1200));
        reasons.push("apt-get simulation failed; the package cannot be installed as-is".into());
    } else if !simulation.removals.is_empty() {
        warnings.push(format!(
            "installing would also remove: {}",
            simulation.removals.join(", ")
        ));
    }
    for warning in simulation.warnings.drain(..) {
        push_unique(&mut warnings, &warning);
    }
    let compatible = error.is_none() && reasons.is_empty();
    let depends = split_dpkg_field(control.get("Depends"));
    let conflicts = split_dpkg_field(control.get("Conflicts"));
    let description = control
        .get("Description")
        .map(|value| value.lines().next().unwrap_or("").to_string())
        .unwrap_or_default();
    Ok(json!({
        "compatible": compatible,
        "package": package,
        "version": version,
        "architecture": architecture,
        "nativeArch": native_arch,
        "description": description,
        "depends": depends,
        "conflicts": conflicts,
        "installs": simulation
            .installs
            .iter()
            .map(|(name, version)| json!({"name": name, "version": version}))
            .collect::<Vec<Value>>(),
        "removals": simulation.removals,
        "upgrades": simulation.upgrades,
        "downloadBytes": simulation.download_bytes.unwrap_or(0),
        "warnings": warnings,
        "reasons": reasons,
        "preexisting": policy.installed.is_some(),
        "installedVersion": policy.installed,
        "candidate": policy.candidate,
        "repository": policy.repository,
        "source": policy.source,
        "error": error,
    }))
}

/// Packages that an install simulation would remove; refusing protects host
/// packages the admin did not approve.
fn install_blockers(simulation: &AptSimulation) -> Vec<String> {
    simulation.removals.clone()
}

async fn deb_install(options: &NativeOptions, op: DebInstallOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_deb_path(&ctx, &op.path)?;
    validate_deb_sha256(&op.sha256)?;
    let control = deb_control(options, &path).await?;
    let package = control.get("Package").cloned().unwrap_or_default();
    let version = control.get("Version").cloned().unwrap_or_default();
    let architecture = control.get("Architecture").cloned().unwrap_or_default();
    let policy = query_package_policy(options, &package)
        .await
        .unwrap_or_default();
    let preexisting = policy.installed.is_some();

    let mut args = apt_common_args();
    args.extend([
        OsString::from("-s"),
        OsString::from("-y"),
        OsString::from("--no-install-recommends"),
        OsString::from("install"),
        path.as_os_str().to_os_string(),
    ]);
    let simulation_output = apt_get(options, args, APT_PREVIEW_TIMEOUT).await?;
    if !simulation_output.success {
        return Err(NativeError::install_failed(format!(
            "cannot install {package}: the apt simulation failed before any change: {}; \
             run `apt-get update` and retry, and check that the package matches the host \
             architecture ({})",
            summarize_command(&simulation_output),
            native_dpkg_arch()
        )));
    }
    let simulation = parse_apt_simulation(&simulation_output.stdout);
    let blockers = install_blockers(&simulation);
    if !blockers.is_empty() {
        return Err(NativeError::unsafe_removal(format!(
            "refusing to install {package}: the transaction would also remove packages that \
             were not approved: {}",
            blockers.join(", ")
        )));
    }

    // Re-hash immediately before handing the file to apt.
    verify_checksum(&path, &op.sha256).await?;

    let sources_before = apt_sources_snapshot();
    let mut args = apt_common_args();
    args.extend([
        OsString::from("-y"),
        OsString::from("--no-install-recommends"),
        OsString::from("install"),
        path.as_os_str().to_os_string(),
    ]);
    let output = apt_get(options, args, APT_INSTALL_TIMEOUT).await?;
    let combined = last_output_words(&output.stdout, &output.stderr);
    if !output.success {
        return Err(NativeError::install_failed(format!(
            "apt-get install failed for {package}: {}; run `apt-get update`, \
             `dpkg --configure -a`, free disk space, then retry — see the journal for details",
            last_lines(output.stderr.trim(), 1500)
        )));
    }
    let sources_after = apt_sources_snapshot();
    let apt_sources_added: Vec<String> = sources_after
        .difference(&sources_before)
        .map(|path| path_string(path))
        .collect();
    let mut installed = parse_setup_packages(&output.stdout);
    if installed.is_empty() {
        installed = simulation
            .installs
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
    }
    Ok(json!({
        "packages": installed,
        "package": package,
        "version": version,
        "architecture": architecture,
        "installs": simulation
            .installs
            .iter()
            .map(|(name, version)| json!({"name": name, "version": version}))
            .collect::<Vec<Value>>(),
        "removals": simulation.removals,
        "preexisting": preexisting,
        "aptSourcesAdded": apt_sources_added,
        "output": combined,
    }))
}

async fn deb_remove(options: &NativeOptions, op: DebRemoveOp) -> Result<Value, NativeError> {
    let packages = validate_packages(&op.packages)?;
    let verb = if op.purge { "purge" } else { "remove" };
    let mut args = apt_common_args();
    args.extend([
        OsString::from("-s"),
        OsString::from("-y"),
        OsString::from(verb),
        OsString::from("--"),
    ]);
    args.extend(packages.iter().map(OsString::from));
    let simulation_output = apt_get(options, args, APT_PREVIEW_TIMEOUT).await?;
    if !simulation_output.success {
        return Err(NativeError::command_failed(format!(
            "cannot {verb} {}: the apt simulation failed: {}",
            packages.join(", "),
            summarize_command(&simulation_output)
        )));
    }
    let simulation = parse_apt_simulation(&simulation_output.stdout);
    let requested: BTreeSet<&str> = packages.iter().map(String::as_str).collect();
    let mut blockers: Vec<String> = Vec::new();
    for name in &simulation.removals {
        if !requested.contains(name.as_str()) {
            push_unique(&mut blockers, name);
        }
    }
    for (name, _) in &simulation.installs {
        if !requested.contains(name.as_str()) {
            push_unique(&mut blockers, name);
        }
    }
    for name in &simulation.upgrades {
        if !requested.contains(name.as_str()) {
            push_unique(&mut blockers, name);
        }
    }
    if !blockers.is_empty() {
        return Err(NativeError::unsafe_removal(format!(
            "refusing to {verb}: the transaction would also change packages that were not \
             requested: {}",
            blockers.join(", ")
        )));
    }
    let removed: Vec<String> = simulation.removals.clone();

    let mut args = apt_common_args();
    args.extend([
        OsString::from("-y"),
        OsString::from(verb),
        OsString::from("--"),
    ]);
    args.extend(packages.iter().map(OsString::from));
    let output = apt_get(options, args, APT_REMOVE_TIMEOUT).await?;
    if !output.success {
        return Err(NativeError::command_failed(format!(
            "apt-get {verb} failed: {}",
            last_lines(output.stderr.trim(), 1500)
        )));
    }
    Ok(json!({
        "removed": removed,
        "output": last_output_words(&output.stdout, &output.stderr),
    }))
}

async fn deb_download(options: &NativeOptions, op: DebDownloadOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_package_name(&op.package)?;
    if let Some(version) = &op.version {
        validate_deb_version(version)?;
    }
    let dest = validate_managed_path(&ctx, &op.dest, crate::native::protocol::PathPurpose::Mkdir)?;
    std::fs::create_dir_all(&dest).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot create download directory {}: {error}",
            dest.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&dest).map_err(|error| {
        NativeError::unavailable(format!("cannot inspect {}: {error}", dest.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(
            "the download destination must be a real directory",
        ));
    }
    let debs_in = |dir: &Path| -> BTreeSet<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().map(|ext| ext == "deb") == Some(true))
                    .filter(|path| {
                        std::fs::symlink_metadata(path)
                            .map(|meta| meta.file_type().is_file())
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let before = debs_in(&dest);
    let spec = match &op.version {
        Some(version) => format!("{}={}", op.package, version),
        None => op.package.clone(),
    };
    let output = run_command_in(
        &options.apt_get_bin,
        vec![OsString::from("download"), OsString::from(&spec)],
        Some(&dest),
        &APT_ENV,
        APT_DOWNLOAD_TIMEOUT,
        MAX_APT_OUTPUT,
        MAX_APT_OUTPUT,
    )
    .await?;
    if !output.success {
        let summary = summarize_command(&output);
        let lower = summary.to_ascii_lowercase();
        if lower.contains("unable to locate")
            || lower.contains("unable to find")
            || lower.contains("no candidate")
            || lower.contains("not found")
        {
            return Err(NativeError::not_found(format!(
                "no downloadable package matches {spec} for this host; run `apt-get update` \
                 and check the candidate version/architecture"
            )));
        }
        return Err(NativeError::command_failed(format!(
            "apt-get download failed for {spec}: {summary}; run apt_update first and check \
             the configured APT sources"
        )));
    }
    let after = debs_in(&dest);
    let downloaded: Vec<PathBuf> = after.difference(&before).cloned().collect();
    if downloaded.is_empty() {
        return Err(NativeError::not_found(format!(
            "apt-get download produced no .deb for {spec}; run apt_update and check that the \
             package is available for {}",
            native_dpkg_arch()
        )));
    }
    if downloaded.len() > MAX_DOWNLOADED_DEBS {
        return Err(NativeError::command_failed(
            "apt-get download produced an unexpected number of files",
        ));
    }
    let path = downloaded[0].clone();
    let control = deb_control(options, &path).await?;
    let actual_package = control.get("Package").cloned().unwrap_or_default();
    if actual_package != op.package {
        return Err(NativeError::command_failed(format!(
            "apt-get download produced package {actual_package:?} for the requested {:?}",
            op.package
        )));
    }
    let sha256 = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || sha256_file_blocking(&path))
            .await
            .map_err(|error| {
                NativeError::command_failed(format!("cannot hash the downloaded package: {error}"))
            })?
            .map_err(|error| {
                NativeError::command_failed(format!("cannot read the downloaded package: {error}"))
            })?
    };
    Ok(json!({
        "path": path_string(&path),
        "package": actual_package,
        "version": control.get("Version").cloned().unwrap_or_default(),
        "architecture": control.get("Architecture").cloned().unwrap_or_default(),
        "sha256": sha256,
    }))
}

async fn package_installed_list(
    options: &NativeOptions,
    op: PackageInstalledListOp,
) -> Result<Value, NativeError> {
    if op.names.is_empty() {
        return Err(NativeError::invalid("names must not be empty"));
    }
    if op.names.len() > MAX_INSTALLED_QUERY {
        return Err(NativeError::invalid(format!(
            "at most {MAX_INSTALLED_QUERY} package names are allowed"
        )));
    }
    let mut wanted: Vec<String> = Vec::new();
    for name in &op.names {
        validate_package_name(name)?;
        push_unique(&mut wanted, name);
    }
    let output = run_command(
        &options.dpkg_query_bin,
        vec![
            OsString::from("-W"),
            OsString::from("--showformat=${db:Status-Abbrev} ${Package}\n"),
        ],
        &[],
        QUICK_CMD_TIMEOUT,
        MAX_DPKG_QUERY_OUTPUT,
        MAX_DPKG_QUERY_OUTPUT,
    )
    .await?;
    if !output.success {
        return Err(NativeError::command_failed(format!(
            "cannot list installed packages: {}",
            summarize_command(&output)
        )));
    }
    let mut installed: Vec<String> = Vec::new();
    for line in output.stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(status) = parts.next() else {
            continue;
        };
        if !status.starts_with("ii") {
            continue;
        }
        let Some(name) = parts.next() else {
            continue;
        };
        if wanted.iter().any(|wanted| wanted == name) {
            push_unique(&mut installed, name);
        }
    }
    Ok(json!({"installed": installed}))
}

async fn package_files(options: &NativeOptions, op: PackageFilesOp) -> Result<Value, NativeError> {
    validate_package_name(&op.package)?;
    let output = run_command(
        &options.dpkg_query_bin,
        vec![OsString::from("-L"), OsString::from(&op.package)],
        &[],
        QUICK_CMD_TIMEOUT,
        MAX_DPKG_QUERY_OUTPUT,
        MAX_DPKG_QUERY_OUTPUT,
    )
    .await?;
    if !output.success {
        let summary = summarize_command(&output);
        if summary.to_ascii_lowercase().contains("not installed")
            || summary.to_ascii_lowercase().contains("no packages found")
        {
            return Err(NativeError::not_found(format!(
                "package {} is not installed",
                op.package
            )));
        }
        return Err(NativeError::command_failed(format!(
            "cannot list files of {}: {summary}",
            op.package
        )));
    }
    let mut files: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in output.stdout.lines() {
        let line = line.trim();
        if !line.starts_with('/') {
            continue;
        }
        if files.len() >= MAX_PACKAGE_FILES || bytes + line.len() > MAX_PACKAGE_FILES_BYTES {
            break;
        }
        bytes += line.len();
        files.push(line.to_string());
    }
    Ok(json!({"files": files}))
}

async fn apt_update(options: &NativeOptions) -> Result<Value, NativeError> {
    let mut args = apt_common_args();
    args.push(OsString::from("update"));
    let output = apt_get(options, args, APT_UPDATE_TIMEOUT).await?;
    if !output.success {
        return Err(NativeError::apt_update_failed(format!(
            "apt-get update failed: {}; check network access and the APT sources/signing keys",
            last_lines(output.stderr.trim(), 1500)
        )));
    }
    Ok(json!({
        "ok": true,
        "updated": true,
        "output": last_output_words(&output.stdout, &output.stderr),
    }))
}

// ---------------------------------------------------------------------------
// Managed filesystem operations
// ---------------------------------------------------------------------------

fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return crate::native::lchown(path, uid, gid);
    }
    if metadata.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            chown_tree(&entry.path(), uid, gid)?;
        }
    }
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
}

fn chmod_tree(path: &Path, mode: u32) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        // Never chmod through a symlink.
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if metadata.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            chmod_tree(&entry.path(), mode)?;
        }
    }
    Ok(())
}

fn require_existing_dir(path: &Path) -> Result<(), NativeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(NativeError::invalid(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(NativeError::not_found(
            format!("{} does not exist", path.display()),
        )),
        Err(error) => Err(NativeError::unavailable(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

async fn path_mkdir(options: &NativeOptions, op: PathMkdirOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_managed_path(&ctx, &op.path, crate::native::protocol::PathPurpose::Mkdir)?;
    validate_mode(op.mode)?;
    validate_managed_uid(&ctx, op.uid, op.gid)?;
    std::fs::create_dir_all(&path).map_err(|error| {
        NativeError::command_failed(format!("cannot create {}: {error}", path.display()))
    })?;
    require_existing_dir(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(op.mode)).map_err(|error| {
        NativeError::command_failed(format!("cannot chmod {}: {error}", path.display()))
    })?;
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&path, Some(op.uid), Some(op.gid)).map_err(|error| {
            NativeError::command_failed(format!("cannot chown {}: {error}", path.display()))
        })?;
    }
    Ok(json!({"ok": true}))
}

async fn path_chown(options: &NativeOptions, op: PathChownOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_managed_path(&ctx, &op.path, crate::native::protocol::PathPurpose::Chown)?;
    validate_managed_uid(&ctx, op.uid, op.gid)?;
    if !path.exists() {
        return Err(NativeError::not_found(format!(
            "{} does not exist",
            path.display()
        )));
    }
    if !options.is_test_mode() {
        let target = path.clone();
        let display = path.display().to_string();
        let (uid, gid) = (op.uid, op.gid);
        let recursive = op.recursive;
        tokio::task::spawn_blocking(move || {
            if recursive {
                chown_tree(&target, uid, gid)
            } else {
                let metadata = std::fs::symlink_metadata(&target)?;
                if metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to chown a symlink",
                    ));
                }
                std::os::unix::fs::chown(&target, Some(uid), Some(gid))
            }
        })
        .await
        .map_err(|error| NativeError::command_failed(format!("chown task failed: {error}")))?
        .map_err(|error| NativeError::command_failed(format!("cannot chown {display}: {error}")))?;
    }
    Ok(json!({"ok": true}))
}

async fn path_remove(options: &NativeOptions, op: PathRemoveOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_managed_path(&ctx, &op.path, crate::native::protocol::PathPurpose::Remove)?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let path_clone = path.clone();
            tokio::task::spawn_blocking(move || crate::native::remove_tree(&path_clone))
                .await
                .map_err(|error| {
                    NativeError::command_failed(format!("remove task failed: {error}"))
                })?
                .map_err(|error| {
                    NativeError::command_failed(format!(
                        "cannot remove {}: {error}",
                        path.display()
                    ))
                })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(NativeError::unavailable(format!(
                "cannot inspect {}: {error}",
                path.display()
            )))
        }
    }
    Ok(json!({"ok": true}))
}

async fn path_chmod(options: &NativeOptions, op: PathChmodOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_managed_path(&ctx, &op.path, crate::native::protocol::PathPurpose::Chmod)?;
    validate_mode(op.mode_octal)?;
    require_existing_dir_or_file(&path)?;
    let path_clone = path.clone();
    let mode = op.mode_octal;
    tokio::task::spawn_blocking(move || {
        if op.recursive {
            chmod_tree(&path_clone, mode)
        } else {
            let metadata = std::fs::symlink_metadata(&path_clone)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to chmod a symlink",
                ));
            }
            std::fs::set_permissions(&path_clone, std::fs::Permissions::from_mode(mode))
        }
    })
    .await
    .map_err(|error| NativeError::command_failed(format!("chmod task failed: {error}")))?
    .map_err(|error| {
        NativeError::command_failed(format!("cannot chmod {}: {error}", path.display()))
    })?;
    Ok(json!({"ok": true}))
}

fn require_existing_dir_or_file(path: &Path) -> Result<(), NativeError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(NativeError::not_found(
            format!("{} does not exist", path.display()),
        )),
        Err(error) => Err(NativeError::unavailable(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

async fn path_acl(options: &NativeOptions, op: PathAclOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let path = validate_managed_path(&ctx, &op.path, crate::native::protocol::PathPurpose::Acl)?;
    validate_managed_uid(&ctx, op.uid, op.uid)?;
    validate_acl_perms(&op.perms)?;
    let Some(setfacl) = &options.setfacl_bin else {
        return Err(NativeError::acls_unavailable(
            "setfacl is not installed; install the 'acl' package (apt-get install acl) and make \
             sure the filesystem is mounted with ACL support",
        ));
    };
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| NativeError::not_found(format!("{} does not exist", path.display())))?;
    if metadata.file_type().is_symlink() {
        return Err(NativeError::forbidden("path must not be a symbolic link"));
    }
    let mut args: Vec<OsString> = Vec::new();
    if op.recursive {
        args.push(OsString::from("-R"));
    }
    args.push(OsString::from("-m"));
    args.push(OsString::from(format!("u:{}:{}", op.uid, op.perms)));
    args.push(path.as_os_str().to_os_string());
    let output = run_command(setfacl, args, &[], QUICK_CMD_TIMEOUT, 64 * 1024, 64 * 1024).await?;
    if !output.success {
        let summary = summarize_command(&output);
        let lower = summary.to_ascii_lowercase();
        if lower.contains("not supported") || lower.contains("operation not supported") {
            return Err(NativeError::acls_unavailable(format!(
                "the filesystem does not support POSIX ACLs for {}; mount it with the 'acl' \
                 option or use a different storage location",
                path.display()
            )));
        }
        return Err(NativeError::command_failed(format!(
            "cannot grant ACL to {} on {}: {summary}",
            op.uid,
            path.display()
        )));
    }
    Ok(json!({"ok": true}))
}

async fn path_symlink(options: &NativeOptions, op: PathSymlinkOp) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    let link_raw = validate_absolute_path(&op.link)?;
    let Some(name) = link_raw.file_name().map(|name| name.to_os_string()) else {
        return Err(NativeError::invalid("symlink path has no file name"));
    };
    let Some(parent) = link_raw.parent() else {
        return Err(NativeError::invalid("symlink path has no parent"));
    };
    let parent = validate_managed_path(
        &ctx,
        &path_string(parent),
        crate::native::protocol::PathPurpose::Mkdir,
    )?;
    require_existing_dir(&parent)?;
    let link = parent.join(name);
    if !link.starts_with(&parent) {
        return Err(NativeError::forbidden(
            "symlink escapes its parent directory",
        ));
    }
    let target = validate_symlink_target(&ctx, &op.target)?;
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() {
                return Err(NativeError::new(
                    "already_exists",
                    format!("{} exists and is not a symbolic link", link.display()),
                ));
            }
            let existing = std::fs::read_link(&link).map_err(|error| {
                NativeError::command_failed(format!(
                    "cannot read existing symlink {}: {error}",
                    link.display()
                ))
            })?;
            if existing != target {
                return Err(NativeError::new(
                    "already_exists",
                    format!(
                        "{} is a symlink to {} instead of {}",
                        link.display(),
                        existing.display(),
                        target.display()
                    ),
                ));
            }
            return Ok(json!({"ok": true}));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(NativeError::unavailable(format!(
                "cannot inspect {}: {error}",
                link.display()
            )))
        }
    }
    std::os::unix::fs::symlink(&target, &link).map_err(|error| {
        NativeError::command_failed(format!(
            "cannot create symlink {} -> {}: {error}",
            link.display(),
            target.display()
        ))
    })?;
    Ok(json!({"ok": true}))
}

fn ensure_owned_dir(
    options: &NativeOptions,
    path: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), NativeError> {
    std::fs::create_dir_all(path).map_err(|error| {
        NativeError::command_failed(format!("cannot create {}: {error}", path.display()))
    })?;
    require_existing_dir(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        NativeError::command_failed(format!("cannot chmod {}: {error}", path.display()))
    })?;
    if !options.is_test_mode() {
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|error| {
            NativeError::command_failed(format!("cannot chown {}: {error}", path.display()))
        })?;
    }
    Ok(())
}

/// Remove Chromium/Electron singleton files left in a person's app profile by
/// a browser that was killed or stopped. Chrome refuses to open a window (or
/// silently delegates to a dead instance) when these stale files remain, and
/// the legacy Docker image did the same cleanup in its init script.
async fn profile_clean_locks(
    options: &NativeOptions,
    op: ProfileCleanLocksOp,
) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_app_id(&op.app_id)?;
    let ids = identity_ids(&ctx, op.user_id)?;
    let profile = options
        .profiles_dir()
        .join(&op.app_id)
        .join(op.user_id.to_string());
    if !profile.exists() {
        return Ok(json!({"removed": 0, "profile": path_string(&profile)}));
    }
    let metadata = std::fs::symlink_metadata(&profile).map_err(|error| {
        NativeError::unavailable(format!("cannot inspect {}: {error}", profile.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(format!(
            "the profile {} is not a real directory",
            profile.display()
        )));
    }
    // A live browser must keep its singleton files: only clean when no other
    // process of this identity is running the same app. The session manager
    // only calls this before starting an app that is not already running, so
    // an extra check is cheap defence in depth.
    let _ = ids;
    let mut removed = 0u32;
    let mut stack = vec![(profile.clone(), 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("Singleton") {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() || meta.file_type().is_symlink() {
                        if std::fs::remove_file(&path).is_ok() {
                            removed += 1;
                        }
                        continue;
                    }
                }
            }
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                stack.push((path, depth + 1));
            }
        }
    }
    Ok(json!({"removed": removed, "profile": path_string(&profile)}))
}

/// Copy a mapped profile tree (created by the unprivileged server under the
/// migration directory) into the person's native profile and hand it to their
/// unprivileged identity. The helper runs as root, so the identity-owned 0700
/// profile can be written without ever exposing it to the server.
async fn profile_install(
    options: &NativeOptions,
    op: ProfileInstallOp,
) -> Result<Value, NativeError> {
    validate_app_id(&op.app_id)?;
    let ids = identity_ids(&options.validation_context(), op.user_id)?;
    let expected_home = options.linux_users_dir().join(op.user_id.to_string());
    let username = crate::native::protocol::username_for(op.user_id);
    let rast_home = options.homes_dir.join(&username);
    if !rast_home.is_dir() {
        std::fs::create_dir_all(&rast_home).map_err(|error| {
            NativeError::command_failed(format!("cannot create {}: {error}", rast_home.display()))
        })?;
        if let Ok((uid, gid)) = crate::native::data_dir_owner(options, ids.gid) {
            let _ = std::os::unix::fs::chown(&rast_home, Some(uid), Some(gid));
        }
    }
    let prepared = profile_prepare(
        options,
        ProfilePrepareOp {
            app_id: op.app_id.clone(),
            user_id: op.user_id,
            uid: ids.uid,
            gid: ids.gid,
            home: path_string(&expected_home),
            rast_home: path_string(&rast_home),
        },
    )
    .await?;
    let profile = prepared
        .get("profile")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| NativeError::protocol("profile_prepare returned no profile"))?;
    let source = validate_absolute_path(&op.source)?;
    let canonical = std::fs::canonicalize(&source).map_err(|error| {
        NativeError::invalid(format!("cannot resolve {}: {error}", source.display()))
    })?;
    let migrations_root = std::fs::canonicalize(options.linux_apps_dir().join("migrations"))
        .map_err(|_| NativeError::forbidden("the migration directory is not available"))?;
    if !canonical.starts_with(&migrations_root) || canonical == migrations_root {
        return Err(NativeError::forbidden(
            "the profile import source must be a migration directory",
        ));
    }
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| {
        NativeError::invalid(format!("cannot inspect {}: {error}", canonical.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::forbidden(
            "the profile import source must be a real directory",
        ));
    }
    let (files, dirs, skipped) = copy_profile_tree(&canonical, &profile, ids.uid, ids.gid)?;
    Ok(json!({"files": files, "dirs": dirs, "skipped": skipped}))
}

/// Recursively copy `source` into the identity-owned `target` (root, so
/// permission bits are not an obstacle), chowning everything to the identity.
/// Symlinks and special files are skipped: app profiles are data, not code,
/// and the migration must never follow a link out of the exported tree.
fn copy_profile_tree(
    source: &Path,
    target: &Path,
    uid: u32,
    gid: u32,
) -> Result<(u64, u64, u64), NativeError> {
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut skipped = 0u64;
    let mut entries_seen = 0u64;
    let mut stack = vec![(source.to_path_buf(), target.to_path_buf(), 0u32)];
    while let Some((from, to, depth)) = stack.pop() {
        if depth > 64 {
            continue;
        }
        if entries_seen > 1_000_000 {
            return Err(NativeError::invalid(
                "the saved app data has too many files to migrate",
            ));
        }
        let entries = std::fs::read_dir(&from).map_err(|error| {
            NativeError::command_failed(format!("cannot read {}: {error}", from.display()))
        })?;
        for entry in entries.flatten() {
            entries_seen += 1;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let child_from = entry.path();
            let child_to = to.join(&name);
            let meta = match std::fs::symlink_metadata(&child_from) {
                Ok(meta) => meta,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() && !meta.is_file() {
                skipped += 1;
                continue;
            }
            if meta.is_dir() {
                if name_str == ".." {
                    skipped += 1;
                    continue;
                }
                std::fs::create_dir_all(&child_to).map_err(|error| {
                    NativeError::command_failed(format!(
                        "cannot create {}: {error}",
                        child_to.display()
                    ))
                })?;
                let _ = std::fs::set_permissions(
                    &child_to,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                );
                let _ = std::os::unix::fs::chown(&child_to, Some(uid), Some(gid));
                dirs += 1;
                stack.push((child_from, child_to, depth + 1));
                continue;
            }
            let mode = meta.permissions().mode() & 0o700;
            std::fs::copy(&child_from, &child_to).map_err(|error| {
                NativeError::command_failed(format!(
                    "cannot copy {}: {error}",
                    child_from.display()
                ))
            })?;
            let _ = std::fs::set_permissions(
                &child_to,
                std::os::unix::fs::PermissionsExt::from_mode(mode.max(0o600)),
            );
            let _ = std::os::unix::fs::chown(&child_to, Some(uid), Some(gid));
            files += 1;
        }
    }
    Ok((files, dirs, skipped))
}

async fn profile_prepare(
    options: &NativeOptions,
    op: ProfilePrepareOp,
) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_app_id(&op.app_id)?;
    let ids = identity_ids(&ctx, op.user_id)?;
    if op.uid != ids.uid || op.gid != ids.gid {
        return Err(NativeError::forbidden(format!(
            "uid/gid {}/{} do not match the identity derived from userId {} ({}/{})",
            op.uid, op.gid, op.user_id, ids.uid, ids.gid
        )));
    }
    let expected_home = options.linux_users_dir().join(op.user_id.to_string());
    let home = validate_absolute_path(&op.home)?;
    let canonical_home = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
    let canonical_expected =
        std::fs::canonicalize(&expected_home).unwrap_or_else(|_| expected_home.clone());
    if canonical_home != canonical_expected {
        return Err(NativeError::forbidden(format!(
            "home must be {}",
            expected_home.display()
        )));
    }
    let rast_home = validate_absolute_path(&op.rast_home)?;
    let homes_root = std::fs::canonicalize(&options.homes_dir).map_err(|error| {
        NativeError::forbidden(format!(
            "the RAST homes directory {} is not available: {error}",
            options.homes_dir.display()
        ))
    })?;
    let canonical_rast_home = std::fs::canonicalize(&rast_home).map_err(|error| {
        NativeError::invalid(format!(
            "cannot resolve rastHome {}: {error}",
            rast_home.display()
        ))
    })?;
    if !canonical_rast_home.starts_with(&homes_root) {
        return Err(NativeError::forbidden(
            "rastHome must be inside the RAST homes directory",
        ));
    }
    require_existing_dir(&canonical_rast_home)?;

    let identity = Identity {
        user_id: ids.user_id,
        username: crate::native::protocol::username_for(ids.user_id),
        uid: ids.uid,
        gid: ids.gid,
        home: expected_home.clone(),
    };
    identity::ensure_home(options, &identity).await?;
    let _ = identity::ensure_runtime_dir(options, &identity)?;
    // The identity must be able to traverse linux-apps -> profiles -> <appId>
    // to reach its own 0700 profile tree. 0711 hides listings while allowing
    // traversal (the same pattern as linux-users). The appId directory stays
    // root:rast so a compromised app identity cannot rename other profiles.
    let rast_gid = options.effective_rast_gid()?;
    // Ensure the complete shared parent chain in one place, with the documented
    // ownership: linux-apps/apps like the data directory (writable by rast,
    // traversable by identities), profiles root:<data-dir gid> 0711. This is
    // what makes systemd's chdir into the per-user profile possible; doing it
    // here (before the profile is returned) prevents the transient state where
    // a root-owned 0750 ancestor blocked the identity.
    crate::native::ensure_managed_roots(options, rast_gid)?;
    let (_, data_gid) = crate::native::data_dir_owner(options, rast_gid)?;
    let app_id_dir = options.profiles_dir().join(&op.app_id);
    ensure_owned_dir(options, &app_id_dir, 0, data_gid, 0o711)?;
    let profile = app_id_dir.join(op.user_id.to_string());
    let profile_home = profile.join("home");
    ensure_owned_dir(options, &profile, op.uid, op.gid, 0o700)?;
    for subdir in ["home", "config", "data", "cache", "state", "tmp"] {
        ensure_owned_dir(options, &profile.join(subdir), op.uid, op.gid, 0o700)?;
    }
    ensure_identity_traversal(options, &profile)?;
    ensure_identity_traversal(options, &expected_home)?;
    Ok(json!({
        "home": path_string(&profile_home),
        "profile": path_string(&profile),
    }))
}

async fn profile_remove(
    options: &NativeOptions,
    op: ProfileRemoveOp,
) -> Result<Value, NativeError> {
    let ctx = options.validation_context();
    validate_app_id(&op.app_id)?;
    let ids = identity_ids(&ctx, op.user_id)?;
    let profile = options
        .profiles_dir()
        .join(&op.app_id)
        .join(ids.user_id.to_string());
    if !profile.starts_with(options.profiles_dir()) {
        return Err(NativeError::forbidden(
            "profile path escapes the profiles root",
        ));
    }
    let remove = profile.clone();
    tokio::task::spawn_blocking(move || match std::fs::symlink_metadata(&remove) {
        Ok(_) => crate::native::remove_tree(&remove),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    })
    .await
    .map_err(|error| NativeError::command_failed(format!("remove task failed: {error}")))?
    .map_err(|error| {
        NativeError::command_failed(format!("cannot remove {}: {error}", profile.display()))
    })?;
    Ok(json!({"removed": true}))
}

/// Validate that a directory is absent or a real directory, and return its
/// canonical path.
fn ensure_directory_does_not_escape(path: &Path, root: &Path) -> Result<PathBuf, NativeError> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        NativeError::invalid(format!("cannot resolve {}: {error}", path.display()))
    })?;
    if !canonical.starts_with(root) {
        return Err(NativeError::forbidden("path escapes the managed directory"));
    }
    Ok(canonical)
}

// Keep the helpers above referenced (documented building blocks used by tests
// and future operations); this avoids dead-code warnings without deleting
// security-relevant validation helpers.
#[allow(dead_code)]
fn _documented_helpers(options: &NativeOptions) {
    let ctx = options.validation_context();
    let _ = ensure_directory_does_not_escape(&options.run_dir, &options.run_dir);
    let _ = validate_symlink_target(&ctx, "/tmp");
    let _ = session_unit(1);
    let _ = validate_port(1024);
    let _ = ResourceLimits::from_config(1, 1, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::client::NativeClient;
    use crate::native::protocol::{
        DebPreviewOp, IdentityEnsureOp, NativeRequest, PathMkdirOp, PathRemoveOp, ProfilePrepareOp,
        ProfileRemoveOp, SessionStartOp, SessionStopOp, UnitStatusOp,
    };
    use tokio::io::AsyncReadExt;
    use tokio::sync::oneshot;

    struct TestHelper {
        _dir: tempfile::TempDir,
        options: NativeOptions,
        socket: PathBuf,
        shutdown: Option<oneshot::Sender<()>>,
        server: Option<tokio::task::JoinHandle<io::Result<()>>>,
    }

    impl Drop for TestHelper {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(server) = self.server.take() {
                server.abort();
            }
        }
    }

    impl TestHelper {
        async fn start(configure: impl FnOnce(&mut NativeOptions, &Path)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let data = root.join("data");
            let run = root.join("run");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::create_dir_all(&run).unwrap();
            std::fs::create_dir_all(root.join("storage/homes/alice")).unwrap();
            let socket = root.join("sock/native.sock");
            let mut options = NativeOptions::for_test(socket.clone(), data, run);
            configure(&mut options, &root);
            let server_options = options.clone();
            let (tx, rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                serve(server_options, async move {
                    let _ = rx.await;
                })
                .await
            });
            let mut ready = false;
            for _ in 0..200 {
                if UnixStream::connect(&socket).await.is_ok() {
                    ready = true;
                    break;
                }
                if server.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if !ready {
                server.abort();
                let result = server.await;
                panic!("native helper test server did not start: {result:?}");
            }
            Self {
                _dir: dir,
                options,
                socket,
                shutdown: Some(tx),
                server: Some(server),
            }
        }

        fn client(&self) -> NativeClient {
            NativeClient::new(self.socket.clone())
        }

        async fn call(&self, op: NativeOp) -> Result<Value, crate::native::protocol::NativeError> {
            self.client().call(NativeRequest::new(1, op)).await
        }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Fake `systemctl`: logs args, answers `show` from marker files.
    fn systemctl_script(dir: &Path) -> PathBuf {
        let log = dir.join("systemctl.log");
        let body = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case "$1" in
  show)
    for arg in "$@"; do
      case "$arg" in
        rast-*.service)
          if [ -f "{dir}/active-$arg" ]; then
            echo "Id=$arg"; echo "LoadState=loaded"; echo "ActiveState=active"; echo "MainPID=1"; echo
          elif [ -f "{dir}/failed-$arg" ]; then
            echo "Id=$arg"; echo "LoadState=loaded"; echo "ActiveState=failed"; echo "MainPID=0"; echo
          fi
          ;;
      esac
    done
    exit 0
    ;;
  reset-failed)
    base=${{2%.service}}
    rm -f "{dir}/active-$base" "{dir}/active-$base.service" "{dir}/failed-$base" "{dir}/failed-$base.service"
    exit 0
    ;;
  stop)
    base=${{2%.service}}
    rm -f "{dir}/active-$base" "{dir}/active-$base.service" "{dir}/failed-$base" "{dir}/failed-$base.service"
    exit 0
    ;;
  is-active) echo active; exit 0 ;;
esac
exit 0
"#,
            log = log.display(),
            dir = dir.display()
        );
        write_script(dir, "systemctl", &body)
    }

    /// Fake `systemd-run`: logs its argv (unit separators) and succeeds.
    fn systemd_run_script(dir: &Path) -> PathBuf {
        let log = dir.join("systemd-run.log");
        let body = format!(
            r#"#!/bin/sh
line=""
sep=""
for arg in "$@"; do line="$line$sep$arg"; sep=$(printf '\037'); done
printf '%s\n' "$line" >> "{log}"
exit 0
"#,
            log = log.display()
        );
        write_script(dir, "systemd-run", &body)
    }

    fn logged_args(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('\u{1f}').map(str::to_string).collect())
            .collect()
    }

    fn fake_xpra(dir: &Path) -> PathBuf {
        write_script(dir, "xpra", "#!/bin/sh\nexit 0\n")
    }

    /// Fake getent + groupadd/useradd/userdel/groupdel backed by marker files.
    fn identity_scripts(dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let getent = write_script(
            dir,
            "getent",
            &format!(
                r#"#!/bin/sh
case "$1" in
  passwd)
    if [ -f "{dir}/user.exists" ]; then
      echo "rast-u7:x:200007:200007:RAST Linux app:{data}/linux-users/7:/usr/sbin/nologin"
      exit 0
    fi
    exit 2
    ;;
  group)
    if [ -f "{dir}/group.exists" ]; then
      echo "rast-u7:x:200007:"
      exit 0
    fi
    exit 2
    ;;
esac
exit 2
"#,
                dir = dir.display(),
                data = dir.join("data").display()
            ),
        );
        let groupadd = write_script(
            dir,
            "groupadd",
            &format!(
                "#!/bin/sh\ntouch '{dir}/group.exists'\nexit 0\n",
                dir = dir.display()
            ),
        );
        let useradd = write_script(
            dir,
            "useradd",
            &format!(
                "#!/bin/sh\ntouch '{dir}/user.exists'\nexit 0\n",
                dir = dir.display()
            ),
        );
        let userdel = write_script(
            dir,
            "userdel",
            &format!(
                "#!/bin/sh\nrm -f '{dir}/user.exists'\nexit 0\n",
                dir = dir.display()
            ),
        );
        let groupdel = write_script(
            dir,
            "groupdel",
            &format!(
                "#!/bin/sh\nrm -f '{dir}/group.exists'\nexit 0\n",
                dir = dir.display()
            ),
        );
        (getent, groupadd, useradd, userdel, groupdel)
    }

    /// Fake `systemctl` for `units_list`: answers the two glob selectors with
    /// fixed active units.
    fn systemctl_list_script(dir: &Path) -> PathBuf {
        let body = r#"#!/bin/sh
case "$1" in
  show)
    for arg in "$@"; do
      case "$arg" in
        *xpra*) echo "Id=rast-xpra-u7.service"; echo "ActiveState=active"; echo "LoadState=loaded"; echo "MainPID=1"; echo ;;
        *app*) echo "Id=rast-app-demo.service"; echo "ActiveState=active"; echo "LoadState=loaded"; echo "MainPID=2"; echo ;;
      esac
    done
    exit 0
    ;;
  reset-failed|stop) exit 0 ;;
esac
exit 0
"#;
        write_script(dir, "systemctl", body)
    }

    fn configure_identity(options: &mut NativeOptions, dir: &Path) {
        let (getent, groupadd, useradd, userdel, groupdel) = identity_scripts(dir);
        options.getent_bin = getent;
        options.groupadd_bin = groupadd;
        options.useradd_bin = useradd;
        options.userdel_bin = userdel;
        options.groupdel_bin = groupdel;
    }

    #[tokio::test]
    async fn ping_reports_protocol_and_xpra() {
        let helper = TestHelper::start(|options, dir| {
            options.xpra_bin = fake_xpra(dir);
        })
        .await;
        let ping = helper.client().ping().await.unwrap();
        assert_eq!(ping["version"], PROTOCOL_VERSION);
        assert_eq!(ping["root"], true);
        assert_eq!(ping["xpra"], true);
        assert_eq!(ping["dockerFallback"], false);
    }

    #[tokio::test]
    async fn unreadable_and_unknown_requests_are_answered() {
        let helper = TestHelper::start(|_, _| {}).await;
        for (line, code) in [
            (r#"{"id":5,"op":"run_shell"}"#, "unsupported"),
            (r#"{"id":6,"op":"ping","extra":1}"#, "invalid_request"),
            ("not json", "invalid_request"),
        ] {
            let mut stream = UnixStream::connect(&helper.socket).await.unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut response = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut response)
                .await
                .unwrap();
            let value: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(value["ok"], false, "{line}");
            assert_eq!(value["error"]["code"], code, "{line}");
        }
    }

    #[tokio::test]
    async fn oversized_request_is_rejected() {
        let helper = TestHelper::start(|_, _| {}).await;
        let mut stream = UnixStream::connect(&helper.socket).await.unwrap();
        let payload = format!(
            "{{\"id\":1,\"op\":\"ping\",\"pad\":\"{}\"}}",
            "x".repeat(MAX_REQUEST_LINE_BYTES + 16)
        );
        let _ = stream.write_all(payload.as_bytes()).await;
        let _ = stream.write_all(b"\n").await;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut response)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn peer_uid_allowlist_is_enforced() {
        let helper = TestHelper::start(|options, _| {
            // The test process is not root; requiring uid 0 must reject it.
            options.allowed_peer_uid = Some(0);
        })
        .await;
        let mut stream = UnixStream::connect(&helper.socket).await.unwrap();
        let _ = stream.write_all(b"{\"id\":1,\"op\":\"ping\"}\n").await;
        let mut buffer = [0u8; 64];
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer)).await;
        assert!(
            matches!(read, Ok(Ok(0))),
            "connection must be dropped before any response: {read:?}"
        );
    }

    #[tokio::test]
    async fn identity_ensure_creates_and_is_idempotent() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
        })
        .await;
        let first = helper
            .call(NativeOp::IdentityEnsure(IdentityEnsureOp {
                user_id: 7,
                username: None,
            }))
            .await
            .unwrap();
        assert_eq!(first["username"], "rast-u7");
        assert_eq!(first["uid"], 200_007);
        assert_eq!(first["gid"], 200_007);
        assert_eq!(first["created"], true);
        let home = PathBuf::from(first["home"].as_str().unwrap());
        assert!(home.is_dir());
        let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let second = helper
            .call(NativeOp::IdentityEnsure(IdentityEnsureOp {
                user_id: 7,
                username: Some("rast-u7".into()),
            }))
            .await
            .unwrap();
        assert_eq!(second["created"], false);

        let bad = helper
            .call(NativeOp::IdentityEnsure(IdentityEnsureOp {
                user_id: 7,
                username: Some("alice".into()),
            }))
            .await
            .unwrap_err();
        assert_eq!(bad.code, "invalid_request");
    }

    #[tokio::test]
    async fn session_start_builds_a_safe_transient_unit() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            // identity exists
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
            options.xpra_bin = fake_xpra(dir);
        })
        .await;
        let runtime_dir = helper.options.run_dir.join("200007").join("sockets");
        let xpra = helper.options.xpra_bin.clone();
        let result = helper
            .call(NativeOp::SessionStart(SessionStartOp {
                user_id: 7,
                display: 102,
                port: 14_500,
                xpra_bin: xpra.to_string_lossy().into_owned(),
                runtime_dir: runtime_dir.to_string_lossy().into_owned(),
                args: vec![
                    "start".into(),
                    ":102".into(),
                    "--daemon=no".into(),
                    "--auth=none".into(),
                ],
                env: BTreeMap::from([("DISPLAY".to_string(), ":102".to_string())]),
                memory_max_mb: 2048,
                cpu_quota_percent: 200,
                tasks_max: 512,
            }))
            .await
            .unwrap();
        assert_eq!(result["unit"], "rast-xpra-u7");
        assert_eq!(result["active"], true);
        assert!(runtime_dir.is_dir());
        let pulse = runtime_dir.join("pulse");
        assert!(pulse.is_dir());
        assert_eq!(
            std::fs::metadata(&pulse).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let log = helper
            .options
            .systemd_run_bin
            .parent()
            .unwrap()
            .join("systemd-run.log");
        let calls = logged_args(&log);
        assert_eq!(calls.len(), 1);
        let args = &calls[0];
        assert!(args.contains(&"--unit=rast-xpra-u7".to_string()));
        assert!(args.contains(&"--slice=rast-linux.slice".to_string()));
        assert!(args.contains(&"--uid=200007".to_string()));
        assert!(args.contains(&"--gid=200007".to_string()));
        assert!(args.contains(&"--property=Type=exec".to_string()));
        assert!(args.contains(&"--property=MemoryMax=2048M".to_string()));
        assert!(args.contains(&"--property=CPUQuota=200%".to_string()));
        assert!(args.contains(&"--property=TasksMax=512".to_string()));
        assert!(args.contains(&"--property=KillMode=control-group".to_string()));
        assert!(args.contains(&"--property=Restart=no".to_string()));
        assert!(args.contains(&"--setenv=DISPLAY=:102".to_string()));
        for (key, value) in super::super::audio::environment(&runtime_dir) {
            assert!(args.contains(&format!("--setenv={key}={value}")));
        }
        assert!(args.contains(&"--setenv=XPRA_PRIVATE_PULSEAUDIO=0".to_string()));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("CAP_SYS_NICE") || arg.contains("--uid=0")));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("--setenv=XAUTHORITY=")));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("--setenv=USER=rast-u7")));
        assert!(args.iter().any(|arg| arg.starts_with("--setenv=HOME=")));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("--setenv=XDG_RUNTIME_DIR=")));
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&args[separator + 1], &xpra.to_string_lossy().into_owned());
        assert_eq!(&args[separator + 2], "start");
        assert_eq!(&args[separator + 3], ":102");
        assert_eq!(&args[separator + 4], "--daemon=no");
    }

    #[test]
    fn runtime_preparation_cannot_chown_another_users_audio_or_helper_config() {
        let root = tempfile::tempdir().unwrap();
        let options = NativeOptions::for_test(
            root.path().join("native.sock"),
            root.path().join("data"),
            root.path().join("run"),
        );
        std::fs::create_dir_all(&options.run_dir).unwrap();
        super::super::audio::prepare_config(&options.run_dir).unwrap();
        let identity = Identity::derive(&options, 7).unwrap();
        for target in [
            options.run_dir.join("200008/pulse"),
            super::super::audio::config_dir(&options.run_dir),
        ] {
            let error =
                prepare_runtime_dir(&options, &identity, Some(&path_string(&target))).unwrap_err();
            assert_eq!(error.code, "forbidden");
        }
        let pulse = options.run_dir.join("200007/pulse");
        prepare_runtime_dir(&options, &identity, Some(&path_string(&pulse))).unwrap();
        assert_eq!(
            std::fs::metadata(pulse).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn session_and_app_audio_env_cannot_select_a_host_or_other_users_server() {
        let options = NativeOptions::for_test(
            PathBuf::from("/tmp/audio-env-test/native.sock"),
            PathBuf::from("/tmp/audio-env-test/data"),
            PathBuf::from("/tmp/audio-env-test/run"),
        );
        let identity = Identity::derive(&options, 7).unwrap();
        let runtime = options.run_dir.join(identity.uid.to_string());
        let requested = BTreeMap::from([
            (
                "PULSE_SERVER".into(),
                "unix:/run/user/1000/pulse/native".into(),
            ),
            ("PULSE_COOKIE".into(), "/tmp/other-user-cookie".into()),
            ("PULSE_SINK".into(), "host-speakers".into()),
            ("PULSE_SOURCE".into(), "host-microphone".into()),
            ("PULSE_RUNTIME_PATH".into(), "/tmp/other-runtime".into()),
            ("PULSE_STATE_PATH".into(), "/tmp/other-state".into()),
            ("PULSE_CONFIG_PATH".into(), "/etc/pulse".into()),
            ("XPRA_PRIVATE_PULSEAUDIO".into(), "1".into()),
            ("HOME".into(), "/tmp/application-profile".into()),
        ]);
        let session = effective_session_env(&options, &identity, &requested, &runtime);
        let app = effective_app_env(&options, &identity, &requested, &runtime, None);
        for (key, value) in super::super::audio::environment(&runtime) {
            assert_eq!(session.get(&key), Some(&value));
            assert_eq!(app.get(&key), Some(&value));
        }
        assert_eq!(session["XPRA_PRIVATE_PULSEAUDIO"], "0");
        assert_eq!(
            session["XPRA_USER_CONF_DIRS"],
            path_string(&super::super::audio::config_dir(&options.run_dir))
        );
        assert_eq!(app["HOME"], "/tmp/application-profile");
        assert_eq!(session["HOME"], path_string(&identity.home));
    }

    #[test]
    fn audio_flags_are_read_from_the_unit_command_line() {
        // The test binary itself is not Xpra: it must be reported as silent,
        // not as unknowable, so an upgrade can replace such a session.
        assert_eq!(unit_plays_audio(Some(std::process::id())), Some(false));
        assert_eq!(unit_plays_audio(None), None);
        assert_eq!(unit_plays_audio(Some(u32::MAX)), None);
    }

    #[tokio::test]
    async fn session_start_requires_an_identity() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
            options.xpra_bin = fake_xpra(dir);
        })
        .await;
        let xpra = helper.options.xpra_bin.clone();
        let runtime = helper.options.run_dir.join("200007");
        let error = helper
            .call(NativeOp::SessionStart(SessionStartOp {
                user_id: 7,
                display: 100,
                port: 14500,
                xpra_bin: xpra.to_string_lossy().into_owned(),
                runtime_dir: runtime.to_string_lossy().into_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                memory_max_mb: 1024,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "identity_missing");
    }

    #[tokio::test]
    async fn session_start_rejects_bad_fields_before_running_anything() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            options.systemd_run_bin = systemd_run_script(dir);
            options.xpra_bin = fake_xpra(dir);
        })
        .await;
        let xpra = helper.options.xpra_bin.to_string_lossy().into_owned();
        let runtime = helper
            .options
            .run_dir
            .join("200007")
            .to_string_lossy()
            .into_owned();
        let base = |display: u32, port: u32, xpra_bin: String| SessionStartOp {
            user_id: 7,
            display,
            port,
            xpra_bin,
            runtime_dir: runtime.clone(),
            args: Vec::new(),
            env: BTreeMap::new(),
            memory_max_mb: 1024,
            cpu_quota_percent: 100,
            tasks_max: 128,
        };
        for op in [
            base(99, 14500, xpra.clone()),
            base(1000, 14500, xpra.clone()),
            base(100, 80, xpra.clone()),
            base(100, 14500, "/usr/bin/other".into()),
        ] {
            let error = helper.call(NativeOp::SessionStart(op)).await.unwrap_err();
            assert_eq!(error.code, "invalid_request", "{error}");
        }
        let log = helper
            .options
            .systemd_run_bin
            .parent()
            .unwrap()
            .join("systemd-run.log");
        assert!(logged_args(&log).is_empty(), "nothing must be started");
    }

    #[tokio::test]
    async fn app_start_conflicts_with_an_active_unit_and_stops_idempotently() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
            std::fs::write(dir.join("active-rast-app-demo.service"), b"x").unwrap();
        })
        .await;
        let cwd = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&cwd).unwrap();
        let error = helper
            .call(NativeOp::AppStart(AppStartOp {
                user_id: 7,
                app_id: "demo".into(),
                run_id: "abc123".into(),
                unit: "rast-app-demo".into(),
                argv: vec!["/usr/bin/true".into()],
                cwd: cwd.to_string_lossy().into_owned(),
                env: BTreeMap::new(),
                memory_max_mb: 512,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "unit_conflict");

        // Stopping a unit that is not loaded is idempotent success.
        let stopped = helper
            .call(NativeOp::AppStop(AppStopOp {
                unit: "rast-app-demo".into(),
            }))
            .await
            .unwrap();
        assert_eq!(stopped["stopped"], true);
        // session_stop derives its unit from userId.
        let stopped = helper
            .call(NativeOp::SessionStop(SessionStopOp { user_id: 7 }))
            .await
            .unwrap();
        assert_eq!(stopped["stopped"], true);
    }

    #[tokio::test]
    async fn app_start_rejects_dangerous_argv_and_paths() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            options.systemd_run_bin = systemd_run_script(dir);
        })
        .await;
        let cwd = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&cwd).unwrap();
        let base = AppStartOp {
            user_id: 7,
            app_id: "demo".into(),
            run_id: "abc123".into(),
            unit: "rast-app-demo".into(),
            argv: vec!["/usr/bin/true".into()],
            cwd: cwd.to_string_lossy().into_owned(),
            env: BTreeMap::new(),
            memory_max_mb: 512,
            cpu_quota_percent: 100,
            tasks_max: 128,
        };
        for (argv, code) in [
            (vec!["relative".to_string()], "invalid_request"),
            (vec!["/etc/passwd".to_string()], "forbidden"),
            (
                vec!["/usr/bin/true".to_string(), "a\0b".to_string()],
                "invalid_request",
            ),
        ] {
            let mut op = base.clone();
            op.argv = argv;
            let error = helper.call(NativeOp::AppStart(op)).await.unwrap_err();
            assert_eq!(error.code, code, "{error}");
        }
        let mut op = base.clone();
        op.unit = "rast-worker".into();
        let error = helper.call(NativeOp::AppStart(op)).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
        let mut op = base;
        op.cwd = "/etc".into();
        let error = helper.call(NativeOp::AppStart(op)).await.unwrap_err();
        assert_eq!(error.code, "forbidden");
    }

    #[tokio::test]
    async fn unit_status_parses_systemd_output() {
        let helper = TestHelper::start(|options, dir| {
            options.systemctl_bin = systemctl_script(dir);
            std::fs::write(dir.join("active-rast-xpra-u7.service"), b"x").unwrap();
        })
        .await;
        let status = helper
            .call(NativeOp::UnitStatus(UnitStatusOp {
                unit: "rast-xpra-u7".into(),
            }))
            .await
            .unwrap();
        assert_eq!(status["unit"], "rast-xpra-u7");
        assert_eq!(status["active"], true);
        assert_eq!(status["state"], "active");
        assert_eq!(status["mainPid"], 1);
        let missing = helper
            .call(NativeOp::UnitStatus(UnitStatusOp {
                unit: "rast-xpra-u8".into(),
            }))
            .await
            .unwrap();
        assert_eq!(missing["active"], false);
        assert_eq!(missing["state"], "inactive");
    }

    #[tokio::test]
    async fn units_list_filters_and_sorts_managed_units() {
        let helper = TestHelper::start(|options, dir| {
            options.systemctl_bin = systemctl_list_script(dir);
        })
        .await;
        let list = helper.call(NativeOp::UnitsList).await.unwrap();
        let units = list["units"].as_array().unwrap();
        let names: Vec<&str> = units
            .iter()
            .map(|item| item["unit"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["rast-app-demo", "rast-xpra-u7"]);
    }

    #[tokio::test]
    async fn unit_ops_refuse_foreign_units() {
        let helper = TestHelper::start(|options, dir| {
            options.systemctl_bin = systemctl_script(dir);
        })
        .await;
        for (unit, code) in [
            ("rast-worker", "forbidden"),
            ("rast.service", "invalid_request"),
            ("anything", "invalid_request"),
        ] {
            let error = helper
                .call(NativeOp::UnitStatus(UnitStatusOp {
                    unit: unit.to_string(),
                }))
                .await
                .unwrap_err();
            assert_eq!(error.code, code, "{unit}: {error}");
        }
    }

    fn write_test_deb(dir: &Path, package: &str, version: &str) -> PathBuf {
        // A real .deb is not required: the fake dpkg-deb script answers from
        // the file name.
        let deb = dir.join(format!("{package}_{version}_amd64.deb"));
        std::fs::write(&deb, b"fake deb payload").unwrap();
        deb
    }

    fn fake_dpkg_deb(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "dpkg-deb",
            r#"#!/bin/sh
case "$1" in
  --field)
    cat <<'EOF'
Package: google-chrome-stable
Version: 157.0.8103.78
Architecture: amd64
Depends: libc6 (>= 2.31), libx11-6
Conflicts: chromium
Description: The web browser from Google
 Long description.
EOF
    ;;
esac
exit 0
"#,
        )
    }

    fn fake_apt_get(dir: &Path, simulation: &str) -> PathBuf {
        let log = dir.join("apt-get.log");
        let body = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case " $* " in
  *" -s "*)
    cat <<'EOF'
{simulation}
EOF
    exit 0
    ;;
esac
echo "Setting up google-chrome-stable (157.0.8103.78) ..."
exit 0
"#,
            log = log.display(),
            simulation = simulation
        );
        write_script(dir, "apt-get", &body)
    }

    fn fake_apt_cache(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "apt-cache",
            r#"#!/bin/sh
cat <<'EOF'
google-chrome-stable:
  Installed: (none)
  Candidate: 157.0.8103.78
  Version table:
 *** 157.0.8103.78 500
        500 https://dl.google.com/linux/chrome/deb stable/main amd64 Packages
        100 /var/lib/dpkg/status
EOF
exit 0
"#,
        )
    }

    #[tokio::test]
    async fn deb_preview_reports_metadata_and_simulation() {
        let helper = TestHelper::start(|options, dir| {
            options.dpkg_deb_bin = fake_dpkg_deb(dir);
            options.apt_cache_bin = fake_apt_cache(dir);
            options.apt_get_bin = fake_apt_get(
                dir,
                "The following NEW packages will be installed:\n  libc6 libx11-6 google-chrome-stable\n\
                 Need to get 3,166 kB of archives.\n\
                 Inst libc6 (2.39-0ubuntu8 amd64 [1.2 MB])\n",
            );
        })
        .await;
        let staging = helper.options.linux_apps_dir().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = write_test_deb(&staging, "google-chrome-stable", "157.0.8103.78");
        let sha = sha256_file_blocking(&deb).unwrap();
        let preview = helper
            .call(NativeOp::DebPreview(DebPreviewOp {
                path: deb.to_string_lossy().into_owned(),
                sha256: sha,
            }))
            .await
            .unwrap();
        assert_eq!(preview["package"], "google-chrome-stable");
        assert_eq!(preview["version"], "157.0.8103.78");
        assert_eq!(preview["architecture"], "amd64");
        assert_eq!(preview["nativeArch"], native_dpkg_arch());
        assert_eq!(preview["compatible"], true);
        assert_eq!(preview["downloadBytes"], 3_166 * 1024);
        assert_eq!(preview["depends"][0], "libc6 (>= 2.31)");
        assert!(preview["error"].is_null());
        assert_eq!(preview["preexisting"], false);
        let installs = preview["installs"].as_array().unwrap();
        assert!(installs.iter().any(|item| item["name"] == "libc6"));
    }

    #[tokio::test]
    async fn deb_ops_reject_checksum_mismatch_and_outside_paths() {
        let helper = TestHelper::start(|options, dir| {
            options.dpkg_deb_bin = fake_dpkg_deb(dir);
            options.apt_get_bin = fake_apt_get(dir, "");
        })
        .await;
        let staging = helper.options.linux_apps_dir().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = write_test_deb(&staging, "demo", "1.0");
        let error = helper
            .call(NativeOp::DebPreview(DebPreviewOp {
                path: deb.to_string_lossy().into_owned(),
                sha256: "a".repeat(64),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "checksum_mismatch");
        let outside = helper.options.data_dir.join("outside.deb");
        std::fs::write(&outside, b"x").unwrap();
        let error = helper
            .call(NativeOp::DebPreview(DebPreviewOp {
                path: outside.to_string_lossy().into_owned(),
                sha256: sha256_file_blocking(&outside).unwrap(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "forbidden");
    }

    #[tokio::test]
    async fn deb_install_refuses_unapproved_removals() {
        let helper = TestHelper::start(|options, dir| {
            options.dpkg_deb_bin = fake_dpkg_deb(dir);
            options.apt_cache_bin = fake_apt_cache(dir);
            options.apt_get_bin = fake_apt_get(
                dir,
                "The following packages will be REMOVED:\n  chromium\n\
                 The following NEW packages will be installed:\n  google-chrome-stable\n\
                 0 upgraded, 1 newly installed, 1 to remove and 0 not upgraded.\n",
            );
        })
        .await;
        let staging = helper.options.linux_apps_dir().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = write_test_deb(&staging, "google-chrome-stable", "157.0.8103.78");
        let sha = sha256_file_blocking(&deb).unwrap();
        let error = helper
            .call(NativeOp::DebInstall(DebInstallOp {
                path: deb.to_string_lossy().into_owned(),
                sha256: sha,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "unsafe_removal");
        assert!(error.message.contains("chromium"), "{error}");
        let apt_log = helper
            .options
            .apt_get_bin
            .parent()
            .unwrap()
            .join("apt-get.log");
        let log = std::fs::read_to_string(&apt_log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1, "only the simulation may run: {lines:?}");
        assert!(lines[0].contains(" -s "), "simulation flags: {}", lines[0]);
    }

    #[tokio::test]
    async fn deb_remove_refuses_to_remove_unrequested_packages() {
        let helper = TestHelper::start(|options, dir| {
            options.apt_get_bin = fake_apt_get(
                dir,
                "The following packages will be REMOVED:\n  bash curl\n\
                 0 upgraded, 0 newly installed, 2 to remove and 0 not upgraded.\n\
                 Remv bash [5.2]\nRemv curl [8.5]\n",
            );
        })
        .await;
        let error = helper
            .call(NativeOp::DebRemove(DebRemoveOp {
                packages: vec!["bash".into()],
                purge: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "unsafe_removal");
        assert!(error.message.contains("curl"), "{error}");
    }

    #[tokio::test]
    async fn deb_download_returns_the_new_file() {
        let helper = TestHelper::start(|options, dir| {
            options.dpkg_deb_bin = fake_dpkg_deb(dir);
            options.apt_get_bin = write_script(
                dir,
                "apt-get",
                &format!(
                    r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "download" ]; then
  echo payload > "{data}/linux-apps/staging/deb/google-chrome-stable_157_amd64.deb"
  exit 0
fi
exit 1
"#,
                    log = dir.join("apt-get.log").display(),
                    data = dir.join("data").display()
                ),
            );
        })
        .await;
        let dest = helper.options.linux_apps_dir().join("staging/deb");
        std::fs::create_dir_all(&dest).unwrap();
        let result = helper
            .call(NativeOp::DebDownload(DebDownloadOp {
                package: "google-chrome-stable".into(),
                dest: dest.to_string_lossy().into_owned(),
                version: Some("157.0".into()),
            }))
            .await
            .unwrap();
        assert_eq!(result["package"], "google-chrome-stable");
        assert_eq!(result["architecture"], "amd64");
        assert!(result["sha256"].as_str().unwrap().len() == 64);
        assert!(PathBuf::from(result["path"].as_str().unwrap()).is_file());
    }

    #[tokio::test]
    async fn path_operations_stay_inside_managed_roots() {
        let helper = TestHelper::start(|_, _| {}).await;
        let target = helper.options.linux_users_dir().join("7/data");
        let created = helper
            .call(NativeOp::PathMkdir(PathMkdirOp {
                path: target.to_string_lossy().into_owned(),
                uid: 200_007,
                gid: 200_007,
                mode: 0o700,
            }))
            .await
            .unwrap();
        assert_eq!(created["ok"], true);
        assert!(target.is_dir());
        let error = helper
            .call(NativeOp::PathMkdir(PathMkdirOp {
                path: "/etc/rast-evil".into(),
                uid: 200_007,
                gid: 200_007,
                mode: 0o700,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "forbidden");
        let error = helper
            .call(NativeOp::PathMkdir(PathMkdirOp {
                path: format!("{}/../escape", helper.options.linux_users_dir().display()),
                uid: 200_007,
                gid: 200_007,
                mode: 0o700,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_request");
        let removed = helper
            .call(NativeOp::PathRemove(PathRemoveOp {
                path: target.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap();
        assert_eq!(removed["ok"], true);
        assert!(!target.exists());
        // Missing paths are ignored on removal (idempotent cleanup).
        let removed = helper
            .call(NativeOp::PathRemove(PathRemoveOp {
                path: target.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap();
        assert_eq!(removed["ok"], true);
    }

    #[tokio::test]
    async fn path_acl_needs_setfacl() {
        let helper = TestHelper::start(|_, _| {}).await;
        let home = helper.options.homes_dir.join("alice");
        let error = helper
            .call(NativeOp::PathAcl(PathAclOp {
                path: home.to_string_lossy().into_owned(),
                uid: 200_007,
                perms: "rwx".into(),
                recursive: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "acls_unavailable");
    }

    #[tokio::test]
    async fn profile_prepare_and_remove() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
        })
        .await;
        let home = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&home).unwrap();
        let rast_home = helper.options.homes_dir.join("alice");
        // Simulate the production failure: a non-traversable ancestor that a
        // previous helper/installer version might have left behind.
        std::fs::set_permissions(
            helper.options.linux_apps_dir(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let prepared = helper
            .call(NativeOp::ProfilePrepare(ProfilePrepareOp {
                app_id: "demo".into(),
                user_id: 7,
                uid: 200_007,
                gid: 200_007,
                home: home.to_string_lossy().into_owned(),
                rast_home: rast_home.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap();
        let profile = PathBuf::from(prepared["profile"].as_str().unwrap());
        assert_eq!(profile, helper.options.profiles_dir().join("demo/7"));
        assert!(profile.join("home").is_dir());
        assert!(profile.join("config").is_dir());
        assert!(profile.join("tmp").is_dir());
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&helper.options.linux_apps_dir()), 0o751);
        assert_eq!(mode(&helper.options.profiles_dir()), 0o711);
        assert_eq!(mode(&helper.options.profiles_dir().join("demo")), 0o711);
        assert_eq!(mode(&profile), 0o700);
        assert_eq!(mode(&profile.join("home")), 0o700);
        let removed = helper
            .call(NativeOp::ProfileRemove(ProfileRemoveOp {
                app_id: "demo".into(),
                user_id: 7,
            }))
            .await
            .unwrap();
        assert_eq!(removed["removed"], true);
        assert!(!profile.exists());
        let bad_uid = helper
            .call(NativeOp::ProfilePrepare(ProfilePrepareOp {
                app_id: "demo".into(),
                user_id: 7,
                uid: 1,
                gid: 1,
                home: home.to_string_lossy().into_owned(),
                rast_home: rast_home.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(bad_uid.code, "forbidden");
    }

    #[tokio::test]
    async fn app_start_refuses_inaccessible_cwd_before_systemd() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemd_run_bin = systemd_run_script(dir);
        })
        .await;
        use std::os::unix::fs::PermissionsExt;
        let cwd = helper.options.linux_users_dir().join("7").join("private");
        std::fs::create_dir_all(&cwd).unwrap();
        // Owner without execute: chdir by the identity must be refused with a
        // stable, actionable code instead of systemd's 200/CHDIR.
        std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = helper
            .call(NativeOp::AppStart(AppStartOp {
                user_id: 7,
                app_id: "demo".into(),
                run_id: "abc123".into(),
                unit: "rast-app-demo".into(),
                argv: vec!["/usr/bin/true".into()],
                cwd: cwd.to_string_lossy().into_owned(),
                env: BTreeMap::new(),
                memory_max_mb: 512,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "cwd_inaccessible", "{error}");
        assert!(error.message.contains("private"), "{error}");
        let log = helper
            .options
            .systemd_run_bin
            .parent()
            .unwrap()
            .join("systemd-run.log");
        assert!(logged_args(&log).is_empty(), "nothing may be started");
    }

    #[tokio::test]
    async fn stale_session_auth_produces_an_actionable_error() {
        use std::os::unix::fs::PermissionsExt;
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
        })
        .await;
        let home = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&home).unwrap();
        let xauth = home.join(".Xauthority");
        std::fs::write(&xauth, b"auth-cookie").unwrap();
        std::fs::set_permissions(&xauth, std::fs::Permissions::from_mode(0o000)).unwrap();
        let request = |cwd: &Path| AppStartOp {
            user_id: 7,
            app_id: "demo".into(),
            run_id: "abc123".into(),
            unit: "rast-app-demo".into(),
            argv: vec!["/usr/bin/true".into()],
            cwd: cwd.to_string_lossy().into_owned(),
            env: BTreeMap::new(),
            memory_max_mb: 512,
            cpu_quota_percent: 100,
            tasks_max: 128,
        };
        let error = helper
            .call(NativeOp::AppStart(request(&home)))
            .await
            .unwrap_err();
        assert_eq!(error.code, "xauthority_unavailable", "{error}");
        assert!(error.message.contains(".Xauthority"), "{error}");
        let log = helper
            .options
            .systemd_run_bin
            .parent()
            .unwrap()
            .join("systemd-run.log");
        assert!(logged_args(&log).is_empty(), "nothing may be started");

        // A readable auth file lets the launch proceed (the session manager
        // restarts genuinely stale sessions; the helper only guards the file).
        std::fs::set_permissions(&xauth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let started = helper
            .call(NativeOp::AppStart(request(&home)))
            .await
            .unwrap();
        assert_eq!(started["active"], true);
    }

    #[tokio::test]
    async fn session_start_repairs_a_mangled_identity_home() {
        use std::os::unix::fs::PermissionsExt;
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
            options.xpra_bin = fake_xpra(dir);
        })
        .await;
        let home = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777)).unwrap();
        let started = helper
            .call(NativeOp::SessionStart(SessionStartOp {
                user_id: 7,
                display: 102,
                port: 14_500,
                xpra_bin: helper.options.xpra_bin.to_string_lossy().into_owned(),
                runtime_dir: helper
                    .options
                    .run_dir
                    .join("200007")
                    .to_string_lossy()
                    .into_owned(),
                args: vec!["start".into()],
                env: BTreeMap::new(),
                memory_max_mb: 1024,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap();
        assert_eq!(started["active"], true);
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700,
            "the helper must repair the identity home before starting Xpra"
        );
    }

    #[tokio::test]
    async fn failed_transient_units_are_reset_and_reused() {
        let helper = TestHelper::start(|options, dir| {
            configure_identity(options, dir);
            std::fs::write(dir.join("user.exists"), b"x").unwrap();
            std::fs::write(dir.join("group.exists"), b"x").unwrap();
            options.systemctl_bin = systemctl_script(dir);
            options.systemd_run_bin = systemd_run_script(dir);
            // /usr/bin/xpra does not exist on the CI runners.
            options.xpra_bin = fake_xpra(dir);
            std::fs::write(dir.join("failed-rast-app-demo.service"), b"x").unwrap();
            std::fs::write(dir.join("failed-rast-xpra-u7.service"), b"x").unwrap();
        })
        .await;
        let cwd = helper.options.linux_users_dir().join("7");
        std::fs::create_dir_all(&cwd).unwrap();
        let started = helper
            .call(NativeOp::AppStart(AppStartOp {
                user_id: 7,
                app_id: "demo".into(),
                run_id: "abc123".into(),
                unit: "rast-app-demo".into(),
                argv: vec!["/usr/bin/true".into()],
                cwd: cwd.to_string_lossy().into_owned(),
                env: BTreeMap::new(),
                memory_max_mb: 512,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap();
        assert_eq!(started["active"], true);

        let session = helper
            .call(NativeOp::SessionStart(SessionStartOp {
                user_id: 7,
                display: 102,
                port: 14_500,
                xpra_bin: helper.options.xpra_bin.to_string_lossy().into_owned(),
                runtime_dir: helper
                    .options
                    .run_dir
                    .join("200007")
                    .to_string_lossy()
                    .into_owned(),
                args: vec!["start".into()],
                env: BTreeMap::new(),
                memory_max_mb: 1024,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }))
            .await
            .unwrap();
        assert_eq!(session["active"], true);
        assert!(!helper
            .options
            .systemctl_bin
            .parent()
            .unwrap()
            .join("failed-rast-xpra-u7.service")
            .exists());
    }

    #[tokio::test]
    async fn apt_lock_serializes_install_transactions() {
        let helper = TestHelper::start(|options, dir| {
            options.dpkg_deb_bin = fake_dpkg_deb(dir);
            options.apt_cache_bin = fake_apt_cache(dir);
            let log = dir.join("apt-get.log");
            options.apt_get_bin = write_script(
                dir,
                "apt-get",
                &format!(
                    r#"#!/bin/sh
case " $* " in
  *" -s "*) exit 0 ;;
esac
printf 'START\n' >> "{log}"
sleep 0.15
printf 'END\n' >> "{log}"
exit 0
"#,
                    log = log.display()
                ),
            );
        })
        .await;
        let staging = helper.options.linux_apps_dir().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = write_test_deb(&staging, "demo", "1.0");
        let sha = sha256_file_blocking(&deb).unwrap();
        let client = helper.client();
        let mut tasks = Vec::new();
        for id in 0..2 {
            let client = client.clone();
            let path = deb.to_string_lossy().into_owned();
            let sha = sha.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .call(NativeRequest::new(
                        id + 1,
                        NativeOp::DebInstall(DebInstallOp { path, sha256: sha }),
                    ))
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let log = helper
            .options
            .apt_get_bin
            .parent()
            .unwrap()
            .join("apt-get.log");
        let lines: Vec<String> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines, vec!["START", "END", "START", "END"]);
    }

    #[test]
    fn apt_simulation_parsing_handles_real_world_output() {
        let text = "Reading package lists...\n\
                    The following additional packages will be installed:\n  libc6 libx11-6\n\
                    The following NEW packages will be installed:\n  libc6 libx11-6 google-chrome-stable\n\
                    The following packages will be REMOVED:\n  chromium\n\
                    0 upgraded, 2 newly installed, 1 to remove and 0 not upgraded.\n\
                    Need to get 3,166 kB of archives.\n\
                    After this operation, 200 MB of additional disk space will be used.\n\
                    Inst libc6 (2.39-0ubuntu8 amd64 [1.2 MB])\n\
                    Inst google-chrome-stable [157.0] (158.0 stable [amd64])\n\
                    Remv chromium [120.0]\n";
        let sim = parse_apt_simulation(text);
        assert!(sim.installs.iter().any(|(name, _)| name == "libc6"));
        assert!(sim
            .installs
            .iter()
            .any(|(name, version)| name == "google-chrome-stable" && version == "158.0"));
        assert_eq!(sim.removals, vec!["chromium"]);
        assert_eq!(sim.download_bytes, Some(3_166 * 1024));
    }

    #[test]
    fn apt_cache_policy_and_control_files_are_parsed() {
        let policy = parse_apt_cache_policy(
            "google-chrome-stable:\n  Installed: (none)\n  Candidate: 157.0\n  Version table:\n\
             *** 157.0 500\n        500 https://dl.google.com/linux/chrome/deb stable/main amd64 Packages\n\
                100 /var/lib/dpkg/status\n",
        );
        assert_eq!(policy.installed, None);
        assert_eq!(policy.candidate.as_deref(), Some("157.0"));
        assert_eq!(
            policy.repository.as_deref(),
            Some("dl.google.com stable/main")
        );
        let control = parse_control_file(
            "Package: demo\nVersion: 1.0\nDescription: Hello\n Long line\nDepends: a, b\n",
        );
        assert_eq!(control.get("Package").unwrap(), "demo");
        assert_eq!(control.get("Description").unwrap(), "Hello\nLong line");
        assert_eq!(split_dpkg_field(control.get("Depends")), vec!["a", "b"]);
    }

    #[test]
    fn unit_show_parsing_handles_blocks() {
        let raw = parse_systemctl_show(
            "Id=rast-xpra-u7.service\nActiveState=active\nLoadState=loaded\nMainPID=1234\n\n\
             Id=rast-app-demo.service\nActiveState=failed\nLoadState=loaded\nMainPID=0\n",
        );
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].id.as_deref(), Some("rast-xpra-u7"));
        assert_eq!(raw[0].main_pid, Some(1234));
        let info = raw[1].to_info("fallback");
        assert_eq!(info.state, "failed");
        assert_eq!(info.main_pid, None);
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("x");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_file_blocking(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn host_arch_mapping_is_a_dpkg_name() {
        let arch = native_dpkg_arch();
        assert!(!arch.is_empty());
        assert!(arch
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));
    }

    #[test]
    fn socket_preparation_refuses_to_delete_non_sockets() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("nested/native.sock");
        prepare_socket_path(&socket).unwrap();
        assert!(!socket.exists());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"not a socket").unwrap();
        assert!(prepare_socket_path(&socket).is_err());
    }
}
