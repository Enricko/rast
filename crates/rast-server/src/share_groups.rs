//! Shared folders as Linux groups (ADR 0005, section 4).
//!
//! Every shared folder has a read-write group `rast-s<h>-rw` and a read-only
//! group `rast-s<h>-ro`. Their members follow the current Effective Access
//! (`access::folder_access_by_id`): write or admin → rw, read → ro, deny or
//! no access → neither, deactivated account → neither. Administrators are in
//! every rw group (D6), as the evaluator gives them admin on every share.
//!
//! The syncer recomputes membership whenever the access rules change
//! (`access::bump_generation`) and every few minutes, and sets both groups
//! of every share through the helper (`share_members_set`). Only accounts
//! that have a Linux identity are members: the others have no processes.
//!
//! Running processes keep the groups they started with. On any change the
//! user's file agent is stopped at once (it starts again with the new
//! groups), so File Station is right immediately; then the `~/Shared` links
//! of a one-HOME account are brought in line. A **revocation** restarts the
//! user's apps and terminals after a [`RESTART_WARNING`] warning; a **grant**
//! applies at the next session start, and the warning offers "Restart
//! session now" (D7).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::auth::{AuthUser, Role};
use crate::db::now_ms;
use crate::error::AppResult;
use crate::homefs::{ShareRoot, ShareRoots};
use crate::native::client::NativeClient;
use crate::state::AppState;
use crate::storage::shares;

/// How long a user is warned before a revocation restarts their session.
pub const RESTART_WARNING: Duration = Duration::from_secs(60);
/// Rule changes often come in bursts (a group edit touches many rows).
const DEBOUNCE: Duration = Duration::from_millis(500);
/// A full pass also runs this often: identities created since, accounts
/// deactivated by other paths, and groups changed by hand.
const RESYNC_INTERVAL: Duration = Duration::from_secs(300);
/// After a failed pass (helper down), wait this long before the next one.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The members of one shared folder's two groups (RAST user ids, sorted).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Membership {
    pub write: Vec<i64>,
    pub read: Vec<i64>,
}

/// One shared folder as the syncer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInfo {
    pub id: String,
    pub name: String,
    /// The container (one folder name in the shared root).
    pub container: String,
}

/// Membership of every shared folder, and the shared folders each account
/// may open (for its `~/Shared` links).
#[derive(Debug, Default, Clone)]
pub struct Plan {
    pub shares: Vec<ShareInfo>,
    pub members: BTreeMap<String, Membership>,
    pub roots: BTreeMap<i64, ShareRoots>,
    /// The accounts the plan covers (active, with a Linux identity).
    pub accounts: Vec<AuthUser>,
}

/// Compute the groups of every shared folder from Effective Access.
pub async fn plan(pool: &SqlitePool) -> AppResult<Plan> {
    let mut plan = Plan::default();
    for record in shares::list_records(pool).await? {
        match shares::validated_container(&record.container_path) {
            Ok(container) => plan.shares.push(ShareInfo {
                id: record.id,
                name: record.name,
                container,
            }),
            Err(error) => tracing::warn!(
                share = %record.id,
                error = %error,
                "a shared folder with an invalid container gets no Linux groups"
            ),
        }
    }
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.quota_bytes
         FROM users u JOIN linux_identities li ON li.user_id = u.id
         WHERE u.active = 1 ORDER BY u.id",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        plan.accounts.push(AuthUser {
            id: row.get("id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
            role: row.get::<Role, _>("role"),
            session_id: String::new(),
            quota_bytes: row.get("quota_bytes"),
            must_change_password: false,
        });
    }
    for share in &plan.shares {
        let mut membership = Membership::default();
        for user in &plan.accounts {
            let access = crate::access::folder_access_by_id(pool, user, &share.id).await?;
            let Some(level) = access.level else {
                continue;
            };
            if level.allows_write() {
                membership.write.push(user.id);
            } else {
                membership.read.push(user.id);
            }
            plan.roots.entry(user.id).or_default().insert(
                &share.name,
                ShareRoot {
                    id: share.id.clone(),
                    name: share.name.clone(),
                    container: share.container.clone(),
                },
            )?;
        }
        plan.members.insert(share.id.clone(), membership);
    }
    Ok(plan)
}

/// Who gained and who lost a group in one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    pub granted: HashSet<i64>,
    pub revoked: HashSet<i64>,
}

impl Changes {
    /// Read one `share_members_set` reply. A user moved from the rw to the
    /// ro group is a revocation: their processes still carry the rw group.
    pub fn record(&mut self, reply: &Value) {
        for key in ["write", "read"] {
            let ids = |field: &str| -> Vec<i64> {
                reply[key][field]
                    .as_array()
                    .map(|list| list.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default()
            };
            self.granted.extend(ids("added"));
            self.revoked.extend(ids("removed"));
        }
    }

    pub fn changed(&self) -> HashSet<i64> {
        self.granted.union(&self.revoked).copied().collect()
    }
}

/// What the syncer remembers between passes.
#[derive(Default)]
struct Known {
    /// Shares whose groups exist.
    ensured: HashSet<String>,
    /// The `~/Shared` links last sent to each one-HOME agent.
    links: HashMap<i64, ShareRoots>,
}

/// Start the syncer (only with the native backend: the groups need the
/// helper).
pub fn spawn(state: AppState) {
    if !state.config.native_enabled {
        return;
    }
    let native = NativeClient::from_config(&state.config);
    tokio::spawn(async move {
        let mut generation = crate::access::subscribe_generation();
        let mut known = Known::default();
        // The same failure (a helper without these operations, or down) is
        // logged once as a warning, then at debug level.
        let mut last_error = None;
        loop {
            let wait = match sync_once(&state, &native, &mut known).await {
                Ok(()) => {
                    last_error = None;
                    RESYNC_INTERVAL
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_ref() != Some(&message) {
                        tracing::warn!(error = %message, "shared folder groups: sync failed");
                    } else {
                        tracing::debug!(error = %message, "shared folder groups: sync failed");
                    }
                    last_error = Some(message);
                    RETRY_AFTER
                }
            };
            tokio::select! {
                changed = generation.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    tokio::time::sleep(DEBOUNCE).await;
                    generation.borrow_and_update();
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

async fn sync_once(state: &AppState, native: &NativeClient, known: &mut Known) -> AppResult<()> {
    let plan = plan(&state.db).await?;
    let mut changes = Changes::default();
    for share in &plan.shares {
        if !known.ensured.contains(&share.id) {
            native
                .call_json(
                    "share_group_ensure",
                    json!({"shareId": share.id, "container": share.container}),
                )
                .await
                .map_err(|error| {
                    crate::error::AppError::unavailable(format!("share_group_ensure: {error}"))
                })?;
            known.ensured.insert(share.id.clone());
        }
        let membership = plan.members.get(&share.id).cloned().unwrap_or_default();
        let reply = native
            .call_json(
                "share_members_set",
                json!({
                    "shareId": share.id,
                    "write": membership.write,
                    "read": membership.read,
                }),
            )
            .await
            .map_err(|error| {
                crate::error::AppError::unavailable(format!("share_members_set: {error}"))
            })?;
        changes.record(&reply);
    }
    apply_changes(state, &changes).await;
    // `~/Shared` links of one-HOME accounts whose shares changed.
    for user in &plan.accounts {
        let roots = plan.roots.get(&user.id).cloned().unwrap_or_default();
        if known.links.get(&user.id) == Some(&roots) && !changes.changed().contains(&user.id) {
            continue;
        }
        if !state
            .homefs
            .is_one_home(&state.db, user)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        match sync_links(state, user.id, &roots).await {
            Ok(report) => {
                if report["conflicts"]
                    .as_array()
                    .is_some_and(|list| !list.is_empty())
                {
                    tracing::info!(
                        user_id = user.id,
                        conflicts = %report["conflicts"],
                        "some ~/Shared names are taken by the user's own files"
                    );
                }
                known.links.insert(user.id, roots);
            }
            Err(error) => {
                tracing::warn!(user_id = user.id, error = %error, "cannot update ~/Shared links");
            }
        }
    }
    let active: HashSet<i64> = plan.accounts.iter().map(|user| user.id).collect();
    known.links.retain(|id, _| active.contains(id));
    Ok(())
}

/// Ask the user's agent to bring `~/Shared` in line with `roots`.
async fn sync_links(state: &AppState, user_id: i64, roots: &ShareRoots) -> AppResult<Value> {
    let mut headers = HeaderMap::new();
    roots.apply(&mut headers)?;
    state
        .homefs
        .call(
            user_id,
            crate::homefs::agent::SHARED_LINKS_ROUTE,
            &json!({}),
            headers,
        )
        .await
}

/// Restart agents at once; warn and schedule session restarts.
async fn apply_changes(state: &AppState, changes: &Changes) {
    let mut changed: Vec<i64> = changes.changed().into_iter().collect();
    changed.sort_unstable();
    for user_id in changed {
        state.homefs.stop_agent(user_id).await;
        if changes.revoked.contains(&user_id) {
            state.session_restarts.revoked(state, user_id);
        } else {
            state.session_restarts.granted(state, user_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Session restarts
// ---------------------------------------------------------------------------

/// A restart a user was warned about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRestart {
    /// `access_revoked` (forced after the warning) or `access_granted`
    /// (only offered).
    pub reason: &'static str,
    pub forced: bool,
    /// When a forced restart happens (ms since 1970).
    pub deadline_ms: Option<i64>,
    #[serde(skip)]
    token: u64,
}

/// The restarts users were warned about, by user id.
#[derive(Clone)]
pub struct SessionRestarts {
    pending: Arc<Mutex<HashMap<i64, PendingRestart>>>,
    next_token: Arc<std::sync::atomic::AtomicU64>,
    warning: Duration,
}

impl Default for SessionRestarts {
    fn default() -> Self {
        Self::with_warning(RESTART_WARNING)
    }
}

impl SessionRestarts {
    /// Restarts after `warning` instead of [`RESTART_WARNING`] (tests).
    pub fn with_warning(warning: Duration) -> Self {
        Self {
            pending: Arc::default(),
            next_token: Arc::default(),
            warning,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, PendingRestart>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn token(&self) -> u64 {
        self.next_token
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// The restart `user_id` was warned about, if any.
    pub fn pending(&self, user_id: i64) -> Option<PendingRestart> {
        self.lock().get(&user_id).cloned()
    }

    /// Access was revoked: restart after the warning. A forced restart that
    /// is already scheduled keeps its deadline.
    pub fn revoked(&self, state: &AppState, user_id: i64) {
        let (pending, new) = {
            let mut map = self.lock();
            match map.get(&user_id) {
                Some(pending) if pending.forced => (pending.clone(), false),
                _ => {
                    let pending = PendingRestart {
                        reason: "access_revoked",
                        forced: true,
                        deadline_ms: Some(now_ms() + self.warning.as_millis() as i64),
                        token: self.token(),
                    };
                    map.insert(user_id, pending.clone());
                    (pending, true)
                }
            }
        };
        if !new {
            return;
        }
        publish(state, user_id, &pending, self.warning);
        let state = state.clone();
        let warning = self.warning;
        tokio::spawn(async move {
            tokio::time::sleep(warning).await;
            let due = {
                let mut map = state.session_restarts.lock();
                match map.get(&user_id) {
                    Some(current) if current.token == pending.token => {
                        map.remove(&user_id);
                        true
                    }
                    _ => false,
                }
            };
            if due {
                restart(&state, user_id, "access_revoked").await;
            }
        });
    }

    /// Access was granted: offer a restart (never forced).
    pub fn granted(&self, state: &AppState, user_id: i64) {
        let pending = {
            let mut map = self.lock();
            if map.contains_key(&user_id) {
                return;
            }
            let pending = PendingRestart {
                reason: "access_granted",
                forced: false,
                deadline_ms: None,
                token: self.token(),
            };
            map.insert(user_id, pending.clone());
            pending
        };
        publish(state, user_id, &pending, self.warning);
    }

    /// The user asked for the restart now.
    pub async fn restart_now(&self, state: &AppState, user_id: i64) {
        self.lock().remove(&user_id);
        restart(state, user_id, "requested").await;
    }
}

fn publish(state: &AppState, user_id: i64, pending: &PendingRestart, warning: Duration) {
    state.events.publish_user(
        "session.restart",
        user_id,
        json!({
            "reason": pending.reason,
            "forced": pending.forced,
            "deadlineMs": pending.deadline_ms,
            "seconds": pending.forced.then_some(warning.as_secs()),
        }),
    );
}

/// End the user's apps and terminals; they start again with the current
/// groups.
async fn restart(state: &AppState, user_id: i64, reason: &str) {
    state.linux_apps.stop_user_sessions(user_id).await;
    state.terminal.close_user(user_id).await;
    state
        .events
        .publish_user("session.restarted", user_id, json!({"reason": reason}));
}

/// Before a shared folder is used: its groups, and its container with the
/// group layout (a new, empty container needs no recursive pass).
pub async fn share_created(state: &AppState, share_id: &str, container_path: &str) {
    if !state.config.native_enabled {
        return;
    }
    let Ok(container) = shares::validated_container(container_path) else {
        return;
    };
    let native = NativeClient::from_config(&state.config);
    for (op, params) in [
        (
            "share_group_ensure",
            json!({"shareId": share_id, "container": container}),
        ),
        (
            "share_dir_prepare",
            json!({"shareId": share_id, "container": container, "recursive": false}),
        ),
    ] {
        if let Err(error) = native.call_json(op, params).await {
            tracing::warn!(share = share_id, op, error = %error, "cannot set up the shared folder groups");
            return;
        }
    }
}

/// After a shared folder's definition is removed (its data stays): close its
/// container to the old groups, then remove the groups.
pub async fn share_deleted(state: &AppState, share_id: &str, container_path: &str) {
    if !state.config.native_enabled {
        return;
    }
    let native = NativeClient::from_config(&state.config);
    if let Ok(container) = shares::validated_container(container_path) {
        if let Err(error) = native
            .call_json(
                "share_dir_release",
                json!({"shareId": share_id, "container": container}),
            )
            .await
        {
            // The groups stay too: removing them would hand their gids to
            // the next share while the container still names them.
            tracing::warn!(share = share_id, error = %error, "cannot close the container of a deleted shared folder");
            return;
        }
    }
    if let Err(error) = native
        .call_json("share_group_remove", json!({"shareId": share_id}))
        .await
    {
        tracing::warn!(share = share_id, error = %error, "cannot remove the groups of a deleted shared folder");
    }
}

#[cfg(test)]
mod tests;
