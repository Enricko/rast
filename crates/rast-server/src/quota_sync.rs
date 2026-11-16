//! Which quota backend applies, and kernel limits for one-HOME accounts
//! (ADR 0005, section 6).
//!
//! The server asks the helper (`quota_status`) at start and every ten
//! minutes which quota the homes filesystem keeps. With user quota on
//! (accounting or enforced), the limits of every one-HOME account follow
//! D4 (`quota_set`: soft = `users.quota_bytes`, hard = soft + max(5 %,
//! 1 GiB), none for administrators and quota 0), and its used bytes come
//! from the kernel (`quota_report`): everything the identity owns on that
//! filesystem, apps, caches, the trash and its files in shared folders
//! (D2). The backend is "filesystem" only while user quota is enforced.
//! Otherwise RAST's own accounting stays (the RAST fallback, D1): File
//! Station and App Center refuse writes over quota, a HOME scan refreshes
//! the used bytes, and apps, Terminal and launchers are not limited.
//!
//! Shared-folder limits on the kernel side are group quota on the rw gid
//! ("filesystem, best effort", D3).
//!
//! Legacy accounts (HOME owned by the service) always use RAST's
//! accounting.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error::{AppError, AppResult};
use crate::native::client::NativeClient;
use crate::native::quota::{QuotaState, RootStatus, MAX_REPORT_IDS};
use crate::state::AppState;
use crate::storage::ops::{QUOTA_BACKEND_FILESYSTEM, QUOTA_BACKEND_RAST};
use crate::storage::quota;

/// How often the backend is checked and the limits are synced.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(600);
/// After a failed check.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Shortest hard-limit margin above the soft limit (D4).
pub const HARD_MARGIN_MIN: u64 = 1 << 30;
/// Ids per `quota_report` call.
const REPORT_CHUNK: usize = 1000;

/// What enforces a shared folder's limit.
pub const SHARE_ENFORCED_FILESYSTEM_BEST_EFFORT: &str = "filesystem, best effort";
pub const SHARE_ENFORCED_RAST: &str = "RAST only";

/// The kernel limits of D4 for an account, in bytes: `(soft, hard)`,
/// `(0, 0)` for no limit. Capped at the filesystem's size (`total`, when
/// known), which the helper refuses to exceed.
pub fn d4_limits(quota_bytes: i64, admin: bool, total: u64) -> (u64, u64) {
    if admin || quota_bytes <= 0 {
        return (0, 0);
    }
    let soft = quota_bytes as u64;
    let mut hard = soft.saturating_add((soft / 20).max(HARD_MARGIN_MIN));
    let mut soft = soft;
    if total > 0 {
        hard = hard.min(total);
        soft = soft.min(hard);
    }
    (soft, hard)
}

/// The last `quota_status` the server got.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    pub homes: Option<RootStatus>,
    pub shared: Option<RootStatus>,
    /// Unix milliseconds of the last check.
    pub checked_at: Option<i64>,
    /// Why the last check failed (the helper is down or older).
    pub error: Option<String>,
}

impl Detected {
    fn homes_user(&self) -> QuotaState {
        self.homes
            .as_ref()
            .map(|root| root.user)
            .unwrap_or_default()
    }
    fn shared_group(&self) -> QuotaState {
        self.shared
            .as_ref()
            .map(|root| root.group)
            .unwrap_or_default()
    }
}

/// The backend state shared by the request handlers and the syncer.
#[derive(Debug, Default)]
pub struct QuotaBackend {
    detected: RwLock<Detected>,
    wake: Notify,
}

impl QuotaBackend {
    pub fn snapshot(&self) -> Detected {
        self.detected
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn store(&self, detected: Detected) {
        *self
            .detected
            .write()
            .unwrap_or_else(PoisonError::into_inner) = detected;
    }

    /// Whether the kernel keeps the usage of the identities on the homes
    /// filesystem (its numbers replace the HOME scan).
    pub fn kernel_usage(&self) -> bool {
        self.snapshot().homes_user() != QuotaState::Off
    }

    /// `UsageDto.quotaBackend` of a one-HOME account.
    pub fn one_home_backend(&self) -> &'static str {
        if self.snapshot().homes_user() == QuotaState::Enforced {
            QUOTA_BACKEND_FILESYSTEM
        } else {
            QUOTA_BACKEND_RAST
        }
    }

    /// What enforces a shared folder's limit.
    pub fn share_enforced_by(&self) -> &'static str {
        if self.snapshot().shared_group() == QuotaState::Enforced {
            SHARE_ENFORCED_FILESYSTEM_BEST_EFFORT
        } else {
            SHARE_ENFORCED_RAST
        }
    }

    /// The summary of the Control Panel and `rast doctor`.
    pub fn summary(&self) -> Value {
        let detected = self.snapshot();
        json!({
            "backend": self.one_home_backend(),
            "userQuota": detected.homes_user(),
            "groupQuota": detected.shared_group(),
            "shareLimits": self.share_enforced_by(),
            "checkedAt": detected.checked_at,
            "error": detected.error,
        })
    }

    /// Sync again soon (an account's quota or role changed).
    pub fn poke(&self) {
        self.wake.notify_one();
    }
}

/// Parse a `quota_status` reply (untrusted only in the sense that the
/// helper may be older: unknown shapes are an error).
pub fn parse_status(value: &Value) -> AppResult<(RootStatus, RootStatus)> {
    let root = |key: &str| {
        serde_json::from_value::<RootStatus>(value.get(key).cloned().unwrap_or(Value::Null))
            .map_err(|error| {
                AppError::unavailable(format!("unexpected quota_status reply: {error}"))
            })
    };
    Ok((root("homes")?, root("shared")?))
}

/// One reported id: `[id, used, soft, hard, graceEnds]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub user_id: i64,
    pub used: u64,
    pub soft: u64,
    pub hard: u64,
}

pub fn parse_rows(value: &Value) -> AppResult<Vec<Row>> {
    let bad = || AppError::unavailable("unexpected quota_report reply");
    let rows = value
        .get("users")
        .and_then(Value::as_array)
        .ok_or_else(bad)?;
    rows.iter()
        .map(|row| {
            let field = |index: usize| row.get(index).and_then(Value::as_u64).ok_or_else(bad);
            Ok(Row {
                user_id: row.get(0).and_then(Value::as_i64).ok_or_else(bad)?,
                used: field(1)?,
                soft: field(2)?,
                hard: field(3)?,
            })
        })
        .collect()
}

/// The kernel's usage of some accounts.
pub async fn report(native: &NativeClient, user_ids: &[i64]) -> AppResult<Vec<Row>> {
    let mut out = Vec::with_capacity(user_ids.len());
    for chunk in user_ids.chunks(REPORT_CHUNK.min(MAX_REPORT_IDS)) {
        let reply = native
            .call_json("quota_report", json!({ "userIds": chunk }))
            .await
            .map_err(|error| AppError::unavailable(format!("quota report: {error}")))?;
        out.extend(parse_rows(&reply)?);
    }
    Ok(out)
}

/// An account of the sync.
struct Account {
    id: i64,
    admin: bool,
    quota_bytes: i64,
}

/// The one-HOME accounts (active or not: their files still count).
async fn one_home_accounts(state: &AppState) -> AppResult<Vec<Account>> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.role, u.quota_bytes
         FROM users u JOIN linux_identities li ON li.user_id = u.id ORDER BY u.id",
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let role: String = row.get("role");
        let user = crate::auth::AuthUser {
            id: row.get("id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
            role: if role == "admin" {
                crate::auth::Role::Admin
            } else {
                crate::auth::Role::User
            },
            session_id: String::new(),
            quota_bytes: row.get("quota_bytes"),
            must_change_password: false,
        };
        if state.homefs.is_one_home(&state.db, &user).await? {
            out.push(Account {
                id: user.id,
                admin: user.is_admin(),
                quota_bytes: user.quota_bytes,
            });
        }
    }
    Ok(out)
}

/// One pass: check the backend; with user quota on, set the D4 limits that
/// differ and take the used bytes from the kernel.
pub async fn sync_once(state: &AppState, native: &NativeClient) -> AppResult<SyncSummary> {
    let status = native.call_json("quota_status", json!({})).await;
    let mut detected = Detected {
        checked_at: Some(crate::db::now_ms()),
        ..Default::default()
    };
    let (homes, shared) = match status
        .map_err(|error| error.to_string())
        .and_then(|value| parse_status(&value).map_err(|error| error.to_string()))
    {
        Ok(roots) => roots,
        Err(error) => {
            detected.error = Some(error.clone());
            state.quota.store(detected);
            return Err(AppError::unavailable(error));
        }
    };
    let user_state = homes.user;
    let total = homes.total_bytes;
    detected.homes = Some(homes);
    detected.shared = Some(shared);
    state.quota.store(detected);
    let mut summary = SyncSummary {
        user_quota: user_state,
        ..Default::default()
    };
    if user_state == QuotaState::Off {
        return Ok(summary);
    }
    let accounts = one_home_accounts(state).await?;
    let ids: Vec<i64> = accounts.iter().map(|account| account.id).collect();
    let rows = report(native, &ids).await?;
    for account in &accounts {
        let Some(row) = rows.iter().find(|row| row.user_id == account.id) else {
            continue;
        };
        let (soft, hard) = d4_limits(account.quota_bytes, account.admin, total);
        if (row.soft, row.hard) != (soft, hard) {
            match native
                .call_json(
                    "quota_set",
                    json!({"userId": account.id, "softBytes": soft, "hardBytes": hard}),
                )
                .await
            {
                Ok(_) => summary.limits_set += 1,
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(user_id = account.id, error = %error, "cannot set the kernel quota");
                }
            }
        }
        let used = i64::try_from(row.used).unwrap_or(i64::MAX);
        let before = quota::info(&state.db, account.id).await?.used_bytes;
        if before != used {
            quota::set_used(&state.db, account.id, used).await?;
            crate::realtime::publish_quota(&state.events, &state.db, account.id).await;
        }
        summary.accounts += 1;
    }
    Ok(summary)
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub user_quota: QuotaState,
    pub accounts: u64,
    pub limits_set: u64,
    pub failed: u64,
}

/// Start the syncer (native backend only: quota needs the helper).
pub fn spawn(state: AppState) {
    if !state.config.native_enabled {
        return;
    }
    let native = NativeClient::from_config(&state.config);
    tokio::spawn(async move {
        let mut last_error: Option<String> = None;
        loop {
            let wait = match sync_once(&state, &native).await {
                Ok(summary) => {
                    if last_error.take().is_some() || summary.limits_set > 0 {
                        tracing::info!(
                            user_quota = ?summary.user_quota,
                            accounts = summary.accounts,
                            limits_set = summary.limits_set,
                            "quota backend checked"
                        );
                    }
                    SYNC_INTERVAL
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_ref() != Some(&message) {
                        tracing::warn!(error = %message, "quota backend: check failed; RAST accounting applies");
                    }
                    last_error = Some(message);
                    RETRY_AFTER.max(Duration::from_secs(1))
                }
            };
            tokio::select! {
                _ = state.quota.wake.notified() => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// The `rast doctor` lines about quota: `(lines, warnings)`. Read-only:
/// RAST never turns quota on (the runbook is in ADR 0005, section 9).
pub async fn doctor_report(cfg: &crate::config::Config) -> (Vec<String>, usize) {
    let mut lines = vec!["[INFO] storage quota".to_string()];
    let fallback = "RAST accounting: File Station and App Center refuse writes over quota; \
                    apps, Terminal and launchers are not limited";
    if !cfg.native_enabled {
        lines.push(format!("        backend: {fallback} (native backend off)"));
        return (lines, 0);
    }
    let native = NativeClient::from_config(cfg);
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        native.call_json("quota_status", json!({})),
    )
    .await;
    let (homes, shared) = match reply {
        Ok(Ok(value)) => match parse_status(&value) {
            Ok(roots) => roots,
            Err(error) => {
                lines.push(format!("        backend: {fallback} ({error})"));
                return (lines, 0);
            }
        },
        Ok(Err(error)) => {
            lines.push(format!(
                "        backend: {fallback} (quota_status: {error})"
            ));
            return (lines, 0);
        }
        Err(_) => {
            lines.push(format!(
                "        backend: {fallback} (quota_status timed out)"
            ));
            return (lines, 0);
        }
    };
    let state = |state: QuotaState| match state {
        QuotaState::Off => "off",
        QuotaState::Accounting => "accounting only",
        QuotaState::Enforced => "enforced",
    };
    let grace = |secs: Option<u64>| match secs {
        Some(secs) if secs % 86_400 == 0 => format!(", grace {} days", secs / 86_400),
        Some(secs) => format!(", grace {secs} s"),
        None => String::new(),
    };
    lines.push(format!(
        "        homes: {} ({}), user quota {}{}, group quota {}, project quota {}",
        homes.fs_type,
        homes.device,
        state(homes.user),
        grace(homes.user_grace_secs),
        state(homes.group),
        state(homes.project)
    ));
    if shared.device != homes.device {
        lines.push(format!(
            "        shared folders: {} ({}), group quota {}{}, project quota {}",
            shared.fs_type,
            shared.device,
            state(shared.group),
            grace(shared.group_grace_secs),
            state(shared.project)
        ));
    }
    let mut warnings = 0;
    match homes.user {
        QuotaState::Enforced => lines.push(
            "        backend: filesystem (kernel user quota; File Station, Terminal and every app \
             are limited)"
                .to_string(),
        ),
        QuotaState::Accounting => {
            warnings += 1;
            lines.push(format!("        backend: {fallback}"));
            lines.push(
                "[WARN] user quota keeps usage but does not enforce limits; turn enforcement on \
                 (quotaon) to limit apps too"
                    .to_string(),
            );
        }
        QuotaState::Off => lines.push(format!("        backend: {fallback}")),
    }
    if homes.user == QuotaState::Enforced && homes.user_grace_secs != Some(7 * 86_400) {
        lines.push(
            "        note: D4 expects a 7-day grace time (setquota -t -u 604800 604800 <mount>)"
                .to_string(),
        );
    }
    let share_limits = if shared.group == QuotaState::Enforced {
        SHARE_ENFORCED_FILESYSTEM_BEST_EFFORT
    } else {
        SHARE_ENFORCED_RAST
    };
    lines.push(format!("        shared folder limits: {share_limits}"));
    (lines, warnings)
}

/// Shared by the state constructor.
pub fn new_backend() -> Arc<QuotaBackend> {
    Arc::new(QuotaBackend::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d4_limits_follow_the_quota() {
        const GIB: u64 = 1 << 30;
        assert_eq!(d4_limits(0, false, 0), (0, 0));
        assert_eq!(d4_limits(-5, false, 0), (0, 0));
        assert_eq!(d4_limits(10 * GIB as i64, true, 0), (0, 0));
        // 5 % below 20 GiB is under 1 GiB: the margin is 1 GiB.
        assert_eq!(d4_limits(10 * GIB as i64, false, 0), (10 * GIB, 11 * GIB));
        // 5 % of 100 GiB.
        assert_eq!(
            d4_limits(100 * GIB as i64, false, 0),
            (100 * GIB, 105 * GIB)
        );
        // Never over the filesystem.
        assert_eq!(
            d4_limits(100 * GIB as i64, false, 50 * GIB),
            (50 * GIB, 50 * GIB)
        );
        assert_eq!(
            d4_limits(10 * GIB as i64, false, 10 * GIB + 5),
            (10 * GIB, 10 * GIB + 5)
        );
    }

    #[test]
    fn helper_replies_are_parsed_strictly() {
        let root = json!({
            "device": "8:2", "fsType": "ext4", "totalBytes": 100,
            "user": "enforced", "group": "off", "project": "off"
        });
        let (homes, _) = parse_status(&json!({"homes": root, "shared": root})).unwrap();
        assert_eq!(homes.user, QuotaState::Enforced);
        let mut odd = root.clone();
        odd["user"] = json!("maybe");
        assert!(parse_status(&json!({"homes": odd, "shared": root})).is_err());
        assert!(parse_status(&json!({"homes": root})).is_err());
        let rows = parse_rows(&json!({"users": [[3, 10, 20, 30, 0]]})).unwrap();
        assert_eq!(
            rows,
            [Row {
                user_id: 3,
                used: 10,
                soft: 20,
                hard: 30
            }]
        );
        for bad in [
            json!({}),
            json!({"users": [[3, -1, 0, 0, 0]]}),
            json!({"users": [[3]]}),
        ] {
            assert!(parse_rows(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_backend_follows_the_detected_state() {
        let backend = QuotaBackend::default();
        assert_eq!(backend.one_home_backend(), QUOTA_BACKEND_RAST);
        assert_eq!(backend.share_enforced_by(), SHARE_ENFORCED_RAST);
        assert!(!backend.kernel_usage());
        let root = |user, group| RootStatus {
            device: "8:2".into(),
            fs_type: "ext4".into(),
            total_bytes: 1,
            user,
            group,
            project: QuotaState::Off,
            user_grace_secs: None,
            group_grace_secs: None,
        };
        backend.store(Detected {
            homes: Some(root(QuotaState::Accounting, QuotaState::Off)),
            shared: Some(root(QuotaState::Accounting, QuotaState::Accounting)),
            ..Default::default()
        });
        assert!(backend.kernel_usage());
        assert_eq!(backend.one_home_backend(), QUOTA_BACKEND_RAST);
        assert_eq!(backend.share_enforced_by(), SHARE_ENFORCED_RAST);
        backend.store(Detected {
            homes: Some(root(QuotaState::Enforced, QuotaState::Off)),
            shared: Some(root(QuotaState::Enforced, QuotaState::Enforced)),
            ..Default::default()
        });
        assert_eq!(backend.one_home_backend(), QUOTA_BACKEND_FILESYSTEM);
        assert_eq!(
            backend.share_enforced_by(),
            SHARE_ENFORCED_FILESYSTEM_BEST_EFFORT
        );
        assert_eq!(backend.summary()["userQuota"], "enforced");
    }
}
