//! User domain logic shared by the HTTP API, the CLI and the setup flow.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::{
    auth::{
        password::{self, hash_password_async},
        Role,
    },
    config::Config,
    db,
    error::{AppError, AppResult},
};

#[derive(Debug, Clone, Serialize)]
pub struct UserRecord {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub active: bool,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub must_change_password: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_login_at: Option<i64>,
}

impl UserRecord {
    pub fn home_rel(&self) -> String {
        format!("homes/{}", self.username.to_ascii_lowercase())
    }
}

/// Validate a username: 1-32 chars, starts alphanumeric, no path separators.
pub fn validate_username(username: &str) -> AppResult<String> {
    let trimmed = username.trim();
    if trimmed.is_empty() || trimmed.len() > 32 {
        return Err(AppError::bad_request(
            "username must be 1-32 characters long",
        ));
    }
    let first = trimmed.chars().next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(AppError::bad_request(
            "username must start with a letter or digit",
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AppError::bad_request(
            "username may only contain letters, digits, dot, dash and underscore",
        ));
    }
    if trimmed == "." || trimmed == ".." || trimmed.ends_with('.') {
        return Err(AppError::bad_request("invalid username"));
    }
    Ok(trimmed.to_string())
}

/// Create a user, their home directory and quota row atomically-ish.
pub async fn create(
    pool: &SqlitePool,
    cfg: &Config,
    username: &str,
    password: &str,
    role: Role,
    display_name: Option<&str>,
    quota_bytes: Option<i64>,
) -> AppResult<UserRecord> {
    let username = validate_username(username)?;
    let lower = username.to_ascii_lowercase();
    // Password policy handled inside hash_password.
    let hash = hash_password_async(password.to_string()).await?;
    let now = db::now_ms();
    let display = display_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| username.clone());
    let quota = quota_bytes.unwrap_or(cfg.default_quota_bytes as i64).max(0);

    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&lower)
        .fetch_optional(pool)
        .await?;
    if existing.is_some() {
        return Err(AppError::conflict("username already exists"));
    }

    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT INTO users (username, display_name, password_hash, role, active, quota_bytes, created_at, updated_at)
         VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(&lower)
    .bind(&display)
    .bind(&hash)
    .bind(role.as_str())
    .bind(quota)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    let id = match res {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::conflict("username already exists"))
        }
        Err(e) => return Err(e.into()),
    };
    sqlx::query(
        "INSERT INTO storage_usage (user_id, used_bytes, reserved_bytes, updated_at) VALUES (?, 0, 0, ?)",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // Home directory: 0700, owned by the service account running the server.
    let home = cfg.home_for(&lower);
    std::fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }

    find_by_id(pool, id)
        .await?
        .ok_or(AppError::internal("user disappeared after creation"))
}

pub async fn find_by_username(pool: &SqlitePool, username: &str) -> AppResult<Option<UserRecord>> {
    let row = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.active, u.quota_bytes,
                u.must_change_password, u.created_at, u.updated_at, u.last_login_at,
                COALESCE(s.used_bytes, 0) AS used_bytes
         FROM users u LEFT JOIN storage_usage s ON s.user_id = u.id
         WHERE u.username = ?",
    )
    .bind(username.trim())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(map_user))
}

pub async fn find_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<UserRecord>> {
    let row = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.active, u.quota_bytes,
                u.must_change_password, u.created_at, u.updated_at, u.last_login_at,
                COALESCE(s.used_bytes, 0) AS used_bytes
         FROM users u LEFT JOIN storage_usage s ON s.user_id = u.id
         WHERE u.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(map_user))
}

/// Account status filter for [`list_filtered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveFilter {
    #[default]
    All,
    Active,
    Inactive,
}

impl ActiveFilter {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => Self::Active,
            Some("inactive") | Some("disabled") => Self::Inactive,
            _ => Self::All,
        }
    }
}

/// Escape `%`, `_` and `\` for a `LIKE ... ESCAPE '\'` pattern.
fn like_pattern(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('%');
    for c in value.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Paginated account list (search by username/display name, any length).
pub async fn list(
    pool: &SqlitePool,
    query: Option<&str>,
    page: i64,
    per_page: i64,
) -> AppResult<(Vec<UserRecord>, i64)> {
    list_filtered(pool, query, ActiveFilter::All, page, per_page).await
}

/// Paginated account list with search and status filters. Searching is
/// case-insensitive (usernames and display names use NOCASE/LOWER) and works
/// for any non-empty query, including one or two characters.
pub async fn list_filtered(
    pool: &SqlitePool,
    query: Option<&str>,
    active: ActiveFilter,
    page: i64,
    per_page: i64,
) -> AppResult<(Vec<UserRecord>, i64)> {
    let page = page.max(1);
    let per_page = per_page.clamp(1, 100);
    let offset = (page - 1) * per_page;
    let search = query.map(str::trim).filter(|q| !q.is_empty());
    let mut clauses: Vec<&str> = Vec::new();
    if search.is_some() {
        clauses.push(
            "(u.username LIKE ? ESCAPE '\\' OR LOWER(u.display_name) LIKE LOWER(?) ESCAPE '\\')",
        );
    }
    match active {
        ActiveFilter::All => {}
        ActiveFilter::Active => clauses.push("u.active = 1"),
        ActiveFilter::Inactive => clauses.push("u.active = 0"),
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let pattern = search.map(like_pattern);

    let count_sql = format!("SELECT COUNT(*) FROM users u{where_sql}");
    let mut count = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql));
    if let Some(pattern) = &pattern {
        count = count.bind(pattern.clone()).bind(pattern.clone());
    }
    let total = count.fetch_one(pool).await?;

    let rows_sql = format!(
        "SELECT u.id, u.username, u.display_name, u.role, u.active, u.quota_bytes,
                u.must_change_password, u.created_at, u.updated_at, u.last_login_at,
                COALESCE(s.used_bytes, 0) AS used_bytes
         FROM users u LEFT JOIN storage_usage s ON s.user_id = u.id{where_sql}
         ORDER BY u.id ASC LIMIT ? OFFSET ?"
    );
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(rows_sql));
    if let Some(pattern) = &pattern {
        rows = rows.bind(pattern.clone()).bind(pattern.clone());
    }
    let rows = rows.bind(per_page).bind(offset).fetch_all(pool).await?;
    Ok((rows.into_iter().map(map_user).collect(), total))
}

pub async fn active_admin_count(pool: &SqlitePool) -> AppResult<i64> {
    let n = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Update mutable profile fields. Returns the updated record.
///
/// The last-active-admin safeguard runs inside the same transaction as the
/// account/role change: the first statement inside the transaction takes the
/// SQLite write lock, so a concurrent demotion/deactivation cannot observe a
/// stale admin count and end up removing the last administrator.
pub async fn update(
    pool: &SqlitePool,
    id: i64,
    display_name: Option<&str>,
    role: Option<Role>,
    active: Option<bool>,
    quota_bytes: Option<i64>,
    must_change_password: Option<bool>,
) -> AppResult<UserRecord> {
    let mut tx = pool.begin().await?;
    // Take the write lock before reading the admin count.
    let reserved = sqlx::query("UPDATE users SET updated_at = updated_at WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if reserved.rows_affected() == 0 {
        return Err(AppError::NotFound("user not found"));
    }
    let current = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.active, u.quota_bytes,
                u.must_change_password, u.created_at, u.updated_at, u.last_login_at,
                COALESCE(s.used_bytes, 0) AS used_bytes
         FROM users u LEFT JOIN storage_usage s ON s.user_id = u.id
         WHERE u.id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound("user not found"))
    .map(map_user)?;

    let losing_admin = current.role == Role::Admin
        && (role.is_some_and(|r| r != Role::Admin) || active == Some(false));
    if losing_admin {
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1")
                .fetch_one(&mut *tx)
                .await?;
        if admins <= 1 {
            return Err(AppError::bad_request(
                "cannot demote or deactivate the last active administrator",
            ));
        }
    }

    let now = db::now_ms();
    sqlx::query(
        "UPDATE users SET display_name = ?, role = ?, active = ?, quota_bytes = ?,
                must_change_password = ?, updated_at = ? WHERE id = ?",
    )
    .bind(
        display_name
            .map(|s| s.trim().to_string())
            .unwrap_or(current.display_name),
    )
    .bind(role.unwrap_or(current.role).as_str())
    .bind(active.unwrap_or(current.active))
    .bind(quota_bytes.unwrap_or(current.quota_bytes).max(0))
    .bind(must_change_password.unwrap_or(current.must_change_password))
    .bind(now)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if active == Some(false) {
        crate::auth::session::revoke_all_for_user(pool, id, None).await?;
    }
    find_by_id(pool, id)
        .await?
        .ok_or(AppError::NotFound("user not found"))
}

/// Set a new password (hashed) and revoke other sessions optionally.
pub async fn set_password(
    pool: &SqlitePool,
    id: i64,
    new_password: &str,
    must_change: bool,
    revoke_sessions_except: Option<&str>,
) -> AppResult<()> {
    let hash = hash_password_async(new_password.to_string()).await?;
    let now = db::now_ms();
    let res = sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_password = ?, failed_attempts = 0,
                locked_until = 0, updated_at = ? WHERE id = ?",
    )
    .bind(&hash)
    .bind(must_change)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("user not found"));
    }
    crate::auth::session::revoke_all_for_user(pool, id, revoke_sessions_except).await?;
    Ok(())
}

/// Fetch the stored PHC hash (never exposed through the API).
pub async fn password_hash(pool: &SqlitePool, id: i64) -> AppResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Deactivate an account (default admin action; data is preserved).
pub async fn deactivate(pool: &SqlitePool, id: i64) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    // Same transaction/locking discipline as `update`: write lock first, then
    // count, then flip the row.
    let reserved = sqlx::query("UPDATE users SET updated_at = updated_at WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if reserved.rows_affected() == 0 {
        return Err(AppError::NotFound("user not found"));
    }
    let (role, active): (String, bool) =
        sqlx::query_as("SELECT role, active FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if role == "admin" && active {
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND active = 1")
                .fetch_one(&mut *tx)
                .await?;
        if admins <= 1 {
            return Err(AppError::bad_request(
                "cannot deactivate the last active administrator",
            ));
        }
    }
    let now = db::now_ms();
    sqlx::query("UPDATE users SET active = 0, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::auth::session::revoke_all_for_user(pool, id, None).await?;
    Ok(())
}

fn map_user(row: sqlx::sqlite::SqliteRow) -> UserRecord {
    UserRecord {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        role: Role::parse(row.get::<String, _>("role").as_str()).unwrap_or(Role::User),
        active: row.get("active"),
        quota_bytes: row.get("quota_bytes"),
        used_bytes: row.get("used_bytes"),
        must_change_password: row.get("must_change_password"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_login_at: row.get("last_login_at"),
    }
}

/// DTO returned by the API (keeps the password hash internal).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub active: bool,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub must_change_password: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_login_at: Option<i64>,
    /// Groups the account belongs to (filled by the users endpoints).
    pub groups: Vec<crate::access::GroupRef>,
    /// Convenience id list for editors that only need memberships.
    pub group_ids: Vec<i64>,
}

impl From<UserRecord> for UserDto {
    fn from(u: UserRecord) -> Self {
        Self {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            role: u.role,
            active: u.active,
            quota_bytes: u.quota_bytes,
            used_bytes: u.used_bytes,
            must_change_password: u.must_change_password,
            created_at: u.created_at,
            updated_at: u.updated_at,
            last_login_at: u.last_login_at,
            groups: Vec::new(),
            group_ids: Vec::new(),
        }
    }
}

/// Request body shared by user creation endpoints.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub quota_bytes: Option<i64>,
}

/// Verify a password for a user id, used by long-lived CLI operations.
pub async fn verify_password(pool: &SqlitePool, id: i64, candidate: &str) -> AppResult<bool> {
    let Some(hash) = password_hash(pool, id).await? else {
        return Ok(false);
    };
    password::verify_password(candidate, &hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db};

    fn test_cfg(dir: &std::path::Path) -> Config {
        Config::from_parts(
            None,
            crate::config::FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap()
    }

    async fn setup() -> (tempfile::TempDir, Config, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        let pool = db::init(&cfg).await.unwrap();
        (dir, cfg, pool)
    }

    #[test]
    fn username_validation() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("alice.b-1_2").is_ok());
        for bad in [
            "",
            " ",
            "../etc",
            "a/b",
            ".hidden",
            "-x",
            "a".repeat(40).as_str().to_string().as_str(),
        ] {
            assert!(validate_username(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[tokio::test]
    async fn create_and_find_user() {
        let (_d, cfg, pool) = setup().await;
        let user = create(
            &pool,
            &cfg,
            "Alice",
            "Sup3r-Secret-1!",
            Role::Admin,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(user.username, "alice");
        assert_eq!(user.role, Role::Admin);
        assert!(cfg.home_for("alice").is_dir());
        assert!(find_by_username(&pool, "ALICE").await.unwrap().is_some());
        assert!(validate_username("alice").is_ok());
        let dup = create(
            &pool,
            &cfg,
            "alice",
            "Sup3r-Secret-1!",
            Role::User,
            None,
            None,
        )
        .await;
        assert!(dup.is_err());
    }

    #[tokio::test]
    async fn weak_password_rejected() {
        let (_d, cfg, pool) = setup().await;
        let err = create(&pool, &cfg, "bob", "short", Role::User, None, None).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn last_admin_protected() {
        let (_d, cfg, pool) = setup().await;
        let admin = create(
            &pool,
            &cfg,
            "root",
            "Sup3r-Secret-1!",
            Role::Admin,
            None,
            None,
        )
        .await
        .unwrap();
        let err = update(&pool, admin.id, None, Some(Role::User), None, None, None).await;
        assert!(err.is_err());
        let err = deactivate(&pool, admin.id).await;
        assert!(err.is_err());
        // A second admin allows demotion.
        create(
            &pool,
            &cfg,
            "admin2",
            "Sup3r-Secret-2!",
            Role::Admin,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            update(&pool, admin.id, None, Some(Role::User), None, None, None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn password_change_revokes_sessions() {
        let (_d, cfg, pool) = setup().await;
        let user = create(
            &pool,
            &cfg,
            "carol",
            "Sup3r-Secret-1!",
            Role::User,
            None,
            None,
        )
        .await
        .unwrap();
        let s = crate::auth::session::create(&pool, user.id, 24, "", "")
            .await
            .unwrap();
        set_password(&pool, user.id, "An0ther-Secret!", false, None)
            .await
            .unwrap();
        assert!(crate::auth::session::validate(&pool, &s.token, 24)
            .await
            .unwrap()
            .is_none());
        assert!(verify_password(&pool, user.id, "An0ther-Secret!")
            .await
            .unwrap());
        assert!(!verify_password(&pool, user.id, "Sup3r-Secret-1!")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn listing_paginates() {
        let (_d, cfg, pool) = setup().await;
        for i in 0..5 {
            create(
                &pool,
                &cfg,
                &format!("user{i}"),
                "Sup3r-Secret-1!",
                Role::User,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let (rows, total) = list(&pool, None, 1, 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(total, 5);
        let (rows, _) = list(&pool, Some("user3"), 1, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].username, "user3");
    }
}
