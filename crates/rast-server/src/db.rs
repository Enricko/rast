//! SQLite storage: accounts, sessions and an audit trail.

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::Role;

const SCHEMA_V1: &str = "
CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    display_name  TEXT    NOT NULL DEFAULT '',
    role          TEXT    NOT NULL CHECK (role IN ('superadmin', 'admin', 'user')),
    password_hash TEXT    NOT NULL,
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_login_at INTEGER
);

-- Only the SHA-256 of each session token is stored.
CREATE TABLE sessions (
    token_hash TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);

-- Who changed which account, and sign-ins.
CREATE TABLE audit_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,
    actor_id INTEGER,
    action   TEXT    NOT NULL,
    detail   TEXT    NOT NULL DEFAULT ''
);

PRAGMA user_version = 1;
";

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub(crate) async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// One connection: every connection to `:memory:` is its own database.
pub(crate) async fn open_memory() -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version < 1 {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(SCHEMA_V1).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct User {
    pub(crate) id: i64,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) role: Role,
    pub(crate) password_hash: String,
    pub(crate) disabled: bool,
    pub(crate) created_at: i64,
    pub(crate) last_login_at: Option<i64>,
}

/// The account columns, as a literal so every query stays a static string.
macro_rules! user_columns {
    () => {
        "id, username, display_name, role, password_hash, disabled, created_at, last_login_at"
    };
}

fn user_from(row: &SqliteRow) -> User {
    User {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        role: Role::parse(row.get::<&str, _>("role")).unwrap_or(Role::User),
        password_hash: row.get("password_hash"),
        disabled: row.get::<i64, _>("disabled") != 0,
        created_at: row.get("created_at"),
        last_login_at: row.get("last_login_at"),
    }
}

pub(crate) async fn user_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
}

pub(crate) async fn users(pool: &SqlitePool) -> Result<Vec<User>, sqlx::Error> {
    let rows = sqlx::query(concat!("SELECT ", user_columns!(), " FROM users"))
        .fetch_all(pool)
        .await?;
    let mut users: Vec<User> = rows.iter().map(user_from).collect();
    users.sort_by(|a, b| {
        (a.role.rank(), a.username.to_lowercase()).cmp(&(b.role.rank(), b.username.to_lowercase()))
    });
    Ok(users)
}

pub(crate) async fn user_by_id(pool: &SqlitePool, id: i64) -> Result<Option<User>, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(user_from))
}

pub(crate) async fn user_by_name(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<User>, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE username = ?"
    ))
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(user_from))
}

/// Active Super Admins other than `except`.
pub(crate) async fn other_super_admins(pool: &SqlitePool, except: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE role = 'superadmin' AND disabled = 0 AND id != ?",
    )
    .bind(except)
    .fetch_one(pool)
    .await
}

pub(crate) struct NewUser<'a> {
    pub(crate) username: &'a str,
    pub(crate) display_name: &'a str,
    pub(crate) role: Role,
    pub(crate) password_hash: &'a str,
}

/// Insert an account. `Ok(None)` when the username is taken.
pub(crate) async fn insert_user(
    pool: &SqlitePool,
    user: NewUser<'_>,
) -> Result<Option<i64>, sqlx::Error> {
    let at = now();
    let result = sqlx::query(
        "INSERT INTO users (username, display_name, role, password_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user.username)
    .bind(user.display_name)
    .bind(user.role.as_str())
    .bind(user.password_hash)
    .bind(at)
    .bind(at)
    .execute(pool)
    .await;
    match result {
        Ok(done) => Ok(Some(done.last_insert_rowid())),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Insert the first account only while there are none (two setups cannot race).
pub(crate) async fn insert_first_user(
    pool: &SqlitePool,
    user: NewUser<'_>,
) -> Result<Option<i64>, sqlx::Error> {
    let at = now();
    let done = sqlx::query(
        "INSERT INTO users (username, display_name, role, password_hash, created_at, updated_at)
         SELECT ?, ?, ?, ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM users)",
    )
    .bind(user.username)
    .bind(user.display_name)
    .bind(user.role.as_str())
    .bind(user.password_hash)
    .bind(at)
    .bind(at)
    .execute(pool)
    .await?;
    Ok((done.rows_affected() == 1).then(|| done.last_insert_rowid()))
}

#[derive(Default)]
pub(crate) struct UserChange {
    pub(crate) display_name: Option<String>,
    pub(crate) role: Option<Role>,
    pub(crate) disabled: Option<bool>,
    pub(crate) password_hash: Option<String>,
}

pub(crate) async fn update_user(
    pool: &SqlitePool,
    id: i64,
    change: &UserChange,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET
            display_name  = COALESCE(?, display_name),
            role          = COALESCE(?, role),
            disabled      = COALESCE(?, disabled),
            password_hash = COALESCE(?, password_hash),
            updated_at    = ?
         WHERE id = ?",
    )
    .bind(change.display_name.as_deref())
    .bind(change.role.map(Role::as_str))
    .bind(change.disabled.map(i64::from))
    .bind(change.password_hash.as_deref())
    .bind(now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn delete_user(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn touch_login(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
        .bind(now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn insert_session(
    pool: &SqlitePool,
    token_hash: &str,
    user_id: i64,
) -> Result<(), sqlx::Error> {
    let at = now();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(at)
    .bind(at + crate::SESSION_SECS)
    .execute(pool)
    .await?;
    Ok(())
}

/// The enabled account that owns an unexpired session.
pub(crate) async fn session_user(
    pool: &SqlitePool,
    token_hash: &str,
) -> Result<Option<User>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.password_hash, u.disabled,
                u.created_at, u.last_login_at
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > ? AND u.disabled = 0",
    )
    .bind(token_hash)
    .bind(now())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(user_from))
}

pub(crate) async fn delete_session(pool: &SqlitePool, token_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sign an account out everywhere, except the session `keep` if given.
pub(crate) async fn delete_sessions_of(
    pool: &SqlitePool,
    user_id: i64,
    keep: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?")
        .bind(user_id)
        .bind(keep.unwrap_or(""))
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn prune_sessions(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn audit(pool: &SqlitePool, actor: Option<i64>, action: &str, detail: &str) {
    let result =
        sqlx::query("INSERT INTO audit_log (at, actor_id, action, detail) VALUES (?, ?, ?, ?)")
            .bind(now())
            .bind(actor)
            .bind(action)
            .bind(detail)
            .execute(pool)
            .await;
    if let Err(error) = result {
        eprintln!("rast-server: could not write the audit log: {error}");
    }
}
