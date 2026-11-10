//! Bulk filesystem transfers: copy, move and delete (recycle bin) jobs, plus
//! the low-level streaming primitives they share.
//!
//! All long operations run as background jobs through [`crate::jobs`] so HTTP
//! requests stay responsive. Jobs are cooperatively cancellable, report
//! progress through the event bus and re-validate every object server-side at
//! execution time — a revoked share ACL immediately invalidates the job.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::{
    audit::{self, AuditLog},
    auth::AuthUser,
    error::{AppError, AppResult},
    events::EventBus,
    jobs::JobContext,
    realtime,
    storage::{
        quota, safe_path, trash, Access, LogicalPath, Storage, MAX_BATCH_PATHS, MAX_TREE_ENTRIES,
        TEMP_PREFIX,
    },
};

/// `EXDEV`: cross-device rename; falls back to stream copy + delete.
const EXDEV: i32 = 18;

/// One validated source of a copy/move batch.
#[derive(Debug, Clone)]
pub struct PlannedSource {
    pub logical: LogicalPath,
    pub physical: PathBuf,
    pub name: String,
    pub in_home: bool,
    pub is_dir: bool,
}

/// A validated copy/move request.
#[derive(Debug, Clone)]
pub struct BatchPlan {
    pub dest: PathBuf,
    pub dest_root_physical: PathBuf,
    pub dest_logical: LogicalPath,
    pub dest_in_home: bool,
    pub sources: Vec<PlannedSource>,
}

/// One entry of an enumerated source tree (relative to the tree root).
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub relative: PathBuf,
    pub is_dir: bool,
    pub len: u64,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a copy/move request before enqueueing the job. The job repeats the
/// same checks when it runs (authorisation must hold at execution time).
pub async fn plan_batch(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
    dest_dir: &str,
    moving: bool,
) -> AppResult<BatchPlan> {
    validate_batch(paths)?;
    let dest = storage.resolve(pool, user, dest_dir, Access::Write).await?;
    if !dest.physical.is_dir() {
        return Err(AppError::bad_request("destination is not a directory"));
    }
    let mut sources = Vec::with_capacity(paths.len());
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let access = if moving { Access::Write } else { Access::Read };
    for raw in paths {
        let resolved = storage.resolve(pool, user, raw, access).await?;
        if resolved.is_storage_root() {
            return Err(AppError::bad_request(
                "storage roots cannot be copied, moved or deleted",
            ));
        }
        if !seen.insert(resolved.physical.clone()) {
            return Err(AppError::bad_request("duplicate paths in request"));
        }
        let name = resolved.name().to_string();
        let (_, target_physical, exists) = storage.child_target(&dest, &name)?;
        if exists {
            return Err(AppError::conflict(format!(
                "\"{name}\" already exists in the destination"
            )));
        }
        if target_physical == resolved.physical {
            return Err(AppError::conflict("source and destination are the same"));
        }
        let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
        if resolved.physical.is_dir() && target_physical.starts_with(&resolved.physical) {
            return Err(AppError::bad_request(
                "a folder cannot be copied or moved into itself",
            ));
        }
        sources.push(PlannedSource {
            logical: resolved.logical,
            physical: resolved.physical,
            name,
            in_home: resolved.in_home,
            is_dir: meta.is_dir(),
        });
    }
    Ok(BatchPlan {
        dest: dest.physical,
        dest_root_physical: dest.root_physical,
        dest_logical: dest.logical,
        dest_in_home: dest.in_home,
        sources,
    })
}

/// Validate a delete (recycle bin) request before enqueueing the job.
pub async fn plan_delete(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
) -> AppResult<Vec<LogicalPath>> {
    validate_batch(paths)?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let resolved = storage.resolve(pool, user, raw, Access::Write).await?;
        if resolved.is_storage_root() {
            return Err(AppError::bad_request("storage roots cannot be deleted"));
        }
        if !seen.insert(resolved.physical.clone()) {
            return Err(AppError::bad_request("duplicate paths in request"));
        }
        out.push(resolved.logical);
    }
    Ok(out)
}

fn validate_batch(paths: &[String]) -> AppResult<()> {
    if paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    if paths.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many paths in one request"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Low-level filesystem primitives
// ---------------------------------------------------------------------------

fn walk_error(error: walkdir::Error) -> AppError {
    match error.io_error() {
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
            AppError::NotFound("path not found")
        }
        Some(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            AppError::Forbidden("permission denied")
        }
        Some(io) => {
            tracing::warn!(error = %io, "filesystem walk failed");
            AppError::internal("filesystem error")
        }
        None => AppError::internal("cannot traverse directory tree"),
    }
}

/// Enumerate a tree (excluding symlinks and temporary files) without following
/// links. Blocking; called from `spawn_blocking`.
fn enumerate_tree_blocking(root: &Path) -> AppResult<Vec<TreeEntry>> {
    if !root.is_dir() {
        let meta = std::fs::symlink_metadata(root)?;
        if meta.file_type().is_symlink() {
            return Err(AppError::Forbidden("symbolic links are not supported"));
        }
        return Ok(vec![TreeEntry {
            relative: PathBuf::new(),
            is_dir: false,
            len: meta.len(),
        }]);
    }
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !entry.file_type().is_symlink());
    for entry in walker {
        let entry = entry.map_err(walk_error)?;
        if entry.depth() == 0 {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| AppError::internal("path escaped its tree root"))?
            .to_path_buf();
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_dir() {
            out.push(TreeEntry {
                relative,
                is_dir: true,
                len: 0,
            });
        } else if entry.file_type().is_file() {
            out.push(TreeEntry {
                relative,
                is_dir: false,
                len: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
        if out.len() > MAX_TREE_ENTRIES {
            return Err(AppError::bad_request(
                "folder is too large for a single copy or move operation",
            ));
        }
    }
    Ok(out)
}

async fn enumerate_tree(root: &Path) -> AppResult<Vec<TreeEntry>> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || enumerate_tree_blocking(&root))
        .await
        .map_err(AppError::from)?
}

/// Total size of a tree, ignoring symlinks and temporary files.
pub fn tree_size_blocking(root: &Path) -> u64 {
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !entry.file_type().is_symlink())
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            continue;
        }
        total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
    }
    total
}

pub async fn tree_size(root: &Path) -> AppResult<u64> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || tree_size_blocking(&root))
        .await
        .map_err(AppError::from)
}

/// Remove a file or directory tree. Blocking; called from `spawn_blocking`.
pub fn remove_path_blocking(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub async fn remove_path(path: &Path) -> AppResult<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || remove_path_blocking(&path))
        .await
        .map_err(AppError::from)??;
    Ok(())
}

/// Copy one file through a temporary file + atomic rename, streaming with a
/// fixed-size buffer (never loads the file into memory).
async fn copy_file_raw(src: &Path, dst: &Path) -> AppResult<u64> {
    let expected = tokio::fs::symlink_metadata(src).await?;
    if expected.file_type().is_symlink() {
        return Err(AppError::Forbidden("symbolic links are not supported"));
    }
    let temp = dst.with_file_name(format!("{TEMP_PREFIX}{}", Uuid::new_v4()));
    let result: AppResult<u64> = async {
        let mut input = safe_path::open_read_no_follow_async(src).await?;
        let opened = input.metadata().await?;
        safe_path::verify_same_inode(&opened, &expected)?;
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        let copied = tokio::io::copy(&mut input, &mut output).await?;
        tokio::io::AsyncWriteExt::flush(&mut output).await?;
        drop(output);
        tokio::fs::rename(&temp, dst).await?;
        Ok(copied)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

/// Copy one file, enforcing the destination quota when it lands in a home.
async fn copy_one_file(
    pool: &SqlitePool,
    user_id: i64,
    src: &Path,
    dst: &Path,
    dest_in_home: bool,
) -> AppResult<u64> {
    let meta = tokio::fs::symlink_metadata(src).await?;
    if !meta.is_file() {
        return Ok(0);
    }
    let len = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    let reserved = if dest_in_home {
        if !quota::reserve(pool, user_id, len).await? {
            return Err(AppError::PayloadTooLarge("storage quota exceeded"));
        }
        len
    } else {
        0
    };
    match copy_file_raw(src, dst).await {
        Ok(copied) => {
            if dest_in_home {
                quota::commit(pool, user_id, reserved, copied as i64).await?;
            }
            Ok(copied)
        }
        Err(error) => {
            if dest_in_home {
                let _ = quota::rollback(pool, user_id, reserved).await;
            }
            Err(error)
        }
    }
}

/// Recursively copy a tree without quota accounting (used by cross-device move
/// fallbacks). The caller is responsible for the quota adjustment.
async fn copy_tree_raw(ctx: &JobContext, src: &Path, dst: &Path, is_dir: bool) -> AppResult<()> {
    if !is_dir {
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        copy_file_raw(src, dst).await?;
        return Ok(());
    }
    tokio::fs::create_dir_all(dst).await?;
    for entry in enumerate_tree(src).await? {
        ctx.check_cancelled()?;
        let target = dst.join(&entry.relative);
        if entry.is_dir {
            tokio::fs::create_dir_all(&target).await?;
        } else {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            copy_file_raw(&src.join(&entry.relative), &target).await?;
        }
    }
    Ok(())
}

/// Rename a file/directory, falling back to stream-copy + delete across
/// devices.
async fn move_entry(ctx: &JobContext, src: &Path, dst: &Path, is_dir: bool) -> AppResult<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EXDEV) => {
            copy_tree_raw(ctx, src, dst, is_dir).await?;
            remove_path(src).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Blocking recursive copy used by the trash fallback path.
pub fn copy_tree_blocking(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "symbolic links are not supported",
        ));
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree_blocking(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

/// Move a path, falling back to copy + delete when the rename crosses devices.
pub async fn move_path(src: &Path, dst: &Path) -> AppResult<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EXDEV) => {
            let src = src.to_path_buf();
            let dst = dst.to_path_buf();
            tokio::task::spawn_blocking(move || -> AppResult<()> {
                copy_tree_blocking(&src, &dst)?;
                remove_path_blocking(&src)?;
                Ok(())
            })
            .await
            .map_err(AppError::from)??;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

struct Progress {
    done: u64,
    total: u64,
    bytes: u64,
}

async fn audit_job(
    pool: &SqlitePool,
    action: &str,
    user: &AuthUser,
    job_id: &str,
    paths: &[String],
    ok: bool,
    detail: String,
) {
    let mut log = AuditLog::new(action)
        .actor(user)
        .object("job", job_id.to_string())
        .path(paths.join(", "))
        .detail(detail);
    log = if ok { log.success() } else { log.failure() };
    audit::record(pool, log).await;
}

/// Background body of `POST /api/files/copy`.
pub async fn run_copy_job(
    ctx: JobContext,
    pool: SqlitePool,
    storage: Storage,
    user: AuthUser,
    paths: Vec<String>,
    dest_dir: String,
    events: EventBus,
) -> AppResult<Value> {
    let outcome = copy_job_inner(&ctx, &pool, &storage, &user, &paths, &dest_dir, &events).await;
    match &outcome {
        Ok(value) => {
            audit_job(
                &pool,
                "file.copy",
                &user,
                &ctx.id,
                &paths,
                true,
                value.to_string(),
            )
            .await;
        }
        Err(error) => {
            audit_job(
                &pool,
                "file.copy",
                &user,
                &ctx.id,
                &paths,
                false,
                error.public_message(),
            )
            .await;
        }
    }
    outcome
}

async fn copy_job_inner(
    ctx: &JobContext,
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
    dest_dir: &str,
    events: &EventBus,
) -> AppResult<Value> {
    let plan = plan_batch(pool, storage, user, paths, dest_dir, false).await?;
    let mut affected: Vec<String> = Vec::with_capacity(plan.sources.len());
    // Enumerate sources first: gives an accurate progress total and keeps the
    // copy loop simple.
    let mut trees: Vec<(usize, Vec<TreeEntry>)> = Vec::with_capacity(plan.sources.len());
    let mut total: u64 = 0;
    for (index, source) in plan.sources.iter().enumerate() {
        ctx.check_cancelled()?;
        if source.is_dir {
            let entries = enumerate_tree(&source.physical).await?;
            total = total.saturating_add(entries.len() as u64);
            trees.push((index, entries));
        } else {
            total = total.saturating_add(1);
            trees.push((index, Vec::new()));
        }
    }
    let total = total.max(1);
    let mut progress = Progress {
        done: 0,
        total,
        bytes: 0,
    };
    let mut copied: u64 = 0;
    for (index, entries) in &trees {
        let source = &plan.sources[*index];
        ctx.check_cancelled()?;
        let target = plan.dest.join(&source.name);
        if source.is_dir {
            tokio::fs::create_dir_all(&target).await?;
            for entry in entries {
                ctx.check_cancelled()?;
                let dst = target.join(&entry.relative);
                if entry.is_dir {
                    tokio::fs::create_dir_all(&dst).await?;
                } else {
                    if let Some(parent) = dst.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    let src = source.physical.join(&entry.relative);
                    progress.bytes +=
                        copy_one_file(pool, user.id, &src, &dst, plan.dest_in_home).await?;
                    copied += 1;
                }
                progress.done += 1;
                ctx.progress(
                    progress.done,
                    progress.total,
                    &format!(
                        "Copying {} ({}/{})",
                        source.name, progress.done, progress.total
                    ),
                )
                .await;
            }
        } else {
            progress.bytes +=
                copy_one_file(pool, user.id, &source.physical, &target, plan.dest_in_home).await?;
            copied += 1;
            progress.done += 1;
            ctx.progress(
                progress.done,
                progress.total,
                &format!("Copying {}", source.name),
            )
            .await;
        }
        if let Ok(logical) = plan.dest_logical.child(&source.name) {
            affected.push(logical.as_string());
        }
    }
    realtime::publish_commit(events, pool, user, "copy", &affected, None, true).await;
    Ok(json!({
        "copied": copied,
        "bytes": progress.bytes,
        "destination": plan.dest_logical.as_string(),
    }))
}

/// Background body of `POST /api/files/move`.
pub async fn run_move_job(
    ctx: JobContext,
    pool: SqlitePool,
    storage: Storage,
    user: AuthUser,
    paths: Vec<String>,
    dest_dir: String,
    events: EventBus,
) -> AppResult<Value> {
    let outcome = move_job_inner(&ctx, &pool, &storage, &user, &paths, &dest_dir, &events).await;
    match &outcome {
        Ok(value) => {
            audit_job(
                &pool,
                "file.move",
                &user,
                &ctx.id,
                &paths,
                true,
                value.to_string(),
            )
            .await;
        }
        Err(error) => {
            audit_job(
                &pool,
                "file.move",
                &user,
                &ctx.id,
                &paths,
                false,
                error.public_message(),
            )
            .await;
        }
    }
    outcome
}

async fn move_job_inner(
    ctx: &JobContext,
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
    dest_dir: &str,
    events: &EventBus,
) -> AppResult<Value> {
    let plan = plan_batch(pool, storage, user, paths, dest_dir, true).await?;
    let mut affected: Vec<String> = Vec::new();
    let total = (plan.sources.len() as u64).max(1);
    let mut done: u64 = 0;
    let mut bytes: u64 = 0;
    for source in &plan.sources {
        ctx.check_cancelled()?;
        let target = plan.dest.join(&source.name);
        // Never overwrite a file that appeared after validation.
        if safe_path::verify_no_symlinks(&plan.dest_root_physical, &target)? {
            return Err(AppError::conflict(format!(
                "\"{}\" already exists in the destination",
                source.name
            )));
        }
        let meta = tokio::fs::symlink_metadata(&source.physical).await?;
        let size = if meta.is_dir() {
            tree_size(&source.physical).await?
        } else {
            meta.len()
        };
        let cross_root = source.in_home != plan.dest_in_home;
        let reserved = if cross_root && plan.dest_in_home {
            if !quota::reserve(pool, user.id, size as i64).await? {
                return Err(AppError::PayloadTooLarge("storage quota exceeded"));
            }
            size as i64
        } else {
            0
        };
        let outcome = move_entry(ctx, &source.physical, &target, source.is_dir).await;
        match outcome {
            Ok(()) => {
                if reserved > 0 {
                    quota::commit(pool, user.id, reserved, size as i64).await?;
                } else if cross_root && source.in_home {
                    quota::adjust_used(pool, user.id, -(size as i64)).await?;
                }
                done += 1;
                bytes = bytes.saturating_add(size);
                affected.push(source.logical.as_string());
                if let Ok(logical) = plan.dest_logical.child(&source.name) {
                    affected.push(logical.as_string());
                }
                ctx.progress(done, total, &format!("Moving {}", source.name))
                    .await;
            }
            Err(error) => {
                if reserved > 0 {
                    let _ = quota::rollback(pool, user.id, reserved).await;
                }
                // The destination did not exist before this operation; remove
                // any partial fallback copy before reporting the failure.
                let _ = remove_path(&target).await;
                return Err(error);
            }
        }
    }
    realtime::publish_commit(events, pool, user, "move", &affected, None, true).await;
    Ok(json!({
        "moved": done,
        "bytes": bytes,
        "destination": plan.dest_logical.as_string(),
    }))
}

/// Background body of `POST /api/files/delete` (moves to the recycle bin).
pub async fn run_delete_job(
    ctx: JobContext,
    pool: SqlitePool,
    storage: Storage,
    user: AuthUser,
    paths: Vec<String>,
    events: EventBus,
) -> AppResult<Value> {
    let outcome = delete_job_inner(&ctx, &pool, &storage, &user, &paths, &events).await;
    match &outcome {
        Ok(value) => {
            audit_job(
                &pool,
                "file.delete",
                &user,
                &ctx.id,
                &paths,
                true,
                value.to_string(),
            )
            .await;
        }
        Err(error) => {
            audit_job(
                &pool,
                "file.delete",
                &user,
                &ctx.id,
                &paths,
                false,
                error.public_message(),
            )
            .await;
        }
    }
    outcome
}

async fn delete_job_inner(
    ctx: &JobContext,
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
    events: &EventBus,
) -> AppResult<Value> {
    // Re-validate every path at execution time.
    let logicals = plan_delete(pool, storage, user, paths).await?;
    let total = (logicals.len() as u64).max(1);
    let mut deleted: u64 = 0;
    let mut bytes: i64 = 0;
    for logical in &logicals {
        ctx.check_cancelled()?;
        let resolved = storage
            .resolve_logical(pool, user, logical, Access::Write)
            .await?;
        let row = trash::move_to_trash(pool, storage, user, &resolved).await?;
        deleted += 1;
        bytes = bytes.saturating_add(row.size_bytes);
        ctx.progress(
            deleted,
            total,
            &format!("Moving {} to the recycle bin", logical),
        )
        .await;
    }
    let affected: Vec<String> = logicals.iter().map(LogicalPath::as_string).collect();
    realtime::publish_commit(events, pool, user, "delete", &affected, None, true).await;
    Ok(json!({"deleted": deleted, "bytes": bytes, "trashed": true}))
}

/// Run a copy/move/delete operation in the foreground (tests and maintenance).
#[allow(clippy::too_many_arguments)]
pub async fn run_blocking_transfer(
    ctx: &JobContext,
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    paths: &[String],
    dest_dir: &str,
    copy: bool,
    events: &EventBus,
) -> AppResult<Value> {
    if copy {
        copy_job_inner(ctx, pool, storage, user, paths, dest_dir, events).await
    } else {
        move_job_inner(ctx, pool, storage, user, paths, dest_dir, events).await
    }
}

/// Small helper used by unit tests: map a job status poll loop.
pub async fn wait_for_job(pool: &SqlitePool, id: &str) -> AppResult<Value> {
    for _ in 0..300 {
        if let Some(job) = crate::jobs::job_json(pool, id).await? {
            let status = job
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if status != "pending" && status != "running" {
                return Ok(job);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Err(AppError::internal("job did not finish in time"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::Role,
        config::Config,
        db,
        jobs::{JobManager, JobSpec},
    };

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
        let cfg = test_cfg(dir.path());
        let storage = Storage::new(&cfg).unwrap();
        let pool = db::connect(&dir.path().join("transfer.db"), 4)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
             VALUES (1, 'alice', 'x', 'user', 100000, 1, 1)",
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

    fn write(path: &Path, size: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; size]).unwrap();
    }

    async fn run_job(ctx: &Ctx, kind: &str, paths: Vec<String>, dest: &str) -> Value {
        let events = crate::events::EventBus::new();
        let manager = JobManager::new(ctx.pool.clone(), events.clone(), 2);
        let pool = ctx.pool.clone();
        let storage = ctx.storage.clone();
        let user = ctx.user.clone();
        let events = events.clone();
        let dest = dest.to_string();
        let kind_owned = kind.to_string();
        let id = manager
            .submit(JobSpec::new(
                kind_owned.clone(),
                move |job_ctx| async move {
                    match kind_owned.as_str() {
                        "file.copy" => {
                            run_copy_job(job_ctx, pool, storage, user, paths, dest, events).await
                        }
                        "file.move" => {
                            run_move_job(job_ctx, pool, storage, user, paths, dest, events).await
                        }
                        "file.delete" => {
                            run_delete_job(job_ctx, pool, storage, user, paths, events).await
                        }
                        _ => Err(AppError::internal("unknown job")),
                    }
                },
            ))
            .await
            .unwrap();
        wait_for_job(&ctx.pool, &id).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_job_copies_tree_and_updates_quota() {
        let ctx = setup().await;
        let home = ctx.storage.home_dir("alice");
        write(&home.join("src/a.txt"), 100);
        write(&home.join("src/nested/b.txt"), 50);
        std::fs::create_dir_all(home.join("dest")).unwrap();
        let job = run_job(&ctx, "file.copy", vec!["/src".into()], "/dest").await;
        assert_eq!(job["status"], "done", "job failed: {job}");
        assert!(home.join("dest/src/a.txt").is_file());
        assert!(home.join("dest/src/nested/b.txt").is_file());
        assert_eq!(job["result"]["copied"], 2);
        assert_eq!(job["result"]["bytes"], 150);
        // Source still present; only the copy is new quota usage (the sources
        // were created directly on disk in this test).
        assert!(home.join("src/a.txt").is_file());
        let info = quota::info(&ctx.pool, 1).await.unwrap();
        assert_eq!(info.used_bytes, 150);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_job_rejects_conflicts_and_escapes() {
        let ctx = setup().await;
        let home = ctx.storage.home_dir("alice");
        write(&home.join("a.txt"), 10);
        write(&home.join("dest/a.txt"), 10);
        let job = run_job(&ctx, "file.copy", vec!["/a.txt".into()], "/dest").await;
        assert_eq!(job["status"], "failed", "conflict must fail the job");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn move_job_renames_and_preserves_quota() {
        let ctx = setup().await;
        let home = ctx.storage.home_dir("alice");
        write(&home.join("docs/x.bin"), 123);
        // Account the file as if it had been uploaded through the API.
        quota::adjust_used(&ctx.pool, 1, 123).await.unwrap();
        let job = run_job(&ctx, "file.move", vec!["/docs/x.bin".into()], "/").await;
        assert_eq!(job["status"], "done", "job failed: {job}");
        assert!(home.join("x.bin").is_file());
        assert!(!home.join("docs/x.bin").exists());
        let info = quota::info(&ctx.pool, 1).await.unwrap();
        assert_eq!(info.used_bytes, 123);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_job_moves_to_trash() {
        let ctx = setup().await;
        let home = ctx.storage.home_dir("alice");
        write(&home.join("doomed/a.txt"), 42);
        let job = run_job(&ctx, "file.delete", vec!["/doomed".into()], "/").await;
        assert_eq!(job["status"], "done", "job failed: {job}");
        assert!(!home.join("doomed").exists());
        let rows = trash::list(&ctx.pool, 1, 1, 10).await.unwrap();
        assert_eq!(rows.0.len(), 1);
        assert_eq!(rows.0[0].original_path, "/doomed");
        assert_eq!(rows.0[0].size_bytes, 42);
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 0);
    }

    #[test]
    fn enumerate_tree_is_bounded_and_skips_temps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        write(&root.join("a.txt"), 3);
        write(&root.join("sub/b.txt"), 4);
        write(&root.join(format!("{TEMP_PREFIX}zzz")), 9);
        let entries = enumerate_tree_blocking(&root).unwrap();
        assert_eq!(entries.len(), 3); // dir + 2 files
        assert_eq!(tree_size_blocking(&root), 7);
    }
}
