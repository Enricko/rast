//! The root file agent: the File Station API over the server's own file
//! system, run as root in a transient unit (`rast rootfs-agent`).
//!
//! It answers on a Unix socket in a directory only root and the RAST service
//! group can enter, and requires the token the helper wrote for this run.
//! The agent trusts its caller: the API layer has already checked that the
//! request comes from a super administrator. It exits when idle.
//!
//! Paths are the logical `/@root/...` paths (see [`super::physical`]), and
//! the JSON shapes are those of `/api/files`, so the API layer forwards
//! requests and responses unchanged.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Query;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::error::{AppError, AppResult};
use crate::fileagent::{
    self, adopt_parent_owner, blocking, check, file_name, io_error, prepare_replacement,
    read_token, Agent, CancelOnDrop, PeerListener, Target, UploadSpace,
};
use crate::storage::ops::{
    self, BreadcrumbDto, EntryDto, ListingDto, QuotaDto, SearchDto, TextPreviewDto,
};
use crate::storage::serve::{serve_path, Disposition, PREVIEW_CSP};

use super::{logical, physical, UPLOAD_PREFIX};

/// Temporary upload files (`.rast-upload-<id>` in the destination folder).
const TEMP_PREFIX: &str = ".rast-upload-";
/// A search stops after this many entries or this long.
const SEARCH_VISIT_LIMIT: usize = 400_000;
const SEARCH_MATCH_LIMIT: usize = 2_000;
const SEARCH_TIME_LIMIT: Duration = Duration::from_secs(10);
/// Kernel and runtime trees a search from higher up does not descend into.
const SEARCH_SKIP: &[&str] = &["/proc", "/sys", "/dev", "/run"];

/// Uploads of the root agent: anywhere on the file system, owned by the
/// owner of the folder they land in.
struct RootSpace;

impl UploadSpace for RootSpace {
    const ID_PREFIX: &'static str = UPLOAD_PREFIX;
    const TEMP_PREFIX: &'static str = TEMP_PREFIX;
    const DEFAULT_DIR: &'static str = super::PREFIX;
    const REPORTS_REPLACED: bool = false;

    type Target = Target;
    type Temp = PathBuf;

    fn target(&self, dir: &str, name: &str, overwrite: bool) -> AppResult<Target> {
        let dest = upload_target(dir, name, overwrite)?;
        Ok(Target {
            logical: logical(&dest),
            dest,
        })
    }

    fn create_temp(&self, target: &Target, id: &str) -> AppResult<(PathBuf, std::fs::File)> {
        fileagent::create_temp_path(&target.dest, Self::TEMP_PREFIX, id)
    }

    fn open_temp(&self, temp: &PathBuf) -> AppResult<std::fs::File> {
        fileagent::open_temp_path(temp)
    }

    fn remove_temp(&self, temp: &PathBuf) {
        let _ = std::fs::remove_file(temp);
    }

    fn commit(
        &self,
        temp: &PathBuf,
        target: &Target,
        overwrite: bool,
    ) -> AppResult<(EntryDto, u64)> {
        commit(temp, &target.dest, overwrite).map(|entry| (entry, 0))
    }
}

/// Run the agent until it has been idle for `idle`.
pub async fn run(socket: PathBuf, token_file: PathBuf, idle: Duration) -> io::Result<()> {
    let token = read_token(&token_file)?;
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    // The directory (root:rast, setgid, 0750) already limits who can reach
    // the socket; the group needs write access to connect.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    let agent = Agent::new(token, RootSpace);
    let app = router(agent.clone());
    let result = fileagent::serve(PeerListener::new(listener, None), app, agent, idle, || {
        false
    })
    .await;
    let _ = std::fs::remove_file(&socket);
    result
}

fn router(agent: Arc<Agent<RootSpace>>) -> Router {
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
        .route("/api/files/delete", post(delete))
        .merge(fileagent::upload_routes::<RootSpace>())
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn_with_state(
            agent.clone(),
            fileagent::guard::<RootSpace>,
        ))
        .with_state(agent)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Metadata of what a path leads to (a symbolic link's target when it
/// resolves, the link itself otherwise).
fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path).or_else(|_| std::fs::symlink_metadata(path))
}

fn entry(path: &Path, meta: &std::fs::Metadata) -> EntryDto {
    let name = file_name(path);
    let is_dir = meta.is_dir();
    EntryDto {
        path: logical(path),
        kind: if is_dir { "dir" } else { "file" }.to_string(),
        size: if is_dir {
            0
        } else {
            i64::try_from(meta.len()).unwrap_or(i64::MAX)
        },
        modified_at: ops::modified_ms(meta),
        mime: if is_dir {
            String::new()
        } else {
            ops::mime_for_name(&name)
        },
        readable: true,
        writable: true,
        trashed: false,
        name,
    }
}

fn stat_entry(path: &Path) -> AppResult<EntryDto> {
    let meta = metadata(path).map_err(|error| io_error("read", path, error))?;
    Ok(entry(path, &meta))
}

/// A name for a new entry: one path segment, nothing that could leave the
/// folder.
fn valid_name(name: &str) -> AppResult<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
        || name.len() > 255
    {
        return Err(AppError::bad_request("invalid name"));
    }
    Ok(name)
}

fn require_dir(path: &Path) -> AppResult<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(AppError::bad_request(format!(
            "{} is not a folder",
            path.display()
        ))),
        Err(error) => Err(io_error("open", path, error)),
    }
}

fn breadcrumbs(path: &Path) -> Vec<BreadcrumbDto> {
    let mut out = vec![BreadcrumbDto {
        name: "Root file system".to_string(),
        path: logical(Path::new("/")),
    }];
    let mut current = PathBuf::from("/");
    for part in path.components() {
        if let std::path::Component::Normal(name) = part {
            current.push(name);
            out.push(BreadcrumbDto {
                name: name.to_string_lossy().into_owned(),
                path: logical(&current),
            });
        }
    }
    out
}

/// Size and use of the file system that holds `path`.
fn disk_usage(path: &Path) -> QuotaDto {
    use std::os::unix::ffi::OsStrExt;
    let empty = QuotaDto {
        quota_bytes: 0,
        used_bytes: 0,
        free_bytes: 0,
    };
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return empty;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string and `stats` is a
    // properly sized, zero-initialised out parameter.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return empty;
    }
    let block = stats.f_frsize as u64;
    let total = (stats.f_blocks as u64).saturating_mul(block);
    let free = (stats.f_bavail as u64).saturating_mul(block);
    let unused = (stats.f_bfree as u64).saturating_mul(block);
    let clamp = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    QuotaDto {
        quota_bytes: clamp(total),
        used_bytes: clamp(total.saturating_sub(unused)),
        free_bytes: clamp(free),
    }
}

fn paginate(mut items: Vec<EntryDto>, page: i64, per_page: i64) -> (Vec<EntryDto>, i64) {
    let total = items.len() as i64;
    let start = ((page - 1).saturating_mul(per_page)) as usize;
    let items = if start >= items.len() {
        Vec::new()
    } else {
        items.drain(start..).take(per_page as usize).collect()
    };
    (items, total)
}

fn page_params(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    (
        page.unwrap_or(1).max(1),
        per_page.unwrap_or(200).clamp(1, 1000),
    )
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
    /// `false` leaves out names that start with a dot.
    hidden: Option<bool>,
}

async fn list(Query(query): Query<ListQuery>) -> AppResult<Json<ListingDto>> {
    let dir = physical(query.path.as_deref().unwrap_or(super::PREFIX))?;
    let (page, per_page) = page_params(query.page, query.per_page);
    let show_hidden = query.hidden.unwrap_or(true);
    blocking(move || {
        require_dir(&dir)?;
        let needle = query
            .q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let mut items = Vec::new();
        let reader = std::fs::read_dir(&dir).map_err(|error| io_error("open", &dir, error))?;
        for item in reader.flatten() {
            // Names that are not UTF-8 cannot be addressed over the API.
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with(TEMP_PREFIX) || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            if needle
                .as_ref()
                .is_some_and(|needle| !name.to_lowercase().contains(needle))
            {
                continue;
            }
            let path = item.path();
            if let Ok(meta) = metadata(&path) {
                items.push(entry(&path, &meta));
            }
        }
        ops::sort_entries(
            &mut items,
            query.sort.as_deref().unwrap_or("name"),
            query.order.as_deref().unwrap_or("asc"),
        );
        // Folders first, as a file manager shows a system tree.
        items.sort_by_key(|item| item.kind != "dir");
        let (items, total) = paginate(items, page, per_page);
        Ok(Json(ListingDto {
            path: logical(&dir),
            breadcrumbs: breadcrumbs(&dir),
            items,
            page,
            per_page,
            total,
            quota: disk_usage(&dir),
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: Option<String>,
    #[serde(rename = "maxBytes", alias = "max_bytes")]
    max_bytes: Option<u64>,
}

fn query_path(query: &PathQuery) -> AppResult<PathBuf> {
    physical(
        query
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| AppError::bad_request("path is required"))?,
    )
}

async fn stat(Query(query): Query<PathQuery>) -> AppResult<Json<EntryDto>> {
    let path = query_path(&query)?;
    blocking(move || stat_entry(&path).map(Json)).await
}

async fn search(Query(query): Query<ListQuery>) -> AppResult<Json<SearchDto>> {
    let base = physical(query.path.as_deref().unwrap_or(super::PREFIX))?;
    let (page, per_page) = page_params(query.page, query.per_page);
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase)
        .ok_or_else(|| AppError::bad_request("q is required"))?;
    let kind = query.kind.map(|kind| kind.trim().to_ascii_lowercase());
    let show_hidden = query.hidden.unwrap_or(true);
    blocking(move || {
        require_dir(&base)?;
        let started = Instant::now();
        let mut items = Vec::new();
        let walker = walkdir::WalkDir::new(&base)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|item| {
                let dotted = item
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'));
                (show_hidden || !dotted)
                    && !SEARCH_SKIP
                        .iter()
                        .any(|skip| item.path() == Path::new(skip) && base != Path::new(skip))
            });
        let mut scanned = 0usize;
        let mut truncated = false;
        for item in walker.flatten() {
            if scanned >= SEARCH_VISIT_LIMIT
                || items.len() >= SEARCH_MATCH_LIMIT
                || started.elapsed() > SEARCH_TIME_LIMIT
            {
                truncated = true;
                break;
            }
            scanned += 1;
            let Some(name) = item.file_name().to_str() else {
                continue;
            };
            if !name.to_lowercase().contains(&needle) || name.starts_with(TEMP_PREFIX) {
                continue;
            }
            let Ok(meta) = metadata(item.path()) else {
                continue;
            };
            if kind
                .as_deref()
                .is_some_and(|kind| !ops::kind_matches(kind, meta.is_dir(), name))
            {
                continue;
            }
            items.push(entry(item.path(), &meta));
        }
        let (items, total) = paginate(items, page, per_page);
        Ok(Json(SearchDto {
            items,
            page,
            per_page,
            total,
            truncated,
            scanned,
        }))
    })
    .await
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

async fn mkdir(Json(body): Json<MkdirRequest>) -> AppResult<Json<EntryDto>> {
    let path = match (body.path, body.parent, body.name) {
        (Some(path), _, _) => physical(&path)?,
        (None, Some(parent), Some(name)) => physical(&parent)?.join(valid_name(&name)?),
        _ => return Err(AppError::bad_request("path (or parent + name) is required")),
    };
    blocking(move || {
        std::fs::create_dir(&path).map_err(|error| io_error("create", &path, error))?;
        adopt_parent_owner(&path);
        stat_entry(&path).map(Json)
    })
    .await
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    path: String,
    #[serde(rename = "newName", alias = "new_name")]
    new_name: String,
}

async fn rename(Json(body): Json<RenameRequest>) -> AppResult<Json<EntryDto>> {
    let source = physical(&body.path)?;
    let target = source
        .parent()
        .ok_or_else(|| AppError::bad_request("the root folder cannot be renamed"))?
        .join(valid_name(&body.new_name)?);
    blocking(move || {
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(AppError::conflict(format!(
                "\"{}\" already exists",
                file_name(&target)
            )));
        }
        std::fs::rename(&source, &target).map_err(|error| io_error("rename", &source, error))?;
        stat_entry(&target).map(Json)
    })
    .await
}

// ---------------------------------------------------------------------------
// Uploads (the handlers are shared, see `crate::fileagent`)
// ---------------------------------------------------------------------------

/// Where an upload goes, refusing what it may not replace.
fn upload_target(dir: &str, name: &str, overwrite: bool) -> AppResult<PathBuf> {
    let dir = physical(dir)?;
    require_dir(&dir)?;
    let dest = dir.join(valid_name(name)?);
    match std::fs::symlink_metadata(&dest) {
        Ok(meta) if meta.is_dir() => Err(AppError::conflict(format!(
            "a folder named \"{name}\" already exists"
        ))),
        Ok(_) if !overwrite => Err(AppError::conflict(format!("\"{name}\" already exists"))),
        _ => Ok(dest),
    }
}

/// Move a finished upload into place. A replaced file keeps its owner and
/// mode; a new one gets the folder's owner and the usual 0644.
fn commit(temp: &Path, dest: &Path, overwrite: bool) -> AppResult<EntryDto> {
    let result = (|| {
        if prepare_replacement(temp, dest, overwrite)?.is_none() {
            adopt_parent_owner(temp);
        }
        std::fs::rename(temp, dest).map_err(|error| io_error("write", dest, error))?;
        stat_entry(dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The regular file a path leads to (symbolic links followed, as root reads
/// them), with the name the browser asked for.
async fn regular_file(query: &PathQuery) -> AppResult<(PathBuf, String)> {
    let path = query_path(query)?;
    let name = file_name(&path);
    let real = tokio::fs::canonicalize(&path)
        .await
        .map_err(|error| io_error("read", &path, error))?;
    let meta = tokio::fs::metadata(&real)
        .await
        .map_err(|error| io_error("read", &path, error))?;
    if meta.is_dir() {
        return Err(AppError::bad_request("path is a directory"));
    }
    if !meta.is_file() {
        return Err(AppError::bad_request(
            "only regular files can be opened (this is a device, socket or pipe)",
        ));
    }
    Ok((real, name))
}

async fn download(Query(query): Query<PathQuery>, headers: HeaderMap) -> AppResult<Response> {
    let (path, name) = regular_file(&query).await?;
    let content_type = ops::mime_for_name(&name);
    serve_path(
        &path,
        &name,
        &headers,
        &content_type,
        Disposition::Attachment,
        None,
    )
    .await
}

async fn preview(Query(query): Query<PathQuery>, headers: HeaderMap) -> AppResult<Response> {
    let (path, name) = regular_file(&query).await?;
    let kind = ops::preview_kind(&name).ok_or(AppError::UnsupportedMediaType(
        "preview is not supported for this file type",
    ))?;
    let content_type = ops::preview_content_type(&name, kind);
    serve_path(
        &path,
        &name,
        &headers,
        &content_type,
        Disposition::Inline,
        Some(PREVIEW_CSP),
    )
    .await
}

async fn text(Query(query): Query<PathQuery>) -> AppResult<Json<TextPreviewDto>> {
    let (path, _) = regular_file(&query).await?;
    let limit = query
        .max_bytes
        .unwrap_or(crate::storage::DEFAULT_TEXT_PREVIEW_BYTES)
        .clamp(1, crate::storage::MAX_TEXT_PREVIEW_BYTES);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| io_error("read", &path, error))?;
    // Read one byte past the limit instead of trusting the size: files in
    // /proc and /sys report a length of zero.
    let mut buffer = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut buffer)
        .await
        .map_err(|error| io_error("read", &path, error))?;
    let truncated = buffer.len() as u64 > limit;
    buffer.truncate(limit as usize);
    if buffer.contains(&0) {
        return Ok(Json(TextPreviewDto {
            content: String::new(),
            truncated: true,
            encoding: "binary".to_string(),
        }));
    }
    Ok(Json(TextPreviewDto {
        content: String::from_utf8_lossy(&buffer).into_owned(),
        truncated,
        encoding: "utf-8".to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Copy, move, delete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BatchRequest {
    paths: Vec<String>,
    #[serde(rename = "destDir", alias = "dest_dir", default)]
    dest_dir: Option<String>,
}

fn sources(paths: &[String]) -> AppResult<Vec<PathBuf>> {
    if paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    paths
        .iter()
        .map(|path| {
            let path = physical(path)?;
            if path == Path::new("/") {
                return Err(AppError::bad_request(
                    "the root folder itself cannot be copied, moved or deleted",
                ));
            }
            Ok(path)
        })
        .collect()
}

/// Where `source` lands in `dest_dir`; nothing is ever overwritten.
fn landing(source: &Path, dest_dir: &Path) -> AppResult<PathBuf> {
    let name = source
        .file_name()
        .ok_or_else(|| AppError::bad_request("invalid source path"))?;
    let target = dest_dir.join(name);
    if target == source {
        return Err(AppError::conflict("source and destination are the same"));
    }
    if dest_dir.starts_with(source) {
        return Err(AppError::bad_request(format!(
            "\"{}\" cannot be placed inside itself",
            name.to_string_lossy()
        )));
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(AppError::conflict(format!(
            "\"{}\" already exists in the destination",
            name.to_string_lossy()
        )));
    }
    Ok(target)
}

/// Copy one entry like `cp -a`: links stay links, owners, modes and
/// modification times are kept.
fn copy_tree(source: &Path, target: &Path, cancel: &AtomicBool, bytes: &mut u64) -> AppResult<()> {
    check(cancel)?;
    let meta =
        std::fs::symlink_metadata(source).map_err(|error| io_error("read", source, error))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        let link = std::fs::read_link(source).map_err(|error| io_error("read", source, error))?;
        std::os::unix::fs::symlink(&link, target)
            .map_err(|error| io_error("create", target, error))?;
    } else if kind.is_dir() {
        std::fs::create_dir(target).map_err(|error| io_error("create", target, error))?;
        let reader = std::fs::read_dir(source).map_err(|error| io_error("read", source, error))?;
        for item in reader {
            let item = item.map_err(|error| io_error("read", source, error))?;
            copy_tree(&item.path(), &target.join(item.file_name()), cancel, bytes)?;
        }
        let _ = std::fs::set_permissions(target, meta.permissions());
    } else if kind.is_file() {
        *bytes += std::fs::copy(source, target).map_err(|error| io_error("copy", source, error))?;
    } else {
        return Err(AppError::bad_request(format!(
            "{} is a device, socket or pipe and cannot be copied",
            source.display()
        )));
    }
    let _ = std::os::unix::fs::lchown(target, Some(meta.uid()), Some(meta.gid()));
    if !kind.is_symlink() {
        if let Ok(modified) = meta.modified() {
            if let Ok(file) = std::fs::File::open(target) {
                let _ = file.set_modified(modified);
            }
        }
    }
    Ok(())
}

fn remove_tree(path: &Path, cancel: &AtomicBool) -> AppResult<()> {
    check(cancel)?;
    let meta = std::fs::symlink_metadata(path).map_err(|error| io_error("delete", path, error))?;
    if meta.is_dir() {
        let reader = std::fs::read_dir(path).map_err(|error| io_error("delete", path, error))?;
        for item in reader {
            let item = item.map_err(|error| io_error("delete", path, error))?;
            remove_tree(&item.path(), cancel)?;
        }
        std::fs::remove_dir(path).map_err(|error| io_error("delete", path, error))
    } else {
        std::fs::remove_file(path).map_err(|error| io_error("delete", path, error))
    }
}

async fn transfer(body: BatchRequest, moving: bool) -> AppResult<Json<Value>> {
    let paths = sources(&body.paths)?;
    let dest_dir = physical(
        body.dest_dir
            .as_deref()
            .ok_or_else(|| AppError::bad_request("destDir is required"))?,
    )?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    blocking(move || {
        require_dir(&dest_dir)?;
        // Every conflict is found before anything is touched.
        let plan: Vec<(PathBuf, PathBuf)> = paths
            .into_iter()
            .map(|source| landing(&source, &dest_dir).map(|target| (source, target)))
            .collect::<AppResult<_>>()?;
        let mut bytes = 0u64;
        for (source, target) in &plan {
            check(&cancel)?;
            if moving {
                match std::fs::rename(source, target) {
                    Ok(()) => continue,
                    // Another file system: copy, then remove the original.
                    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
                    Err(error) => return Err(io_error("move", source, error)),
                }
            }
            if let Err(error) = copy_tree(source, target, &cancel, &mut bytes) {
                // Do not leave half a copy behind.
                let _ = remove_tree(target, &AtomicBool::new(false));
                return Err(error);
            }
            if moving {
                remove_tree(source, &cancel)?;
            }
        }
        let count = plan.len();
        Ok(Json(if moving {
            json!({"moved": count, "bytes": bytes})
        } else {
            json!({"copied": count, "bytes": bytes})
        }))
    })
    .await
}

async fn copy(Json(body): Json<BatchRequest>) -> AppResult<Json<Value>> {
    transfer(body, false).await
}

async fn move_items(Json(body): Json<BatchRequest>) -> AppResult<Json<Value>> {
    transfer(body, true).await
}

async fn delete(Json(body): Json<BatchRequest>) -> AppResult<Json<Value>> {
    let paths = sources(&body.paths)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    blocking(move || {
        for path in &paths {
            remove_tree(path, &cancel)?;
        }
        Ok(Json(json!({"deleted": paths.len(), "trashed": false})))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn copies_keep_links_and_refuse_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("a.txt", source.join("link")).unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();

        let target = landing(&source, &dest).unwrap();
        let mut bytes = 0;
        copy_tree(&source, &target, &flag(), &mut bytes).unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"hello");
        assert!(std::fs::symlink_metadata(target.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());

        // A second copy would overwrite; a folder cannot go inside itself.
        assert!(landing(&source, &dest).is_err());
        assert!(landing(&source, &source.join("inner")).is_err());
        assert!(landing(&source, dir.path()).is_err());

        remove_tree(&target, &flag()).unwrap();
        assert!(!target.exists());
        assert!(source.join("a.txt").exists());
    }

    #[test]
    fn cancelled_work_stops() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keep");
        std::fs::write(&file, b"x").unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            remove_tree(&file, &cancelled),
            Err(AppError::Cancelled)
        ));
        assert!(file.exists());
    }

    #[test]
    fn uploads_replace_only_when_asked_and_keep_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("conf");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();
        let temp = dir.path().join(format!("{TEMP_PREFIX}one"));
        std::fs::write(&temp, b"new").unwrap();
        assert!(commit(&temp, &dest, false).is_err());
        assert!(!temp.exists(), "a refused upload leaves no temporary file");
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");

        std::fs::write(&temp, b"new").unwrap();
        let entry = commit(&temp, &dest, true).unwrap();
        assert_eq!(entry.size, 3);
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn dot_files_are_left_out_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/needle"), b"x").unwrap();
        std::fs::write(dir.path().join(".env"), b"x").unwrap();
        std::fs::write(dir.path().join("needle"), b"x").unwrap();
        let query = |hidden: Option<bool>, q: Option<&str>| ListQuery {
            path: Some(super::super::logical(dir.path())),
            page: None,
            per_page: None,
            sort: None,
            order: None,
            q: q.map(str::to_string),
            kind: None,
            hidden,
        };
        let names = |listing: &ListingDto| -> Vec<String> {
            listing.items.iter().map(|item| item.name.clone()).collect()
        };

        let Json(all) = list(Query(query(None, None))).await.unwrap();
        assert_eq!(names(&all), [".git", ".env", "needle"]);
        let Json(visible) = list(Query(query(Some(false), None))).await.unwrap();
        assert_eq!(names(&visible), ["needle"]);

        let Json(found) = search(Query(query(None, Some("needle")))).await.unwrap();
        assert_eq!(found.total, 2);
        let Json(found) = search(Query(query(Some(false), Some("needle"))))
            .await
            .unwrap();
        assert_eq!(found.total, 1);
    }

    #[test]
    fn names_cannot_leave_their_folder() {
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name(".bashrc").is_ok());
    }
}
