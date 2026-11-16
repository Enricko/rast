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
//!
//! While a user waits for that restart, every shared folder they lost
//! entirely gets the deny entry `u:rast-u<id>:---` on its container
//! (`share_deny_set`), which stops new lookups at once; it goes when the
//! session restarts or access is granted again. A move from write to read
//! gets no entry (it cannot be expressed on the container).
//!
//! A **repair pass** (`share_dir_repair`) applies the group layout again to
//! everything in every migrated container: at start-up, which follows every
//! `rast migrate shares` (its apply needs the server stopped), and then
//! daily. It resets entries that kept a foreign group (moved in with
//! `rename`, `chgrp`, `cp -p`, or by the legacy in-process File Station),
//! so the group quota of a share (D3: "filesystem, best effort") counts
//! them again.
//!
//! Pending restarts live in memory, so every pass also asks the helper
//! which share groups the running processes carry (`share_session_groups`):
//! a session that still holds a group its user lost (after a server
//! restart, or one that survived its restart) is warned and restarted
//! again.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
use crate::native::NativeError;
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
/// Processes of a session that was just restarted may still be exiting:
/// they are not taken for a session that kept a revoked group.
const RESTART_GRACE: Duration = Duration::from_secs(30);
/// Users per `share_session_groups` call (the helper accepts 200).
const SESSION_GROUPS_BATCH: usize = 100;
/// Users with deny entries on one container (the helper accepts 256).
const MAX_DENIALS: usize = 256;
/// How often the repair pass runs (also once at start-up).
const REPAIR_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Paths of entries with foreign ACL entries logged per share.
const REPAIR_LOGGED_PATHS: usize = 5;

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
    /// Every account with a Linux identity, deactivated ones included
    /// (their processes must carry no share group either).
    pub identities: Vec<i64>,
}

impl Plan {
    /// Which group of which share a group name is.
    fn group_index(&self) -> HashMap<String, (&ShareInfo, bool)> {
        let mut out = HashMap::new();
        for share in &self.shares {
            if let Some(rw) = shares::rw_group_name(&share.id) {
                out.insert(rw, (share, true));
            }
            if let Some(ro) = shares::ro_group_name(&share.id) {
                out.insert(ro, (share, false));
            }
        }
        out
    }

    fn container(&self, share_id: &str) -> Option<&str> {
        self.shares
            .iter()
            .find(|share| share.id == share_id)
            .map(|share| share.container.as_str())
    }
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
    plan.identities = sqlx::query_scalar("SELECT user_id FROM linux_identities ORDER BY user_id")
        .fetch_all(pool)
        .await?;
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
    /// Per user, the shares they lost entirely (in neither group now).
    pub lost: HashMap<i64, BTreeSet<String>>,
    /// Per user, the shares they gained a group of.
    pub gained: HashMap<i64, BTreeSet<String>>,
}

impl Changes {
    /// Read one `share_members_set` reply. A user moved from the rw to the
    /// ro group is a revocation: their processes still carry the rw group.
    /// A move from the ro to the rw group is not: the ro group gives
    /// nothing that the rw group does not.
    pub fn record(&mut self, reply: &Value) {
        let ids = |key: &str, field: &str| -> HashSet<i64> {
            reply[key][field]
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default()
        };
        let (write_added, write_removed) = (ids("write", "added"), ids("write", "removed"));
        let (read_added, read_removed) = (ids("read", "added"), ids("read", "removed"));
        let upgraded: HashSet<i64> = read_removed.intersection(&write_added).copied().collect();
        let lost: HashSet<i64> = write_removed
            .difference(&read_added)
            .chain(read_removed.difference(&write_added))
            .copied()
            .collect();
        self.granted.extend(write_added.iter().chain(&read_added));
        self.revoked.extend(
            write_removed
                .iter()
                .chain(read_removed.difference(&upgraded)),
        );
        if let Some(share) = reply["shareId"].as_str() {
            for user in lost {
                self.lost.entry(user).or_default().insert(share.to_string());
            }
            for user in write_added.iter().chain(&read_added) {
                self.gained
                    .entry(*user)
                    .or_default()
                    .insert(share.to_string());
            }
        }
    }

    pub fn changed(&self) -> HashSet<i64> {
        self.granted.union(&self.revoked).copied().collect()
    }
}

/// What the syncer remembers between passes.
#[derive(Default)]
struct Known {
    /// The last `share_session_groups` call failed (logged once).
    stale_check_failed: bool,
    /// The last error of each shared folder whose groups could not be set
    /// (a warning when it changes, then at debug level).
    share_errors: HashMap<String, String>,
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
        let mut last_repair = None;
        let repairing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        loop {
            let wait = match sync_once(&state, &native, &mut known).await {
                Ok(()) => {
                    last_error = None;
                    // The groups exist now: time for the repair pass?
                    let now = std::time::Instant::now();
                    if repair_due(last_repair, now)
                        && !repairing.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        last_repair = Some(now);
                        let (state, native, repairing) =
                            (state.clone(), native.clone(), repairing.clone());
                        tokio::spawn(async move {
                            if let Err(error) = repair_all(&state, &native).await {
                                tracing::warn!(error = %error, "shared folder repair pass failed");
                            }
                            repairing.store(false, std::sync::atomic::Ordering::SeqCst);
                        });
                    }
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

/// Whether the repair pass is due: never run yet, or a day ago. (It
/// starts only after a successful sync, so the helper is up and the groups
/// exist; a pass that fails waits for the next day like any other.)
fn repair_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= REPAIR_INTERVAL)
}

/// One repair pass over every shared folder, one at a time.
pub async fn repair_all(state: &AppState, native: &NativeClient) -> AppResult<RepairSummary> {
    let mut summary = RepairSummary::default();
    for record in shares::list_records(&state.db).await? {
        let Ok(container) = shares::validated_container(&record.container_path) else {
            continue;
        };
        let reply = match native
            .call_json(
                "share_dir_repair",
                json!({"shareId": record.id, "container": container}),
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                summary.failed += 1;
                tracing::warn!(share = %record.id, error = %error, "cannot repair a shared folder");
                continue;
            }
        };
        if reply["repaired"] != true {
            summary.not_migrated += 1;
            continue;
        }
        summary.repaired += 1;
        let report = &reply["report"];
        let changed = report["changes"]["count"].as_u64().unwrap_or(0);
        summary.changed += changed;
        if changed > 0 {
            tracing::info!(
                share = %record.id,
                name = %record.name,
                changed,
                "shared folder repair: entries got the group layout back"
            );
        }
        let foreign = report["foreignAclEntries"]["count"].as_u64().unwrap_or(0);
        if foreign > 0 {
            let sample: Vec<&str> = report["foreignAclEntries"]["paths"]
                .as_array()
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .take(REPAIR_LOGGED_PATHS)
                        .collect()
                })
                .unwrap_or_default();
            tracing::warn!(
                share = %record.id,
                name = %record.name,
                entries = foreign,
                sample = ?sample,
                "shared folder repair removed ACL entries that RAST does not manage"
            );
        }
    }
    Ok(summary)
}

/// What one repair pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepairSummary {
    pub repaired: u64,
    /// Legacy containers, left for `rast migrate shares`.
    pub not_migrated: u64,
    pub failed: u64,
    /// Entries changed over all shares.
    pub changed: u64,
}

/// Ensure the groups of one shared folder (once) and set their members.
async fn sync_share(
    native: &NativeClient,
    known: &mut Known,
    share: &ShareInfo,
    plan: &Plan,
) -> Result<Value, (&'static str, NativeError)> {
    if !known.ensured.contains(&share.id) {
        native
            .call_json(
                "share_group_ensure",
                json!({"shareId": share.id, "container": share.container}),
            )
            .await
            .map_err(|error| ("share_group_ensure", error))?;
        known.ensured.insert(share.id.clone());
    }
    let membership = plan.members.get(&share.id).cloned().unwrap_or_default();
    native
        .call_json(
            "share_members_set",
            json!({
                "shareId": share.id,
                "write": membership.write,
                "read": membership.read,
            }),
        )
        .await
        .map_err(|error| ("share_members_set", error))
}

async fn sync_once(state: &AppState, native: &NativeClient, known: &mut Known) -> AppResult<()> {
    let plan = plan(&state.db).await?;
    let mut changes = Changes::default();
    // A shared folder whose groups the helper refuses (a group name taken
    // outside the range, an account with an unexpected uid) must not hold
    // up the others: their members still change, and every revocation
    // still restarts the sessions and sets the deny entries below. Only a
    // helper that cannot be reached ends the pass early.
    let mut unreachable = None;
    for share in &plan.shares {
        match sync_share(native, known, share, &plan).await {
            Ok(reply) => {
                known.share_errors.remove(&share.id);
                changes.record(&reply);
            }
            Err((op, error)) if error.code == "unavailable" => {
                unreachable = Some(format!("{op}: {error}"));
                break;
            }
            Err((op, error)) => {
                let message = format!("{op}: {error}");
                if known.share_errors.get(&share.id) != Some(&message) {
                    tracing::warn!(
                        share = %share.id,
                        name = %share.name,
                        error = %message,
                        "shared folder groups: cannot set the groups of a shared folder"
                    );
                } else {
                    tracing::debug!(share = %share.id, error = %message, "shared folder groups: still failing");
                }
                known.share_errors.insert(share.id.clone(), message);
            }
        }
    }
    let current: HashSet<&str> = plan.shares.iter().map(|share| share.id.as_str()).collect();
    known
        .share_errors
        .retain(|id, _| current.contains(id.as_str()));
    // Changes the helper already made are acted on even when it went away
    // half way through.
    apply_changes(state, &changes, &plan).await;
    if let Some(error) = unreachable {
        return Err(crate::error::AppError::unavailable(error));
    }
    // Sessions that still carry a group their user lost.
    match stale_sessions(native, &plan).await {
        Ok(stale) => {
            known.stale_check_failed = false;
            for (user_id, lost) in stale {
                if state.session_restarts.recently_restarted(user_id) {
                    continue;
                }
                if state.session_restarts.pending_forced(user_id).is_none() {
                    tracing::info!(
                        user_id,
                        "a session still carries shared folder groups its user lost; restarting it"
                    );
                    state.homefs.stop_agent(user_id).await;
                }
                state.session_restarts.revoked(state, user_id, lost);
            }
        }
        Err(error) => {
            if !known.stale_check_failed {
                tracing::warn!(error = %error, "shared folder groups: cannot inspect running sessions");
            }
            known.stale_check_failed = true;
        }
    }
    // Deny entries of pending restarts (on the first pass also every
    // leftover entry of an earlier run is removed).
    state
        .session_restarts
        .sync_denials(native, &plan.shares)
        .await;
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

/// The users whose running processes carry a share group they are no
/// longer a member of, with the shares they lost entirely (a session that
/// holds the rw group of a share it may now only read has lost nothing
/// entirely).
async fn stale_sessions(
    native: &NativeClient,
    plan: &Plan,
) -> AppResult<BTreeMap<i64, BTreeMap<String, String>>> {
    let index = plan.group_index();
    let mut out = BTreeMap::new();
    for batch in plan.identities.chunks(SESSION_GROUPS_BATCH) {
        let reply = native
            .call_json("share_session_groups", json!({ "userIds": batch }))
            .await
            .map_err(|error| {
                crate::error::AppError::unavailable(format!("share_session_groups: {error}"))
            })?;
        for (user_id, lost) in stale_in_reply(&reply, plan, &index) {
            out.insert(user_id, lost);
        }
    }
    Ok(out)
}

/// [`stale_sessions`] for one `share_session_groups` reply.
fn stale_in_reply(
    reply: &Value,
    plan: &Plan,
    index: &HashMap<String, (&ShareInfo, bool)>,
) -> Vec<(i64, BTreeMap<String, String>)> {
    let mut out = Vec::new();
    for user in reply["users"].as_array().into_iter().flatten() {
        let Some(user_id) = user["userId"].as_i64() else {
            continue;
        };
        let mut stale = false;
        let mut lost = BTreeMap::new();
        for gid in user["gids"].as_array().into_iter().flatten() {
            let Some(name) = gid
                .as_u64()
                .and_then(|gid| reply["groups"][gid.to_string()].as_str())
            else {
                // A gid without a share group: its share is gone and its
                // container closed.
                continue;
            };
            let Some((share, rw)) = index.get(name) else {
                continue;
            };
            let membership = plan.members.get(&share.id);
            let writes = membership.is_some_and(|m| m.write.contains(&user_id));
            let reads = membership.is_some_and(|m| m.read.contains(&user_id));
            let allowed = if *rw { writes } else { writes || reads };
            if allowed {
                continue;
            }
            stale = true;
            if !writes && !reads {
                lost.insert(share.id.clone(), share.container.clone());
            }
        }
        if stale {
            out.push((user_id, lost));
        }
    }
    out
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
async fn apply_changes(state: &AppState, changes: &Changes, plan: &Plan) {
    let mut changed: Vec<i64> = changes.changed().into_iter().collect();
    changed.sort_unstable();
    for user_id in changed {
        state.homefs.stop_agent(user_id).await;
        if let Some(gained) = changes.gained.get(&user_id) {
            state.session_restarts.regained(user_id, gained);
        }
        if changes.revoked.contains(&user_id) {
            let lost = changes
                .lost
                .get(&user_id)
                .into_iter()
                .flatten()
                .filter_map(|share| Some((share.clone(), plan.container(share)?.to_string())))
                .collect();
            state.session_restarts.revoked(state, user_id, lost);
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
    /// Shares lost entirely (id → container): their containers deny the
    /// user until the restart.
    #[serde(skip)]
    denied: BTreeMap<String, String>,
}

/// The restarts users were warned about, by user id.
#[derive(Clone)]
pub struct SessionRestarts {
    pending: Arc<Mutex<HashMap<i64, PendingRestart>>>,
    next_token: Arc<std::sync::atomic::AtomicU64>,
    warning: Duration,
    /// When each user's session was last restarted.
    restarted: Arc<Mutex<HashMap<i64, std::time::Instant>>>,
    /// The deny entries the helper last set, by share id. A share missing
    /// here is set (and so cleaned) on the next sync.
    denials: Arc<tokio::sync::Mutex<HashMap<String, BTreeSet<i64>>>>,
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
            restarted: Arc::default(),
            denials: Arc::default(),
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

    /// The forced restart `user_id` waits for, if any.
    pub fn pending_forced(&self, user_id: i64) -> Option<PendingRestart> {
        self.pending(user_id).filter(|pending| pending.forced)
    }

    /// Whether the user's session was restarted less than
    /// [`RESTART_GRACE`] ago.
    pub fn recently_restarted(&self, user_id: i64) -> bool {
        self.restarted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&user_id)
            .is_some_and(|at| at.elapsed() < RESTART_GRACE)
    }

    /// The shares whose containers deny `user_id` now.
    pub fn denied(&self, user_id: i64) -> BTreeMap<String, String> {
        self.pending(user_id)
            .map(|pending| pending.denied)
            .unwrap_or_default()
    }

    /// Access was revoked (`lost`: the shares lost entirely, id →
    /// container): restart after the warning. A forced restart that is
    /// already scheduled keeps its deadline and adds the shares.
    pub fn revoked(&self, state: &AppState, user_id: i64, lost: BTreeMap<String, String>) {
        let (pending, new) = {
            let mut map = self.lock();
            match map.get_mut(&user_id) {
                Some(pending) if pending.forced => {
                    pending.denied.extend(lost);
                    (pending.clone(), false)
                }
                _ => {
                    let pending = PendingRestart {
                        reason: "access_revoked",
                        forced: true,
                        deadline_ms: Some(now_ms() + self.warning.as_millis() as i64),
                        token: self.token(),
                        denied: lost,
                    };
                    map.insert(user_id, pending.clone());
                    (pending, true)
                }
            }
        };
        if !pending.denied.is_empty() {
            self.spawn_denials(state, pending.denied.clone());
        }
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
                    Some(current) if current.token == pending.token => map.remove(&user_id),
                    _ => None,
                }
            };
            if let Some(current) = due {
                restart(&state, user_id, "access_revoked").await;
                state
                    .session_restarts
                    .after_restart(&state, user_id, current.denied)
                    .await;
            }
        });
    }

    /// Access to `shares` was granted again: their containers stop denying
    /// the user (a pending forced restart still happens).
    pub fn regained(&self, user_id: i64, shares: &BTreeSet<String>) {
        if let Some(pending) = self.lock().get_mut(&user_id) {
            pending.denied.retain(|share, _| !shares.contains(share));
        }
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
                denied: BTreeMap::new(),
            };
            map.insert(user_id, pending.clone());
            pending
        };
        publish(state, user_id, &pending, self.warning);
    }

    /// The user asked for the restart now.
    pub async fn restart_now(&self, state: &AppState, user_id: i64) {
        let denied = self
            .lock()
            .remove(&user_id)
            .map(|pending| pending.denied)
            .unwrap_or_default();
        restart(state, user_id, "requested").await;
        self.after_restart(state, user_id, denied).await;
    }

    /// After a restart: remember when, and lift the user's deny entries.
    async fn after_restart(
        &self,
        state: &AppState,
        user_id: i64,
        denied: BTreeMap<String, String>,
    ) {
        self.restarted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(user_id, std::time::Instant::now());
        if denied.is_empty() || !state.config.native_enabled {
            return;
        }
        let native = NativeClient::from_config(&state.config);
        let shares: Vec<ShareInfo> = denied
            .into_iter()
            .map(|(id, container)| ShareInfo {
                id,
                name: String::new(),
                container,
            })
            .collect();
        self.sync_denials(&native, &shares).await;
    }

    fn spawn_denials(&self, state: &AppState, shares: BTreeMap<String, String>) {
        if !state.config.native_enabled {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            let native = NativeClient::from_config(&state.config);
            let shares: Vec<ShareInfo> = shares
                .into_iter()
                .map(|(id, container)| ShareInfo {
                    id,
                    name: String::new(),
                    container,
                })
                .collect();
            state.session_restarts.sync_denials(&native, &shares).await;
        });
    }

    /// The users each share must deny now: those with a pending forced
    /// restart who lost it entirely (at most [`MAX_DENIALS`]).
    pub fn wanted_denials(&self, share_id: &str) -> BTreeSet<i64> {
        let wanted: BTreeSet<i64> = self
            .lock()
            .iter()
            .filter(|(_, pending)| pending.forced && pending.denied.contains_key(share_id))
            .map(|(user_id, _)| *user_id)
            .collect();
        if wanted.len() > MAX_DENIALS {
            tracing::warn!(
                share = share_id,
                users = wanted.len(),
                "more pending revocations than deny entries; the rest wait for their restart"
            );
        }
        wanted.into_iter().take(MAX_DENIALS).collect()
    }

    /// Bring the deny entries of `shares` in line with the pending
    /// restarts. Calls are serialized, and only made when the set differs
    /// from the one last set (or is not known yet).
    pub async fn sync_denials(&self, native: &NativeClient, shares: &[ShareInfo]) {
        let mut applied = self.denials.lock().await;
        for share in shares {
            let wanted = self.wanted_denials(&share.id);
            if applied.get(&share.id) == Some(&wanted) {
                continue;
            }
            match native
                .call_json(
                    "share_deny_set",
                    json!({
                        "shareId": share.id,
                        "container": share.container,
                        "userIds": wanted,
                    }),
                )
                .await
            {
                Ok(_) => {
                    applied.insert(share.id.clone(), wanted);
                }
                Err(error) => {
                    applied.remove(&share.id);
                    tracing::warn!(
                        share = %share.id,
                        error = %error,
                        "cannot set the deny entries of a shared folder"
                    );
                }
            }
        }
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
