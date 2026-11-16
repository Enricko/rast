//! The per-user file agent (`rast file-agent`): the File Station API over one
//! user's HOME, run as that user (`rast-u<id>`) in the socket-activated unit
//! `rast-files-u<uid>` that the helper starts (`files_agent_start`).
//!
//! Namespace (ADR 0005, section 2):
//!
//! * `/` is HOME;
//! * `/shared/<name>/…` resolves through the symlink `~/Shared/<name>`, whose
//!   target must be a direct child of the shared root;
//! * `/@root` is never served here.
//!
//! Every path goes through the safe-path layer of `rast-base` (no traversal,
//! symlinks refused on every component, `O_NOFOLLOW` on the last one), and
//! the operations are the same database-free cores the server runs for
//! legacy homes, so the JSON shapes are identical. The agent's uid is the
//! hard boundary: a bug or a symlink race here only reaches what the user
//! can reach anyway. The server has already checked the session, CSRF and
//! the RAST access rules before it forwards a request.

use std::io;
use std::os::unix::ffi::OsStrExt;
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
    self, blocking, check, prepare_replacement, read_token, Agent, CancelOnDrop, PeerListener,
    Target, UploadSpace,
};
use crate::storage::ops::{self, EntryDto, ListingDto, QuotaDto, SearchDto, TextPreviewDto};
use crate::storage::safe_path::{self, validate_component, validate_new_name};
use crate::storage::serve::{serve_file, Disposition, PREVIEW_CSP};
use crate::storage::{
    transfer, Access, LogicalPath, Perm, ResolvedPath, RootKind, StorageRoot, MAX_BATCH_PATHS,
    MAX_TREE_ENTRIES, TEMP_PREFIX,
};

use super::{SHARED_DIR, UPLOAD_PREFIX};

/// Where `~/Shared/<name>` links may point.
fn share_target_ok(shared_root: &Path, target: &Path) -> bool {
    target.is_absolute()
        && target.parent() == Some(shared_root)
        && target
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| validate_component(name).is_ok())
}

/// The namespace of one HOME.
#[derive(Debug, Clone)]
pub struct HomeSpace {
    home: PathBuf,
    shared_root: PathBuf,
}

impl HomeSpace {
    /// Both roots must be real directories; they are canonicalised once so
    /// containment checks compare resolved paths.
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
        Ok(Self {
            home: std::fs::canonicalize(home)?,
            shared_root: std::fs::canonicalize(shared_root)?,
        })
    }

    /// The (canonical) HOME directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn home_root(&self) -> StorageRoot {
        StorageRoot {
            physical: self.home.clone(),
            kind: RootKind::Home,
            perm: Perm::Admin,
            in_home: true,
            share_id: None,
            share_name: None,
        }
    }

    /// The target of `~/Shared/<name>`, when it is a valid share link.
    fn share_link(&self, name: &str) -> Option<PathBuf> {
        validate_component(name).ok()?;
        let shared = self.home.join(SHARED_DIR);
        let meta = std::fs::symlink_metadata(&shared).ok()?;
        if !meta.is_dir() {
            return None;
        }
        let link = shared.join(name);
        if !std::fs::symlink_metadata(&link)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
        let target = std::fs::read_link(&link).ok()?;
        if !share_target_ok(&self.shared_root, &target) {
            return None;
        }
        // The container itself must be a real directory, not another link.
        std::fs::symlink_metadata(&target)
            .ok()
            .filter(|meta| meta.is_dir())
            .map(|_| target)
    }

    /// The namespace root of a logical path.
    fn root_for(&self, logical: &LogicalPath) -> AppResult<StorageRoot> {
        let first = logical.segments().first().map(String::as_str);
        if first == Some(safe_path::RESERVED_NAME) {
            return Err(AppError::bad_request(
                "the root file system is not served by the file agent",
            ));
        }
        if let Some((name, _)) = logical.shared_parts() {
            let physical = self
                .share_link(name)
                .ok_or(AppError::NotFound("path not found"))?;
            return Ok(StorageRoot {
                physical,
                kind: RootKind::Share,
                // The kernel decides what the user may write; the server has
                // already applied the RAST access rules.
                perm: Perm::Write,
                in_home: false,
                share_id: None,
                share_name: Some(name.to_string()),
            });
        }
        if first == Some("shared") {
            return Err(AppError::NotFound("path not found"));
        }
        Ok(self.home_root())
    }

    /// Resolve an existing logical path.
    pub fn resolve(&self, path: &str, access: Access) -> AppResult<ResolvedPath> {
        let logical = LogicalPath::parse(path)?;
        self.resolve_logical(&logical, access)
    }

    fn resolve_logical(&self, logical: &LogicalPath, access: Access) -> AppResult<ResolvedPath> {
        self.root_for(logical)?.resolve(logical, access)
    }
}

/// Whether this process may write `path` (`access(W_OK)`, effective ids).
fn writable(path: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::W_OK, libc::AT_EACCESS) == 0 }
}

/// An I/O error of the agent (a full quota is 413, see `AppError::from`).
fn fs_error(error: io::Error) -> AppError {
    error.into()
}

impl UploadSpace for HomeSpace {
    const ID_PREFIX: &'static str = UPLOAD_PREFIX;
    const TEMP_PREFIX: &'static str = TEMP_PREFIX;
    const DEFAULT_DIR: &'static str = "/";
    const REPORTS_REPLACED: bool = true;

    fn target(&self, dir: &str, name: &str, overwrite: bool) -> AppResult<Target> {
        validate_new_name(name)?;
        let parent = self.resolve(dir, Access::Write)?;
        if !parent.physical.is_dir() {
            return Err(AppError::bad_request(
                "upload destination is not a directory",
            ));
        }
        let (logical, dest, exists) = parent.child_target(name)?;
        if exists {
            let meta = std::fs::symlink_metadata(&dest)?;
            if meta.is_dir() {
                return Err(AppError::conflict("a folder with that name already exists"));
            }
            if !overwrite {
                return Err(AppError::conflict("a file with that name already exists"));
            }
        }
        Ok(Target {
            dest,
            logical: logical.as_string(),
        })
    }

    fn commit(&self, temp: &Path, target: &Target, overwrite: bool) -> AppResult<(EntryDto, u64)> {
        let replaced = prepare_replacement(temp, &target.dest, overwrite)?.unwrap_or(0);
        // `rename` replaces a link planted at the destination, never follows it.
        std::fs::rename(temp, &target.dest).map_err(fs_error)?;
        let logical = LogicalPath::parse(&target.logical)?;
        let meta = std::fs::symlink_metadata(&target.dest)?;
        let name = logical.name().unwrap_or_default().to_string();
        let mut entry = <EntryDto as ops::EntryDtoExt>::new(&logical, &name, &meta, true);
        entry.writable = writable(&target.dest);
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
        .route(fileagent::WATCH_ROUTE, get(super::watch::stream))
        .merge(fileagent::upload_routes::<HomeSpace>())
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn_with_state(
            agent.clone(),
            fileagent::guard::<HomeSpace>,
        ))
        .with_state(agent)
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
}

fn page_params(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    (
        page.unwrap_or(1).max(1),
        per_page
            .unwrap_or(crate::storage::DEFAULT_PAGE_SIZE)
            .clamp(1, crate::storage::MAX_PAGE_SIZE),
    )
}

/// Valid share links in `~/Shared`, as folders named `/shared/<name>`.
fn share_link_entries(space: &HomeSpace, needle: Option<&str>) -> Vec<EntryDto> {
    let Ok(reader) = std::fs::read_dir(space.home.join(SHARED_DIR)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in reader.flatten() {
        let Some(name) = item.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if needle.is_some_and(|needle| !name.to_lowercase().contains(needle)) {
            continue;
        }
        let Some(target) = space.share_link(&name) else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        let Ok(logical) = LogicalPath::parse(&format!("/shared/{name}")) else {
            continue;
        };
        let mut entry = <EntryDto as ops::EntryDtoExt>::new(&logical, &name, &meta, true);
        entry.writable = writable(&target);
        out.push(entry);
    }
    out
}

async fn list(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<ListingDto>> {
    let space = agent.space.clone();
    let path = query.path.clone().unwrap_or_else(|| "/".to_string());
    let (page, per_page) = page_params(query.page, query.per_page);
    let resolved = space.resolve(&path, Access::Read)?;
    let sort = query.sort.as_deref().unwrap_or("name");
    let order = query.order.as_deref().unwrap_or("asc");
    let mut entries = ops::read_entries(&resolved, query.q.as_deref(), sort, order).await?;
    for entry in &mut entries {
        entry.writable = writable(&resolved.physical.join(&entry.name));
    }
    if resolved.kind == RootKind::Home && resolved.logical.segments() == [SHARED_DIR] {
        let needle = query
            .q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        entries.extend(share_link_entries(&space, needle.as_deref()));
        ops::sort_entries(&mut entries, sort, order);
    }
    let (items, page, per_page, total) = ops::paginate(entries, page, per_page);
    Ok(Json(ListingDto {
        path: resolved.logical.as_string(),
        breadcrumbs: ops::breadcrumbs_for(&resolved),
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
    }))
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: Option<String>,
    #[serde(rename = "maxBytes", alias = "max_bytes")]
    max_bytes: Option<u64>,
}

fn required(path: &Option<String>) -> AppResult<&str> {
    path.as_deref()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| AppError::bad_request("path is required"))
}

async fn stat(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
) -> AppResult<Json<EntryDto>> {
    let resolved = agent.space.resolve(required(&query.path)?, Access::Read)?;
    let mut entry = ops::entry_for_resolved(&resolved).await?;
    entry.writable = writable(&resolved.physical);
    Ok(Json(entry))
}

async fn search(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<SearchDto>> {
    let (page, per_page) = page_params(query.page, query.per_page);
    let q = query
        .q
        .as_deref()
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| AppError::bad_request("q is required"))?;
    let base = agent
        .space
        .resolve(query.path.as_deref().unwrap_or("/"), Access::Read)?;
    let mut found = ops::search_in(&base, q, query.kind.as_deref(), page, per_page).await?;
    for entry in &mut found.items {
        if let Ok(logical) = LogicalPath::parse(&entry.path) {
            if let Ok(physical) =
                safe_path::physical_path(&base.root_physical, logical.relative_segments())
            {
                entry.writable = writable(&physical);
            }
        }
    }
    Ok(Json(found))
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
    Json(body): Json<MkdirRequest>,
) -> AppResult<Json<EntryDto>> {
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
    let parent = agent
        .space
        .resolve_logical(&parent_logical, Access::Write)?;
    let name = logical
        .name()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    ops::mkdir_in(&parent, name).await.map(Json)
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    path: String,
    #[serde(rename = "newName", alias = "new_name")]
    new_name: String,
}

async fn rename(
    State(agent): State<Arc<HomeAgent>>,
    Json(body): Json<RenameRequest>,
) -> AppResult<Json<EntryDto>> {
    let resolved = agent.space.resolve(&body.path, Access::Write)?;
    if resolved.is_storage_root() {
        return Err(AppError::bad_request("cannot rename a storage root"));
    }
    let parent_logical = resolved
        .logical
        .parent()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let new_name = body.new_name.trim();
    validate_new_name(new_name)?;
    if new_name == resolved.name() {
        return ops::entry_for_resolved(&resolved).await.map(Json);
    }
    let parent = agent
        .space
        .resolve_logical(&parent_logical, Access::Write)?;
    ops::rename_in(&resolved, &parent, new_name).await.map(Json)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

async fn regular_file(agent: &HomeAgent, query: &PathQuery) -> AppResult<ResolvedPath> {
    let resolved = agent.space.resolve(required(&query.path)?, Access::Read)?;
    ops::ensure_regular_file(&resolved).await?;
    Ok(resolved)
}

async fn download(
    State(agent): State<Arc<HomeAgent>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let resolved = regular_file(&agent, &query).await?;
    let content_type = ops::mime_for_name(resolved.name());
    serve_file(
        &resolved,
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
    let resolved = regular_file(&agent, &query).await?;
    let name = resolved.name().to_string();
    let kind = ops::preview_kind(&name).ok_or(AppError::UnsupportedMediaType(
        "preview is not supported for this file type",
    ))?;
    let content_type = ops::preview_content_type(&name, kind);
    serve_file(
        &resolved,
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
) -> AppResult<Json<TextPreviewDto>> {
    let resolved = regular_file(&agent, &query).await?;
    let limit = query
        .max_bytes
        .unwrap_or(crate::storage::DEFAULT_TEXT_PREVIEW_BYTES)
        .clamp(1, crate::storage::MAX_TEXT_PREVIEW_BYTES);
    ops::text_preview_of(&resolved, Some(limit)).await.map(Json)
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

/// One planned source and where it lands.
struct Planned {
    source: PathBuf,
    target: PathBuf,
    /// Whether the source is in HOME (not in a shared folder).
    in_home: bool,
}

/// The free quota the server sent with a copy or move into HOME (absent:
/// no limit).
fn quota_budget(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(fileagent::QUOTA_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// Bytes a source occupies, as the server's quota counts them.
fn tree_size(path: &Path) -> AppResult<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(if meta.is_dir() {
        crate::storage::quota::dir_used_bytes_blocking(path)
    } else {
        meta.len()
    })
}

/// The checks of `transfer::plan_batch`, against this namespace.
fn plan(
    space: &HomeSpace,
    body: &BatchRequest,
    moving: bool,
) -> AppResult<(ResolvedPath, Vec<Planned>)> {
    if body.paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    if body.paths.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many paths in one request"));
    }
    let dest = space.resolve(&body.dest_dir, Access::Write)?;
    if !dest.physical.is_dir() {
        return Err(AppError::bad_request("destination is not a directory"));
    }
    let access = if moving { Access::Write } else { Access::Read };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(body.paths.len());
    for raw in &body.paths {
        let resolved = space.resolve(raw, access)?;
        if resolved.is_storage_root() {
            return Err(AppError::bad_request(
                "storage roots cannot be copied, moved or deleted",
            ));
        }
        if !seen.insert(resolved.physical.clone()) {
            return Err(AppError::bad_request("duplicate paths in request"));
        }
        let name = resolved.name().to_string();
        let (_, target, exists) = dest.child_target(&name)?;
        if exists {
            return Err(AppError::conflict(format!(
                "\"{name}\" already exists in the destination"
            )));
        }
        if target == resolved.physical {
            return Err(AppError::conflict("source and destination are the same"));
        }
        if resolved.physical.is_dir() && target.starts_with(&resolved.physical) {
            return Err(AppError::bad_request(
                "a folder cannot be copied or moved into itself",
            ));
        }
        out.push(Planned {
            in_home: resolved.kind == RootKind::Home,
            source: resolved.physical,
            target,
        });
    }
    Ok((dest, out))
}

/// Copy a tree without following links: symlinks, temporary files and
/// special files are left out, as in-process copies do.
fn copy_tree(source: &Path, target: &Path, cancel: &AtomicBool, bytes: &mut u64) -> AppResult<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        return Err(AppError::Forbidden("symbolic links are not supported"));
    }
    if !meta.is_dir() {
        check(cancel)?;
        *bytes += std::fs::copy(source, target).map_err(fs_error)?;
        return Ok(());
    }
    let mut entries = 0usize;
    for item in walkdir::WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|item| !item.file_type().is_symlink())
    {
        check(cancel)?;
        let item = item.map_err(|error| {
            error
                .into_io_error()
                .map(AppError::from)
                .unwrap_or_else(|| AppError::internal("cannot traverse directory tree"))
        })?;
        if item
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            continue;
        }
        entries += 1;
        if entries > MAX_TREE_ENTRIES {
            return Err(AppError::bad_request(
                "folder is too large for a single copy or move operation",
            ));
        }
        let relative = item
            .path()
            .strip_prefix(source)
            .map_err(|_| AppError::internal("path escaped its tree root"))?;
        let destination = target.join(relative);
        if item.file_type().is_dir() {
            std::fs::create_dir(&destination).map_err(fs_error)?;
        } else if item.file_type().is_file() {
            *bytes += std::fs::copy(item.path(), &destination).map_err(fs_error)?;
        }
    }
    Ok(())
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
    blocking(move || {
        let (dest, plan) = plan(&space, &body, moving)?;
        let dest_in_home = dest.kind == RootKind::Home;
        // Sizes first: the whole batch must fit, so a full quota writes
        // nothing (in-process transfers stop at the item that does not fit).
        let mut sizes = Vec::with_capacity(plan.len());
        let mut needed = 0u64;
        for item in &plan {
            check(&cancel)?;
            let into_home = dest_in_home && (!moving || !item.in_home);
            let size = if moving || into_home {
                tree_size(&item.source)?
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
        for (item, size) in plan.iter().zip(sizes) {
            check(&cancel)?;
            let into_home = dest_in_home && (!moving || !item.in_home);
            let out_of_home = moving && item.in_home && !dest_in_home;
            let delta = i64::try_from(size).unwrap_or(i64::MAX);
            if into_home {
                home_bytes = home_bytes.saturating_add(delta);
            } else if out_of_home {
                home_bytes = home_bytes.saturating_sub(delta);
            }
            if moving {
                bytes = bytes.saturating_add(size);
                match std::fs::rename(&item.source, &item.target) {
                    Ok(()) => {
                        done += 1;
                        continue;
                    }
                    // Another file system: copy, then remove the original.
                    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
                    Err(error) => return Err(fs_error(error)),
                }
            }
            let mut copied = 0u64;
            if let Err(error) = copy_tree(&item.source, &item.target, &cancel, &mut copied) {
                // Do not leave half a copy behind.
                let _ = transfer::remove_path_blocking(&item.target);
                return Err(error);
            }
            if moving {
                transfer::remove_path_blocking(&item.source).map_err(fs_error)?;
            } else {
                bytes = bytes.saturating_add(copied);
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

    struct Fixture {
        dir: tempfile::TempDir,
        home: PathBuf,
        shared: PathBuf,
        socket: PathBuf,
        token: String,
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
        }
    }

    impl Fixture {
        async fn send(
            &self,
            method: Method,
            uri: &str,
            headers: &HeaderMap,
            body: Body,
        ) -> AppResult<Response> {
            let stream = tokio::net::UnixStream::connect(&self.socket).await?;
            client::send(
                "file agent",
                stream,
                &self.token,
                method,
                uri,
                headers,
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
    async fn shared_folders_resolve_only_through_valid_links() {
        let fx = start(None).await;
        let team = fx.shared.join("team-1");
        std::fs::create_dir(&team).unwrap();
        std::fs::write(team.join("plan.txt"), b"plan").unwrap();
        std::fs::create_dir(fx.shared.join("team-1/sub")).unwrap();
        let links = fx.home.join(SHARED_DIR);
        std::fs::create_dir(&links).unwrap();
        std::os::unix::fs::symlink(&team, links.join("Team")).unwrap();
        // Invalid targets: outside, nested, relative, a link in the root.
        std::os::unix::fs::symlink(fx.dir.path(), links.join("Up")).unwrap();
        std::os::unix::fs::symlink(team.join("sub"), links.join("Nested")).unwrap();
        std::os::unix::fs::symlink("../../shared/team-1", links.join("Relative")).unwrap();
        std::os::unix::fs::symlink(fx.dir.path(), fx.shared.join("alias")).unwrap();
        std::os::unix::fs::symlink(fx.shared.join("alias"), links.join("Alias")).unwrap();

        let (status, listing) = fx.get("/api/files?path=/shared/Team").await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(listing["path"], "/shared/Team");
        assert_eq!(listing["breadcrumbs"][0]["path"], "/shared/Team");
        assert_eq!(listing["items"][0]["path"], "/shared/Team/plan.txt");
        let (status, entry) = fx.get("/api/files/stat?path=/shared/Team").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(entry["name"], "Team");
        for name in ["Up", "Nested", "Relative", "Alias", "Missing"] {
            let (status, _) = fx.get(&format!("/api/files?path=/shared/{name}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{name}");
        }
        // `~/Shared` lists the valid links as folders named /shared/<name>.
        let (_, links_listing) = fx.get("/api/files?path=/Shared").await;
        let items = links_listing["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{links_listing}");
        assert_eq!(items[0]["path"], "/shared/Team");
        assert_eq!(items[0]["kind"], "dir");

        // Copy from a share into HOME stays inside the agent.
        let (status, copied) = fx
            .post(
                "/api/files/copy",
                json!({"paths": ["/shared/Team/plan.txt"], "destDir": "/"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{copied}");
        assert_eq!(copied["copied"], 1);
        assert_eq!(std::fs::read(fx.home.join("plan.txt")).unwrap(), b"plan");

        // A symlinked ~/Shared is not followed either.
        std::fs::rename(&links, fx.home.join("old-links")).unwrap();
        std::os::unix::fs::symlink(fx.home.join("old-links"), &links).unwrap();
        let (status, _) = fx.get("/api/files?path=/shared/Team").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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
        std::fs::create_dir(fx.home.join(SHARED_DIR)).unwrap();
        std::os::unix::fs::symlink(&team, fx.home.join("Shared/Team")).unwrap();
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
