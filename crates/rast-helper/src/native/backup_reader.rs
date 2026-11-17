//! The backup reader (ADR 0005, D12): a root process that streams one HOME
//! or one shared folder container to Backup Center as a tar archive, with
//! numeric owners, modes and POSIX ACLs.
//!
//! The `rast` service cannot read a HOME in one-HOME mode (`rast-u<id>`,
//! 0700) or everything in a migrated shared folder (`root:rast-s<h>-rw`
//! 2770, recycle bins of the members), and it must not get a general root
//! file reader either. So:
//!
//! * `backup_reader_start` (no fields, [`backup_reader_start`]) starts the
//!   socket-activated transient unit `rast-backup-reader` with the fixed
//!   homes root and shared root of the helper's configuration. systemd
//!   creates the socket (`root:rast 0660`) in a `root:rast 0750` folder; the
//!   reader gets its token through `LoadCredential=`, checks the peer uid
//!   (the `rast` service) and the token of every connection, and exits when
//!   idle. The unit is read-only on the whole file system
//!   (`ProtectSystem=strict`, `ProtectHome=read-only`), has no network and
//!   keeps only `CAP_DAC_READ_SEARCH`.
//! * One connection reads one tree. The request is one JSON line
//!   ([`ReaderRequest`]: the token, `home` with a username or `shared` with
//!   a container name; nothing else, no path). The reader opens the folder
//!   directly below its root without following a link, on the root's file
//!   system and mount, and only when it is in the new layout: a HOME owned
//!   by a RAST identity, a container owned by root with a shared folder
//!   group. Legacy folders belong to the service, which reads them itself.
//! * The reply is one JSON line ([`ReaderReply`]), then the tar stream of
//!   [`archive`] (the walk of [`walk`]), then one JSON trailer line
//!   ([`Trailer`]) with the counts and the first problems, then the end of
//!   the connection.
//! * Below the tree's root, the walk stores only entries of the owners the
//!   tree may hold ([`OwnerRules::entry_owners`]): in a HOME its identity
//!   and the service, in a container the RAST identities and the service.
//!   Anything else (a hard link to a root file, with
//!   `fs.protected_hardlinks = 0`) is left out as `otherOwner`. A file with
//!   more holes than the tree's budget ([`walk::MAX_TREE_HOLES`]) is left
//!   out as `sparse`: quota counts blocks, the archive would count bytes.
//! * The service can read every HOME in one-HOME mode and every migrated
//!   container through the reader (ADR 0005, D5 and section 14); only the
//!   server's plan applies the native pilot. Every tree served is logged
//!   (target `rast::backup_reader::audit`, the reader unit's journal) with
//!   its kind and name, so reads can be audited.

pub mod archive;
mod walk;

pub use walk::{excluded, MAX_DEPTH, MAX_TREE_HOLES, MAX_TREE_PATH};

use std::ffi::{CString, OsString};
use std::io::{self, BufWriter, Read, Write};
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native::daemon::{random_token, run_systemd_unit, stop_unit, unit_active};
use crate::native::homes::{fstat, is_dir, is_symlink, open_dir, DIR_FLAGS};
use crate::native::protocol::{validate_home_username, validate_share_container, NativeError};
use crate::native::shares::{
    dev_of, fd_mount_id, identity_uid_range, lstat_at, open_at, same_inode, service_uid,
    share_gid_range,
};
use crate::native::NativeOptions;

/// The reader's unit (`.socket` and `.service`).
pub const UNIT: &str = "rast-backup-reader";
/// The reader exits after this long without a connection (systemd starts
/// it again on the next one).
pub const IDLE_SECONDS: u64 = 300;
/// Memory and task limits of the reader.
pub const MEMORY_MAX: &str = "256M";
pub const TASKS_MAX: u32 = 16;
/// Longest request line.
pub const MAX_REQUEST_LINE: usize = 4096;
/// Longest reply line.
pub const MAX_REPLY_LINE: usize = 4096;
/// Longest trailer line.
pub const MAX_TRAILER_LINE: usize = 64 * 1024;
/// Problems listed in a trailer at most, and their length in characters.
pub const MAX_PROBLEMS: usize = 20;
pub const MAX_PROBLEM_CHARS: usize = 300;
/// How long the reader waits for the request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A write that blocks this long (the server stopped reading) ends the
/// connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// Which tree a request names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeKind {
    /// A HOME: `<homes>/<username>`.
    Home,
    /// A shared folder container: `<shared>/<container>`.
    Shared,
}

impl TreeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Shared => "shared",
        }
    }
}

/// The request line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderRequest {
    pub token: String,
    pub tree: TreeKind,
    /// A username (`home`) or a container name (`shared`).
    pub name: String,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyError {
    pub code: String,
    pub message: String,
}

impl ReplyError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// The reply line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReplyError>,
}

/// The trailer line after the archive: what was read and what was not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Trailer {
    /// The walk reached its end.
    pub done: bool,
    pub entries: u64,
    pub files: u64,
    pub bytes: u64,
    pub hard_links: u64,
    /// Left out on purpose (caches, recycle bins, Flatpak).
    pub excluded: u64,
    /// Entries on another file system or mount, left out.
    pub other_file_systems: u64,
    /// FIFOs, sockets and devices, left out.
    pub special: u64,
    /// Gone between the listing and the read.
    pub vanished: u64,
    pub unreadable: u64,
    /// Swapped or shortened while they were read.
    pub changed: u64,
    pub too_deep: u64,
    pub too_long: u64,
    /// Owned by an account the tree may not hold (review F6), left out.
    #[serde(default)]
    pub other_owner: u64,
    /// Sparse files beyond the tree's hole budget (review F7), left out.
    #[serde(default)]
    pub sparse: u64,
    /// The first [`MAX_PROBLEMS`] problems, `<path>: <reason>`.
    pub problems: Vec<String>,
}

/// Who owns the folders the reader may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRules {
    /// The uids of RAST identities (a HOME in one-HOME mode).
    pub home_owners: RangeInclusive<u32>,
    /// The owner of a migrated container (root).
    pub container_owner: u32,
    /// The gids of shared folder groups (the group of a migrated
    /// container).
    pub container_groups: RangeInclusive<u32>,
    /// The `rast` service, which may own entries in both (`None`: only the
    /// identities).
    pub service: Option<u32>,
}

impl OwnerRules {
    /// The rules of `linux_uid_base` and the service uid.
    pub fn for_uid_base(uid_base: u32, service: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            home_owners: identity_uid_range(uid_base),
            container_owner: 0,
            container_groups: share_gid_range(uid_base).map_err(|error| error.message)?,
            service,
        })
    }

    /// Who may own the entries below a tree's root (review F6): in a HOME
    /// its own identity (`root_uid`, the HOME's owner) and the service; in
    /// a shared folder container every RAST identity and the service.
    pub fn entry_owners(&self, kind: TreeKind, root_uid: u32) -> Vec<RangeInclusive<u32>> {
        let tree = match kind {
            TreeKind::Home => root_uid..=root_uid,
            TreeKind::Shared => self.home_owners.clone(),
        };
        std::iter::once(tree)
            .chain(self.service.map(|service| service..=service))
            .collect()
    }
}

/// Where the reader listens.
pub enum Listen {
    /// The socket systemd passed (`LISTEN_FDS`, socket activation).
    Systemd,
    /// Bind this path (development and tests).
    Path(PathBuf),
}

/// Options of one reader run.
pub struct ReaderOptions {
    /// `<storage_dir>/homes`.
    pub homes: PathBuf,
    /// `<storage_dir>/shared`.
    pub shared: PathBuf,
    pub rules: OwnerRules,
    pub token: String,
    /// The only uid allowed to connect (the `rast` service).
    pub peer_uid: Option<u32>,
    pub idle: Duration,
    pub listen: Listen,
    /// Test-only: every audit record ([`audit`]) is also kept here.
    #[cfg(test)]
    pub test_audit: Option<AuditLog>,
}

/// Test-only record of the trees a reader served.
#[cfg(test)]
pub type AuditLog = std::sync::Arc<std::sync::Mutex<Vec<(TreeKind, String)>>>;

// ---------------------------------------------------------------------------
// The reader process (`rast backup-reader`)
// ---------------------------------------------------------------------------

/// The token from `--token-file`, or from the systemd credential `token`.
pub fn token_from(token_file: Option<&Path>) -> io::Result<String> {
    let path = match token_file {
        Some(path) => path.to_path_buf(),
        None => {
            let dir = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no backup reader token credential")
            })?;
            Path::new(&dir).join("token")
        }
    };
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the backup reader token is missing or too short",
        ));
    }
    Ok(token)
}

/// The listening socket systemd passed to this process.
fn systemd_listener() -> io::Result<UnixListener> {
    use std::os::fd::FromRawFd;
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
        == Some(std::process::id());
    let fds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|fds| fds.parse::<u32>().ok())
        .unwrap_or(0);
    if !pid_ok || fds != 1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no socket was passed by systemd (LISTEN_FDS)",
        ));
    }
    // SAFETY: systemd passes exactly one listening socket as fd 3
    // (SD_LISTEN_FDS_START); this process owns it from here on.
    let listener = unsafe { UnixListener::from_raw_fd(3) };
    for name in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: called once at start-up, before other threads read the
        // environment.
        unsafe { std::env::remove_var(name) };
    }
    Ok(listener)
}

/// Wait until `fd` has a connection, at most `timeout`. False: none came.
fn wait_readable(fd: i32, timeout: Duration) -> io::Result<bool> {
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    loop {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut poll, 1, millis) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(rc > 0);
    }
}

/// The uid of the process at the other end of `stream` (`SO_PEERCRED`).
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    // SAFETY: zeroed `ucred` is a valid out parameter.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid socket, out pointer and length.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Run the reader until it has been idle for `options.idle`.
pub fn run(options: ReaderOptions) -> io::Result<()> {
    let (listener, bound) = match &options.listen {
        Listen::Systemd => (systemd_listener()?, None),
        Listen::Path(path) => {
            let _ = std::fs::remove_file(path);
            (UnixListener::bind(path)?, Some(path.clone()))
        }
    };
    listener.set_nonblocking(false)?;
    loop {
        if !wait_readable(listener.as_raw_fd(), options.idle)? {
            break;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(error) => {
                tracing::warn!(%error, "backup reader cannot accept a connection");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let allowed = match (options.peer_uid, peer_uid(&stream)) {
            (None, _) => true,
            (Some(expected), Ok(uid)) => uid == expected,
            (Some(_), Err(_)) => false,
        };
        if !allowed {
            tracing::warn!("backup reader refused a connection from another account");
            continue;
        }
        if let Err(error) = serve(&stream, &options) {
            tracing::warn!(%error, "backup reader connection ended early");
        }
    }
    if let Some(path) = bound {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Read one line (without its newline), at most `limit` bytes.
pub fn read_line<R: Read + ?Sized>(input: &mut R, limit: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the line ended without a newline",
                ))
            }
            Ok(_) if byte[0] == b'\n' => return Ok(line),
            Ok(_) => {
                if line.len() >= limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("the line is longer than {limit} bytes"),
                    ));
                }
                line.push(byte[0]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_line<W: Write, T: Serialize>(out: &mut W, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    out.write_all(&line)
}

fn same_token(presented: &[u8], expected: &[u8]) -> bool {
    presented.len() == expected.len()
        && presented
            .iter()
            .zip(expected)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Parse and check a request line: strict JSON, the token, a valid
/// username or container name.
pub fn check_request(line: &[u8], token: &str) -> Result<(TreeKind, String), ReplyError> {
    let request: ReaderRequest = serde_json::from_slice(line)
        .map_err(|error| ReplyError::new("invalid_request", format!("invalid request: {error}")))?;
    if !same_token(request.token.as_bytes(), token.as_bytes()) {
        return Err(ReplyError::new("forbidden", "invalid backup reader token"));
    }
    match request.tree {
        TreeKind::Home => validate_home_username(&request.name),
        TreeKind::Shared => validate_share_container(&request.name),
    }
    .map_err(|error| ReplyError::new("invalid_request", error.message))?;
    Ok((request.tree, request.name))
}

/// Open the tree a checked request names: a real folder directly below the
/// homes or shared root (never a link), on the root's file system and
/// mount, in the new layout.
pub fn open_tree(
    options: &ReaderOptions,
    kind: TreeKind,
    name: &str,
) -> Result<OwnedFd, ReplyError> {
    let root_path = match kind {
        TreeKind::Home => &options.homes,
        TreeKind::Shared => &options.shared,
    };
    let io = |what: &str, error: io::Error| {
        ReplyError::new(
            "io",
            format!("cannot {what} {}: {error}", root_path.display()),
        )
    };
    let root = open_dir(root_path).map_err(|error| io("open", error))?;
    let root_stat = fstat(root.as_raw_fd()).map_err(|error| io("inspect", error))?;
    let c_name =
        CString::new(name).map_err(|_| ReplyError::new("invalid_request", "invalid name"))?;
    let stat = match lstat_at(root.as_raw_fd(), &c_name) {
        Ok(stat) => stat,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            return Err(ReplyError::new(
                "not_found",
                format!("{name} does not exist"),
            ))
        }
        Err(error) => return Err(io("inspect a folder in", error)),
    };
    if is_symlink(&stat) {
        return Err(ReplyError::new(
            "forbidden",
            format!("{name} is a symbolic link; it is never followed"),
        ));
    }
    if !is_dir(&stat) {
        return Err(ReplyError::new(
            "not_found",
            format!("{name} is not a folder"),
        ));
    }
    let fd = open_at(root.as_raw_fd(), &c_name, DIR_FLAGS).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            ReplyError::new(
                "forbidden",
                format!("{name} is a symbolic link; it is never followed"),
            )
        } else {
            io("open a folder in", error)
        }
    })?;
    let opened = fstat(fd.as_raw_fd()).map_err(|error| io("inspect a folder in", error))?;
    if !same_inode(&stat, &opened) {
        return Err(ReplyError::new(
            "forbidden",
            format!("{name} changed while it was opened"),
        ));
    }
    let other_mount = match (fd_mount_id(fd.as_raw_fd()), fd_mount_id(root.as_raw_fd())) {
        (Ok(mine), Ok(root)) => mine != root,
        _ => false,
    };
    if dev_of(&opened) != dev_of(&root_stat) || other_mount {
        return Err(ReplyError::new(
            "other_file_system",
            format!("{name} is on another file system or mount"),
        ));
    }
    match kind {
        TreeKind::Home if !options.rules.home_owners.contains(&opened.st_uid) => {
            Err(ReplyError::new(
                "not_one_home",
                format!(
                    "the HOME {name} is not in one-HOME mode (owner uid {}); the service reads it itself",
                    opened.st_uid
                ),
            ))
        }
        TreeKind::Shared
            if opened.st_uid != options.rules.container_owner
                || !options.rules.container_groups.contains(&opened.st_gid) =>
        {
            Err(ReplyError::new(
                "legacy_container",
                format!(
                    "the shared folder {name} is not in the group layout (owner {}:{}); the service reads it itself",
                    opened.st_uid, opened.st_gid
                ),
            ))
        }
        _ => Ok(fd),
    }
}

/// Record that the reader serves `name` to the service (review F12): one
/// record per tree, before its first byte, in the reader unit's journal.
fn audit(options: &ReaderOptions, kind: TreeKind, name: &str) {
    tracing::info!(
        target: "rast::backup_reader::audit",
        tree = kind.as_str(),
        %name,
        peer_uid = ?options.peer_uid,
        "backup reader serves a tree to the RAST service"
    );
    #[cfg(test)]
    if let Some(log) = &options.test_audit {
        log.lock().unwrap().push((kind, name.to_string()));
    }
}

/// One connection: the request, the reply, the archive and the trailer.
fn serve(stream: &UnixStream, options: &ReaderOptions) -> io::Result<()> {
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut input = stream;
    let mut out = BufWriter::with_capacity(64 * 1024, stream);
    let checked = match read_line(&mut input, MAX_REQUEST_LINE) {
        Ok(line) => check_request(&line, &options.token),
        Err(error) => Err(ReplyError::new(
            "invalid_request",
            format!("invalid request: {error}"),
        )),
    }
    .and_then(|(kind, name)| open_tree(options, kind, &name).map(|fd| (kind, name, fd)));
    let (kind, name, root) = match checked {
        Ok(found) => found,
        Err(error) => {
            tracing::info!(code = %error.code, message = %error.message, "backup reader refused a request");
            write_line(
                &mut out,
                &ReaderReply {
                    ok: false,
                    error: Some(error),
                },
            )?;
            return out.flush();
        }
    };
    audit(options, kind, &name);
    write_line(
        &mut out,
        &ReaderReply {
            ok: true,
            error: None,
        },
    )?;
    let summary = walk::stream_tree(&mut out, kind, root, &options.rules)?;
    archive::write_end(&mut out)?;
    write_line(&mut out, &summary)?;
    out.flush()?;
    tracing::info!(
        tree = kind.as_str(),
        %name,
        entries = summary.entries,
        bytes = summary.bytes,
        other_owner = summary.other_owner,
        sparse = summary.sparse,
        problems = summary.problems.len(),
        "backup reader streamed a tree"
    );
    Ok(())
}

/// Bytes available to unprivileged users on the file system that holds
/// `path` (`None`: it cannot be measured). Backup Center keeps a floor of
/// free space with it (review F7).
pub fn available_bytes(path: &Path) -> Option<u64> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a zeroed `statvfs` is a valid out parameter.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated path and a valid out parameter.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

// ---------------------------------------------------------------------------
// backup_reader_start (helper operation)
// ---------------------------------------------------------------------------

fn reader_dir(options: &NativeOptions) -> PathBuf {
    options.run_dir.join("backup-reader")
}

/// `<run_dir>/backup-reader`, `root:rast 0750`: the service group reaches
/// the socket, nobody else does.
fn prepare_dir(options: &NativeOptions, gid: u32) -> Result<PathBuf, NativeError> {
    use std::os::unix::fs::PermissionsExt;
    let dir = reader_dir(options);
    let failed = |what: &str, error: io::Error| {
        NativeError::command_failed(format!("cannot {what} {}: {error}", dir.display()))
    };
    std::fs::create_dir_all(&dir).map_err(|error| failed("create", error))?;
    let meta = std::fs::symlink_metadata(&dir).map_err(|error| failed("inspect", error))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(NativeError::forbidden(
            "the backup reader runtime directory must be a real directory",
        ));
    }
    if !options.is_test_mode() {
        std::os::unix::fs::chown(&dir, Some(0), Some(gid))
            .map_err(|error| failed("chown", error))?;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))
        .map_err(|error| failed("chmod", error))?;
    Ok(dir)
}

fn read_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (token.len() >= 32 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_string())
}

fn write_token(path: &Path, token: &str) -> Result<(), NativeError> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let failed = |error: io::Error| {
        NativeError::command_failed(format!("cannot write {}: {error}", path.display()))
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(failed)?;
    file.write_all(token.as_bytes()).map_err(failed)
}

/// The `systemd-run` arguments of the reader unit.
#[allow(clippy::too_many_arguments)]
pub fn reader_args(
    socket: &Path,
    socket_gid: u32,
    token_file: &Path,
    exe: &Path,
    homes: &Path,
    shared: &Path,
    uid_base: u32,
    peer_uid: u32,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |value: String| args.push(OsString::from(value));
    push(format!("--unit={UNIT}"));
    push("--collect".into());
    push("--quiet".into());
    // systemd (root) creates the socket; the reader inherits it.
    push(format!(
        "--socket-property=ListenStream={}",
        socket.display()
    ));
    push("--socket-property=SocketUser=root".into());
    push(format!("--socket-property=SocketGroup={socket_gid}"));
    push("--socket-property=SocketMode=0660".into());
    push("--socket-property=RemoveOnStop=yes".into());
    push("--property=Type=exec".into());
    push("--property=WorkingDirectory=/".into());
    push("--property=UMask=0077".into());
    push(format!(
        "--property=LoadCredential=token:{}",
        token_file.display()
    ));
    // Read-only everywhere, no network, and of root's powers only reading.
    for property in [
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=read-only",
        "PrivateTmp=yes",
        "PrivateDevices=yes",
        "PrivateNetwork=yes",
        "RestrictAddressFamilies=AF_UNIX",
        "CapabilityBoundingSet=CAP_DAC_READ_SEARCH",
        "AmbientCapabilities=",
        "ProtectKernelTunables=yes",
        "ProtectKernelModules=yes",
        "ProtectControlGroups=yes",
    ] {
        push(format!("--property={property}"));
    }
    push(format!("--property=MemoryMax={MEMORY_MAX}"));
    push(format!("--property=TasksMax={TASKS_MAX}"));
    push("--property=KillMode=control-group".into());
    push("--".into());
    args.push(exe.as_os_str().to_owned());
    for value in [
        "backup-reader".to_string(),
        "--homes".into(),
        homes.display().to_string(),
        "--shared".into(),
        shared.display().to_string(),
        "--uid-base".into(),
        uid_base.to_string(),
        "--peer-uid".into(),
        peer_uid.to_string(),
        "--idle-seconds".into(),
        IDLE_SECONDS.to_string(),
    ] {
        args.push(OsString::from(value));
    }
    args
}

/// Start (or find) the backup reader and reply with its socket and token.
/// No fields: the reader only reads the configured homes and shared roots.
pub(crate) async fn backup_reader_start(options: &NativeOptions) -> Result<Value, NativeError> {
    let peer_uid = service_uid(options)?;
    let gid = options.effective_rast_gid()?;
    // The rules must hold for this configuration before anything starts.
    OwnerRules::for_uid_base(options.uid_base, Some(peer_uid)).map_err(NativeError::invalid)?;
    let dir = prepare_dir(options, gid)?;
    let socket = dir.join("reader.sock");
    let token_file = dir.join("reader.token");
    let reply = |token: &str| {
        json!({
            "unit": UNIT,
            "socket": socket.display().to_string(),
            "token": token,
        })
    };
    let socket_unit = format!("{UNIT}.socket");
    if socket.exists() && unit_active(options, &socket_unit).await? {
        if let Some(token) = read_token(&token_file) {
            return Ok(reply(&token));
        }
    }
    let _ = stop_unit(options, &socket_unit).await;
    let _ = stop_unit(options, &format!("{UNIT}.service")).await;
    let _ = std::fs::remove_file(&socket);
    let token = random_token();
    write_token(&token_file, &token)?;
    let exe = std::env::current_exe().map_err(|error| {
        NativeError::command_failed(format!(
            "cannot locate the RAST binary for the backup reader: {error}"
        ))
    })?;
    let args = reader_args(
        &socket,
        gid,
        &token_file,
        &exe,
        &options.homes_dir,
        &options.storage_dir.join("shared"),
        options.uid_base,
        peer_uid,
    );
    run_systemd_unit(options, UNIT, args).await?;
    for _ in 0..50 {
        if socket.exists() {
            tracing::info!(unit = UNIT, "backup reader socket ready");
            return Ok(reply(&token));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop_unit(options, &socket_unit).await;
    Err(NativeError::command_failed(format!(
        "the backup reader socket did not appear; check `journalctl -u {UNIT}`"
    )))
}

#[cfg(test)]
mod tests;
