//! What the file agents share: the root file agent ([`crate::rootfs`]) and
//! the per-user file agent ([`crate::homefs`]) both speak the File Station
//! API over a Unix socket, require a token, can check the peer uid, stop
//! themselves when idle and take chunked uploads the same way.
//!
//! [`client`] is the server side: one HTTP/1.1 request per connection to an
//! agent socket, with the body and the response streamed through.

pub mod client;

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::storage::chunked::{ChunkResponse, InitUploadResponse};
use crate::storage::ops::EntryDto;

/// Chunk size offered to the browser for resumable uploads.
pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// An unfinished chunked upload is dropped after this long without a chunk.
pub const UPLOAD_TTL: Duration = Duration::from_secs(24 * 3600);
/// Header carrying the agent token.
pub const TOKEN_HEADER: &str = "x-rast-rootfs-token";
/// The message of the 403 an agent answers to a wrong token.
pub const TOKEN_REFUSED: &str = "invalid agent token";
/// Response header with the size of the file an upload replaced, so the
/// server can account the quota of agent-served homes. Never sent on to the
/// browser.
pub const REPLACED_HEADER: &str = "x-rast-replaced-bytes";
/// Header of a copy or move into HOME: the free quota in bytes, set by the
/// server only (it removes the header from browser requests).
pub const QUOTA_HEADER: &str = "x-rast-quota-free";
/// Header of a request that names shared folders: the containers the
/// server's database records for them (`homefs::ShareRoots`). Set by the
/// server only (it removes the header from browser requests).
pub const SHARES_HEADER: &str = "x-rast-shares";

/// Where a validated upload of the root agent goes.
#[derive(Debug, Clone)]
pub struct Target {
    /// The physical destination file.
    pub dest: PathBuf,
    /// Its logical File Station path.
    pub logical: String,
}

/// Where uploads of one agent go. The agent owns the namespace rules: which
/// logical folders exist, what may be replaced, who owns new files, and how
/// the temporary file of an upload is created, found again and removed.
pub trait UploadSpace: Send + Sync + 'static {
    /// Prefix of chunked upload ids (`sys-` for the root agent, `usr-` for
    /// the per-user agent), so the server knows where an id belongs.
    const ID_PREFIX: &'static str;
    /// Prefix of temporary upload files in the destination folder.
    const TEMP_PREFIX: &'static str;
    /// Logical folder of a request that names none.
    const DEFAULT_DIR: &'static str;
    /// Whether replies carry [`REPLACED_HEADER`].
    const REPORTS_REPLACED: bool;

    /// Where a validated upload goes.
    type Target: Send + Sync + 'static;
    /// The temporary file of an upload, as the space finds it again.
    type Temp: Send + Sync + 'static;

    /// Validate where an upload goes, refusing what it may not replace.
    fn target(
        &self,
        dir: &str,
        name: &str,
        overwrite: bool,
        headers: &HeaderMap,
    ) -> AppResult<Self::Target>;

    /// Create the temporary file of upload `id` next to its destination
    /// (`TEMP_PREFIX` + `id`, a new file with mode 0600).
    fn create_temp(&self, target: &Self::Target, id: &str) -> AppResult<(Self::Temp, File)>;

    /// The temporary file again, to write the next chunk into.
    fn open_temp(&self, temp: &Self::Temp) -> AppResult<File>;

    /// Remove the temporary file (a cancelled, failed or expired upload).
    fn remove_temp(&self, temp: &Self::Temp);

    /// Move a finished upload into place. Returns the new entry and the size
    /// of the file it replaced (0 for a new file).
    fn commit(
        &self,
        temp: &Self::Temp,
        target: &Self::Target,
        overwrite: bool,
    ) -> AppResult<(EntryDto, u64)>;

    /// Map an I/O error while writing an upload; `fallback` is the generic
    /// mapping. The per-user agent turns a full quota into 413 first.
    fn map_io(&self, error: io::Error, fallback: impl FnOnce(io::Error) -> AppError) -> AppError {
        fallback(error)
    }
}

pub struct Upload<S: UploadSpace> {
    pub temp: S::Temp,
    pub target: S::Target,
    pub size: u64,
    pub offset: u64,
    pub overwrite: bool,
    pub touched: Instant,
}

/// Token, idle bookkeeping and chunked uploads of one agent.
pub struct Agent<S: UploadSpace> {
    token: String,
    started: Instant,
    /// Seconds since `started` of the last request.
    last_request: AtomicU64,
    active: AtomicUsize,
    uploads: Mutex<HashMap<String, Upload<S>>>,
    /// Becomes true when the agent stops, so long-lived streams end and the
    /// graceful shutdown does not wait for them.
    stop: tokio::sync::watch::Sender<bool>,
    pub space: S,
}

impl<S: UploadSpace> Agent<S> {
    pub fn new(token: String, space: S) -> Arc<Self> {
        Arc::new(Self {
            token,
            started: Instant::now(),
            last_request: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            uploads: Mutex::new(HashMap::new()),
            stop: tokio::sync::watch::channel(false).0,
            space,
        })
    }

    fn touch(&self) {
        self.last_request
            .store(self.started.elapsed().as_secs(), Ordering::Relaxed);
    }

    /// Return once the agent has been idle for `idle`: no request for that
    /// long, none running, no unfinished upload, and `keep_alive` false.
    /// Expired uploads are dropped on the way.
    pub async fn wait_idle(&self, idle: Duration, keep_alive: impl Fn() -> bool) {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut uploads = self.uploads.lock().await;
            uploads.retain(|_, upload| {
                let keep = upload.touched.elapsed() < UPLOAD_TTL;
                if !keep {
                    self.space.remove_temp(&upload.temp);
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
                && !keep_alive()
            {
                return;
            }
        }
    }

    /// Tell long-lived streams that the agent stops.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves when the agent is stopping.
    pub fn stopping(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut stop = self.stop.subscribe();
        async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        }
    }

    /// Remove the temporary files of unfinished uploads (on exit).
    pub async fn drop_uploads(&self) {
        for (_, upload) in self.uploads.lock().await.drain() {
            self.space.remove_temp(&upload.temp);
        }
    }
}

/// Read the agent token from a file (at least 32 characters).
pub fn read_token(path: &Path) -> io::Result<String> {
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the agent token is missing or too short",
        ));
    }
    Ok(token)
}

/// Counts a request as running until it is dropped.
struct Active<S: UploadSpace>(Arc<Agent<S>>);

impl<S: UploadSpace> Drop for Active<S> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Token check and idle bookkeeping for every request. Requests whose path
/// is in `passive` (long-lived streams) keep no agent alive.
pub async fn guard<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    if !bool::from(presented.ct_eq(agent.token.as_bytes())) {
        return AppError::Forbidden(TOKEN_REFUSED).into_response();
    }
    if request.uri().path() == WATCH_ROUTE {
        return next.run(request).await;
    }
    agent.touch();
    agent.active.fetch_add(1, Ordering::Relaxed);
    let _active = Active(agent.clone());
    next.run(request).await
}

/// The realtime stream of the per-user agent. It does not count as activity,
/// so an open stream never keeps an idle agent alive.
pub const WATCH_ROUTE: &str = "/api/files/watch";

/// A Unix listener that drops every connection whose peer is not `uid`
/// (`SO_PEERCRED`), before a byte is read.
pub struct PeerListener {
    inner: tokio::net::UnixListener,
    uid: Option<u32>,
}

impl PeerListener {
    pub fn new(inner: tokio::net::UnixListener, uid: Option<u32>) -> Self {
        Self { inner, uid }
    }
}

impl axum::serve::Listener for PeerListener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    let allowed = match self.uid {
                        None => true,
                        Some(uid) => stream.peer_cred().is_ok_and(|cred| cred.uid() == uid),
                    };
                    if allowed {
                        return (stream, addr);
                    }
                    tracing::warn!("file agent refused a connection from another account");
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An I/O error as a message the user can act on.
pub fn io_error(what: &str, path: &Path, error: io::Error) -> AppError {
    let message = format!("cannot {what} {}: {error}", path.display());
    match error.kind() {
        io::ErrorKind::NotFound => AppError::NotFound("file not found"),
        io::ErrorKind::AlreadyExists => AppError::Conflict(message),
        _ => AppError::BadRequest(message),
    }
}

pub async fn blocking<T, F>(work: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> AppResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
}

pub fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

pub fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// Set when the request is dropped (the caller cancelled the job by closing
/// the connection), so the blocking work stops at the next entry.
pub struct CancelOnDrop(pub Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn check(cancel: &AtomicBool) -> AppResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

/// New entries belong to the owner of the folder they are created in (only
/// root can do this; the per-user agent's files are its user's anyway).
pub fn adopt_parent_owner(path: &Path) {
    if let Some(meta) = path
        .parent()
        .and_then(|parent| std::fs::metadata(parent).ok())
    {
        let _ = std::os::unix::fs::lchown(path, Some(meta.uid()), Some(meta.gid()));
    }
}

/// Keep the owner and mode of a replaced file on its replacement; give a new
/// file the usual 0644. Returns the size of the replaced file.
pub fn prepare_replacement(temp: &Path, dest: &Path, overwrite: bool) -> AppResult<Option<u64>> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => {
            Err(AppError::conflict("a folder with that name already exists"))
        }
        Ok(_) if !overwrite => Err(AppError::conflict(format!(
            "\"{}\" already exists",
            file_name(dest)
        ))),
        Ok(meta) if meta.is_file() => {
            let _ = std::fs::set_permissions(temp, meta.permissions());
            let _ = std::os::unix::fs::chown(temp, Some(meta.uid()), Some(meta.gid()));
            Ok(Some(meta.len()))
        }
        _ => {
            let _ = std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o644));
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/// The temporary file of upload `id` for the destination `dest`, by path
/// (the root agent): `create_new`, mode 0600.
pub fn create_temp_path(dest: &Path, prefix: &str, id: &str) -> AppResult<(PathBuf, File)> {
    let dir = dest.parent().unwrap_or(Path::new("/"));
    let temp = dir.join(format!("{prefix}{id}"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| io_error("write in", dir, error))?;
    Ok((temp, file))
}

/// Open a temporary file created by [`create_temp_path`] again; never follow
/// a link that replaced it.
pub fn open_temp_path(temp: &Path) -> AppResult<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(temp)
        .map_err(|error| io_error("write", temp, error))
}

/// Append a request body to `file`, at most `limit` bytes.
async fn receive<S: UploadSpace>(
    space: &S,
    file: &mut tokio::fs::File,
    body: Body,
    limit: u64,
) -> AppResult<u64> {
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
            .map_err(|error| space.map_io(error, write_failed))?;
    }
    file.flush()
        .await
        .map_err(|error| space.map_io(error, write_failed))?;
    Ok(written)
}

fn write_failed(error: io::Error) -> AppError {
    AppError::bad_request(format!("cannot write the upload: {error}"))
}

/// An entry reply, with the replaced size when the space reports it.
fn entry_reply<S: UploadSpace>(entry: EntryDto, replaced: u64) -> Response {
    let mut response = Json(entry).into_response();
    if S::REPORTS_REPLACED {
        if let Ok(value) = HeaderValue::from_str(&replaced.to_string()) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(REPLACED_HEADER), value);
        }
    }
    response
}

async fn commit<S: UploadSpace>(
    agent: Arc<Agent<S>>,
    temp: S::Temp,
    target: S::Target,
    overwrite: bool,
) -> AppResult<Response> {
    let (entry, replaced) = blocking(move || {
        let result = agent.space.commit(&temp, &target, overwrite);
        if result.is_err() {
            agent.space.remove_temp(&temp);
        }
        result
    })
    .await?;
    Ok(entry_reply::<S>(entry, replaced))
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    dir: Option<String>,
    name: Option<String>,
    overwrite: Option<bool>,
    #[serde(rename = "uploadId", alias = "upload_id")]
    upload_id: Option<String>,
    offset: Option<u64>,
}

/// POST /api/files/upload: one request, one file.
async fn upload<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let name = query
        .name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::bad_request("name is required"))?;
    let overwrite = query.overwrite.unwrap_or(false);
    let length = content_length(&headers)
        .ok_or_else(|| AppError::bad_request("Content-Length is required for uploads"))?;
    let target = agent.space.target(
        query.dir.as_deref().unwrap_or(S::DEFAULT_DIR),
        &name,
        overwrite,
        &headers,
    )?;
    let (temp, file) = agent.space.create_temp(&target, &random_id())?;
    let mut file = tokio::fs::File::from_std(file);
    let received = receive(&agent.space, &mut file, body, length).await;
    drop(file);
    match received {
        Ok(written) if written == length => commit(agent, temp, target, overwrite).await,
        Ok(_) => {
            agent.space.remove_temp(&temp);
            Err(AppError::bad_request(
                "upload ended before the complete body was received",
            ))
        }
        Err(error) => {
            agent.space.remove_temp(&temp);
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

async fn upload_init<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
    headers: HeaderMap,
    Json(body): Json<InitUploadRequest>,
) -> AppResult<Json<InitUploadResponse>> {
    let overwrite = body.overwrite.unwrap_or(false);
    let target = agent.space.target(
        body.dir.as_deref().unwrap_or(S::DEFAULT_DIR),
        &body.name,
        overwrite,
        &headers,
    )?;
    let id = format!("{}{}", S::ID_PREFIX, random_id());
    let (temp, _file) = agent.space.create_temp(&target, &id)?;
    agent.uploads.lock().await.insert(
        id.clone(),
        Upload {
            temp,
            target,
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

async fn upload_chunk<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
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
        let temp_error = |error| agent.space.map_io(error, write_failed);
        let mut file = tokio::fs::File::from_std(agent.space.open_temp(&upload.temp)?);
        // A chunk that failed half way is overwritten by its retry.
        file.set_len(upload.offset).await.map_err(temp_error)?;
        tokio::io::AsyncSeekExt::seek(&mut file, io::SeekFrom::Start(upload.offset))
            .await
            .map_err(temp_error)?;
        receive(&agent.space, &mut file, body, upload.size - upload.offset).await
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

async fn upload_complete<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
    Query(query): Query<UploadQuery>,
) -> AppResult<Response> {
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
    commit(agent.clone(), upload.temp, upload.target, upload.overwrite).await
}

async fn upload_cancel<S: UploadSpace>(
    State(agent): State<Arc<Agent<S>>>,
    Query(query): Query<UploadQuery>,
) -> AppResult<Json<Value>> {
    let id = upload_id(&query)?;
    if let Some(upload) = agent.uploads.lock().await.remove(&id) {
        agent.space.remove_temp(&upload.temp);
    }
    Ok(Json(json!({"ok": true})))
}

/// The upload routes, shared by both agents.
pub fn upload_routes<S: UploadSpace>() -> Router<Arc<Agent<S>>> {
    Router::new()
        .route(
            "/api/files/upload",
            post(upload::<S>).delete(upload_cancel::<S>),
        )
        .route("/api/files/upload/init", post(upload_init::<S>))
        .route("/api/files/upload/chunk", post(upload_chunk::<S>))
        .route("/api/files/upload/complete", post(upload_complete::<S>))
}

/// Serve `app` on `listener` until the agent is idle, then clean up.
pub async fn serve<S: UploadSpace>(
    listener: PeerListener,
    app: Router,
    agent: Arc<Agent<S>>,
    idle: Duration,
    keep_alive: impl Fn() -> bool + Send + 'static,
) -> io::Result<()> {
    let watcher = agent.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            watcher.wait_idle(idle, keep_alive).await;
            watcher.stop();
        })
        .await;
    agent.drop_uploads().await;
    result
}
