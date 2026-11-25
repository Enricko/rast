//! File Station read operations: listings, stat, search, usage, text preview
//! and the streaming upload finalisation helpers.
//!
//! Everything here re-authorises the caller through [`Storage::resolve`]
//! before touching the filesystem; entry DTOs are constructed from `lstat`
//! metadata so symlinks are never followed.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::Serialize;
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    storage::{
        quota, safe_path, Access, LogicalPath, ResolvedPath, Storage, DEFAULT_TEXT_PREVIEW_BYTES,
        MAX_PAGE_SIZE, MAX_SEARCH_DEPTH, MAX_SEARCH_RESULTS, MAX_TEXT_PREVIEW_BYTES,
        MAX_TREE_ENTRIES, TEMP_PREFIX,
    },
};

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// A file or directory entry as exposed by the File Station API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: i64,
    pub modified_at: i64,
    pub mime: String,
    pub readable: bool,
    pub writable: bool,
    pub trashed: bool,
}

impl EntryDto {
    pub fn new(
        logical: &LogicalPath,
        name: &str,
        meta: &std::fs::Metadata,
        writable: bool,
    ) -> Self {
        let is_dir = meta.is_dir();
        Self {
            name: name.to_string(),
            path: logical.as_string(),
            kind: if is_dir { "dir" } else { "file" }.to_string(),
            size: if is_dir {
                0
            } else {
                i64::try_from(meta.len()).unwrap_or(i64::MAX)
            },
            modified_at: modified_ms(meta),
            mime: if is_dir {
                String::new()
            } else {
                mime_for_name(name)
            },
            readable: true,
            writable,
            trashed: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreadcrumbDto {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaDto {
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingDto {
    pub path: String,
    pub breadcrumbs: Vec<BreadcrumbDto>,
    pub items: Vec<EntryDto>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
    pub quota: QuotaDto,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchDto {
    pub items: Vec<EntryDto>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
    pub truncated: bool,
    pub scanned: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDto {
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub trash_bytes: i64,
    pub by_type: BTreeMap<&'static str, i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPreviewDto {
    pub content: String,
    pub truncated: bool,
    pub encoding: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn modified_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn mime_for_name(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string()
}

/// Coarse category used by usage statistics and search type filters.
pub fn classify_file(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "ico" | "svg" | "tiff" => {
            "images"
        }
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv" | "flv" => "videos",
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" | "opus" | "wma" => "audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        | "txt" | "md" | "rtf" | "csv" | "tsv" | "epub" => "documents",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" => "archives",
        "rs" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "py" | "go" | "java" | "c" | "h" | "cpp"
        | "hpp" | "sh" | "bash" | "zsh" | "css" | "html" | "htm" | "json" | "toml" | "yaml"
        | "yml" | "xml" | "sql" | "lua" | "rb" | "php" | "dockerfile" | "conf" | "ini" => "code",
        _ => "other",
    }
}

/// What kind of inline preview a file supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
    Pdf,
    Text,
}

/// Decide whether a file may be previewed inline. Anything else (including
/// unknown binaries) is rejected with 415.
pub fn preview_kind(name: &str) -> Option<PreviewKind> {
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "ico" | "svg" => {
            Some(PreviewKind::Image)
        }
        "pdf" => Some(PreviewKind::Pdf),
        "txt" | "log" | "md" | "markdown" | "json" | "xml" | "yaml" | "yml" | "toml" | "csv"
        | "tsv" | "ini" | "conf" | "cfg" | "sh" | "bash" | "rs" | "py" | "js" | "ts" | "css"
        | "html" | "htm" | "sql" | "env" | "properties" => Some(PreviewKind::Text),
        _ => None,
    }
}

/// Content type used when serving a preview. Text-ish content is always
/// served as `text/plain` so HTML/JS/SVG can never execute in the app origin.
pub fn preview_content_type(name: &str, kind: PreviewKind) -> String {
    match kind {
        PreviewKind::Pdf => "application/pdf".to_string(),
        PreviewKind::Text => "text/plain; charset=utf-8".to_string(),
        PreviewKind::Image => {
            let mime = mime_for_name(name);
            if mime.starts_with("image/") {
                mime
            } else {
                "application/octet-stream".to_string()
            }
        }
    }
}

/// Build the breadcrumb trail. Home paths include the `Home` root crumb
/// (`/`); shared paths start at the shared folder.
pub fn breadcrumbs_for(resolved: &ResolvedPath) -> Vec<BreadcrumbDto> {
    let mut out = Vec::new();
    if resolved.in_home {
        out.push(BreadcrumbDto {
            name: "Home".to_string(),
            path: "/".to_string(),
        });
        let mut current = String::new();
        for segment in resolved.logical.segments() {
            current.push('/');
            current.push_str(segment);
            out.push(BreadcrumbDto {
                name: segment.clone(),
                path: current.clone(),
            });
        }
    } else if let Some(share_name) = &resolved.share_name {
        let mut current = format!("/shared/{share_name}");
        out.push(BreadcrumbDto {
            name: share_name.clone(),
            path: current.clone(),
        });
        for segment in resolved.logical.segments().iter().skip(2) {
            current.push('/');
            current.push_str(segment);
            out.push(BreadcrumbDto {
                name: segment.clone(),
                path: current.clone(),
            });
        }
    }
    out
}

fn sort_entries(items: &mut [EntryDto], sort: &str, order: &str) {
    let descending = order.eq_ignore_ascii_case("desc");
    let sort = sort.to_ascii_lowercase();
    items.sort_by(|a, b| {
        let primary = match sort.as_str() {
            "size" => a.size.cmp(&b.size),
            "modified" | "modifiedat" | "mtime" | "date" => a.modified_at.cmp(&b.modified_at),
            "kind" => a.kind.cmp(&b.kind),
            _ => Ordering::Equal,
        };
        let cmp = if primary == Ordering::Equal {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        } else {
            primary
        };
        if descending {
            cmp.reverse()
        } else {
            cmp
        }
    });
}

fn paginate<T>(mut items: Vec<T>, page: i64, per_page: i64) -> (Vec<T>, i64, i64, i64) {
    let page = page.max(1);
    let per_page = per_page.clamp(1, MAX_PAGE_SIZE);
    let total = items.len() as i64;
    let start = ((page - 1).saturating_mul(per_page)) as usize;
    let page_items: Vec<T> = if start >= items.len() {
        Vec::new()
    } else {
        items.drain(start..).take(per_page as usize).collect()
    };
    (page_items, page, per_page, total)
}

// ---------------------------------------------------------------------------
// Listing / stat
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn list(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
    q: Option<&str>,
    sort: &str,
    order: &str,
    page: i64,
    per_page: i64,
) -> AppResult<ListingDto> {
    let resolved = storage.resolve(pool, user, path, Access::Read).await?;
    if !resolved.physical.is_dir() {
        return Err(AppError::bad_request("path is not a directory"));
    }
    let mut entries: Vec<EntryDto> = Vec::new();
    let mut reader = tokio::fs::read_dir(&resolved.physical).await?;
    while let Some(entry) = reader.next_entry().await? {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue; // non-UTF-8 names are not addressable over the API
        };
        if name.starts_with(TEMP_PREFIX) || safe_path::validate_component(name).is_err() {
            continue;
        }
        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_symlink() || !(file_type.is_file() || file_type.is_dir()) {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(meta) => meta,
            Err(_) => continue, // raced with another operation
        };
        let logical = match resolved.logical.child(name) {
            Ok(logical) => logical,
            Err(_) => continue,
        };
        entries.push(EntryDto::new(
            &logical,
            name,
            &meta,
            resolved.perm.allows_write(),
        ));
    }
    if let Some(query) = q.map(str::trim).filter(|query| !query.is_empty()) {
        let needle = query.to_lowercase();
        entries.retain(|entry| entry.name.to_lowercase().contains(&needle));
    }
    sort_entries(&mut entries, sort, order);
    let (items, page, per_page, total) = paginate(entries, page, per_page);
    let quota = quota::info(pool, user.id).await?;
    Ok(ListingDto {
        path: resolved.logical.as_string(),
        breadcrumbs: breadcrumbs_for(&resolved),
        items,
        page,
        per_page,
        total,
        quota: QuotaDto {
            quota_bytes: quota.quota_bytes,
            used_bytes: quota.used_bytes,
            free_bytes: quota.free_bytes(),
        },
    })
}

pub async fn stat(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
) -> AppResult<EntryDto> {
    let resolved = storage.resolve(pool, user, path, Access::Read).await?;
    entry_for_resolved(&resolved).await
}

/// Build an entry DTO for an already resolved path.
pub async fn entry_for_resolved(resolved: &ResolvedPath) -> AppResult<EntryDto> {
    let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
    let name = if resolved.logical.is_root() {
        "Home".to_string()
    } else if resolved.kind == crate::storage::RootKind::Share
        && resolved.logical.segments().len() == 2
    {
        resolved
            .share_name
            .clone()
            .unwrap_or_else(|| resolved.name().to_string())
    } else {
        resolved.name().to_string()
    };
    Ok(EntryDto::new(
        &resolved.logical,
        &name,
        &meta,
        resolved.perm.allows_write(),
    ))
}

/// Build an entry DTO for a logical path known to be inside `physical`.
pub async fn entry_for_path(
    logical: &LogicalPath,
    physical: &Path,
    writable: bool,
) -> AppResult<EntryDto> {
    let meta = tokio::fs::symlink_metadata(physical).await?;
    let name = if logical.is_root() {
        "Home".to_string()
    } else {
        logical.name().unwrap_or("").to_string()
    };
    Ok(EntryDto::new(logical, &name, &meta, writable))
}

// ---------------------------------------------------------------------------
// Mutations (single-object, synchronous)
// ---------------------------------------------------------------------------

pub async fn mkdir(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
) -> AppResult<EntryDto> {
    let logical = LogicalPath::parse(path)?;
    if logical.is_root() {
        return Err(AppError::bad_request("cannot create the storage root"));
    }
    let parent_logical = logical
        .parent()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let parent = storage
        .resolve_logical(pool, user, &parent_logical, Access::Write)
        .await?;
    if !parent.physical.is_dir() {
        return Err(AppError::bad_request("parent is not a directory"));
    }
    let name = logical
        .name()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let (target_logical, physical, exists) = storage.child_target(&parent, name)?;
    if exists {
        return Err(AppError::conflict(
            "a file or folder with that name already exists",
        ));
    }
    tokio::fs::create_dir(&physical).await?;
    entry_for_path(&target_logical, &physical, true).await
}

pub async fn rename(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
    new_name: &str,
) -> AppResult<EntryDto> {
    let resolved = storage.resolve(pool, user, path, Access::Write).await?;
    if resolved.is_storage_root() {
        return Err(AppError::bad_request("cannot rename a storage root"));
    }
    let new_name = new_name.trim();
    safe_path::validate_new_name(new_name)?;
    if new_name == resolved.name() {
        return entry_for_resolved(&resolved).await;
    }
    let parent_logical = resolved
        .logical
        .parent()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let parent = storage
        .resolve_logical(pool, user, &parent_logical, Access::Write)
        .await?;
    let (target_logical, target, exists) = storage.child_target(&parent, new_name)?;
    if exists {
        return Err(AppError::conflict(
            "a file or folder with that name already exists",
        ));
    }
    tokio::fs::rename(&resolved.physical, &target).await?;
    entry_for_path(&target_logical, &target, true).await
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn relative_components(path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => out.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }
    Some(out)
}

fn kind_matches(filter: &str, is_dir: bool, name: &str) -> bool {
    match filter {
        "file" | "files" => !is_dir,
        "dir" | "dirs" | "folder" | "folders" => is_dir,
        other => !is_dir && classify_file(name) == other,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn search(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
    query: &str,
    kind: Option<&str>,
    page: i64,
    per_page: i64,
) -> AppResult<SearchDto> {
    let query = query.trim();
    if query.is_empty() {
        return Err(AppError::bad_request("search query is required"));
    }
    if query.chars().count() > 128 {
        return Err(AppError::bad_request("search query is too long"));
    }
    let base = storage.resolve(pool, user, path, Access::Read).await?;
    if !base.physical.is_dir() {
        return Err(AppError::bad_request("search path is not a directory"));
    }
    let needle = query.to_lowercase();
    let kind = kind.map(|k| k.trim().to_ascii_lowercase());
    let base_physical = base.physical.clone();
    let base_logical = base.logical.clone();
    let writable = base.perm.allows_write();
    let (hits, truncated, scanned) =
        tokio::task::spawn_blocking(move || -> (Vec<EntryDto>, bool, usize) {
            let mut hits: Vec<EntryDto> = Vec::new();
            let mut scanned = 0usize;
            let walker = walkdir::WalkDir::new(&base_physical)
                .max_depth(MAX_SEARCH_DEPTH)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| !entry.file_type().is_symlink());
            for entry in walker {
                let Ok(entry) = entry else { continue };
                if entry.depth() == 0 {
                    continue;
                }
                scanned += 1;
                if scanned > MAX_TREE_ENTRIES {
                    break;
                }
                let file_type = entry.file_type();
                if file_type.is_symlink() || !(file_type.is_file() || file_type.is_dir()) {
                    continue;
                }
                let Some(name) = entry.file_name().to_str() else {
                    continue;
                };
                if name.starts_with(TEMP_PREFIX) {
                    continue;
                }
                if !name.to_lowercase().contains(&needle) {
                    continue;
                }
                if let Some(filter) = &kind {
                    if !kind_matches(filter, file_type.is_dir(), name) {
                        continue;
                    }
                }
                let Ok(relative) = entry.path().strip_prefix(&base_physical) else {
                    continue;
                };
                let Some(components) = relative_components(relative) else {
                    continue;
                };
                let Ok(logical) = base_logical.with_segments(&components) else {
                    continue;
                };
                let Ok(meta) = entry.metadata() else { continue };
                hits.push(EntryDto::new(&logical, name, &meta, writable));
                if hits.len() > MAX_SEARCH_RESULTS {
                    break;
                }
            }
            let truncated = hits.len() > MAX_SEARCH_RESULTS;
            hits.truncate(MAX_SEARCH_RESULTS);
            (hits, truncated, scanned)
        })
        .await
        .map_err(AppError::from)?;

    let mut hits = hits;
    sort_entries(&mut hits, "name", "asc");
    let (items, page, per_page, total) = paginate(hits, page, per_page);
    Ok(SearchDto {
        items,
        page,
        per_page,
        total,
        truncated,
        scanned,
    })
}

// ---------------------------------------------------------------------------
// Usage statistics
// ---------------------------------------------------------------------------

pub async fn usage(pool: &SqlitePool, storage: &Storage, user: &AuthUser) -> AppResult<UsageDto> {
    let home = storage.resolve(pool, user, "/", Access::Read).await?;
    let physical = home.physical.clone();
    let by_type = tokio::task::spawn_blocking(move || -> BTreeMap<&'static str, i64> {
        let mut by_type: BTreeMap<&'static str, i64> = BTreeMap::new();
        let mut counted = 0usize;
        for entry in walkdir::WalkDir::new(&physical)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !entry.file_type().is_symlink())
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            if name.starts_with(TEMP_PREFIX) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let category = classify_file(name);
            let bucket = by_type.entry(category).or_insert(0);
            *bucket = bucket.saturating_add(i64::try_from(size).unwrap_or(i64::MAX));
            counted += 1;
            if counted >= MAX_TREE_ENTRIES {
                break;
            }
        }
        by_type
    })
    .await
    .map_err(AppError::from)?;

    let quota = quota::info(pool, user.id).await?;
    let trash_bytes = crate::storage::trash::total_bytes(pool, user.id).await?;
    Ok(UsageDto {
        quota_bytes: quota.quota_bytes,
        used_bytes: quota.used_bytes,
        trash_bytes,
        by_type,
    })
}

// ---------------------------------------------------------------------------
// Text preview
// ---------------------------------------------------------------------------

/// Read at most `max_bytes` of a text file. Returns `(content, truncated, encoding)`.
pub async fn text_preview(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
    max_bytes: Option<u64>,
) -> AppResult<TextPreviewDto> {
    let resolved = require_file(pool, storage, user, path, Access::Read).await?;
    let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
    let limit = max_bytes
        .unwrap_or(DEFAULT_TEXT_PREVIEW_BYTES)
        .clamp(1, MAX_TEXT_PREVIEW_BYTES);
    let to_read = meta.len().min(limit);
    let file = safe_path::open_read_no_follow_async(&resolved.physical).await?;
    let opened_meta = file.metadata().await?;
    safe_path::verify_same_inode(&opened_meta, &meta)?;
    let mut buffer = Vec::with_capacity(to_read.min(1024 * 1024) as usize);
    file.take(to_read).read_to_end(&mut buffer).await?;
    let truncated = meta.len() > to_read;
    if buffer.contains(&0) {
        return Ok(TextPreviewDto {
            content: String::new(),
            truncated: true,
            encoding: "binary".to_string(),
        });
    }
    Ok(TextPreviewDto {
        content: String::from_utf8_lossy(&buffer).into_owned(),
        truncated,
        encoding: "utf-8".to_string(),
    })
}

/// Resolve a path and require it to be a regular file.
pub async fn require_file(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    path: &str,
    access: Access,
) -> AppResult<ResolvedPath> {
    let resolved = storage.resolve(pool, user, path, access).await?;
    let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
    if meta.is_dir() {
        return Err(AppError::bad_request("path is a directory"));
    }
    if !meta.is_file() {
        return Err(AppError::bad_request("path is not a regular file"));
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Streaming upload (single request)
// ---------------------------------------------------------------------------

/// Everything needed to stream one upload into a temporary file and commit it
/// atomically once the whole body has been received.
#[derive(Debug)]
pub struct UploadPlan {
    /// Temporary file opened for writing (`create_new`), in the target dir.
    pub file: tokio::fs::File,
    pub temp_path: PathBuf,
    pub target_path: PathBuf,
    pub logical: LogicalPath,
    pub in_home: bool,
    /// Bytes reserved in `storage_usage` (home uploads only).
    pub reserved: i64,
    /// Size of the file being replaced, if any.
    pub old_size: i64,
    pub expected: u64,
}

/// Validate an upload target and reserve the quota needed for it.
pub async fn plan_upload(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    dir: &str,
    name: &str,
    overwrite: bool,
    content_length: u64,
) -> AppResult<UploadPlan> {
    safe_path::validate_new_name(name)?;
    let parent = storage.resolve(pool, user, dir, Access::Write).await?;
    if !parent.physical.is_dir() {
        return Err(AppError::bad_request(
            "upload destination is not a directory",
        ));
    }
    let (logical, target, exists) = storage.child_target(&parent, name)?;
    let mut old_size = 0i64;
    if exists {
        let meta = tokio::fs::symlink_metadata(&target).await?;
        if meta.is_dir() {
            return Err(AppError::conflict("a folder with that name already exists"));
        }
        if !overwrite {
            return Err(AppError::conflict("a file with that name already exists"));
        }
        old_size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    }
    let length = i64::try_from(content_length).unwrap_or(i64::MAX);
    let reserved = if parent.in_home {
        length.saturating_sub(old_size).max(0)
    } else {
        0
    };
    if reserved > 0 && !quota::reserve(pool, user.id, reserved).await? {
        return Err(AppError::PayloadTooLarge("storage quota exceeded"));
    }
    let temp_path = parent
        .physical
        .join(format!("{TEMP_PREFIX}{}", Uuid::new_v4()));
    let file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            if reserved > 0 {
                let _ = quota::rollback(pool, user.id, reserved).await;
            }
            return Err(error.into());
        }
    };
    Ok(UploadPlan {
        file,
        temp_path,
        target_path: target,
        logical,
        in_home: parent.in_home,
        reserved,
        old_size,
        expected: content_length,
    })
}

/// Finalise an upload: flush, atomically move the temporary file into place and
/// commit the quota reservation.
pub async fn commit_upload(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    plan: UploadPlan,
    written: u64,
) -> AppResult<EntryDto> {
    let UploadPlan {
        mut file,
        temp_path,
        target_path,
        logical,
        in_home,
        reserved,
        old_size,
        expected,
    } = plan;
    if written != expected {
        let _ = tokio::fs::remove_file(&temp_path).await;
        if reserved > 0 {
            let _ = quota::rollback(pool, user.id, reserved).await;
        }
        return Err(AppError::bad_request("upload was incomplete"));
    }
    if let Err(error) = file.flush().await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        if reserved > 0 {
            let _ = quota::rollback(pool, user.id, reserved).await;
        }
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = tokio::fs::rename(&temp_path, &target_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        if reserved > 0 {
            let _ = quota::rollback(pool, user.id, reserved).await;
        }
        return Err(error.into());
    }
    if in_home {
        let used_delta = written as i64 - old_size;
        quota::commit(pool, user.id, reserved, used_delta).await?;
    }
    let writable = true;
    let _ = storage;
    entry_for_path(&logical, &target_path, writable).await
}

/// Abort an upload, removing the temporary file and releasing the reservation.
pub async fn abort_upload(pool: &SqlitePool, user: &AuthUser, plan: UploadPlan) {
    drop(plan.file);
    let _ = tokio::fs::remove_file(&plan.temp_path).await;
    if plan.in_home && plan.reserved > 0 {
        let _ = quota::rollback(pool, user.id, plan.reserved).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Role, config::Config, db, storage::DEFAULT_PAGE_SIZE};

    fn test_cfg(dir: &Path) -> Config {
        crate::storage::tests::test_config(dir)
    }

    fn auth_user(id: i64, username: &str) -> AuthUser {
        AuthUser {
            id,
            username: username.to_string(),
            display_name: username.to_string(),
            role: Role::User,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    struct Ctx {
        _dir: tempfile::TempDir,
        storage: Storage,
        pool: SqlitePool,
        user: AuthUser,
    }

    async fn setup() -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&test_cfg(dir.path())).unwrap();
        let pool = db::connect(&dir.path().join("ops.db"), 4).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
             VALUES (1, 'alice', 'x', 'user', 1000000, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        std::fs::create_dir_all(storage.home_dir("alice")).unwrap();
        Ctx {
            _dir: dir,
            storage,
            pool,
            user: auth_user(1, "alice"),
        }
    }

    async fn upload_bytes(ctx: &Ctx, dir: &str, name: &str, data: &[u8]) -> EntryDto {
        let plan = plan_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            dir,
            name,
            false,
            data.len() as u64,
        )
        .await
        .unwrap();
        let mut file = plan.file;
        file.write_all(data).await.unwrap();
        commit_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            UploadPlan { file, ..plan },
            data.len() as u64,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn upload_list_download_roundtrip() {
        let ctx = setup().await;
        let entry = upload_bytes(&ctx, "/", "hello.txt", b"hello world").await;
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.path, "/hello.txt");
        assert_eq!(entry.kind, "file");
        assert_eq!(entry.size, 11);
        assert_eq!(entry.mime, "text/plain");

        let listing = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            None,
            "name",
            "asc",
            1,
            DEFAULT_PAGE_SIZE,
        )
        .await
        .unwrap();
        assert_eq!(listing.total, 1);
        assert_eq!(listing.breadcrumbs.len(), 1);
        assert_eq!(listing.breadcrumbs[0].name, "Home");
        assert_eq!(listing.breadcrumbs[0].path, "/");

        let file = require_file(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/hello.txt",
            Access::Read,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(&file.physical).unwrap(),
            b"hello world".to_vec()
        );
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 11);
    }

    #[tokio::test]
    async fn listing_paginates_sorts_and_filters() {
        let ctx = setup().await;
        for (name, data) in [
            ("c.txt", b"c".as_slice()),
            ("a.txt", b"aa".as_slice()),
            ("b.bin", b"bbb".as_slice()),
            ("d.txt", b"dddd".as_slice()),
            ("e.txt", b"eeeee".as_slice()),
        ] {
            upload_bytes(&ctx, "/", name, data).await;
        }
        let page1 = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            None,
            "name",
            "asc",
            1,
            2,
        )
        .await
        .unwrap();
        assert_eq!(page1.total, 5);
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].name, "a.txt");
        assert_eq!(page1.items[1].name, "b.bin");
        let page2 = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            None,
            "name",
            "asc",
            2,
            2,
        )
        .await
        .unwrap();
        assert_eq!(page2.items[0].name, "c.txt");
        let page3 = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            None,
            "name",
            "asc",
            3,
            2,
        )
        .await
        .unwrap();
        assert_eq!(page3.items.len(), 1);
        assert_eq!(page3.items[0].name, "e.txt");

        // Sorting by size (descending) and name filtering.
        let by_size = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            None,
            "size",
            "desc",
            1,
            10,
        )
        .await
        .unwrap();
        assert_eq!(by_size.items[0].name, "e.txt");
        let filtered = list(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            Some("B."),
            "name",
            "asc",
            1,
            10,
        )
        .await
        .unwrap();
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.items[0].name, "b.bin");
    }

    #[tokio::test]
    async fn mkdir_and_rename() {
        let ctx = setup().await;
        let dir = mkdir(&ctx.pool, &ctx.storage, &ctx.user, "/docs")
            .await
            .unwrap();
        assert_eq!(dir.kind, "dir");
        let nested = mkdir(&ctx.pool, &ctx.storage, &ctx.user, "/docs/sub")
            .await
            .unwrap();
        assert_eq!(nested.path, "/docs/sub");
        let err = mkdir(&ctx.pool, &ctx.storage, &ctx.user, "/docs")
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);

        upload_bytes(&ctx, "/docs", "a.txt", b"abc").await;
        let renamed = rename(&ctx.pool, &ctx.storage, &ctx.user, "/docs/a.txt", "b.txt")
            .await
            .unwrap();
        assert_eq!(renamed.path, "/docs/b.txt");
        assert!(!ctx.storage.home_dir("alice").join("docs/a.txt").exists());
        // Collision with existing entry is rejected.
        upload_bytes(&ctx, "/docs", "c.txt", b"c").await;
        let err = rename(&ctx.pool, &ctx.storage, &ctx.user, "/docs/b.txt", "c.txt")
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn search_is_bounded_and_respects_filters() {
        let ctx = setup().await;
        mkdir(&ctx.pool, &ctx.storage, &ctx.user, "/a")
            .await
            .unwrap();
        mkdir(&ctx.pool, &ctx.storage, &ctx.user, "/a/b")
            .await
            .unwrap();
        upload_bytes(&ctx, "/a", "report.pdf", b"pdf").await;
        upload_bytes(&ctx, "/a/b", "report.txt", b"txt").await;
        upload_bytes(&ctx, "/", "photo.png", b"png").await;
        let found = search(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "report",
            None,
            1,
            10,
        )
        .await
        .unwrap();
        assert_eq!(found.total, 2);
        assert!(found.items.iter().all(|item| item.name.contains("report")));
        // Both files classify as documents.
        let documents = search(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "report",
            Some("documents"),
            1,
            10,
        )
        .await
        .unwrap();
        assert_eq!(documents.total, 2);
        assert!(documents
            .items
            .iter()
            .all(|item| item.kind == "file" && item.name.starts_with("report")));
        // Filtering by kind narrows the result set.
        let files = search(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "photo",
            Some("file"),
            1,
            10,
        )
        .await
        .unwrap();
        assert_eq!(files.total, 1);
        assert_eq!(files.items[0].name, "photo.png");
        let dirs = search(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "b",
            Some("dir"),
            1,
            10,
        )
        .await
        .unwrap();
        assert!(dirs.items.iter().all(|item| item.kind == "dir"));
        let empty = search(&ctx.pool, &ctx.storage, &ctx.user, "/", "  ", None, 1, 10).await;
        assert!(empty.is_err());
    }

    #[tokio::test]
    async fn usage_counts_by_type_and_trash() {
        let ctx = setup().await;
        upload_bytes(&ctx, "/", "a.png", &[0u8; 10]).await;
        upload_bytes(&ctx, "/", "b.mp4", &[0u8; 20]).await;
        upload_bytes(&ctx, "/", "c.bin", &[0u8; 5]).await;
        let usage = usage(&ctx.pool, &ctx.storage, &ctx.user).await.unwrap();
        assert_eq!(usage.used_bytes, 35);
        assert_eq!(usage.trash_bytes, 0);
        assert_eq!(usage.by_type.get("images"), Some(&10));
        assert_eq!(usage.by_type.get("videos"), Some(&20));
        assert_eq!(usage.by_type.get("other"), Some(&5));
    }

    #[tokio::test]
    async fn text_preview_is_capped_and_detects_binary() {
        let ctx = setup().await;
        upload_bytes(&ctx, "/", "note.txt", b"hello").await;
        let preview = text_preview(&ctx.pool, &ctx.storage, &ctx.user, "/note.txt", Some(3))
            .await
            .unwrap();
        assert_eq!(preview.content, "hel");
        assert!(preview.truncated);
        assert_eq!(preview.encoding, "utf-8");

        upload_bytes(&ctx, "/", "blob.bin", &[0u8, 1, 2, 3]).await;
        let preview = text_preview(&ctx.pool, &ctx.storage, &ctx.user, "/blob.bin", None)
            .await
            .unwrap();
        assert_eq!(preview.encoding, "binary");
        assert!(preview.content.is_empty());
    }

    #[tokio::test]
    async fn upload_conflicts_and_quota() {
        let ctx = setup().await;
        upload_bytes(&ctx, "/", "a.txt", b"one").await;
        let err = plan_upload(&ctx.pool, &ctx.storage, &ctx.user, "/", "a.txt", false, 3)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);

        // Overwrite reuses quota: replacing 3 bytes with 4 only adds 1.
        let plan = plan_upload(&ctx.pool, &ctx.storage, &ctx.user, "/", "a.txt", true, 4)
            .await
            .unwrap();
        let mut file = plan.file;
        file.write_all(b"four").await.unwrap();
        commit_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            UploadPlan { file, ..plan },
            4,
        )
        .await
        .unwrap();
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 4);

        // Exceeding the quota is rejected before writing.
        let err = plan_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "huge.bin",
            false,
            10_000_000,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn abort_releases_reservation() {
        let ctx = setup().await;
        let plan = plan_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            "/",
            "x.bin",
            false,
            1000,
        )
        .await
        .unwrap();
        let mut file = plan.file;
        file.write_all(&[1u8; 100]).await.unwrap();
        abort_upload(&ctx.pool, &ctx.user, UploadPlan { file, ..plan }).await;
        let info = quota::info(&ctx.pool, 1).await.unwrap();
        assert_eq!(info.reserved_bytes, 0);
        assert_eq!(info.used_bytes, 0);
        assert!(!ctx.storage.home_dir("alice").join("x.bin").exists());
    }

    #[test]
    fn preview_classification_is_safe() {
        assert_eq!(preview_kind("a.png"), Some(PreviewKind::Image));
        assert_eq!(preview_kind("a.pdf"), Some(PreviewKind::Pdf));
        assert_eq!(preview_kind("a.txt"), Some(PreviewKind::Text));
        assert_eq!(preview_kind("a.html"), Some(PreviewKind::Text));
        assert_eq!(preview_kind("a.svg"), Some(PreviewKind::Image));
        assert_eq!(preview_kind("a.exe"), None);
        // HTML/JS/XML previews must be served as inert text, never as markup.
        assert!(preview_content_type("a.html", PreviewKind::Text).starts_with("text/plain"));
        assert!(preview_content_type("a.svg", PreviewKind::Image).starts_with("image/svg"));
        assert_eq!(
            preview_content_type("a.pdf", PreviewKind::Pdf),
            "application/pdf"
        );
    }
}
