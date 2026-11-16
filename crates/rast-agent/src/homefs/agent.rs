//! The per-user file agent (`rast file-agent`): the File Station API over one
//! user's HOME, run as that user (`rast-u<id>`) in the socket-activated unit
//! `rast-files-u<uid>` that the helper starts (`files_agent_start`).
//!
//! Namespace (ADR 0005, section 2):
//!
//! * `/` is HOME;
//! * `/shared/<name>/…` is the container that the server names for `<name>`
//!   in [`SHARES_HEADER`](crate::fileagent::SHARES_HEADER), read from its
//!   database after its access checks. The agent opens it directly below the
//!   shared root and checks its owner and group; it never follows
//!   `~/Shared/<name>` (those links are for apps);
//! * `/@root` is never served here.
//!
//! The agent runs as the user: every app and terminal of the user shares its
//! uid, and the other members of a shared folder can change entries in it
//! while the agent works. Every operation therefore goes through
//! [`super::fsat`]: paths are opened below a descriptor of the namespace
//! root, no symbolic link is followed, and the work is done relative to
//! folders the agent holds open, so a link or a folder that someone else
//! swaps in cannot point the agent at other files of its user. The JSON
//! shapes are those of the in-process handlers. The server has already
//! checked the session, CSRF and the RAST access rules before it forwards a
//! request.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::fileagent::{
    self, blocking, check, read_token, Agent, CancelOnDrop, PeerListener, UploadSpace,
};
use crate::storage::ops::{self, EntryDto, ListingDto, QuotaDto, SearchDto, TextPreviewDto};
use crate::storage::safe_path::{self, validate_component, validate_new_name};
use crate::storage::serve::{serve_open_file, Disposition, PREVIEW_CSP};
use crate::storage::{
    LogicalPath, RootKind, MAX_BATCH_PATHS, MAX_SEARCH_DEPTH, MAX_SEARCH_RESULTS, MAX_TREE_ENTRIES,
    TEMP_PREFIX,
};

use super::fsat::{self, fs_error, Info, Kind, Resolve, Step};
use super::{ShareRoots, UPLOAD_PREFIX};
use crate::storage::shares::rw_group_name;

/// The gid of a group in the system's group database.
fn group_id(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buffer: Vec<libc::c_char> = vec![0; 4096];
    loop {
        // SAFETY: `group` and `found` are plain out-parameters, `buffer` is
        // valid for `buffer.len()` bytes and outlives the call.
        let mut group: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        let error = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                &mut group,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if error == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        return (error == 0 && !found.is_null()).then_some(group.gr_gid);
    }
}

/// Whether a container is set up as a RAST shared folder: a folder owned by
/// root or by the owner of the shared root, whose group is the shared
/// root's (legacy containers) or the share's read-write group.
fn container_ok(container: &Info, shared_root: &Info, rw_gid: Option<u32>) -> bool {
    container.is_dir()
        && (container.uid == 0 || container.uid == shared_root.uid)
        && (container.gid == shared_root.gid || Some(container.gid) == rw_gid)
}

/// The namespace of one HOME. Both roots are opened once; every lookup
/// starts from those descriptors.
#[derive(Debug, Clone)]
pub struct HomeSpace {
    home: PathBuf,
    home_root: Arc<OwnedFd>,
    home_info: Info,
    /// The canonical shared root, as the `~/Shared` links name it.
    shared_root: PathBuf,
    shared_dir: Arc<OwnedFd>,
    shared_info: Info,
}

/// The root of one namespace: HOME, or the container of a shared folder.
#[derive(Debug, Clone)]
struct NsRoot {
    fd: Arc<OwnedFd>,
    info: Info,
    kind: RootKind,
    /// The shared folder's display name.
    share_name: Option<String>,
}

/// An existing entry of the namespace, reached through descriptors.
#[derive(Debug)]
struct Node {
    logical: LogicalPath,
    root: NsRoot,
    /// The folder that holds the entry, and its name (`None`: a namespace
    /// root).
    at: Option<(OwnedFd, String)>,
    info: Info,
}

impl HomeSpace {
    /// Both roots must be real directories; they are canonicalised and
    /// opened once.
    pub fn new(home: &Path, shared_root: &Path) -> io::Result<Self> {
        for root in [home, shared_root] {
            let meta = std::fs::symlink_metadata(root)?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a real directory", root.display()),
                ));
            }
        }
        let home = std::fs::canonicalize(home)?;
        let shared_root = std::fs::canonicalize(shared_root)?;
        let (home_root, home_info) = fsat::open_root(&home)?;
        let (shared_dir, shared_info) = fsat::open_root(&shared_root)?;
        Ok(Self {
            home,
            home_root: Arc::new(home_root),
            home_info,
            shared_root,
            shared_dir: Arc::new(shared_dir),
            shared_info,
        })
    }

    /// The (canonical) HOME directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The root of the shared folder that `/shared/<name>` names: the
    /// container the server sent, a real folder directly in the shared root
    /// (no link followed), with the owner and group of a shared folder.
    fn share_root(&self, name: &str, shares: &ShareRoots) -> AppResult<NsRoot> {
        let share = shares
            .get(name)
            .ok_or(AppError::NotFound("path not found"))?;
        let (fd, info) = fsat::open_dir_resolved(
            self.shared_dir.as_fd(),
            std::slice::from_ref(&share.container),
            Resolve::SameFs,
        )
        .map_err(fs_error)?;
        let rw_gid = if info.gid == self.shared_info.gid {
            None
        } else {
            rw_group_name(&share.id).and_then(|group| group_id(&group))
        };
        if !container_ok(&info, &self.shared_info, rw_gid) {
            tracing::warn!(
                share = %share.id,
                uid = info.uid,
                gid = info.gid,
                "the container of a shared folder has an unexpected owner or group"
            );
            return Err(AppError::Forbidden(
                "this shared folder is not set up correctly",
            ));
        }
        Ok(NsRoot {
            fd: Arc::new(fd),
            info,
            kind: RootKind::Share,
            share_name: Some(share.name.clone()),
        })
    }

    /// The namespace root of a logical path.
    fn root_for(&self, logical: &LogicalPath, shares: &ShareRoots) -> AppResult<NsRoot> {
        let first = logical.segments().first().map(String::as_str);
        if first == Some(safe_path::RESERVED_NAME) {
            return Err(AppError::bad_request(
                "the root file system is not served by the file agent",
            ));
        }
        if let Some((name, _)) = logical.shared_parts() {
            return self.share_root(name, shares);
        }
        if first == Some("shared") {
            return Err(AppError::NotFound("path not found"));
        }
        Ok(NsRoot {
            fd: self.home_root.clone(),
            info: self.home_info,
            kind: RootKind::Home,
            share_name: None,
        })
    }

    /// An existing entry: its folder is opened below the namespace root
    /// (no link followed), the entry itself is a name in that folder.
    fn node(&self, logical: &LogicalPath, shares: &ShareRoots) -> AppResult<Node> {
        let root = self.root_for(logical, shares)?;
        let Some((name, parents)) = logical.relative_segments().split_last() else {
            return Ok(Node {
                logical: logical.clone(),
                info: root.info,
                root,
                at: None,
            });
        };
        let (dir, _) =
            fsat::open_dir_resolved(root.fd.as_fd(), parents, root.resolve()).map_err(fs_error)?;
        let info = fsat::stat_at(dir.as_fd(), name)
            .map_err(fs_error)?
            .ok_or(AppError::NotFound("path not found"))?;
        if info.kind == Kind::Symlink {
            return Err(AppError::Forbidden("symbolic links are not supported"));
        }
        // A mount point in a shared folder is not entered (F3).
        if root.resolve() == Resolve::SameFs && info.dev != root.info.dev {
            return Err(AppError::Forbidden(
                "this folder is on another file system and is not served",
            ));
        }
        Ok(Node {
            logical: logical.clone(),
            root,
            at: Some((dir, name.clone())),
            info,
        })
    }

    fn node_at(&self, path: &str, shares: &ShareRoots) -> AppResult<Node> {
        self.node(&LogicalPath::parse(path)?, shares)
    }
}

impl NsRoot {
    /// Shares are resolved on their container's file system only; HOME
    /// keeps the user's own mounts reachable.
    fn resolve(&self) -> Resolve {
        match self.kind {
            RootKind::Share => Resolve::SameFs,
            RootKind::Home => Resolve::Beneath,
        }
    }

    /// The file system that tree walks below this root keep to: the
    /// container's below a share (F3), any in HOME.
    fn tree_fs(&self) -> Option<u64> {
        (self.kind == RootKind::Share).then_some(self.info.dev)
    }
}

impl Node {
    fn is_storage_root(&self) -> bool {
        self.at.is_none()
    }

    fn in_home(&self) -> bool {
        self.root.kind == RootKind::Home
    }

    fn name(&self) -> &str {
        self.at.as_ref().map_or("", |(_, name)| name)
    }

    /// `Home` for HOME, the shared folder's name for its root.
    fn display_name(&self) -> String {
        match &self.at {
            Some((_, name)) => name.clone(),
            None if self.in_home() => "Home".to_string(),
            None => self.root.share_name.clone().unwrap_or_default(),
        }
    }

    fn writable(&self) -> bool {
        match &self.at {
            Some((dir, name)) => fsat::writable_at(dir.as_fd(), name),
            None => fsat::writable_at(self.root.fd.as_fd(), "."),
        }
    }

    fn entry(&self) -> EntryDto {
        entry_of(
            &self.logical,
            &self.display_name(),
            &self.info,
            self.writable(),
        )
    }

    /// A handle on this folder, for `*at` calls.
    fn folder(&self, not_a_folder: &'static str) -> AppResult<OwnedFd> {
        if !self.info.is_dir() {
            return Err(AppError::bad_request(not_a_folder));
        }
        match &self.at {
            Some((dir, name)) => fsat::open_dir_at(dir.as_fd(), name, &self.info),
            None => fsat::open_dir(self.root.fd.as_fd(), &[])
                .map(|(fd, _)| fd)
                .map_err(fs_error),
        }
    }

    /// This folder, opened to read its entries.
    fn listing(&self, not_a_folder: &'static str) -> AppResult<OwnedFd> {
        if !self.info.is_dir() {
            return Err(AppError::bad_request(not_a_folder));
        }
        match &self.at {
            Some((dir, name)) => fsat::open_listing(dir.as_fd(), Some(name), &self.info),
            None => fsat::open_listing(self.root.fd.as_fd(), None, &self.info),
        }
    }

    /// This regular file, opened for reading.
    fn file(&self) -> AppResult<File> {
        match (&self.at, self.info.kind) {
            (_, Kind::Dir) => Err(AppError::bad_request("path is a directory")),
            (Some((dir, name)), Kind::File) => fsat::open_file_at(dir.as_fd(), name, &self.info),
            _ => Err(AppError::bad_request("path is not a regular file")),
        }
    }
}

fn entry_of(logical: &LogicalPath, name: &str, info: &Info, writable: bool) -> EntryDto {
    ops::entry_from_parts(
        logical,
        name,
        info.is_dir(),
        info.len,
        info.modified_ms,
        writable,
    )
}

/// `EEXIST` as a conflict with `message`, any other error as usual.
fn exists_as(message: String) -> impl FnOnce(io::Error) -> AppError {
    move |error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            AppError::Conflict(message)
        } else {
            fs_error(error)
        }
    }
}

/// Where an upload goes: a name in a folder the agent holds open.
pub struct HomeTarget {
    dir: Arc<OwnedFd>,
    name: String,
    logical: LogicalPath,
    /// The folder is in a shared folder (decides the new file's bits).
    in_share: bool,
}

/// The temporary file of an upload. It stays open: chunks are written
/// through this descriptor, never through its name.
pub struct HomeTemp {
    dir: Arc<OwnedFd>,
    name: String,
    file: File,
    info: Info,
}

impl HomeTemp {
    /// Whether the temporary name still is this upload's file.
    fn still_mine(&self) -> bool {
        matches!(
            fsat::stat_at(self.dir.as_fd(), &self.name),
            Ok(Some(info)) if info.same_inode(&self.info)
        )
    }
}

impl UploadSpace for HomeSpace {
    const ID_PREFIX: &'static str = UPLOAD_PREFIX;
    const TEMP_PREFIX: &'static str = TEMP_PREFIX;
    const DEFAULT_DIR: &'static str = "/";
    const REPORTS_REPLACED: bool = true;

    type Target = HomeTarget;
    type Temp = HomeTemp;

    fn target(
        &self,
        dir: &str,
        name: &str,
        overwrite: bool,
        headers: &HeaderMap,
    ) -> AppResult<HomeTarget> {
        validate_new_name(name)?;
        let parent = self.node_at(dir, &ShareRoots::from_headers(headers)?)?;
        let folder = parent.folder("upload destination is not a directory")?;
        let logical = parent.logical.child(name)?;
        match fsat::stat_at(folder.as_fd(), name).map_err(fs_error)? {
            Some(info) if info.kind == Kind::Symlink => {
                return Err(AppError::Forbidden("symbolic links are not supported"))
            }
            Some(info) if info.is_dir() => {
                return Err(AppError::conflict("a folder with that name already exists"))
            }
            Some(_) if !overwrite => {
                return Err(AppError::conflict("a file with that name already exists"))
            }
            _ => {}
        }
        Ok(HomeTarget {
            dir: Arc::new(folder),
            name: name.to_string(),
            logical,
            in_share: !parent.in_home(),
        })
    }

    fn create_temp(&self, target: &HomeTarget, id: &str) -> AppResult<(HomeTemp, File)> {
        let name = format!("{TEMP_PREFIX}{id}");
        let file = fsat::create_at(target.dir.as_fd(), &name, 0o600)
            .map_err(|error| self.map_io(error, fs_error))?;
        let info = fsat::stat_fd(&file).map_err(fs_error)?;
        let kept = file.try_clone().map_err(fs_error)?;
        Ok((
            HomeTemp {
                dir: target.dir.clone(),
                name,
                file: kept,
                info,
            },
            file,
        ))
    }

    fn open_temp(&self, temp: &HomeTemp) -> AppResult<File> {
        temp.file.try_clone().map_err(fs_error)
    }

    fn remove_temp(&self, temp: &HomeTemp) {
        // Never a file that someone else put under the temporary name.
        if temp.still_mine() {
            let _ = fsat::unlink_at(temp.dir.as_fd(), &temp.name);
        }
    }

    fn commit(
        &self,
        temp: &HomeTemp,
        target: &HomeTarget,
        overwrite: bool,
    ) -> AppResult<(EntryDto, u64)> {
        if !temp.still_mine() {
            return Err(AppError::conflict(
                "the upload's temporary file was replaced",
            ));
        }
        let dir = target.dir.as_fd();
        let exists = || AppError::conflict(format!("\"{}\" already exists", target.name));
        // Keep the permission bits of a replaced file (never set-uid or
        // set-gid); a new file gets 0644 in HOME and 0660 in a shared folder.
        let (replaced, mode) = match fsat::stat_at(dir, &target.name).map_err(fs_error)? {
            Some(info) if info.is_dir() => {
                return Err(AppError::conflict("a folder with that name already exists"))
            }
            Some(_) if !overwrite => return Err(exists()),
            Some(info) if info.kind == Kind::File => (info.len, info.mode & 0o777),
            _ => (0, fsat::new_file_mode(None, target.in_share)),
        };
        fsat::set_mode(&temp.file, mode).map_err(fs_error)?;
        let renamed = if overwrite {
            fsat::rename_over(temp.dir.as_fd(), &temp.name, dir, &target.name)
        } else {
            fsat::rename_noreplace(temp.dir.as_fd(), &temp.name, dir, &target.name)
        };
        renamed.map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                exists()
            } else {
                fs_error(error)
            }
        })?;
        let info = fsat::stat_fd(&temp.file).map_err(fs_error)?;
        let entry = entry_of(
            &target.logical,
            &target.name,
            &info,
            fsat::writable_at(dir, &target.name),
        );
        Ok((entry, replaced))
    }

    fn map_io(&self, error: io::Error, fallback: impl FnOnce(io::Error) -> AppError) -> AppError {
        match error.raw_os_error() {
            Some(libc::EDQUOT) | Some(libc::ENOSPC) => fs_error(error),
            _ => fallback(error),
        }
    }
}

type HomeAgent = Agent<HomeSpace>;

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Where the agent listens.
pub enum Listen {
    /// The socket systemd passed (`LISTEN_FDS`, socket activation).
    Systemd,
    /// Bind this path (development and tests).
    Path(PathBuf),
}

/// The listening socket systemd passed to this process, if any.
fn systemd_listener() -> io::Result<std::os::unix::net::UnixListener> {
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
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    listener.set_nonblocking(true)?;
    for name in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: called once at start-up, before other threads read the
        // environment.
        unsafe { std::env::remove_var(name) };
    }
    Ok(listener)
}

/// The token from `--token-file`, or from the systemd credential `token`.
pub fn token_from(token_file: Option<&Path>) -> io::Result<String> {
    match token_file {
        Some(path) => read_token(path),
        None => {
            let dir = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no agent token credential")
            })?;
            read_token(&Path::new(&dir).join("token"))
        }
    }
}

/// Make this process non-dumpable. The agent runs as the user, so every app
/// and terminal of the user shares its uid; without this they could ptrace
/// it or read and write its memory through `/proc/<pid>/mem` (wherever
/// Yama's `ptrace_scope` is 0) and answer the server in its name. Call it
/// once at start-up; it also turns off core dumps.
pub fn forbid_tracing() -> io::Result<()> {
    // SAFETY: PR_SET_DUMPABLE takes plain integer arguments.
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether another process of this account is running (a session, app or
/// terminal): the agent then stays up so realtime events keep flowing.
fn user_has_other_processes() -> bool {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            return false;
        };
        if pid == me {
            return false;
        }
        std::fs::read_to_string(entry.path().join("status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("Uid:"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|value| value.parse::<u32>().ok())
            })
            == Some(uid)
    })
}

/// Options of one agent run.
pub struct RunOptions {
    pub home: PathBuf,
    pub shared_root: PathBuf,
    pub listen: Listen,
    pub token: String,
    /// The only uid allowed to connect (the `rast` service).
    pub peer_uid: Option<u32>,
    pub idle: Duration,
    /// Stay up while the user has other processes.
    pub stay_while_user_runs: bool,
}

/// Run the agent until it has been idle for `idle`.
pub async fn run(options: RunOptions) -> io::Result<()> {
    let space = HomeSpace::new(&options.home, &options.shared_root)?;
    let (listener, socket) = match &options.listen {
        Listen::Systemd => (
            tokio::net::UnixListener::from_std(systemd_listener()?)?,
            None,
        ),
        Listen::Path(path) => {
            let _ = std::fs::remove_file(path);
            (tokio::net::UnixListener::bind(path)?, Some(path.clone()))
        }
    };
    let agent = Agent::new(options.token, space);
    let app = router(agent.clone());
    let stay = options.stay_while_user_runs;
    let result = fileagent::serve(
        PeerListener::new(listener, options.peer_uid),
        app,
        agent,
        options.idle,
        move || stay && user_has_other_processes(),
    )
    .await;
    if let Some(socket) = socket {
        let _ = std::fs::remove_file(socket);
    }
    result
}

pub fn router(agent: Arc<HomeAgent>) -> Router {
    Router::new()
        .route("/api/files", get(list))
        .route("/api/files/stat", get(stat))
        .route("/api/files/mkdir", post(mkdir))
        .route("/api/files/rename", post(rename))
        .route("/api/files/download", get(download))
        .route("/api/files/preview", get(preview))
        .route("/api/files/text", get(text))
        .route("/api/files/search", get(search))
        .route("/api/files/copy", post(copy))
        .route("/api/files/move", post(move_items))
        .route(SHARED_LINKS_ROUTE, post(shared_links))
        .route(fileagent::WATCH_ROUTE, get(super::watch::stream))
        .merge(fileagent::upload_routes::<HomeSpace>())
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn_with_state(
            agent.clone(),
            fileagent::guard::<HomeSpace>,
        ))
        .with_state(agent)
}

/// Internal route: bring the `~/Shared` links in line with the shared folders
/// in the request's shares header (the server sends all that the user may
/// open). Not part of the File Station API.
pub const SHARED_LINKS_ROUTE: &str = "/internal/shared-links";

async fn shared_links(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
) -> AppResult<Json<super::links::SharedLinks>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let space = agent.space.clone();
    blocking(move || super::links::sync(space.home_root.as_fd(), &space.shared_root, &shares))
        .await
        .map(Json)
}

// ---------------------------------------------------------------------------
// Listing, stat, search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListQuery {
    path: Option<String>,
    page: Option<i64>,
    #[serde(rename = "perPage")]
    per_page: Option<i64>,
    sort: Option<String>,
    order: Option<String>,
    q: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    /// `false` leaves out dot files.
    hidden: Option<bool>,
}

fn page_params(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    (
        page.unwrap_or(1).max(1),
        per_page
            .unwrap_or(crate::storage::DEFAULT_PAGE_SIZE)
            .clamp(1, crate::storage::MAX_PAGE_SIZE),
    )
}

async fn list(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> AppResult<Json<ListingDto>> {
    let space = agent.space.clone();
    let shares = ShareRoots::from_headers(&headers)?;
    blocking(move || {
        let logical = LogicalPath::parse(query.path.as_deref().unwrap_or("/"))?;
        let (page, per_page) = page_params(query.page, query.per_page);
        let node = space.node(&logical, &shares)?;
        let dir = node.listing("path is not a directory")?;
        let show_hidden = query.hidden.unwrap_or(true);
        let mut entries = Vec::new();
        for (name, info) in fsat::entries(dir.as_fd()).map_err(fs_error)? {
            if name.starts_with(TEMP_PREFIX)
                || validate_component(&name).is_err()
                || (!show_hidden && name.starts_with('.'))
                || !matches!(info.kind, Kind::File | Kind::Dir)
            {
                continue;
            }
            let Ok(child) = logical.child(&name) else {
                continue;
            };
            let writable = fsat::writable_at(dir.as_fd(), &name);
            entries.push(entry_of(&child, &name, &info, writable));
        }
        let needle = query
            .q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        if let Some(needle) = &needle {
            entries.retain(|entry| entry.name.to_lowercase().contains(needle));
        }
        ops::sort_entries(
            &mut entries,
            query.sort.as_deref().unwrap_or("name"),
            query.order.as_deref().unwrap_or("asc"),
        );
        let (items, page, per_page, total) = ops::paginate(entries, page, per_page);
        Ok(ListingDto {
            path: logical.as_string(),
            breadcrumbs: ops::breadcrumbs(
                &logical,
                node.in_home(),
                node.root.share_name.as_deref(),
            ),
            items,
            page,
            per_page,
            total,
            // The server fills in the quota it accounts.
            quota: QuotaDto {
                quota_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
            },
        })
    })
    .await
    .map(Json)
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: Option<String>,
    #[serde(rename = "maxBytes", alias = "max_bytes")]
    max_bytes: Option<u64>,
}

fn required(path: &Option<String>) -> AppResult<String> {
    path.as_deref()
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::bad_request("path is required"))
}

async fn stat(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Json<EntryDto>> {
    let space = agent.space.clone();
    let path = required(&query.path)?;
    let shares = ShareRoots::from_headers(&headers)?;
    blocking(move || Ok(space.node_at(&path, &shares)?.entry()))
        .await
        .map(Json)
}

async fn search(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> AppResult<Json<SearchDto>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let (page, per_page) = page_params(query.page, query.per_page);
    let q = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| AppError::bad_request("q is required"))?;
    if q.chars().count() > 128 {
        return Err(AppError::bad_request("search query is too long"));
    }
    let needle = q.to_lowercase();
    let space = agent.space.clone();
    blocking(move || {
        let node = space.node_at(query.path.as_deref().unwrap_or("/"), &shares)?;
        let dir = node.listing("search path is not a directory")?;
        let kind = query.kind.as_deref().map(|k| k.trim().to_ascii_lowercase());
        let show_hidden = query.hidden.unwrap_or(true);
        let mut hits: Vec<EntryDto> = Vec::new();
        let mut scanned = 0usize;
        fsat::walk(dir, MAX_SEARCH_DEPTH, node.root.tree_fs(), |visit| {
            // Links are never followed; dot entries (and what is below
            // them) are left out unless asked for.
            if visit.info.kind == Kind::Symlink || (!show_hidden && visit.name.starts_with('.')) {
                return Step::Over;
            }
            scanned += 1;
            if scanned > MAX_TREE_ENTRIES {
                return Step::Stop;
            }
            let is_dir = visit.info.is_dir();
            if !is_dir && visit.info.kind != Kind::File {
                return Step::Over;
            }
            if visit.name.starts_with(TEMP_PREFIX) || !visit.name.to_lowercase().contains(&needle) {
                return Step::Into;
            }
            if kind
                .as_deref()
                .is_some_and(|filter| !ops::kind_matches(filter, is_dir, visit.name))
            {
                return Step::Into;
            }
            let Ok(logical) = node.logical.with_segments(visit.path) else {
                return Step::Into;
            };
            let writable = fsat::writable_at(visit.dir, visit.name);
            hits.push(entry_of(&logical, visit.name, visit.info, writable));
            if hits.len() > MAX_SEARCH_RESULTS {
                return Step::Stop;
            }
            Step::Into
        })
        .map_err(fs_error)?;
        let truncated = hits.len() > MAX_SEARCH_RESULTS;
        hits.truncate(MAX_SEARCH_RESULTS);
        ops::sort_entries(&mut hits, "name", "asc");
        let (items, page, per_page, total) = ops::paginate(hits, page, per_page);
        Ok(SearchDto {
            items,
            page,
            per_page,
            total,
            truncated,
            scanned,
        })
    })
    .await
    .map(Json)
}

// ---------------------------------------------------------------------------
// Folders and names
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MkdirRequest {
    path: Option<String>,
    parent: Option<String>,
    name: Option<String>,
}

async fn mkdir(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<MkdirRequest>,
) -> AppResult<Json<EntryDto>> {
    let shares = ShareRoots::from_headers(&headers)?;
    let logical = match (body.path, body.parent, body.name) {
        (Some(path), _, _) => LogicalPath::parse(&path)?,
        (None, Some(parent), Some(name)) => LogicalPath::parse(&parent)?.child(&name)?,
        _ => return Err(AppError::bad_request("path (or parent + name) is required")),
    };
    if logical.is_root() {
        return Err(AppError::bad_request("cannot create the storage root"));
    }
    let parent_logical = logical
        .parent()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let name = logical
        .name()
        .ok_or_else(|| AppError::bad_request("invalid path"))?
        .to_string();
    let space = agent.space.clone();
    blocking(move || {
        let parent = space.node(&parent_logical, &shares)?;
        let dir = parent.folder("parent is not a directory")?;
        fsat::mkdir_at(dir.as_fd(), &name).map_err(exists_as(
            "a file or folder with that name already exists".to_string(),
        ))?;
        let info = fsat::stat_at(dir.as_fd(), &name)
            .map_err(fs_error)?
            .ok_or(AppError::NotFound("path not found"))?;
        Ok(entry_of(&logical, &name, &info, true))
    })
    .await
    .map(Json)
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    path: String,
    #[serde(rename = "newName", alias = "new_name")]
    new_name: String,
}

async fn rename(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<RenameRequest>,
) -> AppResult<Json<EntryDto>> {
    let space = agent.space.clone();
    let shares = ShareRoots::from_headers(&headers)?;
    blocking(move || {
        let node = space.node_at(&body.path, &shares)?;
        let Some((dir, old)) = &node.at else {
            return Err(AppError::bad_request("cannot rename a storage root"));
        };
        let new_name = body.new_name.trim();
        validate_new_name(new_name)?;
        if new_name == old {
            return Ok(node.entry());
        }
        let logical = node
            .logical
            .parent()
            .ok_or_else(|| AppError::bad_request("invalid path"))?
            .child(new_name)?;
        fsat::rename_noreplace(dir.as_fd(), old, dir.as_fd(), new_name).map_err(exists_as(
            "a file or folder with that name already exists".to_string(),
        ))?;
        let info = fsat::stat_at(dir.as_fd(), new_name)
            .map_err(fs_error)?
            .ok_or(AppError::NotFound("path not found"))?;
        Ok(entry_of(&logical, new_name, &info, true))
    })
    .await
    .map(Json)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The regular file of a query, opened (the inode checked) for reading.
async fn regular_file(
    agent: &HomeAgent,
    query: &PathQuery,
    headers: &HeaderMap,
) -> AppResult<(tokio::fs::File, Node)> {
    let space = agent.space.clone();
    let path = required(&query.path)?;
    let shares = ShareRoots::from_headers(headers)?;
    blocking(move || {
        let node = space.node_at(&path, &shares)?;
        let file = node.file()?;
        Ok((tokio::fs::File::from_std(file), node))
    })
    .await
}

async fn download(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (file, node) = regular_file(&agent, &query, &headers).await?;
    let content_type = ops::mime_for_name(node.name());
    serve_open_file(
        file,
        node.info.len,
        node.name(),
        &headers,
        &content_type,
        Disposition::Attachment,
        None,
    )
    .await
}

async fn preview(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (file, node) = regular_file(&agent, &query, &headers).await?;
    let name = node.name();
    let kind = ops::preview_kind(name).ok_or(AppError::UnsupportedMediaType(
        "preview is not supported for this file type",
    ))?;
    let content_type = ops::preview_content_type(name, kind);
    serve_open_file(
        file,
        node.info.len,
        name,
        &headers,
        &content_type,
        Disposition::Inline,
        Some(PREVIEW_CSP),
    )
    .await
}

async fn text(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Json<TextPreviewDto>> {
    let (file, node) = regular_file(&agent, &query, &headers).await?;
    let limit = query
        .max_bytes
        .unwrap_or(crate::storage::DEFAULT_TEXT_PREVIEW_BYTES)
        .clamp(1, crate::storage::MAX_TEXT_PREVIEW_BYTES);
    ops::text_preview_from(file, node.info.len, Some(limit))
        .await
        .map(Json)
}

// ---------------------------------------------------------------------------
// Copy and move
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BatchRequest {
    paths: Vec<String>,
    #[serde(rename = "destDir", alias = "dest_dir")]
    dest_dir: String,
}

/// The free quota the server sent with a copy or move into HOME (absent:
/// no limit).
fn quota_budget(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(fileagent::QUOTA_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// The checks of `transfer::plan_batch`, against this namespace: the
/// destination folder (held open) and the sources (never namespace roots).
fn plan(
    space: &HomeSpace,
    body: &BatchRequest,
    shares: &ShareRoots,
) -> AppResult<(Node, OwnedFd, Vec<Node>)> {
    if body.paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    if body.paths.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many paths in one request"));
    }
    let dest = space.node_at(&body.dest_dir, shares)?;
    let dest_dir = dest.folder("destination is not a directory")?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(body.paths.len());
    for raw in &body.paths {
        let node = space.node_at(raw, shares)?;
        if node.is_storage_root() {
            return Err(AppError::bad_request(
                "storage roots cannot be copied, moved or deleted",
            ));
        }
        if !seen.insert(node.logical.as_string()) {
            return Err(AppError::bad_request("duplicate paths in request"));
        }
        let name = node.name();
        match fsat::stat_at(dest_dir.as_fd(), name).map_err(fs_error)? {
            Some(info) if info.kind == Kind::Symlink => {
                return Err(AppError::Forbidden("symbolic links are not supported"))
            }
            Some(_) => {
                return Err(AppError::conflict(format!(
                    "\"{name}\" already exists in the destination"
                )))
            }
            None => {}
        }
        if node.info.is_dir()
            && fsat::is_within(dest_dir.as_fd(), &node.info, &dest.root.info).map_err(fs_error)?
        {
            return Err(AppError::bad_request(
                "a folder cannot be copied or moved into itself",
            ));
        }
        out.push(node);
    }
    Ok((dest, dest_dir, out))
}

/// The folder that holds a source, and its name (sources are never
/// namespace roots).
fn source(node: &Node) -> AppResult<(std::os::fd::BorrowedFd<'_>, String)> {
    node.at
        .as_ref()
        .map(|(dir, name)| (dir.as_fd(), name.clone()))
        .ok_or_else(|| AppError::internal("a source is a namespace root"))
}

/// Copy or move a batch. Bytes that land in HOME are checked against the
/// budget the server sent, item by item like in-process transfers, and
/// the reply's additive `homeBytes` says how HOME usage changed (the server
/// accounts it and removes the field).
async fn transfer_items(
    agent: Arc<HomeAgent>,
    body: BatchRequest,
    headers: HeaderMap,
    moving: bool,
) -> AppResult<Json<Value>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let space = agent.space.clone();
    let budget = quota_budget(&headers);
    let shares = ShareRoots::from_headers(&headers)?;
    blocking(move || {
        let (dest, dest_dir, plan) = plan(&space, &body, &shares)?;
        let dest_in_home = dest.in_home();
        // Sizes first: the whole batch must fit, so a full quota writes
        // nothing (in-process transfers stop at the item that does not fit).
        let mut sizes = Vec::with_capacity(plan.len());
        let mut needed = 0u64;
        for node in &plan {
            check(&cancel)?;
            let into_home = dest_in_home && (!moving || !node.in_home());
            let size = if moving || into_home {
                let (dir, name) = source(node)?;
                fsat::tree_bytes(dir, &name, &node.info, TEMP_PREFIX, node.root.tree_fs())
            } else {
                0
            };
            if into_home {
                needed = needed.saturating_add(size);
            }
            sizes.push(size);
        }
        if budget.is_some_and(|free| needed > free) {
            return Err(AppError::PayloadTooLarge("storage quota exceeded"));
        }
        let mut bytes = 0u64;
        let mut home_bytes = 0i64;
        let mut done = 0usize;
        for (node, size) in plan.iter().zip(sizes) {
            check(&cancel)?;
            let (dir, name) = source(node)?;
            let into_home = dest_in_home && (!moving || !node.in_home());
            let out_of_home = moving && node.in_home() && !dest_in_home;
            let delta = i64::try_from(size).unwrap_or(i64::MAX);
            if into_home {
                home_bytes = home_bytes.saturating_add(delta);
            } else if out_of_home {
                home_bytes = home_bytes.saturating_sub(delta);
            }
            let in_destination = || format!("\"{name}\" already exists in the destination");
            // A rename keeps the group, bits and ACL an entry had in its old
            // folder, so between HOME and a share, or two shares, it would
            // stay closed to the new folder's members (and outside a share's
            // group quota). Only moves inside one namespace rename.
            let same_root = node.root.info.same_inode(&dest.root.info);
            if moving && same_root {
                bytes = bytes.saturating_add(size);
                match fsat::rename_noreplace(dir, &name, dest_dir.as_fd(), &name) {
                    Ok(()) => {
                        done += 1;
                        continue;
                    }
                    // Another file system: copy, then remove the original.
                    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
                    Err(error) => return Err(exists_as(in_destination())(error)),
                }
            }
            let mut copied = fsat::Copied::default();
            let mut created = None;
            let result = fsat::copy_at(
                dir,
                &name,
                &node.info,
                node.root.tree_fs(),
                dest_dir.as_fd(),
                !dest_in_home,
                &cancel,
                TEMP_PREFIX,
                &mut copied,
                &mut created,
            );
            if let Err(error) = result {
                // Do not leave half a copy behind (and remove nothing else).
                if let Some(made) = created {
                    fsat::remove_created(dest_dir.as_fd(), &name, &made, dest.root.tree_fs());
                }
                return Err(match error {
                    AppError::Conflict(_) => AppError::conflict(in_destination()),
                    other => other,
                });
            }
            if moving && !same_root {
                bytes = bytes.saturating_add(size);
            }
            if moving {
                // Only the entry that was copied.
                if matches!(fsat::stat_at(dir, &name), Ok(Some(info)) if info.same_inode(&node.info))
                {
                    fsat::remove_at(dir, &name, node.root.tree_fs())?;
                }
            } else {
                bytes = bytes.saturating_add(copied.bytes);
            }
            done += 1;
        }
        let destination = dest.logical.as_string();
        Ok(Json(if moving {
            json!({"moved": done, "bytes": bytes, "destination": destination, "homeBytes": home_bytes})
        } else {
            json!({"copied": done, "bytes": bytes, "destination": destination, "homeBytes": home_bytes})
        }))
    })
    .await
}

async fn copy(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<BatchRequest>,
) -> AppResult<Json<Value>> {
    transfer_items(agent, body, headers, false).await
}

async fn move_items(
    State(agent): State<Arc<HomeAgent>>,
    headers: HeaderMap,
    Json(body): Json<BatchRequest>,
) -> AppResult<Json<Value>> {
    transfer_items(agent, body, headers, true).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, StatusCode};

    use crate::fileagent::client;
    use crate::homefs::{ShareRoot, SHARED_DIR};

    /// A dummy share id.
    const SHARE_ID: &str = "00000000-0000-4000-8000-000000000001";

    struct Fixture {
        dir: tempfile::TempDir,
        home: PathBuf,
        shared: PathBuf,
        socket: PathBuf,
        token: String,
        /// What the server would send in the shares header.
        shares: std::sync::Mutex<ShareRoots>,
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// An agent on a temporary HOME, listening on a socket path.
    async fn start(peer_uid: Option<u32>) -> Fixture {
        let dir = crate::test_support::socket_tempdir();
        let home = dir.path().join("homes/alice");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let socket = dir.path().join("a.sock");
        let token = "f".repeat(64);
        tokio::spawn(run(RunOptions {
            home: home.clone(),
            shared_root: shared.clone(),
            listen: Listen::Path(socket.clone()),
            token: token.clone(),
            peer_uid,
            idle: Duration::from_secs(600),
            stay_while_user_runs: false,
        }));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Fixture {
            dir,
            home,
            shared,
            socket,
            token,
            shares: std::sync::Mutex::new(ShareRoots::default()),
        }
    }

    impl Fixture {
        /// Let requests name `/shared/<name>` with this container, as the
        /// server does after its checks.
        fn share(&self, name: &str, container: &str) {
            let root = ShareRoot {
                id: SHARE_ID.to_string(),
                name: name.to_string(),
                container: container.to_string(),
            };
            self.shares.lock().unwrap().insert(name, root).unwrap();
        }

        async fn send(
            &self,
            method: Method,
            uri: &str,
            headers: &HeaderMap,
            body: Body,
        ) -> AppResult<Response> {
            let stream = tokio::net::UnixStream::connect(&self.socket).await?;
            let mut headers = headers.clone();
            if !headers.contains_key(fileagent::SHARES_HEADER) {
                self.shares.lock().unwrap().apply(&mut headers)?;
            }
            client::send(
                "file agent",
                stream,
                &self.token,
                method,
                uri,
                &headers,
                body,
            )
            .await
        }

        async fn get(&self, uri: &str) -> (StatusCode, Value) {
            let response = self
                .send(Method::GET, uri, &HeaderMap::new(), Body::empty())
                .await
                .unwrap();
            let status = response.status();
            (
                status,
                client::read_json("file agent", response).await.unwrap(),
            )
        }

        async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
            self.post_with(uri, body, HeaderMap::new()).await
        }

        async fn post_with(
            &self,
            uri: &str,
            body: Value,
            mut headers: HeaderMap,
        ) -> (StatusCode, Value) {
            headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            let response = self
                .send(Method::POST, uri, &headers, Body::from(body.to_string()))
                .await
                .unwrap();
            let status = response.status();
            (
                status,
                client::read_json("file agent", response).await.unwrap(),
            )
        }

        async fn upload(&self, uri: &str, bytes: &'static [u8]) -> Response {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_LENGTH, bytes.len().into());
            self.send(Method::POST, uri, &headers, Body::from(bytes))
                .await
                .unwrap()
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_agent_cannot_be_traced_by_the_users_other_processes() {
        const CHILD: &str = "RAST_TEST_FORBID_TRACING";
        const NAME: &str =
            "homefs::agent::tests::the_agent_cannot_be_traced_by_the_users_other_processes";
        let dumpable = || {
            // SAFETY: PR_GET_DUMPABLE takes no pointer arguments.
            unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }
        };
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(dumpable(), 1);
            forbid_tracing().unwrap();
            assert_eq!(dumpable(), 0);
            return;
        }
        // In a child process, so this test binary itself stays dumpable.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_full_quota_is_413_and_a_full_disk_says_so() {
        let quota = fs_error(io::Error::from_raw_os_error(libc::EDQUOT));
        assert!(matches!(quota, AppError::PayloadTooLarge(_)), "{quota:?}");
        let disk = fs_error(io::Error::from_raw_os_error(libc::ENOSPC));
        assert!(disk.to_string().contains("disk space"), "{disk}");
    }

    #[tokio::test]
    async fn traversal_and_foreign_namespaces_are_refused() {
        let fx = start(None).await;
        std::fs::write(fx.dir.path().join("secret.txt"), b"outside").unwrap();
        for path in [
            "%2e%2e",
            "/%2e%2e/secret.txt",
            "/a/..%2f..%2fsecret.txt",
            "/a%00b",
            "relative",
            "/a%5c..%5csecret.txt",
        ] {
            let (status, _) = fx.get(&format!("/api/files/stat?path={path}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        }
        // An absolute host path is just a name inside HOME.
        let (status, _) = fx
            .get(&format!(
                "/api/files/stat?path={}",
                fx.dir.path().join("secret.txt").display()
            ))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = fx.get("/api/files?path=/@root/etc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = fx.get("/api/files?path=/shared").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // There is no route for deleting (the recycle bin comes later).
        let (status, _) = fx.post("/api/files/delete", json!({"paths": ["/x"]})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dot_files_are_left_out_only_when_asked() {
        let fx = start(None).await;
        std::fs::write(fx.home.join(".bashrc"), b"x").unwrap();
        std::fs::create_dir_all(fx.home.join(".cache/app")).unwrap();
        std::fs::write(fx.home.join(".cache/app/report.txt"), b"x").unwrap();
        std::fs::write(fx.home.join("report.txt"), b"x").unwrap();
        let names = |listing: &Value| -> Vec<String> {
            listing["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_string())
                .collect()
        };

        let (status, all) = fx.get("/api/files?path=/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(names(&all), [".bashrc", ".cache", "report.txt"]);
        let (_, visible) = fx.get("/api/files?path=/&hidden=false").await;
        assert_eq!(names(&visible), ["report.txt"]);

        let (_, found) = fx.get("/api/files/search?path=/&q=report").await;
        assert_eq!(found["total"], 2);
        let (_, found) = fx
            .get("/api/files/search?path=/&q=report&hidden=false")
            .await;
        assert_eq!(found["total"], 1);
        assert_eq!(found["items"][0]["path"], "/report.txt");
    }

    #[tokio::test]
    async fn symlinks_never_lead_out_of_home() {
        let fx = start(None).await;
        let outside = fx.dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, fx.home.join("docs")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), fx.home.join("note.txt")).unwrap();
        std::fs::write(fx.home.join("real.txt"), b"mine").unwrap();

        // In the middle of a path, and as its last component.
        for uri in [
            "/api/files?path=/docs",
            "/api/files/stat?path=/docs/secret.txt",
            "/api/files/download?path=/docs/secret.txt",
            "/api/files/download?path=/note.txt",
            "/api/files/text?path=/note.txt",
        ] {
            let (status, _) = fx.get(uri).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        for (uri, body) in [
            ("/api/files/mkdir", json!({"path": "/docs/new"})),
            (
                "/api/files/rename",
                json!({"path": "/note.txt", "newName": "x"}),
            ),
            (
                "/api/files/copy",
                json!({"paths": ["/note.txt"], "destDir": "/"}),
            ),
            (
                "/api/files/copy",
                json!({"paths": ["/real.txt"], "destDir": "/docs"}),
            ),
            (
                "/api/files/move",
                json!({"paths": ["/real.txt"], "destDir": "/docs"}),
            ),
        ] {
            let (status, _) = fx.post(uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        let response = fx
            .upload("/api/files/upload?dir=/docs&name=x.txt", b"x")
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Listings and searches skip the links; nothing outside changed.
        let (status, listing) = fx.get("/api/files?path=/").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = listing["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["real.txt"]);
        let (_, found) = fx.get("/api/files/search?path=/&q=secret").await;
        assert_eq!(found["total"], 0);
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn shared_folders_are_the_containers_the_server_names() {
        let fx = start(None).await;
        let team = fx.shared.join("team-1");
        std::fs::create_dir_all(team.join("sub")).unwrap();
        std::fs::write(team.join("plan.txt"), b"plan").unwrap();
        let other = fx.shared.join("other-2");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("salaries.txt"), b"secret").unwrap();
        // A link in the shared root, and a container that is a file.
        std::os::unix::fs::symlink(&other, fx.shared.join("alias")).unwrap();
        std::fs::write(fx.shared.join("plain"), b"x").unwrap();
        // The path names `team`, the share's display name is `Team`.
        let root = ShareRoot {
            id: SHARE_ID.to_string(),
            name: "Team".to_string(),
            container: "team-1".to_string(),
        };
        fx.shares.lock().unwrap().insert("team", root).unwrap();
        fx.share("Alias", "alias");
        fx.share("Plain", "plain");
        fx.share("Gone", "gone-3");

        let (status, listing) = fx.get("/api/files?path=/shared/team").await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(listing["path"], "/shared/team");
        // The breadcrumb shows the share's display name (as in-process).
        assert_eq!(listing["breadcrumbs"][0]["path"], "/shared/Team");
        assert_eq!(listing["breadcrumbs"][0]["name"], "Team");
        assert_eq!(listing["items"][0]["path"], "/shared/team/plan.txt");
        let (status, entry) = fx.get("/api/files/stat?path=/shared/team").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(entry["name"], "Team");
        // A share the server did not name does not exist for the agent;
        // a linked container is refused, a missing one is not found.
        for (name, expected) in [
            ("Other", StatusCode::NOT_FOUND),
            ("Gone", StatusCode::NOT_FOUND),
            ("Alias", StatusCode::FORBIDDEN),
            ("Plain", StatusCode::NOT_FOUND),
        ] {
            let (status, body) = fx.get(&format!("/api/files?path=/shared/{name}")).await;
            assert_eq!(status, expected, "{name}: {body}");
        }

        // `~/Shared/<name>` links are for apps: never followed, never
        // listed, even when they point elsewhere than the server's
        // container.
        let links = fx.home.join(SHARED_DIR);
        std::fs::create_dir(&links).unwrap();
        std::os::unix::fs::symlink(&other, links.join("team")).unwrap();
        std::os::unix::fs::symlink(&other, links.join("Other")).unwrap();
        let (_, listing) = fx.get("/api/files?path=/shared/team").await;
        assert_eq!(listing["total"], 2, "{listing}");
        let (status, _) = fx.get("/api/files?path=/shared/Other").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, links_listing) = fx.get("/api/files?path=/Shared").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(links_listing["total"], 0, "{links_listing}");
        for uri in [
            "/api/files?path=/Shared/team",
            "/api/files/download?path=/Shared/Other/salaries.txt",
        ] {
            let (status, _) = fx.get(uri).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }

        // Copy from a share into HOME stays inside the agent.
        let (status, copied) = fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/team/plan.txt"], "destDir": "/"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{copied}");
        assert_eq!(copied["copied"], 1);
        assert_eq!(std::fs::read(fx.home.join("plan.txt")).unwrap(), b"plan");

        // An invalid shares header is refused as a whole.
        let mut headers = HeaderMap::new();
        headers.insert(fileagent::SHARES_HEADER, "7a7a".parse().unwrap());
        let response = fx
            .send(
                Method::GET,
                "/api/files?path=/shared/team",
                &headers,
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn files_written_into_a_share_stay_writable_for_its_group() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fx = start(None).await;
        let team = fx.shared.join("team-1");
        std::fs::create_dir(&team).unwrap();
        fx.share("team", "team-1");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;

        // An upload into a share is 0660, into HOME 0644.
        let response = fx
            .upload("/api/files/upload?dir=/shared/team&name=a.txt", b"a")
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mode(&team.join("a.txt")), 0o660);
        let response = fx.upload("/api/files/upload?dir=/&name=h.txt", b"h").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mode(&fx.home.join("h.txt")), 0o644);

        // A copy into a share is group-writable (executables stay so);
        // a copy into HOME keeps the source's bits.
        std::fs::create_dir(fx.home.join("tools")).unwrap();
        std::fs::write(fx.home.join("tools/run.sh"), b"#!/bin/sh").unwrap();
        std::fs::set_permissions(
            fx.home.join("tools/run.sh"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let (status, body) = fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/h.txt", "/tools"], "destDir": "/shared/team"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(mode(&team.join("h.txt")), 0o660);
        assert_eq!(mode(&team.join("tools/run.sh")), 0o770);
        let (status, body) = fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/team/a.txt"], "destDir": "/tools"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(mode(&fx.home.join("tools/a.txt")), 0o660);

        // A move from HOME into a share is a copy: a new inode with the
        // share's bits, and the original is gone.
        std::fs::write(fx.home.join("m.txt"), b"m").unwrap();
        let inode = std::fs::metadata(fx.home.join("m.txt")).unwrap().ino();
        let (status, body) = fx
            .post(
                "/api/files/move",
                json!({"paths": ["/m.txt"], "destDir": "/shared/team"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["moved"], 1);
        assert_eq!(body["bytes"], 1);
        assert_eq!(body["homeBytes"], -1);
        assert!(!fx.home.join("m.txt").exists());
        let moved = std::fs::metadata(team.join("m.txt")).unwrap();
        assert_ne!(moved.ino(), inode);
        assert_eq!(moved.permissions().mode() & 0o7777, 0o660);
        // A move inside one namespace is still a rename.
        let inode = moved.ino();
        std::fs::create_dir(team.join("sub")).unwrap();
        let (status, body) = fx
            .post(
                "/api/files/move",
                json!({"paths": ["/shared/team/m.txt"], "destDir": "/shared/team/sub"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            std::fs::metadata(team.join("sub/m.txt")).unwrap().ino(),
            inode
        );
    }

    #[test]
    fn only_containers_set_up_as_shared_folders_are_used() {
        let info = |uid: u32, gid: u32, kind: Kind| Info {
            kind,
            len: 0,
            modified_ms: 0,
            dev: 1,
            ino: 2,
            uid,
            gid,
            mode: 0o2770,
        };
        // The shared root of an install: rast (uid 900, gid 900).
        let root = info(900, 900, Kind::Dir);
        // Legacy containers, and task 1.3's root:rast-s<h>-rw (gid 1500).
        assert!(container_ok(&info(900, 900, Kind::Dir), &root, None));
        assert!(container_ok(&info(0, 1500, Kind::Dir), &root, Some(1500)));
        assert!(container_ok(&info(0, 900, Kind::Dir), &root, None));
        // A folder a user owns, a group of their own, not a folder.
        assert!(!container_ok(&info(1001, 900, Kind::Dir), &root, None));
        assert!(!container_ok(&info(0, 1001, Kind::Dir), &root, Some(1500)));
        assert!(!container_ok(&info(0, 1500, Kind::Dir), &root, None));
        assert!(!container_ok(&info(900, 900, Kind::File), &root, None));
    }

    #[test]
    fn group_ids_come_from_the_group_database() {
        assert_eq!(group_id("root"), Some(0));
        assert_eq!(group_id("rast-s0000000000000000-rw"), None);
        assert_eq!(group_id("bad\0name"), None);
    }

    #[tokio::test]
    async fn chunked_uploads_work_and_report_what_they_replace() {
        let fx = start(None).await;
        std::fs::create_dir(fx.home.join("docs")).unwrap();
        let (status, init) = fx
            .post(
                "/api/files/upload/init",
                json!({"dir": "/docs", "name": "big.bin", "size": 10}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{init}");
        let id = init["uploadId"].as_str().unwrap().to_string();
        assert!(id.starts_with(UPLOAD_PREFIX), "{id}");
        // The temporary file is never listed.
        let (_, listing) = fx.get("/api/files?path=/docs").await;
        assert_eq!(listing["total"], 0);
        for (offset, part) in [(0, &b"hello"[..]), (5, &b"world"[..])] {
            let response = fx
                .send(
                    Method::POST,
                    &format!("/api/files/upload/chunk?uploadId={id}&offset={offset}"),
                    &HeaderMap::new(),
                    Body::from(part),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        // A wrong offset is a conflict; an unknown id is not found.
        let response = fx
            .send(
                Method::POST,
                &format!("/api/files/upload/chunk?uploadId={id}&offset=3"),
                &HeaderMap::new(),
                Body::from("x"),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let (status, _) = fx
            .post("/api/files/upload/complete?uploadId=usr-unknown", json!({}))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let response = fx
            .send(
                Method::POST,
                &format!("/api/files/upload/complete?uploadId={id}"),
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[crate::fileagent::REPLACED_HEADER], "0");
        let entry = client::read_json("file agent", response).await.unwrap();
        assert_eq!(entry["path"], "/docs/big.bin");
        assert_eq!(entry["size"], 10);
        assert_eq!(entry["writable"], true);
        assert_eq!(
            std::fs::read(fx.home.join("docs/big.bin")).unwrap(),
            b"helloworld"
        );

        // A single-request upload over it reports the replaced size.
        let response = fx
            .upload("/api/files/upload?dir=/docs&name=big.bin", b"abc")
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let response = fx
            .upload(
                "/api/files/upload?dir=/docs&name=big.bin&overwrite=true",
                b"abc",
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[crate::fileagent::REPLACED_HEADER], "10");
        let leftovers: Vec<_> = std::fs::read_dir(fx.home.join("docs"))
            .unwrap()
            .flatten()
            .map(|item| item.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        // A ranged download streams the part asked for.
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=1-".parse().unwrap());
        let part = fx
            .send(
                Method::GET,
                "/api/files/download?path=/docs/big.bin",
                &headers,
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        let bytes = axum::body::to_bytes(part.into_body(), 64).await.unwrap();
        assert_eq!(&bytes[..], b"bc");
    }

    #[tokio::test]
    async fn transfers_into_home_respect_the_budget_and_report_home_bytes() {
        let fx = start(None).await;
        std::fs::create_dir(fx.home.join("docs")).unwrap();
        std::fs::write(fx.home.join("a.bin"), [0u8; 10]).unwrap();
        std::fs::create_dir(fx.home.join("tree")).unwrap();
        std::fs::write(fx.home.join("tree/b.bin"), [0u8; 6]).unwrap();
        let team = fx.shared.join("team-1");
        std::fs::create_dir(&team).unwrap();
        fx.share("Team", "team-1");
        let budget = |bytes: u64| {
            let mut headers = HeaderMap::new();
            headers.insert(fileagent::QUOTA_HEADER, bytes.into());
            headers
        };

        // A copy that does not fit is refused before anything is written.
        let (status, error) = fx
            .post_with(
                "/api/files/copy",
                json!({"paths": ["/a.bin"], "destDir": "/docs"}),
                budget(9),
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{error}");
        assert!(!fx.home.join("docs/a.bin").exists());
        let (status, copied) = fx
            .post_with(
                "/api/files/copy",
                json!({"paths": ["/a.bin", "/tree"], "destDir": "/docs"}),
                budget(16),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{copied}");
        assert_eq!(copied["copied"], 2);
        assert_eq!(copied["bytes"], 16);
        assert_eq!(copied["homeBytes"], 16);

        // Moves inside HOME change nothing; out of HOME they free space,
        // back into HOME they need it again.
        let (_, moved) = fx
            .post(
                "/api/files/move",
                json!({"paths": ["/docs/a.bin"], "destDir": "/tree"}),
            )
            .await;
        assert_eq!(moved["homeBytes"], 0, "{moved}");
        let (status, moved) = fx
            .post(
                "/api/files/move",
                json!({"paths": ["/tree"], "destDir": "/shared/Team"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{moved}");
        assert_eq!(moved["homeBytes"], -16);
        assert!(team.join("tree/b.bin").exists());
        let (status, _) = fx
            .post_with(
                "/api/files/move",
                json!({"paths": ["/shared/Team/tree"], "destDir": "/"}),
                budget(15),
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (_, moved) = fx
            .post_with(
                "/api/files/move",
                json!({"paths": ["/shared/Team/tree"], "destDir": "/"}),
                budget(16),
            )
            .await;
        assert_eq!(moved["homeBytes"], 16, "{moved}");
        assert_eq!(moved["bytes"], 16);
    }

    /// Lines of a watch stream, as JSON, until `stop` says so or 10 s pass.
    async fn watch_until(
        body: &mut axum::body::BodyDataStream,
        buffer: &mut Vec<u8>,
        mut stop: impl FnMut(&Value) -> bool,
    ) -> Vec<Value> {
        use futures::StreamExt;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                let value: Value = serde_json::from_slice(&line).unwrap();
                let done = stop(&value);
                seen.push(value);
                if done {
                    return seen;
                }
            }
            match tokio::time::timeout_at(deadline, body.next()).await {
                Ok(Some(Ok(bytes))) => buffer.extend_from_slice(&bytes),
                other => panic!("watch stream ended early: {other:?}, seen {seen:?}"),
            }
        }
    }

    fn has_path(value: &Value, path: &str) -> bool {
        value["paths"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|item| item == path))
    }

    #[tokio::test]
    async fn the_watch_stream_reports_changes_in_the_visible_tree() {
        let fx = start(None).await;
        std::fs::create_dir_all(fx.home.join("docs")).unwrap();
        std::fs::create_dir_all(fx.home.join(".cache/app")).unwrap();
        let response = fx
            .send(
                Method::GET,
                "/api/files/watch",
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        let mut body = response.into_body().into_data_stream();
        let mut buffer = Vec::new();
        watch_until(&mut body, &mut buffer, |value| value["ready"] == true).await;

        // Hidden folders are not watched; visible ones are.
        std::fs::write(fx.home.join(".cache/app/blob"), b"x").unwrap();
        std::fs::write(fx.home.join("docs/a.txt"), b"a").unwrap();
        let seen = watch_until(&mut body, &mut buffer, |value| {
            has_path(value, "/docs/a.txt")
        })
        .await;
        assert!(
            seen.iter()
                .all(|value| !value.to_string().contains("cache")),
            "{seen:?}"
        );

        // A folder created later is watched too.
        std::fs::create_dir(fx.home.join("projects")).unwrap();
        watch_until(&mut body, &mut buffer, |value| has_path(value, "/projects")).await;
        std::fs::write(fx.home.join("projects/b.txt"), b"b").unwrap();
        watch_until(&mut body, &mut buffer, |value| {
            has_path(value, "/projects/b.txt")
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // Races in shared folders: another member changes the tree while the
    // agent works (ADR 0005, section 2). The test plays that member at a
    // point in the middle of the operation.
    // -----------------------------------------------------------------------

    use super::fsat::{race, Point};

    /// A shared folder `Team` that alice reaches, alice's private key and
    /// her autostart folder (what another member would like to reach).
    struct RaceFixture {
        fx: Fixture,
        team: PathBuf,
        key: PathBuf,
        autostart: PathBuf,
    }

    async fn race_fixture() -> RaceFixture {
        let fx = start(None).await;
        let team = fx.shared.join("team-1");
        std::fs::create_dir(&team).unwrap();
        fx.share("Team", "team-1");
        std::fs::create_dir_all(fx.home.join(".ssh")).unwrap();
        let key = fx.home.join(".ssh/id_ed25519");
        std::fs::write(&key, b"PRIVATE KEY").unwrap();
        let autostart = fx.home.join(".config/autostart");
        std::fs::create_dir_all(&autostart).unwrap();
        RaceFixture {
            fx,
            team,
            key,
            autostart,
        }
    }

    /// Replace `path` by a link to `target`, keeping the original as
    /// `<path>-moved`.
    fn swap_for_link(path: &Path, target: &Path) {
        let moved = PathBuf::from(format!("{}-moved", path.display()));
        std::fs::rename(path, moved).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// Every file below `dir` whose content is `needle`.
    fn files_with(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
        walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .flatten()
            .filter(|item| item.file_type().is_file())
            .filter(|item| std::fs::read(item.path()).is_ok_and(|bytes| bytes == needle))
            .map(|item| item.path().to_path_buf())
            .collect()
    }

    #[tokio::test]
    async fn a_source_file_swapped_for_a_link_is_never_read() {
        let rx = race_fixture().await;
        let source = rx.team.join("race-report.txt");
        std::fs::write(&source, b"report").unwrap();
        std::fs::create_dir(rx.team.join("out")).unwrap();
        // Checked as a regular file, then a link to alice's key.
        let key = rx.key.clone();
        race::at(Point::OpenSource, "race-report.txt", move || {
            swap_for_link(&source, &key)
        });
        let (status, body) = rx
            .fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/Team/race-report.txt"], "destDir": "/shared/Team/out"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(files_with(&rx.team, b"PRIVATE KEY").is_empty());
        assert!(!rx.team.join("out/race-report.txt").exists());
    }

    #[tokio::test]
    async fn a_source_folder_swapped_for_a_link_is_never_walked() {
        let rx = race_fixture().await;
        let docs = rx.team.join("race-docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), b"a").unwrap();
        std::fs::create_dir(rx.team.join("out")).unwrap();
        let ssh = rx.key.parent().unwrap().to_path_buf();
        race::at(Point::OpenSource, "race-docs", move || {
            swap_for_link(&docs, &ssh)
        });
        let (status, body) = rx
            .fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/Team/race-docs"], "destDir": "/shared/Team/out"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(files_with(&rx.team, b"PRIVATE KEY").is_empty());
        assert!(!rx.team.join("out/race-docs").exists());
    }

    #[tokio::test]
    async fn a_folder_swapped_for_a_link_inside_a_copied_tree_is_never_walked() {
        let rx = race_fixture().await;
        let tree = rx.team.join("race-tree");
        std::fs::create_dir_all(tree.join("race-sub")).unwrap();
        std::fs::write(tree.join("race-sub/b.txt"), b"b").unwrap();
        std::fs::create_dir(rx.team.join("out")).unwrap();
        let (sub, ssh) = (
            tree.join("race-sub"),
            rx.key.parent().unwrap().to_path_buf(),
        );
        race::at(Point::OpenSource, "race-sub", move || {
            swap_for_link(&sub, &ssh)
        });
        let (status, body) = rx
            .fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/Team/race-tree"], "destDir": "/shared/Team/out"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(files_with(&rx.team, b"PRIVATE KEY").is_empty());
        // The partial copy is gone.
        assert!(!rx.team.join("out/race-tree").exists());
    }

    #[tokio::test]
    async fn a_destination_folder_swapped_for_a_link_is_never_written_through() {
        let rx = race_fixture().await;
        std::fs::write(rx.fx.home.join("race-notes.txt"), b"notes").unwrap();
        let inbox = rx.team.join("race-inbox");
        std::fs::create_dir(&inbox).unwrap();
        let autostart = rx.autostart.clone();
        race::at(Point::CreateTarget, "race-notes.txt", move || {
            swap_for_link(&inbox, &autostart)
        });
        let (status, body) = rx
            .fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/race-notes.txt"], "destDir": "/shared/Team/race-inbox"}),
            )
            .await;
        // The copy lands in the folder that was checked, wherever it is now.
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            std::fs::read(rx.team.join("race-inbox-moved/race-notes.txt")).unwrap(),
            b"notes"
        );
        assert_eq!(std::fs::read_dir(&rx.autostart).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_new_folder_swapped_for_a_link_is_never_written_into() {
        let rx = race_fixture().await;
        let project = rx.fx.home.join("race-proj");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("run.desktop"), b"[Desktop Entry]").unwrap();
        std::fs::create_dir(rx.team.join("out")).unwrap();
        let (made, autostart) = (rx.team.join("out/race-proj"), rx.autostart.clone());
        race::at(Point::MadeDir, "race-proj", move || {
            swap_for_link(&made, &autostart)
        });
        let (status, body) = rx
            .fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/race-proj"], "destDir": "/shared/Team/out"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(std::fs::read_dir(&rx.autostart).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_upload_lands_in_the_folder_it_was_started_in() {
        let rx = race_fixture().await;
        let inbox = rx.team.join("race-up");
        std::fs::create_dir(&inbox).unwrap();
        let (status, init) = rx
            .fx
            .post(
                "/api/files/upload/init",
                json!({"dir": "/shared/Team/race-up", "name": "run.desktop", "size": 3}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{init}");
        let id = init["uploadId"].as_str().unwrap().to_string();
        // Between two requests, the folder becomes a link to autostart.
        swap_for_link(&inbox, &rx.autostart);
        let response = rx
            .fx
            .send(
                Method::POST,
                &format!("/api/files/upload/chunk?uploadId={id}&offset=0"),
                &HeaderMap::new(),
                Body::from("abc"),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = rx
            .fx
            .send(
                Method::POST,
                &format!("/api/files/upload/complete?uploadId={id}"),
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(rx.team.join("race-up-moved/run.desktop")).unwrap(),
            b"abc"
        );
        assert_eq!(std::fs::read_dir(&rx.autostart).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn only_the_service_with_the_token_is_answered() {
        // Another peer uid: the connection is dropped before any reply.
        let fx = start(Some(euid().wrapping_add(1))).await;
        let refused = fx
            .send(Method::GET, "/api/files", &HeaderMap::new(), Body::empty())
            .await;
        assert!(
            matches!(refused, Err(AppError::Unavailable(_))),
            "{refused:?}"
        );

        // The right peer uid with a wrong token: 403.
        let fx = start(Some(euid())).await;
        let stream = tokio::net::UnixStream::connect(&fx.socket).await.unwrap();
        let response = client::send(
            "file agent",
            stream,
            &"0".repeat(64),
            Method::GET,
            "/api/files",
            &HeaderMap::new(),
            Body::empty(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let (status, _) = fx.get("/api/files").await;
        assert_eq!(status, StatusCode::OK);
    }
}
