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
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

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

/// Move the database (with its WAL files) and the master key of an older
/// release from the data directory into `<data_dir>/private` (ADR 0006).
/// The data directory stays traversable for native app identities and
/// package users, so files directly in it can be named, and hard-linked
/// into a folder they own with `fs.protected_hardlinks = 0`.
///
/// Needs the instance lock: no server has them open. Each file is first
/// hard-linked into `private`, then its old name is replaced by a link to
/// the new one in a single rename. Every file can therefore be opened under
/// its old name at every moment (a CLI started meanwhile, or an older
/// binary after a downgrade, finds the same file), and a move that stopped
/// half way resumes. A file at both places that is not the same file is a
/// conflict: nothing is moved and the caller must stop. Returns the names
/// moved.
pub fn move_into_private(cfg: &Config, _lock: &InstanceLock) -> std::io::Result<Vec<&'static str>> {
    use std::os::unix::fs::MetadataExt;
    cfg.ensure_dirs()?;
    let private = cfg.private_dir();
    let mut found = Vec::new();
    for name in crate::config::PRIVATE_FILES {
        let legacy = cfg.data_dir.join(name);
        let target = private.join(name);
        let Ok(meta) = legacy.symlink_metadata() else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        match target.symlink_metadata() {
            Ok(existing) if existing.dev() == meta.dev() && existing.ino() == meta.ino() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "both {} and {} exist and are different files (was an older RAST \
                         release or a restore run after the move?); keep the one you need \
                         in {} and move the other away, then start RAST again",
                        legacy.display(),
                        target.display(),
                        private.display()
                    ),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        found.push((*name, legacy, target));
    }
    let mut moved = Vec::new();
    for (name, legacy, target) in found {
        if target.symlink_metadata().is_err() {
            std::fs::hard_link(&legacy, &target)?;
        }
        let link = cfg.data_dir.join(format!(".{name}.private-link"));
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::os::unix::fs::symlink(format!("{}/{name}", crate::config::PRIVATE_DIR), &link)?;
        std::fs::rename(&link, &legacy)?;
        moved.push(name);
    }
    // The WAL files need no link once the database has one: SQLite
    // resolves the link and opens them next to the real file.
    for name in ["rast.db-wal", "rast.db-shm"] {
        let legacy = cfg.data_dir.join(name);
        if legacy
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            std::fs::remove_file(&legacy)?;
        }
    }
    cfg.link_legacy_names()?;
    if !moved.is_empty() {
        // The renames must survive a crash before the server starts.
        for dir in [&cfg.data_dir, &private] {
            if let Ok(dir) = std::fs::File::open(dir) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(moved)
}

/// Files of an older release left at their old place next to a file of
/// the same name in `private` (see [`move_into_private`]), for `rast
/// doctor`.
pub fn private_conflicts(cfg: &Config) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    crate::config::PRIVATE_FILES
        .iter()
        .filter_map(|name| {
            let legacy = cfg.data_dir.join(name);
            let meta = legacy.symlink_metadata().ok()?;
            let target = cfg.private_dir().join(name).symlink_metadata().ok()?;
            (meta.file_type().is_file() && (meta.dev(), meta.ino()) != (target.dev(), target.ino()))
                .then_some(legacy)
        })
        .collect()
}

/// What `rast doctor` says about the private data folder and hard-link
/// protection: `(lines, warnings, failures)`. `protected_hardlinks` is the
/// content of `/proc/sys/fs/protected_hardlinks` (`None`: unreadable).
pub fn private_report(
    cfg: &Config,
    protected_hardlinks: Option<&str>,
) -> (Vec<String>, usize, usize) {
    use std::os::unix::fs::PermissionsExt;
    let (mut lines, mut warnings, mut failures) = (Vec::new(), 0, 0);
    let private = cfg.private_dir();
    match std::fs::symlink_metadata(&private) {
        Ok(meta) if meta.is_dir() && meta.permissions().mode() & 0o077 == 0 => {
            lines.push(format!(
                "[    ] private data folder ... ok ({})",
                private.display()
            ));
        }
        Ok(meta) => {
            warnings += 1;
            lines.push(format!(
                "[WARN] {} must be a folder with mode 0700 (it is {:o}); every RAST command \
                 sets it",
                private.display(),
                meta.permissions().mode() & 0o7777
            ));
        }
        Err(error) => {
            warnings += 1;
            lines.push(format!(
                "[WARN] cannot inspect {}: {error}",
                private.display()
            ));
        }
    }
    let conflicts = private_conflicts(cfg);
    for path in &conflicts {
        failures += 1;
        lines.push(format!(
            "[FAIL] {} is a file of an older release and {} holds another file of that name: \
             the server does not start until you keep one of them",
            path.display(),
            private.display()
        ));
    }
    for name in crate::config::PRIVATE_FILES {
        let legacy = cfg.data_dir.join(name);
        let waiting = legacy
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_file());
        if waiting && !conflicts.contains(&legacy) {
            lines.push(format!(
                "[INFO] {} moves into {} at the next server start",
                legacy.display(),
                private.display()
            ));
        }
    }
    match protected_hardlinks.map(str::trim) {
        Some("1") => lines.push("[    ] fs.protected_hardlinks ... ok (1)".to_string()),
        Some(value) => {
            warnings += 1;
            lines.push(format!(
                "[WARN] fs.protected_hardlinks is {value}: any account can hard-link files it \
                 cannot read (of the rast service, of other accounts) into folders it owns, \
                 such as a shared folder; set it to 1 (`sysctl -w fs.protected_hardlinks=1` \
                 and a file in /etc/sysctl.d)"
            ));
        }
        None => {
            warnings += 1;
            lines.push(
                "[WARN] cannot read /proc/sys/fs/protected_hardlinks; it should be 1".to_string(),
            );
        }
    }
    (lines, warnings, failures)
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
    fn private_config(dir: &Path) -> Config {
        crate::storage::tests::test_config(dir)
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[tokio::test]
    async fn a_new_data_directory_keeps_its_secrets_in_the_private_folder() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = private_config(dir.path());
        let pool = init(&cfg).await.unwrap();
        crate::secrets::Secrets::load_or_create(&cfg.secret_path()).unwrap();
        let private = cfg.data_dir.join("private");
        assert_eq!(cfg.db_path(), private.join("rast.db"));
        assert_eq!(cfg.secret_path(), private.join("secret.key"));
        assert!(private.join("rast.db").is_file());
        assert_eq!(mode(&private), 0o700);
        assert_eq!(mode(&cfg.backup_dir()), 0o700);
        pool.close().await;
        // The old names are links into the folder (for a downgrade).
        cfg.ensure_dirs().unwrap();
        for name in ["rast.db", "secret.key"] {
            let link = cfg.data_dir.join(name);
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                PathBuf::from(format!("private/{name}"))
            );
        }
        let lock = InstanceLock::try_acquire(&cfg).unwrap().unwrap();
        assert!(move_into_private(&cfg, &lock).unwrap().is_empty());
        let (lines, warnings, failures) = private_report(&cfg, Some("1\n"));
        assert_eq!((warnings, failures), (0, 0), "{lines:?}");
        let (lines, warnings, _) = private_report(&cfg, Some("0\n"));
        assert_eq!(warnings, 1);
        assert!(lines
            .iter()
            .any(|line| line.contains("fs.protected_hardlinks is 0")));
        assert_eq!(private_report(&cfg, None).1, 1);
    }

    #[tokio::test]
    async fn files_of_an_older_release_move_into_the_private_folder() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = private_config(dir.path());
        // An older release: the database and the key directly in the data
        // directory, with a WAL file left by a crash.
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        let legacy_db = cfg.data_dir.join("rast.db");
        let pool = connect(&legacy_db, 1).await.unwrap();
        set_setting(&pool, "probe", "legacy").await.unwrap();
        pool.close().await;
        std::fs::write(cfg.data_dir.join("secret.key"), "k").unwrap();
        std::fs::write(cfg.data_dir.join("rast.db-shm"), "").unwrap();
        // Before the move every command keeps using them where they are.
        cfg.ensure_dirs().unwrap();
        assert_eq!(cfg.db_path(), legacy_db);
        let (lines, _, failures) = private_report(&cfg, Some("1"));
        assert_eq!(failures, 0);
        assert!(
            lines.iter().any(|line| line.contains("moves into")),
            "{lines:?}"
        );

        let lock = InstanceLock::try_acquire(&cfg).unwrap().unwrap();
        let moved = move_into_private(&cfg, &lock).unwrap();
        assert_eq!(moved, vec!["rast.db-shm", "rast.db", "secret.key"]);
        let private = cfg.private_dir();
        assert_eq!(cfg.db_path(), private.join("rast.db"));
        assert_eq!(std::fs::read(private.join("secret.key")).unwrap(), b"k");
        assert!(std::fs::symlink_metadata(cfg.data_dir.join("rast.db-shm")).is_err());
        assert!(std::fs::symlink_metadata(&legacy_db)
            .unwrap()
            .file_type()
            .is_symlink());
        // One link each: nothing else names them any more.
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(private.join("rast.db")).unwrap().nlink(),
            1
        );
        // Once moved, again is nothing to do.
        assert!(move_into_private(&cfg, &lock).unwrap().is_empty());
        drop(lock);

        // The database is the same, opened at its new place or through the
        // old name (an older release after a downgrade): SQLite follows the
        // link and keeps its WAL next to the real file.
        let pool = connect(&cfg.db_path(), 1).await.unwrap();
        assert_eq!(get_setting(&pool, "probe").await.as_deref(), Some("legacy"));
        pool.close().await;
        let old = connect(&legacy_db, 1).await.unwrap();
        set_setting(&old, "probe", "through the link")
            .await
            .unwrap();
        assert!(!cfg.data_dir.join("rast.db-wal").exists());
        old.close().await;
        let pool = connect(&cfg.db_path(), 1).await.unwrap();
        assert_eq!(
            get_setting(&pool, "probe").await.as_deref(),
            Some("through the link")
        );
        pool.close().await;
    }

    #[test]
    fn a_move_that_stopped_resumes_and_two_different_files_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = private_config(dir.path());
        cfg.ensure_dirs().unwrap();
        let private = cfg.private_dir();
        // Stopped after the hard link of the database.
        std::fs::write(cfg.data_dir.join("rast.db"), "db").unwrap();
        std::fs::hard_link(cfg.data_dir.join("rast.db"), private.join("rast.db")).unwrap();
        std::fs::write(cfg.data_dir.join(".rast.db.private-link"), "leftover").unwrap();
        let lock = InstanceLock::try_acquire(&cfg).unwrap().unwrap();
        assert_eq!(move_into_private(&cfg, &lock).unwrap(), vec!["rast.db"]);
        assert_eq!(std::fs::read(cfg.db_path()).unwrap(), b"db");
        assert!(!cfg.data_dir.join(".rast.db.private-link").exists());

        // A restore by an older release replaced the link by a new file:
        // which one is right is the administrator's call.
        std::fs::remove_file(cfg.data_dir.join("rast.db")).unwrap();
        std::fs::write(cfg.data_dir.join("rast.db"), "restored").unwrap();
        std::fs::write(cfg.data_dir.join("secret.key"), "k").unwrap();
        let error = move_into_private(&cfg, &lock).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("both"), "{error}");
        // Nothing moved, not even the key.
        assert!(!private.join("secret.key").exists());
        let (lines, _, failures) = private_report(&cfg, Some("1"));
        assert_eq!(failures, 1, "{lines:?}");
    }
}
