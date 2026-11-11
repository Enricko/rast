//! File Station API: listings, streaming upload/download, filesystem
//! operations, recycle bin, previews, quota, search, chunked uploads and
//! shared folders.
//!
//! Every handler extracts [`AuthUser`] and re-authorises the object through
//! `crate::storage` before touching the filesystem; cross-user or non-member
//! access always yields 404/403 from the storage layer. Mutations are audited
//! with `file.*` / `share.*` actions.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use crate::{
    audit::{self, AuditLog},
    auth::{AdminUser, AuthUser, ClientIp},
    db,
    error::{AppError, AppResult},
    jobs::JobSpec,
    realtime,
    state::AppState,
    storage::{
        chunked, ops, safe_path, shares, transfer, trash, Access, EntryDto, LogicalPath,
        ResolvedPath, DEFAULT_PAGE_SIZE, DEFAULT_TEXT_PREVIEW_BYTES, MAX_PAGE_SIZE,
    },
};

/// Sandboxing CSP for inline previews. `sandbox` without `allow-scripts` means
/// even an SVG/HTML payload that somehow got a markup content type cannot run
/// script in the application origin or read the session cookie.
const PREVIEW_CSP: &str = "sandbox; default-src 'none'; img-src 'self' data: blob:; \
style-src 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'none'; \
frame-ancestors 'self'";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/files", get(list_files))
        .route("/api/files/roots", get(list_roots))
        .route("/api/files/stat", get(stat_file))
        .route("/api/files/mkdir", post(mkdir))
        .route(
            "/api/files/upload",
            post(upload).delete(cancel_chunked_upload),
        )
        .route("/api/files/upload/init", post(init_chunked_upload))
        .route("/api/files/upload/chunk", post(write_chunk))
        .route("/api/files/upload/complete", post(complete_chunked_upload))
        .route("/api/files/download", get(download))
        .route("/api/files/preview", get(preview))
        .route("/api/files/text", get(text_preview))
        .route("/api/files/rename", post(rename))
        .route("/api/files/copy", post(copy_paths))
        .route("/api/files/move", post(move_paths))
        .route("/api/files/delete", post(delete_paths))
        .route("/api/files/search", get(search))
        .route("/api/files/usage", get(usage))
        .route("/api/files/trash", get(trash_list).delete(trash_empty))
        .route("/api/files/trash/restore", post(trash_restore))
        .route("/api/files/trash/delete", post(trash_delete))
        .route("/api/shares", get(list_shares).post(create_share))
        .route("/api/shares/{id}", patch(update_share).delete(delete_share))
}

// ---------------------------------------------------------------------------
// Query / body types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
    pub sort: Option<String>,
    pub order: Option<String>,
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub path: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TextQuery {
    pub path: Option<String>,
    #[serde(rename = "maxBytes", alias = "max_bytes")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub dir: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct MkdirRequest {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub path: String,
    #[serde(rename = "newName", alias = "new_name")]
    pub new_name: String,
}

#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub paths: Vec<String>,
    #[serde(rename = "destDir", alias = "dest_dir")]
    pub dest_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdsRequest {
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct InitUploadRequest {
    pub dir: Option<String>,
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UploadIdQuery {
    #[serde(rename = "uploadId", alias = "upload_id")]
    pub upload_id: Option<String>,
    #[serde(default)]
    pub offset: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TrashListQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn page_params(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    let page = page.unwrap_or(1).max(1);
    let per_page = per_page
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    (page, per_page)
}

fn required_path(query: &PathQuery) -> AppResult<String> {
    query
        .path
        .clone()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| AppError::bad_request("path is required"))
}

async fn audit_file(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    action: &str,
    path: &str,
    detail: String,
) {
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new(action)
            .actor(user)
            .object("file", path)
            .path(path)
            .success()
            .detail(detail)
            .ip(ip.as_string()),
    )
    .await;
}

/// Publish a committed File Station mutation (scoped `file.changed`) and, when
/// the change touched storage usage, a `file.quota` snapshot. Realtime
/// publication is best-effort: it never fails the request.
async fn publish_file_mutation(
    state: &AppState,
    user: &AuthUser,
    op: &str,
    affected_paths: &[String],
    entry: Option<&EntryDto>,
    quota_changed: bool,
) {
    realtime::publish_commit(
        &state.events,
        &state.db,
        user,
        op,
        affected_paths,
        entry,
        quota_changed,
    )
    .await;
}

/// Notify affected accounts that their effective access changed (share rename,
/// member change or deletion). Clients refetch `/api/auth/me` / effective views.
fn publish_access_changed(state: &AppState, user_ids: &[i64], reason: &str, changed_by: i64) {
    for user_id in user_ids {
        state.events.publish_user(
            "access.changed",
            *user_id,
            json!({
                "reason": reason,
                "userId": user_id,
                "changedBy": changed_by,
            }),
        );
    }
}

fn map_upload_io_error(error: std::io::Error) -> AppError {
    // ENOSPC / EDQUOT: surface a clear (but non-internal) failure.
    match error.raw_os_error() {
        Some(28) => AppError::internal("not enough disk space on the server"),
        Some(122) => AppError::internal("storage quota exceeded on the server"),
        _ => error.into(),
    }
}

// ---------------------------------------------------------------------------
// Listing / roots / stat
// ---------------------------------------------------------------------------

/// GET /api/files
pub async fn list_files(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<ops::ListingDto>> {
    let path = query.path.as_deref().unwrap_or("/");
    let (page, per_page) = page_params(query.page, query.per_page);
    let result = ops::list(
        &state.db,
        &state.storage,
        &user,
        path,
        query.q.as_deref(),
        query.sort.as_deref().unwrap_or("name"),
        query.order.as_deref().unwrap_or("asc"),
        page,
        per_page,
    )
    .await?;
    Ok(Json(result))
}

/// GET /api/files/roots
pub async fn list_roots(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let mut roots = vec![json!({"name": "Home", "path": "/", "kind": "home"})];
    // The evaluator decides both visibility (group rules, legacy membership,
    // ownership) and the advertised permission level.
    for (share, level) in shares::list_for_user_with_levels(&state.db, &user).await? {
        roots.push(json!({
            "name": share.name,
            "path": share.path,
            "kind": "share",
            "perms": level.as_str(),
        }));
    }
    Ok(Json(Value::Array(roots)))
}

/// GET /api/files/stat
pub async fn stat_file(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PathQuery>,
) -> AppResult<Json<EntryDto>> {
    let path = required_path(&query)?;
    Ok(Json(
        ops::stat(&state.db, &state.storage, &user, &path).await?,
    ))
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// POST /api/files/mkdir
pub async fn mkdir(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Json(body): Json<MkdirRequest>,
) -> AppResult<Json<EntryDto>> {
    let path = match (body.path, body.parent, body.name) {
        (Some(path), _, _) => path,
        (None, Some(parent), Some(name)) => LogicalPath::parse(&parent)?.child(&name)?.as_string(),
        _ => return Err(AppError::bad_request("path (or parent + name) is required")),
    };
    let entry = ops::mkdir(&state.db, &state.storage, &user, &path).await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.mkdir",
        &entry.path,
        "created folder".to_string(),
    )
    .await;
    publish_file_mutation(
        &state,
        &user,
        "mkdir",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        false,
    )
    .await;
    Ok(Json(entry))
}

/// POST /api/files/rename
pub async fn rename(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Json(body): Json<RenameRequest>,
) -> AppResult<Json<EntryDto>> {
    let entry = ops::rename(&state.db, &state.storage, &user, &body.path, &body.new_name).await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.rename",
        &entry.path,
        format!("renamed from {}", body.path),
    )
    .await;
    let old_path = LogicalPath::parse(&body.path)
        .map(|path| path.as_string())
        .unwrap_or_else(|_| body.path.clone());
    publish_file_mutation(
        &state,
        &user,
        "rename",
        &[old_path, entry.path.clone()],
        Some(&entry),
        false,
    )
    .await;
    Ok(Json(entry))
}

// ---------------------------------------------------------------------------
// Streaming upload
// ---------------------------------------------------------------------------

/// POST /api/files/upload — raw single-request stream.
pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let name = query
        .name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::bad_request("name is required"))?;
    let dir = query.dir.as_deref().unwrap_or("/");
    let overwrite = query.overwrite.unwrap_or(false);
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AppError::bad_request("Content-Length is required for uploads"))?;
    if content_length > state.storage.max_upload_bytes {
        return Err(AppError::PayloadTooLarge(
            "file exceeds the maximum upload size",
        ));
    }
    let mut plan = ops::plan_upload(
        &state.db,
        &state.storage,
        &user,
        dir,
        &name,
        overwrite,
        content_length,
    )
    .await?;

    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    let mut failure: Option<AppError> = None;
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(bytes) => {
                written += bytes.len() as u64;
                if written > content_length {
                    failure = Some(AppError::PayloadTooLarge(
                        "upload is larger than its Content-Length",
                    ));
                    break;
                }
                if let Err(error) = plan.file.write_all(&bytes).await {
                    failure = Some(map_upload_io_error(error));
                    break;
                }
            }
            Err(error) => {
                failure = Some(AppError::bad_request(format!(
                    "upload stream failed: {error}"
                )));
                break;
            }
        }
    }
    if failure.is_none() && written != content_length {
        failure = Some(AppError::bad_request(
            "upload ended before the complete body was received",
        ));
    }
    if let Some(error) = failure {
        ops::abort_upload(&state.db, &user, plan).await;
        return Err(error);
    }
    if let Err(error) = plan.file.flush().await {
        ops::abort_upload(&state.db, &user, plan).await;
        return Err(map_upload_io_error(error));
    }
    let entry = ops::commit_upload(&state.db, &state.storage, &user, plan, written).await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.upload",
        &entry.path,
        format!("{written} bytes"),
    )
    .await;
    publish_file_mutation(
        &state,
        &user,
        "upload",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        true,
    )
    .await;
    Ok(Json(entry).into_response())
}

// ---------------------------------------------------------------------------
// Chunked / resumable uploads
// ---------------------------------------------------------------------------

/// POST /api/files/upload/init
pub async fn init_chunked_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<InitUploadRequest>,
) -> AppResult<Json<chunked::InitUploadResponse>> {
    let dir = body.dir.clone().unwrap_or_else(|| "/".to_string());
    let overwrite = body.overwrite.unwrap_or(false);
    let response = state
        .storage
        .init_chunked_upload(&state.db, &user, &dir, &body.name, body.size, overwrite)
        .await?;
    Ok(Json(response))
}

/// POST /api/files/upload/chunk?uploadId=..&offset=..
pub async fn write_chunk(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<UploadIdQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Json<chunked::ChunkResponse>> {
    let upload_id = query
        .upload_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::bad_request("uploadId is required"))?;
    let offset = query
        .offset
        .ok_or_else(|| AppError::bad_request("offset is required"))?;
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let response = state
        .storage
        .write_chunk(&user, &upload_id, offset, content_length, body)
        .await?;
    Ok(Json(response))
}

/// POST /api/files/upload/complete?uploadId=..
pub async fn complete_chunked_upload(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Query(query): Query<UploadIdQuery>,
) -> AppResult<Json<EntryDto>> {
    let upload_id = query
        .upload_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::bad_request("uploadId is required"))?;
    let entry = state
        .storage
        .complete_chunked_upload(&state.db, &user, &upload_id)
        .await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.upload",
        &entry.path,
        format!("chunked upload completed: {} bytes", entry.size),
    )
    .await;
    publish_file_mutation(
        &state,
        &user,
        "chunk_complete",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        true,
    )
    .await;
    Ok(Json(entry))
}

/// DELETE /api/files/upload?uploadId=..
pub async fn cancel_chunked_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<UploadIdQuery>,
) -> AppResult<Json<Value>> {
    let upload_id = query
        .upload_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::bad_request("uploadId is required"))?;
    state
        .storage
        .cancel_chunked_upload(&state.db, &user, &upload_id)
        .await?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Download / preview / text
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Attachment,
    Inline,
}

fn content_disposition(kind: Disposition, file_name: &str) -> HeaderValue {
    let kind = match kind {
        Disposition::Attachment => "attachment",
        Disposition::Inline => "inline",
    };
    // Build a header-safe ASCII fallback and an RFC 5987 encoded name. Control
    // characters, quotes and path separators can never reach the header.
    let ascii: String = file_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || " ._()-[]{}".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded =
        percent_encoding::utf8_percent_encode(file_name, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""))
}

enum RangeRequest {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn parse_range(value: Option<&str>, len: u64) -> RangeRequest {
    let Some(value) = value else {
        return RangeRequest::Full;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        // Multiple ranges are legal to ignore; serve the full representation.
        return RangeRequest::Full;
    }
    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    if len == 0 {
        return RangeRequest::Unsatisfiable;
    }
    if start_raw.trim().is_empty() {
        let Ok(suffix) = end_raw.trim().parse::<u64>() else {
            return RangeRequest::Full;
        };
        if suffix == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }
    let Ok(start) = start_raw.trim().parse::<u64>() else {
        return RangeRequest::Full;
    };
    if start >= len {
        return RangeRequest::Unsatisfiable;
    }
    let end = if end_raw.trim().is_empty() {
        len - 1
    } else {
        match end_raw.trim().parse::<u64>() {
            Ok(end) => end.min(len - 1),
            Err(_) => return RangeRequest::Full,
        }
    };
    if start > end {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Partial { start, end }
}

fn numeric_header(value: u64) -> HeaderValue {
    HeaderValue::from_str(&value.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

/// Stream a resolved file with optional `Range` support.
async fn serve_file(
    resolved: &ResolvedPath,
    request_headers: &HeaderMap,
    content_type: &str,
    disposition: Disposition,
    csp: Option<&'static str>,
) -> AppResult<Response> {
    let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
    if !meta.is_file() {
        return Err(AppError::bad_request("path is not a regular file"));
    }
    let len = meta.len();
    let file_name = resolved.name();
    let (status, start, length, content_range) = match parse_range(
        request_headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        len,
    ) {
        RangeRequest::Full => (StatusCode::OK, 0u64, len, None),
        RangeRequest::Partial { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{len}")),
        ),
        RangeRequest::Unsatisfiable => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let headers = response.headers_mut();
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(response);
        }
    };

    // Reopen-safe: O_NOFOLLOW (Linux) + inode comparison against the earlier
    // lstat, so a symlink swapped in after resolution cannot leak data.
    let mut file = crate::storage::safe_path::open_read_no_follow_async(&resolved.physical).await?;
    let opened_meta = file.metadata().await?;
    safe_path::verify_same_inode(&opened_meta, &meta)?;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let stream = ReaderStream::new(file.take(length));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    match HeaderValue::from_str(content_type) {
        Ok(value) => {
            headers.insert(header::CONTENT_TYPE, value);
        }
        Err(_) => {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
    }
    headers.insert(header::CONTENT_LENGTH, numeric_header(length));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(disposition, file_name),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(range) = content_range {
        if let Ok(value) = HeaderValue::from_str(&range) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    if let Some(csp) = csp {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        );
    }
    Ok(response)
}

/// GET /api/files/download
pub async fn download(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let path = required_path(&query)?;
    let resolved = ops::require_file(&state.db, &state.storage, &user, &path, Access::Read).await?;
    let content_type = ops::mime_for_name(resolved.name());
    audit_file(&state, &user, &ip, "file.download", &path, String::new()).await;
    serve_file(
        &resolved,
        &headers,
        &content_type,
        Disposition::Attachment,
        None,
    )
    .await
}

/// GET /api/files/preview
pub async fn preview(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let path = required_path(&query)?;
    let resolved = ops::require_file(&state.db, &state.storage, &user, &path, Access::Read).await?;
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

/// GET /api/files/text
pub async fn text_preview(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<TextQuery>,
) -> AppResult<Json<ops::TextPreviewDto>> {
    let path = query
        .path
        .filter(|path| !path.is_empty())
        .ok_or_else(|| AppError::bad_request("path is required"))?;
    let max_bytes = query.max_bytes.unwrap_or(DEFAULT_TEXT_PREVIEW_BYTES);
    Ok(Json(
        ops::text_preview(&state.db, &state.storage, &user, &path, Some(max_bytes)).await?,
    ))
}

// ---------------------------------------------------------------------------
// Background jobs (copy / move / delete)
// ---------------------------------------------------------------------------

fn validated_paths(paths: Vec<String>) -> AppResult<Vec<String>> {
    if paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    Ok(paths)
}

/// POST /api/files/copy
pub async fn copy_paths(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<BatchRequest>,
) -> AppResult<Json<Value>> {
    let paths = validated_paths(body.paths)?;
    transfer::plan_batch(
        &state.db,
        &state.storage,
        &user,
        &paths,
        &body.dest_dir,
        false,
    )
    .await?;
    let count = paths.len();
    let job = submit_transfer_job(&state, &user, "file.copy", paths, &body.dest_dir, true).await?;
    Ok(Json(json!({"jobId": job, "items": count})))
}

/// POST /api/files/move
pub async fn move_paths(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<BatchRequest>,
) -> AppResult<Json<Value>> {
    let paths = validated_paths(body.paths)?;
    transfer::plan_batch(
        &state.db,
        &state.storage,
        &user,
        &paths,
        &body.dest_dir,
        true,
    )
    .await?;
    let count = paths.len();
    let job = submit_transfer_job(&state, &user, "file.move", paths, &body.dest_dir, false).await?;
    Ok(Json(json!({"jobId": job, "items": count})))
}

/// POST /api/files/delete — moves objects to the recycle bin.
pub async fn delete_paths(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<DeleteRequest>,
) -> AppResult<Json<Value>> {
    let paths = validated_paths(body.paths)?;
    transfer::plan_delete(&state.db, &state.storage, &user, &paths).await?;
    let count = paths.len();
    let pool = state.db.clone();
    let storage = (*state.storage).clone();
    let events = state.events.clone();
    let actor = user.clone();
    let job_paths = paths.clone();
    let spec = JobSpec::new("file.delete", move |ctx| async move {
        transfer::run_delete_job(ctx, pool, storage, actor, job_paths, events).await
    })
    .actor(&user)
    .message(format!("delete {count} item(s)"));
    let job_id = state.jobs.submit(spec).await?;
    Ok(Json(json!({"jobId": job_id, "items": count})))
}

async fn submit_transfer_job(
    state: &AppState,
    user: &AuthUser,
    kind: &str,
    paths: Vec<String>,
    dest_dir: &str,
    copy: bool,
) -> AppResult<String> {
    let count = paths.len();
    let pool = state.db.clone();
    let storage = (*state.storage).clone();
    let events = state.events.clone();
    let actor = user.clone();
    let job_paths = paths;
    let dest = dest_dir.to_string();
    let kind_owned = kind.to_string();
    let spec = JobSpec::new(kind_owned, move |ctx| async move {
        if copy {
            transfer::run_copy_job(ctx, pool, storage, actor, job_paths, dest, events).await
        } else {
            transfer::run_move_job(ctx, pool, storage, actor, job_paths, dest, events).await
        }
    })
    .actor(user)
    .message(format!("{kind}: {count} item(s)"));
    state.jobs.submit(spec).await
}

// ---------------------------------------------------------------------------
// Search / usage
// ---------------------------------------------------------------------------

/// GET /api/files/search
pub async fn search(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<ops::SearchDto>> {
    let (page, per_page) = page_params(query.page, query.per_page);
    let q = query
        .q
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::bad_request("q is required"))?;
    let path = query.path.as_deref().unwrap_or("/");
    Ok(Json(
        ops::search(
            &state.db,
            &state.storage,
            &user,
            path,
            &q,
            query.kind.as_deref(),
            page,
            per_page,
        )
        .await?,
    ))
}

/// GET /api/files/usage
pub async fn usage(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<ops::UsageDto>> {
    Ok(Json(ops::usage(&state.db, &state.storage, &user).await?))
}

// ---------------------------------------------------------------------------
// Recycle bin
// ---------------------------------------------------------------------------

/// GET /api/files/trash
pub async fn trash_list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<TrashListQuery>,
) -> AppResult<Json<Value>> {
    // Honour the `trashRetentionDays` control-panel setting lazily, without a
    // global scheduler.
    let retention_days = db::get_setting(&state.db, "trashRetentionDays")
        .await
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(30);
    let _ = trash::cleanup_user_expired(&state.db, &state.storage, user.id, retention_days).await;
    let (page, per_page) = page_params(query.page, query.per_page);
    let (items, total) = trash::list(&state.db, user.id, page, per_page).await?;
    Ok(Json(json!({
        "items": items,
        "page": page,
        "perPage": per_page,
        "total": total,
    })))
}

/// POST /api/files/trash/restore
pub async fn trash_restore(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Json(body): Json<IdsRequest>,
) -> AppResult<Json<Value>> {
    let restored = trash::restore(&state.db, &state.storage, &user, &body.ids).await?;
    for item in &restored {
        audit_file(
            &state,
            &user,
            &ip,
            "file.restore",
            &item.path,
            format!("restored from recycle bin ({})", item.id),
        )
        .await;
    }
    let affected: Vec<String> = restored.iter().map(|item| item.path.clone()).collect();
    publish_file_mutation(&state, &user, "restore", &affected, None, true).await;
    Ok(Json(json!({"ok": true, "restored": restored})))
}

/// POST /api/files/trash/delete
pub async fn trash_delete(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Json(body): Json<IdsRequest>,
) -> AppResult<Json<Value>> {
    let (deleted, bytes, original_paths) =
        trash::delete_permanent(&state.db, &state.storage, &user, &body.ids).await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.trash_delete",
        &body.ids.join(","),
        format!("permanently deleted {deleted} recycle bin entries ({bytes} bytes)"),
    )
    .await;
    publish_file_mutation(&state, &user, "trash_delete", &original_paths, None, true).await;
    Ok(Json(
        json!({"ok": true, "deleted": deleted, "bytes": bytes}),
    ))
}

/// DELETE /api/files/trash — empty the recycle bin.
pub async fn trash_empty(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
) -> AppResult<Json<Value>> {
    let (deleted, bytes, original_paths) = trash::empty(&state.db, &state.storage, &user).await?;
    audit_file(
        &state,
        &user,
        &ip,
        "file.trash_delete",
        "/",
        format!("emptied recycle bin: {deleted} entries ({bytes} bytes)"),
    )
    .await;
    publish_file_mutation(&state, &user, "trash_empty", &original_paths, None, true).await;
    Ok(Json(
        json!({"ok": true, "deleted": deleted, "bytes": bytes}),
    ))
}

// ---------------------------------------------------------------------------
// Shared folders (admin)
// ---------------------------------------------------------------------------

/// GET /api/shares
pub async fn list_shares(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<Value>> {
    let shares = shares::list_for_user(&state.db, &admin.0).await?;
    Ok(Json(json!(shares)))
}

/// POST /api/shares
pub async fn create_share(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<shares::CreateShareInput>,
) -> AppResult<Json<shares::ShareDto>> {
    let dto = shares::create(&state.db, &state.storage, &admin.0, body).await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("share.create")
            .actor(&admin.0)
            .object("share", dto.id.clone())
            .path(dto.path.clone())
            .success()
            .detail(format!("{} member(s)", dto.members.len()))
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(dto))
}

/// PATCH /api/shares/{id}
pub async fn update_share(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<shares::UpdateShareInput>,
) -> AppResult<Json<shares::ShareDto>> {
    // Capture the affected audience *before* the rename rewrite moves rules to
    // the new name (legacy members/owner are found by the current name).
    let before = shares::find_by_id(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("shared folder not found"))?;
    let members_changed = body.members.is_some();
    let mut affected = crate::access::affected_users_for_share(&state.db, &before.name)
        .await
        .unwrap_or_default();
    let dto = shares::update(&state.db, &state.storage, &id, body).await?;
    let renamed = dto.name != before.name;
    if renamed {
        if let Ok(extra) = crate::access::affected_users_for_share(&state.db, &dto.name).await {
            affected.extend(extra);
        }
    }
    affected.sort_unstable();
    affected.dedup();
    if renamed {
        publish_access_changed(&state, &affected, "share_renamed", admin.0.id);
    } else if members_changed {
        publish_access_changed(&state, &affected, "share_updated", admin.0.id);
    }
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("share.update")
            .actor(&admin.0)
            .object("share", dto.id.clone())
            .path(dto.path.clone())
            .success()
            .detail(format!("{} member(s)", dto.members.len()))
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(dto))
}

/// DELETE /api/shares/{id} — removes the definition but keeps the data.
pub async fn delete_share(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let before = shares::find_by_id(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("shared folder not found"))?;
    let affected = crate::access::affected_users_for_share(&state.db, &before.name)
        .await
        .unwrap_or_default();
    let record = shares::delete(&state.db, &id).await?;
    publish_access_changed(&state, &affected, "share_deleted", admin.0.id);
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("share.delete")
            .actor(&admin.0)
            .object("share", record.id)
            .path(record.name)
            .success()
            .detail("shared folder definition removed; data preserved")
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::Role,
        config::{Config, FileConfig},
        state::AppState,
    };
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        Extension,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_config(dir: &std::path::Path) -> Config {
        Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                monitor_interval_secs: Some(300),
                ..Default::default()
            },
            |_| None,
        )
        .expect("test config")
    }

    fn auth_user(id: i64, username: &str, role: Role) -> AuthUser {
        AuthUser {
            id,
            username: username.to_string(),
            display_name: username.to_string(),
            role,
            session_id: "test".to_string(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    struct TestApp {
        _dir: tempfile::TempDir,
        state: AppState,
        app: Router,
    }

    async fn setup(role: Role, quota: i64) -> TestApp {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
             VALUES (1, 'alice', 'x', ?, ?, 1, 1)",
        )
        .bind(role.as_str())
        .bind(quota)
        .execute(&state.db)
        .await
        .unwrap();
        std::fs::create_dir_all(state.storage.home_dir("alice")).unwrap();
        let user = auth_user(1, "alice", role);
        // Replace the session/CSRF middleware with a fixed authenticated user:
        // handler-level tests focus on File Station behaviour, not auth wiring
        // (which is covered by the auth middleware tests).
        let app = routes()
            .layer(Extension(user.clone()))
            .with_state(state.clone());
        TestApp {
            _dir: dir,
            state,
            app,
        }
    }

    async fn request(
        app: &Router,
        method: Method,
        uri: &str,
        body: Body,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    // JSON handlers require the documented content type. Raw
                    // body endpoints (upload/chunk) build their requests
                    // separately and never send this header.
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    async fn json_body(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("valid JSON response")
    }

    async fn upload_raw(app: &Router, dir: &str, name: &str, data: &[u8]) -> Value {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/files/upload?dir={}&name={}",
                percent_encoding::utf8_percent_encode(dir, percent_encoding::NON_ALPHANUMERIC),
                percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC)
            ))
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::from(data.to_vec()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::OK,
            "upload failed: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        json_body(&bytes).await
    }

    #[tokio::test]
    async fn upload_list_download_roundtrip_over_http() {
        let ctx = setup(Role::User, 1_000_000).await;
        let entry = upload_raw(&ctx.app, "/", "hello.txt", b"hello world").await;
        assert_eq!(entry["name"], "hello.txt");
        assert_eq!(entry["path"], "/hello.txt");
        assert_eq!(entry["kind"], "file");
        assert_eq!(entry["size"], 11);
        assert!(entry["modifiedAt"].as_i64().unwrap() > 0);
        assert_eq!(entry["mime"], "text/plain");
        assert!(entry["readable"].as_bool().unwrap());
        assert!(entry["writable"].as_bool().unwrap());

        let (status, _, body) =
            request(&ctx.app, Method::GET, "/api/files?path=/", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        let listing = json_body(&body).await;
        assert_eq!(listing["path"], "/");
        assert_eq!(listing["total"], 1);
        assert_eq!(listing["items"][0]["name"], "hello.txt");
        assert_eq!(listing["breadcrumbs"][0]["name"], "Home");
        assert_eq!(listing["breadcrumbs"][0]["path"], "/");
        assert_eq!(listing["quota"]["usedBytes"], 11);
        assert_eq!(listing["quota"]["quotaBytes"], 1_000_000);
        assert!(listing["quota"]["freeBytes"].is_number());

        let (status, headers, body) = request(
            &ctx.app,
            Method::GET,
            "/api/files/download?path=/hello.txt",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"hello world".to_vec());
        assert!(headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment"));
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        // Range request.
        let response = ctx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/files/download?path=/hello.txt")
                    .header(header::RANGE, "bytes=0-4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-4/11");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"hello");

        // Unsatisfiable range.
        let response = ctx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/files/download?path=/hello.txt")
                    .header(header::RANGE, "bytes=99-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn traversal_and_hidden_paths_are_rejected() {
        let ctx = setup(Role::User, 0).await;
        for uri in [
            "/api/files?path=/../etc/passwd",
            "/api/files?path=/%2e%2e/etc",
            "/api/files/stat?path=/a/../../etc",
            "/api/files/download?path=../secret",
        ] {
            let (status, _, _) = request(&ctx.app, Method::GET, uri, Body::empty()).await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
                "{uri} -> {status}"
            );
        }
        // `/shared` alone is not a listable namespace root.
        let (status, _, _) = request(
            &ctx.app,
            Method::GET,
            "/api/files?path=/shared",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Mutations validate too.
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"path":"/../escape"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mkdir_rename_and_preview_over_http() {
        let ctx = setup(Role::User, 1_000_000).await;
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"path":"/docs"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["kind"], "dir");

        // parent + name variant is accepted too.
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"parent":"/docs","name":"nested"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["path"], "/docs/nested");

        let entry = upload_raw(&ctx.app, "/docs", "note.txt", b"hello preview").await;
        let (status, _headers, body) = request(
            &ctx.app,
            Method::GET,
            &format!(
                "/api/files/text?path={}",
                percent_encoding::utf8_percent_encode(
                    entry["path"].as_str().unwrap(),
                    percent_encoding::NON_ALPHANUMERIC
                )
            ),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let preview = json_body(&body).await;
        assert_eq!(preview["content"], "hello preview");
        assert_eq!(preview["truncated"], false);

        // Inline preview: text/plain + sandbox CSP, never executable markup.
        let (status, headers, body) = request(
            &ctx.app,
            Method::GET,
            &format!(
                "/api/files/preview?path={}",
                percent_encoding::utf8_percent_encode(
                    entry["path"].as_str().unwrap(),
                    percent_encoding::NON_ALPHANUMERIC
                )
            ),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain"));
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox"));
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(body, b"hello preview".to_vec());

        // Rename returns the updated entry.
        let body =
            serde_json::json!({"path": "/docs/note.txt", "newName": "renamed.txt"}).to_string();
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/rename",
            Body::from(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["path"], "/docs/renamed.txt");
    }

    #[tokio::test]
    async fn chunked_upload_roundtrip_over_http() {
        let ctx = setup(Role::User, 1_000_000).await;
        let data: Vec<u8> = (0..100_000u32).map(|value| (value % 251) as u8).collect();
        let init_body = serde_json::json!({
            "dir": "/", "name": "big.bin", "size": data.len(), "overwrite": false
        })
        .to_string();
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/upload/init",
            Body::from(init_body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let init = json_body(&body).await;
        let upload_id = init["uploadId"].as_str().unwrap().to_string();
        let chunk_size = init["chunkSize"].as_u64().unwrap();
        assert_eq!(init["offset"], 0);
        assert!(chunk_size >= 1024);

        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + (chunk_size as usize)).min(data.len());
            let chunk = &data[offset..end];
            let uri = format!("/api/files/upload/chunk?uploadId={upload_id}&offset={offset}");
            let response = ctx
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .header(header::CONTENT_LENGTH, chunk.len())
                        .body(Body::from(chunk.to_vec()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let chunk_response = json_body(&body).await;
            offset = end;
            assert_eq!(chunk_response["received"].as_u64().unwrap(), offset as u64);
            assert_eq!(chunk_response["size"].as_u64().unwrap(), data.len() as u64);
        }

        let uri = format!("/api/files/upload/complete?uploadId={upload_id}");
        let (status, _, body) = request(&ctx.app, Method::POST, &uri, Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        let entry = json_body(&body).await;
        assert_eq!(entry["name"], "big.bin");
        assert_eq!(entry["size"], data.len());
        assert_eq!(
            std::fs::read(ctx.state.storage.home_dir("alice").join("big.bin")).unwrap(),
            data
        );
    }

    #[tokio::test]
    async fn copy_job_returns_job_id_and_completes() {
        let ctx = setup(Role::User, 1_000_000).await;
        upload_raw(&ctx.app, "/", "a.txt", b"payload").await;
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"path":"/dest"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/copy",
            Body::from(r#"{"paths":["/a.txt"],"destDir":"/dest"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let job_id = json_body(&body).await["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        // Poll until the background job finishes.
        let mut status_text = String::new();
        for _ in 0..300 {
            let job = crate::jobs::job_json(&ctx.state.db, &job_id)
                .await
                .unwrap()
                .unwrap();
            status_text = job["status"].as_str().unwrap_or_default().to_string();
            if status_text != "pending" && status_text != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(status_text, "done");
        assert!(ctx
            .state
            .storage
            .home_dir("alice")
            .join("dest/a.txt")
            .is_file());
    }

    #[tokio::test]
    async fn admin_share_crud_and_member_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();
        for (id, username, role) in [(1_i64, "alice", "admin"), (2, "bob", "user")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', ?, 1, 1)",
            )
            .bind(id)
            .bind(username)
            .bind(role)
            .execute(&state.db)
            .await
            .unwrap();
            std::fs::create_dir_all(state.storage.home_dir(username)).unwrap();
        }
        let admin = auth_user(1, "alice", Role::Admin);
        let admin_app = routes()
            .layer(Extension(admin.clone()))
            .with_state(state.clone());
        let (status, _, body) = request(
            &admin_app,
            Method::POST,
            "/api/shares",
            Body::from(
                r#"{"name":"team-api-crud","description":"x","members":[{"userId":2,"perms":"write"}]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let share = json_body(&body).await;
        assert_eq!(share["path"], "/shared/team-api-crud");
        assert_eq!(share["members"][0]["username"], "bob");
        assert!(state.storage.shared_container_dir("team-api-crud").is_dir());

        // Bob sees the shared root; a third user would not.
        let bob = auth_user(2, "bob", Role::User);
        let bob_app = routes().layer(Extension(bob)).with_state(state.clone());
        let (status, _, body) =
            request(&bob_app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        let roots = json_body(&body).await;
        assert_eq!(roots.as_array().unwrap().len(), 2);
        assert_eq!(roots[1]["path"], "/shared/team-api-crud");
        assert_eq!(roots[1]["perms"], "write");

        // Admin-only guard: non-admin cannot list shares.
        let (status, _, _) = request(&bob_app, Method::GET, "/api/shares", Body::empty()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Rename + delete.
        let (status, _, body) = request(
            &admin_app,
            Method::PATCH,
            &format!("/api/shares/{}", share["id"].as_str().unwrap()),
            Body::from(r#"{"name":"team-api-crud2"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["path"], "/shared/team-api-crud2");
        let (status, _, _) = request(
            &admin_app,
            Method::DELETE,
            &format!("/api/shares/{}", share["id"].as_str().unwrap()),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn range_parser_covers_edge_cases() {
        assert!(matches!(parse_range(None, 10), RangeRequest::Full));
        match parse_range(Some("bytes=0-4"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (0, 4)),
            _ => panic!("expected partial"),
        }
        match parse_range(Some("bytes=5-"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (5, 9)),
            _ => panic!("expected partial"),
        }
        match parse_range(Some("bytes=-3"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (7, 9)),
            _ => panic!("expected partial"),
        }
        assert!(matches!(
            parse_range(Some("bytes=20-30"), 10),
            RangeRequest::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some("bytes=-0"), 10),
            RangeRequest::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some("bytes=0-4"), 0),
            RangeRequest::Unsatisfiable
        ));
        // Multi-range requests are ignored (full representation).
        assert!(matches!(
            parse_range(Some("bytes=0-1,3-4"), 10),
            RangeRequest::Full
        ));
        assert!(matches!(
            parse_range(Some("chunks=1-2"), 10),
            RangeRequest::Full
        ));
    }

    #[test]
    fn content_disposition_cannot_inject_headers() {
        let value = content_disposition(Disposition::Attachment, "evil\r\nSet-Cookie: x=1\";.txt");
        let text = value.to_str().unwrap();
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert!(text.starts_with("attachment; filename=\""));
        assert!(text.contains("filename*=UTF-8''"));
        // Unicode names survive through the encoded form.
        let unicode = content_disposition(Disposition::Inline, "日本語.txt");
        assert!(unicode.to_str().unwrap().contains("filename*=UTF-8''"));
    }

    // ------------------------------------------------------------ realtime

    async fn next_matching_event(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        kind: &str,
        predicate: impl Fn(&crate::events::Event) -> bool,
    ) -> crate::events::Event {
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(event) if event.kind == kind && predicate(&event) => return Some(event),
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), wait)
            .await
            .expect("timed out waiting for realtime event")
            .expect("event bus closed")
    }

    #[tokio::test]
    async fn mutations_publish_private_file_events() {
        let ctx = setup(Role::User, 1_000_000).await;
        let mut rx = ctx.state.events.subscribe();
        upload_raw(&ctx.app, "/", "note.txt", b"hello").await;

        let changed = next_matching_event(&mut rx, "file.changed", |event| {
            event.data["path"] == "/note.txt"
        })
        .await;
        assert_eq!(changed.scope, crate::events::EventScope::User(1));
        assert_eq!(changed.data["op"], "upload");
        assert_eq!(changed.data["source"], "api");
        assert_eq!(changed.data["userId"], 1);
        assert_eq!(changed.data["paths"], json!(["/"]));
        assert_eq!(changed.data["entry"]["path"], "/note.txt");
        // Homes are private: an administrator may not observe them.
        assert!(!changed.scope.allows(9, true));

        let quota =
            next_matching_event(&mut rx, "file.quota", |event| event.data["usedBytes"] == 5).await;
        assert_eq!(quota.scope, crate::events::EventScope::User(1));
        assert_eq!(quota.data["quotaBytes"], 1_000_000);
        assert_eq!(quota.data["trashBytes"], 0);
        assert_eq!(quota.data["userId"], 1);
    }

    #[tokio::test]
    async fn shared_mutations_publish_member_scoped_events() {
        let ctx = setup(Role::User, 1_000_000).await;
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (2, 'bob', 'x', 'user', 1, 1)",
        )
        .execute(&ctx.state.db)
        .await
        .unwrap();
        std::fs::create_dir_all(ctx.state.storage.shared_container_dir("team-api-rt")).unwrap();
        sqlx::query(
            "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
             VALUES ('share-team-api-rt', 'team-api-rt', '', 'team-api-rt', 1, 1, 1)",
        )
        .execute(&ctx.state.db)
        .await
        .unwrap();
        for user_id in [1_i64, 2] {
            sqlx::query(
                "INSERT INTO share_members (share_id, user_id, perms) VALUES ('share-team-api-rt', ?, 'write')",
            )
            .bind(user_id)
            .execute(&ctx.state.db)
            .await
            .unwrap();
        }

        let mut rx = ctx.state.events.subscribe();
        upload_raw(
            &ctx.app,
            "/shared/team-api-rt",
            "team-api-rt.txt",
            b"shared",
        )
        .await;
        let changed = next_matching_event(&mut rx, "file.changed", |event| {
            event.data["path"] == "/shared/team-api-rt/team-api-rt.txt"
        })
        .await;
        match &changed.scope {
            crate::events::EventScope::Users(ids) => {
                assert!(ids.contains(&1));
                assert!(ids.contains(&2));
            }
            other => panic!("expected Users scope, got {other:?}"),
        }
        assert_eq!(changed.data["source"], "api");
        assert_eq!(changed.data["userId"], 1);
        let user_ids = changed.data["userIds"].as_array().unwrap();
        assert!(user_ids.iter().any(|id| id == 1));
        assert!(user_ids.iter().any(|id| id == 2));
    }

    #[tokio::test]
    async fn copy_job_publishes_file_events_after_commit() {
        let ctx = setup(Role::User, 1_000_000).await;
        upload_raw(&ctx.app, "/", "a.txt", b"payload").await;
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"path":"/dest"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let mut rx = ctx.state.events.subscribe();
        let (status, _, body) = request(
            &ctx.app,
            Method::POST,
            "/api/files/copy",
            Body::from(r#"{"paths":["/a.txt"],"destDir":"/dest"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let job_id = json_body(&body).await["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        for _ in 0..300 {
            let job = crate::jobs::job_json(&ctx.state.db, &job_id)
                .await
                .unwrap()
                .unwrap();
            let status = job["status"].as_str().unwrap_or_default();
            if status == "done" {
                break;
            }
            assert!(status != "failed", "copy job failed: {job}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let changed =
            next_matching_event(&mut rx, "file.changed", |event| event.data["op"] == "copy").await;
        assert_eq!(changed.scope, crate::events::EventScope::User(1));
        assert_eq!(changed.data["path"], "/dest/a.txt");
        assert_eq!(changed.data["paths"], json!(["/dest"]));
        assert_eq!(changed.data["source"], "api");
        let quota =
            next_matching_event(&mut rx, "file.quota", |event| event.data["usedBytes"] == 14).await;
        assert_eq!(quota.scope, crate::events::EventScope::User(1));
    }

    // ------------------------------------------- group policy (task-13)

    async fn add_user_row(pool: &sqlx::SqlitePool, id: i64, username: &str, role: &str) {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (?, ?, 'x', ?, 1, 1)",
        )
        .bind(id)
        .bind(username)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_share_row(
        state: &AppState,
        name: &str,
        created_by: Option<i64>,
        members: &[(i64, &str)],
    ) -> String {
        let id = format!("share-{name}");
        sqlx::query(
            "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
             VALUES (?, ?, '', ?, ?, 1, 1)",
        )
        .bind(&id)
        .bind(name)
        .bind(name)
        .bind(created_by)
        .execute(&state.db)
        .await
        .unwrap();
        std::fs::create_dir_all(state.storage.shared_container_dir(name)).unwrap();
        for (user_id, perms) in members {
            sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES (?, ?, ?)")
                .bind(&id)
                .bind(user_id)
                .bind(perms)
                .execute(&state.db)
                .await
                .unwrap();
        }
        crate::access::bump_generation();
        id
    }

    async fn add_group(pool: &sqlx::SqlitePool, id: i64, name: &str, member_id: i64) {
        sqlx::query(
            "INSERT INTO groups (id, name, description, created_at, updated_at)
             VALUES (?, ?, '', 1, 1)",
        )
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, 1)")
            .bind(id)
            .bind(member_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn set_group_rule(pool: &sqlx::SqlitePool, group_id: i64, share_name: &str, rule: &str) {
        sqlx::query(
            "INSERT OR REPLACE INTO group_folder_rules (group_id, share_name, rule)
             VALUES (?, ?, ?)",
        )
        .bind(group_id)
        .bind(share_name)
        .bind(rule)
        .execute(pool)
        .await
        .unwrap();
        crate::access::bump_generation();
    }

    async fn set_user_rule(pool: &sqlx::SqlitePool, user_id: i64, share_name: &str, rule: &str) {
        sqlx::query(
            "INSERT OR REPLACE INTO user_folder_rules (user_id, share_name, rule)
             VALUES (?, ?, ?)",
        )
        .bind(user_id)
        .bind(share_name)
        .bind(rule)
        .execute(pool)
        .await
        .unwrap();
        crate::access::bump_generation();
    }

    async fn upload_status(app: &Router, dir: &str, name: &str, data: &[u8]) -> StatusCode {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/files/upload?dir={}&name={}",
                percent_encoding::utf8_percent_encode(dir, percent_encoding::NON_ALPHANUMERIC),
                percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC)
            ))
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::from(data.to_vec()))
            .unwrap();
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn read_only_share_member_cannot_mutate_but_can_read() {
        let ctx = setup(Role::User, 1_000_000).await;
        add_user_row(&ctx.state.db, 9, "owner-admin", "admin").await;
        let _share_id = add_share_row(&ctx.state, "team-api-read", Some(9), &[(1, "read")]).await;
        let team_dir = ctx.state.storage.shared_container_dir("team-api-read");
        std::fs::write(team_dir.join("report.txt"), b"quarterly").unwrap();

        // Reads work through the evaluator: listing/stat/search/download/preview.
        let (status, _, body) = request(
            &ctx.app,
            Method::GET,
            "/api/files?path=/shared/team-api-read",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["total"], 1);
        let (status, _, body) = request(
            &ctx.app,
            Method::GET,
            "/api/files/stat?path=/shared/team-api-read/report.txt",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["name"], "report.txt");
        let (status, _, body) = request(
            &ctx.app,
            Method::GET,
            "/api/files/search?q=report&path=/shared/team-api-read",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await["total"], 1);
        let (status, _, body) = request(
            &ctx.app,
            Method::GET,
            "/api/files/download?path=/shared/team-api-read/report.txt",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"quarterly".to_vec());
        let (status, headers, _) = request(
            &ctx.app,
            Method::GET,
            "/api/files/preview?path=/shared/team-api-read/report.txt",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox"));
        // Root listing advertises the evaluated read level.
        let (status, _, body) =
            request(&ctx.app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        let roots = json_body(&body).await;
        assert_eq!(roots[1]["path"], "/shared/team-api-read");
        assert_eq!(roots[1]["perms"], "read");

        // Mutations are denied for a read-only member.
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/mkdir",
            Body::from(r#"{"path":"/shared/team-api-read/new"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/rename",
            Body::from(r#"{"path":"/shared/team-api-read/report.txt","newName":"renamed.txt"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/delete",
            Body::from(r#"{"paths":["/shared/team-api-read/report.txt"]}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            upload_status(&ctx.app, "/shared/team-api-read", "up.txt", b"x").await,
            StatusCode::FORBIDDEN
        );
        // Copy out only needs read on the source; copy in needs write on dest.
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/copy",
            Body::from(r#"{"paths":["/shared/team-api-read/report.txt"],"destDir":"/"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        upload_raw(&ctx.app, "/", "home.txt", b"home").await;
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/copy",
            Body::from(r#"{"paths":["/home.txt"],"destDir":"/shared/team-api-read"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Move writes both sides: denied for a read-only member.
        let (status, _, _) = request(
            &ctx.app,
            Method::POST,
            "/api/files/move",
            Body::from(r#"{"paths":["/shared/team-api-read/report.txt"],"destDir":"/"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A group write grant upgrades access; the share is still mounted.
        add_group(&ctx.state.db, 10, "editors", 1).await;
        set_group_rule(&ctx.state.db, 10, "team-api-read", "write").await;
        let (status, _, body) =
            request(&ctx.app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body).await[1]["perms"], "write");
        assert_eq!(
            upload_status(&ctx.app, "/shared/team-api-read", "written.txt", b"data").await,
            StatusCode::OK
        );

        // An explicit direct deny wins over the group write: share is hidden.
        set_user_rule(&ctx.state.db, 1, "team-api-read", "none").await;
        let (status, _, _) = request(
            &ctx.app,
            Method::GET,
            "/api/files?path=/shared/team-api-read",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = request(
            &ctx.app,
            Method::GET,
            "/api/files/search?q=report&path=/shared/team-api-read",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, body) = request(&ctx.app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert!(json_body(&body)
            .await
            .as_array()
            .unwrap()
            .iter()
            .all(|root| root["path"] != "/shared/team-api-read"));
        assert_eq!(
            upload_status(&ctx.app, "/shared/team-api-read", "denied.txt", b"x").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn share_rename_delete_rewrite_rules_and_notify_access_changed() {
        let ctx = setup(Role::User, 1_000_000).await;
        add_user_row(&ctx.state.db, 9, "owner-admin", "admin").await;
        let share_id = add_share_row(&ctx.state, "team-api-rules", Some(9), &[(1, "read")]).await;
        add_group(&ctx.state.db, 10, "staff", 1).await;
        set_group_rule(&ctx.state.db, 10, "team-api-rules", "write").await;
        let mut rx = ctx.state.events.subscribe();

        // Admin renames the share; rules are rewritten and affected users get a
        // user-scoped access.changed event.
        let admin = auth_user(9, "owner-admin", Role::Admin);
        let admin_app = routes()
            .layer(Extension(admin.clone()))
            .with_state(ctx.state.clone());
        let (status, _, body) = request(
            &admin_app,
            Method::PATCH,
            &format!("/api/shares/{share_id}"),
            Body::from(r#"{"name":"team-api-rules-renamed"}"#),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{:?}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            json_body(&body).await["path"],
            "/shared/team-api-rules-renamed"
        );
        let rules: Vec<(String, String)> =
            sqlx::query_as("SELECT share_name, rule FROM group_folder_rules")
                .fetch_all(&ctx.state.db)
                .await
                .unwrap();
        assert_eq!(
            rules,
            vec![("team-api-rules-renamed".to_string(), "write".to_string())]
        );
        let mut saw_access_changed = false;
        for _ in 0..20 {
            let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
            let Ok(Ok(event)) = received else { break };
            if event.kind == "access.changed" {
                assert!(matches!(event.scope, crate::events::EventScope::User(_)));
                assert_eq!(event.data["reason"], "share_renamed");
                assert_eq!(event.data["changedBy"], 9);
                saw_access_changed = true;
                break;
            }
        }
        assert!(
            saw_access_changed,
            "share rename must publish access.changed"
        );

        // The member sees the renamed share through the evaluator.
        let (status, _, body) =
            request(&ctx.app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json_body(&body)
            .await
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root["path"] == "/shared/team-api-rules-renamed"));

        // Deleting the share removes its rules and publishes access.changed.
        let (status, _, _) = request(
            &admin_app,
            Method::DELETE,
            &format!("/api/shares/{share_id}"),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_folder_rules")
            .fetch_one(&ctx.state.db)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
        let mut saw_deleted = false;
        for _ in 0..20 {
            let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
            let Ok(Ok(event)) = received else { break };
            if event.kind == "access.changed" && event.data["reason"] == "share_deleted" {
                saw_deleted = true;
                break;
            }
        }
        assert!(saw_deleted, "share delete must publish access.changed");
        let (status, _, body) =
            request(&ctx.app, Method::GET, "/api/files/roots", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json_body(&body)
            .await
            .as_array()
            .unwrap()
            .iter()
            .all(|root| root["path"] != "/shared/team-api-rules-renamed"));
    }
}
