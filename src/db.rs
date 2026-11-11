//! Database bootstrap and small shared helpers.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, SqlitePool,
};

use crate::{config::Config, error::AppResult};

/// Migration set compiled into the binary.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Current Unix epoch in milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Current Unix epoch in seconds.
pub fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Open (and migrate) the database used by the server and CLI.
pub async fn init(cfg: &Config) -> AppResult<SqlitePool> {
    cfg.ensure_dirs().map_err(|e| {
        crate::error::AppError::internal(format!("cannot create data directories: {e}"))
    })?;
    let pool = connect(&cfg.db_path(), cfg.db_max_connections).await?;
    Ok(pool)
}

/// Connect to a SQLite database at `path`, creating it when missing, and apply
/// pending migrations. WAL is used because the database sits on local storage.
pub async fn connect(path: &Path, max_connections: u32) -> AppResult<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(10))
        .foreign_keys(true)
        .pragma("temp_store", "MEMORY")
        .pragma("wal_autocheckpoint", "1000");

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections.max(1))
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await
        .map_err(|e| {
            crate::error::AppError::internal(format!(
                "cannot open database {}: {e}",
                path.display()
            ))
        })?;

    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| crate::error::AppError::internal(format!("database migration failed: {e}")))?;
    Ok(pool)
}

/// Exclusive lock on `<data_dir>/rast.lock` held by a running server.
///
/// Offline maintenance (`rast restore`) must never replace the database under
/// a live server. Checking `systemctl is-active rast.service` is not enough:
/// it misses servers started manually or in containers and blocks restores of
/// unrelated data directories. The OS releases the lock automatically when the
/// process exits (including crashes), so it can never go stale.
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(cfg: &Config) -> PathBuf {
        cfg.data_dir.join("rast.lock")
    }

    /// Acquire the lock without blocking. `Ok(None)` means another process
    /// (normally a running server) holds it.
    pub fn try_acquire(cfg: &Config) -> std::io::Result<Option<Self>> {
        let path = Self::path(cfg);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    pub fn lock_path(&self) -> &Path {
        &self.path
    }
}

/// Read a setting from the `settings` table.
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|row| row.get::<String, _>("value"))
}

/// Write a setting (insert or update).
pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// True when at least one active admin account exists. The setup endpoints are
/// only available while this is false.
pub async fn has_admin(pool: &SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
        > 0
}

/// Total number of users (any role), used to gate first-run setup.
pub async fn user_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Force a WAL checkpoint; used before consistent backups and on shutdown.
pub async fn checkpoint(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete old rows from high-volume tables (retention policy).
pub async fn prune_old_rows(
    pool: &SqlitePool,
    audit_retention_days: i64,
    jobs_retention_days: i64,
) -> AppResult<()> {
    let now = now_ms();
    let audit_cutoff = now - audit_retention_days.max(1) * 86_400_000;
    let jobs_cutoff = now - jobs_retention_days.max(1) * 86_400_000;
    sqlx::query("DELETE FROM audit_logs WHERE created_at < ?")
        .bind(audit_cutoff)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM jobs WHERE finished_at IS NOT NULL AND finished_at < ?")
        .bind(jobs_cutoff)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM login_attempts WHERE created_at < ?")
        .bind(now - 7 * 86_400_000)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at < ? OR revoked_at IS NOT NULL")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// Migration version currently applied (for update/rollback compatibility checks).
pub async fn schema_version(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0)
}

/// Schema version recorded in the existing database at `path`, read
/// *without* running this binary's migrations.
///
/// `rast update` runs as the previous binary: after the new service starts,
/// the database may carry migrations this binary does not know, and opening
/// it with [`connect`] would fail ("migration N was previously applied but is
/// missing") although the update succeeded.
pub async fn recorded_schema_version(path: &Path) -> AppResult<i64> {
    use sqlx::{ConnectOptions, Connection};
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(10))
        .connect()
        .await
        .map_err(|e| {
            crate::error::AppError::internal(format!(
                "cannot open database {}: {e}",
                path.display()
            ))
        })?;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(&mut conn)
    .await?;
    let _ = conn.close().await;
    Ok(version)
}

/// Highest migration version compiled into this binary.
pub fn embedded_schema_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recorded_schema_version_reads_newer_databases_without_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("newer.db");
        let pool = connect(&path, 2).await.unwrap();
        // Simulate a newer binary that applied a migration this one lacks.
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
             VALUES (9999, 'future', CURRENT_TIMESTAMP, 1, x'00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        let error = connect(&path, 1).await.unwrap_err().to_string();
        assert!(error.contains("previously applied"), "{error}");
        assert_eq!(recorded_schema_version(&path).await.unwrap(), 9999);
    }

    #[tokio::test]
    async fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let pool = connect(&path, 4).await.unwrap();
        assert_eq!(schema_version(&pool).await, embedded_schema_version());
        // Reconnecting must not fail.
        let pool2 = connect(&path, 2).await.unwrap();
        assert_eq!(schema_version(&pool2).await, embedded_schema_version());
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for expected in [
            "users",
            "sessions",
            "audit_logs",
            "jobs",
            "apps",
            "backups",
            "shares",
            "trash",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}"
            );
        }
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("t.db"), 2).await.unwrap();
        assert!(get_setting(&pool, "missing").await.is_none());
        set_setting(&pool, "k", "v1").await.unwrap();
        set_setting(&pool, "k", "v2").await.unwrap();
        assert_eq!(get_setting(&pool, "k").await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn admins_detected() {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("t.db"), 2).await.unwrap();
        assert!(!has_admin(&pool).await);
        assert_eq!(user_count(&pool).await, 0);
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at, updated_at) VALUES ('a','x','admin',1,1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(has_admin(&pool).await);
        assert_eq!(user_count(&pool).await, 1);
    }
}
