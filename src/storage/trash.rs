//! Recycle bin.
//!
//! Deleted objects are *moved* (rename, or stream-copy across devices) into a
//! per-user directory below `<storage>/.trash/<user_id>/<uuid>` and recorded in
//! the `trash` table with the original logical path. Home-directory deletions
//! release quota; restores re-reserve it transactionally before moving the data
//! back. Permanent deletion and "empty bin" remove payloads for good.

use std::path::PathBuf;

use serde::Serialize;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    db,
    error::{AppError, AppResult},
    storage::{quota, safe_path, transfer, LogicalPath, ResolvedPath, Storage, MAX_BATCH_PATHS},
};

/// Default number of expired entries physically removed per maintenance pass.
const EXPIRED_BATCH: i64 = 50;

/// A recycle-bin entry as exposed by the API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashEntryDto {
    pub id: String,
    pub original_path: String,
    pub size_bytes: i64,
    pub kind: String,
    pub deleted_at: i64,
}

/// Result of moving one object to the recycle bin.
#[derive(Debug, Clone)]
pub struct TrashRow {
    pub id: String,
    pub original_path: String,
    pub size_bytes: i64,
    pub kind: String,
}

/// One successfully restored item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredItem {
    pub id: String,
    pub path: String,
}

struct TrashRecord {
    id: String,
    original_path: String,
    trashed_path: String,
    size_bytes: i64,
    kind: String,
    deleted_at: i64,
}

fn map_record(row: sqlx::sqlite::SqliteRow) -> TrashRecord {
    TrashRecord {
        id: row.get("id"),
        original_path: row.get("original_path"),
        trashed_path: row.get("trashed_path"),
        size_bytes: row.get("size_bytes"),
        kind: row.get("kind"),
        deleted_at: row.get("deleted_at"),
    }
}

fn map_dto(record: &TrashRecord) -> TrashEntryDto {
    TrashEntryDto {
        id: record.id.clone(),
        original_path: record.original_path.clone(),
        size_bytes: record.size_bytes,
        kind: record.kind.clone(),
        deleted_at: record.deleted_at,
    }
}

/// Total bytes currently held in a user's recycle bin.
pub async fn total_bytes(pool: &SqlitePool, user_id: i64) -> AppResult<i64> {
    let total: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM trash WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(total)
}

/// Page through a user's recycle bin (newest first).
pub async fn list(
    pool: &SqlitePool,
    user_id: i64,
    page: i64,
    per_page: i64,
) -> AppResult<(Vec<TrashEntryDto>, i64)> {
    let page = page.max(1);
    let per_page = per_page.clamp(1, 500);
    let offset = (page - 1).saturating_mul(per_page);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trash WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query(
        "SELECT id, original_path, trashed_path, size_bytes, kind, deleted_at
         FROM trash WHERE user_id = ? ORDER BY deleted_at DESC LIMIT ? OFFSET ?",
    )
    .bind(user_id)
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok((
        rows.into_iter()
            .map(|row| map_dto(&map_record(row)))
            .collect(),
        total,
    ))
}

/// Resolve the physical payload path of a trash row, validating that it stays
/// inside the recycle bin root.
fn payload_path(storage: &Storage, trashed_path: &str) -> AppResult<PathBuf> {
    let candidate = storage.trash_root.join(trashed_path);
    let exists = safe_path::verify_no_symlinks(&storage.trash_root, &candidate)?;
    if !exists {
        return Err(AppError::NotFound("recycle bin data is missing"));
    }
    safe_path::verify_containment(&storage.trash_root, &candidate)?;
    Ok(candidate)
}

/// Move one existing object to the recycle bin.
pub async fn move_to_trash(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    resolved: &ResolvedPath,
) -> AppResult<TrashRow> {
    if resolved.is_storage_root() {
        return Err(AppError::bad_request("storage roots cannot be deleted"));
    }
    let meta = tokio::fs::symlink_metadata(&resolved.physical).await?;
    let is_dir = meta.is_dir();
    let size = if is_dir {
        transfer::tree_size(&resolved.physical).await?
    } else {
        meta.len()
    };
    let id = Uuid::new_v4().to_string();
    let bin_dir = storage.ensure_trash_dir(user.id)?;
    let payload = bin_dir.join(&id);
    transfer::move_path(&resolved.physical, &payload).await?;
    let size_bytes = i64::try_from(size).unwrap_or(i64::MAX);
    let kind = if is_dir { "dir" } else { "file" }.to_string();
    let trashed_path = payload
        .strip_prefix(&storage.trash_root)
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|_| format!("{}/{}", user.id, id));
    let original_path = resolved.logical.as_string();
    let inserted = sqlx::query(
        "INSERT INTO trash (id, user_id, original_path, trashed_path, size_bytes, kind, deleted_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&original_path)
    .bind(&trashed_path)
    .bind(size_bytes)
    .bind(&kind)
    .bind(db::now_ms())
    .execute(pool)
    .await;
    if let Err(error) = inserted {
        // Never leave data orphaned: undo the move when the row cannot be
        // recorded.
        let _ = transfer::move_path(&payload, &resolved.physical).await;
        return Err(error.into());
    }
    if resolved.in_home && size_bytes > 0 {
        quota::adjust_used(pool, user.id, -size_bytes).await?;
    }
    Ok(TrashRow {
        id,
        original_path,
        size_bytes,
        kind,
    })
}

struct RestorePlan {
    record: TrashRecord,
    logical: LogicalPath,
    target: PathBuf,
    in_home: bool,
}

async fn load_records(
    pool: &SqlitePool,
    user_id: i64,
    ids: &[String],
) -> AppResult<Vec<TrashRecord>> {
    if ids.is_empty() {
        return Err(AppError::bad_request("at least one trash id is required"));
    }
    if ids.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many ids in one request"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        let row = sqlx::query(
            "SELECT id, original_path, trashed_path, size_bytes, kind, deleted_at
             FROM trash WHERE id = ? AND user_id = ?",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound("recycle bin entry not found"))?;
        records.push(map_record(row));
    }
    Ok(records)
}

/// Restore recycle-bin entries to their original logical paths.
///
/// Every destination is validated (ACL + conflict + quota) *before* the first
/// byte is moved, so a conflicting batch never partially restores.
pub async fn restore(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    ids: &[String],
) -> AppResult<Vec<RestoredItem>> {
    let records = load_records(pool, user.id, ids).await?;
    let mut plans: Vec<RestorePlan> = Vec::with_capacity(records.len());
    for record in records {
        let logical = LogicalPath::parse(&record.original_path)
            .map_err(|_| AppError::internal("recycle bin entry has an invalid path"))?;
        if logical.is_root() {
            return Err(AppError::internal("recycle bin entry points at a root"));
        }
        let root = storage.resolve_root(pool, user, &logical).await?;
        if !root.perm.allows_write() {
            return Err(AppError::Forbidden("write permission required"));
        }
        let target = safe_path::physical_path(&root.physical, logical.relative_segments())?;
        let exists = safe_path::verify_no_symlinks(&root.physical, &target)?;
        if exists {
            return Err(AppError::conflict(format!(
                "\"{logical}\" already exists; rename or remove it before restoring"
            )));
        }
        safe_path::verify_containment(&root.physical, &target)?;
        plans.push(RestorePlan {
            record,
            logical,
            target,
            in_home: root.in_home,
        });
    }

    let reserve_total: i64 = plans
        .iter()
        .filter(|plan| plan.in_home)
        .map(|plan| plan.record.size_bytes.max(0))
        .sum();
    if reserve_total > 0 && !quota::reserve(pool, user.id, reserve_total).await? {
        return Err(AppError::PayloadTooLarge("storage quota exceeded"));
    }
    let mut reserved_remaining = reserve_total;
    let mut restored = Vec::with_capacity(plans.len());
    for plan in plans {
        let source = payload_path(storage, &plan.record.trashed_path)?;
        let outcome: AppResult<()> = async {
            if let Some(parent) = plan.target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            transfer::move_path(&source, &plan.target).await?;
            sqlx::query("DELETE FROM trash WHERE id = ? AND user_id = ?")
                .bind(&plan.record.id)
                .bind(user.id)
                .execute(pool)
                .await?;
            if plan.in_home && plan.record.size_bytes > 0 {
                quota::commit(
                    pool,
                    user.id,
                    plan.record.size_bytes,
                    plan.record.size_bytes,
                )
                .await?;
                reserved_remaining = (reserved_remaining - plan.record.size_bytes).max(0);
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            if reserved_remaining > 0 {
                let _ = quota::rollback(pool, user.id, reserved_remaining).await;
            }
            return Err(error);
        }
        restored.push(RestoredItem {
            id: plan.record.id,
            path: plan.logical.as_string(),
        });
    }
    Ok(restored)
}

/// Permanently delete recycle-bin entries (irreversible).
pub async fn delete_permanent(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
    ids: &[String],
) -> AppResult<(i64, i64, Vec<String>)> {
    let records = load_records(pool, user.id, ids).await?;
    let mut deleted = 0i64;
    let mut bytes = 0i64;
    let mut original_paths = Vec::with_capacity(records.len());
    for record in records {
        if let Ok(payload) = payload_path(storage, &record.trashed_path) {
            transfer::remove_path(&payload).await?;
        }
        sqlx::query("DELETE FROM trash WHERE id = ? AND user_id = ?")
            .bind(&record.id)
            .bind(user.id)
            .execute(pool)
            .await?;
        deleted += 1;
        bytes = bytes.saturating_add(record.size_bytes);
        original_paths.push(record.original_path);
    }
    Ok((deleted, bytes, original_paths))
}

/// Empty the user's recycle bin, permanently deleting every payload.
pub async fn empty(
    pool: &SqlitePool,
    storage: &Storage,
    user: &AuthUser,
) -> AppResult<(i64, i64, Vec<String>)> {
    let rows = sqlx::query(
        "SELECT id, original_path, trashed_path, size_bytes, kind, deleted_at
         FROM trash WHERE user_id = ?",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await?;
    let records: Vec<TrashRecord> = rows.into_iter().map(map_record).collect();
    let mut deleted = 0i64;
    let mut bytes = 0i64;
    let mut removable: Vec<String> = Vec::new();
    let mut original_paths = Vec::with_capacity(records.len());
    for record in &records {
        if let Ok(payload) = payload_path(storage, &record.trashed_path) {
            if let Err(error) = transfer::remove_path(&payload).await {
                // Keep the row so the payload can be retried later.
                tracing::warn!(error = %error, id = %record.id, "cannot remove recycle bin payload");
                continue;
            }
        }
        deleted += 1;
        bytes = bytes.saturating_add(record.size_bytes);
        removable.push(record.id.clone());
        original_paths.push(record.original_path.clone());
    }
    for id in removable {
        sqlx::query("DELETE FROM trash WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user.id)
            .execute(pool)
            .await?;
    }
    Ok((deleted, bytes, original_paths))
}

/// Remove recycle-bin entries older than `retention_days` for one user. Used by
/// the trash list endpoint so the `trashRetentionDays` setting is honoured
/// without a background scheduler. Returns the number of purged entries.
pub async fn cleanup_user_expired(
    pool: &SqlitePool,
    storage: &Storage,
    user_id: i64,
    retention_days: i64,
) -> AppResult<u64> {
    let retention_days = retention_days.clamp(1, 3650);
    let cutoff = db::now_ms() - retention_days * 86_400_000;
    let rows = sqlx::query(
        "SELECT id, original_path, trashed_path, size_bytes, kind, deleted_at
         FROM trash WHERE user_id = ? AND deleted_at < ? ORDER BY deleted_at ASC LIMIT ?",
    )
    .bind(user_id)
    .bind(cutoff)
    .bind(EXPIRED_BATCH)
    .fetch_all(pool)
    .await?;
    let mut purged = 0u64;
    for record in rows.into_iter().map(map_record) {
        if let Ok(payload) = payload_path(storage, &record.trashed_path) {
            if transfer::remove_path(&payload).await.is_err() {
                continue;
            }
        }
        sqlx::query("DELETE FROM trash WHERE id = ?")
            .bind(&record.id)
            .execute(pool)
            .await?;
        purged += 1;
    }
    Ok(purged)
}

/// Remove expired recycle-bin entries for every user (maintenance helper).
pub async fn cleanup_all_expired(
    pool: &SqlitePool,
    storage: &Storage,
    retention_days: i64,
) -> AppResult<u64> {
    let user_ids: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT user_id FROM trash")
        .fetch_all(pool)
        .await?;
    let mut purged = 0u64;
    for user_id in user_ids {
        purged += cleanup_user_expired(pool, storage, user_id, retention_days).await?;
    }
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::Role,
        config::Config,
        db,
        storage::{ops, shares, Access, Storage},
    };

    fn test_cfg(dir: &std::path::Path) -> Config {
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
        let pool = db::connect(&dir.path().join("trash.db"), 4).await.unwrap();
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

    async fn upload(ctx: &Ctx, dir: &str, name: &str, data: &[u8]) -> ops::EntryDto {
        let plan = ops::plan_upload(
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
        use tokio::io::AsyncWriteExt;
        file.write_all(data).await.unwrap();
        ops::commit_upload(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            ops::UploadPlan { file, ..plan },
            data.len() as u64,
        )
        .await
        .unwrap()
    }

    async fn resolve(ctx: &Ctx, path: &str) -> ResolvedPath {
        ctx.storage
            .resolve(&ctx.pool, &ctx.user, path, Access::Write)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn delete_list_restore_roundtrip() {
        let ctx = setup().await;
        upload(&ctx, "/", "keep.txt", b"payload").await;
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 7);

        let resolved = resolve(&ctx, "/keep.txt").await;
        let row = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &resolved)
            .await
            .unwrap();
        assert_eq!(row.original_path, "/keep.txt");
        assert_eq!(row.size_bytes, 7);
        assert!(!ctx.storage.home_dir("alice").join("keep.txt").exists());
        // Deletion releases quota and the payload is counted as trash.
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 0);
        assert_eq!(total_bytes(&ctx.pool, 1).await.unwrap(), 7);
        assert!(ctx.storage.user_trash_dir(1).join(&row.id).exists());

        let (items, total) = list(&ctx.pool, 1, 1, 10).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].original_path, "/keep.txt");
        assert_eq!(items[0].kind, "file");

        let restored = restore(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            std::slice::from_ref(&row.id),
        )
        .await
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].path, "/keep.txt");
        assert_eq!(
            std::fs::read(ctx.storage.home_dir("alice").join("keep.txt")).unwrap(),
            b"payload".to_vec()
        );
        assert_eq!(quota::info(&ctx.pool, 1).await.unwrap().used_bytes, 7);
        assert_eq!(total_bytes(&ctx.pool, 1).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn restore_directory_tree() {
        let ctx = setup().await;
        let home = ctx.storage.home_dir("alice");
        std::fs::create_dir_all(home.join("docs/sub")).unwrap();
        std::fs::write(home.join("docs/a.txt"), b"a").unwrap();
        std::fs::write(home.join("docs/sub/b.txt"), b"bb").unwrap();
        let resolved = resolve(&ctx, "/docs").await;
        let row = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &resolved)
            .await
            .unwrap();
        assert_eq!(row.kind, "dir");
        assert_eq!(row.size_bytes, 3);
        assert!(!home.join("docs").exists());
        restore(&ctx.pool, &ctx.storage, &ctx.user, &[row.id])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(home.join("docs/a.txt")).unwrap(),
            b"a".to_vec()
        );
        assert_eq!(
            std::fs::read(home.join("docs/sub/b.txt")).unwrap(),
            b"bb".to_vec()
        );
    }

    #[tokio::test]
    async fn restore_conflict_and_missing_entry_are_rejected() {
        let ctx = setup().await;
        upload(&ctx, "/", "conflict.txt", b"one").await;
        let resolved = resolve(&ctx, "/conflict.txt").await;
        let row = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &resolved)
            .await
            .unwrap();
        upload(&ctx, "/", "conflict.txt", b"two").await;
        let error = restore(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            std::slice::from_ref(&row.id),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
        // The new file was not overwritten and the trashed payload is intact.
        assert_eq!(
            std::fs::read(ctx.storage.home_dir("alice").join("conflict.txt")).unwrap(),
            b"two".to_vec()
        );
        let error = restore(&ctx.pool, &ctx.storage, &ctx.user, &["missing".into()])
            .await
            .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn permanent_delete_and_empty() {
        let ctx = setup().await;
        upload(&ctx, "/", "a.txt", b"aaa").await;
        upload(&ctx, "/", "b.txt", b"bbb").await;
        let a = resolve(&ctx, "/a.txt").await;
        let row_a = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &a)
            .await
            .unwrap();
        let b = resolve(&ctx, "/b.txt").await;
        let row_b = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &b)
            .await
            .unwrap();

        let (deleted, bytes, paths) = delete_permanent(
            &ctx.pool,
            &ctx.storage,
            &ctx.user,
            std::slice::from_ref(&row_a.id),
        )
        .await
        .unwrap();
        assert_eq!((deleted, bytes), (1, 3));
        assert_eq!(paths, vec!["/a.txt".to_string()]);
        assert!(!ctx.storage.user_trash_dir(1).join(&row_a.id).exists());
        assert_eq!(list(&ctx.pool, 1, 1, 10).await.unwrap().1, 1);

        let (deleted, bytes, paths) = empty(&ctx.pool, &ctx.storage, &ctx.user).await.unwrap();
        assert_eq!((deleted, bytes), (1, 3));
        assert_eq!(paths, vec!["/b.txt".to_string()]);
        assert!(!ctx.storage.user_trash_dir(1).join(&row_b.id).exists());
        assert_eq!(list(&ctx.pool, 1, 1, 10).await.unwrap().1, 0);
    }

    #[tokio::test]
    async fn retention_cleanup_purges_old_entries() {
        let ctx = setup().await;
        upload(&ctx, "/", "old.txt", b"old").await;
        let resolved = resolve(&ctx, "/old.txt").await;
        let row = move_to_trash(&ctx.pool, &ctx.storage, &ctx.user, &resolved)
            .await
            .unwrap();
        // Nothing expires with the default retention.
        assert_eq!(
            cleanup_user_expired(&ctx.pool, &ctx.storage, 1, 30)
                .await
                .unwrap(),
            0
        );
        // Force the row to look old.
        sqlx::query("UPDATE trash SET deleted_at = 1 WHERE id = ?")
            .bind(&row.id)
            .execute(&ctx.pool)
            .await
            .unwrap();
        assert_eq!(
            cleanup_user_expired(&ctx.pool, &ctx.storage, 1, 30)
                .await
                .unwrap(),
            1
        );
        assert!(!ctx.storage.user_trash_dir(1).join(&row.id).exists());
        assert_eq!(total_bytes(&ctx.pool, 1).await.unwrap(), 0);
        assert_eq!(
            cleanup_all_expired(&ctx.pool, &ctx.storage, 30)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn read_only_share_member_cannot_restore() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(&test_cfg(dir.path())).unwrap();
        let pool = db::connect(&dir.path().join("trash-share.db"), 4)
            .await
            .unwrap();
        for (id, username, role) in [(1_i64, "admin", "admin"), (2, "alice", "user")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
                 VALUES (?, ?, 'x', ?, 0, 1, 1)",
            )
            .bind(id)
            .bind(username)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        }
        std::fs::create_dir_all(storage.home_dir("alice")).unwrap();
        let admin = auth_user(1, "admin");
        let mut alice = auth_user(2, "alice");
        alice.role = Role::User;
        let share = shares::create(
            &pool,
            &storage,
            &admin,
            shares::CreateShareInput {
                name: "team".into(),
                description: None,
                members: Some(vec![shares::MemberInput {
                    user_id: 2,
                    perms: Some("write".into()),
                }]),
            },
        )
        .await
        .unwrap();
        // Alice writes a file into the share and deletes it.
        std::fs::write(
            storage.shared_container_dir(&share.name).join("f.txt"),
            b"data",
        )
        .unwrap();
        let resolved = storage
            .resolve(&pool, &alice, "/shared/team/f.txt", Access::Write)
            .await
            .unwrap();
        let row = move_to_trash(&pool, &storage, &alice, &resolved)
            .await
            .unwrap();
        // Demote Alice to read-only and try to restore.
        shares::update(
            &pool,
            &storage,
            &share.id,
            shares::UpdateShareInput {
                name: None,
                description: None,
                members: Some(vec![shares::MemberInput {
                    user_id: 2,
                    perms: Some("read".into()),
                }]),
            },
        )
        .await
        .unwrap();
        let error = restore(&pool, &storage, &alice, &[row.id])
            .await
            .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
