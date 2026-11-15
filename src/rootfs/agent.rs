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

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::api::files::{serve_path, Disposition, PREVIEW_CSP};
use crate::error::{AppError, AppResult};
use crate::storage::chunked::{ChunkResponse, InitUploadResponse};
use crate::storage::ops::{
    self, BreadcrumbDto, EntryDto, ListingDto, QuotaDto, SearchDto, TextPreviewDto,
};

use super::{logical, physical, TOKEN_HEADER, UPLOAD_PREFIX};

/// Chunk size offered to the browser for resumable uploads.
const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// An unfinished chunked upload is dropped after this long without a chunk.
const UPLOAD_TTL: Duration = Duration::from_secs(24 * 3600);
/// Temporary upload files (`.rast-upload-<id>` in the destination folder).
const TEMP_PREFIX: &str = ".rast-upload-";
/// A search stops after this many entries or this long.
const SEARCH_VISIT_LIMIT: usize = 400_000;
const SEARCH_MATCH_LIMIT: usize = 2_000;
const SEARCH_TIME_LIMIT: Duration = Duration::from_secs(10);
/// Kernel and runtime trees a search from higher up does not descend into.
const SEARCH_SKIP: &[&str] = &["/proc", "/sys", "/dev", "/run"];

struct Upload {
    temp: PathBuf,
    dest: PathBuf,
    size: u64,
    offset: u64,
    overwrite: bool,
    touched: Instant,
}

struct Agent {
    token: String,
    started: Instant,
    /// Seconds since `started` of the last request.
    last_request: AtomicU64,
    active: AtomicUsize,
    uploads: Mutex<HashMap<String, Upload>>,
}

/// Run the agent until it has been idle for `idle`.
pub async fn run(socket: PathBuf, token_file: PathBuf, idle: Duration) -> io::Result<()> {
    let token = std::fs::read_to_string(&token_file)?.trim().to_string();
    if token.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the agent token is missing or too short",
        ));
    }
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    // The directory (root:rast, setgid, 0750) already limits who can reach
    // the socket; the group needs write access to connect.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    let agent = Arc::new(Agent {
        token,
        started: Instant::now(),
        last_request: AtomicU64::new(0),
        active: AtomicUsize::new(0),
        uploads: Mutex::new(HashMap::new()),
    });
    let app = router(agent.clone());
    let watcher = agent.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move { watcher.wait_idle(idle).await })
        .await;
    for (_, upload) in agent.uploads.lock().await.drain() {
        let _ = std::fs::remove_file(&upload.temp);
    }
    let _ = std::fs::remove_file(&socket);
    result
}

fn router(agent: Arc<Agent>) -> Router {
    Router::new()
        .route("/api/files", get(list))
        .route("/api/files/stat", get(stat))
        .route("/api/files/mkdir", post(mkdir))
        .route("/api/files/rename", post(rename))
        .route("/api/files/upload", post(upload).delete(upload_cancel))
        .route("/api/files/upload/init", post(upload_init))
        .route("/api/files/upload/chunk", post(upload_chunk))
        .route("/api/files/upload/complete", post(upload_complete))
        .route("/api/files/download", get(download))
        .route("/api/files/preview", get(preview))
        .route("/api/files/text", get(text))
        .route("/api/files/search", get(search))
        .route("/api/files/copy", post(copy))
        .route("/api/files/move", post(move_items))
        .route("/api/files/delete", post(delete))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn_with_state(agent.clone(), guard))
        .with_state(agent)
}

impl Agent {
    fn touch(&self) {
        self.last_request
            .store(self.started.elapsed().as_secs(), Ordering::Relaxed);
    }

    async fn wait_idle(&self, idle: Duration) {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut uploads = self.uploads.lock().await;
            uploads.retain(|_, upload| {
                let keep = upload.touched.elapsed() < UPLOAD_TTL;
                if !keep {
                    let _ = std::fs::remove_file(&upload.temp);
                }
                keep
            });
            let quiet = self
                .started
                .elapsed()
                .as_secs()
                .saturating_sub(self.last_request.load(Ordering::Relaxed));
            if uploads.is_empty()
                && self.active.load(Ordering::Relaxed) == 0
                && quiet >= idle.as_secs()
            {
                return;
            }
        }
    }
}

/// Counts a request as running until it is dropped.
struct Active(Arc<Agent>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.touch();
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Token check and idle bookkeeping for every request.
async fn guard(State(agent): State<Arc<Agent>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    if !bool::from(presented.ct_eq(agent.token.as_bytes())) {
        return AppError::Forbidden("invalid agent token").into_response();
    }
    agent.touch();
    agent.active.fetch_add(1, Ordering::Relaxed);
    let _active = Active(agent.clone());
    next.run(request).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An I/O error as a message the administrator can act on.
fn io_error(what: &str, path: &Path, error: io::Error) -> AppError {
    let message = format!("cannot {what} {}: {error}", path.display());
    match error.kind() {
        io::ErrorKind::NotFound => AppError::NotFound("file not found"),
        io::ErrorKind::AlreadyExists => AppError::Conflict(message),
        _ => AppError::BadRequest(message),
    }
}

async fn blocking<T, F>(work: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> AppResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

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

/// New entries belong to the owner of the folder they are created in, so a
/// file uploaded into someone's home is theirs, not root's.
fn adopt_parent_owner(path: &Path) {
    if let Some(meta) = path
        .parent()
        .and_then(|parent| std::fs::metadata(parent).ok())
    {
        let _ = std::os::unix::fs::lchown(path, Some(meta.uid()), Some(meta.gid()));
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
}

async fn list(Query(query): Query<ListQuery>) -> AppResult<Json<ListingDto>> {
    let dir = physical(query.path.as_deref().unwrap_or(super::PREFIX))?;
    let (page, per_page) = page_params(query.page, query.per_page);
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
            if name.starts_with(TEMP_PREFIX) {
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
    blocking(move || {
        require_dir(&base)?;
        let started = Instant::now();
        let mut items = Vec::new();
        let walker = walkdir::WalkDir::new(&base)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|item| {
                !SEARCH_SKIP
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
// Uploads
// ---------------------------------------------------------------------------

fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

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

async fn create_temp(dest: &Path, id: &str) -> AppResult<(PathBuf, tokio::fs::File)> {
    let temp = dest
        .parent()
        .unwrap_or(Path::new("/"))
        .join(format!("{TEMP_PREFIX}{id}"));
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .await
        .map_err(|error| io_error("write in", dest.parent().unwrap_or(dest), error))?;
    Ok((temp, file))
}

/// Move a finished upload into place. A replaced file keeps its owner and
/// mode; a new one gets the folder's owner and the usual 0644.
fn commit(temp: &Path, dest: &Path, overwrite: bool) -> AppResult<EntryDto> {
    let result = (|| {
        match std::fs::symlink_metadata(dest) {
            Ok(meta) if meta.is_dir() => {
                return Err(AppError::conflict("a folder with that name already exists"))
            }
            Ok(_) if !overwrite => {
                return Err(AppError::conflict(format!(
                    "\"{}\" already exists",
                    file_name(dest)
                )))
            }
            Ok(meta) if meta.is_file() => {
                let _ = std::fs::set_permissions(temp, meta.permissions());
                let _ = std::os::unix::fs::chown(temp, Some(meta.uid()), Some(meta.gid()));
            }
            _ => {
                let _ = std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o644));
                adopt_parent_owner(temp);
            }
        }
        std::fs::rename(temp, dest).map_err(|error| io_error("write", dest, error))?;
        stat_entry(dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

/// Append a request body to `file`, at most `limit` bytes.
async fn receive(file: &mut tokio::fs::File, body: Body, limit: u64) -> AppResult<u64> {
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    while let Some(frame) = stream.next().await {
        let bytes = frame
            .map_err(|error| AppError::bad_request(format!("upload stream failed: {error}")))?;
        written += bytes.len() as u64;
        if written > limit {
            return Err(AppError::PayloadTooLarge(
                "upload is larger than its declared size",
            ));
        }
        file.write_all(&bytes)
            .await
            .map_err(|error| AppError::bad_request(format!("cannot write the upload: {error}")))?;
    }
    file.flush()
        .await
        .map_err(|error| AppError::bad_request(format!("cannot write the upload: {error}")))?;
    Ok(written)
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    dir: Option<String>,
    name: Option<String>,
    overwrite: Option<bool>,
    #[serde(rename = "uploadId", alias = "upload_id")]
    upload_id: Option<String>,
    offset: Option<u64>,
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// POST /api/files/upload: one request, one file.
async fn upload(
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Json<EntryDto>> {
    let name = query
        .name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::bad_request("name is required"))?;
    let overwrite = query.overwrite.unwrap_or(false);
    let length = content_length(&headers)
        .ok_or_else(|| AppError::bad_request("Content-Length is required for uploads"))?;
    let dest = upload_target(
        query.dir.as_deref().unwrap_or(super::PREFIX),
        &name,
        overwrite,
    )?;
    let (temp, mut file) = create_temp(&dest, &random_id()).await?;
    let received = receive(&mut file, body, length).await;
    drop(file);
    match received {
        Ok(written) if written == length => {
            blocking(move || commit(&temp, &dest, overwrite).map(Json)).await
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&temp);
            Err(AppError::bad_request(
                "upload ended before the complete body was received",
            ))
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

#[derive(Debug, Deserialize)]
struct InitUploadRequest {
    dir: Option<String>,
    name: String,
    size: u64,
    overwrite: Option<bool>,
}

async fn upload_init(
    State(agent): State<Arc<Agent>>,
    Json(body): Json<InitUploadRequest>,
) -> AppResult<Json<InitUploadResponse>> {
    let overwrite = body.overwrite.unwrap_or(false);
    let dest = upload_target(
        body.dir.as_deref().unwrap_or(super::PREFIX),
        &body.name,
        overwrite,
    )?;
    let id = format!("{UPLOAD_PREFIX}{}", random_id());
    let (temp, _file) = create_temp(&dest, &id).await?;
    agent.uploads.lock().await.insert(
        id.clone(),
        Upload {
            temp,
            dest,
            size: body.size,
            offset: 0,
            overwrite,
            touched: Instant::now(),
        },
    );
    Ok(Json(InitUploadResponse {
        upload_id: id,
        chunk_size: CHUNK_SIZE,
        offset: 0,
        size: body.size,
        expires_at: crate::db::now_ms() + UPLOAD_TTL.as_millis() as i64,
    }))
}

fn upload_id(query: &UploadQuery) -> AppResult<String> {
    query
        .upload_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::bad_request("uploadId is required"))
}

async fn upload_chunk(
    State(agent): State<Arc<Agent>>,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> AppResult<Json<ChunkResponse>> {
    let id = upload_id(&query)?;
    let offset = query
        .offset
        .ok_or_else(|| AppError::bad_request("offset is required"))?;
    // The upload leaves the table while a chunk is written: a second chunk
    // for it at the same time finds nothing instead of interleaving.
    let mut upload = agent
        .uploads
        .lock()
        .await
        .remove(&id)
        .ok_or(AppError::NotFound("upload not found"))?;
    let result = async {
        if offset != upload.offset {
            return Err(AppError::conflict(format!(
                "unexpected offset {offset}; the upload is at {}",
                upload.offset
            )));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&upload.temp)
            .await
            .map_err(|error| io_error("write", &upload.temp, error))?;
        // A chunk that failed half way is overwritten by its retry.
        file.set_len(upload.offset)
            .await
            .map_err(|error| io_error("write", &upload.temp, error))?;
        tokio::io::AsyncSeekExt::seek(&mut file, io::SeekFrom::Start(upload.offset))
            .await
            .map_err(|error| io_error("write", &upload.temp, error))?;
        receive(&mut file, body, upload.size - upload.offset).await
    }
    .await;
    upload.touched = Instant::now();
    if let Ok(written) = result {
        upload.offset += written;
    }
    let response = ChunkResponse {
        upload_id: id.clone(),
        received: upload.offset,
        size: upload.size,
        offset: upload.offset,
    };
    agent.uploads.lock().await.insert(id, upload);
    result.map(|_| Json(response))
}

async fn upload_complete(
    State(agent): State<Arc<Agent>>,
    Query(query): Query<UploadQuery>,
) -> AppResult<Json<EntryDto>> {
    let id = upload_id(&query)?;
    let mut uploads = agent.uploads.lock().await;
    let upload = uploads
        .get(&id)
        .ok_or(AppError::NotFound("upload not found"))?;
    if upload.offset != upload.size {
        return Err(AppError::bad_request(format!(
            "upload is incomplete: {} of {} bytes received",
            upload.offset, upload.size
        )));
    }
    let upload = uploads.remove(&id).expect("upload present");
    drop(uploads);
    blocking(move || commit(&upload.temp, &upload.dest, upload.overwrite).map(Json)).await
}

async fn upload_cancel(
    State(agent): State<Arc<Agent>>,
    Query(query): Query<UploadQuery>,
) -> AppResult<Json<Value>> {
    let id = upload_id(&query)?;
    if let Some(upload) = agent.uploads.lock().await.remove(&id) {
        let _ = std::fs::remove_file(&upload.temp);
    }
    Ok(Json(json!({"ok": true})))
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

/// Set when the request is dropped (the caller cancelled the job by closing
/// the connection), so the blocking work stops at the next entry.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn check(cancel: &AtomicBool) -> AppResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
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

    #[test]
    fn names_cannot_leave_their_folder() {
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name(".bashrc").is_ok());
    }
}
