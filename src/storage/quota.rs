//! Quota accounting.
//!
//! Quotas are enforced at the application level because the storage root can
//! be shared with other data and because SQLite gives us a single writer. The
//! invariant is:
//!
//! ```text
//! quota == 0  -> unlimited (also true for admins, so an admin can never lock
//!                themselves out of their own home)
//! reserved    -> bytes currently in flight (upload/copy/restore)
//! used        -> bytes of committed files in the user's home
//! ```
//!
//! `reserve` performs the quota check and the reservation in **one atomic
//! UPDATE**, so racy concurrent uploads cannot overshoot the quota. Callers
//! must always pair a successful reservation with exactly one [`commit`] or
//! [`rollback`].

use std::path::Path;

use sqlx::SqlitePool;

use crate::{
    db,
    error::{AppError, AppResult},
    storage::{safe_path, Storage, TEMP_PREFIX},
};

/// Current quota state for a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaInfo {
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub reserved_bytes: i64,
}

impl QuotaInfo {
    pub fn is_unlimited(&self) -> bool {
        self.quota_bytes <= 0
    }

    /// Remaining bytes including in-flight reservations. Unlimited quotas
    /// report 0 for the frontend (which treats `quotaBytes == 0` as unlimited).
    pub fn free_bytes(&self) -> i64 {
        if self.is_unlimited() {
            0
        } else {
            (self.quota_bytes - self.used_bytes - self.reserved_bytes).max(0)
        }
    }
}

/// Ensure the `storage_usage` row exists (older databases / CLI-created users).
async fn ensure_row(pool: &SqlitePool, user_id: i64) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO storage_usage (user_id, used_bytes, reserved_bytes, updated_at)
         VALUES (?, 0, 0, ?)
         ON CONFLICT(user_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(db::now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the quota state, reporting zeros for users without a usage row.
pub async fn info(pool: &SqlitePool, user_id: i64) -> AppResult<QuotaInfo> {
    let row = sqlx::query(
        "SELECT u.quota_bytes AS quota_bytes,
                COALESCE(s.used_bytes, 0) AS used_bytes,
                COALESCE(s.reserved_bytes, 0) AS reserved_bytes
         FROM users u LEFT JOIN storage_usage s ON s.user_id = u.id
         WHERE u.id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound("user not found"))?;
    use sqlx::Row;
    Ok(QuotaInfo {
        quota_bytes: row.get("quota_bytes"),
        used_bytes: row.get("used_bytes"),
        reserved_bytes: row.get("reserved_bytes"),
    })
}

/// Atomically reserve `bytes` and report whether the quota allows it.
///
/// On `Ok(false)` nothing was reserved. A successful reservation must be
/// released with [`commit`] (bytes were written) or [`rollback`] (aborted).
pub async fn reserve(pool: &SqlitePool, user_id: i64, bytes: i64) -> AppResult<bool> {
    let bytes = bytes.max(0);
    ensure_row(pool, user_id).await?;
    if bytes == 0 {
        return Ok(true);
    }
    let result = sqlx::query(
        "UPDATE storage_usage
         SET reserved_bytes = reserved_bytes + ?, updated_at = ?
         WHERE user_id = ?
           AND (
             (SELECT quota_bytes FROM users WHERE id = ?) <= 0
             OR (SELECT role FROM users WHERE id = ?) = 'admin'
             OR used_bytes + reserved_bytes + ? <= (SELECT quota_bytes FROM users WHERE id = ?)
           )",
    )
    .bind(bytes)
    .bind(db::now_ms())
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .bind(bytes)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Commit a reservation after the bytes were durably written.
///
/// `reserved` is the amount originally reserved; `used_delta` is added to
/// `used_bytes` (negative values are allowed, e.g. replacing a file with a
/// smaller one, and are clamped at zero).
pub async fn commit(
    pool: &SqlitePool,
    user_id: i64,
    reserved: i64,
    used_delta: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE storage_usage
         SET used_bytes = MAX(0, used_bytes + ?),
             reserved_bytes = MAX(0, reserved_bytes - ?),
             updated_at = ?
         WHERE user_id = ?",
    )
    .bind(used_delta.max(i64::MIN / 2))
    .bind(reserved.max(0))
    .bind(db::now_ms())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Release a reservation without writing anything.
pub async fn rollback(pool: &SqlitePool, user_id: i64, reserved: i64) -> AppResult<()> {
    if reserved <= 0 {
        return Ok(());
    }
    sqlx::query(
        "UPDATE storage_usage
         SET reserved_bytes = MAX(0, reserved_bytes - ?), updated_at = ?
         WHERE user_id = ?",
    )
    .bind(reserved)
    .bind(db::now_ms())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Adjust the committed usage directly (e.g. after moving a file out of the
/// home directory). Clamped at zero.
pub async fn adjust_used(pool: &SqlitePool, user_id: i64, delta: i64) -> AppResult<()> {
    if delta == 0 {
        return Ok(());
    }
    ensure_row(pool, user_id).await?;
    sqlx::query(
        "UPDATE storage_usage SET used_bytes = MAX(0, used_bytes + ?), updated_at = ?
         WHERE user_id = ?",
    )
    .bind(delta)
    .bind(db::now_ms())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Total bytes used below a directory, ignoring symlinks and in-flight
/// temporary files. Blocking; use inside `spawn_blocking`.
pub fn dir_used_bytes_blocking(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let walker = walkdir::WalkDir::new(path)
        .follow_links(false)
        .max_depth(64)
        .into_iter();
    for entry in walker.filter_entry(|e| !e.file_type().is_symlink()) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(TEMP_PREFIX))
        {
            continue;
        }
        total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
    }
    total
}

/// Recompute `used_bytes` from the filesystem (doctor / maintenance helper).
pub async fn recalculate(
    pool: &SqlitePool,
    storage: &Storage,
    user_id: i64,
    username: &str,
) -> AppResult<i64> {
    safe_path::validate_component(&username.to_ascii_lowercase())
        .map_err(|_| AppError::internal("invalid user home directory"))?;
    let home = storage.home_dir(username);
    let used = tokio::task::spawn_blocking(move || dir_used_bytes_blocking(&home))
        .await
        .map_err(AppError::from)?;
    let used = i64::try_from(used).unwrap_or(i64::MAX);
    ensure_row(pool, user_id).await?;
    sqlx::query("UPDATE storage_usage SET used_bytes = ?, updated_at = ? WHERE user_id = ?")
        .bind(used)
        .bind(db::now_ms())
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(used)
}

/// Recalculate usage for every user (doctor / maintenance helper).
pub async fn recalculate_all(pool: &SqlitePool, storage: &Storage) -> AppResult<u64> {
    use sqlx::Row;
    let rows = sqlx::query("SELECT id, username FROM users")
        .fetch_all(pool)
        .await?;
    let mut count = 0;
    for row in rows {
        let id: i64 = row.get("id");
        let username: String = row.get("username");
        if recalculate(pool, storage, id, &username).await.is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool_with_users() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("q.db"), 8).await.unwrap();
        for (id, role, quota) in [(1_i64, "user", 1000_i64), (2, "user", 0), (3, "admin", 100)] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, quota_bytes, created_at, updated_at)
                 VALUES (?, ?, 'x', ?, ?, 1, 1)",
            )
            .bind(id)
            .bind(format!("user{id}"))
            .bind(role)
            .bind(quota)
            .execute(&pool)
            .await
            .unwrap();
        }
        (dir, pool)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_never_overshoot() {
        let (_d, pool) = pool_with_users().await;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let pool = pool.clone();
            set.spawn(async move { reserve(&pool, 1, 200).await.unwrap() });
        }
        let mut granted = 0;
        while let Some(result) = set.join_next().await {
            if result.unwrap() {
                granted += 1;
            }
        }
        assert_eq!(granted, 5, "1000/200 = exactly 5 reservations may succeed");
        let info = info(&pool, 1).await.unwrap();
        assert_eq!(info.reserved_bytes, 1000);
        assert_eq!(info.used_bytes, 0);
        assert_eq!(info.free_bytes(), 0);

        rollback(&pool, 1, 1000).await.unwrap();
        assert!(reserve(&pool, 1, 1000).await.unwrap());
        rollback(&pool, 1, 1000).await.unwrap();
    }

    #[tokio::test]
    async fn commit_and_rollback_track_usage() {
        let (_d, pool) = pool_with_users().await;
        assert!(reserve(&pool, 1, 300).await.unwrap());
        commit(&pool, 1, 300, 300).await.unwrap();
        let state = info(&pool, 1).await.unwrap();
        assert_eq!(state.used_bytes, 300);
        assert_eq!(state.reserved_bytes, 0);

        adjust_used(&pool, 1, -100).await.unwrap();
        assert_eq!(info(&pool, 1).await.unwrap().used_bytes, 200);
        adjust_used(&pool, 1, -5000).await.unwrap();
        assert_eq!(info(&pool, 1).await.unwrap().used_bytes, 0);

        assert!(reserve(&pool, 1, 400).await.unwrap());
        rollback(&pool, 1, 400).await.unwrap();
        assert_eq!(info(&pool, 1).await.unwrap().reserved_bytes, 0);
    }

    #[tokio::test]
    async fn zero_quota_and_admins_are_unlimited() {
        let (_d, pool) = pool_with_users().await;
        assert!(
            reserve(&pool, 2, 10_000_000).await.unwrap(),
            "0 = unlimited"
        );
        assert!(reserve(&pool, 3, 10_000_000).await.unwrap(), "admin bypass");
        // A regular user above quota is rejected.
        assert!(!reserve(&pool, 1, 1001).await.unwrap());
        // And an existing usage row is not required.
        sqlx::query("DELETE FROM storage_usage WHERE user_id = 1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(reserve(&pool, 1, 10).await.unwrap());
    }

    #[tokio::test]
    async fn recalculates_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::storage::tests::test_config(dir.path());
        let storage = Storage::new(&cfg).unwrap();
        let (_d, pool) = pool_with_users().await;
        let home = storage.home_dir("user1");
        std::fs::create_dir_all(home.join("docs")).unwrap();
        std::fs::write(home.join("docs/a.bin"), vec![0u8; 1234]).unwrap();
        std::fs::write(home.join(format!("{TEMP_PREFIX}x")), vec![0u8; 99]).unwrap();
        let used = recalculate(&pool, &storage, 1, "user1").await.unwrap();
        assert_eq!(used, 1234, "temp files must not be counted");
        assert_eq!(info(&pool, 1).await.unwrap().used_bytes, 1234);
        let count = recalculate_all(&pool, &storage).await.unwrap();
        assert_eq!(count, 3);
    }
}
