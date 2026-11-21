//! SQLite storage: accounts, sessions, groups with app rules, each account's
//! appearance, and an audit trail.
//!
//! Every connection runs with `foreign_keys`, `synchronous = FULL` (a committed
//! change survives a power cut), `secure_delete` (deleted rows such as ended
//! sessions and replaced password hashes are overwritten, not left in free
//! pages) and `trusted_schema = OFF` (a tampered schema cannot make queries
//! call functions with side effects).

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use crate::Role;
use crate::access::Rule;
use crate::appearance::{Appearance, Wallpaper};

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

/// Groups and app rules (classic RAST's `0004_groups`, apps only for now).
/// A missing rule means "inherit".
const SCHEMA_V2: &str = "
CREATE TABLE groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    description TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX idx_group_members_user ON group_members(user_id);

CREATE TABLE group_app_rules (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    app_id   TEXT    NOT NULL,
    rule     TEXT    NOT NULL CHECK (rule IN ('allow', 'deny')),
    PRIMARY KEY (group_id, app_id)
);

CREATE TABLE user_app_rules (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id  TEXT    NOT NULL,
    rule    TEXT    NOT NULL CHECK (rule IN ('allow', 'deny')),
    PRIMARY KEY (user_id, app_id)
);

PRAGMA user_version = 2;
";

/// Classic RAST's account protection and session list: a password change
/// forced at the next sign-in, a lockout after repeated failures, and where
/// each session signed in from.
const SCHEMA_V3: &str = "
ALTER TABLE users ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN failed_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN failed_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN locked_until INTEGER NOT NULL DEFAULT 0;

-- A public id to name a session by; the token's hash never leaves the server.
ALTER TABLE sessions ADD COLUMN id TEXT;
UPDATE sessions SET id = lower(hex(randomblob(16)));
CREATE UNIQUE INDEX idx_sessions_id ON sessions(id);
ALTER TABLE sessions ADD COLUMN last_seen_at INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET last_seen_at = created_at;
ALTER TABLE sessions ADD COLUMN ip TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN user_agent TEXT NOT NULL DEFAULT '';

ALTER TABLE audit_log ADD COLUMN ip TEXT NOT NULL DEFAULT '';

PRAGMA user_version = 3;
";

/// Classic RAST's *Personalize* (its `0002_appearance` and `0003_appearance_accent`,
/// without uploaded wallpapers yet): each account's colour mode, accent and wallpaper.
const SCHEMA_V4: &str = "
CREATE TABLE user_appearance (
    user_id        INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    theme          TEXT    NOT NULL DEFAULT 'light',
    accent         TEXT    NOT NULL DEFAULT 'ocean',
    wallpaper_kind TEXT    NOT NULL DEFAULT 'builtin',
    builtin_id     TEXT    NOT NULL DEFAULT 'alpine-dawn',
    color          TEXT    NOT NULL DEFAULT '#1B2430',
    fit            TEXT    NOT NULL DEFAULT 'cover',
    position       TEXT    NOT NULL DEFAULT 'center',
    overlay        INTEGER NOT NULL DEFAULT 20 CHECK (overlay BETWEEN 0 AND 60),
    updated_at     INTEGER NOT NULL
);

PRAGMA user_version = 4;
";

/// Wrong passwords within this many seconds count towards a lockout.
pub(crate) const FAILURE_WINDOW: i64 = 30 * 60;
/// Failed sign-ins in a row that lock an account.
pub(crate) const LOCK_AFTER: i64 = 10;
/// How long a lockout lasts, unless an administrator unlocks the account.
pub(crate) const LOCK_SECS: i64 = 15 * 60;

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The settings every connection runs with (see the module docs).
fn hardened(options: SqliteConnectOptions) -> SqliteConnectOptions {
    options
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Full)
        .pragma("secure_delete", "ON")
        .pragma("trusted_schema", "OFF")
}

pub(crate) async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = hardened(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5)),
    );
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// One connection: every connection to `:memory:` is its own database.
pub(crate) async fn open_memory() -> Result<SqlitePool, sqlx::Error> {
    let options = hardened(SqliteConnectOptions::from_str("sqlite::memory:")?);
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
    for (target, schema) in [(2, SCHEMA_V2), (3, SCHEMA_V3), (4, SCHEMA_V4)] {
        if version < target {
            let mut tx = pool.begin().await?;
            sqlx::raw_sql(schema).execute(&mut *tx).await?;
            tx.commit().await?;
        }
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
    /// An administrator set the password: choose a new one before anything else.
    pub(crate) must_change_password: bool,
    /// Wrong passwords since the last sign-in, within [`FAILURE_WINDOW`].
    pub(crate) failed_attempts: i64,
    pub(crate) failed_at: i64,
    /// Sign-in is refused until then (Unix seconds); 0 when not locked.
    pub(crate) locked_until: i64,
}

impl User {
    /// Locked out at `now`, until the returned time.
    pub(crate) fn locked_at(&self, now: i64) -> Option<i64> {
        (self.locked_until > now).then_some(self.locked_until)
    }

    /// Recent wrong passwords, forgetting those older than the window.
    pub(crate) fn recent_failures(&self, now: i64) -> i64 {
        if now - self.failed_at < FAILURE_WINDOW {
            self.failed_attempts
        } else {
            0
        }
    }
}

/// The account columns, as a literal so every query stays a static string.
/// `u.` because session lookups join `users u` with `sessions`.
macro_rules! user_columns {
    () => {
        "u.id, u.username, u.display_name, u.role, u.password_hash, u.disabled, u.created_at,
         u.last_login_at, u.must_change_password, u.failed_attempts, u.failed_at, u.locked_until"
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
        must_change_password: row.get::<i64, _>("must_change_password") != 0,
        failed_attempts: row.get("failed_attempts"),
        failed_at: row.get("failed_at"),
        locked_until: row.get("locked_until"),
    }
}

pub(crate) async fn user_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
}

pub(crate) async fn users(pool: &SqlitePool) -> Result<Vec<User>, sqlx::Error> {
    let rows = sqlx::query(concat!("SELECT ", user_columns!(), " FROM users u"))
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
        " FROM users u WHERE u.id = ?"
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
        " FROM users u WHERE u.username = ?"
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
    pub(crate) must_change_password: bool,
}

/// Insert an account. `Ok(None)` when the username is taken.
pub(crate) async fn insert_user(
    pool: &SqlitePool,
    user: NewUser<'_>,
) -> Result<Option<i64>, sqlx::Error> {
    let at = now();
    let result = sqlx::query(
        "INSERT INTO users (username, display_name, role, password_hash, must_change_password,
                            created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.username)
    .bind(user.display_name)
    .bind(user.role.as_str())
    .bind(user.password_hash)
    .bind(i64::from(user.must_change_password))
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
    pub(crate) must_change_password: Option<bool>,
    /// Forget failed sign-ins and lift a lockout.
    pub(crate) unlock: bool,
}

pub(crate) async fn update_user(
    pool: &SqlitePool,
    id: i64,
    change: &UserChange,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET
            display_name         = COALESCE(?, display_name),
            role                 = COALESCE(?, role),
            disabled             = COALESCE(?, disabled),
            password_hash        = COALESCE(?, password_hash),
            must_change_password = COALESCE(?, must_change_password),
            failed_attempts      = CASE WHEN ? THEN 0 ELSE failed_attempts END,
            failed_at            = CASE WHEN ? THEN 0 ELSE failed_at END,
            locked_until         = CASE WHEN ? THEN 0 ELSE locked_until END,
            updated_at           = ?
         WHERE id = ?",
    )
    .bind(change.display_name.as_deref())
    .bind(change.role.map(Role::as_str))
    .bind(change.disabled.map(i64::from))
    .bind(change.password_hash.as_deref())
    .bind(change.must_change_password.map(i64::from))
    .bind(change.unlock)
    .bind(change.unlock)
    .bind(change.unlock)
    .bind(now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// An account's saved appearance; `None` until it saves one.
pub(crate) async fn appearance_of(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<Appearance>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT theme, accent, wallpaper_kind, builtin_id, color, fit, position, overlay
         FROM user_appearance WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| {
        Appearance {
            theme: row.get("theme"),
            accent: row.get("accent"),
            wallpaper: Wallpaper {
                kind: row.get("wallpaper_kind"),
                builtin_id: row.get("builtin_id"),
                color: row.get("color"),
                fit: row.get("fit"),
                position: row.get("position"),
                overlay: row.get("overlay"),
            },
        }
        .sanitized()
    }))
}

pub(crate) async fn save_appearance(
    pool: &SqlitePool,
    user_id: i64,
    appearance: &Appearance,
) -> Result<(), sqlx::Error> {
    let wallpaper = &appearance.wallpaper;
    sqlx::query(
        "INSERT INTO user_appearance
            (user_id, theme, accent, wallpaper_kind, builtin_id, color, fit, position, overlay,
             updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (user_id) DO UPDATE SET
            theme = excluded.theme,
            accent = excluded.accent,
            wallpaper_kind = excluded.wallpaper_kind,
            builtin_id = excluded.builtin_id,
            color = excluded.color,
            fit = excluded.fit,
            position = excluded.position,
            overlay = excluded.overlay,
            updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(&appearance.theme)
    .bind(&appearance.accent)
    .bind(&wallpaper.kind)
    .bind(&wallpaper.builtin_id)
    .bind(&wallpaper.color)
    .bind(&wallpaper.fit)
    .bind(&wallpaper.position)
    .bind(wallpaper.overlay)
    .bind(now())
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

/// A successful sign-in: remember when, and forget earlier wrong passwords.
pub(crate) async fn touch_login(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET last_login_at = ?, failed_attempts = 0, failed_at = 0, locked_until = 0
         WHERE id = ?",
    )
    .bind(now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count a wrong password for account `id`. After [`LOCK_AFTER`] of them within
/// [`FAILURE_WINDOW`] the account locks for [`LOCK_SECS`]; returns the time it
/// is locked until when this failure locked it.
pub(crate) async fn record_failure(pool: &SqlitePool, id: i64) -> Result<Option<i64>, sqlx::Error> {
    let at = now();
    let mut tx = pool.begin().await?;
    let attempts: i64 = sqlx::query_scalar(
        "UPDATE users SET
            failed_attempts = CASE WHEN failed_at > ? THEN failed_attempts + 1 ELSE 1 END,
            failed_at = ?
         WHERE id = ?
         RETURNING failed_attempts",
    )
    .bind(at - FAILURE_WINDOW)
    .bind(at)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let locked = (attempts >= LOCK_AFTER).then_some(at + LOCK_SECS);
    if let Some(until) = locked {
        // The count starts again once the lockout ends.
        sqlx::query("UPDATE users SET locked_until = ?, failed_attempts = 0 WHERE id = ?")
            .bind(until)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(locked)
}

/// Where a session signed in from.
pub(crate) struct Origin {
    pub(crate) ip: String,
    pub(crate) user_agent: String,
}

pub(crate) async fn insert_session(
    pool: &SqlitePool,
    id: &str,
    token_hash: &str,
    user_id: i64,
    origin: &Origin,
) -> Result<(), sqlx::Error> {
    let at = now();
    sqlx::query(
        "INSERT INTO sessions (id, token_hash, user_id, created_at, expires_at, last_seen_at,
                               ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(token_hash)
    .bind(user_id)
    .bind(at)
    .bind(at + crate::SESSION_SECS)
    .bind(at)
    .bind(&origin.ip)
    .bind(&origin.user_agent)
    .execute(pool)
    .await?;
    Ok(())
}

/// The enabled account that owns an unexpired session, and the session's id
/// and last activity.
pub(crate) async fn session_user(
    pool: &SqlitePool,
    token_hash: &str,
) -> Result<Option<(User, String, i64)>, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        ", s.id AS session_id, s.last_seen_at
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > ? AND u.disabled = 0"
    ))
    .bind(token_hash)
    .bind(now())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| {
        (
            user_from(&row),
            row.get("session_id"),
            row.get("last_seen_at"),
        )
    }))
}

/// Note that a session is in use; at most once a minute, to spare the disk.
pub(crate) async fn touch_session(pool: &SqlitePool, token_hash: &str) -> Result<(), sqlx::Error> {
    let at = now();
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token_hash = ? AND last_seen_at < ?")
        .bind(at)
        .bind(token_hash)
        .bind(at - 60)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) token_hash: String,
    pub(crate) created_at: i64,
    pub(crate) last_seen_at: i64,
    pub(crate) expires_at: i64,
    pub(crate) ip: String,
    pub(crate) user_agent: String,
}

/// An account's unexpired sessions, most recently used first.
pub(crate) async fn sessions_of(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<Session>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, token_hash, created_at, last_seen_at, expires_at, ip, user_agent
         FROM sessions WHERE user_id = ? AND expires_at > ?
         ORDER BY last_seen_at DESC, created_at DESC",
    )
    .bind(user_id)
    .bind(now())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| Session {
            id: row.get("id"),
            token_hash: row.get("token_hash"),
            created_at: row.get("created_at"),
            last_seen_at: row.get("last_seen_at"),
            expires_at: row.get("expires_at"),
            ip: row.get("ip"),
            user_agent: row.get("user_agent"),
        })
        .collect())
}

/// End one of `user_id`'s sessions by its public id. `false` when it has none
/// by that id.
pub(crate) async fn delete_session_by_id(
    pool: &SqlitePool,
    user_id: i64,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("DELETE FROM sessions WHERE user_id = ? AND id = ?")
        .bind(user_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
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

/// An audit entry with the client address it came from.
pub(crate) async fn audit_from(
    pool: &SqlitePool,
    actor: Option<i64>,
    action: &str,
    detail: &str,
    ip: &str,
) {
    let result = sqlx::query(
        "INSERT INTO audit_log (at, actor_id, action, detail, ip) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(now())
    .bind(actor)
    .bind(action)
    .bind(detail)
    .bind(ip)
    .execute(pool)
    .await;
    if let Err(error) = result {
        eprintln!("rast-server: could not write the audit log: {error}");
    }
}

// --- groups and app rules -----------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct Group {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) member_ids: Vec<i64>,
    pub(crate) apps: BTreeMap<String, Rule>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

fn rule_from(row: &SqliteRow) -> Option<Rule> {
    Rule::parse(row.get::<&str, _>("rule"))
}

/// Every group with its members and rules, by name.
pub(crate) async fn groups(pool: &SqlitePool) -> Result<Vec<Group>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, description, created_at, updated_at FROM groups ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    let mut groups: Vec<Group> = rows
        .iter()
        .map(|row| Group {
            id: row.get("id"),
            name: row.get("name"),
            description: row.get("description"),
            member_ids: Vec::new(),
            apps: BTreeMap::new(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect();
    let members = sqlx::query("SELECT group_id, user_id FROM group_members ORDER BY user_id")
        .fetch_all(pool)
        .await?;
    for row in &members {
        let id: i64 = row.get("group_id");
        if let Some(group) = groups.iter_mut().find(|g| g.id == id) {
            group.member_ids.push(row.get("user_id"));
        }
    }
    let rules = sqlx::query("SELECT group_id, app_id, rule FROM group_app_rules")
        .fetch_all(pool)
        .await?;
    for row in &rules {
        let id: i64 = row.get("group_id");
        if let (Some(group), Some(rule)) = (groups.iter_mut().find(|g| g.id == id), rule_from(row))
        {
            group.apps.insert(row.get("app_id"), rule);
        }
    }
    Ok(groups)
}

pub(crate) async fn group_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Group>, sqlx::Error> {
    Ok(groups(pool).await?.into_iter().find(|group| group.id == id))
}

/// Create a group. `Ok(None)` when the name is taken.
pub(crate) async fn insert_group(
    pool: &SqlitePool,
    name: &str,
    description: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let at = now();
    let result = sqlx::query(
        "INSERT INTO groups (name, description, created_at, updated_at) VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(description)
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

/// App rule changes: `None` removes the rule (inherit).
pub(crate) type RuleChanges = Vec<(String, Option<Rule>)>;

#[derive(Default)]
pub(crate) struct GroupChange {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    /// Replaces the members when given.
    pub(crate) members: Option<Vec<i64>>,
    pub(crate) apps: RuleChanges,
}

/// Apply `change` in one transaction. `Ok(false)` when the new name is taken.
pub(crate) async fn update_group(
    pool: &SqlitePool,
    id: i64,
    change: &GroupChange,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let renamed = sqlx::query(
        "UPDATE groups SET
            name        = COALESCE(?, name),
            description = COALESCE(?, description),
            updated_at  = ?
         WHERE id = ?",
    )
    .bind(change.name.as_deref())
    .bind(change.description.as_deref())
    .bind(now())
    .bind(id)
    .execute(&mut *tx)
    .await;
    match renamed {
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => return Ok(false),
        other => {
            other?;
        }
    }
    if let Some(members) = &change.members {
        let list = serde_json::to_string(members).unwrap_or_else(|_| "[]".to_owned());
        sqlx::query(
            "DELETE FROM group_members
             WHERE group_id = ? AND user_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(id)
        .bind(&list)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO group_members (group_id, user_id, added_at)
             SELECT ?, value, ? FROM json_each(?)",
        )
        .bind(id)
        .bind(now())
        .bind(&list)
        .execute(&mut *tx)
        .await?;
    }
    for (app, rule) in &change.apps {
        match rule {
            Some(rule) => {
                sqlx::query(
                    "INSERT INTO group_app_rules (group_id, app_id, rule) VALUES (?, ?, ?)
                     ON CONFLICT (group_id, app_id) DO UPDATE SET rule = excluded.rule",
                )
                .bind(id)
                .bind(app)
                .bind(rule.as_str())
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM group_app_rules WHERE group_id = ? AND app_id = ?")
                    .bind(id)
                    .bind(app)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn delete_group(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM groups WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The groups an account belongs to, by name.
pub(crate) async fn user_groups(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT g.id, g.name FROM group_members m JOIN groups g ON g.id = m.group_id
         WHERE m.user_id = ? ORDER BY g.name COLLATE NOCASE",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get("id"), row.get("name")))
        .collect())
}

/// Every account's group names, by account id.
pub(crate) async fn group_names_by_user(
    pool: &SqlitePool,
) -> Result<BTreeMap<i64, Vec<String>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT m.user_id, g.name FROM group_members m JOIN groups g ON g.id = m.group_id
         ORDER BY g.name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    let mut names: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for row in &rows {
        names
            .entry(row.get("user_id"))
            .or_default()
            .push(row.get("name"));
    }
    Ok(names)
}

/// The rules on the account itself.
pub(crate) async fn user_rules(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<BTreeMap<String, Rule>, sqlx::Error> {
    let rows = sqlx::query("SELECT app_id, rule FROM user_app_rules WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| Some((row.get("app_id"), rule_from(row)?)))
        .collect())
}

pub(crate) struct GroupRule {
    pub(crate) group: String,
    pub(crate) app_id: String,
    pub(crate) rule: Rule,
}

/// The app rules of every group the account belongs to.
pub(crate) async fn group_rules_for_user(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<GroupRule>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT g.name, r.app_id, r.rule
         FROM group_members m
         JOIN groups g ON g.id = m.group_id
         JOIN group_app_rules r ON r.group_id = g.id
         WHERE m.user_id = ? ORDER BY g.name COLLATE NOCASE",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(GroupRule {
                group: row.get("name"),
                app_id: row.get("app_id"),
                rule: rule_from(row)?,
            })
        })
        .collect())
}

/// Set an account's groups (replaced when given) and change its own rules, in
/// one transaction.
pub(crate) async fn update_user_access(
    pool: &SqlitePool,
    user_id: i64,
    groups: Option<&[i64]>,
    apps: &RuleChanges,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(groups) = groups {
        let list = serde_json::to_string(groups).unwrap_or_else(|_| "[]".to_owned());
        sqlx::query(
            "DELETE FROM group_members
             WHERE user_id = ? AND group_id NOT IN (SELECT value FROM json_each(?))",
        )
        .bind(user_id)
        .bind(&list)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO group_members (group_id, user_id, added_at)
             SELECT value, ?, ? FROM json_each(?)",
        )
        .bind(user_id)
        .bind(now())
        .bind(&list)
        .execute(&mut *tx)
        .await?;
    }
    for (app, rule) in apps {
        match rule {
            Some(rule) => {
                sqlx::query(
                    "INSERT INTO user_app_rules (user_id, app_id, rule) VALUES (?, ?, ?)
                     ON CONFLICT (user_id, app_id) DO UPDATE SET rule = excluded.rule",
                )
                .bind(user_id)
                .bind(app)
                .bind(rule.as_str())
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM user_app_rules WHERE user_id = ? AND app_id = ?")
                    .bind(user_id)
                    .bind(app)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_version_1_database_keeps_its_accounts_when_groups_arrive() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::raw_sql(SCHEMA_V1).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, role, password_hash, created_at, updated_at)
             VALUES ('root', 'superadmin', 'x', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        migrate(&pool).await.unwrap();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, 4);
        assert_eq!(user_count(&pool).await.unwrap(), 1);
        let id = insert_group(&pool, "Staff", "").await.unwrap().unwrap();
        let change = GroupChange {
            members: Some(vec![1]),
            apps: vec![("files".to_owned(), Some(Rule::Deny))],
            ..GroupChange::default()
        };
        assert!(update_group(&pool, id, &change).await.unwrap());
        let rules = group_rules_for_user(&pool, 1).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(
            (rules[0].group.as_str(), rules[0].rule),
            ("Staff", Rule::Deny)
        );

        migrate(&pool).await.unwrap();
        assert_eq!(
            groups(&pool).await.unwrap().len(),
            1,
            "migrating twice is a no-op"
        );
    }

    #[tokio::test]
    async fn sessions_signed_in_before_version_3_stay_signed_in() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::raw_sql(SCHEMA_V1).execute(&pool).await.unwrap();
        sqlx::raw_sql(SCHEMA_V2).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, role, password_hash, created_at, updated_at)
             VALUES ('root', 'superadmin', 'x', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for hash in ["aa", "bb"] {
            sqlx::query(
                "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
                 VALUES (?, 1, 50, 9999999999)",
            )
            .bind(hash)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrate(&pool).await.unwrap();
        let (user, id, last_seen) = session_user(&pool, "aa").await.unwrap().unwrap();
        assert_eq!(user.username, "root");
        assert!(!user.must_change_password && user.locked_until == 0);
        assert_eq!(id.len(), 32, "a public id was assigned");
        assert_eq!(last_seen, 50, "last seen starts at sign-in");
        let sessions = sessions_of(&pool, 1).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_ne!(sessions[0].id, sessions[1].id, "ids are distinct");
    }

    #[tokio::test]
    async fn ten_wrong_passwords_lock_an_account_and_a_sign_in_forgets_them() {
        let pool = open_memory().await.unwrap();
        let id = insert_user(
            &pool,
            NewUser {
                username: "bob",
                display_name: "",
                role: Role::User,
                password_hash: "x",
                must_change_password: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
        for _ in 1..LOCK_AFTER {
            assert_eq!(record_failure(&pool, id).await.unwrap(), None);
        }
        let bob = user_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(bob.recent_failures(now()), LOCK_AFTER - 1);
        let until = record_failure(&pool, id).await.unwrap().expect("locks");
        let bob = user_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(bob.locked_at(now()), Some(until));
        assert!(until - now() > LOCK_SECS - 5);

        let unlock = UserChange {
            unlock: true,
            ..UserChange::default()
        };
        update_user(&pool, id, &unlock).await.unwrap();
        let bob = user_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!((bob.locked_at(now()), bob.failed_attempts), (None, 0));

        record_failure(&pool, id).await.unwrap();
        touch_login(&pool, id).await.unwrap();
        let bob = user_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(bob.recent_failures(now()), 0, "a sign-in forgets failures");
    }

    #[tokio::test]
    async fn appearance_is_kept_per_account_and_goes_with_it() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for schema in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3] {
            sqlx::raw_sql(schema).execute(&pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO users (username, role, password_hash, created_at, updated_at)
             VALUES ('root', 'superadmin', 'x', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(appearance_of(&pool, 1).await.unwrap(), None);

        let defaults = Appearance::default();
        let look = Appearance {
            theme: "dark".to_owned(),
            wallpaper: Wallpaper {
                builtin_id: "desert-dusk".to_owned(),
                ..defaults.wallpaper
            },
            ..Appearance::default()
        };
        save_appearance(&pool, 1, &look).await.unwrap();
        let look = Appearance {
            accent: "rose".to_owned(),
            ..look
        };
        save_appearance(&pool, 1, &look).await.unwrap();
        assert_eq!(appearance_of(&pool, 1).await.unwrap(), Some(look));

        delete_user(&pool, 1).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_appearance")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "deleting an account deletes its appearance");
    }

    #[tokio::test]
    async fn connections_overwrite_deleted_rows() {
        let pool = open_memory().await.unwrap();
        let secure: i64 = sqlx::query_scalar("PRAGMA secure_delete")
            .fetch_one(&pool)
            .await
            .unwrap();
        let trusted: i64 = sqlx::query_scalar("PRAGMA trusted_schema")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((secure, trusted), (1, 0));
    }
}
