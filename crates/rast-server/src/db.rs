//! SQLite storage: accounts, sessions, groups with app rules, and an audit trail.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::Role;
use crate::access::Rule;

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
    if version < 2 {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(SCHEMA_V2).execute(&mut *tx).await?;
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
        assert_eq!(version, 2);
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
}
