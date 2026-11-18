//! `rast migrate home` (ADR 0005, section 8): move accounts into one-HOME
//! mode, one at a time, after `rast migrate shares`.
//!
//! The CLI runs as the `rast` service user, reads the accounts, their
//! shared folders and their legacy recycle bin from the database, and asks
//! the helper for every root step:
//!
//! - `--dry-run`: `home_migrate_scan` (read-only); the CLI adds the legacy
//!   recycle bin, the projected usage against the quota, `~/Shared` name
//!   collisions and the shared folders that still need `rast migrate
//!   shares`;
//! - apply: `identity_ensure`, `home_migrate_apply` (journal first, then
//!   owners, ACLs, the merge of the app profiles, the passwd home and the
//!   HOME itself), the legacy recycle bin in batches through
//!   `home_migrate_trash` (its rows are saved below
//!   `<data_dir>/migrate-home/<userId>/` first and deleted once moved), the
//!   `linux_identities` row and `home_ensure`;
//! - `--rollback`: `home_migrate_rollback`, then the recycle bin rows of the
//!   entries the helper moved back and the identity's home in the database.
//!
//! Every account is done on its own: a failure is reported and the others
//! go on. Apply and rollback need the server stopped (the instance lock,
//! D9).
//!
//! With the native pilot (`native_pilot_prefixes`, ADR 0007) a dry run
//! still reports every account and marks the ones outside the pilot;
//! apply and rollback refuse a `--user` outside it before any helper call,
//! and `--all` skips those (listed as skipped). Apply also refuses an
//! account in the pilot that can open a shared folder outside it (in
//! one-HOME mode it could not open it), and recycle bin entries of such
//! shared folders stay in the legacy recycle bin.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::auth::AuthUser;
use crate::config::{Config, NativePilot};
use crate::error::{AppError, AppResult};
use crate::native::client::NativeClient;
use crate::native::protocol::MAX_TRASH_BATCH;
use crate::share_groups::{self, ShareInfo};
use crate::storage::safe_path::{validate_component, LogicalPath};
use crate::users::UserRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Apply,
    Rollback,
}

/// Why apply and rollback refuse an account outside the native pilot.
pub const OUTSIDE_PILOT: &str = "this account is outside the native pilot \
    (native_pilot_prefixes): apply and rollback only run for accounts whose username starts \
    with a pilot prefix; --dry-run still reports it";

/// Refuse apply and rollback of a `--user` outside the native pilot
/// before the server is asked to stop (the CLI's first check; usernames
/// compare in lower case, so the name typed is enough).
pub fn refuse_outside_pilot(pilot: &NativePilot, mode: Mode, name: &str) -> Option<&'static str> {
    (mode != Mode::DryRun && !pilot.allows_user(name)).then_some(OUTSIDE_PILOT)
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Apply => "apply",
            Self::Rollback => "rollback",
        }
    }
}

/// Which accounts a run covers: one, or every one (never by default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    User(String),
    All,
}

/// What a run does with the legacy recycle bin of one account.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashSummary {
    /// Rows of the legacy recycle bin.
    pub rows: u64,
    pub bytes: i64,
    /// Moved into XDG Trash by this run (their rows are deleted).
    pub moved: u64,
    /// Moved by an earlier run that stopped before the rows were deleted.
    pub already: u64,
    /// Rows put back by a rollback.
    pub restored: u64,
    /// Entries that stay in the legacy recycle bin: `<id> (<path>): why`.
    pub left: Vec<String>,
    /// The same entries without their path (`<id>: why`), for
    /// [`hide_paths_outside_pilot`].
    #[serde(skip)]
    left_without_paths: Vec<String>,
}

impl TrashSummary {
    /// An entry that stays in the legacy recycle bin, and why.
    fn leave(&mut self, row: &TrashRow, why: &str) {
        self.left.push(left(row, why));
        self.left_without_paths.push(format!("{}: {why}", row.id));
    }
}

/// Usage after the migration against the account's quota: the HOME, the
/// app data merged into it and the recycle bin all count then.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaProjection {
    /// `0`: no limit.
    pub quota_bytes: i64,
    pub projected_bytes: u64,
    pub over: bool,
}

/// The result for one account.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOutcome {
    pub user_id: i64,
    pub username: String,
    pub ok: bool,
    /// The helper's report (`null` before it answered).
    pub report: Value,
    pub trash: TrashSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaProjection>,
    /// Names in `~/Shared` that the links to shared folders need.
    pub shared_collisions: Vec<String>,
    /// Shared folders the account can open that `rast migrate shares` has
    /// not converted yet: apply refuses until they are.
    pub legacy_shares: Vec<String>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The account is outside the native pilot: a dry run reports it,
    /// `--all` with apply or rollback skips it (no helper call).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outside_pilot: bool,
    /// Shared folders the account can open that are outside the native
    /// pilot: apply refuses while there are any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_pilot_shares: Vec<String>,
}

/// What a run works with.
pub struct Env<'a> {
    pub cfg: &'a Config,
    pub pool: &'a SqlitePool,
    pub native: &'a NativeClient,
}

/// Whether a share's container still has the legacy layout: `rast migrate
/// shares` gives a container to root (and the share's group).
fn container_is_legacy(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.uid() != 0)
}

/// Run one mode for the selected accounts.
pub async fn run(env: &Env<'_>, mode: Mode, selection: &Selection) -> AppResult<Vec<UserOutcome>> {
    run_with(env, mode, selection, &container_is_legacy).await
}

async fn run_with(
    env: &Env<'_>,
    mode: Mode,
    selection: &Selection,
    is_legacy: &(dyn Fn(&Path) -> bool + Sync),
) -> AppResult<Vec<UserOutcome>> {
    let pilot = &env.cfg.native_pilot;
    let accounts = match selection {
        Selection::User(name) => {
            let account = crate::users::find_by_username(env.pool, name)
                .await?
                .ok_or(AppError::NotFound("no account has that name"))?;
            if mode != Mode::DryRun && !pilot.allows_user(&account.username) {
                return Err(AppError::Forbidden(OUTSIDE_PILOT));
            }
            vec![account]
        }
        Selection::All => {
            let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM users ORDER BY id")
                .fetch_all(env.pool)
                .await?;
            let mut accounts = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(user) = crate::users::find_by_id(env.pool, id).await? {
                    accounts.push(user);
                }
            }
            accounts
        }
    };
    let shares = share_groups::plan(env.pool).await?.shares;
    let mut out = Vec::with_capacity(accounts.len());
    for account in accounts {
        let mut outcome = UserOutcome {
            user_id: account.id,
            username: account.username.clone(),
            ok: false,
            report: Value::Null,
            trash: TrashSummary::default(),
            quota: None,
            shared_collisions: Vec::new(),
            legacy_shares: Vec::new(),
            warnings: Vec::new(),
            error: None,
            outside_pilot: !pilot.allows_user(&account.username),
            outside_pilot_shares: Vec::new(),
        };
        if outcome.outside_pilot && mode != Mode::DryRun {
            // `--all`: skipped, never sent to the helper.
            outcome.ok = true;
            out.push(outcome);
            continue;
        }
        let result = match mode {
            Mode::DryRun => dry_run(env, &account, &shares, is_legacy, &mut outcome).await,
            Mode::Apply => apply(env, &account, &shares, is_legacy, &mut outcome).await,
            Mode::Rollback => rollback(env, &account, &mut outcome).await,
        };
        match result {
            Ok(()) => outcome.ok = true,
            Err(error) => outcome.error = Some(error),
        }
        out.push(outcome);
    }
    Ok(out)
}

async fn call(native: &NativeClient, op: &str, params: Value) -> Result<Value, String> {
    native
        .call_json(op, params)
        .await
        .map_err(|error| format!("{op}: {error}"))
}

fn db_error(error: impl std::fmt::Display) -> String {
    format!("database: {error}")
}

/// The name of the account's HOME (and the helper's `username`).
fn home_name(account: &UserRecord) -> String {
    account.username.to_ascii_lowercase()
}

fn auth_user(account: &UserRecord) -> AuthUser {
    AuthUser {
        id: account.id,
        username: account.username.clone(),
        display_name: account.display_name.clone(),
        role: account.role,
        session_id: String::new(),
        quota_bytes: account.quota_bytes,
        must_change_password: false,
    }
}

/// The shared folders the account can open.
async fn accessible<'s>(
    pool: &SqlitePool,
    account: &UserRecord,
    shares: &'s [ShareInfo],
) -> Result<Vec<&'s ShareInfo>, String> {
    let user = auth_user(account);
    let mut out = Vec::new();
    for share in shares {
        let access = crate::access::folder_access_by_id(pool, &user, &share.id)
            .await
            .map_err(db_error)?;
        if access.level.is_some() {
            out.push(share);
        }
    }
    Ok(out)
}

/// Fill in what the CLI checks itself: shared folders still in the legacy
/// layout and `~/Shared` collisions.
async fn check_shares(
    env: &Env<'_>,
    account: &UserRecord,
    shares: &[ShareInfo],
    is_legacy: &(dyn Fn(&Path) -> bool + Sync),
    shared_names: Option<&Value>,
    outcome: &mut UserOutcome,
) -> Result<(), String> {
    let pilot = &env.cfg.native_pilot;
    let (open, outside): (Vec<&ShareInfo>, Vec<&ShareInfo>) = accessible(env.pool, account, shares)
        .await?
        .into_iter()
        .partition(|share| pilot.allows_share(&share.name, &share.container));
    outcome.outside_pilot_shares = outside.iter().map(|share| share.name.clone()).collect();
    let root = env.cfg.storage_dir.join("shared");
    outcome.legacy_shares = open
        .iter()
        .filter(|share| is_legacy(&root.join(&share.container)))
        .map(|share| share.name.clone())
        .collect();
    let existing: Vec<&str> = shared_names
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    outcome.shared_collisions = open
        .iter()
        .filter(|share| {
            existing
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&share.name))
        })
        .map(|share| share.name.clone())
        .collect();
    Ok(())
}

async fn dry_run(
    env: &Env<'_>,
    account: &UserRecord,
    shares: &[ShareInfo],
    is_legacy: &(dyn Fn(&Path) -> bool + Sync),
    outcome: &mut UserOutcome,
) -> Result<(), String> {
    let scan = call(
        env.native,
        "home_migrate_scan",
        json!({"userId": account.id, "username": home_name(account)}),
    )
    .await?;
    check_shares(env, account, shares, is_legacy, scan.get("shared"), outcome).await?;
    if !outcome.outside_pilot_shares.is_empty() {
        outcome.warnings.push(format!(
            "the account can open shared folders outside the native pilot; apply refuses \
             until it cannot: {}",
            outcome.outside_pilot_shares.join(", ")
        ));
    }
    if !outcome.legacy_shares.is_empty() {
        outcome.warnings.push(format!(
            "migrate these shared folders first (rast migrate shares --share <name>): {}",
            outcome.legacy_shares.join(", ")
        ));
    }
    let rows = load_rows(env.pool, account.id).await?;
    outcome.trash.rows = rows.len() as u64;
    outcome.trash.bytes = rows.iter().map(|row| row.size_bytes.max(0)).sum();
    let open = accessible(env.pool, account, shares).await?;
    for row in &rows {
        if let Err(why) = pilot_row(row, shares, &env.cfg.native_pilot)
            .and_then(|()| map_row(row, account.id, shares, &open))
        {
            outcome.trash.leave(row, &why);
        }
    }
    if scan["mode"] == "legacy" {
        let sources: u64 = scan["merge"]["sources"]
            .as_array()
            .map(|sources| sources.iter().filter_map(|s| s["bytes"].as_u64()).sum())
            .unwrap_or(0);
        let projected = scan["tree"]["allocatedBytes"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(sources)
            .saturating_add(outcome.trash.bytes as u64);
        outcome.quota = Some(QuotaProjection {
            quota_bytes: account.quota_bytes.max(0),
            projected_bytes: projected,
            over: account.quota_bytes > 0 && projected > account.quota_bytes as u64,
        });
    }
    if scan["strayHome"] == true {
        outcome.warnings.push(format!(
            "{} exists (D13); RAST does not delete it",
            env.cfg
                .homes_dir()
                .join(format!("rast-u{}", account.id))
                .display()
        ));
    }
    if let Some(units) = scan["units"].as_array().filter(|units| !units.is_empty()) {
        let units: Vec<&str> = units.iter().filter_map(Value::as_str).collect();
        outcome.warnings.push(format!(
            "running units are stopped by the migration: {}",
            units.join(", ")
        ));
    }
    if scan["processes"].as_u64().unwrap_or(0) > 0 {
        outcome.warnings.push(format!(
            "{} process(es) of rast-u{} run; the migration refuses until they stop",
            scan["processes"], account.id
        ));
    }
    if scan["passwdHome"].is_null() {
        outcome.warnings.push(format!(
            "rast-u{} does not exist yet; the migration creates it",
            account.id
        ));
    }
    outcome.report = scan;
    Ok(())
}

async fn apply(
    env: &Env<'_>,
    account: &UserRecord,
    shares: &[ShareInfo],
    is_legacy: &(dyn Fn(&Path) -> bool + Sync),
    outcome: &mut UserOutcome,
) -> Result<(), String> {
    let name = home_name(account);
    check_shares(env, account, shares, is_legacy, None, outcome).await?;
    if !outcome.outside_pilot_shares.is_empty() {
        return Err(format!(
            "the account can open shared folders outside the native pilot, which it could not \
             open in one-HOME mode; remove its access to them first: {}",
            outcome.outside_pilot_shares.join(", ")
        ));
    }
    if !outcome.legacy_shares.is_empty() {
        return Err(format!(
            "these shared folders of the account are not migrated yet; run \
             `rast migrate shares --share <name>` for each first: {}",
            outcome.legacy_shares.join(", ")
        ));
    }
    let identity = call(env.native, "identity_ensure", json!({"userId": account.id})).await?;
    let applied = call(
        env.native,
        "home_migrate_apply",
        json!({"userId": account.id, "username": name}),
    )
    .await?;
    outcome.report = applied.clone();
    if applied["jobs"]["crontab"] == true {
        let identity = format!("rast-u{}", account.id);
        outcome.warnings.push(format!(
            "{identity} has a crontab from before the migration, and cron may still run it \
             outside RAST units: check it with `sudo crontab -l -u {identity}` and remove it \
             with `sudo crontab -r -u {identity}`"
        ));
    }
    let home = env.cfg.home_for(&name);
    save_identity(env.pool, account.id, &identity, &home).await?;
    if applied["alreadyOneHome"] == true && applied["journal"].is_null() {
        // Made in one-HOME mode: there is no legacy recycle bin to move.
        let rows = load_rows(env.pool, account.id).await?;
        for row in &rows {
            outcome.trash.leave(row, "the HOME was never migrated");
        }
        outcome.trash.rows = rows.len() as u64;
    } else {
        move_trash(env, account, &name, shares, &mut outcome.trash).await?;
    }
    if let Err(error) = call(
        env.native,
        "home_ensure",
        json!({"userId": account.id, "username": name}),
    )
    .await
    {
        outcome.warnings.push(format!(
            "the HOME is migrated, but {error}; the server repairs it at the next start"
        ));
    }
    Ok(())
}

async fn rollback(
    env: &Env<'_>,
    account: &UserRecord,
    outcome: &mut UserOutcome,
) -> Result<(), String> {
    let rolled = call(
        env.native,
        "home_migrate_rollback",
        json!({"userId": account.id, "username": home_name(account)}),
    )
    .await?;
    outcome.report = rolled.clone();
    let restored: Vec<String> = rolled["report"]["trashRestored"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    restore_rows(env, account.id, &restored, outcome).await?;
    let legacy = env.cfg.linux_users_dir().join(account.id.to_string());
    sqlx::query("UPDATE linux_identities SET home = ? WHERE user_id = ?")
        .bind(legacy.display().to_string())
        .bind(account.id)
        .execute(env.pool)
        .await
        .map_err(db_error)?;
    Ok(())
}

async fn save_identity(
    pool: &SqlitePool,
    user_id: i64,
    identity: &Value,
    home: &Path,
) -> Result<(), String> {
    let field = |name: &str| identity.get(name).and_then(Value::as_u64);
    let (Some(username), Some(uid), Some(gid)) = (
        identity.get("username").and_then(Value::as_str),
        field("uid"),
        field("gid"),
    ) else {
        return Err("identity_ensure: unexpected reply".into());
    };
    if username != format!("rast-u{user_id}") {
        return Err(format!("identity_ensure: unexpected identity {username:?}"));
    }
    sqlx::query(
        "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET
             username = excluded.username, uid = excluded.uid, gid = excluded.gid,
             home = excluded.home",
    )
    .bind(user_id)
    .bind(username)
    .bind(uid as i64)
    .bind(gid as i64)
    .bind(home.display().to_string())
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The legacy recycle bin
// ---------------------------------------------------------------------------

/// One row of the `trash` table, as saved before it is deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrashRow {
    id: String,
    user_id: i64,
    original_path: String,
    trashed_path: String,
    size_bytes: i64,
    kind: String,
    deleted_at: i64,
}

async fn load_rows(pool: &SqlitePool, user_id: i64) -> Result<Vec<TrashRow>, String> {
    let rows = sqlx::query(
        "SELECT id, user_id, original_path, trashed_path, size_bytes, kind, deleted_at
         FROM trash WHERE user_id = ? ORDER BY deleted_at, id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    Ok(rows
        .into_iter()
        .map(|row| TrashRow {
            id: row.get("id"),
            user_id: row.get("user_id"),
            original_path: row.get("original_path"),
            trashed_path: row.get("trashed_path"),
            size_bytes: row.get("size_bytes"),
            kind: row.get("kind"),
            deleted_at: row.get("deleted_at"),
        })
        .collect())
}

fn left(row: &TrashRow, why: &str) -> String {
    format!("{} ({}): {why}", row.id, row.original_path)
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// With the native pilot: a row of a shared folder outside it stays in the
/// legacy recycle bin (no helper call names that container).
fn pilot_row(row: &TrashRow, shares: &[ShareInfo], pilot: &NativePilot) -> Result<(), String> {
    if !pilot.is_active() {
        return Ok(());
    }
    let Ok(logical) = LogicalPath::parse(&row.original_path) else {
        return Ok(());
    };
    let Some((name, _)) = logical.shared_parts() else {
        return Ok(());
    };
    match shares
        .iter()
        .find(|share| share.name.eq_ignore_ascii_case(name))
    {
        Some(share) if !pilot.allows_share(&share.name, &share.container) => {
            Err("its shared folder is outside the native pilot".into())
        }
        _ => Ok(()),
    }
}

/// The helper's entry for a row: where it came from, below the HOME or
/// below a shared folder's container. The helper refuses a whole batch
/// with an invalid entry, so everything it checks is checked here first.
/// A row of a shared folder maps only to one the account can open now
/// (`open`, review F8): the payload of a folder it lost stays in the
/// legacy recycle bin.
fn map_row(
    row: &TrashRow,
    user_id: i64,
    shares: &[ShareInfo],
    open: &[&ShareInfo],
) -> Result<Value, String> {
    if !valid_uuid(&row.id) {
        return Err("its id is not a lowercase UUID".into());
    }
    if row.trashed_path != format!("{user_id}/{}", row.id) {
        return Err("its data is not in the account's recycle bin".into());
    }
    if row.deleted_at < 0 {
        return Err("its deletion time is invalid".into());
    }
    let logical = LogicalPath::parse(&row.original_path)
        .map_err(|_| "its original path is invalid".to_string())?;
    let (container, segments) = match logical.shared_parts() {
        Some((name, rest)) => {
            let share = shares
                .iter()
                .find(|share| share.name.eq_ignore_ascii_case(name))
                .ok_or("its shared folder no longer exists")?;
            if !open.iter().any(|known| known.id == share.id) {
                return Err("the account can no longer open its shared folder".into());
            }
            (Some(share.container.clone()), rest.to_vec())
        }
        None => (None, logical.segments().to_vec()),
    };
    if segments.is_empty()
        || segments
            .iter()
            .any(|part| validate_component(part).is_err())
    {
        return Err("its original path is invalid".into());
    }
    let path = segments.join("/");
    if path.len() > 4096 {
        return Err("its original path is too long".into());
    }
    let mut entry = json!({"id": row.id, "deletedAt": row.deleted_at, "path": path});
    if let Some(container) = container {
        entry["container"] = json!(container);
    }
    Ok(entry)
}

/// `<data_dir>/migrate-home/<userId>`: the saved rows of the account.
fn rows_dir(cfg: &Config, user_id: i64) -> PathBuf {
    cfg.data_dir.join("migrate-home").join(user_id.to_string())
}

const ROWS_PREFIX: &str = "trash-rows-";
const ROWS_SUFFIX: &str = ".jsonl";
const ROLLED_BACK: &str = ".rolled-back";

/// Save the rows before any of them is deleted (0700 folder, 0600 file,
/// synced).
fn save_rows(cfg: &Config, user_id: i64, rows: &[&TrashRow]) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    // `create_new` (O_EXCL) never opens an existing file or link.
    let failed = |error: std::io::Error| format!("cannot save the recycle bin rows: {error}");
    let dir = rows_dir(cfg, user_id);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(failed)?;
    let mut stamp = crate::db::now_ms();
    let (path, mut file) = loop {
        let path = dir.join(format!("{ROWS_PREFIX}{stamp}{ROWS_SUFFIX}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => stamp += 1,
            Err(error) => return Err(failed(error)),
        }
    };
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row).map_err(|error| error.to_string())?);
        text.push('\n');
    }
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    if let Ok(dir) = std::fs::File::open(&dir) {
        let _ = dir.sync_all();
    }
    Ok(path)
}

/// The saved rows of the account that no rollback used yet, by id, and
/// the files that hold them.
fn saved_rows(
    cfg: &Config,
    user_id: i64,
) -> Result<(BTreeMap<String, TrashRow>, Vec<PathBuf>), String> {
    let dir = rows_dir(cfg, user_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), Vec::new()))
        }
        Err(error) => return Err(format!("cannot read {}: {error}", dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(ROWS_PREFIX) && name.ends_with(ROWS_SUFFIX))
        })
        .collect();
    files.sort();
    let mut rows = BTreeMap::new();
    for path in &files {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let row: TrashRow = serde_json::from_str(&line)
                .map_err(|_| format!("line {} of {} is invalid", index + 1, path.display()))?;
            if row.user_id != user_id || row.trashed_path != format!("{user_id}/{}", row.id) {
                return Err(format!(
                    "line {} of {} is not a row of this account",
                    index + 1,
                    path.display()
                ));
            }
            rows.insert(row.id.clone(), row);
        }
    }
    Ok((rows, files))
}

async fn move_trash(
    env: &Env<'_>,
    account: &UserRecord,
    name: &str,
    shares: &[ShareInfo],
    summary: &mut TrashSummary,
) -> Result<(), String> {
    let rows = load_rows(env.pool, account.id).await?;
    summary.rows = rows.len() as u64;
    summary.bytes = rows.iter().map(|row| row.size_bytes.max(0)).sum();
    let open = accessible(env.pool, account, shares).await?;
    let mut entries = Vec::new();
    for row in &rows {
        match pilot_row(row, shares, &env.cfg.native_pilot)
            .and_then(|()| map_row(row, account.id, shares, &open))
        {
            Ok(entry) => entries.push((row, entry)),
            Err(why) => summary.leave(row, &why),
        }
    }
    if entries.is_empty() {
        return Ok(());
    }
    let saved: Vec<&TrashRow> = entries.iter().map(|(row, _)| *row).collect();
    save_rows(env.cfg, account.id, &saved)?;
    for batch in entries.chunks(MAX_TRASH_BATCH) {
        let reply = call(
            env.native,
            "home_migrate_trash",
            json!({
                "userId": account.id,
                "username": name,
                "entries": batch.iter().map(|(_, entry)| entry.clone()).collect::<Vec<_>>(),
            }),
        )
        .await?;
        let by_id: BTreeMap<&str, &TrashRow> = batch
            .iter()
            .map(|(row, _)| (row.id.as_str(), *row))
            .collect();
        for result in reply["results"].as_array().into_iter().flatten() {
            let Some(row) = result["id"].as_str().and_then(|id| by_id.get(id)) else {
                continue;
            };
            match result["status"].as_str() {
                Some(status @ ("moved" | "already")) => {
                    sqlx::query("DELETE FROM trash WHERE id = ? AND user_id = ?")
                        .bind(&row.id)
                        .bind(account.id)
                        .execute(env.pool)
                        .await
                        .map_err(db_error)?;
                    if status == "moved" {
                        summary.moved += 1;
                    } else {
                        summary.already += 1;
                    }
                }
                Some("missing") => summary.leave(row, "its data is missing"),
                _ => summary.leave(
                    row,
                    result["message"]
                        .as_str()
                        .unwrap_or("it could not be moved"),
                ),
            }
        }
    }
    Ok(())
}

async fn restore_rows(
    env: &Env<'_>,
    user_id: i64,
    restored: &[String],
    outcome: &mut UserOutcome,
) -> Result<(), String> {
    let (rows, files) = saved_rows(env.cfg, user_id)?;
    let mut seen = HashSet::new();
    for id in restored {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let Some(row) = rows.get(id) else {
            outcome.warnings.push(format!(
                "recycle bin entry {id} is back in the legacy recycle bin, but its row \
                 was not saved; it is not listed in File Station"
            ));
            continue;
        };
        let done = sqlx::query(
            "INSERT OR IGNORE INTO trash
                 (id, user_id, original_path, trashed_path, size_bytes, kind, deleted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(row.user_id)
        .bind(&row.original_path)
        .bind(&row.trashed_path)
        .bind(row.size_bytes)
        .bind(&row.kind)
        .bind(row.deleted_at)
        .execute(env.pool)
        .await
        .map_err(db_error)?;
        outcome.trash.restored += done.rows_affected();
    }
    for file in files {
        let mut target = file.clone().into_os_string();
        target.push(ROLLED_BACK);
        std::fs::rename(&file, &target)
            .map_err(|error| format!("cannot mark {} as rolled back: {error}", file.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rast doctor
// ---------------------------------------------------------------------------

/// `rast doctor`: the accounts in one-HOME mode (their HOME is a folder of
/// their identity, as `is_one_home` checks, never followed).
///
/// - Review F13: an account outside the native pilot, or on a server with
///   native apps off, is stranded: File Station runs in-process as `rast`
///   and cannot open the HOME, backups skip it, and the RAST quota applies
///   again (the files are intact). Roll such accounts back first.
/// - Review F3 (decision A7): the identity of an account in one-HOME mode
///   must be in `<etc>/cron.deny`, and in `<etc>/at.deny` when that file
///   exists (without it only root may use `at`). Files this user cannot
///   read are named once.
///
/// Read-only, as the service user.
pub async fn doctor_report(cfg: &Config, pool: &SqlitePool, etc: &Path) -> (Vec<String>, usize) {
    use std::os::unix::fs::MetadataExt;
    let mut lines = Vec::new();
    let mut warnings = 0;
    let rows = match sqlx::query(
        "SELECT u.username, li.username AS identity, li.uid
         FROM users u JOIN linux_identities li ON li.user_id = u.id ORDER BY u.id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                vec![format!("[WARN] cannot list the one-HOME accounts: {error}")],
                1,
            )
        }
    };
    let mut one_home = Vec::new();
    for row in rows {
        let username: String = row.get("username");
        let identity: String = row.get("identity");
        let uid: i64 = row.get("uid");
        let name = username.to_ascii_lowercase();
        if validate_component(&name).is_err() {
            continue;
        }
        let owned = std::fs::symlink_metadata(cfg.home_for(&name))
            .is_ok_and(|meta| meta.is_dir() && i64::from(meta.uid()) == uid);
        if owned {
            one_home.push((username, identity));
        }
    }
    let read = |file: &str| match std::fs::read_to_string(etc.join(file)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    let cron = read(crate::native::job_deny::CRON_DENY);
    let at = read(crate::native::job_deny::AT_DENY);
    let mut unreadable = BTreeMap::new();
    let mut active = 0;
    for (username, identity) in &one_home {
        let reason = if !cfg.native_enabled {
            Some("native apps are off (native_enabled = false)")
        } else if !cfg.native_pilot.allows_user(username) {
            Some("it is outside the native pilot (native_pilot_prefixes)")
        } else {
            None
        };
        if let Some(reason) = reason {
            warnings += 1;
            lines.push(format!(
                "[WARN] the account {username} is in one-HOME mode, but {reason}: File Station \
                 cannot open its HOME, backups skip it and the RAST quota applies again (its \
                 files are intact). Put it back in the pilot with native apps on, and run \
                 `rast migrate home --rollback --user {username}` before you take it out"
            ));
            continue;
        }
        active += 1;
        for (file, text, required) in [
            (crate::native::job_deny::CRON_DENY, &cron, true),
            (crate::native::job_deny::AT_DENY, &at, false),
        ] {
            let listed = match text {
                Ok(Some(text)) => crate::native::job_deny::lists(text, identity),
                Ok(None) => !required,
                Err(error) => {
                    unreadable.insert(file, error.to_string());
                    continue;
                }
            };
            if !listed {
                warnings += 1;
                lines.push(format!(
                    "[WARN] the account {username} is in one-HOME mode, but {identity} is not in \
                     {}: its jobs start outside RAST units and keep the groups of shared folders \
                     it loses; run `rast migrate home --user {username}` again to list it",
                    etc.join(file).display()
                ));
            }
        }
    }
    for (file, error) in unreadable {
        lines.push(format!(
            "[INFO] cannot read {} ({error}); run `sudo rast doctor` to check that one-HOME \
             identities are listed in it",
            etc.join(file).display()
        ));
    }
    if !one_home.is_empty() && warnings == 0 {
        lines.insert(
            0,
            format!("[    ] one-HOME accounts ... ok ({active} in the native pilot)"),
        );
    }
    (lines, warnings)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn count(value: &Value) -> u64 {
    value["count"].as_u64().unwrap_or(0)
}

/// `text: sample`, or `text` alone when the sample is empty (paths hidden).
fn with_sample(text: String, sample: String) -> String {
    if sample.is_empty() {
        text
    } else {
        format!("{text}: {sample}")
    }
}

fn sample(value: &Value, limit: usize) -> String {
    value["paths"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .take(limit)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Lines for a tree report (`HomeTree` of the helper).
fn tree_lines(tree: &Value, done: bool, lines: &mut Vec<String>) {
    lines.push(format!(
        "  HOME: {} folders, {} files, {} bytes ({} on disk); {} entries {} the identity",
        tree["dirs"].as_u64().unwrap_or(0),
        tree["files"].as_u64().unwrap_or(0),
        tree["bytes"].as_u64().unwrap_or(0),
        tree["allocatedBytes"].as_u64().unwrap_or(0),
        count(&tree["changeOwner"]),
        if done { "given to" } else { "to give to" },
    ));
    for (key, label) in [
        (
            "identityAclEntries",
            "entries with ACL entries of RAST identities, such as the 0.18.3 read grant (removed)",
        ),
        ("foreignAclEntries", "entries with other ACL entries (kept)"),
        ("hardLinks", "files with several hard links (skipped)"),
        ("symlinks", "symlinks (left unchanged)"),
        ("rootOwned", "entries owned by root (left unchanged)"),
        (
            "otherOwner",
            "entries owned by other accounts (left unchanged)",
        ),
        (
            "otherDevice",
            "mount points of other file systems (skipped)",
        ),
        ("special", "special files (skipped)"),
        ("changedDuringWalk", "entries that changed during the walk"),
    ] {
        let n = count(&tree[key]);
        if n > 0 {
            lines.push(with_sample(format!("  {n} {label}"), sample(&tree[key], 5)));
        }
    }
    if tree["nonUtf8"].as_u64().unwrap_or(0) > 0 {
        lines.push(format!(
            "  {} names that are not UTF-8 (skipped)",
            tree["nonUtf8"]
        ));
    }
}

/// Lines for a merge report (`MergeReport` of the helper).
fn merge_lines(merge: &Value, done: bool, lines: &mut Vec<String>) {
    let sources = merge["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        lines.push("  no app data to merge".to_string());
        return;
    }
    lines.push(format!(
        "  app data {} the HOME ({} moves, {} folders created):",
        if done { "merged into" } else { "to merge into" },
        merge["moves"].as_u64().unwrap_or(0),
        merge["foldersCreated"].as_u64().unwrap_or(0),
    ));
    for source in &sources {
        let mut line = format!(
            "    {}{}: {} moves, {} conflicts, {} skipped",
            source["source"].as_str().unwrap_or("?"),
            source["path"]
                .as_str()
                .map(|path| format!(" ({path})"))
                .unwrap_or_default(),
            source["moves"].as_u64().unwrap_or(0),
            source["conflicts"].as_u64().unwrap_or(0),
            source["skipped"].as_u64().unwrap_or(0),
        );
        if source["files"].as_u64().is_some() && !done {
            line.push_str(&format!(
                "; {} files, {} bytes",
                source["files"].as_u64().unwrap_or(0),
                source["bytes"].as_u64().unwrap_or(0)
            ));
        }
        if source["otherFileSystem"] == true {
            line.push_str("; on another file system (not moved)");
        }
        lines.push(line);
    }
    let conflicts = count(&merge["conflicts"]);
    if conflicts > 0 {
        lines.push(with_sample(
            format!("  {conflicts} conflicts (they stay in the old profile)"),
            sample(&merge["conflicts"], 50),
        ));
    }
    let skipped = count(&merge["skipped"]);
    if skipped > 0 {
        lines.push(with_sample(
            format!("  {skipped} skipped"),
            sample(&merge["skipped"], 10),
        ));
    }
}

fn trash_lines(mode: Mode, trash: &TrashSummary, lines: &mut Vec<String>) {
    match mode {
        Mode::DryRun if trash.rows > 0 => lines.push(format!(
            "  recycle bin: {} entries, {} bytes, to move into XDG Trash",
            trash.rows, trash.bytes
        )),
        Mode::Apply if trash.rows > 0 => lines.push(format!(
            "  recycle bin: {} entries moved into XDG Trash, {} moved before",
            trash.moved, trash.already
        )),
        Mode::Rollback if trash.restored > 0 => lines.push(format!(
            "  recycle bin: {} entries back in the legacy recycle bin",
            trash.restored
        )),
        _ => {}
    }
    if !trash.left.is_empty() {
        lines.push(format!(
            "  {} recycle bin entries stay in the legacy recycle bin:",
            trash.left.len()
        ));
        for entry in trash.left.iter().take(20) {
            lines.push(format!("    {entry}"));
        }
        if trash.left.len() > 20 {
            lines.push(format!(
                "    and {} more (see --json)",
                trash.left.len() - 20
            ));
        }
    }
}

/// Accounts outside the native pilot (ADR 0007, review F10, decision A9):
/// a dry run shows only counts and sizes for them, no paths and no ACL
/// entry text, unless `--show-paths` is given. Accounts in the pilot keep
/// every detail.
pub fn hide_paths_outside_pilot(outcomes: &mut [UserOutcome]) {
    for outcome in outcomes.iter_mut().filter(|outcome| outcome.outside_pilot) {
        crate::migrate_shares::redact_paths(&mut outcome.report);
        outcome.trash.left = std::mem::take(&mut outcome.trash.left_without_paths);
    }
}

/// Human-readable lines for one outcome.
pub fn describe(mode: Mode, outcome: &UserOutcome) -> Vec<String> {
    if outcome.outside_pilot && mode != Mode::DryRun {
        return vec![format!(
            "{} (user {}): skipped (outside the native pilot)",
            outcome.username, outcome.user_id
        )];
    }
    let mut lines = describe_outcome(mode, outcome);
    if outcome.outside_pilot {
        lines.insert(
            1,
            "  outside the native pilot: apply and rollback refuse this account".to_string(),
        );
    }
    lines
}

fn describe_outcome(mode: Mode, outcome: &UserOutcome) -> Vec<String> {
    let report = &outcome.report;
    let status = match (&outcome.error, mode) {
        (Some(error), _) => format!("FAILED: {error}"),
        (None, Mode::DryRun) => match report["mode"].as_str() {
            Some("legacy") if report["journal"].is_string() => {
                "legacy HOME; an interrupted migration resumes".to_string()
            }
            Some("legacy") => "legacy HOME, would be migrated".to_string(),
            Some("oneHome") if report["applied"] == false && report["journal"].is_string() => {
                "an interrupted migration resumes".to_string()
            }
            Some("oneHome") => "already in one-HOME mode".to_string(),
            Some("missing") => "no HOME folder (nothing to migrate)".to_string(),
            Some(other) => format!("the HOME belongs to someone else ({other})"),
            None => "unknown".to_string(),
        },
        (None, Mode::Apply) if report["alreadyOneHome"] == true => {
            "already in one-HOME mode".to_string()
        }
        (None, Mode::Apply) if report["resumed"] == true => {
            "migrated (resumed with the existing journal)".to_string()
        }
        (None, Mode::Apply) => "migrated to one-HOME mode".to_string(),
        (None, Mode::Rollback) => "rolled back to the legacy layout".to_string(),
    };
    let mut lines = vec![format!(
        "{} (user {}): {status}",
        outcome.username, outcome.user_id
    )];
    if mode == Mode::Apply && outcome.error.is_some() && !outcome.legacy_shares.is_empty() {
        return lines;
    }
    if outcome.error.is_some() && report.is_null() {
        return lines;
    }
    match mode {
        Mode::DryRun | Mode::Apply => {
            if report["tree"].is_object() {
                tree_lines(&report["tree"], mode == Mode::Apply, &mut lines);
            }
            if report["merge"].is_object() {
                merge_lines(&report["merge"], mode == Mode::Apply, &mut lines);
            }
            if mode == Mode::Apply && report["steps"].is_object() {
                let steps = &report["steps"];
                let missing = count(&steps["missing"]);
                let conflicts = count(&steps["conflicts"]);
                if missing + conflicts > 0 {
                    lines.push(format!(
                        "  {missing} planned moves found nothing to move, {conflicts} found \
                         their target taken: {}",
                        [sample(&steps["missing"], 5), sample(&steps["conflicts"], 5)]
                            .into_iter()
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            if let Some(units) = report["stopped"]
                .as_array()
                .filter(|units| !units.is_empty())
            {
                let units: Vec<&str> = units.iter().filter_map(Value::as_str).collect();
                lines.push(format!("  stopped: {}", units.join(", ")));
            }
        }
        Mode::Rollback => {
            let rolled = &report["report"];
            lines.push(format!(
                "  {} entries restored; {} made since given back to the service, {} changed \
                 since (left as they are), {} missing",
                rolled["restored"].as_u64().unwrap_or(0),
                count(&rolled["returned"]),
                count(&rolled["changedSince"]),
                count(&rolled["missing"]),
            ));
            let steps = &rolled["steps"];
            lines.push(format!(
                "  app data: {} entries moved back, {} folders removed, {} not found, {} \
                 left in the HOME",
                steps["moved"].as_u64().unwrap_or(0),
                steps["created"].as_u64().unwrap_or(0),
                count(&steps["missing"]),
                count(&steps["conflicts"]),
            ));
            if let Some(links) = rolled["sharedLinksRemoved"]
                .as_array()
                .filter(|links| !links.is_empty())
            {
                let names: Vec<&str> = links.iter().filter_map(Value::as_str).collect();
                lines.push(format!(
                    "  shared folder links removed from ~/Shared: {}{}",
                    names.join(", "),
                    if rolled["sharedFolderRemoved"].as_bool().unwrap_or(false) {
                        " (and the empty ~/Shared folder)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    trash_lines(mode, &outcome.trash, &mut lines);
    if let Some(quota) = &outcome.quota {
        lines.push(if quota.quota_bytes > 0 {
            format!(
                "  projected usage: {} of {} bytes{}",
                quota.projected_bytes,
                quota.quota_bytes,
                if quota.over { " (OVER QUOTA)" } else { "" }
            )
        } else {
            format!(
                "  projected usage: {} bytes (no quota)",
                quota.projected_bytes
            )
        });
    }
    if !outcome.shared_collisions.is_empty() {
        lines.push(format!(
            "  ~/Shared already holds entries named like shared folders (their links are \
             not made): {}",
            outcome.shared_collisions.join(", ")
        ));
    }
    if let Some(journal) = report["journal"].as_str() {
        lines.push(format!(
            "  journal: <data_dir>/migrations/home/{}/{journal}",
            outcome.user_id
        ));
    }
    for warning in &outcome.warnings {
        lines.push(format!("  warning: {warning}"));
    }
    lines
}

#[cfg(test)]
mod tests;
