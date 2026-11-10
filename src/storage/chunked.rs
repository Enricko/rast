//! Chunked / resumable uploads.
//!
//! Cloudflare (and several reverse proxies) cap a single HTTP request body
//! well below RAST's configured upload limit, so large files are uploaded in
//! chunks:
//!
//! 1. `POST /api/files/upload/init` validates the destination, reserves the
//!    quota for the declared size and returns an `uploadId` + chunk size;
//! 2. `POST /api/files/upload/chunk?uploadId=..&offset=..` appends a raw chunk
//!    at the exact current offset (no holes, one writer at a time);
//! 3. `POST /api/files/upload/complete` fsyncs the temporary file and moves it
//!    atomically into place;
//! 4. `DELETE /api/files/upload?uploadId=..` cancels and releases the quota.
//!
//! Sessions live in a bounded in-memory map (they are bound to temp files under
//! `<data_dir>/tmp/uploads`, which are wiped on startup) and expire after
//! [`UPLOAD_TTL_MS`]. Ownership is checked on every call: an upload id is only
//! usable by the user who created it (others get a 404).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::body::Body;
use futures::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    db,
    error::{AppError, AppResult},
    storage::{ops, quota, safe_path, Access, LogicalPath, Storage, TEMP_PREFIX},
};

/// Default chunk size advertised to clients (48 MiB below Cloudflare's 100 MB
/// request limit).
pub const CHUNK_SIZE: u64 = 64 * 1024 * 1024;
/// Hard server-side cap for a single chunk.
pub const MAX_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
/// Stale sessions are purged after 24 hours.
pub const UPLOAD_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Maximum number of concurrent upload sessions (global / per user).
pub const MAX_UPLOADS_GLOBAL: usize = 64;
pub const MAX_UPLOADS_PER_USER: usize = 8;

/// Cross-device rename errno (fallback to copy + rename).
const EXDEV: i32 = 18;

/// A chunked upload session.
#[derive(Debug, Clone)]
pub struct ChunkedUpload {
    pub id: String,
    pub user_id: i64,
    /// Logical destination directory.
    pub dir: String,
    pub name: String,
    pub declared: u64,
    pub written: u64,
    pub overwrite: bool,
    pub in_home: bool,
    pub reserved: i64,
    pub old_size: i64,
    pub temp_path: PathBuf,
    pub created_at: i64,
    pub expires_at: i64,
    /// Serialises writers (and complete/cancel) for one upload id.
    pub write_lock: Arc<tokio::sync::Mutex<()>>,
}

/// `POST /api/files/upload/init` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitUploadResponse {
    pub upload_id: String,
    pub chunk_size: u64,
    pub offset: u64,
    pub size: u64,
    pub expires_at: i64,
}

/// `POST /api/files/upload/chunk` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkResponse {
    pub upload_id: String,
    /// Cumulative bytes received so far.
    pub received: u64,
    pub size: u64,
    pub offset: u64,
}

impl Storage {
    /// Path where chunked upload temporary files live.
    pub fn uploads_dir(&self) -> &Path {
        &self.uploads_root
    }

    /// Drop sessions older than their expiry, releasing reservations and
    /// removing part files. Called opportunistically by the upload endpoints.
    pub async fn cleanup_expired_uploads(&self, pool: &SqlitePool) -> usize {
        let now = db::now_ms();
        let expired: Vec<ChunkedUpload> = {
            let Ok(mut map) = self.uploads.lock() else {
                return 0;
            };
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, session)| session.expires_at <= now)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| map.remove(id)).collect()
        };
        let mut count = 0usize;
        for session in expired {
            let _ = tokio::fs::remove_file(&session.temp_path).await;
            if session.reserved > 0 {
                let _ = quota::rollback(pool, session.user_id, session.reserved).await;
            }
            count += 1;
        }
        count
    }

    /// Snapshot a session, enforcing ownership. Expired sessions are reported
    /// as not found (the next cleanup pass reclaims their space).
    pub fn upload_snapshot(&self, upload_id: &str, user_id: i64) -> AppResult<ChunkedUpload> {
        let map = self
            .uploads
            .lock()
            .map_err(|_| AppError::internal("upload state is unavailable"))?;
        let session = map
            .get(upload_id)
            .filter(|session| session.user_id == user_id)
            .cloned()
            .ok_or(AppError::NotFound("upload not found"))?;
        if session.expires_at <= db::now_ms() {
            return Err(AppError::NotFound("upload not found"));
        }
        Ok(session)
    }

    /// Remove a session from the map (idempotent).
    fn take_session(&self, upload_id: &str, user_id: i64) -> Option<ChunkedUpload> {
        let mut map = self.uploads.lock().ok()?;
        let owned = map
            .get(upload_id)
            .map(|session| session.user_id == user_id)
            .unwrap_or(false);
        if !owned {
            return None;
        }
        map.remove(upload_id)
    }

    fn record_written(&self, upload_id: &str, user_id: i64, written: u64) -> AppResult<()> {
        let mut map = self
            .uploads
            .lock()
            .map_err(|_| AppError::internal("upload state is unavailable"))?;
        let session = map
            .get_mut(upload_id)
            .filter(|session| session.user_id == user_id)
            .ok_or(AppError::NotFound("upload not found"))?;
        session.written = written;
        Ok(())
    }

    /// Validate the final destination of an upload session and enforce the
    /// overwrite policy. Returns `(logical path, physical target, mapping)`.
    async fn refresh_target(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        session: &ChunkedUpload,
    ) -> AppResult<(LogicalPath, PathBuf, bool, i64)> {
        let dir = LogicalPath::parse(&session.dir)?;
        let parent = self
            .resolve_logical(pool, user, &dir, Access::Write)
            .await?;
        if !parent.physical.is_dir() {
            return Err(AppError::bad_request(
                "upload destination is not a directory",
            ));
        }
        let (logical, target, exists) = self.child_target(&parent, &session.name)?;
        let mut old_size = 0i64;
        if exists {
            let meta = tokio::fs::symlink_metadata(&target).await?;
            if meta.is_dir() {
                return Err(AppError::conflict("a folder with that name already exists"));
            }
            if !session.overwrite {
                return Err(AppError::conflict("a file with that name already exists"));
            }
            old_size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        }
        Ok((logical, target, parent.in_home, old_size))
    }

    /// Start a chunked upload: validate the destination, reserve quota and
    /// create the temporary part file.
    pub async fn init_chunked_upload(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        dir: &str,
        name: &str,
        size: u64,
        overwrite: bool,
    ) -> AppResult<InitUploadResponse> {
        safe_path::validate_new_name(name)?;
        if size > self.max_upload_bytes {
            return Err(AppError::PayloadTooLarge(
                "file exceeds the maximum upload size",
            ));
        }
        self.cleanup_expired_uploads(pool).await;

        let dir_logical = LogicalPath::parse(dir)?;
        let parent = self
            .resolve_logical(pool, user, &dir_logical, Access::Write)
            .await?;
        if !parent.physical.is_dir() {
            return Err(AppError::bad_request(
                "upload destination is not a directory",
            ));
        }
        let (_, target, exists) = self.child_target(&parent, name)?;
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
        let reserved = if parent.in_home {
            (i64::try_from(size).unwrap_or(i64::MAX))
                .saturating_sub(old_size)
                .max(0)
        } else {
            0
        };
        if reserved > 0 && !quota::reserve(pool, user.id, reserved).await? {
            return Err(AppError::PayloadTooLarge("storage quota exceeded"));
        }

        let upload_id = Uuid::new_v4().to_string();
        let temp_path = self.uploads_root.join(format!("{upload_id}.part"));
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
        drop(file);

        let now = db::now_ms();
        let session = ChunkedUpload {
            id: upload_id.clone(),
            user_id: user.id,
            dir: dir_logical.as_string(),
            name: name.to_string(),
            declared: size,
            written: 0,
            overwrite,
            in_home: parent.in_home,
            reserved,
            old_size,
            temp_path,
            created_at: now,
            expires_at: now.saturating_add(self.upload_ttl_ms.max(1)),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let inserted = {
            let mut map = self
                .uploads
                .lock()
                .map_err(|_| AppError::internal("upload state is unavailable"))?;
            let user_sessions = map
                .values()
                .filter(|existing| existing.user_id == user.id)
                .count();
            if user_sessions >= MAX_UPLOADS_PER_USER || map.len() >= MAX_UPLOADS_GLOBAL {
                None
            } else {
                map.insert(upload_id.clone(), session.clone());
                Some(())
            }
        };
        if inserted.is_none() {
            let _ = tokio::fs::remove_file(&session.temp_path).await;
            if reserved > 0 {
                let _ = quota::rollback(pool, user.id, reserved).await;
            }
            return Err(AppError::TooManyRequests {
                retry_after_secs: 60,
                message: "too many concurrent uploads, finish or cancel one first",
            });
        }
        Ok(InitUploadResponse {
            upload_id,
            chunk_size: CHUNK_SIZE.min(self.max_upload_bytes).max(1),
            offset: 0,
            size,
            expires_at: session.expires_at,
        })
    }

    /// Append one raw chunk at the current offset. The chunk body is streamed
    /// to disk; nothing is buffered in memory.
    pub async fn write_chunk(
        &self,
        user: &AuthUser,
        upload_id: &str,
        offset: u64,
        content_length: Option<u64>,
        body: Body,
    ) -> AppResult<ChunkResponse> {
        let content_length =
            content_length.ok_or_else(|| AppError::bad_request("Content-Length is required"))?;
        let initial = self.upload_snapshot(upload_id, user.id)?;
        let lock = initial.write_lock.clone();
        let _guard = lock.lock_owned().await;
        // Re-read under the per-upload lock: this is the single-writer gate.
        let session = self.upload_snapshot(upload_id, user.id)?;
        if offset != session.written {
            return Err(AppError::conflict(format!(
                "offset mismatch: expected {} bytes, got {}",
                session.written, offset
            )));
        }
        if content_length > MAX_CHUNK_BYTES {
            return Err(AppError::PayloadTooLarge("chunk is too large"));
        }
        let remaining = session.declared.saturating_sub(offset);
        if content_length > remaining {
            return Err(AppError::PayloadTooLarge(
                "chunk exceeds the declared upload size",
            ));
        }
        if content_length == 0 {
            return Ok(ChunkResponse {
                upload_id: session.id,
                received: session.written,
                size: session.declared,
                offset: session.written,
            });
        }

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&session.temp_path)
            .await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut stream = body.into_data_stream();
        let mut received: u64 = 0;
        let mut failure: Option<AppError> = None;
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(bytes) => {
                    received += bytes.len() as u64;
                    if received > content_length {
                        failure = Some(AppError::bad_request(
                            "chunk body is longer than its Content-Length",
                        ));
                        break;
                    }
                    if let Err(error) = file.write_all(&bytes).await {
                        failure = Some(error.into());
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
        if failure.is_none() && received != content_length {
            failure = Some(AppError::bad_request(
                "upload stream ended before the chunk was complete",
            ));
        }
        if let Some(error) = failure {
            // Roll the part file back to the last committed offset so the
            // client can safely retry the same chunk.
            let _ = file.set_len(offset).await;
            let _ = file.flush().await;
            return Err(error);
        }
        file.flush().await?;
        drop(file);
        self.record_written(upload_id, user.id, offset + received)?;
        Ok(ChunkResponse {
            upload_id: session.id,
            received: session.written + received,
            size: session.declared,
            offset: session.written + received,
        })
    }

    /// Finalise a chunked upload: checks the size, fsyncs and atomically moves
    /// the part file into place, then commits the quota reservation.
    pub async fn complete_chunked_upload(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        upload_id: &str,
    ) -> AppResult<ops::EntryDto> {
        self.cleanup_expired_uploads(pool).await;
        let initial = self.upload_snapshot(upload_id, user.id)?;
        let lock = initial.write_lock.clone();
        let _guard = lock.lock_owned().await;
        let session = self.upload_snapshot(upload_id, user.id)?;
        if session.written != session.declared {
            return Err(AppError::conflict(format!(
                "upload is incomplete: {} of {} bytes received",
                session.written, session.declared
            )));
        }
        let (logical, target, in_home, old_size) =
            self.refresh_target(pool, user, &session).await?;
        finalize_temp_file(&session.temp_path, &target).await?;
        self.take_session(upload_id, user.id);
        if in_home {
            let used_delta = i64::try_from(session.declared).unwrap_or(i64::MAX) - old_size;
            quota::commit(pool, user.id, session.reserved, used_delta).await?;
        } else if session.reserved > 0 {
            quota::rollback(pool, user.id, session.reserved).await?;
        }
        ops::entry_for_path(&logical, &target, true).await
    }

    /// Cancel a chunked upload: remove the part file and release the quota.
    pub async fn cancel_chunked_upload(
        &self,
        pool: &SqlitePool,
        user: &AuthUser,
        upload_id: &str,
    ) -> AppResult<()> {
        let initial = self.upload_snapshot(upload_id, user.id)?;
        let lock = initial.write_lock.clone();
        let _guard = lock.lock_owned().await;
        let Some(session) = self.take_session(upload_id, user.id) else {
            return Err(AppError::NotFound("upload not found"));
        };
        let _ = tokio::fs::remove_file(&session.temp_path).await;
        if session.reserved > 0 {
            quota::rollback(pool, user.id, session.reserved).await?;
        }
        Ok(())
    }
}

/// Flush and fsync the temporary file, then move it into place. Falls back to
/// copy + rename when the data and storage directories are on different
/// filesystems (the configured layout allows that).
async fn finalize_temp_file(temp: &Path, target: &Path) -> AppResult<()> {
    let file = tokio::fs::OpenOptions::new().write(true).open(temp).await?;
    file.sync_all().await?;
    drop(file);
    match tokio::fs::rename(temp, target).await {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EXDEV) => {
            let staging = target.with_file_name(format!("{TEMP_PREFIX}{}", Uuid::new_v4()));
            if let Err(copy_error) = tokio::fs::copy(temp, &staging).await {
                let _ = tokio::fs::remove_file(&staging).await;
                return Err(copy_error.into());
            }
            let staged = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&staging)
                .await?;
            staged.sync_all().await?;
            drop(staged);
            if let Err(rename_error) = tokio::fs::rename(&staging, target).await {
                let _ = tokio::fs::remove_file(&staging).await;
                return Err(rename_error.into());
            }
            let _ = tokio::fs::remove_file(temp).await;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Wipe orphaned part files on startup: sessions are in-memory, so nothing in
/// this directory can survive a restart.
pub fn prepare_uploads_dir(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Role, config::Config, db, storage::Storage};

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
        alice: AuthUser,
        bob: AuthUser,
    }

    async fn setup() -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&test_cfg(dir.path())).unwrap();
        let pool = db::connect(&dir.path().join("chunked.db"), 4)
            .await
            .unwrap();
        for (id, username) in [(1_i64, "alice"), (2, "bob")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
                 VALUES (?, ?, 'x', 'user', 1000000, 1, 1)",
            )
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .unwrap();
            std::fs::create_dir_all(storage.home_dir(username)).unwrap();
        }
        Ctx {
            _dir: dir,
            storage,
            pool,
            alice: auth_user(1, "alice"),
            bob: auth_user(2, "bob"),
        }
    }

    async fn init(ctx: &Ctx, name: &str, size: u64, overwrite: bool) -> InitUploadResponse {
        ctx.storage
            .init_chunked_upload(&ctx.pool, &ctx.alice, "/", name, size, overwrite)
            .await
            .unwrap()
    }

    async fn chunk(
        ctx: &Ctx,
        upload_id: &str,
        offset: u64,
        data: &[u8],
    ) -> AppResult<ChunkResponse> {
        ctx.storage
            .write_chunk(
                &ctx.alice,
                upload_id,
                offset,
                Some(data.len() as u64),
                Body::from(data.to_vec()),
            )
            .await
    }

    #[tokio::test]
    async fn chunked_upload_roundtrip() {
        let ctx = setup().await;
        let init = init(&ctx, "movie.bin", 10, false).await;
        assert_eq!(init.offset, 0);
        assert_eq!(init.size, 10);
        assert_eq!(init.chunk_size, CHUNK_SIZE);
        // Quota is reserved up front.
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().reserved_bytes, 10);
        let first = chunk(&ctx, &init.upload_id, 0, b"abcd").await.unwrap();
        assert_eq!(first.received, 4);
        let second = chunk(&ctx, &init.upload_id, 4, b"efghij").await.unwrap();
        assert_eq!(second.received, 10);
        let entry = ctx
            .storage
            .complete_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap();
        assert_eq!(entry.name, "movie.bin");
        assert_eq!(entry.size, 10);
        let physical = ctx.storage.home_dir("alice").join("movie.bin");
        assert_eq!(std::fs::read(&physical).unwrap(), b"abcdefghij".to_vec());
        let info = quota::info(&ctx.pool, 1).await.unwrap();
        assert_eq!(info.used_bytes, 10);
        assert_eq!(info.reserved_bytes, 0);
        // Session is gone: a second complete is a 404.
        let err = ctx
            .storage
            .complete_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn out_of_order_offsets_are_rejected() {
        let ctx = setup().await;
        let init = init(&ctx, "ordered.bin", 10, false).await;
        let err = chunk(&ctx, &init.upload_id, 5, b"xxxxx").await.unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        // Nothing was written.
        assert_eq!(
            chunk(&ctx, &init.upload_id, 0, b"abc")
                .await
                .unwrap()
                .received,
            3
        );
        // Offsets must match the current length exactly.
        let err = chunk(&ctx, &init.upload_id, 2, b"zz").await.unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        // Completing an incomplete upload is a conflict too.
        let err = ctx
            .storage
            .complete_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn chunk_limits_are_enforced() {
        let ctx = setup().await;
        let init = init(&ctx, "small.bin", 4, false).await;
        let err = ctx
            .storage
            .write_chunk(
                &ctx.alice,
                &init.upload_id,
                0,
                Some(5),
                Body::from(vec![0u8; 5]),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        let err = ctx
            .storage
            .write_chunk(
                &ctx.alice,
                &init.upload_id,
                0,
                None,
                Body::from(vec![0u8; 1]),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        let err = chunk(&ctx, &init.upload_id, 0, b"too long")
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn quota_is_reserved_at_init_and_released_on_cancel() {
        let ctx = setup().await;
        // Alice has a 1 MB quota; reserve it all.
        let init = init(&ctx, "big.bin", 1_000_000, false).await;
        assert_eq!(
            quota::info(&ctx.pool, 1).await.unwrap().reserved_bytes,
            1_000_000
        );
        // A second upload that does not fit is rejected immediately.
        let err = ctx
            .storage
            .init_chunked_upload(&ctx.pool, &ctx.alice, "/", "second.bin", 1_000, false)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        // Cancelling releases the reservation and removes the part file.
        ctx.storage
            .cancel_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap();
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().reserved_bytes, 0);
        assert!(!ctx
            .storage
            .uploads_dir()
            .join(format!("{}.part", init.upload_id))
            .exists());
        // The cancellation is visible as a 404 afterwards.
        let err = ctx
            .storage
            .cancel_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ownership_is_enforced() {
        let ctx = setup().await;
        let init = init(&ctx, "private.bin", 10, false).await;
        // Another user cannot write, complete or cancel someone else's upload.
        let err = ctx
            .storage
            .write_chunk(
                &ctx.bob,
                &init.upload_id,
                0,
                Some(2),
                Body::from(vec![0u8; 2]),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
        let err = ctx
            .storage
            .complete_chunked_upload(&ctx.pool, &ctx.bob, &init.upload_id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
        let err = ctx
            .storage
            .cancel_chunked_upload(&ctx.pool, &ctx.bob, &init.upload_id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stale_sessions_are_cleaned_up() {
        let mut ctx = setup().await;
        ctx.storage.upload_ttl_ms = 1;
        let init = init(&ctx, "stale.bin", 100, false).await;
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().reserved_bytes, 100);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let cleaned = ctx.storage.cleanup_expired_uploads(&ctx.pool).await;
        assert_eq!(cleaned, 1);
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().reserved_bytes, 0);
        assert!(ctx.storage.upload_snapshot(&init.upload_id, 1).is_err());
        assert!(!ctx
            .storage
            .uploads_dir()
            .join(format!("{}.part", init.upload_id))
            .exists());
    }

    #[tokio::test]
    async fn concurrent_uploads_are_bounded_per_user() {
        let ctx = setup().await;
        // Small sessions so quota is not the limiting factor.
        let mut ids = Vec::new();
        for index in 0..MAX_UPLOADS_PER_USER {
            let init = init(&ctx, &format!("f{index}.bin"), 1, false).await;
            ids.push(init.upload_id);
        }
        let err = ctx
            .storage
            .init_chunked_upload(&ctx.pool, &ctx.alice, "/", "overflow.bin", 1, false)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        for id in ids {
            let _ = ctx
                .storage
                .cancel_chunked_upload(&ctx.pool, &ctx.alice, &id)
                .await;
        }
    }

    #[tokio::test]
    async fn overwrite_policy_and_exact_bytes() {
        let ctx = setup().await;
        std::fs::write(ctx.storage.home_dir("alice").join("exists.txt"), b"old").unwrap();
        let err = ctx
            .storage
            .init_chunked_upload(&ctx.pool, &ctx.alice, "/", "exists.txt", 3, false)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);

        let init = init(&ctx, "exists.txt", 3, true).await;
        chunk(&ctx, &init.upload_id, 0, b"new").await.unwrap();
        ctx.storage
            .complete_chunked_upload(&ctx.pool, &ctx.alice, &init.upload_id)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(ctx.storage.home_dir("alice").join("exists.txt")).unwrap(),
            b"new".to_vec()
        );
    }

    #[test]
    fn startup_wipes_orphan_parts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("uploads");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("orphan.part"), b"junk").unwrap();
        prepare_uploads_dir(&root).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}
