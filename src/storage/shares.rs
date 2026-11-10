//! Shared folders: definitions (`shares`) and per-user ACLs
//! (`share_members`).
//!
//! A shared folder is mounted read-only/read-write under `/shared/<name>` for
//! members only. Administrators bypass the ACL. The physical container
//! directory is stored in `shares.container_path` and never recomputed from the
//! display name, so renaming a share does not move or expose user data.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    db,
    error::{AppError, AppResult},
    storage::{safe_path, Perm, Storage},
};

/// Maximum length of a shared-folder display name.
pub const MAX_SHARE_NAME: usize = 64;
/// Maximum length of a shared-folder description.
pub const MAX_SHARE_DESCRIPTION: usize = 500;

/// A share row without its members.
#[derive(Debug, Clone)]
pub struct ShareRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub container_path: String,
    pub created_at: i64,
}

/// A share member as exposed by the API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberDto {
    pub user_id: i64,
    pub username: String,
    pub perms: String,
}

/// A shared folder as exposed by the API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareDto {
    pub id: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub members: Vec<MemberDto>,
    pub created_at: i64,
}

/// Member entry accepted by create/update requests.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberInput {
    pub user_id: i64,
    #[serde(default)]
    pub perms: Option<String>,
}

/// `POST /api/shares` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateShareInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub members: Option<Vec<MemberInput>>,
}

/// `PATCH /api/shares/{id}` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateShareInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub members: Option<Vec<MemberInput>>,
}

/// Validate a shared-folder / container name. Returns the trimmed name.
pub fn validate_share_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    safe_path::validate_new_name(name)
        .map_err(|_| AppError::bad_request("invalid shared folder name"))?;
    if name.chars().count() > MAX_SHARE_NAME {
        return Err(AppError::bad_request(
            "shared folder name must be at most 64 characters",
        ));
    }
    Ok(name.to_string())
}

/// Validate a stored container path (defence in depth against a corrupted DB).
pub fn validated_container(container_path: &str) -> AppResult<String> {
    if container_path.is_empty() || container_path.len() > safe_path::MAX_NAME_BYTES {
        return Err(AppError::internal("invalid shared folder container path"));
    }
    safe_path::validate_component(container_path)
        .map_err(|_| AppError::internal("invalid shared folder container path"))?;
    Ok(container_path.to_string())
}

/// Find a share by display name (case-insensitive, matches the UNIQUE index).
pub async fn find_by_name(pool: &SqlitePool, name: &str) -> AppResult<Option<ShareRecord>> {
    let row = sqlx::query(
        "SELECT id, name, description, container_path, created_at FROM shares WHERE name = ?",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(map_share))
}

pub async fn find_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<ShareRecord>> {
    let row = sqlx::query(
        "SELECT id, name, description, container_path, created_at FROM shares WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(map_share))
}

/// Effective permission for a user on a share. `None` means "not visible"
/// (which callers translate to HTTP 404, not 403).
pub async fn effective_perm(
    pool: &SqlitePool,
    user: &AuthUser,
    share_id: &str,
) -> AppResult<Option<Perm>> {
    if user.is_admin() {
        return Ok(Some(Perm::Admin));
    }
    let row = sqlx::query("SELECT perms FROM share_members WHERE share_id = ? AND user_id = ?")
        .bind(share_id)
        .bind(user.id)
        .fetch_optional(pool)
        .await?;
    Ok(row
        .and_then(|r| Perm::parse(r.get::<String, _>("perms").as_str()))
        .or(None))
}

/// Load the member list of one share.
pub async fn members(pool: &SqlitePool, share_id: &str) -> AppResult<Vec<MemberDto>> {
    let rows = sqlx::query(
        "SELECT m.user_id, u.username, m.perms
         FROM share_members m JOIN users u ON u.id = m.user_id
         WHERE m.share_id = ? ORDER BY u.username",
    )
    .bind(share_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| MemberDto {
            user_id: r.get("user_id"),
            username: r.get("username"),
            perms: r.get("perms"),
        })
        .collect())
}

fn dto(record: ShareRecord, members: Vec<MemberDto>) -> ShareDto {
    ShareDto {
        path: format!("/shared/{}", record.name),
        id: record.id,
        name: record.name,
        description: record.description,
        members,
        created_at: record.created_at,
    }
}

async fn load_dto(pool: &SqlitePool, record: ShareRecord) -> AppResult<ShareDto> {
    let members = members(pool, &record.id).await?;
    Ok(dto(record, members))
}

/// List shares visible to `user`: admins see everything, regular users only
/// the shares they are a member of.
pub async fn list_for_user(pool: &SqlitePool, user: &AuthUser) -> AppResult<Vec<ShareDto>> {
    let rows = if user.is_admin() {
        sqlx::query(
            "SELECT id, name, description, container_path, created_at
             FROM shares ORDER BY name COLLATE NOCASE",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT s.id, s.name, s.description, s.container_path, s.created_at
             FROM shares s JOIN share_members m ON m.share_id = s.id
             WHERE m.user_id = ?
             ORDER BY s.name COLLATE NOCASE",
        )
        .bind(user.id)
        .fetch_all(pool)
        .await?
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(load_dto(pool, map_share(row)).await?);
    }
    Ok(out)
}

/// Normalise and validate a member list: dedupe by user id, validate perms and
/// ensure every referenced user exists.
async fn normalize_members(
    pool: &SqlitePool,
    members: &[MemberInput],
) -> AppResult<Vec<(i64, Perm)>> {
    if members.len() > 500 {
        return Err(AppError::bad_request("too many members"));
    }
    let mut out: Vec<(i64, Perm)> = Vec::with_capacity(members.len());
    for member in members {
        let perm = match member.perms.as_deref() {
            None => Perm::Read,
            Some(raw) => Perm::parse(raw.trim()).ok_or_else(|| {
                AppError::bad_request(format!(
                    "invalid permission {raw:?}; expected read, write or admin"
                ))
            })?,
        };
        if out.iter().any(|(id, _)| *id == member.user_id) {
            continue; // last write wins would be confusing; keep the first entry
        }
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
            .bind(member.user_id)
            .fetch_optional(pool)
            .await?;
        if exists.is_none() {
            return Err(AppError::bad_request(format!(
                "member user {} does not exist",
                member.user_id
            )));
        }
        out.push((member.user_id, perm));
    }
    Ok(out)
}

/// Create a shared folder. Returns the created definition and makes sure the
/// physical container directory exists (existing data is adopted, never
/// deleted).
pub async fn create(
    pool: &SqlitePool,
    storage: &Storage,
    actor: &AuthUser,
    input: CreateShareInput,
) -> AppResult<ShareDto> {
    let name = validate_share_name(&input.name)?;
    let description = normalize_description(input.description)?;
    let members = normalize_members(pool, input.members.as_deref().unwrap_or(&[])).await?;
    let container = name.clone();
    let container_dir = storage.shared_container_dir(&container);
    let existed = container_dir.exists();
    std::fs::create_dir_all(&container_dir)?;

    let id = Uuid::new_v4().to_string();
    let now = db::now_ms();
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&description)
    .bind(&container)
    .bind(actor.id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::conflict(
                "a shared folder with this name already exists",
            ));
        }
        Err(e) => {
            if !existed {
                let _ = std::fs::remove_dir(&container_dir);
            }
            return Err(e.into());
        }
    }
    for (user_id, perm) in &members {
        sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(perm.as_str())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    let record = find_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::internal("shared folder disappeared after creation"))?;
    load_dto(pool, record).await
}

fn normalize_description(description: Option<String>) -> AppResult<String> {
    let description = description.unwrap_or_default();
    let trimmed = description.trim();
    if trimmed.chars().count() > MAX_SHARE_DESCRIPTION {
        return Err(AppError::bad_request(
            "shared folder description is too long",
        ));
    }
    Ok(trimmed.to_string())
}

/// Update a shared folder definition. The physical container is kept stable on
/// rename so data is never moved.
pub async fn update(
    pool: &SqlitePool,
    storage: &Storage,
    id: &str,
    input: UpdateShareInput,
) -> AppResult<ShareDto> {
    let current = find_by_id(pool, id)
        .await?
        .ok_or(AppError::NotFound("shared folder not found"))?;
    let now = db::now_ms();
    let mut name = current.name.clone();
    let mut description = current.description.clone();
    if let Some(raw) = input.name {
        name = validate_share_name(&raw)?;
    }
    if let Some(raw) = input.description {
        description = normalize_description(Some(raw))?;
    }
    let new_members = match input.members {
        Some(list) => Some(normalize_members(pool, &list).await?),
        None => None,
    };

    let mut tx = pool.begin().await?;
    let result =
        sqlx::query("UPDATE shares SET name = ?, description = ?, updated_at = ? WHERE id = ?")
            .bind(&name)
            .bind(&description)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await;
    match result {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::conflict(
                "a shared folder with this name already exists",
            ));
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(members) = new_members {
        sqlx::query("DELETE FROM share_members WHERE share_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (user_id, perm) in &members {
            sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES (?, ?, ?)")
                .bind(id)
                .bind(user_id)
                .bind(perm.as_str())
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;

    // Repair a container directory that was removed out-of-band.
    let _ = std::fs::create_dir_all(storage.shared_container_dir(&current.container_path));

    let record = find_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::internal("shared folder disappeared after update"))?;
    load_dto(pool, record).await
}

/// Delete a shared folder definition; the data on disk is preserved.
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<ShareRecord> {
    let record = find_by_id(pool, id)
        .await?
        .ok_or(AppError::NotFound("shared folder not found"))?;
    sqlx::query("DELETE FROM shares WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(record)
}

fn map_share(row: sqlx::sqlite::SqliteRow) -> ShareRecord {
    ShareRecord {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        container_path: row.get("container_path"),
        created_at: row.get("created_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Role, db};

    fn user(id: i64, username: &str, role: Role) -> AuthUser {
        AuthUser {
            id,
            username: username.to_string(),
            display_name: username.to_string(),
            role,
            session_id: "test-session".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    async fn setup() -> (tempfile::TempDir, Storage, SqlitePool, AuthUser) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::storage::tests::test_config(dir.path());
        let storage = Storage::new(&cfg).unwrap();
        let pool = db::connect(&dir.path().join("shares.db"), 4).await.unwrap();
        for (id, username, role) in [
            (1_i64, "admin", "admin"),
            (2, "alice", "user"),
            (3, "bob", "user"),
        ] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', ?, 1, 1)",
            )
            .bind(id)
            .bind(username)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        }
        let admin = user(1, "admin", Role::Admin);
        (dir, storage, pool, admin)
    }

    #[tokio::test]
    async fn create_list_and_acls() {
        let (_d, storage, pool, admin) = setup().await;
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "team".into(),
                description: Some("Team files".into()),
                members: Some(vec![
                    MemberInput {
                        user_id: 2,
                        perms: Some("write".into()),
                    },
                    MemberInput {
                        user_id: 3,
                        perms: Some("read".into()),
                    },
                ]),
            },
        )
        .await
        .unwrap();
        assert_eq!(share.name, "team");
        assert_eq!(share.path, "/shared/team");
        assert_eq!(share.members.len(), 2);
        assert!(storage.shared_container_dir("team").is_dir());

        // Admin sees the share and has full permissions.
        assert_eq!(list_for_user(&pool, &admin).await.unwrap().len(), 1);
        assert_eq!(
            effective_perm(&pool, &admin, &share.id).await.unwrap(),
            Some(Perm::Admin)
        );

        // Members see the share with their permission.
        let alice = user(2, "alice", Role::User);
        let bob = user(3, "bob", Role::User);
        let carol = user(4, "carol", Role::User);
        assert_eq!(
            effective_perm(&pool, &alice, &share.id).await.unwrap(),
            Some(Perm::Write)
        );
        assert_eq!(
            effective_perm(&pool, &bob, &share.id).await.unwrap(),
            Some(Perm::Read)
        );
        // Non-members must not even learn the share exists.
        assert!(effective_perm(&pool, &carol, &share.id)
            .await
            .unwrap()
            .is_none());
        assert!(list_for_user(&pool, &carol).await.unwrap().is_empty());
        assert!(list_for_user(&pool, &bob).await.unwrap().len() == 1);
    }

    #[tokio::test]
    async fn duplicate_names_conflict_case_insensitively() {
        let (_d, storage, pool, admin) = setup().await;
        let input = |name: &str| CreateShareInput {
            name: name.into(),
            description: None,
            members: None,
        };
        create(&pool, &storage, &admin, input("Team"))
            .await
            .unwrap();
        let err = create(&pool, &storage, &admin, input("team"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        // Invalid names are rejected.
        for bad in ["../x", "a/b", "", "..", ".rast-tmp-x"] {
            assert!(
                create(&pool, &storage, &admin, input(bad)).await.is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[tokio::test]
    async fn rename_keeps_container_and_delete_keeps_data() {
        let (_d, storage, pool, admin) = setup().await;
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "docs".into(),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        let container = storage.shared_container_dir("docs");
        std::fs::write(container.join("keep.txt"), b"data").unwrap();

        let updated = update(
            &pool,
            &storage,
            &share.id,
            UpdateShareInput {
                name: Some("documents".into()),
                description: Some("renamed".into()),
                members: Some(vec![MemberInput {
                    user_id: 2,
                    perms: Some("read".into()),
                }]),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "documents");
        assert_eq!(updated.path, "/shared/documents");
        // Same physical container, same data.
        assert_eq!(updated.members.len(), 1);
        assert!(container.join("keep.txt").is_file());

        delete(&pool, &share.id).await.unwrap();
        assert!(find_by_id(&pool, &share.id).await.unwrap().is_none());
        assert!(
            container.join("keep.txt").is_file(),
            "data must be preserved"
        );
    }

    #[tokio::test]
    async fn invalid_members_are_rejected() {
        let (_d, storage, pool, admin) = setup().await;
        let err = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "x".into(),
                description: None,
                members: Some(vec![MemberInput {
                    user_id: 999,
                    perms: None,
                }]),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        let err = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "x".into(),
                description: None,
                members: Some(vec![MemberInput {
                    user_id: 2,
                    perms: Some("owner".into()),
                }]),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
