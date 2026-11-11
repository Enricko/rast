//! User groups and the access-policy administration API.
//!
//! Everything here is administrator-only except `GET /api/access/me`, which
//! returns the caller's own effective access for the desktop launcher. The
//! evaluator itself lives in [`crate::access`]; this module only stores the
//! policy, validates it, audits changes and publishes live-update events.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::{
    access::{self, AppRule, FolderRule, GroupRef, APP_IDS},
    audit::{self, AuditLog},
    auth::{AdminUser, AuthUser, ClientIp},
    db,
    error::{AppError, AppResult},
    state::AppState,
    users,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/groups", get(list_groups).post(create_group))
        .route(
            "/api/groups/{id}",
            get(get_group).put(update_group).delete(delete_group),
        )
        .route(
            "/api/users/{id}/groups",
            get(get_user_groups).put(set_user_groups),
        )
        .route(
            "/api/users/{id}/access-rules",
            get(get_user_access_rules).put(set_user_access_rules),
        )
        .route(
            "/api/users/{id}/effective-access",
            get(get_user_effective_access),
        )
        .route("/api/access/me", get(my_effective_access))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSummary {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub member_count: i64,
    pub folder_rule_count: i64,
    pub app_rule_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemberDto {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRuleDto {
    pub share_name: String,
    pub rule: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRuleDto {
    pub app_id: String,
    pub rule: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupDetail {
    #[serde(flatten)]
    pub summary: GroupSummary,
    pub members: Vec<GroupMemberDto>,
    pub folder_rules: Vec<FolderRuleDto>,
    pub app_rules: Vec<AppRuleDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupPage {
    pub items: Vec<GroupSummary>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRuleInput {
    pub share_name: String,
    #[serde(default)]
    pub rule: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRuleInput {
    pub app_id: String,
    #[serde(default)]
    pub rule: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub members: Option<Vec<i64>>,
    #[serde(default)]
    pub folder_rules: Option<Vec<FolderRuleInput>>,
    #[serde(default)]
    pub app_rules: Option<Vec<AppRuleInput>>,
}

#[derive(Debug, Deserialize)]
pub struct GroupListQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page_camel: Option<i64>,
    pub per_page: Option<i64>,
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetUserGroupsInput {
    #[serde(default)]
    pub group_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRulesInput {
    #[serde(default)]
    pub folder_rules: Vec<FolderRuleInput>,
    #[serde(default)]
    pub app_rules: Vec<AppRuleInput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRulesDto {
    pub folder_rules: Vec<FolderRuleDto>,
    pub app_rules: Vec<AppRuleDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserGroupsDto {
    pub group_ids: Vec<i64>,
    pub groups: Vec<GroupRef>,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

const MAX_GROUP_NAME: usize = 64;
const MAX_GROUP_DESCRIPTION: usize = 500;
const MAX_MEMBERS: usize = 2000;
const MAX_RULES: usize = 500;

pub fn validate_group_name(raw: &str) -> AppResult<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("group name is required"));
    }
    if name.chars().count() > MAX_GROUP_NAME {
        return Err(AppError::bad_request(
            "group name must be at most 64 characters",
        ));
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\' | '\0' | '\u{7f}'))
    {
        return Err(AppError::bad_request(
            "group name must not contain control characters or path separators",
        ));
    }
    Ok(name.to_string())
}

fn validate_description(raw: Option<&str>) -> AppResult<String> {
    let value = raw.unwrap_or("").trim().to_string();
    if value.chars().count() > MAX_GROUP_DESCRIPTION {
        return Err(AppError::bad_request(
            "group description must be at most 500 characters",
        ));
    }
    Ok(value)
}

async fn validate_members(pool: &sqlx::SqlitePool, members: &[i64]) -> AppResult<Vec<i64>> {
    if members.len() > MAX_MEMBERS {
        return Err(AppError::bad_request("too many group members"));
    }
    let mut unique = BTreeSet::new();
    for id in members {
        unique.insert(*id);
    }
    if unique.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = unique.into_iter().collect();
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM users WHERE id IN ({placeholders})");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in &ids {
        query = query.bind(id);
    }
    let found: Vec<i64> = query
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| row.get("id"))
        .collect();
    if found.len() != ids.len() {
        let missing: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| !found.contains(id))
            .collect();
        return Err(AppError::bad_request(format!(
            "unknown user id(s): {missing:?}"
        )));
    }
    Ok(ids)
}

async fn validate_folder_rules(
    pool: &sqlx::SqlitePool,
    rules: &[FolderRuleInput],
) -> AppResult<Vec<FolderRuleDto>> {
    if rules.len() > MAX_RULES {
        return Err(AppError::bad_request("too many shared-folder rules"));
    }
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for input in rules {
        let rule = FolderRule::parse(&input.rule).ok_or_else(|| {
            AppError::bad_request(format!(
                "invalid folder rule {:?}; expected inherit, none, read or write",
                input.rule
            ))
        })?;
        let share_name = input.share_name.trim().to_string();
        if share_name.is_empty() {
            return Err(AppError::bad_request("shareName is required"));
        }
        let exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM shares WHERE name = ? COLLATE NOCASE")
                .bind(&share_name)
                .fetch_optional(pool)
                .await?;
        let canonical = exists.ok_or_else(|| {
            AppError::bad_request(format!("unknown shared folder {share_name:?}"))
        })?;
        merged.insert(
            canonical.to_ascii_lowercase(),
            format!("{}:{}", canonical, rule.as_str()),
        );
    }
    Ok(merged
        .into_values()
        .map(|value| {
            let (share_name, rule) = value.split_once(':').unwrap_or((value.as_str(), ""));
            FolderRuleDto {
                share_name: share_name.to_string(),
                rule: rule.to_string(),
            }
        })
        .collect())
}

fn validate_app_rules(rules: &[AppRuleInput]) -> AppResult<Vec<AppRuleDto>> {
    if rules.len() > MAX_RULES {
        return Err(AppError::bad_request("too many application rules"));
    }
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for input in rules {
        let app_id = input.app_id.trim().to_ascii_lowercase();
        if !APP_IDS.contains(&app_id.as_str()) {
            return Err(AppError::bad_request(format!(
                "unknown application id {:?}",
                input.app_id
            )));
        }
        let rule = AppRule::parse(&input.rule).ok_or_else(|| {
            AppError::bad_request(format!(
                "invalid application rule {:?}; expected inherit, none or allow",
                input.rule
            ))
        })?;
        merged.insert(app_id, rule.as_str().to_string());
    }
    Ok(merged
        .into_iter()
        .map(|(app_id, rule)| AppRuleDto { app_id, rule })
        .collect())
}

fn describe_rules(folder_rules: &[FolderRuleDto], app_rules: &[AppRuleDto]) -> String {
    let mut parts = Vec::new();
    for rule in folder_rules {
        if rule.rule != "inherit" {
            parts.push(format!("{}={}", rule.share_name, rule.rule));
        }
    }
    for rule in app_rules {
        if rule.rule != "inherit" {
            parts.push(format!("app:{}={}", rule.app_id, rule.rule));
        }
    }
    if parts.is_empty() {
        "no explicit rules".to_string()
    } else {
        truncate_detail(&parts.join(", "))
    }
}

fn truncate_detail(value: &str) -> String {
    const MAX: usize = 900;
    if value.chars().count() <= MAX {
        return value.to_string();
    }
    let mut out: String = value.chars().take(MAX - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

fn publish_group_changed(state: &AppState, reason: &str, group_id: i64) {
    state.events.publish_admin(
        "group.changed",
        json!({"reason": reason, "groupId": group_id}),
    );
}

fn publish_access_changed(
    state: &AppState,
    user_ids: &[i64],
    reason: &str,
    group_id: Option<i64>,
    changed_by: Option<i64>,
) {
    const MAX_AUDIENCE: usize = 5000;
    let mut unique: BTreeSet<i64> = BTreeSet::new();
    for id in user_ids.iter().copied() {
        unique.insert(id);
        if unique.len() >= MAX_AUDIENCE {
            break;
        }
    }
    for user_id in unique {
        state.events.publish_user(
            "access.changed",
            user_id,
            json!({
                "reason": reason,
                "groupId": group_id,
                "userId": user_id,
                "changedBy": changed_by,
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// Loading helpers
// ---------------------------------------------------------------------------

async fn load_group_summary(pool: &sqlx::SqlitePool, id: i64) -> AppResult<GroupSummary> {
    let row = sqlx::query(
        "SELECT g.id, g.name, g.description, g.created_at, g.updated_at,
                (SELECT COUNT(*) FROM group_members m WHERE m.group_id = g.id) AS member_count,
                (SELECT COUNT(*) FROM group_folder_rules r WHERE r.group_id = g.id) AS folder_rule_count,
                (SELECT COUNT(*) FROM group_app_rules a WHERE a.group_id = g.id) AS app_rule_count
         FROM groups g WHERE g.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound("group not found"))?;
    Ok(summary_from_row(&row))
}

fn summary_from_row(row: &sqlx::sqlite::SqliteRow) -> GroupSummary {
    GroupSummary {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        member_count: row.get("member_count"),
        folder_rule_count: row.get("folder_rule_count"),
        app_rule_count: row.get("app_rule_count"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

async fn load_group_detail(pool: &sqlx::SqlitePool, id: i64) -> AppResult<GroupDetail> {
    let summary = load_group_summary(pool, id).await?;
    let members = sqlx::query(
        "SELECT u.id AS user_id, u.username, u.display_name
         FROM group_members m JOIN users u ON u.id = m.user_id
         WHERE m.group_id = ? ORDER BY u.username COLLATE NOCASE",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| GroupMemberDto {
        user_id: row.get("user_id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
    })
    .collect();
    let folder_rules = sqlx::query(
        "SELECT share_name, rule FROM group_folder_rules WHERE group_id = ? ORDER BY share_name COLLATE NOCASE",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| FolderRuleDto {
        share_name: row.get("share_name"),
        rule: row.get("rule"),
    })
    .collect();
    let app_rules =
        sqlx::query("SELECT app_id, rule FROM group_app_rules WHERE group_id = ? ORDER BY app_id")
            .bind(id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| AppRuleDto {
                app_id: row.get("app_id"),
                rule: row.get("rule"),
            })
            .collect();
    Ok(GroupDetail {
        summary,
        members,
        folder_rules,
        app_rules,
    })
}

// ---------------------------------------------------------------------------
// Groups CRUD
// ---------------------------------------------------------------------------

/// GET /api/groups
pub async fn list_groups(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<GroupListQuery>,
) -> AppResult<Json<GroupPage>> {
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query
        .per_page
        .or(query.per_page_camel)
        .unwrap_or(25)
        .clamp(1, 100);
    let offset = (page - 1) * per_page;

    let search = query.q.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let like = search.map(|value| format!("%{}%", value.replace('%', "\\%")));
    let base = "FROM groups g";
    let filter = "WHERE g.name LIKE ? ESCAPE '\\' OR g.description LIKE ? ESCAPE '\\'";

    let (rows, total) = if let Some(pattern) = like.as_ref() {
        let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) {base} {filter}"
        )))
        .bind(pattern)
        .bind(pattern)
        .fetch_one(&state.db)
        .await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT g.id, g.name, g.description, g.created_at, g.updated_at,
                    (SELECT COUNT(*) FROM group_members m WHERE m.group_id = g.id) AS member_count,
                    (SELECT COUNT(*) FROM group_folder_rules r WHERE r.group_id = g.id) AS folder_rule_count,
                    (SELECT COUNT(*) FROM group_app_rules a WHERE a.group_id = g.id) AS app_rule_count
             {base} {filter}
             ORDER BY g.name COLLATE NOCASE LIMIT ? OFFSET ?"
        )))
        .bind(pattern)
        .bind(pattern)
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (rows, total)
    } else {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM groups")
            .fetch_one(&state.db)
            .await?;
        let rows = sqlx::query(
            "SELECT g.id, g.name, g.description, g.created_at, g.updated_at,
                    (SELECT COUNT(*) FROM group_members m WHERE m.group_id = g.id) AS member_count,
                    (SELECT COUNT(*) FROM group_folder_rules r WHERE r.group_id = g.id) AS folder_rule_count,
                    (SELECT COUNT(*) FROM group_app_rules a WHERE a.group_id = g.id) AS app_rule_count
             FROM groups g ORDER BY g.name COLLATE NOCASE LIMIT ? OFFSET ?",
        )
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (rows, total)
    };

    Ok(Json(GroupPage {
        items: rows.iter().map(summary_from_row).collect(),
        page,
        per_page,
        total,
    }))
}

/// POST /api/groups
pub async fn create_group(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<GroupInput>,
) -> AppResult<Json<GroupDetail>> {
    let name = validate_group_name(&body.name)?;
    let description = validate_description(body.description.as_deref())?;
    let members = validate_members(&state.db, body.members.as_deref().unwrap_or(&[])).await?;
    let folder_rules =
        validate_folder_rules(&state.db, body.folder_rules.as_deref().unwrap_or(&[])).await?;
    let app_rules = validate_app_rules(body.app_rules.as_deref().unwrap_or(&[]))?;

    let now = db::now_ms();
    let mut tx = state.db.begin().await?;
    let result = sqlx::query(
        "INSERT INTO groups (name, description, created_at, updated_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&name)
    .bind(&description)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    let group_id = match result {
        Ok(outcome) => outcome.last_insert_rowid(),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(AppError::Conflict(format!(
                "a group named {name:?} already exists"
            )));
        }
        Err(error) => return Err(error.into()),
    };
    for user_id in &members {
        sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, ?)")
            .bind(group_id)
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    for rule in &folder_rules {
        sqlx::query("INSERT INTO group_folder_rules (group_id, share_name, rule) VALUES (?, ?, ?)")
            .bind(group_id)
            .bind(&rule.share_name)
            .bind(&rule.rule)
            .execute(&mut *tx)
            .await?;
    }
    for rule in &app_rules {
        sqlx::query("INSERT INTO group_app_rules (group_id, app_id, rule) VALUES (?, ?, ?)")
            .bind(group_id)
            .bind(&rule.app_id)
            .bind(&rule.rule)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    access::bump_generation();

    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("group.create")
            .actor(&admin.0)
            .object("group", group_id.to_string())
            .path(&name)
            .success()
            .detail(format!(
                "{} member(s); {}",
                members.len(),
                describe_rules(&folder_rules, &app_rules)
            ))
            .ip(ip.as_string()),
    )
    .await;
    publish_group_changed(&state, "created", group_id);
    publish_access_changed(
        &state,
        &members,
        "group_created",
        Some(group_id),
        Some(admin.0.id),
    );

    Ok(Json(load_group_detail(&state.db, group_id).await?))
}

/// GET /api/groups/{id}
pub async fn get_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupDetail>> {
    Ok(Json(load_group_detail(&state.db, id).await?))
}

/// PUT /api/groups/{id} — replace name/description/members/rules in one save.
pub async fn update_group(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<i64>,
    Json(body): Json<GroupInput>,
) -> AppResult<Json<GroupDetail>> {
    let existing = load_group_detail(&state.db, id).await?;
    let name = validate_group_name(&body.name)?;
    let description = validate_description(body.description.as_deref())?;
    let members = validate_members(&state.db, body.members.as_deref().unwrap_or(&[])).await?;
    let folder_rules =
        validate_folder_rules(&state.db, body.folder_rules.as_deref().unwrap_or(&[])).await?;
    let app_rules = validate_app_rules(body.app_rules.as_deref().unwrap_or(&[]))?;

    let old_members: Vec<i64> = existing.members.iter().map(|m| m.user_id).collect();
    let old_folders: Vec<FolderRuleDto> = existing.folder_rules.clone();
    let old_apps: Vec<AppRuleDto> = existing.app_rules.clone();
    let members_changed = sorted_i64(&old_members) != sorted_i64(&members);
    let folders_changed =
        normalized_folder_rules(&old_folders) != normalized_folder_rules(&folder_rules);
    let apps_changed = normalized_app_rules(&old_apps) != normalized_app_rules(&app_rules);
    let general_changed =
        existing.summary.name != name || existing.summary.description != description;

    let now = db::now_ms();
    let mut tx = state.db.begin().await?;
    let update =
        sqlx::query("UPDATE groups SET name = ?, description = ?, updated_at = ? WHERE id = ?")
            .bind(&name)
            .bind(&description)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await;
    match update {
        Ok(outcome) if outcome.rows_affected() == 0 => {
            return Err(AppError::NotFound("group not found"));
        }
        Ok(_) => {}
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(AppError::Conflict(format!(
                "a group named {name:?} already exists"
            )));
        }
        Err(error) => return Err(error.into()),
    }
    sqlx::query("DELETE FROM group_members WHERE group_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for user_id in &members {
        sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, ?)")
            .bind(id)
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM group_folder_rules WHERE group_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for rule in &folder_rules {
        sqlx::query("INSERT INTO group_folder_rules (group_id, share_name, rule) VALUES (?, ?, ?)")
            .bind(id)
            .bind(&rule.share_name)
            .bind(&rule.rule)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM group_app_rules WHERE group_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for rule in &app_rules {
        sqlx::query("INSERT INTO group_app_rules (group_id, app_id, rule) VALUES (?, ?, ?)")
            .bind(id)
            .bind(&rule.app_id)
            .bind(&rule.rule)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    access::bump_generation();

    let mut affected: Vec<i64> = old_members.clone();
    affected.extend(members.iter().copied());
    if general_changed {
        audit::record_with_events(
            &state.db,
            &state.events,
            AuditLog::new("group.update")
                .actor(&admin.0)
                .object("group", id.to_string())
                .path(&name)
                .success()
                .detail(format!("renamed from {:?}", existing.summary.name))
                .ip(ip.as_string()),
        )
        .await;
    }
    if members_changed {
        audit::record_with_events(
            &state.db,
            &state.events,
            AuditLog::new("group.members_update")
                .actor(&admin.0)
                .object("group", id.to_string())
                .path(&name)
                .success()
                .detail(truncate_detail(&format!(
                    "{} member(s) -> {} member(s)",
                    old_members.len(),
                    members.len()
                )))
                .ip(ip.as_string()),
        )
        .await;
    }
    if folders_changed || apps_changed {
        audit::record_with_events(
            &state.db,
            &state.events,
            AuditLog::new("group.permissions_update")
                .actor(&admin.0)
                .object("group", id.to_string())
                .path(&name)
                .success()
                .detail(describe_rules(&folder_rules, &app_rules))
                .ip(ip.as_string()),
        )
        .await;
    }
    if !general_changed && !members_changed && !folders_changed && !apps_changed {
        // Still bump the timestamp/return current state for a no-op save.
        audit::record_with_events(
            &state.db,
            &state.events,
            AuditLog::new("group.update")
                .actor(&admin.0)
                .object("group", id.to_string())
                .path(&name)
                .success()
                .detail("no changes")
                .ip(ip.as_string()),
        )
        .await;
    }
    publish_group_changed(&state, "updated", id);
    publish_access_changed(
        &state,
        &affected,
        "group_updated",
        Some(id),
        Some(admin.0.id),
    );
    // Other administrators editing the users table should also refresh counts.
    state.events.publish_admin(
        "user.changed",
        json!({"event": "groups_changed", "groupId": id}),
    );

    Ok(Json(load_group_detail(&state.db, id).await?))
}

/// DELETE /api/groups/{id} — removes memberships/rules, keeps users and files.
pub async fn delete_group(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let detail = load_group_detail(&state.db, id).await?;
    let affected_users: Vec<i64> = detail.members.iter().map(|m| m.user_id).collect();
    let affected_folder_rules = detail.folder_rules.len() as i64;
    let affected_app_rules = detail.app_rules.len() as i64;

    let mut tx = state.db.begin().await?;
    let deleted = sqlx::query("DELETE FROM groups WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound("group not found"));
    }
    tx.commit().await?;
    access::bump_generation();

    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("group.delete")
            .actor(&admin.0)
            .object("group", id.to_string())
            .path(&detail.summary.name)
            .success()
            .detail(format!(
                "affected users={}, folders={}, apps={}; users/files retained",
                affected_users.len(),
                affected_folder_rules,
                affected_app_rules
            ))
            .ip(ip.as_string()),
    )
    .await;
    publish_group_changed(&state, "deleted", id);
    publish_access_changed(
        &state,
        &affected_users,
        "group_deleted",
        Some(id),
        Some(admin.0.id),
    );
    state.events.publish_admin(
        "user.changed",
        json!({"event": "groups_changed", "groupId": id}),
    );

    Ok(Json(json!({
        "deleted": true,
        "groupId": id,
        "affectedUsers": affected_users.len(),
        "affectedFolderRules": affected_folder_rules,
        "affectedAppRules": affected_app_rules,
        "retained": ["users", "shared folders", "files"],
    })))
}

// ---------------------------------------------------------------------------
// User membership + direct rules
// ---------------------------------------------------------------------------

async fn ensure_user_exists(pool: &sqlx::SqlitePool, id: i64) -> AppResult<users::UserRecord> {
    users::find_by_id(pool, id)
        .await?
        .ok_or(AppError::NotFound("user not found"))
}

/// GET /api/users/{id}/groups
pub async fn get_user_groups(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<UserGroupsDto>> {
    ensure_user_exists(&state.db, id).await?;
    let groups = access::group_refs_for_user(&state.db, id).await?;
    Ok(Json(UserGroupsDto {
        group_ids: groups.iter().map(|g| g.id).collect(),
        groups,
    }))
}

/// PUT /api/users/{id}/groups — atomic membership replace.
pub async fn set_user_groups(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<i64>,
    Json(body): Json<SetUserGroupsInput>,
) -> AppResult<Json<UserGroupsDto>> {
    ensure_user_exists(&state.db, id).await?;
    if body.group_ids.len() > MAX_RULES {
        return Err(AppError::bad_request("too many groups for one user"));
    }
    let wanted: BTreeSet<i64> = body.group_ids.iter().copied().collect();
    let current: BTreeSet<i64> = access::group_refs_for_user(&state.db, id)
        .await?
        .into_iter()
        .map(|g| g.id)
        .collect();
    if wanted.is_empty() && !current.is_empty() {
        // Removing every group is allowed; nothing extra to validate.
    }
    if !wanted.is_empty() {
        let ids: Vec<i64> = wanted.iter().copied().collect();
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id, name FROM groups WHERE id IN ({placeholders})");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for group_id in &ids {
            query = query.bind(group_id);
        }
        let found: Vec<i64> = query
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|row| row.get("id"))
            .collect();
        if found.len() != ids.len() {
            return Err(AppError::bad_request("one or more groups do not exist"));
        }
    }

    let now = db::now_ms();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM group_members WHERE user_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for group_id in &wanted {
        sqlx::query("INSERT INTO group_members (group_id, user_id, added_at) VALUES (?, ?, ?)")
            .bind(group_id)
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    access::bump_generation();

    let groups = access::group_refs_for_user(&state.db, id).await?;
    let names = groups
        .iter()
        .map(|g| g.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("user.groups_update")
            .actor(&admin.0)
            .object("user", id.to_string())
            .success()
            .detail(truncate_detail(&format!(
                "groups=[{}]",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names
                }
            )))
            .ip(ip.as_string()),
    )
    .await;

    // Group membership counts changed for every old and new group.
    let mut changed_groups: BTreeSet<i64> = current;
    changed_groups.extend(wanted.iter().copied());
    for group_id in changed_groups {
        publish_group_changed(&state, "membership_updated", group_id);
    }
    state
        .events
        .publish_admin("user.changed", json!({"userId": id, "event": "updated"}));
    publish_access_changed(&state, &[id], "membership_changed", None, Some(admin.0.id));

    Ok(Json(UserGroupsDto {
        group_ids: groups.iter().map(|g| g.id).collect(),
        groups,
    }))
}

/// GET /api/users/{id}/access-rules — direct (user-level) overrides.
pub async fn get_user_access_rules(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<AccessRulesDto>> {
    ensure_user_exists(&state.db, id).await?;
    Ok(Json(load_direct_rules(&state.db, id).await?))
}

async fn load_direct_rules(pool: &sqlx::SqlitePool, user_id: i64) -> AppResult<AccessRulesDto> {
    let folder_rules = sqlx::query(
        "SELECT share_name, rule FROM user_folder_rules WHERE user_id = ? ORDER BY share_name COLLATE NOCASE",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| FolderRuleDto {
        share_name: row.get("share_name"),
        rule: row.get("rule"),
    })
    .collect();
    let app_rules =
        sqlx::query("SELECT app_id, rule FROM user_app_rules WHERE user_id = ? ORDER BY app_id")
            .bind(user_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| AppRuleDto {
                app_id: row.get("app_id"),
                rule: row.get("rule"),
            })
            .collect();
    Ok(AccessRulesDto {
        folder_rules,
        app_rules,
    })
}

/// PUT /api/users/{id}/access-rules — replace explicit user overrides.
pub async fn set_user_access_rules(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<i64>,
    Json(body): Json<AccessRulesInput>,
) -> AppResult<Json<AccessRulesDto>> {
    ensure_user_exists(&state.db, id).await?;
    let folder_rules = validate_folder_rules(&state.db, &body.folder_rules).await?;
    let app_rules = validate_app_rules(&body.app_rules)?;

    let now = db::now_ms();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM user_folder_rules WHERE user_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for rule in &folder_rules {
        sqlx::query(
            "INSERT INTO user_folder_rules (user_id, share_name, rule, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&rule.share_name)
        .bind(&rule.rule)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM user_app_rules WHERE user_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for rule in &app_rules {
        sqlx::query(
            "INSERT INTO user_app_rules (user_id, app_id, rule, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&rule.app_id)
        .bind(&rule.rule)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    access::bump_generation();

    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("user.access_update")
            .actor(&admin.0)
            .object("user", id.to_string())
            .success()
            .detail(describe_rules(&folder_rules, &app_rules))
            .ip(ip.as_string()),
    )
    .await;
    publish_access_changed(
        &state,
        &[id],
        "direct_rules_changed",
        None,
        Some(admin.0.id),
    );

    Ok(Json(load_direct_rules(&state.db, id).await?))
}

fn auth_user_from_record(record: &users::UserRecord) -> AuthUser {
    AuthUser {
        id: record.id,
        username: record.username.clone(),
        display_name: record.display_name.clone(),
        role: record.role,
        session_id: String::new(),
        quota_bytes: record.quota_bytes,
        must_change_password: record.must_change_password,
    }
}

/// GET /api/users/{id}/effective-access — admin explanation view.
pub async fn get_user_effective_access(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<access::EffectiveAccessDto>> {
    let record = ensure_user_exists(&state.db, id).await?;
    let user = auth_user_from_record(&record);
    Ok(Json(
        access::effective_access(&state.db, &user, true).await?,
    ))
}

/// GET /api/access/me — the caller's own effective access (launcher/help).
pub async fn my_effective_access(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<access::EffectiveAccessDto>> {
    Ok(Json(
        access::effective_access(&state.db, &user, false).await?,
    ))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn sorted_i64(values: &[i64]) -> Vec<i64> {
    let mut out = values.to_vec();
    out.sort_unstable();
    out
}

fn normalized_folder_rules(rules: &[FolderRuleDto]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = rules
        .iter()
        .map(|rule| (rule.share_name.to_ascii_lowercase(), rule.rule.clone()))
        .collect();
    out.sort();
    out
}

fn normalized_app_rules(rules: &[AppRuleDto]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = rules
        .iter()
        .map(|rule| (rule.app_id.clone(), rule.rule.clone()))
        .collect();
    out.sort();
    out
}

/// Helper for tests/UI: list of application ids with labels.
pub fn app_catalog() -> Vec<serde_json::Value> {
    APP_IDS
        .iter()
        .map(|id| json!({"id": id, "label": access::app_label(id)}))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_group_names() {
        assert_eq!(
            validate_group_name("  Project Editors ").unwrap(),
            "Project Editors"
        );
        assert!(validate_group_name("").is_err());
        assert!(validate_group_name("bad/name").is_err());
        assert!(validate_group_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn validates_app_rules() {
        let rules = vec![
            AppRuleInput {
                app_id: "files".into(),
                rule: "allow".into(),
            },
            AppRuleInput {
                app_id: "files".into(),
                rule: "none".into(),
            },
        ];
        let out = validate_app_rules(&rules).unwrap();
        assert_eq!(out.len(), 1, "duplicate app ids collapse");
        assert_eq!(out[0].rule, "none", "last rule wins");
        assert!(validate_app_rules(&[AppRuleInput {
            app_id: "admin-panel".into(),
            rule: "allow".into()
        }])
        .is_err());
    }

    #[test]
    fn normalizes_rule_order_for_change_detection() {
        let a = vec![
            FolderRuleDto {
                share_name: "Projects".into(),
                rule: "write".into(),
            },
            FolderRuleDto {
                share_name: "Finance".into(),
                rule: "none".into(),
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(normalized_folder_rules(&a), normalized_folder_rules(&b));
    }
}
