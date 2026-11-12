//! Package Center orchestration: catalog checks, views, install/update jobs,
//! default selection, removal, consumers and GitHub imports.
//!
//! Every mutation is serialised per package (one operation at a time), runs
//! file work on blocking threads, reports progress through the job system,
//! publishes `package.changed` / `package.catalog` events and writes an audit
//! entry with the actor, package, version and outcome.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use serde::Serialize;
use serde_json::json;
use sqlx::SqlitePool;

use super::{
    catalog::{self, Catalog, Detected, TrustedKey, Verified},
    github::{self, Check, GithubRef},
    host::{self, HostInfo},
    install::{self, Layout, Reporter},
    manifest::{self, Artifact, Manifest, Provenance, Requirements},
    sandbox, shell,
    store::{self, ConsumerRow, JobHistory, PackageRow, VersionRow},
    system::{self, SystemInstall},
    version,
};
use crate::{
    audit::{self, AuditLog},
    auth::AuthUser,
    config::Config,
    db,
    error::{AppError, AppResult},
    events::EventBus,
    jobs::{JobContext, JobManager, JobSpec},
};

const INSPECTION_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub url: String,
    pub official: bool,
    pub name: Option<String>,
    pub key_id: Option<String>,
    pub official_key: bool,
    pub sequence: Option<u64>,
    pub generated_at: Option<String>,
    pub expires_at: Option<String>,
    pub checked_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub interval_minutes: u64,
    pub next_check_at: Option<i64>,
    pub stale: bool,
    pub expired: bool,
    pub checking: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubUpdate {
    pub tag: String,
    pub version: String,
    pub url: String,
    pub checked_at: i64,
}

/// Releases the per-package lock when an operation ends.
struct BusyGuard {
    busy: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.busy.lock() {
            set.remove(&self.id);
        }
    }
}

pub struct PackageManager {
    config: Arc<Config>,
    db: SqlitePool,
    events: EventBus,
    jobs: Arc<JobManager>,
    pub layout: Layout,
    pub host: HostInfo,
    keys: Vec<TrustedKey>,
    catalog: RwLock<Option<Verified>>,
    status: RwLock<CatalogStatus>,
    github_updates: RwLock<HashMap<String, GithubUpdate>>,
    /// Runtimes the operating system provides, by package id.
    system: RwLock<HashMap<String, SystemInstall>>,
    /// Set by tests that provide their own system runtimes.
    system_fixed: std::sync::atomic::AtomicBool,
    /// The login-shell profile script (see [`shell`]).
    profile: RwLock<std::path::PathBuf>,
    busy: Arc<Mutex<HashSet<String>>>,
    check_lock: tokio::sync::Mutex<()>,
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutablePath {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactView {
    pub target: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub requires: Requirements,
    pub provenance: Provenance,
}

impl From<&Artifact> for ArtifactView {
    fn from(a: &Artifact) -> Self {
        Self {
            target: a.target.clone(),
            url: a.url.clone(),
            size: a.size,
            sha256: a.sha256.clone(),
            requires: a.requires.clone(),
            provenance: a.provenance.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledView {
    pub version: String,
    pub revision: u32,
    pub channel: String,
    pub target: String,
    pub path: String,
    pub executables: Vec<ExecutablePath>,
    pub is_default: bool,
    /// `ok` or `missing` (files removed outside RAST).
    pub status: String,
    pub installed_at: i64,
    pub installed_by: String,
    pub size_bytes: i64,
    pub check_output: String,
    pub release_tag: Option<String>,
    pub artifact: ArtifactView,
    pub bundled: Vec<manifest::Bundled>,
    pub pinned_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionChoice {
    pub version: String,
    pub revision: u32,
    pub channel: String,
    pub channel_label: Option<String>,
    pub channel_status: Option<String>,
    pub prerelease: bool,
    pub release_date: Option<String>,
    pub compatible: bool,
    pub reason: Option<String>,
    /// Installed revision of this version, if any.
    pub installed_revision: Option<u32>,
    pub targets: Vec<String>,
    pub artifact: Option<ArtifactView>,
    pub bundled: Vec<manifest::Bundled>,
    pub extensions: Vec<String>,
    pub executables: Vec<ExecutablePath>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateView {
    pub channel: String,
    pub from: String,
    pub to: String,
    pub revision: u32,
    /// The installed version this replaces is the RAST default.
    pub replaces_default: bool,
    /// `from` is the system's own runtime (apt or manual install); the update
    /// is installed next to it.
    pub from_system: bool,
}

/// What a terminal runs for the package's main command.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalView {
    pub command: String,
    /// `package-center` (the default's shim, first on PATH) or `system`.
    pub source: String,
    pub version: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedView {
    pub version: String,
    pub channel: String,
    pub state: String,
    pub detail: String,
    pub detected_at: String,
    /// `catalog` (official workflow) or `github` (new release of an import).
    pub source: String,
    pub release_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    /// `signed-catalog` or `github-release`.
    pub kind: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerView {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub package_id: String,
    pub version: Option<String>,
    pub follows_default: bool,
    pub effective_version: Option<String>,
    pub executables: Vec<ExecutablePath>,
    pub created_by: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageView {
    pub id: String,
    pub package_type: String,
    pub name: String,
    pub description: String,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub publisher: manifest::Publisher,
    pub source: String,
    pub source_repo: Option<String>,
    pub official: bool,
    pub verification: Verification,
    pub in_catalog: bool,
    pub installed: Vec<InstalledView>,
    pub default_version: Option<String>,
    /// Stable path of the default executables (`runtimes/<id>/current/...`).
    pub default_executables: Vec<ExecutablePath>,
    pub latest: Option<VersionChoice>,
    pub versions: Vec<VersionChoice>,
    pub unsupported: Vec<catalog::Unsupported>,
    pub updates: Vec<UpdateView>,
    pub detected: Vec<DetectedView>,
    /// `ready`, `detected` or `none`.
    pub update_state: String,
    pub active_job: Option<JobHistory>,
    pub consumers: Vec<ConsumerView>,
    pub unavailable_reason: Option<String>,
    /// The runtime the operating system already has, if any.
    pub system: Option<SystemInstall>,
    /// What a login shell runs for the main command.
    pub terminal: Option<TerminalView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub host: HostInfo,
    pub catalog: CatalogStatus,
    pub shell: shell::ShellPath,
    pub packages: Vec<PackageView>,
    pub counts: BTreeMap<String, usize>,
    pub can_manage: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub inspection_id: String,
    pub repo: String,
    pub repo_url: String,
    pub release: github::Release,
    pub manifest: Manifest,
    pub manifest_sha256: String,
    pub checks: Vec<Check>,
    pub official: bool,
    pub source_approved: bool,
    pub source_approved_by: Option<String>,
    pub host_target: String,
    pub compatible: bool,
    pub reason: Option<String>,
    pub artifact: Option<ArtifactView>,
    pub already_installed: bool,
    pub expires_at: i64,
}

fn exe_paths(base: &str, m: &Manifest) -> Vec<ExecutablePath> {
    m.runtime
        .executables
        .iter()
        .map(|e| ExecutablePath {
            name: e.name.clone(),
            path: format!("{base}/{}", e.path),
        })
        .collect()
}

impl PackageManager {
    pub fn new(
        config: Arc<Config>,
        db: SqlitePool,
        events: EventBus,
        jobs: Arc<JobManager>,
    ) -> Arc<Self> {
        let layout = Layout::new(config.packages_dir());
        if let Err(e) = layout.ensure() {
            tracing::warn!(error = %e, "cannot create the package directory");
        }
        layout.clean_temporary();
        if let Err(e) = shell::share(&config.data_dir, &layout, config.packages_shell_path) {
            tracing::warn!(error = %e, "cannot set the permissions of the package directory");
        }
        let keys = catalog::trusted_keys(&config.packages_catalog_keys);
        let cached = catalog::load_cache(&layout.catalog_dir(), &keys);
        let status = CatalogStatus {
            url: config.packages_catalog_url.clone(),
            official: config.packages_catalog_url == catalog::OFFICIAL_CATALOG_URL,
            interval_minutes: config.packages_check_interval_minutes,
            ..CatalogStatus::default()
        };
        Arc::new(Self {
            layout,
            host: HostInfo::detect(),
            keys,
            catalog: RwLock::new(cached),
            status: RwLock::new(status),
            github_updates: RwLock::new(HashMap::new()),
            system: RwLock::new(HashMap::new()),
            system_fixed: std::sync::atomic::AtomicBool::new(false),
            profile: RwLock::new(std::path::PathBuf::from(shell::PROFILE_SCRIPT)),
            busy: Arc::new(Mutex::new(HashSet::new())),
            check_lock: tokio::sync::Mutex::new(()),
            config,
            db,
            events,
            jobs,
        })
    }

    /// Restore the persisted check state and start the periodic checker.
    pub async fn start(self: &Arc<Self>) {
        self.recover_displaced().await;
        let me = self.clone();
        tokio::spawn(async move { me.refresh_system().await });
        // The cache on disk must not be older than the accepted catalog.
        let floor: Option<u64> = db::get_setting(&self.db, "packages.catalogSequence")
            .await
            .and_then(|v| v.parse().ok());
        let cached = self
            .catalog
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|v| v.catalog.sequence));
        if let (Some(floor), Some(seq)) = (floor, cached) {
            if seq < floor {
                tracing::warn!(
                    cached = seq,
                    accepted = floor,
                    "ignoring the cached package catalog: older than the accepted sequence"
                );
                if let Ok(mut g) = self.catalog.write() {
                    *g = None;
                }
            }
        }
        let checked = db::get_setting(&self.db, "packages.catalogCheckedAt")
            .await
            .and_then(|v| v.parse().ok());
        let attempt = db::get_setting(&self.db, "packages.catalogLastAttemptAt")
            .await
            .and_then(|v| v.parse().ok());
        let error = db::get_setting(&self.db, "packages.catalogLastError")
            .await
            .filter(|v| !v.is_empty());
        if let Ok(mut status) = self.status.write() {
            status.checked_at = checked;
            status.last_attempt_at = attempt;
            status.last_error = error;
        }
        let interval = self.config.packages_check_interval_minutes;
        if interval == 0 {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let mut shutdown = me.events.shutdown_receiver();
            // First check shortly after start, then every interval. After a
            // failed check, retry sooner with backoff instead of staying
            // stale for a whole interval.
            let full = Duration::from_secs((interval * 60).clamp(60, 7 * 24 * 3600));
            let mut failures: u32 = 0;
            let mut wait = Duration::from_secs(20);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = shutdown.changed() => return,
                }
                let due = failures > 0
                    || me
                        .status_snapshot()
                        .checked_at
                        .map(|t| db::now_ms() - t >= (interval as i64) * 60_000 - 5_000)
                        .unwrap_or(true);
                if due {
                    let error = match me.check_now(None).await {
                        Ok(status) => status.last_error,
                        Err(e) => Some(e.to_string()),
                    };
                    match error {
                        None => failures = 0,
                        Some(error) => {
                            failures = failures.saturating_add(1);
                            let retry = retry_delay(failures).min(full);
                            tracing::warn!(
                                %error,
                                failures,
                                retry_in_minutes = retry.as_secs() / 60,
                                "package catalog check failed"
                            );
                            wait = retry;
                            continue;
                        }
                    }
                }
                wait = full;
                if !due {
                    // Align with the last successful check.
                    let since = me
                        .status_snapshot()
                        .checked_at
                        .map(|t| db::now_ms() - t)
                        .unwrap_or(0);
                    let remaining = (interval as i64) * 60_000 - since;
                    wait = Duration::from_millis(
                        remaining.clamp(60_000, (interval as i64) * 60_000) as u64,
                    );
                }
            }
        });
    }

    /// Settle replacements interrupted by a stop: keep whichever tree the
    /// database describes (by artifact SHA-256), so files and records agree.
    async fn recover_displaced(&self) {
        let layout = self.layout.clone();
        let displaced = tokio::task::spawn_blocking(move || layout.displaced())
            .await
            .unwrap_or_default();
        for d in displaced {
            let recorded = store::versions(&self.db, Some(&d.marker.id))
                .await
                .ok()
                .and_then(|vs| vs.into_iter().find(|v| v.version == d.marker.version))
                .map(|v| v.artifact_sha256);
            let target = self.layout.version_dir(&d.marker.id, &d.marker.version);
            let current = install::read_marker(&target);
            let target_recorded = recorded
                .as_ref()
                .is_some_and(|sha| current.as_ref().is_some_and(|c| &c.sha256 == sha));
            let aside_recorded = recorded.as_ref().is_some_and(|sha| &d.marker.sha256 == sha);
            let keep_new = target_recorded || (!aside_recorded && target.exists());
            let aside = d.path.clone();
            let settled = tokio::task::spawn_blocking(move || {
                install::settle_replacement(&target, &aside, keep_new)
            })
            .await;
            match settled {
                Ok(Ok(())) => tracing::warn!(
                    package = %d.marker.id,
                    version = %d.marker.version,
                    restored = !keep_new,
                    "settled a package replacement interrupted by a restart"
                ),
                Ok(Err(e)) => {
                    tracing::warn!(package = %d.marker.id, error = %e, "cannot settle an interrupted package replacement")
                }
                Err(e) => tracing::warn!(error = %e, "package recovery task failed"),
            }
        }
    }

    fn status_snapshot(&self) -> CatalogStatus {
        let mut status = self.status.read().map(|s| s.clone()).unwrap_or_default();
        if let Ok(guard) = self.catalog.read() {
            if let Some(v) = guard.as_ref() {
                status.name = Some(v.catalog.name.clone());
                status.key_id = Some(v.key_id.clone());
                status.official_key = v.official_key;
                status.sequence = Some(v.catalog.sequence);
                status.generated_at = Some(v.catalog.generated_at.clone());
                status.expires_at = v.catalog.expires_at.clone();
                status.expired = v.catalog.expires_at_ms().is_some_and(|t| db::now_ms() > t);
            }
        }
        let interval_ms = (status.interval_minutes.max(1) as i64) * 60_000;
        let stale_after = (interval_ms * 2).max(60 * 60_000);
        status.next_check_at = match (status.interval_minutes, status.checked_at) {
            (0, _) => None,
            (_, Some(t)) => Some(t + interval_ms),
            (_, None) => None,
        };
        status.stale = status.expired
            || status.sequence.is_none()
            || status.last_error.is_some()
            || status
                .checked_at
                .is_none_or(|t| db::now_ms() - t > stale_after);
        status
    }

    fn current_catalog(&self) -> Option<Catalog> {
        self.catalog
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|v| v.catalog.clone()))
    }

    /// Look again for the runtimes the operating system provides (at start
    /// and with every catalog check). Publishes `package.changed` when the
    /// result differs.
    pub async fn refresh_system(&self) {
        let root = self.layout.root.clone();
        let scratch = self.layout.staging();
        let found = tokio::task::spawn_blocking(move || system::detect_all(&root, &scratch))
            .await
            .unwrap_or_default();
        let key = |m: &HashMap<String, SystemInstall>| {
            let mut v: Vec<(String, String, Option<String>)> = m
                .iter()
                .map(|(id, s)| (id.clone(), s.real_path.clone(), s.version.clone()))
                .collect();
            v.sort();
            v
        };
        let changed = match self.system.write() {
            Ok(_) if self.system_fixed.load(std::sync::atomic::Ordering::SeqCst) => false,
            Ok(mut g) => {
                let changed = key(&g) != key(&found);
                *g = found;
                changed
            }
            Err(_) => false,
        };
        if changed {
            self.events
                .publish("package.changed", json!({"reason": "system"}));
        }
    }

    /// Tests: use these system runtimes instead of detecting the host's,
    /// and this profile script instead of `/etc/profile.d`.
    #[doc(hidden)]
    pub fn set_system_for_tests(
        &self,
        found: HashMap<String, SystemInstall>,
        profile: std::path::PathBuf,
    ) {
        self.system_fixed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut g) = self.system.write() {
            *g = found;
        }
        if let Ok(mut p) = self.profile.write() {
            *p = profile;
        }
    }

    fn system_install(&self, id: &str) -> Option<SystemInstall> {
        self.system.read().ok().and_then(|g| g.get(id).cloned())
    }

    fn shell_status(&self) -> shell::ShellPath {
        let profile = self
            .profile
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| shell::PROFILE_SCRIPT.into());
        shell::status(
            self.config.packages_shell_path,
            &self.layout.bin_dir(),
            &profile,
        )
    }

    /// Fetch and verify the catalog now; also look for new releases of
    /// imported packages. Returns the resulting status.
    pub async fn check_now(self: &Arc<Self>, actor: Option<&AuthUser>) -> AppResult<CatalogStatus> {
        let _guard = self.check_lock.lock().await;
        if let Ok(mut s) = self.status.write() {
            s.checking = true;
        }
        self.events
            .publish("package.catalog", json!({"reason": "checking"}));
        let url = self.config.packages_catalog_url.clone();
        let keys = self.keys.clone();
        let fetched = tokio::task::spawn_blocking(move || catalog::fetch(&url, &keys))
            .await
            .map_err(|e| AppError::internal(format!("catalog check task failed: {e}")))?;
        let now = db::now_ms();
        let previous_seq = self
            .catalog
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|v| (v.catalog.sequence, v.bytes.clone())));
        let accepted_seq: Option<u64> = db::get_setting(&self.db, "packages.catalogSequence")
            .await
            .and_then(|v| v.parse().ok());
        let outcome: Result<bool, String> = match fetched {
            Ok(verified) => {
                let floor = accepted_seq.or(previous_seq.as_ref().map(|p| p.0));
                if verified.catalog.expires_at_ms().is_some_and(|t| now > t) {
                    Err(format!(
                        "refused catalog sequence {}: it expired at {} (the source may be serving a stale copy)",
                        verified.catalog.sequence,
                        verified.catalog.expires_at.as_deref().unwrap_or_default()
                    ))
                } else if floor.is_some_and(|f| verified.catalog.sequence < f) {
                    Err(format!(
                        "refused catalog sequence {} (older than the accepted {}): possible rollback",
                        verified.catalog.sequence,
                        floor.unwrap_or_default()
                    ))
                } else if previous_seq.as_ref().is_some_and(|(seq, bytes)| {
                    *seq == verified.catalog.sequence && *bytes != verified.bytes
                }) {
                    Err("refused catalog: same sequence with different contents".into())
                } else {
                    let changed =
                        previous_seq.as_ref().map(|p| p.0) != Some(verified.catalog.sequence);
                    if let Err(e) = catalog::store_cache(&self.layout.catalog_dir(), &verified) {
                        tracing::warn!(error = %e, "cannot cache the package catalog");
                    }
                    db::set_setting(
                        &self.db,
                        "packages.catalogSequence",
                        &verified.catalog.sequence.to_string(),
                    )
                    .await?;
                    if let Ok(mut g) = self.catalog.write() {
                        *g = Some(verified);
                    }
                    Ok(changed)
                }
            }
            Err(e) => Err(e),
        };
        db::set_setting(&self.db, "packages.catalogLastAttemptAt", &now.to_string()).await?;
        let (changed, error) = match &outcome {
            Ok(changed) => {
                db::set_setting(&self.db, "packages.catalogCheckedAt", &now.to_string()).await?;
                db::set_setting(&self.db, "packages.catalogLastError", "").await?;
                (*changed, None)
            }
            Err(e) => {
                db::set_setting(&self.db, "packages.catalogLastError", e).await?;
                (false, Some(e.clone()))
            }
        };
        if let Ok(mut s) = self.status.write() {
            s.last_attempt_at = Some(now);
            if error.is_none() {
                s.checked_at = Some(now);
            }
            s.last_error = error.clone();
        }
        let github_changed = self.check_github_sources().await;
        self.refresh_system().await;
        if let Ok(mut s) = self.status.write() {
            s.checking = false;
        }
        if let Some(user) = actor {
            let mut log = AuditLog::new("package.catalog_check")
                .actor(user)
                .object("catalog", self.config.packages_catalog_url.clone());
            log = match &error {
                None => log.success().detail(format!(
                    "sequence={}",
                    self.status_snapshot().sequence.unwrap_or_default()
                )),
                Some(e) => log.failure().detail(e.clone()),
            };
            audit::record_with_events(&self.db, &self.events, log).await;
        }
        let status = self.status_snapshot();
        self.events.publish(
            "package.catalog",
            json!({
                "reason": "checked",
                "changed": changed || github_changed,
                "sequence": status.sequence,
                "error": error,
            }),
        );
        Ok(status)
    }

    async fn check_github_sources(&self) -> bool {
        let Ok(rows) = store::packages(&self.db).await else {
            return false;
        };
        let token = self.config.packages_github_token.clone();
        let mut changed = false;
        for row in rows.into_iter().filter(|r| r.source == "github") {
            let Some(repo) = row.source_repo.clone() else {
                continue;
            };
            let token = token.clone();
            let found = tokio::task::spawn_blocking(move || {
                github::latest_manifest(&repo, token.as_deref())
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
            if let Some((release, m)) = found {
                if m.id != row.id {
                    continue;
                }
                let update = GithubUpdate {
                    tag: release.tag.clone(),
                    version: m.version.clone(),
                    url: release.html_url.clone(),
                    checked_at: db::now_ms(),
                };
                if let Ok(mut map) = self.github_updates.write() {
                    let before = map.get(&row.id).map(|u| u.tag.clone());
                    if before.as_deref() != Some(update.tag.as_str()) {
                        changed = true;
                    }
                    map.insert(row.id.clone(), update);
                }
            }
        }
        changed
    }

    // -----------------------------------------------------------------------
    // Read side
    // -----------------------------------------------------------------------

    pub async fn overview(&self, can_manage: bool) -> AppResult<Overview> {
        let catalog = self.current_catalog();
        let rows = store::packages(&self.db).await?;
        let versions = store::versions(&self.db, None).await?;
        let consumers = store::consumers(&self.db).await?;
        let active = store::active_jobs(&self.db).await?;
        let systems = self.system.read().map(|g| g.clone()).unwrap_or_default();
        let shell = self.shell_status();
        let mut ids: Vec<String> = Vec::new();
        if let Some(c) = &catalog {
            ids.extend(c.packages.iter().map(|p| p.id.clone()));
        }
        for row in &rows {
            if !ids.contains(&row.id) {
                ids.push(row.id.clone());
            }
        }
        let mut packages = Vec::new();
        for id in ids {
            let row = rows.iter().find(|r| r.id == id);
            let installed: Vec<&VersionRow> =
                versions.iter().filter(|v| v.package_id == id).collect();
            let pkg_consumers: Vec<&ConsumerRow> =
                consumers.iter().filter(|c| c.package_id == id).collect();
            let job = active.iter().find(|j| j.package_id == id).cloned();
            if let Some(view) = self.package_view(
                &id,
                catalog.as_ref(),
                row,
                &installed,
                &pkg_consumers,
                job,
                systems.get(&id),
                shell.active,
            ) {
                packages.push(view);
            }
        }
        packages.sort_by(|a, b| {
            b.official
                .cmp(&a.official)
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let mut counts = BTreeMap::new();
        counts.insert("all".to_string(), packages.len());
        counts.insert(
            "installed".to_string(),
            packages
                .iter()
                .filter(|p| !p.installed.is_empty() || p.system.is_some())
                .count(),
        );
        counts.insert(
            "updates".to_string(),
            packages.iter().filter(|p| p.update_state != "none").count(),
        );
        counts.insert(
            "updatesReady".to_string(),
            packages
                .iter()
                .filter(|p| p.update_state == "ready")
                .count(),
        );
        Ok(Overview {
            host: self.host.clone(),
            catalog: self.status_snapshot(),
            shell,
            packages,
            counts,
            can_manage,
        })
    }

    pub async fn package(
        &self,
        id: &str,
        can_manage: bool,
    ) -> AppResult<(PackageView, Vec<JobHistory>)> {
        let overview = self.overview(can_manage).await?;
        let view = overview
            .packages
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(AppError::NotFound("package not found"))?;
        let jobs = store::job_history(&self.db, Some(id), 25).await?;
        Ok((view, jobs))
    }

    fn choice(
        &self,
        m: &Manifest,
        channels: &[catalog::Channel],
        installed_rev: Option<u32>,
    ) -> VersionChoice {
        let reason = host::incompatibility(m, &self.host);
        let channel = m.channel();
        let ch = channels.iter().find(|c| c.id == channel);
        let artifact = m.artifact_for(&self.host.target).map(ArtifactView::from);
        VersionChoice {
            version: m.version.clone(),
            revision: m.revision,
            channel_label: ch.map(|c| c.label.clone()).filter(|l| !l.is_empty()),
            channel_status: ch.map(|c| c.status.clone()).filter(|s| !s.is_empty()),
            channel,
            prerelease: m.is_prerelease(),
            release_date: m.release_date.clone(),
            compatible: reason.is_none(),
            reason,
            installed_revision: installed_rev,
            targets: m.targets(),
            artifact,
            bundled: m.runtime.bundled.clone(),
            extensions: m.runtime.extensions.clone(),
            executables: exe_paths("", m)
                .into_iter()
                .map(|e| ExecutablePath {
                    name: e.name,
                    path: e.path.trim_start_matches('/').to_string(),
                })
                .collect(),
            notes: m.notes.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn package_view(
        &self,
        id: &str,
        catalog: Option<&Catalog>,
        row: Option<&PackageRow>,
        installed: &[&VersionRow],
        consumers: &[&ConsumerRow],
        active_job: Option<JobHistory>,
        system: Option<&SystemInstall>,
        shell_active: bool,
    ) -> Option<PackageView> {
        let from_catalog = catalog.and_then(|c| c.package(id));
        let source = row
            .map(|r| r.source.clone())
            .unwrap_or_else(|| "official".into());
        let official = source == "official";
        // Only official runtimes are matched with the system's commands.
        let system = system.filter(|_| official);
        // Official packages come from the catalog; installed official packages
        // that left the catalog are still shown from their stored manifests.
        let newest_installed = installed.first().map(|v| &v.manifest);
        let base = if official {
            from_catalog
                .and_then(|p| p.versions.first())
                .or(newest_installed)
        } else {
            newest_installed
        };
        // A catalog package without any verified version yet is still listed
        // (with the reason) from its catalog entry.
        if base.is_none() && !(official && from_catalog.is_some()) {
            return None;
        }
        let default_version = row.and_then(|r| r.default_version.clone());
        let channels = from_catalog.map(|p| p.channels.clone()).unwrap_or_default();
        let pinned = |version: &str| -> Vec<String> {
            consumers
                .iter()
                .filter(|c| c.version.as_deref() == Some(version))
                .map(|c| c.name.clone())
                .collect()
        };
        let installed_views: Vec<InstalledView> = installed
            .iter()
            .map(|v| {
                let path = std::path::Path::new(&v.install_path);
                let artifact = v
                    .manifest
                    .artifact_for(&v.target)
                    .map(ArtifactView::from)
                    .unwrap_or(ArtifactView {
                        target: v.target.clone(),
                        url: v.artifact_url.clone(),
                        size: 0,
                        sha256: v.artifact_sha256.clone(),
                        requires: Requirements::default(),
                        provenance: Provenance::default(),
                    });
                InstalledView {
                    version: v.version.clone(),
                    revision: v.revision,
                    channel: v.manifest.channel(),
                    target: v.target.clone(),
                    path: v.install_path.clone(),
                    executables: exe_paths(&v.install_path, &v.manifest),
                    is_default: default_version.as_deref() == Some(v.version.as_str()),
                    status: if path.is_dir() {
                        "ok".into()
                    } else {
                        "missing".into()
                    },
                    installed_at: v.installed_at,
                    installed_by: v.installed_by.clone(),
                    size_bytes: v.size_bytes,
                    check_output: v.check_output.clone(),
                    release_tag: v.release_tag.clone(),
                    artifact,
                    bundled: v.manifest.runtime.bundled.clone(),
                    pinned_by: pinned(&v.version),
                }
            })
            .collect();
        let installed_rev = |version: &str| {
            installed
                .iter()
                .find(|v| v.version == version)
                .map(|v| v.revision)
        };
        let versions: Vec<VersionChoice> = if official {
            from_catalog
                .map(|p| {
                    p.versions
                        .iter()
                        .map(|m| self.choice(m, &channels, installed_rev(&m.version)))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            installed
                .iter()
                .map(|v| self.choice(&v.manifest, &channels, Some(v.revision)))
                .collect()
        };
        let latest = versions
            .iter()
            .find(|v| v.compatible && !v.prerelease)
            .cloned();

        // Updates: per installed channel, the newest compatible newer version
        // (or a newer revision of the same version).
        let mut updates = Vec::new();
        let mut channels_installed: Vec<String> = Vec::new();
        for v in installed {
            let ch = v.manifest.channel();
            if channels_installed.contains(&ch) {
                continue;
            }
            channels_installed.push(ch.clone());
            let newest = installed
                .iter()
                .filter(|x| x.manifest.channel() == ch)
                .max_by(|a, b| {
                    version::compare(&a.version, &b.version).then(a.revision.cmp(&b.revision))
                });
            let Some(newest) = newest else { continue };
            let candidate = versions.iter().find(|c| {
                c.channel == ch
                    && c.compatible
                    && (!c.prerelease || newest.manifest.is_prerelease())
                    && match version::compare(&c.version, &newest.version) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Equal => c.revision > newest.revision,
                        std::cmp::Ordering::Less => false,
                    }
            });
            if let Some(c) = candidate {
                updates.push(UpdateView {
                    channel: ch.clone(),
                    from: newest.version.clone(),
                    to: c.version.clone(),
                    revision: c.revision,
                    replaces_default: installed.iter().any(|x| {
                        x.manifest.channel() == ch
                            && default_version.as_deref() == Some(x.version.as_str())
                    }),
                    from_system: false,
                });
            }
        }
        // The system's own runtime: a newer verified version of its release
        // line (unless Package Center already has that line) is an update,
        // installed next to it. It becomes the default when nothing else is.
        let channel_ids: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
        let system_version = system.and_then(|s| s.version.clone());
        let system_channel = system_version
            .as_deref()
            .map(|v| system::channel_of(v, &channel_ids));
        if let (Some(sv), Some(sch)) = (&system_version, &system_channel) {
            if !channels_installed.contains(sch) {
                let candidate = versions.iter().find(|c| {
                    &c.channel == sch
                        && c.compatible
                        && !c.prerelease
                        && version::compare(&c.version, sv) == std::cmp::Ordering::Greater
                });
                if let Some(c) = candidate {
                    updates.push(UpdateView {
                        channel: sch.clone(),
                        from: sv.clone(),
                        to: c.version.clone(),
                        revision: c.revision,
                        replaces_default: default_version.is_none(),
                        from_system: true,
                    });
                }
            }
        }
        // Detected: upstream versions newer than everything verified in their
        // channel (official workflow) or new releases of an imported package.
        let mut detected: Vec<DetectedView> = Vec::new();
        if let Some(p) = from_catalog {
            for d in &p.detected {
                let ch = if d.channel.is_empty() {
                    version::Version::parse(&d.version)
                        .map(|v| v.default_channel())
                        .unwrap_or_default()
                } else {
                    d.channel.clone()
                };
                let newer_than_ready = versions.iter().filter(|c| c.channel == ch).all(|c| {
                    version::compare(&d.version, &c.version) == std::cmp::Ordering::Greater
                });
                if newer_than_ready {
                    detected.push(detected_view(d, &ch));
                }
            }
        }
        if !official {
            if let Ok(map) = self.github_updates.read() {
                if let Some(u) = map.get(id) {
                    let newer = installed.iter().all(|v| {
                        version::compare(&u.version, &v.version) == std::cmp::Ordering::Greater
                    });
                    if newer {
                        detected.push(DetectedView {
                            version: u.version.clone(),
                            channel: version::Version::parse(&u.version).map(|v| v.default_channel()).unwrap_or_default(),
                            state: "review".into(),
                            detail: format!("Release {} was published. Review and approve it with Install from GitHub.", u.tag),
                            detected_at: String::new(),
                            source: "github".into(),
                            release_url: Some(u.url.clone()),
                        });
                    }
                }
            }
        }
        let relevant_detected = detected.iter().any(|d| {
            d.source == "github"
                || channels_installed.contains(&d.channel)
                || system_channel.as_deref() == Some(d.channel.as_str())
        });
        let update_state = if !updates.is_empty() {
            "ready"
        } else if (!installed.is_empty() || system.is_some()) && relevant_detected {
            "detected"
        } else {
            "none"
        };
        let default_manifest = default_version
            .as_deref()
            .and_then(|dv| installed.iter().find(|v| v.version == dv))
            .map(|v| &v.manifest);
        let current = self.layout.current_link(id).display().to_string();
        let default_executables = default_manifest
            .map(|m| exe_paths(&current, m))
            .unwrap_or_default();
        let consumer_views = consumers
            .iter()
            .map(|c| self.consumer_view(c, installed, default_version.as_deref()))
            .collect();
        // A login shell finds the default's shim first when the PATH entry is
        // in place; otherwise the system's command, if it is on PATH.
        let command = system::primary_command(id).map(str::to_string).or_else(|| {
            default_manifest
                .and_then(|m| m.runtime.executables.first())
                .map(|e| e.name.clone())
        });
        let shim = command
            .as_ref()
            .filter(|c| default_manifest.is_some_and(|m| m.executable(c).is_some()));
        let terminal = match (shim, system.filter(|s| s.on_path)) {
            (Some(c), _) if shell_active => Some(TerminalView {
                command: c.clone(),
                source: "package-center".into(),
                version: default_version.clone(),
                path: self.layout.bin_dir().join(c).display().to_string(),
            }),
            (_, Some(s)) => Some(TerminalView {
                command: s.command.clone(),
                source: "system".into(),
                version: s.version.clone(),
                path: s.path.clone(),
            }),
            _ => None,
        };
        let unavailable_reason =
            if official && !versions.is_empty() && !versions.iter().any(|v| v.compatible) {
                versions.first().and_then(|v| v.reason.clone())
            } else if official && versions.is_empty() && installed.is_empty() {
                Some("No verified version is published in the catalog yet.".into())
            } else {
                None
            };
        let verification = if official {
            let key = self
                .catalog
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|v| (v.key_id.clone(), v.official_key)));
            Verification {
                kind: "signed-catalog".into(),
                label: if key.as_ref().is_some_and(|k| k.1) { "Official · signed catalog".into() } else { "Signed catalog".into() },
                detail: format!(
                    "Listed in the catalog signed with key {}; each artifact is pinned by SHA-256 and size and checked after download.",
                    key.map(|k| k.0).unwrap_or_else(|| "?".into())
                ),
            }
        } else {
            Verification {
                kind: "github-release".into(),
                label: "Third-party · approved GitHub source".into(),
                detail: format!(
                    "Installed from a release of {} approved by an administrator; artifacts are pinned to that release and their SHA-256. RAST does not vouch for the publisher.",
                    row.and_then(|r| r.source_repo.clone()).unwrap_or_default()
                ),
            }
        };
        Some(PackageView {
            id: id.to_string(),
            package_type: base
                .map(|b| b.package_type.clone())
                .or_else(|| from_catalog.map(|p| p.package_type.clone()))
                .unwrap_or_default(),
            name: from_catalog
                .map(|p| p.name.clone())
                .or_else(|| base.map(|b| b.name.clone()))
                .unwrap_or_default(),
            description: from_catalog
                .map(|p| p.description.clone())
                .or_else(|| base.map(|b| b.description.clone()))
                .unwrap_or_default(),
            homepage: from_catalog
                .and_then(|p| p.homepage.clone())
                .or_else(|| base.and_then(|b| b.homepage.clone())),
            license: from_catalog
                .and_then(|p| p.license.clone())
                .or_else(|| base.and_then(|b| b.license.clone())),
            publisher: base
                .map(|b| b.publisher.clone())
                .or_else(|| catalog.map(|c| c.publisher.clone()))
                .unwrap_or_else(|| manifest::Publisher {
                    id: String::new(),
                    name: String::new(),
                    url: None,
                }),
            source: source.clone(),
            source_repo: row.and_then(|r| r.source_repo.clone()),
            official,
            verification,
            in_catalog: from_catalog.is_some(),
            installed: installed_views,
            default_version,
            default_executables,
            latest,
            versions,
            unsupported: from_catalog
                .map(|p| p.unsupported.clone())
                .unwrap_or_default(),
            updates,
            detected,
            update_state: update_state.into(),
            active_job,
            consumers: consumer_views,
            unavailable_reason,
            system: system.cloned(),
            terminal,
        })
    }

    fn consumer_view(
        &self,
        c: &ConsumerRow,
        installed: &[&VersionRow],
        default: Option<&str>,
    ) -> ConsumerView {
        let effective = c.version.clone().or_else(|| default.map(str::to_string));
        let manifest = effective
            .as_deref()
            .and_then(|ev| installed.iter().find(|v| v.version == ev))
            .map(|v| &v.manifest);
        let base = match &c.version {
            Some(v) => self
                .layout
                .version_dir(&c.package_id, v)
                .display()
                .to_string(),
            None => self
                .layout
                .current_link(&c.package_id)
                .display()
                .to_string(),
        };
        ConsumerView {
            id: c.id,
            name: c.name.clone(),
            description: c.description.clone(),
            package_id: c.package_id.clone(),
            version: c.version.clone(),
            follows_default: c.version.is_none(),
            effective_version: effective,
            executables: manifest.map(|m| exe_paths(&base, m)).unwrap_or_default(),
            created_by: c.created_by.clone(),
            updated_at: c.updated_at,
        }
    }

    // -----------------------------------------------------------------------
    // Install / update
    // -----------------------------------------------------------------------

    fn lock(&self, id: &str, label: &str) -> AppResult<BusyGuard> {
        let mut set = self
            .busy
            .lock()
            .map_err(|_| AppError::internal("package lock poisoned"))?;
        if !set.insert(id.to_string()) {
            return Err(AppError::conflict(format!(
                "Another operation on {label} is still running. Wait for it to finish."
            )));
        }
        Ok(BusyGuard {
            busy: self.busy.clone(),
            id: id.to_string(),
        })
    }

    /// Install (or update to) a catalog version as a background job.
    pub async fn install_official(
        self: &Arc<Self>,
        user: &AuthUser,
        id: &str,
        requested: Option<&str>,
        make_default: Option<bool>,
    ) -> AppResult<String> {
        let catalog = self.current_catalog().ok_or_else(|| {
            AppError::unavailable(
                "The package catalog has not been loaded yet. Check for updates first.",
            )
        })?;
        if catalog.expires_at_ms().is_some_and(|t| db::now_ms() > t) {
            return Err(AppError::unavailable(format!(
                "The package catalog expired at {}. Check for updates; if the check fails, the catalog source may be serving a stale copy.",
                catalog.expires_at.as_deref().unwrap_or("an earlier date")
            )));
        }
        let pkg = catalog
            .package(id)
            .ok_or(AppError::NotFound("package not found in the catalog"))?;
        if let Some(row) = store::package(&self.db, id).await? {
            if row.source != "official" {
                return Err(AppError::conflict(
                    "This package id is installed from another source.",
                ));
            }
        }
        let manifest = match requested {
            Some(v) => pkg
                .versions
                .iter()
                .find(|m| m.version == v)
                .ok_or_else(|| {
                    AppError::bad_request(format!("{} {v} is not in the catalog", pkg.name))
                })?,
            None => {
                // Update: newest compatible version in the default's channel.
                // The channel comes from the installed manifest (Node.js
                // streams are majors, not major.minor).
                let row = store::package(&self.db, id).await?;
                let installed = store::versions(&self.db, Some(id)).await?;
                let channel_ids: Vec<String> = pkg.channels.iter().map(|c| c.id.clone()).collect();
                // Without a default, the system runtime's release line.
                let channel = row
                    .and_then(|r| r.default_version)
                    .and_then(|dv| {
                        installed
                            .iter()
                            .find(|v| v.version == dv)
                            .map(|v| v.manifest.channel())
                    })
                    .or_else(|| {
                        self.system_install(id)
                            .and_then(|s| s.version)
                            .map(|v| system::channel_of(&v, &channel_ids))
                    });
                pkg.versions
                    .iter()
                    .find(|m| {
                        host::incompatibility(m, &self.host).is_none()
                            && !m.is_prerelease()
                            && channel.as_ref().is_none_or(|c| &m.channel() == c)
                    })
                    .ok_or_else(|| AppError::bad_request("No compatible version is available."))?
            }
        };
        self.start_install(user, manifest.clone(), "official", None, None, make_default)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_install(
        self: &Arc<Self>,
        user: &AuthUser,
        manifest: Manifest,
        source: &str,
        source_repo: Option<String>,
        release_tag: Option<String>,
        make_default: Option<bool>,
    ) -> AppResult<String> {
        if let Some(reason) = host::incompatibility(&manifest, &self.host) {
            return Err(AppError::bad_request(reason));
        }
        let artifact = manifest
            .artifact_for(&self.host.target)
            .cloned()
            .ok_or_else(|| AppError::bad_request("No artifact for this server."))?;
        // Hold the package lock before checking anything, so nothing about
        // this id changes between the checks and the job.
        let guard = self.lock(&manifest.id, &manifest.name)?;
        self.check_ownership(&manifest, source, source_repo.as_deref())
            .await?;
        let installed = store::versions(&self.db, Some(&manifest.id)).await?;
        if let Some(existing) = installed.iter().find(|v| v.version == manifest.version) {
            let files_ok = std::path::Path::new(&existing.install_path).is_dir();
            if files_ok && existing.revision >= manifest.revision {
                return Err(AppError::conflict(format!(
                    "{} {} is already installed.",
                    manifest.name, manifest.version
                )));
            }
        }
        let row = store::package(&self.db, &manifest.id).await?;
        let has_default = row
            .as_ref()
            .and_then(|r| r.default_version.clone())
            .is_some();
        // The first Package Center version becomes the default, unless the
        // system already provides the command (then the caller decides).
        let system = self
            .system_install(&manifest.id)
            .filter(|_| source == "official");
        let make_default =
            make_default.unwrap_or(!has_default) || (!has_default && system.is_none());
        let policy = match source {
            "official" => self
                .current_catalog()
                .map(|c| c.origin_policy())
                .ok_or_else(|| AppError::unavailable("catalog not loaded"))?,
            _ => github::download_policy(),
        };
        let mut action = install_action(
            installed
                .iter()
                .map(|v| (v.version.as_str(), v.manifest.channel())),
            &manifest.version,
            &manifest.channel(),
        );
        // A newer version of the system runtime's own release line.
        let channel = manifest.channel();
        if action == "install"
            && system.and_then(|s| s.version).is_some_and(|sv| {
                system::channel_of(&sv, std::slice::from_ref(&channel)) == channel
                    && version::compare(&manifest.version, &sv) == std::cmp::Ordering::Greater
            })
        {
            action = "update";
        }
        let action = action.to_string();
        let me = self.clone();
        let actor = user.clone();
        let title = format!(
            "{} {} {}",
            match action.as_str() {
                "update" => "Update to",
                "reinstall" => "Reinstall",
                _ => "Install",
            },
            manifest.name,
            manifest.version
        );
        let job_manifest = manifest.clone();
        let job_action = action.clone();
        let source = source.to_string();
        let spec = JobSpec::new(
            format!("package.{action}"),
            move |ctx: JobContext| async move {
                let _guard = guard;
                me.run_install(
                    ctx,
                    actor,
                    job_manifest,
                    artifact,
                    policy,
                    source,
                    source_repo,
                    release_tag,
                    make_default,
                    job_action,
                )
                .await
            },
        )
        .actor(user)
        .message(title);
        let job_id = self.jobs.submit(spec).await?;
        store::link_job(&self.db, &job_id, &manifest.id, &manifest.version, &action).await?;
        self.events.publish(
            "package.changed",
            json!({"packageId": manifest.id, "version": manifest.version, "reason": "queued", "jobId": job_id}),
        );
        Ok(job_id)
    }

    /// Who may install under this package id, checked again for every
    /// install (a preview can be stale): an id belongs to one source — the
    /// official catalog, or one GitHub repository with one publisher — and an
    /// imported package can never take an official id or command name.
    async fn check_ownership(
        &self,
        manifest: &Manifest,
        source: &str,
        source_repo: Option<&str>,
    ) -> AppResult<()> {
        if let Some(row) = store::package(&self.db, &manifest.id).await? {
            if !store::same_owner(&row, source, source_repo, &manifest.publisher.id) {
                return Err(AppError::conflict(format!(
                    "Package id {:?} is already installed from {} (publisher {:?}).",
                    manifest.id,
                    row.source_repo.as_deref().unwrap_or("the official catalog"),
                    row.publisher_id
                )));
            }
        }
        if source == "official" {
            return Ok(());
        }
        let catalog = self.current_catalog().ok_or_else(|| {
            AppError::unavailable(
                "Load the official package catalog first (Check for updates): RAST uses it to make sure an imported package does not take an official package id or command name.",
            )
        })?;
        if catalog.package(&manifest.id).is_some() {
            return Err(AppError::conflict(format!(
                "Package id {:?} belongs to the official catalog; a third-party package cannot replace it.",
                manifest.id
            )));
        }
        let official: HashSet<&str> = catalog
            .packages
            .iter()
            .flat_map(|p| p.versions.iter())
            .flat_map(|m| m.runtime.executables.iter().map(|e| e.name.as_str()))
            .collect();
        if let Some(exe) = manifest
            .runtime
            .executables
            .iter()
            .find(|e| official.contains(e.name.as_str()))
        {
            return Err(AppError::conflict(format!(
                "{:?} is a command of an official package; a third-party package cannot provide it.",
                exe.name
            )));
        }
        if let Some(reason) = sandbox::imported_unsupported_reason() {
            return Err(AppError::unavailable(reason));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_install(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        manifest: Manifest,
        artifact: Artifact,
        policy: super::net::OriginPolicy,
        source: String,
        source_repo: Option<String>,
        release_tag: Option<String>,
        make_default: bool,
        action: String,
    ) -> AppResult<serde_json::Value> {
        self.events.publish(
            "package.changed",
            json!({"packageId": manifest.id, "version": manifest.version, "reason": "started", "jobId": ctx.id}),
        );
        let result = self
            .install_inner(
                &ctx,
                &actor,
                &manifest,
                &artifact,
                policy,
                &source,
                source_repo.as_deref(),
                release_tag.as_deref(),
                make_default,
            )
            .await;
        let audit_action = format!("package.{action}");
        let log = AuditLog::new(audit_action)
            .actor(&actor)
            .object("package", manifest.id.clone())
            .path(format!("{} {}", manifest.id, manifest.version));
        match &result {
            Ok(value) => {
                // Leave a summary in the job history instead of the last phase.
                let became_default = value
                    .get("default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                ctx.set_message(&format!(
                    "Installed and verified {} {}{}",
                    manifest.name,
                    manifest.version,
                    if became_default {
                        " · RAST default"
                    } else {
                        ""
                    }
                ))
                .await;
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    log.success().detail(format!(
                        "version={} revision={} source={}{} sha256={} default={}",
                        manifest.version,
                        manifest.revision,
                        source,
                        source_repo
                            .as_deref()
                            .map(|r| format!(" repo={r}"))
                            .unwrap_or_default(),
                        artifact.sha256,
                        value
                            .get("default")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    )),
                )
                .await;
                self.events.publish(
                    "package.changed",
                    json!({"packageId": manifest.id, "version": manifest.version, "reason": "installed", "jobId": ctx.id}),
                );
            }
            Err(e) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    log.failure().detail(format!(
                        "version={} error={}",
                        manifest.version,
                        e.public_message()
                    )),
                )
                .await;
                self.events.publish(
                    "package.changed",
                    json!({"packageId": manifest.id, "version": manifest.version, "reason": "failed", "jobId": ctx.id, "error": e.public_message()}),
                );
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_inner(
        &self,
        ctx: &JobContext,
        actor: &AuthUser,
        manifest: &Manifest,
        artifact: &Artifact,
        policy: super::net::OriginPolicy,
        source: &str,
        source_repo: Option<&str>,
        release_tag: Option<&str>,
        make_default: bool,
    ) -> AppResult<serde_json::Value> {
        let handle = tokio::runtime::Handle::current();
        let layout = self.layout.clone();
        let m = manifest.clone();
        let a = artifact.clone();
        let job = ctx.clone();
        // Imported packages are third-party code: their checks must run
        // under Landlock (see `sandbox`).
        let untrusted = source != "official";
        let installed = tokio::task::spawn_blocking(move || {
            let mut reporter = JobReporter {
                ctx: job.clone(),
                handle,
            };
            install::install_version(&layout, &m, &a, &policy, &job.id, untrusted, &mut reporter)
        })
        .await
        .map_err(|e| AppError::internal(format!("install task failed: {e}")))?;
        let installed = match installed {
            Ok(i) => i,
            Err(e) if e == "cancelled" => return Err(AppError::Cancelled),
            Err(e) => return Err(AppError::bad_request(e)),
        };
        // Other users run it through the login-shell PATH.
        if self.config.packages_shell_path {
            if let Err(e) = shell::share_version(&self.layout, &manifest.id, &manifest.version) {
                tracing::warn!(package = %manifest.id, error = %e, "cannot make the installed version readable by other users");
            }
        }
        let check_output = installed
            .checks
            .iter()
            .map(|c| format!("{}: {}", c.command, c.output))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.progress(0, 0, "Recording installation").await;
        let recorded = store::record_install(
            &self.db,
            store::NewVersion {
                manifest,
                source,
                source_repo,
                target: &artifact.target,
                artifact_url: &artifact.url,
                artifact_sha256: &artifact.sha256,
                release_tag,
                install_path: &installed.path.display().to_string(),
                size_bytes: installed.size_bytes,
                check_output: &check_output,
                installed_by: &actor.username,
            },
        )
        .await;
        // A replaced tree is deleted only once the new one is recorded;
        // otherwise it goes back so the files match the database.
        if let Some(aside) = installed.displaced.clone() {
            let target = installed.path.clone();
            let keep_new = recorded.is_ok();
            let settled = tokio::task::spawn_blocking(move || {
                install::settle_replacement(&target, &aside, keep_new)
            })
            .await;
            if let Ok(Err(e)) = settled {
                tracing::warn!(package = %manifest.id, error = %e, "cannot settle a replaced package version");
            }
        }
        recorded?;
        let mut activation = None;
        if make_default {
            ctx.progress(0, 0, "Making it the RAST default").await;
            activation = Some(self.activate_version(&manifest.id, &manifest.version).await.map_err(|e| {
                AppError::bad_request(format!(
                    "{} {} was installed, but making it the default failed (the previous default is unchanged): {}",
                    manifest.name,
                    manifest.version,
                    e.public_message()
                ))
            })?);
        }
        Ok(json!({
            "packageId": manifest.id,
            "version": manifest.version,
            "revision": manifest.revision,
            "path": installed.path.display().to_string(),
            "sizeBytes": installed.size_bytes,
            "checks": installed.checks,
            "default": make_default,
            "activation": activation,
        }))
    }

    /// Switch `current` + shims, then record the default.
    async fn activate_version(&self, id: &str, version: &str) -> AppResult<install::Activation> {
        let versions = store::versions(&self.db, Some(id)).await?;
        let target = versions
            .iter()
            .find(|v| v.version == version)
            .ok_or(AppError::NotFound("version not installed"))?
            .manifest
            .clone();
        let row = store::package(&self.db, id).await?;
        let previous = row.and_then(|r| r.default_version).and_then(|dv| {
            versions
                .iter()
                .find(|v| v.version == dv)
                .map(|v| v.manifest.clone())
        });
        let layout = self.layout.clone();
        let activation = tokio::task::spawn_blocking(move || {
            install::activate(&layout, &target, previous.as_ref())
        })
        .await
        .map_err(|e| AppError::internal(format!("activation task failed: {e}")))?
        .map_err(AppError::bad_request)?;
        store::set_default(&self.db, id, Some(version)).await?;
        Ok(activation)
    }

    pub async fn set_default(
        &self,
        user: &AuthUser,
        id: &str,
        version: &str,
    ) -> AppResult<install::Activation> {
        let row = store::package(&self.db, id)
            .await?
            .ok_or(AppError::NotFound("package is not installed"))?;
        let _guard = self.lock(id, &row.name)?;
        let versions = store::versions(&self.db, Some(id)).await?;
        let v = versions
            .iter()
            .find(|v| v.version == version)
            .ok_or_else(|| {
                AppError::bad_request(format!("{} {version} is not installed", row.name))
            })?;
        if !std::path::Path::new(&v.install_path).is_dir() {
            return Err(AppError::conflict(format!(
                "The files of {} {version} are missing; reinstall it first.",
                row.name
            )));
        }
        let previous = row.default_version.clone();
        let result = self.activate_version(id, version).await;
        let log = AuditLog::new("package.activate")
            .actor(user)
            .object("package", id.to_string())
            .path(format!("{id} {version}"));
        match &result {
            Ok(a) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    log.success().detail(format!(
                        "default {} -> {version}{}",
                        previous.as_deref().unwrap_or("none"),
                        if a.conflicts.is_empty() {
                            String::new()
                        } else {
                            format!(" (shim conflicts: {})", a.conflicts.join(", "))
                        }
                    )),
                )
                .await
            }
            Err(e) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    log.failure().detail(e.public_message()),
                )
                .await
            }
        }
        self.events.publish(
            "package.changed",
            json!({"packageId": id, "version": version, "reason": "default"}),
        );
        result
    }

    /// Go back to the system's own runtime: remove the default's `current`
    /// link and shims, so a terminal finds the system command again. The
    /// installed versions stay.
    pub async fn use_system(&self, user: &AuthUser, id: &str) -> AppResult<()> {
        let row = store::package(&self.db, id)
            .await?
            .ok_or(AppError::NotFound("package is not installed"))?;
        let _guard = self.lock(id, &row.name)?;
        let system = self
            .system_install(id)
            .filter(|s| s.on_path && row.source == "official")
            .ok_or_else(|| {
                AppError::bad_request(format!(
                    "This server has no system {} on its PATH.",
                    row.name
                ))
            })?;
        let Some(previous) = row.default_version.clone() else {
            return Ok(());
        };
        let consumers = store::consumers(&self.db).await?;
        let followers: Vec<&str> = consumers
            .iter()
            .filter(|c| c.package_id == id && c.version.is_none())
            .map(|c| c.name.as_str())
            .collect();
        let log = AuditLog::new("package.activate")
            .actor(user)
            .object("package", id.to_string())
            .path(format!("{id} system"));
        if !followers.is_empty() {
            let message = format!(
                "{} follow the default {} version. Pin them to a version or remove them first.",
                followers.join(", "),
                row.name
            );
            audit::record_with_events(
                &self.db,
                &self.events,
                log.failure().detail(message.clone()),
            )
            .await;
            return Err(AppError::conflict(message));
        }
        let versions = store::versions(&self.db, Some(id)).await?;
        let manifest = versions
            .iter()
            .find(|v| v.version == previous)
            .map(|v| v.manifest.clone());
        let layout = self.layout.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(m) = &manifest {
                install::deactivate(&layout, m);
            }
        })
        .await
        .map_err(|e| AppError::internal(format!("deactivation task failed: {e}")))?;
        store::set_default(&self.db, id, None).await?;
        audit::record_with_events(
            &self.db,
            &self.events,
            log.success().detail(format!(
                "default {previous} -> system {} ({})",
                system.version.as_deref().unwrap_or("unknown version"),
                system.path
            )),
        )
        .await;
        self.events.publish(
            "package.changed",
            json!({"packageId": id, "reason": "default"}),
        );
        Ok(())
    }

    pub async fn remove_version(&self, user: &AuthUser, id: &str, version: &str) -> AppResult<()> {
        let row = store::package(&self.db, id)
            .await?
            .ok_or(AppError::NotFound("package is not installed"))?;
        let _guard = self.lock(id, &row.name)?;
        let versions = store::versions(&self.db, Some(id)).await?;
        if !versions.iter().any(|v| v.version == version) {
            return Err(AppError::NotFound("version not installed"));
        }
        let consumers = store::consumers(&self.db).await?;
        let pinned: Vec<&str> = consumers
            .iter()
            .filter(|c| c.package_id == id && c.version.as_deref() == Some(version))
            .map(|c| c.name.as_str())
            .collect();
        let refuse = |message: String| async {
            audit::record_with_events(
                &self.db,
                &self.events,
                AuditLog::new("package.remove")
                    .actor(user)
                    .object("package", id.to_string())
                    .path(format!("{id} {version}"))
                    .failure()
                    .detail(message.clone()),
            )
            .await;
            Err(AppError::conflict(message))
        };
        if !pinned.is_empty() {
            return refuse(format!(
                "{} {version} is pinned by {}. Change or remove those consumers first.",
                row.name,
                pinned.join(", ")
            ))
            .await;
        }
        let is_default = row.default_version.as_deref() == Some(version);
        if is_default {
            if versions.len() > 1 {
                return refuse(format!(
                    "{} {version} is the RAST default. Make another version the default{} first.",
                    row.name,
                    if self.system_install(id).is_some_and(|s| s.on_path) {
                        " (or use the system version)"
                    } else {
                        ""
                    }
                ))
                .await;
            }
            let followers: Vec<&str> = consumers
                .iter()
                .filter(|c| c.package_id == id && c.version.is_none())
                .map(|c| c.name.as_str())
                .collect();
            if !followers.is_empty() {
                return refuse(format!(
                    "{} follow the default {} version. Change or remove those consumers first.",
                    followers.join(", "),
                    row.name
                ))
                .await;
            }
        }
        let manifest = versions
            .iter()
            .find(|v| v.version == version)
            .map(|v| v.manifest.clone());
        let layout = self.layout.clone();
        let (pid, ver) = (id.to_string(), version.to_string());
        tokio::task::spawn_blocking(move || {
            if is_default {
                if let Some(m) = &manifest {
                    install::deactivate(&layout, m);
                }
            }
            install::remove_version(&layout, &pid, &ver)
        })
        .await
        .map_err(|e| AppError::internal(format!("remove task failed: {e}")))?
        .map_err(AppError::internal)?;
        store::delete_version(&self.db, id, version).await?;
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("package.remove")
                .actor(user)
                .object("package", id.to_string())
                .path(format!("{id} {version}"))
                .success()
                .detail(format!(
                    "removed {version}{}",
                    if is_default { " (was default)" } else { "" }
                )),
        )
        .await;
        self.events.publish(
            "package.changed",
            json!({"packageId": id, "version": version, "reason": "removed"}),
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Consumers
    // -----------------------------------------------------------------------

    async fn validate_consumer(&self, package_id: &str, version: Option<&str>) -> AppResult<()> {
        let row = store::package(&self.db, package_id)
            .await?
            .ok_or_else(|| AppError::bad_request("Install the package before adding consumers."))?;
        match version {
            Some(v) => {
                let versions = store::versions(&self.db, Some(package_id)).await?;
                if !versions.iter().any(|x| x.version == v) {
                    return Err(AppError::bad_request(format!(
                        "{} {v} is not installed",
                        row.name
                    )));
                }
            }
            None => {
                if row.default_version.is_none() {
                    return Err(AppError::bad_request(format!(
                        "{} has no default version yet",
                        row.name
                    )));
                }
            }
        }
        Ok(())
    }

    pub async fn create_consumer(
        &self,
        user: &AuthUser,
        name: &str,
        description: &str,
        package_id: &str,
        version: Option<&str>,
    ) -> AppResult<i64> {
        let name = consumer_name(name)?;
        let description = description.trim();
        if description.chars().count() > 300 {
            return Err(AppError::bad_request(
                "description must be at most 300 characters",
            ));
        }
        self.validate_consumer(package_id, version).await?;
        let now = db::now_ms();
        let res = sqlx::query(
            "INSERT INTO package_consumers (name, description, package_id, version, created_by, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&name)
        .bind(description)
        .bind(package_id)
        .bind(version)
        .bind(&user.username)
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await;
        let id = match res {
            Ok(r) => r.last_insert_rowid(),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(AppError::conflict(format!(
                    "A consumer named {name:?} already exists."
                )))
            }
            Err(e) => return Err(e.into()),
        };
        self.consumer_audit(user, "package.consumer_create", package_id, &name, version)
            .await;
        Ok(id)
    }

    pub async fn update_consumer(
        &self,
        user: &AuthUser,
        id: i64,
        name: &str,
        description: &str,
        version: Option<&str>,
    ) -> AppResult<()> {
        let existing = store::consumer(&self.db, id)
            .await?
            .ok_or(AppError::NotFound("consumer not found"))?;
        let name = consumer_name(name)?;
        self.validate_consumer(&existing.package_id, version)
            .await?;
        let res = sqlx::query(
            "UPDATE package_consumers SET name = ?, description = ?, version = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&name)
        .bind(description.trim())
        .bind(version)
        .bind(db::now_ms())
        .bind(id)
        .execute(&self.db)
        .await;
        if let Err(sqlx::Error::Database(e)) = &res {
            if e.is_unique_violation() {
                return Err(AppError::conflict(format!(
                    "A consumer named {name:?} already exists."
                )));
            }
        }
        res?;
        self.consumer_audit(
            user,
            "package.consumer_update",
            &existing.package_id,
            &name,
            version,
        )
        .await;
        Ok(())
    }

    pub async fn delete_consumer(&self, user: &AuthUser, id: i64) -> AppResult<()> {
        let existing = store::consumer(&self.db, id)
            .await?
            .ok_or(AppError::NotFound("consumer not found"))?;
        sqlx::query("DELETE FROM package_consumers WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        self.consumer_audit(
            user,
            "package.consumer_delete",
            &existing.package_id,
            &existing.name,
            existing.version.as_deref(),
        )
        .await;
        Ok(())
    }

    async fn consumer_audit(
        &self,
        user: &AuthUser,
        action: &str,
        package_id: &str,
        name: &str,
        version: Option<&str>,
    ) {
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new(action)
                .actor(user)
                .object("package", package_id.to_string())
                .path(name.to_string())
                .success()
                .detail(format!("version={}", version.unwrap_or("default"))),
        )
        .await;
        self.events.publish(
            "package.changed",
            json!({"packageId": package_id, "reason": "consumers"}),
        );
    }

    // -----------------------------------------------------------------------
    // Install from GitHub
    // -----------------------------------------------------------------------

    pub async fn inspect_github(&self, user: &AuthUser, input: &str) -> AppResult<Preview> {
        let reference: GithubRef = github::parse_link(input).map_err(AppError::bad_request)?;
        let token = self.config.packages_github_token.clone();
        let r = reference.clone();
        let inspected = tokio::task::spawn_blocking(move || github::inspect(&r, token.as_deref()))
            .await
            .map_err(|e| AppError::internal(format!("inspection task failed: {e}")))?
            .map_err(AppError::bad_request)?;
        let m = &inspected.manifest;
        if self
            .current_catalog()
            .is_some_and(|c| c.package(&m.id).is_some())
        {
            return Err(AppError::conflict(format!(
                "Package id {:?} belongs to the official catalog; a third-party package cannot replace it.",
                m.id
            )));
        }
        if let Some(row) = store::package(&self.db, &m.id).await? {
            let same_source = row.source == "github"
                && row
                    .source_repo
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&inspected.repo));
            if !same_source {
                return Err(AppError::conflict(format!(
                    "Package id {:?} is already installed from {}.",
                    m.id,
                    row.source_repo.as_deref().unwrap_or("the official catalog")
                )));
            }
        }
        let approved = store::approved_source(&self.db, &inspected.repo).await?;
        if let Some((publisher, _, _)) = &approved {
            if publisher != &m.publisher.id {
                return Err(AppError::conflict(format!(
                    "{} was approved for publisher {publisher:?}, but this release declares {:?}.",
                    inspected.repo, m.publisher.id
                )));
            }
        }
        let reason =
            host::incompatibility(m, &self.host).or_else(sandbox::imported_unsupported_reason);
        let versions = store::versions(&self.db, Some(&m.id)).await?;
        let already = versions
            .iter()
            .any(|v| v.version == m.version && v.revision >= m.revision);
        let id = uuid::Uuid::new_v4().to_string();
        let now = db::now_ms();
        let expires = now + INSPECTION_TTL_MS;
        sqlx::query("DELETE FROM package_inspections WHERE expires_at < ?")
            .bind(now)
            .execute(&self.db)
            .await?;
        sqlx::query(
            "INSERT INTO package_inspections (id, input_url, repo, release_tag, release_id, manifest_json,
                                              manifest_sha256, created_by, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.trim())
        .bind(&inspected.repo)
        .bind(&inspected.release.tag)
        .bind(inspected.release.id as i64)
        .bind(serde_json::to_string(m).unwrap_or_default())
        .bind(&inspected.manifest_sha256)
        .bind(user.id)
        .bind(now)
        .bind(expires)
        .execute(&self.db)
        .await?;
        Ok(Preview {
            inspection_id: id,
            repo_url: format!("https://github.com/{}", inspected.repo),
            repo: inspected.repo.clone(),
            artifact: m.artifact_for(&self.host.target).map(ArtifactView::from),
            compatible: reason.is_none(),
            reason,
            host_target: self.host.target.clone(),
            official: false,
            source_approved: approved.is_some(),
            source_approved_by: approved.map(|a| a.1),
            already_installed: already,
            manifest: inspected.manifest.clone(),
            manifest_sha256: inspected.manifest_sha256.clone(),
            checks: inspected.checks.clone(),
            release: inspected.release.clone(),
            expires_at: expires,
        })
    }

    pub async fn install_inspection(
        self: &Arc<Self>,
        user: &AuthUser,
        inspection_id: &str,
        approve_source: bool,
        make_default: Option<bool>,
    ) -> AppResult<String> {
        let row = sqlx::query(
            "SELECT repo, release_tag, manifest_json, manifest_sha256, expires_at FROM package_inspections WHERE id = ?",
        )
        .bind(inspection_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(AppError::NotFound("inspection not found; inspect the link again"))?;
        use sqlx::Row;
        let expires: i64 = row.get("expires_at");
        if expires < db::now_ms() {
            return Err(AppError::bad_request(
                "This preview expired; inspect the link again.",
            ));
        }
        let repo: String = row.get("repo");
        let tag: String = row.get("release_tag");
        let json_text: String = row.get("manifest_json");
        let manifest = manifest::parse(json_text.as_bytes()).map_err(AppError::bad_request)?;
        let manifest_sha256: String = row.get("manifest_sha256");
        let approved = store::approved_source(&self.db, &repo).await?;
        match &approved {
            None if !approve_source => {
                return Err(AppError::bad_request(format!(
                    "Approve {repo} as a package source to install from it."
                )));
            }
            Some((publisher, _, _)) if publisher != &manifest.publisher.id => {
                return Err(AppError::conflict(format!(
                    "{repo} was approved for publisher {publisher:?}, but this release declares {:?}.",
                    manifest.publisher.id
                )));
            }
            _ => {}
        }
        // `start_install` re-checks ownership of the id under the package
        // lock; the approval is recorded only once the install is queued.
        let publisher = manifest.publisher.id.clone();
        let label = format!("{} {}", manifest.id, manifest.version);
        let job = self
            .start_install(
                user,
                manifest,
                "github",
                Some(repo.clone()),
                Some(tag.clone()),
                make_default,
            )
            .await;
        if job.is_ok() && approved.is_none() {
            store::approve_source(&self.db, &repo, &publisher, &user.username).await?;
            audit::record_with_events(
                &self.db,
                &self.events,
                AuditLog::new("package.source_approve")
                    .actor(user)
                    .object("package_source", repo.clone())
                    .path(label)
                    .success()
                    .detail(format!(
                        "publisher={publisher} release={tag} manifest_sha256={manifest_sha256}"
                    )),
            )
            .await;
        }
        if job.is_ok() {
            sqlx::query("DELETE FROM package_inspections WHERE id = ?")
                .bind(inspection_id)
                .execute(&self.db)
                .await?;
        }
        job
    }
}

/// Delay before retrying a failed catalog check: 5, 10, 20, 40 … minutes
/// (the caller caps it at the check interval).
fn retry_delay(failures: u32) -> Duration {
    let minutes = 5u64.saturating_mul(1u64 << failures.saturating_sub(1).min(16));
    Duration::from_secs(minutes.saturating_mul(60))
}

/// Label of an install job (job title, history, audit action), derived from
/// what actually happens rather than which endpoint was called: `update` when
/// it supersedes an older installed version of the same channel, `reinstall`
/// for an installed version, otherwise `install` (side by side, e.g. Node.js 26
/// next to 24).
fn install_action<'a>(
    installed: impl IntoIterator<Item = (&'a str, String)>,
    version: &str,
    channel: &str,
) -> &'static str {
    let new = version::Version::parse(version);
    let mut supersedes = false;
    for (v, c) in installed {
        if v == version {
            return "reinstall";
        }
        if c == channel
            && matches!((&new, version::Version::parse(v)), (Some(n), Some(old)) if *n > old)
        {
            supersedes = true;
        }
    }
    if supersedes {
        "update"
    } else {
        "install"
    }
}

fn consumer_name(raw: &str) -> AppResult<String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(|c| c.is_control()) {
        return Err(AppError::bad_request(
            "consumer name must be 1-64 characters",
        ));
    }
    Ok(name.to_string())
}

fn detected_view(d: &Detected, channel: &str) -> DetectedView {
    DetectedView {
        version: d.version.clone(),
        channel: channel.to_string(),
        state: d.state.clone(),
        detail: if d.detail.is_empty() {
            "New upstream version detected; the catalog workflow has not verified it yet.".into()
        } else {
            d.detail.clone()
        },
        detected_at: d.detected_at.clone(),
        source: "catalog".into(),
        release_url: None,
    }
}

struct JobReporter {
    ctx: JobContext,
    handle: tokio::runtime::Handle,
}

impl Reporter for JobReporter {
    fn phase(&mut self, done: u64, total: u64, message: &str) {
        self.handle
            .block_on(self.ctx.progress(done, total, message));
    }
    fn cancel_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.ctx.cancel_flag()
    }
}

#[cfg(test)]
mod tests {
    use super::{install_action, retry_delay};

    fn installed(list: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        list.iter().map(|(v, c)| (*v, c.to_string())).collect()
    }

    #[test]
    fn failed_catalog_checks_retry_with_backoff() {
        let minutes = |n| retry_delay(n).as_secs() / 60;
        assert_eq!(
            [minutes(1), minutes(2), minutes(3), minutes(4)],
            [5, 10, 20, 40]
        );
        // Large failure counts neither overflow nor shrink the delay.
        assert!(retry_delay(u32::MAX) >= retry_delay(10));
    }

    #[test]
    fn install_jobs_are_labelled_by_what_they_do() {
        let node = installed(&[("24.22.1", "24"), ("24.22.2", "24")]);
        // Newer patch of an installed stream: an update.
        assert_eq!(install_action(node.clone(), "24.22.3", "24"), "update");
        // A new major next to the installed one: side by side, not an update.
        assert_eq!(install_action(node.clone(), "26.13.0", "26"), "install");
        // An older version of the same stream (rollback target): install.
        assert_eq!(install_action(node.clone(), "24.22.0", "24"), "install");
        assert_eq!(install_action(node, "24.22.2", "24"), "reinstall");
        assert_eq!(install_action(Vec::new(), "1.26.10", "1.26"), "install");
        let go = installed(&[("1.26.10", "1.26")]);
        assert_eq!(install_action(go.clone(), "1.26.11", "1.26"), "update");
        assert_eq!(install_action(go, "1.27.4", "1.27"), "install");
    }
}
