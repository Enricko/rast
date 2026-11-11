//! Group-based access policy: one evaluator used by API enforcement and the
//! Effective Access admin view.
//!
//! Precedence (documented in `docs/GROUPS.md`):
//! 1. explicit `none` from the user or any of their groups wins;
//! 2. otherwise the strongest grant wins (`write` includes `read`);
//! 3. with no grant the legacy ACL (`share_members`) decides, so
//!    upgrading installations keep exactly the same effective access;
//! 4. administrators bypass folder policy; personal homes keep ownership rules.
//!
//! Results are cached in-process and invalidated by [`bump_generation`] after
//! every committed membership/rule/share change.

pub mod middleware;
pub mod review;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock, RwLock,
    },
};

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::{
    auth::{AuthUser, Role},
    error::{AppError, AppResult},
    storage::Perm,
};

/// Stable application ids offered by the group policy.
pub const APP_IDS: &[&str] = &[
    "files",
    "applications",
    "monitor",
    "backup",
    "packages",
    "logs",
    "controlpanel",
];

pub fn app_label(app_id: &str) -> &'static str {
    match app_id {
        "files" => "File Station",
        "applications" => "Applications",
        "monitor" => "Resource Monitor",
        "backup" => "Backup Center",
        "packages" => "Package Center",
        "logs" => "Activity Logs",
        "controlpanel" => "Control Panel",
        _ => "Unknown application",
    }
}

/// Applications allowed to every authenticated user by default.
fn app_default_allowed(app_id: &str) -> bool {
    matches!(app_id, "files" | "monitor" | "logs" | "controlpanel")
}

fn app_default_read_only(_app_id: &str) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderRule {
    Inherit,
    None,
    Read,
    Write,
}

impl FolderRule {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "inherit" => Some(Self::Inherit),
            "none" => Some(Self::None),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppRule {
    Inherit,
    None,
    Allow,
}

impl AppRule {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "inherit" => Some(Self::Inherit),
            "none" => Some(Self::None),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::None => "none",
            Self::Allow => "allow",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSource {
    /// `direct` (user-level rule) or `group`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    pub rule: String,
}

fn direct_source(rule: &str) -> RuleSource {
    RuleSource {
        kind: "direct".into(),
        group_id: None,
        group_name: None,
        rule: rule.to_string(),
    }
}

fn group_source(group_id: i64, group_name: &str, rule: &str) -> RuleSource {
    RuleSource {
        kind: "group".into(),
        group_id: Some(group_id),
        group_name: Some(group_name.to_string()),
        rule: rule.to_string(),
    }
}

/// Result of evaluating folder access for one account.
#[derive(Debug, Clone)]
pub struct FolderAccess {
    pub share_id: String,
    pub share_name: String,
    /// `None` means No Access; otherwise the granted read/write/admin level.
    pub level: Option<Perm>,
    pub sources: Vec<RuleSource>,
    /// `admin`, `legacy-acl` (a legacy share membership decided), `explicit`
    /// (an explicit deny decided) or `none` (no rule or membership grants
    /// access).
    pub fallback: Option<String>,
}

impl FolderAccess {
    pub fn allowed(&self) -> bool {
        self.level.is_some()
    }
}

/// Result of evaluating application access for one account.
#[derive(Debug, Clone)]
pub struct AppAccess {
    pub app_id: String,
    pub allowed: bool,
    /// Group grants for non-administrators are read-only for
    /// Applications/Backup; this flag drives UI and mutation guards.
    pub read_only: bool,
    pub sources: Vec<RuleSource>,
    /// `admin`, `default`, `explicit` or `none`.
    pub fallback: String,
}

// ---------------------------------------------------------------------------
// Decision cache
// ---------------------------------------------------------------------------

static GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
enum Cached {
    Folder(FolderAccess),
    App(AppAccess),
}

type CacheMap = HashMap<(i64, String), (u64, Cached)>;

fn cache() -> &'static RwLock<CacheMap> {
    static CACHE: OnceLock<RwLock<CacheMap>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Invalidate every cached decision. Call after a policy, membership or share
/// change commits (the same process serves API and SSE, so this is immediate).
pub fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut map) = cache().write() {
        map.clear();
    }
}

fn current_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

fn cache_get(user_id: i64, key: &str) -> Option<Cached> {
    let guard = cache().read().ok()?;
    let (generation, value) = guard.get(&(user_id, key.to_string()))?;
    if *generation == current_generation() {
        Some(value.clone())
    } else {
        None
    }
}

/// Cache a decision under the generation observed *before* its inputs were
/// read. An evaluation that straddles a policy change (`bump_generation`)
/// read pre-change rules; stamping it with the newer generation would serve
/// that stale decision - e.g. a just-revoked grant - until the next change.
/// Stale results are therefore dropped, and one that races past the check
/// keeps its old generation, which `cache_get` never matches.
fn cache_put(user_id: i64, key: &str, read_at: u64, value: Cached) {
    if read_at != current_generation() {
        return;
    }
    if let Ok(mut map) = cache().write() {
        // Bound the cache; policy changes invalidate it anyway.
        if map.len() > 4096 {
            map.clear();
        }
        map.insert((user_id, key.to_string()), (read_at, value));
    }
}

fn folder_cache_key(share_id: &str) -> String {
    format!("folder:{share_id}")
}

fn app_cache_key(app_id: &str) -> String {
    format!("app:{app_id}")
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Evaluate access to a shared folder by display name.
pub async fn folder_access_by_name(
    pool: &SqlitePool,
    user: &AuthUser,
    share_name: &str,
) -> AppResult<FolderAccess> {
    if user.is_admin() {
        let row = sqlx::query("SELECT id, name FROM shares WHERE name = ?")
            .bind(share_name)
            .fetch_optional(pool)
            .await?
            .ok_or(AppError::NotFound("path not found"))?;
        return Ok(FolderAccess {
            share_id: row.get("id"),
            share_name: row.get::<String, _>("name"),
            level: Some(Perm::Admin),
            sources: Vec::new(),
            fallback: Some("admin".into()),
        });
    }
    let read_at = current_generation();
    let row = sqlx::query("SELECT id, name FROM shares WHERE name = ?")
        .bind(share_name)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound("path not found"))?;
    let share_id: String = row.get("id");
    let key = folder_cache_key(&share_id);
    if let Some(Cached::Folder(access)) = cache_get(user.id, &key) {
        return Ok(access);
    }
    let access = compute_folder(pool, user, share_id, row.get::<String, _>("name")).await?;
    cache_put(user.id, &key, read_at, Cached::Folder(access.clone()));
    Ok(access)
}

/// Evaluate access to a shared folder by id.
pub async fn folder_access_by_id(
    pool: &SqlitePool,
    user: &AuthUser,
    share_id: &str,
) -> AppResult<FolderAccess> {
    if user.is_admin() {
        let row = sqlx::query("SELECT id, name FROM shares WHERE id = ?")
            .bind(share_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AppError::NotFound("path not found"))?;
        return Ok(FolderAccess {
            share_id: row.get("id"),
            share_name: row.get::<String, _>("name"),
            level: Some(Perm::Admin),
            sources: Vec::new(),
            fallback: Some("admin".into()),
        });
    }
    let read_at = current_generation();
    let row = sqlx::query("SELECT id, name FROM shares WHERE id = ?")
        .bind(share_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound("path not found"))?;
    let share_id: String = row.get("id");
    let key = folder_cache_key(&share_id);
    if let Some(Cached::Folder(access)) = cache_get(user.id, &key) {
        return Ok(access);
    }
    let access = compute_folder(pool, user, share_id, row.get::<String, _>("name")).await?;
    cache_put(user.id, &key, read_at, Cached::Folder(access.clone()));
    Ok(access)
}

async fn compute_folder(
    pool: &SqlitePool,
    user: &AuthUser,
    share_id: String,
    share_name: String,
) -> AppResult<FolderAccess> {
    if user.is_admin() {
        return Ok(FolderAccess {
            share_id,
            share_name,
            level: Some(Perm::Admin),
            sources: Vec::new(),
            fallback: Some("admin".into()),
        });
    }

    let direct: Option<String> =
        sqlx::query_scalar("SELECT rule FROM user_folder_rules WHERE user_id = ? AND share_id = ?")
            .bind(user.id)
            .bind(&share_id)
            .fetch_optional(pool)
            .await?;
    let group_rows = sqlx::query(
        "SELECT g.id AS group_id, g.name AS group_name, r.rule AS rule
         FROM group_members gm
         JOIN groups g ON g.id = gm.group_id
         JOIN group_folder_rules r ON r.group_id = g.id
         WHERE gm.user_id = ? AND r.share_id = ?
         ORDER BY g.name COLLATE NOCASE",
    )
    .bind(user.id)
    .bind(&share_id)
    .fetch_all(pool)
    .await?;

    let mut sources = Vec::new();
    let mut explicit_deny = false;
    let mut granted: Option<FolderRule> = None;
    if let Some(rule) = direct.as_deref() {
        if let Some(parsed) = FolderRule::parse(rule) {
            if parsed != FolderRule::Inherit {
                sources.push(direct_source(parsed.as_str()));
                match parsed {
                    FolderRule::None => explicit_deny = true,
                    other => {
                        if granted != Some(FolderRule::Write) {
                            granted = Some(other);
                        }
                    }
                }
            }
        }
    }
    for row in &group_rows {
        let rule: String = row.get("rule");
        let Some(parsed) = FolderRule::parse(&rule) else {
            continue;
        };
        if parsed == FolderRule::Inherit {
            continue;
        }
        sources.push(group_source(
            row.get("group_id"),
            row.get::<String, _>("group_name").as_str(),
            parsed.as_str(),
        ));
        match parsed {
            FolderRule::None => explicit_deny = true,
            other => {
                if granted != Some(FolderRule::Write) {
                    granted = match (granted, other) {
                        (Some(FolderRule::Write), _) => Some(FolderRule::Write),
                        (_, FolderRule::Write) => Some(FolderRule::Write),
                        _ => Some(FolderRule::Read),
                    };
                }
            }
        }
    }

    if explicit_deny {
        return Ok(FolderAccess {
            share_id,
            share_name,
            level: None,
            sources,
            fallback: Some("explicit".into()),
        });
    }
    if let Some(rule) = granted {
        let level = match rule {
            FolderRule::Write => Perm::Write,
            _ => Perm::Read,
        };
        return Ok(FolderAccess {
            share_id,
            share_name,
            level: Some(level),
            sources,
            fallback: None,
        });
    }

    // No explicit grant: preserve the pre-groups behaviour (legacy ACL only —
    // share creators get no implicit access; admins already bypass above).
    let legacy: Option<String> =
        sqlx::query_scalar("SELECT perms FROM share_members WHERE share_id = ? AND user_id = ?")
            .bind(&share_id)
            .bind(user.id)
            .fetch_optional(pool)
            .await?;
    let level = legacy.as_deref().and_then(Perm::parse);
    // `legacy-acl` only when a legacy membership actually decided; otherwise
    // nothing granted access (`none`), which the UI explains as "No grant".
    let fallback = if level.is_some() {
        "legacy-acl"
    } else {
        "none"
    };
    Ok(FolderAccess {
        share_id,
        share_name,
        level,
        sources,
        fallback: Some(fallback.into()),
    })
}

/// Evaluate application access for one account.
pub async fn app_access(pool: &SqlitePool, user: &AuthUser, app_id: &str) -> AppResult<AppAccess> {
    if !APP_IDS.contains(&app_id) {
        return Err(AppError::bad_request(format!(
            "unknown application {app_id:?}"
        )));
    }
    if user.is_admin() {
        return Ok(AppAccess {
            app_id: app_id.to_string(),
            allowed: true,
            read_only: false,
            sources: Vec::new(),
            fallback: "admin".into(),
        });
    }
    let key = app_cache_key(app_id);
    let read_at = current_generation();
    if let Some(Cached::App(access)) = cache_get(user.id, &key) {
        return Ok(access);
    }

    let access = if user.is_admin() {
        AppAccess {
            app_id: app_id.to_string(),
            allowed: true,
            read_only: false,
            sources: Vec::new(),
            fallback: "admin".into(),
        }
    } else {
        let direct: Option<String> =
            sqlx::query_scalar("SELECT rule FROM user_app_rules WHERE user_id = ? AND app_id = ?")
                .bind(user.id)
                .bind(app_id)
                .fetch_optional(pool)
                .await?;
        let group_rows = sqlx::query(
            "SELECT g.id AS group_id, g.name AS group_name, r.rule AS rule
             FROM group_members gm
             JOIN groups g ON g.id = gm.group_id
             JOIN group_app_rules r ON r.group_id = g.id
             WHERE gm.user_id = ? AND r.app_id = ?
             ORDER BY g.name COLLATE NOCASE",
        )
        .bind(user.id)
        .bind(app_id)
        .fetch_all(pool)
        .await?;

        let mut sources = Vec::new();
        let mut explicit_deny = false;
        let mut explicit_allow = false;
        for row in &group_rows {
            let rule: String = row.get("rule");
            let Some(parsed) = AppRule::parse(&rule) else {
                continue;
            };
            if parsed == AppRule::Inherit {
                continue;
            }
            sources.push(group_source(
                row.get("group_id"),
                row.get::<String, _>("group_name").as_str(),
                parsed.as_str(),
            ));
            match parsed {
                AppRule::None => explicit_deny = true,
                AppRule::Allow => explicit_allow = true,
                AppRule::Inherit => {}
            }
        }
        if let Some(rule) = direct.as_deref() {
            if let Some(parsed) = AppRule::parse(rule) {
                if parsed != AppRule::Inherit {
                    sources.push(direct_source(parsed.as_str()));
                    match parsed {
                        AppRule::None => explicit_deny = true,
                        AppRule::Allow => explicit_allow = true,
                        AppRule::Inherit => {}
                    }
                }
            }
        }

        if explicit_deny {
            AppAccess {
                app_id: app_id.to_string(),
                allowed: false,
                read_only: false,
                sources,
                fallback: "explicit".into(),
            }
        } else if explicit_allow {
            // A group grant for a normally-admin-only surface (Applications,
            // Backup Center, Package Center) is read-only; for surfaces every account may use
            // by default an explicit allow is just a regular grant.
            AppAccess {
                app_id: app_id.to_string(),
                allowed: true,
                read_only: !app_default_allowed(app_id),
                sources,
                fallback: "explicit".into(),
            }
        } else {
            AppAccess {
                app_id: app_id.to_string(),
                allowed: app_default_allowed(app_id),
                read_only: app_default_read_only(app_id),
                sources,
                fallback: "default".into(),
            }
        }
    };

    cache_put(user.id, &key, read_at, Cached::App(access.clone()));
    Ok(access)
}

/// Require a minimum folder level. Hidden resources yield `NotFound` so the
/// existence of denied shared folders is not leaked.
pub async fn require_folder(
    pool: &SqlitePool,
    user: &AuthUser,
    share_name: &str,
    need: Perm,
) -> AppResult<FolderAccess> {
    let access = folder_access_by_name(pool, user, share_name).await?;
    match access.level {
        None => Err(AppError::NotFound("path not found")),
        Some(level) if perm_at_least(level, need) => Ok(access),
        Some(_) => Err(AppError::Forbidden(
            "you do not have permission to modify this shared folder",
        )),
    }
}

/// Folder level check used by storage/core code.
pub fn perm_at_least(have: Perm, need: Perm) -> bool {
    match need {
        Perm::Read => true,
        Perm::Write => matches!(have, Perm::Write | Perm::Admin),
        Perm::Admin => matches!(have, Perm::Admin),
    }
}

/// Guard for application API surfaces: returns 403 when the app is denied.
pub async fn app_surface(
    pool: &SqlitePool,
    _state: &crate::state::AppState,
    user: &AuthUser,
    app_id: &str,
) -> AppResult<AppAccess> {
    let access = app_access(pool, user, app_id).await?;
    if access.allowed {
        Ok(access)
    } else {
        Err(AppError::Forbidden(
            "access to this application is denied by group policy",
        ))
    }
}

// ---------------------------------------------------------------------------
// Effective access + group helpers used by the API/UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderAccessDto {
    pub share_id: String,
    pub share_name: String,
    pub path: String,
    /// `none`, `read`, `write` or `admin`.
    pub level: String,
    pub sources: Vec<RuleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppAccessDto {
    pub app_id: String,
    pub label: String,
    pub allowed: bool,
    pub read_only: bool,
    pub sources: Vec<RuleSource>,
    pub fallback: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveAccessDto {
    pub user: EffectiveUser,
    pub groups: Vec<GroupRef>,
    pub folders: Vec<FolderAccessDto>,
    pub apps: Vec<AppAccessDto>,
}

fn level_label(level: Option<Perm>) -> String {
    match level {
        None => "none".into(),
        Some(Perm::Read) => "read".into(),
        Some(Perm::Write) => "write".into(),
        Some(Perm::Admin) => "admin".into(),
    }
}

/// Effective access for `user`, used by both the admin user editor and the
/// caller's own `/api/access/me`.
///
/// `include_hidden = true` (admin editor) lists every shared folder with the
/// computed level, including No Access. For the caller's own view the denied
/// shares are omitted so the endpoint does not reveal hidden folder names.
pub async fn effective_access(
    pool: &SqlitePool,
    user: &AuthUser,
    include_hidden: bool,
) -> AppResult<EffectiveAccessDto> {
    let groups = group_refs_for_user(pool, user.id).await?;
    let share_rows = sqlx::query("SELECT id, name FROM shares ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    let mut folders = Vec::with_capacity(share_rows.len());
    for row in &share_rows {
        let share_id: String = row.get("id");
        let share_name: String = row.get("name");
        let access = compute_folder(pool, user, share_id.clone(), share_name.clone()).await?;
        if !include_hidden && access.level.is_none() {
            continue;
        }
        folders.push(FolderAccessDto {
            share_id,
            share_name: share_name.clone(),
            path: format!("/shared/{share_name}"),
            level: level_label(access.level),
            sources: access.sources,
            fallback: access.fallback,
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

    Ok(EffectiveAccessDto {
        user: EffectiveUser {
            id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            role: if user.is_admin() { "admin" } else { "user" }.to_string(),
        },
        groups,
        folders,
        apps,
    })
}

/// Groups a user belongs to.
pub async fn group_refs_for_user(pool: &SqlitePool, user_id: i64) -> AppResult<Vec<GroupRef>> {
    let rows = sqlx::query(
        "SELECT g.id, g.name FROM group_members gm JOIN groups g ON g.id = gm.group_id
         WHERE gm.user_id = ? ORDER BY g.name COLLATE NOCASE",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| GroupRef {
            id: row.get("id"),
            name: row.get("name"),
        })
        .collect())
}

/// Batch variant for the users table (avoids N+1).
pub async fn group_refs_for_users(
    pool: &SqlitePool,
    user_ids: &[i64],
) -> AppResult<HashMap<i64, Vec<GroupRef>>> {
    let mut out: HashMap<i64, Vec<GroupRef>> = HashMap::new();
    if user_ids.is_empty() {
        return Ok(out);
    }
    let placeholders = std::iter::repeat_n("?", user_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT gm.user_id, g.id, g.name FROM group_members gm
         JOIN groups g ON g.id = gm.group_id
         WHERE gm.user_id IN ({placeholders})
         ORDER BY g.name COLLATE NOCASE"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in user_ids {
        query = query.bind(id);
    }
    for row in query.fetch_all(pool).await? {
        out.entry(row.get("user_id")).or_default().push(GroupRef {
            id: row.get("id"),
            name: row.get("name"),
        });
    }
    Ok(out)
}

/// Accounts affected by a change to `share_name`'s rules: legacy members,
/// direct-rule holders and members of every group with a rule on the share.
pub async fn affected_users_for_share(pool: &SqlitePool, share_name: &str) -> AppResult<Vec<i64>> {
    let Some(share_id) = sqlx::query_scalar::<_, String>("SELECT id FROM shares WHERE name = ?")
        .bind(share_name)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<i64> = Vec::new();
    let mut push = |values: Vec<i64>| ids.extend(values);
    push(
        sqlx::query_scalar("SELECT user_id FROM user_folder_rules WHERE share_id = ?")
            .bind(&share_id)
            .fetch_all(pool)
            .await?,
    );
    push(
        sqlx::query_scalar(
            "SELECT gm.user_id FROM group_members gm
             JOIN group_folder_rules r ON r.group_id = gm.group_id
             WHERE r.share_id = ?",
        )
        .bind(&share_id)
        .fetch_all(pool)
        .await?,
    );
    push(
        sqlx::query_scalar("SELECT user_id FROM share_members WHERE share_id = ?")
            .bind(&share_id)
            .fetch_all(pool)
            .await?,
    );
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Accounts affected by a membership/rule change of one group.
pub async fn affected_users_for_group(pool: &SqlitePool, group_id: i64) -> AppResult<Vec<i64>> {
    let ids = sqlx::query_scalar("SELECT user_id FROM group_members WHERE group_id = ?")
        .bind(group_id)
        .fetch_all(pool)
        .await?;
    let mut ids: Vec<i64> = ids;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Live-event authorization
// ---------------------------------------------------------------------------

/// Which app governs a realtime event kind (`None` = always delivered to the
/// scoped audience). Mirrors `docs/GROUPS.md`.
pub fn event_app(kind: &str, data: &serde_json::Value) -> Option<&'static str> {
    match kind {
        "monitor" => Some("monitor"),
        "file.changed" | "file.quota" | "files.resync" => Some("files"),
        "app.changed" | "app.deploy" | "app.logs" | "docker.changed" => Some("applications"),
        "backup.changed" | "backup.done" => Some("backup"),
        "package.changed" | "package.catalog" => Some("packages"),
        "audit.created" => Some("logs"),
        "user.changed" | "settings.changed" | "group.changed" => Some("controlpanel"),
        "job.queued" | "job.start" | "job.progress" | "job.done" | "job.failed"
        | "job.cancelled" => {
            let job_kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if job_kind.starts_with("backup") {
                Some("backup")
            } else if job_kind.starts_with("package") {
                Some("packages")
            } else if job_kind.starts_with("app") {
                Some("applications")
            } else {
                Some("files")
            }
        }
        // Ownership-scoped administrative events are delivered to their audience.
        "access.changed" | "appearance.changed" | "notice" | "hello" | "ping" | "resync" => None,
        _ => None,
    }
}

/// Can `user` receive an event of this kind (application policy)?
pub async fn event_allowed(
    pool: &SqlitePool,
    user: &AuthUser,
    kind: &str,
    data: &serde_json::Value,
) -> bool {
    if user.is_admin() {
        return true;
    }
    match event_app(kind, data) {
        None => true,
        Some(app_id) => app_access(pool, user, app_id)
            .await
            .map(|access| access.allowed)
            .unwrap_or(false),
    }
}

/// Role reference used in tests/UI.
pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::User => "user",
    }
}

/// Pure folder precedence used by previews that evaluate an uncommitted
/// state (see [`review::group_deletion_impact`]). Same order as the
/// enforcement evaluator: explicit `none` wins, then the strongest grant,
/// then the legacy `share_members` level. Administrators are handled by the
/// caller.
pub fn merge_folder<'a>(
    direct: Option<&str>,
    group_rules: impl IntoIterator<Item = &'a str>,
    legacy: Option<&str>,
) -> Option<Perm> {
    let mut deny = false;
    let mut granted: Option<Perm> = None;
    let mut consider = |rule: &str| match FolderRule::parse(rule) {
        Some(FolderRule::None) => deny = true,
        Some(FolderRule::Write) => granted = Some(Perm::Write),
        Some(FolderRule::Read) => {
            if granted.is_none() {
                granted = Some(Perm::Read);
            }
        }
        Some(FolderRule::Inherit) | None => {}
    };
    if let Some(rule) = direct {
        consider(rule);
    }
    for rule in group_rules {
        consider(rule);
    }
    if deny {
        return None;
    }
    if granted.is_some() {
        return granted;
    }
    legacy.and_then(Perm::parse)
}

/// Pure application precedence (see [`merge_folder`]): returns
/// `(allowed, read_only)` for a non-administrator.
pub fn merge_app<'a>(
    app_id: &str,
    direct: Option<&'a str>,
    group_rules: impl IntoIterator<Item = &'a str>,
) -> (bool, bool) {
    let mut deny = false;
    let mut allow = false;
    for rule in direct.into_iter().chain(group_rules) {
        match AppRule::parse(rule) {
            Some(AppRule::None) => deny = true,
            Some(AppRule::Allow) => allow = true,
            Some(AppRule::Inherit) | None => {}
        }
    }
    if deny {
        (false, false)
    } else if allow {
        (true, !app_default_allowed(app_id))
    } else {
        (app_default_allowed(app_id), app_default_read_only(app_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder_decision(level: Option<Perm>) -> Cached {
        Cached::Folder(FolderAccess {
            share_id: "straddle".into(),
            share_name: "straddle".into(),
            level,
            sources: Vec::new(),
            fallback: None,
        })
    }

    #[test]
    fn decision_read_before_a_policy_change_is_never_served_after_it() {
        // A user id no fixture uses; other tests bump the generation
        // concurrently, which only makes the stale case more likely.
        let (user, key) = (-4_311, "folder:straddle");
        // An evaluation reads the old policy (a read grant)...
        let read_at = current_generation();
        // ...an administrator revokes it while the evaluation is in flight...
        bump_generation();
        // ...and the evaluation finishes, caching what it read.
        cache_put(user, key, read_at, folder_decision(Some(Perm::Read)));
        assert!(
            cache_get(user, key).is_none(),
            "a pre-change decision must not be served as current"
        );

        // A decision read under the current policy is served (retry if a
        // concurrent test changes the generation in between).
        let served = (0..100).any(|_| {
            let read_at = current_generation();
            cache_put(user, key, read_at, folder_decision(Some(Perm::Write)));
            matches!(
                cache_get(user, key),
                Some(Cached::Folder(FolderAccess {
                    level: Some(Perm::Write),
                    ..
                }))
            )
        });
        assert!(served, "a current decision should be cached");
    }

    #[test]
    fn folder_rule_parsing() {
        assert_eq!(FolderRule::parse("inherit"), Some(FolderRule::Inherit));
        assert_eq!(FolderRule::parse("NONE"), Some(FolderRule::None));
        assert_eq!(FolderRule::parse(" read "), Some(FolderRule::Read));
        assert_eq!(FolderRule::parse("write"), Some(FolderRule::Write));
        assert_eq!(FolderRule::parse("read-write"), None);
    }

    #[test]
    fn app_rule_parsing() {
        assert_eq!(AppRule::parse("inherit"), Some(AppRule::Inherit));
        assert_eq!(AppRule::parse("none"), Some(AppRule::None));
        assert_eq!(AppRule::parse("allow"), Some(AppRule::Allow));
        assert_eq!(AppRule::parse("deny"), None);
    }

    #[test]
    fn perm_hierarchy() {
        assert!(perm_at_least(Perm::Read, Perm::Read));
        assert!(!perm_at_least(Perm::Read, Perm::Write));
        assert!(perm_at_least(Perm::Write, Perm::Read));
        assert!(perm_at_least(Perm::Write, Perm::Write));
        assert!(!perm_at_least(Perm::Write, Perm::Admin));
        assert!(perm_at_least(Perm::Admin, Perm::Write));
        assert!(perm_at_least(Perm::Admin, Perm::Admin));
    }

    #[test]
    fn event_app_mapping() {
        let job = serde_json::json!({"kind": "backup.create"});
        assert_eq!(event_app("job.progress", &job), Some("backup"));
        let file_job = serde_json::json!({"kind": "file.copy"});
        assert_eq!(event_app("job.progress", &file_job), Some("files"));
        let package_job = serde_json::json!({"kind": "package.install"});
        assert_eq!(event_app("job.progress", &package_job), Some("packages"));
        assert_eq!(
            event_app("package.catalog", &serde_json::json!({})),
            Some("packages")
        );
        assert_eq!(
            event_app("audit.created", &serde_json::json!({})),
            Some("logs")
        );
        assert_eq!(event_app("access.changed", &serde_json::json!({})), None);
    }

    #[test]
    fn app_defaults_only_expected_apps_are_open() {
        for app in APP_IDS {
            let expected = matches!(*app, "files" | "monitor" | "logs" | "controlpanel");
            assert_eq!(app_default_allowed(app), expected, "default for {app}");
        }
        assert!(!APP_IDS.contains(&"admin"));
    }
}
