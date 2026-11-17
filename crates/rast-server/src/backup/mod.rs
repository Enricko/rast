//! Backup engine.
//!
//! Backups are self-contained `.tar.gz` archives containing:
//!
//! ```text
//! manifest.json      format version, schema version, scope, file checksums
//! db/rast.db         consistent SQLite snapshot (VACUUM INTO, WAL safe)
//! config/rast.toml   configuration (if present)
//! config/rast.env    environment file (if present)
//! config/secret.key  server master key (needed to decrypt app secrets)
//! apps/apps.json     managed deployment metadata (compose + encrypted env)
//! users/...          optional copy of user homes and shared folders
//! ```
//!
//! The live database is never copied directly: `VACUUM INTO` produces a
//! transactionally consistent snapshot including committed WAL data.
//!
//! The `users` scope reads legacy HOMEs and shared folders (owned by the
//! service) itself. HOMEs in one-HOME mode and migrated shared folders are
//! read through the root backup reader ([`reader`], ADR 0005 D12), which
//! keeps numeric owners, modes and ACLs; the manifest marks those entries
//! `ownersPreserved` and lists their trees in `preservedTrees`. The
//! `users` scope stops with an error before the backup folder's file
//! system has less than [`MIN_FREE_BYTES`] free ([`SpaceGuard`]), and each
//! tree read through the reader has a byte budget
//! ([`reader::tree_budget`]; review F7). Restore user files with the root
//! command `rast restore-files` ([`restore_files`]), never with a raw `tar`
//! as root (review F1).

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db,
    error::{AppError, AppResult},
    native::{backup_reader::TreeKind, client::NativeClient},
};

pub mod reader;
pub mod restore_files;
pub mod schedule;

pub const BACKUP_FORMAT: u32 = 1;
pub const SCOPES: [&str; 4] = ["db", "config", "users", "apps"];
pub const BACKUP_EXTENSION: &str = "tar.gz";

pub type ProgressFn = Arc<dyn Fn(u64, u64, String) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// Written through the backup reader: the archive keeps the entry's
    /// numeric owner and group, its mode and its ACLs (restore it as root
    /// with `tar --same-owner`). Absent for entries the service read.
    #[serde(
        default,
        rename = "ownersPreserved",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub owners_preserved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub app: String,
    pub app_version: String,
    pub schema_version: i64,
    pub created_at: i64,
    pub hostname: String,
    pub scopes: Vec<String>,
    pub note: String,
    pub entries: Vec<ManifestEntry>,
    /// What the backup left out, and why (for example a HOME in one-HOME
    /// mode when the backup reader is unavailable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The folders written through the backup reader
    /// (`users/homes/<name>`, `users/shared/<container>`): their entries
    /// keep numeric owners, modes and ACLs. Everything else under `users/`
    /// was read by the service and carries neither.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved_trees: Vec<String>,
}

/// Metadata returned to the API/CLI after a backup completes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupMeta {
    pub id: String,
    pub file_name: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub duration_ms: i64,
    /// What the backup left out, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Warnings listed at most (the rest is counted).
const MAX_BACKUP_WARNINGS: usize = 50;

/// Free space the `users` scope leaves on the file system of the backup's
/// work folder (review F7): below it the backup stops with an error rather
/// than filling the disk.
pub const MIN_FREE_BYTES: u64 = 1 << 30;
/// Bytes written between two measurements of [`SpaceGuard`].
const SPACE_CHECK_EVERY: u64 = 64 << 20;

/// Stops a backup before its file system is full.
pub struct SpaceGuard {
    path: PathBuf,
    floor: u64,
    since_check: u64,
}

impl SpaceGuard {
    pub fn new(path: PathBuf, floor: u64) -> Self {
        Self {
            path,
            floor,
            since_check: 0,
        }
    }

    /// Measure now: an error when less than the floor is free.
    pub fn check(&mut self) -> std::io::Result<()> {
        self.since_check = 0;
        if self.floor == 0 {
            return Ok(());
        }
        match crate::native::backup_reader::available_bytes(&self.path) {
            Some(free) if free < self.floor => Err(std::io::Error::other(format!(
                "the file system of {} has {free} bytes free, less than the {} bytes a backup \
                 must leave; the backup stops",
                self.path.display(),
                self.floor
            ))),
            _ => Ok(()),
        }
    }

    /// `bytes` were written: measure again every [`SPACE_CHECK_EVERY`].
    pub fn wrote(&mut self, bytes: u64) -> std::io::Result<()> {
        self.since_check = self.since_check.saturating_add(bytes);
        if self.since_check >= SPACE_CHECK_EVERY {
            self.check()
        } else {
            Ok(())
        }
    }
}

/// Where the `users` scope may read from. Tests replace the owners (they
/// cannot chown) and the helper.
pub(crate) struct BackupEnv {
    /// The owner of legacy HOMEs and containers: the service (by default
    /// the owner of the backup's work folder).
    pub service_uid: Option<u32>,
    /// The owner of migrated shared folder containers: root.
    pub container_owner_uid: u32,
    /// The native helper, when native execution is on.
    pub native: Option<NativeClient>,
    /// Free space the `users` scope leaves ([`MIN_FREE_BYTES`]).
    pub min_free_bytes: u64,
}

impl BackupEnv {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self {
            service_uid: None,
            container_owner_uid: 0,
            native: (cfg.native_enabled && cfg!(target_os = "linux"))
                .then(|| NativeClient::from_config(cfg)),
            min_free_bytes: MIN_FREE_BYTES,
        }
    }
}

/// What the database says about the folders below the storage roots.
struct Facts {
    accounts: HashSet<String>,
    /// Quota of each account (bytes, 0: none).
    account_quota: HashMap<String, u64>,
    /// Shared folder name by container.
    shares: HashMap<String, String>,
    /// Quota of each shared folder by container (bytes).
    share_quota: HashMap<String, u64>,
}

async fn load_facts(cfg: &Config, pool: Option<&SqlitePool>) -> Result<Facts, String> {
    let owned;
    let pool = match pool {
        Some(pool) => pool,
        None => {
            owned = db::connect(&cfg.db_path(), 2)
                .await
                .map_err(|error| error.to_string())?;
            &owned
        }
    };
    let accounts: Vec<(String, i64)> = sqlx::query_as("SELECT username, quota_bytes FROM users")
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;
    let shares: Vec<(String, String, Option<i64>)> =
        sqlx::query_as("SELECT container_path, name, quota_bytes FROM shares")
            .fetch_all(pool)
            .await
            .map_err(|error| error.to_string())?;
    let quota = |bytes: Option<i64>| bytes.and_then(|bytes| u64::try_from(bytes).ok());
    Ok(Facts {
        accounts: accounts.iter().map(|(name, _)| name.clone()).collect(),
        account_quota: accounts
            .iter()
            .filter_map(|(name, bytes)| Some((name.clone(), quota(Some(*bytes))?)))
            .collect(),
        share_quota: shares
            .iter()
            .filter_map(|(container, _, bytes)| Some((container.clone(), quota(*bytes)?)))
            .collect(),
        shares: shares
            .into_iter()
            .map(|(container, name, _)| (container, name))
            .collect(),
    })
}

/// How the `users` scope reads the folders at the top of the homes and
/// shared roots.
#[derive(Default)]
struct UsersPlan {
    /// Read through the backup reader.
    trees: Vec<reader::Tree>,
    /// Top-level HOMEs the service does not read (reader or skipped).
    not_in_process_homes: HashSet<OsString>,
    /// Top-level containers the service does not read.
    not_in_process_shared: HashSet<OsString>,
    warnings: Vec<String>,
    reader: Option<reader::ReaderAccess>,
}

/// The folders directly below `root` with their owner (links and files are
/// left to the walk, as before).
fn top_folders(root: &Path) -> Vec<(OsString, u32)> {
    use std::os::unix::fs::MetadataExt;
    let mut found: Vec<(OsString, u32)> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let meta = entry.path().symlink_metadata().ok()?;
                    meta.is_dir().then(|| (entry.file_name(), meta.uid()))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn skipped_home(name: &str, owner: &str, reason: &str) -> String {
    format!(
        "users/homes/{name}: skipped, the HOME belongs to {owner} (one-HOME mode) and the RAST \
         service cannot read it; {reason}; back it up as root (see docs/OPERATIONS.md)"
    )
}

/// Decide, folder by folder, who reads what. A HOME the service owns is
/// legacy and read by the service (as before). A HOME that belongs to
/// another uid is in one-HOME mode: it goes through the backup reader when
/// the helper is there, a RAST account has that name and the account is in
/// the native pilot (ADR 0007); otherwise it is skipped with a warning. A
/// container owned by root is a migrated shared folder: it goes through
/// the reader when its shared folder is in the pilot, and the service reads
/// it otherwise (as before, without owners).
async fn plan_users(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    env: &BackupEnv,
    service_uid: u32,
) -> UsersPlan {
    let mut plan = UsersPlan::default();
    let homes: Vec<(OsString, u32)> = top_folders(&cfg.homes_dir())
        .into_iter()
        .filter(|(_, uid)| *uid != service_uid)
        .collect();
    let containers: Vec<(OsString, u32)> = top_folders(&cfg.shared_dir())
        .into_iter()
        .filter(|(_, uid)| *uid == env.container_owner_uid && *uid != service_uid)
        .collect();
    if homes.is_empty() && containers.is_empty() {
        return plan;
    }
    let facts = match &env.native {
        Some(_) => Some(load_facts(cfg, pool).await),
        None => None,
    };
    for (name, uid) in homes {
        plan.not_in_process_homes.insert(name.clone());
        let shown = name.to_string_lossy().into_owned();
        let budget = match &facts {
            Some(Ok(facts)) => reader::tree_budget(facts.account_quota.get(&shown).copied()),
            _ => None,
        };
        let reason = match (name.to_str(), &facts) {
            (_, None) => Some(
                "native Linux apps are off on this server, so the backup reader is unavailable"
                    .to_string(),
            ),
            (None, _) => Some("it is not the folder of a RAST account".to_string()),
            (Some(_), Some(Err(error))) => Some(format!("cannot look up its account: {error}")),
            (Some(name), Some(Ok(facts))) => {
                if crate::native::protocol::validate_home_username(name).is_err()
                    || !facts.accounts.contains(name)
                {
                    Some("no RAST account has this name".to_string())
                } else if !cfg.native_pilot.allows_user(name) {
                    Some(
                        "the account is outside the native pilot (native_pilot_prefixes), so the \
                         backup reader does not read it"
                            .to_string(),
                    )
                } else {
                    None
                }
            }
        };
        match reason {
            Some(reason) => {
                tracing::warn!(home = %shown, uid, %reason, "backup skips a HOME in one-HOME mode");
                plan.warnings
                    .push(skipped_home(&shown, &format!("uid {uid}"), &reason));
            }
            None => plan.trees.push(reader::Tree {
                kind: TreeKind::Home,
                name: shown,
                budget,
            }),
        }
    }
    if let Some(Ok(facts)) = &facts {
        for (container, _) in containers {
            let Some(text) = container.to_str() else {
                continue;
            };
            let in_pilot = facts
                .shares
                .get(text)
                .is_some_and(|name| cfg.native_pilot.allows_share(name, text));
            if in_pilot {
                plan.not_in_process_shared.insert(container.clone());
                plan.trees.push(reader::Tree {
                    kind: TreeKind::Shared,
                    name: text.to_string(),
                    budget: reader::tree_budget(facts.share_quota.get(text).copied()),
                });
            }
        }
    }
    if plan.trees.is_empty() {
        return plan;
    }
    let Some(native) = &env.native else {
        return plan;
    };
    match reader::start(native).await {
        Ok(access) => plan.reader = Some(access),
        Err(error) => {
            tracing::warn!(%error, "the backup reader is unavailable");
            for tree in std::mem::take(&mut plan.trees) {
                match tree.kind {
                    TreeKind::Home => plan.warnings.push(skipped_home(
                        &tree.name,
                        "its Linux account",
                        &format!("the backup reader is unavailable: {error}"),
                    )),
                    TreeKind::Shared => {
                        plan.not_in_process_shared
                            .remove(&OsString::from(&tree.name));
                        plan.warnings.push(format!(
                            "{}: read by the RAST service without owners and ACLs, the backup \
                             reader is unavailable: {error}",
                            tree.prefix()
                        ));
                    }
                }
            }
        }
    }
    plan
}

/// The files below `root` that the service reads itself, leaving out the
/// top-level folders in `skip`.
fn walk_in_process(
    root: &Path,
    prefix: &str,
    skip: &HashSet<OsString>,
    files: &mut Vec<(PathBuf, String)>,
    warnings: &mut Vec<String>,
    unlisted: &mut usize,
) {
    if !root.is_dir() {
        return;
    }
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !(entry.depth() == 1 && skip.contains(entry.file_name())));
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if warnings.len() < MAX_BACKUP_WARNINGS {
                    let path = error
                        .path()
                        .and_then(|path| path.strip_prefix(root).ok())
                        .map(|rel| format!("{prefix}/{}", rel.display()))
                        .unwrap_or_else(|| prefix.to_string());
                    warnings.push(format!("{path}: skipped, cannot read it: {error}"));
                } else {
                    *unlisted += 1;
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| entry.file_name().to_string_lossy().to_string());
        files.push((entry.path().to_path_buf(), format!("{prefix}/{rel}")));
    }
}

/// The files of the `users` scope that the service reads itself (legacy
/// HOMEs and shared folders), and what could not be read.
fn in_process_files(
    cfg: &Config,
    skip_homes: &HashSet<OsString>,
    skip_shared: &HashSet<OsString>,
) -> (Vec<(PathBuf, String)>, Vec<String>, usize) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut unlisted = 0usize;
    walk_in_process(
        &cfg.homes_dir(),
        "users/homes",
        skip_homes,
        &mut files,
        &mut warnings,
        &mut unlisted,
    );
    walk_in_process(
        &cfg.shared_dir(),
        "users/shared",
        skip_shared,
        &mut files,
        &mut warnings,
        &mut unlisted,
    );
    (files, warnings, unlisted)
}

/// At most [`MAX_BACKUP_WARNINGS`], then how many more there were.
fn cap_warnings(warnings: &mut Vec<String>, unlisted: usize) {
    let mut more = unlisted;
    if warnings.len() > MAX_BACKUP_WARNINGS {
        more += warnings.len() - MAX_BACKUP_WARNINGS;
        warnings.truncate(MAX_BACKUP_WARNINGS);
    }
    if more > 0 {
        warnings.push(format!(
            "and {more} more entries that could not be read or were left out"
        ));
    }
}

/// Default output directory for backups.
pub fn default_out_dir(cfg: &Config) -> PathBuf {
    cfg.backup_dir()
}

/// Validate and normalise the requested scopes.
pub fn normalize_scopes(scopes: &[String]) -> AppResult<Vec<String>> {
    let known: HashSet<&str> = SCOPES.iter().copied().collect();
    let mut out: Vec<String> = Vec::new();
    for scope in scopes {
        let scope = scope.trim().to_ascii_lowercase();
        if scope.is_empty() {
            continue;
        }
        if !known.contains(scope.as_str()) {
            return Err(AppError::bad_request(format!(
                "unknown backup scope {scope:?}; valid scopes: db, config, users, apps"
            )));
        }
        if !out.contains(&scope) {
            out.push(scope);
        }
    }
    if out.is_empty() {
        out.push("db".to_string());
        out.push("config".to_string());
    }
    Ok(out)
}

/// Create a backup archive. This touches the filesystem and (for large user
/// trees) can take a while — callers should run it as a background job.
pub async fn create(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    scopes: &[String],
    note: &str,
    out_dir: Option<&Path>,
    progress: Option<ProgressFn>,
) -> AppResult<BackupMeta> {
    create_with(
        cfg,
        pool,
        scopes,
        note,
        out_dir,
        progress,
        &BackupEnv::from_config(cfg),
    )
    .await
}

/// [`create`] with the owners and the helper of `env`.
pub(crate) async fn create_with(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    scopes: &[String],
    note: &str,
    out_dir: Option<&Path>,
    progress: Option<ProgressFn>,
    env: &BackupEnv,
) -> AppResult<BackupMeta> {
    let scopes = normalize_scopes(scopes)?;
    let out_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_out_dir(cfg));
    std::fs::create_dir_all(&out_dir)?;

    let now = db::now_ms();
    let stamp = format_stamp(now);
    let suffix = scopes.join("-");
    let file_name = format!("rast-{stamp}-{suffix}.{BACKUP_EXTENSION}");
    let final_path = out_dir.join(&file_name);

    let mut guard = BackupLock::acquire(cfg)?;
    let result = create_inner(cfg, pool, &scopes, note, &final_path, now, &progress, env).await;
    guard.release();
    let meta = result?;

    if let Some(pool) = pool {
        sqlx::query(
            "INSERT INTO backups (id, file_name, path, size_bytes, sha256, scope, status, note, created_at, finished_at)
             VALUES (?, ?, ?, ?, ?, ?, 'done', ?, ?, ?)",
        )
        .bind(&meta.id)
        .bind(&meta.file_name)
        .bind(&meta.path)
        .bind(meta.size_bytes as i64)
        .bind(&meta.sha256)
        .bind(serde_json::to_string(&meta.scopes).unwrap_or_else(|_| "[]".into()))
        .bind(note)
        .bind(meta.created_at)
        .bind(db::now_ms())
        .execute(pool)
        .await?;
    }
    Ok(meta)
}

#[allow(clippy::too_many_arguments)]
async fn create_inner(
    cfg: &Config,
    pool: Option<&SqlitePool>,
    scopes: &[String],
    note: &str,
    final_path: &Path,
    created_at: i64,
    progress: &Option<ProgressFn>,
    env: &BackupEnv,
) -> AppResult<BackupMeta> {
    let tmp_root = cfg.data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_root)?;
    let id = uuid::Uuid::new_v4().to_string();
    let work_dir = tmp_root.join(format!("backup-{id}"));
    std::fs::create_dir_all(&work_dir)?;
    // Ensure the work dir is removed on every exit path.
    let _guard = TempDirGuard(work_dir.clone());

    emit(progress, 0, 100, "preparing".to_string());

    // ---- consistent database snapshot -------------------------------------
    let db_snapshot = work_dir.join("rast.db");
    if scopes.iter().any(|s| s == "db") {
        let owned_pool;
        let pool_ref = match pool {
            Some(p) => p,
            None => {
                owned_pool = db::connect(&cfg.db_path(), 2).await?;
                &owned_pool
            }
        };
        // VACUUM INTO creates a consistent snapshot of the live database.
        let target = db_snapshot.to_string_lossy().to_string();
        sqlx::query("VACUUM INTO ?")
            .bind(&target)
            .execute(pool_ref)
            .await
            .map_err(|e| AppError::internal(format!("database snapshot failed: {e}")))?;
        if !db_snapshot.exists() {
            return Err(AppError::internal("database snapshot was not created"));
        }
        emit(progress, 14, 100, "database snapshot".to_string());
    }

    // ---- configuration -----------------------------------------------------
    let mut config_files: Vec<(String, PathBuf)> = Vec::new();
    if scopes.iter().any(|s| s == "config") {
        let candidates = [
            (cfg.loaded_from.clone(), "config/rast.toml"),
            (
                Some(PathBuf::from("/etc/rast/rast.toml")),
                "config/rast.toml",
            ),
            (Some(PathBuf::from("/etc/rast/rast.env")), "config/rast.env"),
            (Some(cfg.secret_path()), "config/secret.key"),
        ];
        let mut seen = HashSet::new();
        for (source, target) in candidates {
            if let Some(source) = source {
                if !source.is_file() || !seen.insert(target.to_string()) {
                    continue;
                }
                if seen.contains(target) && config_files.iter().any(|(t, _)| t == target) {
                    continue;
                }
                config_files.push((target.to_string(), source));
            }
        }
        // secret.key must always be present for restorable encrypted secrets.
        if !config_files.iter().any(|(t, _)| t == "config/secret.key") && cfg.secret_path().exists()
        {
            config_files.push(("config/secret.key".to_string(), cfg.secret_path()));
        }
        emit(progress, 20, 100, "configuration".to_string());
    }

    // ---- deployment metadata ----------------------------------------------
    let mut apps_json: Option<Value> = None;
    if scopes.iter().any(|s| s == "apps") {
        if let Some(pool) = pool {
            let rows = sqlx::query(
                "SELECT name, slug, compose, env_json, status, created_at, updated_at FROM apps ORDER BY slug",
            )
            .fetch_all(pool)
            .await?;
            use sqlx::Row;
            let items: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "name": row.get::<String, _>("name"),
                        "slug": row.get::<String, _>("slug"),
                        "compose": row.get::<String, _>("compose"),
                        "env": row.get::<String, _>("env_json"),
                        "status": row.get::<String, _>("status"),
                        "createdAt": row.get::<i64, _>("created_at"),
                        "updatedAt": row.get::<i64, _>("updated_at"),
                    })
                })
                .collect();
            apps_json = Some(json!({"format": 1, "apps": items}));
        }
    }

    // ---- user data ---------------------------------------------------------
    let users_scope = scopes.iter().any(|s| s == "users");
    let mut users = UsersPlan::default();
    if users_scope {
        // The work folder was made by this process: its owner is the
        // service.
        use std::os::unix::fs::MetadataExt;
        let service_uid = match env.service_uid {
            Some(uid) => uid,
            None => std::fs::metadata(&work_dir)?.uid(),
        };
        users = plan_users(cfg, pool, env, service_uid).await;
    }
    let mut warnings = std::mem::take(&mut users.warnings);
    let mut preserved_trees: Vec<String> = Vec::new();
    let mut space = SpaceGuard::new(work_dir.clone(), env.min_free_bytes);
    if users_scope {
        space
            .check()
            .map_err(|error| AppError::internal(error.to_string()))?;
    }

    // ---- build the archive -------------------------------------------------
    let total_units = 100u64;
    let tmp_archive = work_dir.join("archive.tar.gz");
    {
        let file = File::create(&tmp_archive)?;
        let writer = BufWriter::with_capacity(256 * 1024, file);
        let encoder = GzEncoder::new(writer, Compression::new(6));
        let mut builder = tar::Builder::new(encoder);
        let mut entries: Vec<ManifestEntry> = Vec::new();

        let add_bytes = |builder: &mut tar::Builder<_>,
                         entries: &mut Vec<ManifestEntry>,
                         path: &str,
                         data: &[u8]|
         -> AppResult<()> {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_mtime((created_at / 1000) as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, data)
                .map_err(|e| AppError::internal(format!("cannot write archive: {e}")))?;
            entries.push(ManifestEntry {
                path: path.to_string(),
                size: data.len() as u64,
                sha256: sha256_bytes(data),
                owners_preserved: false,
            });
            Ok(())
        };

        // Database
        if db_snapshot.exists() {
            let data = std::fs::read(&db_snapshot)?;
            add_bytes(&mut builder, &mut entries, "db/rast.db", &data)?;
        }
        emit(progress, 30, total_units, "database added".to_string());

        // Config
        for (target, source) in &config_files {
            let data = std::fs::read(source).map_err(|e| {
                AppError::internal(format!("cannot read {}: {e}", source.display()))
            })?;
            add_bytes(&mut builder, &mut entries, target, &data)?;
        }

        // Apps metadata
        if let Some(value) = &apps_json {
            let data = serde_json::to_vec_pretty(value).unwrap_or_default();
            add_bytes(&mut builder, &mut entries, "apps/apps.json", &data)?;
        }

        // HOMEs in one-HOME mode and migrated shared folders, through the
        // backup reader (streamed, bounded memory). A shared folder that the
        // reader refuses is read by the service below, as before.
        let tree_total = users.trees.len() as u64;
        for (index, tree) in std::mem::take(&mut users.trees).into_iter().enumerate() {
            let Some(access) = &users.reader else {
                break;
            };
            emit(
                progress,
                30 + (index as u64 * 25) / tree_total.max(1),
                total_units,
                format!("reading {} through the backup reader", tree.prefix()),
            );
            match reader::archive_tree(access, &tree, builder.get_mut(), &mut entries, &mut space)?
            {
                reader::Outcome::Written {
                    complete,
                    files,
                    bytes,
                    warnings: tree_warnings,
                } => {
                    tracing::info!(
                        tree = %tree.prefix(),
                        complete,
                        files,
                        bytes,
                        "backup read a tree through the backup reader"
                    );
                    preserved_trees.push(tree.prefix());
                    warnings.extend(tree_warnings);
                }
                reader::Outcome::Refused(reason) => {
                    tracing::warn!(tree = %tree.prefix(), %reason, "the backup reader did not read a tree");
                    match tree.kind {
                        TreeKind::Home => {
                            warnings.push(skipped_home(&tree.name, "its Linux account", &reason))
                        }
                        TreeKind::Shared => {
                            users
                                .not_in_process_shared
                                .remove(&OsString::from(&tree.name));
                            warnings.push(format!(
                                "{}: read by the RAST service without owners and ACLs, {reason}",
                                tree.prefix()
                            ));
                        }
                    }
                }
            }
        }

        // Legacy HOMEs and shared folders, read by the service (streamed one
        // by one, bounded memory).
        let mut unlisted = 0usize;
        let mut user_files: Vec<(PathBuf, String)> = Vec::new();
        if users_scope {
            let (files, unreadable, more) = in_process_files(
                cfg,
                &users.not_in_process_homes,
                &users.not_in_process_shared,
            );
            user_files = files;
            warnings.extend(unreadable);
            unlisted = more;
        }
        cap_warnings(&mut warnings, unlisted);
        let user_total = user_files.len() as u64;
        for (index, (source, target)) in user_files.iter().enumerate() {
            let mut file = match File::open(source) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %source.display(), error = %e, "skipping unreadable backup file");
                    continue;
                }
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o600);
            header.set_mtime((created_at / 1000) as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, target, &mut file)
                .map_err(|e| AppError::internal(format!("cannot archive user file: {e}")))?;
            entries.push(ManifestEntry {
                path: target.clone(),
                size,
                sha256: sha256_file(source).unwrap_or_default(),
                owners_preserved: false,
            });
            space
                .wrote(size)
                .map_err(|error| AppError::internal(error.to_string()))?;
            if index % 32 == 0 {
                let done = 55 + ((index as u64 * 30) / user_total.max(1));
                emit(
                    progress,
                    done,
                    total_units,
                    format!("archiving {index}/{user_total}"),
                );
            }
        }

        // Manifest (last, once the entry list is complete)
        let manifest = Manifest {
            format: BACKUP_FORMAT,
            app: crate::NAME.to_string(),
            app_version: crate::VERSION.to_string(),
            schema_version: db::embedded_schema_version(),
            created_at,
            hostname: hostname(),
            scopes: scopes.to_vec(),
            note: note.to_string(),
            entries: entries.clone(),
            warnings: warnings.clone(),
            preserved_trees: preserved_trees.clone(),
        };
        let manifest_data = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| AppError::internal(format!("cannot serialise manifest: {e}")))?;
        add_bytes(&mut builder, &mut entries, "manifest.json", &manifest_data)?;

        builder
            .finish()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        let encoder = builder
            .into_inner()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        let mut writer = encoder
            .finish()
            .map_err(|e| AppError::internal(format!("cannot finish archive: {e}")))?;
        writer.flush()?;
        writer.get_ref().sync_all().ok();
    }
    emit(progress, 90, total_units, "checksum".to_string());

    let (sha256, size_bytes) = sha256_and_size(&tmp_archive)?;
    // Move into place (same filesystem, atomic rename).
    std::fs::rename(&tmp_archive, final_path)?;
    emit(progress, 100, total_units, "done".to_string());

    Ok(BackupMeta {
        id: uuid::Uuid::new_v4().to_string(),
        file_name: final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: final_path.to_string_lossy().to_string(),
        size_bytes,
        sha256,
        scopes: scopes.to_vec(),
        created_at,
        duration_ms: db::now_ms() - created_at,
        warnings,
    })
}

/// Verify an archive: checksums of every entry, manifest sanity and (when
/// present) SQLite integrity of the embedded database.
pub async fn verify(path: &Path) -> AppResult<Value> {
    if !path.is_file() {
        return Err(AppError::NotFound("backup archive not found"));
    }
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<Manifest> = None;
    let mut db_bytes: Option<Vec<u8>> = None;

    for entry in archive
        .entries()
        .map_err(|e| AppError::internal(format!("cannot read archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| AppError::internal(format!("corrupt archive: {e}")))?;
        let entry_path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| AppError::internal(format!("cannot read archive entry: {e}")))?;
        if entry_path == "manifest.json" {
            manifest = serde_json::from_slice(&data).ok();
        } else if entry_path == "db/rast.db" {
            db_bytes = Some(data.clone());
        }
        tracing::debug!(entry = %entry_path, size = data.len(), "backup entry");
    }

    let mut checks =
        json!({"archive": true, "manifest": false, "checksums": false, "database": Value::Null});
    let Some(manifest) = manifest else {
        return Ok(json!({
            "ok": false,
            "checks": checks,
            "error": "manifest.json is missing or invalid",
        }));
    };
    checks["manifest"] = json!(true);

    // Re-read entries to verify checksums against the manifest.
    let mut mismatches = Vec::new();
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let expected: std::collections::HashMap<String, &ManifestEntry> = manifest
        .entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut seen = 0usize;
    if let Ok(entries) = archive.entries() {
        for mut entry in entries.flatten() {
            let Ok(entry_path) = entry.path().map(|p| p.to_string_lossy().to_string()) else {
                continue;
            };
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_err() {
                mismatches.push(entry_path);
                continue;
            }
            if let Some(expected_entry) = expected.get(&entry_path) {
                seen += 1;
                if expected_entry.size != data.len() as u64
                    || expected_entry.sha256 != sha256_bytes(&data)
                {
                    mismatches.push(entry_path);
                }
            }
        }
    }
    checks["checksums"] = json!(mismatches.is_empty() && seen == expected.len());

    // Database integrity check on the extracted snapshot.
    let mut db_ok = Value::Null;
    if let Some(bytes) = db_bytes {
        let tmp = tempfile_dir()?;
        let db_path = tmp.join("verify.db");
        std::fs::write(&db_path, &bytes)?;
        match check_sqlite_integrity(&db_path).await {
            Ok(ok) => db_ok = json!(ok),
            Err(e) => db_ok = json!({"ok": false, "error": e.to_string()}),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    checks["database"] = db_ok.clone();

    let all_ok = mismatches.is_empty()
        && checks["checksums"].as_bool().unwrap_or(false)
        && db_ok.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);

    Ok(json!({
        "ok": all_ok,
        "manifest": {
            "format": manifest.format,
            "appVersion": manifest.app_version,
            "schemaVersion": manifest.schema_version,
            "createdAt": manifest.created_at,
            "hostname": manifest.hostname,
            "scopes": manifest.scopes,
            "note": manifest.note,
            "entries": manifest.entries.len(),
            "ownersPreserved": manifest.entries.iter().filter(|entry| entry.owners_preserved).count(),
            "preservedTrees": manifest.preserved_trees,
        },
        "checks": checks,
        "mismatches": mismatches,
        "sizeBytes": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        "path": path.display().to_string(),
    }))
}

/// Restore the database from an archive. The caller must stop the service
/// first; the previous database is preserved as a pre-restore copy.
pub async fn restore_database(cfg: &Config, archive: &Path) -> AppResult<Value> {
    // A running server holds `<data_dir>/rast.lock`; hold it ourselves for the
    // whole replacement so no server can start mid-restore either.
    let _instance = db::InstanceLock::try_acquire(cfg)
        .map_err(|e| AppError::internal(format!("cannot open the instance lock: {e}")))?
        .ok_or_else(|| {
            AppError::conflict(
                "a RAST server is running on this data directory; stop it (sudo systemctl stop rast.service) before restoring the database",
            )
        })?;
    let tmp_dir = cfg
        .data_dir
        .join("tmp")
        .join(format!("restore-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir)?;
    let _guard = TempDirGuard(tmp_dir.clone());

    let file = File::open(archive)?;
    let reader = BufReader::with_capacity(256 * 1024, file);
    let mut tar = tar::Archive::new(GzDecoder::new(reader));
    let mut found = false;
    let extracted = tmp_dir.join("rast.db");
    for entry in tar
        .entries()
        .map_err(|e| AppError::internal(format!("cannot read archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| AppError::internal(format!("corrupt archive: {e}")))?;
        let path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if path == "db/rast.db" {
            let mut out = File::create(&extracted)?;
            std::io::copy(&mut entry, &mut out)?;
            out.sync_all().ok();
            found = true;
            break;
        }
    }
    if !found {
        return Err(AppError::bad_request(
            "archive does not contain db/rast.db (was it created with the db scope?)",
        ));
    }
    let integrity = check_sqlite_integrity(&extracted).await?;
    if !integrity
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(AppError::bad_request(format!(
            "restored database failed integrity check: {integrity}"
        )));
    }

    let stamp = format_stamp(db::now_ms());
    let pre_restore_dir = cfg.backup_dir().join(format!("pre-restore-{stamp}"));
    std::fs::create_dir_all(&pre_restore_dir)?;
    let live = cfg.db_path();
    if live.exists() {
        std::fs::copy(&live, pre_restore_dir.join("rast.db"))?;
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", live.display()));
        if sidecar.exists() {
            std::fs::copy(
                &sidecar,
                pre_restore_dir.join(sidecar.file_name().unwrap_or_default()),
            )?;
            std::fs::remove_file(&sidecar)?;
        }
    }
    // Atomic replacement: same directory, rename over the old file (in the
    // private folder, where nobody else can name the copy either).
    let staging = live.with_file_name(format!(".rast.db.restore-{stamp}"));
    std::fs::copy(&extracted, &staging)?;
    std::fs::rename(&staging, &live)?;
    if let Ok(meta) = std::fs::metadata(&live) {
        let _ = meta.permissions();
    }
    Ok(json!({
        "restored": true,
        "database": live.display().to_string(),
        "preRestoreBackup": pre_restore_dir.display().to_string(),
        "integrity": integrity,
        "message": "database restored; start rast.service and verify sign-in",
    }))
}

/// Delete backups beyond `keep` newest archives (rows and files).
pub async fn prune_retention(cfg: &Config, pool: &SqlitePool, keep: i64) -> AppResult<Vec<String>> {
    let keep = keep.max(1);
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, path FROM backups WHERE status = 'done' ORDER BY created_at DESC LIMIT -1 OFFSET ?",
    )
    .bind(keep)
    .fetch_all(pool)
    .await?;
    let mut removed = Vec::new();
    for row in rows {
        let id: String = row.get("id");
        let path: String = row.get("path");
        let path_ref = PathBuf::from(&path);
        if path_ref.exists() && path_ref.starts_with(cfg.backup_dir()) {
            let _ = std::fs::remove_file(&path_ref);
        }
        sqlx::query("DELETE FROM backups WHERE id = ?")
            .bind(&id)
            .execute(pool)
            .await?;
        removed.push(path);
    }
    Ok(removed)
}

/// Verify a SQLite database file by running `PRAGMA integrity_check`.
pub async fn check_sqlite_integrity(path: &Path) -> AppResult<Value> {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(|e| AppError::internal(format!("cannot open database: {e}")))?;
    let result: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await
        .unwrap_or_else(|_| vec!["error".to_string()]);
    let schema = db::schema_version(&pool).await;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    pool.close().await;
    let ok = result.len() == 1 && result.first().map(|s| s == "ok").unwrap_or(false);
    Ok(json!({"ok": ok, "result": result, "schemaVersion": schema, "tables": tables}))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn emit(progress: &Option<ProgressFn>, done: u64, total: u64, message: String) {
    if let Some(cb) = progress {
        cb(done, total, message);
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_and_size(path: &Path) -> AppResult<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn format_stamp(ts: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ts) {
        Some(dt) => dt.format("%Y%m%d-%H%M%S").to_string(),
        None => ts.to_string(),
    }
}

/// Temporary directory for verification work (uses the system temp dir so
/// verification works for any caller, including the CLI and tests).
fn tempfile_dir() -> AppResult<PathBuf> {
    let dir = std::env::temp_dir().join(format!("rast-verify-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Prevents two concurrent backups from writing overlapping archives.
struct BackupLock {
    path: PathBuf,
}

impl BackupLock {
    fn acquire(cfg: &Config) -> AppResult<Self> {
        let dir = cfg.data_dir.join("tmp");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("backup.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                AppError::conflict(format!(
                    "another backup is already running ({e}); wait for it to finish"
                ))
            })?;
        let _ = file.write_all(db::now_ms().to_string().as_bytes());
        Ok(Self { path })
    }

    fn release(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for BackupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Public helper used by the UI to describe what a backup contains.
pub fn scope_description(scope: &str) -> &'static str {
    match scope {
        "db" => {
            "Accounts, sessions, settings, audit logs, job history (consistent SQLite snapshot)"
        }
        "config" => "Configuration file, environment file and server secret key",
        "users" => "User home directories and shared folders (file data)",
        "apps" => "Managed application definitions and encrypted environment variables",
        _ => "Unknown scope",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};

    fn test_cfg(dir: &Path) -> Config {
        Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                // Never the host's helper, even where Xpra is installed.
                native_enabled: Some(false),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap()
    }

    /// What the service reads itself, and the warnings, for a service uid.
    async fn user_files(cfg: &Config, service_uid: u32) -> (Vec<(PathBuf, String)>, Vec<String>) {
        let env = BackupEnv {
            service_uid: Some(service_uid),
            container_owner_uid: 0,
            native: None,
            min_free_bytes: 0,
        };
        let plan = plan_users(cfg, None, &env, service_uid).await;
        assert!(plan.trees.is_empty());
        let (files, unreadable, _) =
            in_process_files(cfg, &plan.not_in_process_homes, &plan.not_in_process_shared);
        let mut warnings = plan.warnings;
        warnings.extend(unreadable);
        (files, warnings)
    }

    #[tokio::test]
    async fn backup_verify_and_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let pool = db::init(&cfg).await.unwrap();
        // Seed data
        sqlx::query("INSERT INTO users (username, password_hash, role, created_at, updated_at) VALUES ('alice','x','admin',1,1)")
            .execute(&pool)
            .await
            .unwrap();
        let user_home = cfg.home_for("alice");
        std::fs::create_dir_all(&user_home).unwrap();
        std::fs::write(user_home.join("hello.txt"), b"hello rast").unwrap();

        let meta = create(
            &cfg,
            Some(&pool),
            &["db".into(), "config".into(), "users".into(), "apps".into()],
            "test backup",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(meta.size_bytes > 0);
        assert_eq!(meta.sha256.len(), 64);
        let archive = PathBuf::from(&meta.path);
        assert!(archive.exists());

        let verification = verify(&archive).await.unwrap();
        assert_eq!(verification["ok"], true, "verify failed: {verification}");
        assert_eq!(verification["checks"]["checksums"], true);
        assert_eq!(verification["checks"]["database"]["ok"], true);

        // Row recorded in the database.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // The DB snapshot contains the seeded user.
        let file = File::open(&archive).unwrap();
        let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
        let mut restored_db = None;
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "db/rast.db" {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                restored_db = Some(data);
            }
        }
        let restored_db = restored_db.expect("db entry present");
        let rc = check_sqlite_integrity_bytes(&restored_db).await;
        assert!(rc);
    }

    async fn check_sqlite_integrity_bytes(bytes: &[u8]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::fs::write(&path, bytes).unwrap();
        check_sqlite_integrity(&path)
            .await
            .map(|v| v["ok"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn homes_in_one_home_mode_are_skipped_with_a_warning() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        for name in ["alice", "t-bob"] {
            std::fs::create_dir_all(cfg.home_for(name).join("docs")).unwrap();
            std::fs::write(cfg.home_for(name).join("docs/a.txt"), b"a").unwrap();
        }
        std::fs::create_dir_all(cfg.shared_dir().join("team")).unwrap();
        std::fs::write(cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
        let me = std::fs::metadata(dir.path()).unwrap().uid();

        // Every HOME is the service's: everything is in.
        let (files, warnings) = user_files(&cfg, me).await;
        let mut targets: Vec<&str> = files.iter().map(|(_, target)| target.as_str()).collect();
        targets.sort();
        assert_eq!(
            targets,
            [
                "users/homes/alice/docs/a.txt",
                "users/homes/t-bob/docs/a.txt",
                "users/shared/team/plan.txt"
            ]
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        // HOMEs of another uid (one-HOME mode) are left out, by name, and
        // shared folders are still in.
        let (files, warnings) = user_files(&cfg, me.wrapping_add(1)).await;
        let targets: Vec<&str> = files.iter().map(|(_, target)| target.as_str()).collect();
        assert_eq!(targets, ["users/shared/team/plan.txt"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings
            .iter()
            .any(|w| w.starts_with("users/homes/t-bob: skipped") && w.contains("one-HOME mode")));
    }

    #[tokio::test]
    async fn an_unreadable_folder_is_reported_instead_of_dropped_silently() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let locked = cfg.home_for("alice").join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("x.txt"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: permissions do not apply.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let meta = create(&cfg, None, &["users".into()], "", None, None).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
        assert!(meta.warnings[0].starts_with("users/homes/alice/locked: skipped, cannot read it"));
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["warnings"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn verify_rejects_missing_archive() {
        let err = verify(Path::new("/nonexistent/file.tar.gz")).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn normalize_scopes_validates() {
        assert_eq!(
            normalize_scopes(&["db".into(), "DB".into(), " config ".into()]).unwrap(),
            vec!["db".to_string(), "config".to_string()]
        );
        assert!(normalize_scopes(&["nope".into()]).is_err());
        assert_eq!(
            normalize_scopes(&[]).unwrap(),
            vec!["db".to_string(), "config".to_string()]
        );
    }

    #[tokio::test]
    async fn concurrent_backups_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let pool = db::init(&cfg).await.unwrap();
        let _lock = BackupLock::acquire(&cfg).unwrap();
        let err = create(&cfg, Some(&pool), &["db".into()], "", None, None).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("another backup"));
    }

    #[tokio::test]
    async fn restore_rejects_corrupt_archives() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_cfg(dir.path());
        cfg.ensure_dirs().unwrap();
        let fake = dir.path().join("fake.tar.gz");
        std::fs::write(&fake, b"not a gzip archive").unwrap();
        let result = restore_database(&cfg, &fake).await;
        assert!(result.is_err());
    }

    // `restore_refuses_while_a_server_holds_the_instance_lock` lives in
    // `tests/instance_lock.rs`: it needs its own process (see the note there).
}
