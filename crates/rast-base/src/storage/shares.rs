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
    auth::{AuthUser, Role},
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

/// Effective permission for a user on a share, evaluated by the group policy
/// evaluator (explicit rules > strongest grant > legacy `share_members`/owner
/// fallback; admins bypass). `None` means "not visible" (callers translate that
/// to HTTP 404 so hidden shares stay indistinguishable from missing ones).
pub async fn effective_perm(
    pool: &SqlitePool,
    user: &AuthUser,
    share_id: &str,
) -> AppResult<Option<Perm>> {
    let access = crate::access::folder_access_by_id(pool, user, share_id).await?;
    Ok(access.level)
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

/// Account ids that may observe a shared folder. Candidates come from the
/// legacy ACL (members), direct folder rules and group folder rules (all keyed
/// by stable share id); each candidate is then confirmed through the access
/// evaluator so explicit denials are excluded and group-granted users are
/// included. Used for realtime event audiences.
///
/// The share creator is deliberately not a candidate: creators get no implicit
/// access (audit F6); administrators still receive `Users`-scoped events through
/// the event scope rules.
pub async fn member_ids(pool: &SqlitePool, share_id: &str) -> AppResult<Vec<i64>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM shares WHERE id = ?")
        .bind(share_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let mut candidates: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM share_members WHERE share_id = ?")
            .bind(share_id)
            .fetch_all(pool)
            .await?;
    candidates.extend(
        sqlx::query_scalar::<_, i64>("SELECT user_id FROM user_folder_rules WHERE share_id = ?")
            .bind(share_id)
            .fetch_all(pool)
            .await?,
    );
    candidates.extend(
        sqlx::query_scalar::<_, i64>(
            "SELECT gm.user_id FROM group_members gm
             JOIN group_folder_rules r ON r.group_id = gm.group_id
             WHERE r.share_id = ?",
        )
        .bind(share_id)
        .fetch_all(pool)
        .await?,
    );
    candidates.sort_unstable();
    candidates.dedup();

    let mut ids = Vec::with_capacity(candidates.len());
    for user_id in candidates {
        let user_row = sqlx::query("SELECT username, role FROM users WHERE id = ? AND active = 1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
        let Some(user_row) = user_row else {
            continue;
        };
        let username: String = user_row.get("username");
        let Some(role) = Role::parse(&user_row.get::<String, _>("role")) else {
            continue;
        };
        let candidate = AuthUser {
            id: user_id,
            username: username.clone(),
            display_name: username,
            role,
            session_id: String::new(),
            quota_bytes: 0,
            must_change_password: false,
        };
        let allowed = crate::access::folder_access_by_id(pool, &candidate, share_id)
            .await
            .map(|access| access.level.is_some())
            .unwrap_or(false);
        if allowed {
            ids.push(user_id);
        }
    }
    Ok(ids)
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

/// List shares visible to `user`, with the evaluated access level: admins see
/// everything, regular users see exactly the shares the group evaluator grants
/// them (group rules, direct rules, legacy membership or ownership).
pub async fn list_for_user_with_levels(
    pool: &SqlitePool,
    user: &AuthUser,
) -> AppResult<Vec<(ShareDto, Perm)>> {
    let rows = sqlx::query(
        "SELECT id, name, description, container_path, created_at
         FROM shares ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let record = map_share(row);
        let access = match crate::access::folder_access_by_id(pool, user, &record.id).await {
            Ok(access) => access,
            // The share vanished concurrently; skip it instead of failing the
            // whole listing.
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        let Some(level) = access.level else {
            continue;
        };
        out.push((load_dto(pool, record).await?, level));
    }
    Ok(out)
}

/// List shares visible to `user` (see [`list_for_user_with_levels`]).
pub async fn list_for_user(pool: &SqlitePool, user: &AuthUser) -> AppResult<Vec<ShareDto>> {
    Ok(list_for_user_with_levels(pool, user)
        .await?
        .into_iter()
        .map(|(share, _level)| share)
        .collect())
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
    // A new share changes the visibility matrix for cache entries.
    crate::access::bump_generation();

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
    // Folder rules reference stable share ids, so a rename keeps them without
    // any rewrite; only the display name in `shares` changes.
    tx.commit().await?;
    crate::access::bump_generation();

    // Repair a container directory that was removed out-of-band.
    let _ = std::fs::create_dir_all(storage.shared_container_dir(&current.container_path));

    let record = find_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::internal("shared folder disappeared after update"))?;
    load_dto(pool, record).await
}

/// Delete a shared folder definition; the data on disk is preserved. Folder
/// rules reference `shares.id` with `ON DELETE CASCADE`, so they are removed by
/// the same statement.
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<ShareRecord> {
    let record = find_by_id(pool, id)
        .await?
        .ok_or(AppError::NotFound("shared folder not found"))?;
    sqlx::query("DELETE FROM shares WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    crate::access::bump_generation();
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
                name: "team-acl".into(),
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
        assert_eq!(share.name, "team-acl");
        assert_eq!(share.path, "/shared/team-acl");
        assert_eq!(share.members.len(), 2);
        assert!(storage.shared_container_dir("team-acl").is_dir());

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
    async fn member_ids_include_legacy_members_but_not_implicit_creator() {
        let (_d, storage, pool, admin) = setup().await;
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "team-aud".into(),
                description: None,
                members: Some(vec![
                    MemberInput {
                        user_id: 2,
                        perms: Some("read".into()),
                    },
                    MemberInput {
                        user_id: 3,
                        perms: Some("write".into()),
                    },
                ]),
            },
        )
        .await
        .unwrap();
        // F6: the admin creator is not added as an extra candidate; admins are
        // still delivered Users-scoped events by the event scope rules.
        assert_eq!(member_ids(&pool, &share.id).await.unwrap(), vec![2, 3]);
        assert!(member_ids(&pool, "missing").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn member_ids_include_group_grants_and_exclude_denied_users() {
        let (_d, storage, pool, admin) = setup().await;
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "audience-team".into(),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        // carol has no grant; a direct deny must not make her an event audience.
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (4, 'carol', 'x', 'user', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        add_group(&pool, 30, "readers").await;
        add_group_member(&pool, 30, 3).await;
        set_group_rule(&pool, 30, &share.name, "read").await;
        set_user_rule(&pool, 4, &share.name, "none").await;
        crate::access::bump_generation();

        // Group-granted bob is included; denied carol and the (admin) creator
        // are not implicit candidates (admins get Users events via scope).
        assert_eq!(member_ids(&pool, &share.id).await.unwrap(), vec![3]);
        assert!(member_ids(&pool, "missing-share").await.unwrap().is_empty());
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

    // ----------------------------------------------- group policy (task-13)

    async fn add_group(pool: &SqlitePool, id: i64, name: &str) {
        sqlx::query(
            "INSERT INTO groups (id, name, description, created_at, updated_at)
             VALUES (?, ?, '', 1, 1)",
        )
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_group_member(pool: &SqlitePool, group_id: i64, user_id: i64) {
        sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, 1)")
            .bind(group_id)
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn share_id_for_name(pool: &SqlitePool, share_name: &str) -> String {
        sqlx::query_scalar("SELECT id FROM shares WHERE name = ? COLLATE NOCASE")
            .bind(share_name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn set_group_rule(pool: &SqlitePool, group_id: i64, share_name: &str, rule: &str) {
        let share_id = share_id_for_name(pool, share_name).await;
        sqlx::query(
            "INSERT OR REPLACE INTO group_folder_rules (group_id, share_id, rule)
             VALUES (?, ?, ?)",
        )
        .bind(group_id)
        .bind(share_id)
        .bind(rule)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn set_user_rule(pool: &SqlitePool, user_id: i64, share_name: &str, rule: &str) {
        let share_id = share_id_for_name(pool, share_name).await;
        sqlx::query(
            "INSERT OR REPLACE INTO user_folder_rules (user_id, share_id, rule, updated_at)
             VALUES (?, ?, ?, 0)",
        )
        .bind(user_id)
        .bind(share_id)
        .bind(rule)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn level(pool: &SqlitePool, who: &AuthUser, share_name: &str) -> Option<Perm> {
        crate::access::folder_access_by_name(pool, who, share_name)
            .await
            .unwrap()
            .level
    }

    #[tokio::test]
    async fn admin_bypass_and_no_implicit_creator_access() {
        let (_d, storage, pool, admin) = setup().await;
        let alice = user(2, "alice", Role::User);
        let bob = user(3, "bob", Role::User);
        // A non-admin creates a share with no legacy membership rows.
        let share = create(
            &pool,
            &storage,
            &alice,
            CreateShareInput {
                name: "alice-share".into(),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        // F6: being the creator grants nothing on its own.
        assert_eq!(level(&pool, &alice, &share.name).await, None);
        assert_eq!(level(&pool, &bob, &share.name).await, None);
        assert!(list_for_user(&pool, &alice).await.unwrap().is_empty());
        // Admin bypass still works, even with an explicit deny rule targeting
        // an administrator.
        set_user_rule(&pool, 1, &share.name, "none").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &admin, &share.name).await, Some(Perm::Admin));
        let admin_shares = list_for_user_with_levels(&pool, &admin).await.unwrap();
        assert_eq!(admin_shares.len(), 1);
        assert_eq!(admin_shares[0].1, Perm::Admin);

        // A legacy member still keeps exactly the pre-groups permission.
        sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES (?, 2, 'read')")
            .bind(&share.id)
            .execute(&pool)
            .await
            .unwrap();
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, Some(Perm::Read));
        assert_eq!(list_for_user(&pool, &alice).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn legacy_membership_is_preserved_when_no_rules_exist() {
        let (_d, storage, pool, admin) = setup().await;
        let alice = user(2, "alice", Role::User);
        let bob = user(3, "bob", Role::User);
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "team-legacy".into(),
                description: None,
                members: Some(vec![
                    MemberInput {
                        user_id: 2,
                        perms: Some("read".into()),
                    },
                    MemberInput {
                        user_id: 3,
                        perms: Some("write".into()),
                    },
                ]),
            },
        )
        .await
        .unwrap();
        // Exactly the pre-groups behaviour: read stays read, write stays write.
        assert_eq!(level(&pool, &alice, &share.name).await, Some(Perm::Read));
        assert_eq!(level(&pool, &bob, &share.name).await, Some(Perm::Write));
        assert_eq!(list_for_user(&pool, &alice).await.unwrap().len(), 1);
        assert_eq!(list_for_user(&pool, &bob).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn group_grants_and_explicit_denials_win() {
        let (_d, storage, pool, admin) = setup().await;
        let alice = user(2, "alice", Role::User);
        let bob = user(3, "bob", Role::User);
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "team-grp".into(),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        // New restricted resource with no grants is admin-only.
        assert_eq!(level(&pool, &alice, &share.name).await, None);

        // Group grant: read.
        add_group(&pool, 10, "staff").await;
        add_group_member(&pool, 10, 2).await;
        set_group_rule(&pool, 10, "team-grp", "read").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, Some(Perm::Read));
        assert_eq!(level(&pool, &bob, &share.name).await, None);
        assert_eq!(list_for_user(&pool, &alice).await.unwrap().len(), 1);

        // Direct write is stronger than the group read.
        set_user_rule(&pool, 2, "team-grp", "write").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, Some(Perm::Write));

        // Group deny wins over the direct write (any order).
        set_group_rule(&pool, 10, "team-grp", "none").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, None);
        assert!(list_for_user(&pool, &alice).await.unwrap().is_empty());

        // Direct deny wins over the group write.
        set_group_rule(&pool, 10, "team-grp", "write").await;
        set_user_rule(&pool, 2, "team-grp", "none").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, None);

        // Inherit entries are ignored: with no grant left, the legacy ACL
        // (here: nothing) decides.
        set_group_rule(&pool, 10, "team-grp", "inherit").await;
        set_user_rule(&pool, 2, "team-grp", "inherit").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, &share.name).await, None);
        assert!(list_for_user(&pool, &alice).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_keeps_rules_by_stable_id_and_delete_cascades() {
        let (_d, storage, pool, admin) = setup().await;
        let alice = user(2, "alice", Role::User);
        let share = create(
            &pool,
            &storage,
            &admin,
            CreateShareInput {
                name: "docs".into(),
                description: None,
                members: Some(vec![MemberInput {
                    user_id: 2,
                    perms: Some("read".into()),
                }]),
            },
        )
        .await
        .unwrap();
        add_group(&pool, 20, "editors").await;
        add_group_member(&pool, 20, 2).await;
        set_group_rule(&pool, 20, "docs", "write").await;
        set_user_rule(&pool, 2, "docs", "read").await;
        crate::access::bump_generation();
        assert_eq!(level(&pool, &alice, "docs").await, Some(Perm::Write));

        let updated = update(
            &pool,
            &storage,
            &share.id,
            UpdateShareInput {
                name: Some("documents".into()),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "documents");
        // Rename does not touch rules: they are keyed by the stable share id.
        let group_rules: Vec<(String, String)> =
            sqlx::query_as("SELECT share_id, rule FROM group_folder_rules ORDER BY share_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(group_rules, vec![(share.id.clone(), "write".to_string())]);
        let user_rules: Vec<(String, String)> =
            sqlx::query_as("SELECT share_id, rule FROM user_folder_rules ORDER BY share_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(user_rules, vec![(share.id.clone(), "read".to_string())]);
        // Evaluator follows the current name; the old mount name is gone.
        assert_eq!(level(&pool, &alice, "documents").await, Some(Perm::Write));
        assert!(crate::access::folder_access_by_name(&pool, &alice, "docs")
            .await
            .is_err());

        // Deleting the share cascades every rule that referenced its id.
        delete(&pool, &share.id).await.unwrap();
        let group_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM group_folder_rules WHERE share_id = ?")
                .bind(&share.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let user_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_folder_rules WHERE share_id = ?")
                .bind(&share.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((group_count, user_count), (0, 0));
        assert!(
            crate::access::folder_access_by_name(&pool, &alice, "documents")
                .await
                .is_err()
        );
    }

    // ------------------------------------------------- F5 migration (0005)

    /// Pre-0005 evaluator semantics used only to compare effective access:
    /// explicit deny > strongest grant > legacy `share_members`. (The v0.2.0
    /// owner fallback is irrelevant here: fixture shares are created by an
    /// admin and the compared users are non-admins.)
    async fn pre_migration_level(
        pool: &SqlitePool,
        user_id: i64,
        share_id: &str,
        share_name: &str,
    ) -> Option<Perm> {
        let direct: Option<String> = sqlx::query_scalar(
            "SELECT rule FROM user_folder_rules WHERE user_id = ? AND share_name = ?",
        )
        .bind(user_id)
        .bind(share_name)
        .fetch_optional(pool)
        .await
        .unwrap();
        let group_rules: Vec<String> = sqlx::query_scalar(
            "SELECT r.rule FROM group_members gm
             JOIN group_folder_rules r ON r.group_id = gm.group_id
             WHERE gm.user_id = ? AND r.share_name = ?",
        )
        .bind(user_id)
        .bind(share_name)
        .fetch_all(pool)
        .await
        .unwrap();
        let mut all: Vec<String> = group_rules;
        all.extend(direct);
        let mut deny = false;
        let mut write = false;
        let mut read = false;
        for raw in all {
            match crate::access::FolderRule::parse(&raw) {
                Some(crate::access::FolderRule::None) => deny = true,
                Some(crate::access::FolderRule::Write) => write = true,
                Some(crate::access::FolderRule::Read) => read = true,
                _ => {}
            }
        }
        if deny {
            return None;
        }
        if write {
            return Some(Perm::Write);
        }
        if read {
            return Some(Perm::Read);
        }
        let legacy: Option<String> = sqlx::query_scalar(
            "SELECT perms FROM share_members WHERE share_id = ? AND user_id = ?",
        )
        .bind(share_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .unwrap();
        legacy.as_deref().and_then(Perm::parse)
    }

    fn auth_for(id: i64, username: &str, role: Role) -> AuthUser {
        user(id, username, role)
    }

    #[tokio::test]
    async fn migration_0005_converts_rules_to_share_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migrate-0005.db");
        let pool = db::connect(&path, 4).await.unwrap();
        // Fresh installs run 0005 too and record zeroed statistics.
        for key in ["migration.0005.orphaned_rules", "migration.0005.conflicts"] {
            let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(value, "0", "fresh install {key}");
        }

        // Simulate a schema-v4 installation: recreate the name-keyed rule
        // tables and pretend migration 0005 has not run yet.
        sqlx::query("DROP TABLE group_folder_rules")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP TABLE user_folder_rules")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE group_folder_rules (
                group_id   INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                share_name TEXT    NOT NULL COLLATE NOCASE,
                rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
                PRIMARY KEY (group_id, share_name)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX idx_group_folder_rules_share ON group_folder_rules(share_name)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE user_folder_rules (
                user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                share_name TEXT    NOT NULL COLLATE NOCASE,
                rule       TEXT    NOT NULL CHECK (rule IN ('inherit', 'none', 'read', 'write')),
                updated_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, share_name)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX idx_user_folder_rules_share ON user_folder_rules(share_name)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 5")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM settings WHERE key LIKE 'migration.0005.%'")
            .execute(&pool)
            .await
            .unwrap();

        // Fixture: 3 users, 2 groups, 2 shares, legacy ACL, direct + group
        // rules (including an explicit deny) and two orphaned rule names.
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
        for (id, name) in [(1_i64, "staff"), (2, "ops")] {
            sqlx::query(
                "INSERT INTO groups (id, name, description, created_at, updated_at)
                 VALUES (?, ?, '', 1, 1)",
            )
            .bind(id)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (group_id, user_id) in [(1_i64, 2_i64), (2, 3)] {
            sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, 1)")
                .bind(group_id)
                .bind(user_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (id, name) in [("mig5-alpha", "Mig5 Alpha"), ("mig5-beta", "Mig5 Beta")] {
            sqlx::query(
                "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
                 VALUES (?, ?, '', ?, 1, 1, 1)",
            )
            .bind(id)
            .bind(name)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO share_members (share_id, user_id, perms) VALUES ('mig5-beta', 2, 'read')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Name-keyed rules (v4 layout).
        for (group_id, share_name, rule) in [
            (1_i64, "Mig5 Alpha", "write"),
            (1, "Mig5 Beta", "read"),
            (2, "Mig5 Alpha", "read"),
            (2, "Mig5 Beta", "none"),
            (1, "Ghost", "none"), // orphan (no such share)
        ] {
            sqlx::query(
                "INSERT INTO group_folder_rules (group_id, share_name, rule) VALUES (?, ?, ?)",
            )
            .bind(group_id)
            .bind(share_name)
            .bind(rule)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (user_id, share_name, rule, updated_at) in [
            (2_i64, "Mig5 Alpha", "read", 1111_i64),
            (2, "Mig5 Beta", "write", 2222),
            (3, "Ghost", "read", 3333), // orphan (no such share)
        ] {
            sqlx::query(
                "INSERT INTO user_folder_rules (user_id, share_name, rule, updated_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(share_name)
            .bind(rule)
            .bind(updated_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Effective access before the migration, v4 semantics.
        let users = [(2_i64, "alice", Role::User), (3, "bob", Role::User)];
        let shares = [("mig5-alpha", "Mig5 Alpha"), ("mig5-beta", "Mig5 Beta")];
        let mut before = std::collections::BTreeMap::new();
        for (user_id, _, _) in users {
            for (share_id, share_name) in shares {
                before.insert(
                    (user_id, share_id.to_string()),
                    pre_migration_level(&pool, user_id, share_id, share_name).await,
                );
            }
        }
        pool.close().await;

        // Reopening runs the embedded migrator, which applies 0005.
        let pool = db::connect(&path, 4).await.unwrap();
        let applied: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 5 AND success = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(
            db::schema_version(&pool).await,
            db::embedded_schema_version()
        );
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('group_folder_rules')")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(columns.iter().any(|c| c == "share_id"));
        assert!(!columns.iter().any(|c| c == "share_name"));
        let user_columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('user_folder_rules')")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(user_columns.iter().any(|c| c == "share_id"));
        assert!(user_columns.iter().any(|c| c == "updated_at"));

        // Counts: two orphaned rows dropped, no duplicate collisions.
        let orphaned: String = sqlx::query_scalar(
            "SELECT value FROM settings WHERE key = 'migration.0005.orphaned_rules'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let conflicts: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'migration.0005.conflicts'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphaned, "2");
        assert_eq!(conflicts, "0");

        // Migrated rows are keyed by id.
        let group_rules: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT group_id, share_id, rule FROM group_folder_rules ORDER BY group_id, share_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(group_rules.len(), 4);
        assert!(group_rules
            .iter()
            .any(|(g, s, r)| *g == 1 && s == "mig5-alpha" && r == "write"));
        let user_rules: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT user_id, share_id, rule FROM user_folder_rules ORDER BY user_id, share_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(user_rules.len(), 2);
        assert!(user_rules
            .iter()
            .any(|(u, s, r)| *u == 2 && s == "mig5-beta" && r == "write"));
        // Original per-row timestamps are preserved by the migration.
        let migrated_updated_at: i64 = sqlx::query_scalar(
            "SELECT updated_at FROM user_folder_rules WHERE user_id = 2 AND share_id = 'mig5-beta'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(migrated_updated_at, 2222);

        // Effective access is identical before/after for every fixture pair.
        crate::access::bump_generation();
        for (user_id, username, role) in users {
            let auth = auth_for(user_id, username, role);
            for (share_id, share_name) in shares {
                let after = crate::access::folder_access_by_id(&pool, &auth, share_id)
                    .await
                    .unwrap();
                assert_eq!(
                    after.level,
                    before[&(user_id, share_id.to_string())],
                    "access changed for user {user_id} on {share_name}"
                );
                assert_eq!(after.share_name, share_name);
            }
        }
        // Admin bypass is unaffected.
        let admin = auth_for(1, "admin", Role::Admin);
        assert_eq!(
            crate::access::folder_access_by_id(&pool, &admin, "mig5-alpha")
                .await
                .unwrap()
                .level,
            Some(Perm::Admin)
        );

        // Renaming after migration keeps the rules (ids are stable).
        let storage = Storage::new(&crate::storage::tests::test_config(dir.path())).unwrap();
        update(
            &pool,
            &storage,
            "mig5-alpha",
            UpdateShareInput {
                name: Some("Mig5 Renamed".into()),
                description: None,
                members: None,
            },
        )
        .await
        .unwrap();
        let alice = auth_for(2, "alice", Role::User);
        let renamed = crate::access::folder_access_by_name(&pool, &alice, "Mig5 Renamed")
            .await
            .unwrap();
        assert_eq!(renamed.level, Some(Perm::Write));
        assert_eq!(renamed.share_id, "mig5-alpha");

        // Deleting a share cascades its rules.
        delete(&pool, "mig5-beta").await.unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM group_folder_rules WHERE share_id = ?")
                .bind("mig5-beta")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        let user_remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_folder_rules WHERE share_id = ?")
                .bind("mig5-beta")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(user_remaining, 0);
    }
}
