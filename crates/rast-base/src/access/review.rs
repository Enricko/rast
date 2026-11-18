//! Administrator access review: paginated Effective Access with conflict
//! explanations, group member listings and the impact of deleting a group.
//!
//! Every decision shown here comes from the same evaluator the API enforces
//! ([`super::folder_access_by_id`], [`super::app_access`]); this module only
//! filters, paginates and explains those results, it never re-implements the
//! precedence rules.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use super::{
    app_access, app_label, folder_access_by_id, group_refs_for_user, AppAccessDto, EffectiveUser,
    GroupRef, RuleSource, APP_IDS,
};
use crate::{auth::AuthUser, error::AppResult, storage::Perm};

/// Maximum folders per Effective Access page.
pub const MAX_PER_PAGE: i64 = 100;
/// Default folders per Effective Access page.
pub const DEFAULT_PER_PAGE: i64 = 25;
/// Maximum users evaluated for a group-deletion impact preview.
pub const IMPACT_USER_LIMIT: usize = 50;

/// Level filter for the Effective Access folder list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelFilter {
    #[default]
    All,
    None,
    Read,
    Write,
    Admin,
}

impl LevelFilter {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("none") | Some("no-access") => Self::None,
            Some("read") => Self::Read,
            Some("write") => Self::Write,
            Some("admin") => Self::Admin,
            _ => Self::All,
        }
    }

    fn matches(self, level: Option<Perm>) -> bool {
        match self {
            Self::All => true,
            Self::None => level.is_none(),
            Self::Read => level == Some(Perm::Read),
            Self::Write => level == Some(Perm::Write),
            Self::Admin => level == Some(Perm::Admin),
        }
    }
}

/// Why an explicit No Access won over grants for the same folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    /// Rules that denied access (`none`).
    pub denied_by: Vec<RuleSource>,
    /// Grants (`read`/`write`) that were overridden by the deny.
    pub overridden_grants: Vec<RuleSource>,
}

/// One folder row of the paginated Effective Access view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderReviewDto {
    pub share_id: String,
    pub share_name: String,
    pub path: String,
    /// `none`, `read`, `write` or `admin`.
    pub level: String,
    pub sources: Vec<RuleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
}

/// Paginated Effective Access response (superset of the v0.2.0 shape).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveAccessPage {
    pub user: EffectiveUser,
    pub groups: Vec<GroupRef>,
    pub folders: Vec<FolderReviewDto>,
    /// Folders matching the search/level filter (before pagination).
    pub folders_total: i64,
    /// All shared folders known to the server.
    pub folders_all: i64,
    pub page: i64,
    pub per_page: i64,
    pub apps: Vec<AppAccessDto>,
    /// Folder level counts over the search result (ignores the level filter)
    /// so the UI can show filter badges.
    pub level_counts: BTreeMap<String, i64>,
}

/// Query options for [`effective_access_page`].
#[derive(Debug, Clone, Default)]
pub struct ReviewQuery {
    pub search: Option<String>,
    pub level: LevelFilter,
    pub page: i64,
    pub per_page: i64,
    /// `true` for the admin editor (No Access rows included); `false` for the
    /// caller's own view, which never reveals hidden folder names.
    pub include_hidden: bool,
}

pub fn level_label(level: Option<Perm>) -> &'static str {
    match level {
        None => "none",
        Some(Perm::Read) => "read",
        Some(Perm::Write) => "write",
        Some(Perm::Admin) => "admin",
    }
}

/// Split the sources of a denied folder into deny rules and overridden grants.
pub fn explain_conflict(level: Option<Perm>, sources: &[RuleSource]) -> Option<Conflict> {
    if level.is_some() {
        return None;
    }
    let denied_by: Vec<RuleSource> = sources
        .iter()
        .filter(|source| source.rule == "none")
        .cloned()
        .collect();
    let overridden_grants: Vec<RuleSource> = sources
        .iter()
        .filter(|source| source.rule == "read" || source.rule == "write")
        .cloned()
        .collect();
    if denied_by.is_empty() || overridden_grants.is_empty() {
        return None;
    }
    Some(Conflict {
        denied_by,
        overridden_grants,
    })
}

fn search_matches(name: &str, search: Option<&str>) -> bool {
    match search {
        None => true,
        Some(needle) => name.to_lowercase().contains(&needle.to_lowercase()),
    }
}

/// Effective Access with search, level filter and pagination. Every folder is
/// evaluated with the enforcement evaluator (cached per account), so the rows
/// always match what the API would allow.
pub async fn effective_access_page(
    pool: &SqlitePool,
    user: &AuthUser,
    query: &ReviewQuery,
) -> AppResult<EffectiveAccessPage> {
    let per_page = if query.per_page <= 0 {
        DEFAULT_PER_PAGE
    } else {
        query.per_page.min(MAX_PER_PAGE)
    };
    let page = query.page.max(1);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let groups = group_refs_for_user(pool, user.id).await?;
    let share_rows = sqlx::query("SELECT id, name FROM shares ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    let folders_all = share_rows.len() as i64;

    let mut level_counts: BTreeMap<String, i64> = ["none", "read", "write", "admin"]
        .into_iter()
        .map(|label| (label.to_string(), 0))
        .collect();
    let mut matched: Vec<FolderReviewDto> = Vec::new();
    let mut folders_total = 0_i64;
    let skip = (page - 1) * per_page;
    for row in &share_rows {
        let share_id: String = row.get("id");
        let share_name: String = row.get("name");
        if !search_matches(&share_name, search) {
            continue;
        }
        let access = folder_access_by_id(pool, user, &share_id).await?;
        if !query.include_hidden && access.level.is_none() {
            continue;
        }
        *level_counts
            .entry(level_label(access.level).to_string())
            .or_default() += 1;
        if !query.level.matches(access.level) {
            continue;
        }
        folders_total += 1;
        if folders_total <= skip || matched.len() as i64 >= per_page {
            continue;
        }
        let conflict = explain_conflict(access.level, &access.sources);
        matched.push(FolderReviewDto {
            path: format!("/shared/{}", access.share_name),
            share_id: access.share_id,
            share_name: access.share_name,
            level: level_label(access.level).to_string(),
            sources: access.sources,
            fallback: access.fallback,
            conflict,
        });
    }

    let mut apps = Vec::with_capacity(APP_IDS.len());
    for app_id in APP_IDS {
        let access = app_access(pool, user, app_id).await?;
        apps.push(AppAccessDto {
            app_id: access.app_id,
            label: app_label(app_id).to_string(),
            allowed: access.allowed,
            read_only: access.read_only,
            sources: access.sources,
            fallback: access.fallback,
        });
    }

    Ok(EffectiveAccessPage {
        user: EffectiveUser {
            id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            role: if user.is_admin() { "admin" } else { "user" }.to_string(),
        },
        groups,
        folders: matched,
        folders_total,
        folders_all,
        page,
        per_page,
        apps,
        level_counts,
    })
}

// ---------------------------------------------------------------------------
// Group members
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemberDto {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMembersPage {
    pub items: Vec<GroupMemberDto>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
    /// Members whose account is deactivated (always counted over the whole
    /// group so administrators can find them).
    pub inactive_total: i64,
}

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

/// Paginated, searchable member list of one group (active and deactivated).
pub async fn group_members_page(
    pool: &SqlitePool,
    group_id: i64,
    search: Option<&str>,
    page: i64,
    per_page: i64,
) -> AppResult<GroupMembersPage> {
    let per_page = if per_page <= 0 {
        DEFAULT_PER_PAGE
    } else {
        per_page.min(MAX_PER_PAGE)
    };
    let page = page.max(1);
    let pattern = search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(like_pattern);
    let filter = if pattern.is_some() {
        " AND (u.username LIKE ? ESCAPE '\\' OR LOWER(u.display_name) LIKE LOWER(?) ESCAPE '\\')"
    } else {
        ""
    };

    let count_sql = format!(
        "SELECT COUNT(*) FROM group_members gm JOIN users u ON u.id = gm.user_id
         WHERE gm.group_id = ?{filter}"
    );
    let mut count = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql)).bind(group_id);
    if let Some(pattern) = &pattern {
        count = count.bind(pattern.clone()).bind(pattern.clone());
    }
    let total = count.fetch_one(pool).await?;

    let inactive_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM group_members gm JOIN users u ON u.id = gm.user_id
         WHERE gm.group_id = ? AND u.active = 0",
    )
    .bind(group_id)
    .fetch_one(pool)
    .await?;

    let rows_sql = format!(
        "SELECT u.id, u.username, u.display_name, u.role, u.active
         FROM group_members gm JOIN users u ON u.id = gm.user_id
         WHERE gm.group_id = ?{filter}
         ORDER BY u.active DESC, u.username COLLATE NOCASE
         LIMIT ? OFFSET ?"
    );
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(rows_sql)).bind(group_id);
    if let Some(pattern) = &pattern {
        rows = rows.bind(pattern.clone()).bind(pattern.clone());
    }
    let rows = rows
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(pool)
        .await?;
    let items = rows
        .into_iter()
        .map(|row| GroupMemberDto {
            user_id: row.get("id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
            role: row.get("role"),
            active: row.get::<bool, _>("active"),
        })
        .collect();
    Ok(GroupMembersPage {
        items,
        page,
        per_page,
        total,
        inactive_total,
    })
}

// ---------------------------------------------------------------------------
// Group deletion impact
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessChange {
    /// `folder` or `app`.
    pub kind: String,
    /// Share id or application id.
    pub resource_id: String,
    /// Display name (share name or application label).
    pub resource: String,
    /// Folder: `none|read|write|admin`; app: `none|read-only|allowed`.
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserImpact {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub active: bool,
    pub changes: Vec<AccessChange>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupImpact {
    pub group_id: i64,
    pub group_name: String,
    pub members: i64,
    pub folder_rules: i64,
    pub app_rules: i64,
    /// Members whose effective access changes (bounded).
    pub users: Vec<UserImpact>,
    /// Members evaluated (≤ [`IMPACT_USER_LIMIT`]).
    pub evaluated: i64,
    /// `true` when the group has more members than were evaluated.
    pub truncated: bool,
    /// Accounts, shared folders and files are never deleted with a group.
    pub retained: Vec<&'static str>,
}

fn app_state_label(allowed: bool, read_only: bool) -> &'static str {
    match (allowed, read_only) {
        (false, _) => "none",
        (true, true) => "read-only",
        (true, false) => "allowed",
    }
}

/// Compute the access each member would lose or gain if `group_id` were
/// deleted. "Before" is the enforcement evaluator's decision; "after" applies
/// the same precedence ([`super::merge_folder`] / [`super::merge_app`]) to the
/// committed rules with this group's memberships and rules excluded — exactly
/// what deletion removes. The preview is read-only (no write transaction, no
/// SQLite write lock).
pub async fn group_deletion_impact(pool: &SqlitePool, group_id: i64) -> AppResult<GroupImpact> {
    let group_name: String = sqlx::query_scalar("SELECT name FROM groups WHERE id = ?")
        .bind(group_id)
        .fetch_optional(pool)
        .await?
        .ok_or(crate::error::AppError::NotFound("group not found"))?;
    let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = ?")
        .bind(group_id)
        .fetch_one(pool)
        .await?;
    let folder_rules: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_folder_rules WHERE group_id = ?")
            .bind(group_id)
            .fetch_one(pool)
            .await?;
    let app_rules: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_app_rules WHERE group_id = ?")
            .bind(group_id)
            .fetch_one(pool)
            .await?;

    let member_rows = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.active, u.quota_bytes,
                u.must_change_password
         FROM group_members gm JOIN users u ON u.id = gm.user_id
         WHERE gm.group_id = ?
         ORDER BY u.active DESC, u.username COLLATE NOCASE
         LIMIT ?",
    )
    .bind(group_id)
    .bind(IMPACT_USER_LIMIT as i64)
    .fetch_all(pool)
    .await?;
    let accounts: Vec<(AuthUser, bool)> = member_rows
        .iter()
        .map(|row| {
            let role: crate::auth::Role = row.get("role");
            (
                AuthUser {
                    id: row.get("id"),
                    username: row.get("username"),
                    display_name: row.get("display_name"),
                    role,
                    session_id: String::new(),
                    quota_bytes: row.get("quota_bytes"),
                    must_change_password: row.get("must_change_password"),
                },
                row.get::<bool, _>("active"),
            )
        })
        .collect();

    // Resources this group has rules for (only those can change).
    let folder_targets: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.id, s.name FROM group_folder_rules r JOIN shares s ON s.id = r.share_id
         WHERE r.group_id = ? AND r.rule <> 'inherit' ORDER BY s.name COLLATE NOCASE",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    let app_targets: Vec<String> = sqlx::query_scalar(
        "SELECT app_id FROM group_app_rules WHERE group_id = ? AND rule <> 'inherit' ORDER BY app_id",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;

    let before = snapshot(pool, &accounts, &folder_targets, &app_targets).await?;

    let mut after = Vec::with_capacity(accounts.len());
    for (user, _) in &accounts {
        let mut folders = Vec::with_capacity(folder_targets.len());
        for (share_id, _) in &folder_targets {
            folders.push(evaluate_folder_without_group(pool, user, share_id, group_id).await?);
        }
        let mut apps = Vec::with_capacity(app_targets.len());
        for app_id in &app_targets {
            apps.push(evaluate_app_without_group(pool, user, app_id, group_id).await?);
        }
        after.push((folders, apps));
    }

    let mut users = Vec::new();
    for (index, (user, active)) in accounts.iter().enumerate() {
        let (before_folders, before_apps) = &before[index];
        let (after_folders, after_apps) = &after[index];
        let mut changes = Vec::new();
        for (slot, (share_id, share_name)) in folder_targets.iter().enumerate() {
            let was = level_label(before_folders[slot]);
            let now = level_label(after_folders[slot]);
            if was != now {
                changes.push(AccessChange {
                    kind: "folder".into(),
                    resource_id: share_id.clone(),
                    resource: share_name.clone(),
                    before: was.into(),
                    after: now.into(),
                });
            }
        }
        for (slot, app_id) in app_targets.iter().enumerate() {
            let was = before_apps[slot];
            let now = after_apps[slot];
            if was != now {
                changes.push(AccessChange {
                    kind: "app".into(),
                    resource_id: app_id.clone(),
                    resource: app_label(app_id).to_string(),
                    before: was.into(),
                    after: now.into(),
                });
            }
        }
        if !changes.is_empty() {
            users.push(UserImpact {
                user_id: user.id,
                username: user.username.clone(),
                display_name: user.display_name.clone(),
                active: *active,
                changes,
            });
        }
    }

    Ok(GroupImpact {
        group_id,
        group_name,
        members,
        folder_rules,
        app_rules,
        evaluated: accounts.len() as i64,
        truncated: members > accounts.len() as i64,
        users,
        retained: vec!["users", "sharedFolders", "files"],
    })
}

type Snapshot = Vec<(Vec<Option<Perm>>, Vec<&'static str>)>;

async fn snapshot(
    pool: &SqlitePool,
    accounts: &[(AuthUser, bool)],
    folders: &[(String, String)],
    apps: &[String],
) -> AppResult<Snapshot> {
    let mut out = Vec::with_capacity(accounts.len());
    for (user, _) in accounts {
        let mut levels = Vec::with_capacity(folders.len());
        for (share_id, _) in folders {
            levels.push(folder_access_by_id(pool, user, share_id).await?.level);
        }
        let mut states = Vec::with_capacity(apps.len());
        for app_id in apps {
            let access = app_access(pool, user, app_id).await?;
            states.push(app_state_label(access.allowed, access.read_only));
        }
        out.push((levels, states));
    }
    Ok(out)
}

/// Folder decision with one group's memberships/rules excluded, using the
/// evaluator's precedence ([`super::merge_folder`]).
async fn evaluate_folder_without_group(
    pool: &SqlitePool,
    user: &AuthUser,
    share_id: &str,
    excluded_group: i64,
) -> AppResult<Option<Perm>> {
    if user.is_admin() {
        return Ok(Some(Perm::Admin));
    }
    let direct: Option<String> =
        sqlx::query_scalar("SELECT rule FROM user_folder_rules WHERE user_id = ? AND share_id = ?")
            .bind(user.id)
            .bind(share_id)
            .fetch_optional(pool)
            .await?;
    let group_rules: Vec<String> = sqlx::query_scalar(
        "SELECT r.rule FROM group_members gm
         JOIN group_folder_rules r ON r.group_id = gm.group_id
         WHERE gm.user_id = ? AND r.share_id = ? AND gm.group_id <> ?",
    )
    .bind(user.id)
    .bind(share_id)
    .bind(excluded_group)
    .fetch_all(pool)
    .await?;
    let legacy: Option<String> =
        sqlx::query_scalar("SELECT perms FROM share_members WHERE share_id = ? AND user_id = ?")
            .bind(share_id)
            .bind(user.id)
            .fetch_optional(pool)
            .await?;
    Ok(super::merge_folder(
        direct.as_deref(),
        group_rules.iter().map(String::as_str),
        legacy.as_deref(),
    ))
}

/// Application decision with one group's memberships/rules excluded
/// ([`super::merge_app`]).
async fn evaluate_app_without_group(
    pool: &SqlitePool,
    user: &AuthUser,
    app_id: &str,
    excluded_group: i64,
) -> AppResult<&'static str> {
    if user.is_admin() {
        return Ok("allowed");
    }
    let direct: Option<String> =
        sqlx::query_scalar("SELECT rule FROM user_app_rules WHERE user_id = ? AND app_id = ?")
            .bind(user.id)
            .bind(app_id)
            .fetch_optional(pool)
            .await?;
    let group_rules: Vec<String> = sqlx::query_scalar(
        "SELECT r.rule FROM group_members gm
         JOIN group_app_rules r ON r.group_id = gm.group_id
         WHERE gm.user_id = ? AND r.app_id = ? AND gm.group_id <> ?",
    )
    .bind(user.id)
    .bind(app_id)
    .bind(excluded_group)
    .fetch_all(pool)
    .await?;
    let (allowed, read_only) = super::merge_app(
        app_id,
        direct.as_deref(),
        group_rules.iter().map(String::as_str),
    );
    Ok(app_state_label(allowed, read_only))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(kind: &str, group: Option<(&str, i64)>, rule: &str) -> RuleSource {
        RuleSource {
            kind: kind.into(),
            group_id: group.map(|(_, id)| id),
            group_name: group.map(|(name, _)| name.to_string()),
            rule: rule.into(),
        }
    }

    #[test]
    fn conflict_lists_deny_and_overridden_grants() {
        let sources = vec![
            source("group", Some(("Project Editors", 1)), "write"),
            source("direct", None, "none"),
            source("group", Some(("Project Viewers", 2)), "read"),
        ];
        let conflict = explain_conflict(None, &sources).expect("conflict");
        assert_eq!(conflict.denied_by.len(), 1);
        assert_eq!(conflict.denied_by[0].kind, "direct");
        assert_eq!(conflict.overridden_grants.len(), 2);
        // No conflict when access is granted or when nothing was overridden.
        assert!(explain_conflict(Some(Perm::Write), &sources).is_none());
        assert!(explain_conflict(None, &[source("direct", None, "none")]).is_none());
    }

    #[test]
    fn level_filter_parses_and_matches() {
        assert_eq!(LevelFilter::parse(Some("no-access")), LevelFilter::None);
        assert_eq!(LevelFilter::parse(Some("WRITE")), LevelFilter::Write);
        assert_eq!(LevelFilter::parse(None), LevelFilter::All);
        assert!(LevelFilter::None.matches(None));
        assert!(!LevelFilter::Read.matches(Some(Perm::Write)));
        assert!(LevelFilter::All.matches(Some(Perm::Admin)));
    }
}
