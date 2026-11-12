//! Linux desktop apps.
//!
//! An administrator installs a Linux app (`.deb`, AppImage or a portable
//! archive) from File Station. The Docker helper builds it into an image
//! (see [`crate::docker::linux`]); opening the app starts one container for
//! that person and RAST shows the app's streamed desktop in a window through
//! [`proxy`].
//!
//! Rules for running copies ("sessions"):
//! * one copy per person per app (opening it again shows the same copy);
//! * a copy only starts when the server has [`MIN_FREE_MEMORY_BYTES`] free;
//! * it stops [`EMPTY_GRACE`] after its last window closed, after
//!   [`IDLE_LIMIT`] without keyboard or mouse input, or when the app itself
//!   is closed;
//! * copies do not survive a restart of RAST (their addresses and passwords
//!   live only in memory), so leftovers are stopped at start-up.

pub mod proxy;
mod updates;

use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    audit::{self, AuditLog},
    auth::AuthUser,
    config::Config,
    db,
    docker::{
        linux::{self as wire, LinuxArgs, LinuxFormat, LinuxSource},
        protocol::{WorkerOp, WorkerRequest},
        DockerError, DockerManager,
    },
    error::{AppError, AppResult},
    events::EventBus,
    jobs::{JobContext, JobManager, JobSpec},
    storage::{safe_path::Access, Storage},
};

/// A copy only starts while the server has at least this much memory free.
pub const MIN_FREE_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// How long a copy keeps running after its last window closed (a page
/// reload reconnects within this time).
pub const EMPTY_GRACE: Duration = Duration::from_secs(60);
/// A copy without keyboard or mouse input for this long is stopped.
pub const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
/// A copy that is not ready this long after it was asked to start is given up.
const START_LIMIT: Duration = Duration::from_secs(150);
const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// Every this many sweeps the running containers are compared with Docker.
const RECONCILE_EVERY: u32 = 3;
/// Update sources are asked at most this often (an administrator can ask
/// at any time).
const CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const CHECK_TICK: Duration = Duration::from_secs(60 * 60);
/// Default memory limit of a new app, in MiB.
pub const DEFAULT_MEMORY_MB: u32 = 4096;
/// Default CPU limit: half the server's CPUs, at most this many.
pub const DEFAULT_MAX_CPUS: u32 = 6;

/// Admin event: the list of Linux apps changed.
pub const APPS_EVENT: &str = "linuxapps.changed";
/// Owner event: one of the person's running copies changed.
pub const SESSION_EVENT: &str = "linuxapps.session";

/// Linux apps are offered on Linux hosts only.
pub const SUPPORTED: bool = cfg!(target_os = "linux");

/// One person's running copy of an app.
pub struct Session {
    /// Public id used by the API (never part of the app's address).
    pub id: String,
    /// Secret part of the app's address: `/linux-apps/<key>/`.
    key: String,
    pub app_id: String,
    pub app_name: String,
    pub user_id: i64,
    /// Basic-auth credentials of the container's web server.
    authorization: String,
    state: Mutex<SessionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Starting,
    Ready,
    Ended,
}

#[derive(Debug, Clone)]
struct SessionState {
    status: Status,
    port: Option<u16>,
    viewers: usize,
    last_input: Instant,
    empty_since: Option<Instant>,
    created: Instant,
    started_at_ms: i64,
    reason: Option<&'static str>,
    error: Option<String>,
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Port and `Authorization` value for the proxy, while the copy runs.
    pub fn upstream(&self) -> Option<(u16, &str)> {
        let state = self.lock();
        match (state.status, state.port) {
            (Status::Starting | Status::Ready, Some(port)) => Some((port, &self.authorization)),
            _ => None,
        }
    }

    pub fn viewer_joined(&self) {
        let mut state = self.lock();
        state.viewers += 1;
        state.empty_since = None;
        state.last_input = Instant::now();
    }

    pub fn viewer_left(&self) {
        let mut state = self.lock();
        state.viewers = state.viewers.saturating_sub(1);
        if state.viewers == 0 {
            state.empty_since = Some(Instant::now());
        }
    }

    /// Keyboard, mouse or other input arrived from a window.
    pub fn input(&self) {
        self.lock().last_input = Instant::now();
    }

    fn json(&self) -> Value {
        let state = self.lock();
        let status = match state.status {
            Status::Starting => "starting",
            Status::Ready => "ready",
            Status::Ended => "ended",
        };
        json!({
            "id": self.id,
            "appId": self.app_id,
            "appName": self.app_name,
            "status": status,
            "url": (state.status == Status::Ready)
                .then(|| format!("{}{}/", wire::URL_PREFIX, self.key)),
            "reason": state.reason,
            "error": state.error,
            "startedAt": state.started_at_ms,
            "viewers": state.viewers,
        })
    }
}

/// Database row of one app (without the icon).
#[derive(Debug, Clone, sqlx::FromRow)]
struct AppRow {
    id: String,
    name: String,
    format: String,
    file_name: String,
    version: Option<String>,
    package: Option<String>,
    description: Option<String>,
    command: Option<String>,
    launch_guess: bool,
    icon_type: Option<String>,
    build: i64,
    status: String,
    error: Option<String>,
    job_id: Option<String>,
    update_kind: Option<String>,
    update_ref: Option<String>,
    file_sha1: Option<String>,
    latest_version: Option<String>,
    checked_at: Option<i64>,
    check_error: Option<String>,
    memory_mb: i64,
    cpus: i64,
    size_bytes: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

const APP_COLUMNS: &str = "id, name, format, file_name, version, package, description, command, \
    launch_guess, icon_type, build, status, error, job_id, update_kind, update_ref, file_sha1, \
    latest_version, checked_at, check_error, memory_mb, cpus, size_bytes, created_at, updated_at";

impl AppRow {
    fn format(&self) -> LinuxFormat {
        LinuxFormat::parse(&self.format).unwrap_or(LinuxFormat::Deb)
    }

    fn update_available(&self) -> bool {
        match (&self.latest_version, &self.version) {
            (Some(latest), Some(current)) => updates::is_newer(latest, current),
            (Some(latest), None) => latest == updates::NEWER_BUILD,
            _ => false,
        }
    }

    fn json(&self, running: usize) -> Value {
        let command: Value = self
            .command
            .as_deref()
            .and_then(|c| serde_json::from_str(c).ok())
            .unwrap_or(Value::Array(Vec::new()));
        json!({
            "id": self.id,
            "desktopId": format!("linux:{}", self.id),
            "name": self.name,
            "format": self.format,
            "fileName": self.file_name,
            "version": self.version,
            "package": self.package,
            "description": self.description,
            "command": command,
            "launchGuess": self.launch_guess,
            "iconUrl": self.icon_type.as_ref().map(|_| {
                format!("/api/linux-apps/{}/icon?v={}", self.id, self.updated_at)
            }),
            "status": self.status,
            "error": self.error,
            "jobId": self.job_id,
            "update": {
                "source": self.update_kind,
                "latestVersion": self.latest_version,
                "available": self.update_available(),
                "checkedAt": self.checked_at,
                "error": self.check_error,
            },
            "memoryMb": self.memory_mb,
            "cpus": self.cpus,
            "sizeBytes": self.size_bytes,
            "running": running,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

/// Linux apps: records, builds, updates and running copies.
pub struct LinuxApps {
    config: Arc<Config>,
    db: SqlitePool,
    docker: Arc<DockerManager>,
    events: EventBus,
    jobs: Arc<JobManager>,
    /// Running copies by their secret key.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl LinuxApps {
    pub fn new(
        config: Arc<Config>,
        db: SqlitePool,
        docker: Arc<DockerManager>,
        events: EventBus,
        jobs: Arc<JobManager>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            docker,
            events,
            jobs,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Background work: stop leftovers from before a restart, finish
    /// interrupted installs, then sweep copies and check for updates.
    pub async fn start(self: &Arc<Self>) {
        // Builds do not survive a restart.
        let _ = sqlx::query(
            "UPDATE linux_apps SET status = CASE WHEN build > 0 THEN 'ready' ELSE 'failed' END,
                 error = CASE WHEN build > 0 THEN error ELSE 'The install was interrupted by a restart of RAST.' END,
                 job_id = NULL
             WHERE status IN ('installing', 'updating')",
        )
        .execute(&self.db)
        .await;
        if !self.config.docker_enabled {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.stop_leftovers().await;
            let mut tick: u32 = 0;
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                tick = tick.wrapping_add(1);
                this.sweep(tick.is_multiple_of(RECONCILE_EVERY)).await;
            }
        });
        let this = self.clone();
        tokio::spawn(async move {
            // Let the server settle before the first round of checks.
            tokio::time::sleep(Duration::from_secs(120)).await;
            loop {
                this.check_due().await;
                tokio::time::sleep(CHECK_TICK).await;
            }
        });
    }

    async fn stop_leftovers(&self) {
        let Ok(list) = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxList))
            .await
        else {
            return;
        };
        for container in containers_of(&list) {
            let (Some(app), Some(user_id)) = (
                container.get("app").and_then(Value::as_str),
                container.get("userId").and_then(Value::as_i64),
            ) else {
                continue;
            };
            tracing::info!(
                app,
                user_id,
                "stopping a Linux app left from before the restart"
            );
            let _ = self.stop_container(app, user_id).await;
        }
    }

    // -----------------------------------------------------------------------
    // Records
    // -----------------------------------------------------------------------

    async fn row(&self, id: &str) -> AppResult<AppRow> {
        sqlx::query_as::<_, AppRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {APP_COLUMNS} FROM linux_apps WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(AppError::NotFound("Linux app not found"))
    }

    fn running_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for session in self.sessions().values() {
            if session.lock().status != Status::Ended {
                *counts.entry(session.app_id.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    pub async fn list(&self) -> AppResult<Vec<Value>> {
        let rows = sqlx::query_as::<_, AppRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {APP_COLUMNS} FROM linux_apps ORDER BY name COLLATE NOCASE, id"
        )))
        .fetch_all(&self.db)
        .await?;
        let counts = self.running_counts();
        Ok(rows
            .iter()
            .map(|row| row.json(counts.get(&row.id).copied().unwrap_or(0)))
            .collect())
    }

    pub async fn get(&self, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        let running = self.running_counts().get(id).copied().unwrap_or(0);
        Ok(row.json(running))
    }

    pub async fn icon(&self, id: &str) -> AppResult<(String, Vec<u8>)> {
        let row: Option<(Option<String>, Option<Vec<u8>>)> =
            sqlx::query_as("SELECT icon_type, icon FROM linux_apps WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.db)
                .await?;
        match row {
            Some((Some(kind), Some(icon))) => Ok((kind, icon)),
            _ => Err(AppError::NotFound("this app has no icon")),
        }
    }

    /// Linux apps need Docker on a Linux host (not the macOS preview).
    async fn ensure_available(&self) -> AppResult<()> {
        if !SUPPORTED {
            return Err(AppError::unavailable(
                "Linux apps run on Linux servers only",
            ));
        }
        self.docker.ensure_available().await
    }

    fn announce(&self, id: &str) {
        self.events.publish_admin(APPS_EVENT, json!({"id": id}));
    }

    async fn set_status(&self, id: &str, status: &str, error: Option<&str>, job: Option<&str>) {
        let _ = sqlx::query(
            "UPDATE linux_apps SET status = ?, error = ?, job_id = ?, updated_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(error)
        .bind(job)
        .bind(db::now_ms())
        .bind(id)
        .execute(&self.db)
        .await;
        self.announce(id);
    }

    /// Change an app's memory and CPU limits (they apply from its next start).
    pub async fn set_limits(
        &self,
        user: &AuthUser,
        id: &str,
        memory_mb: Option<u32>,
        cpus: Option<u32>,
    ) -> AppResult<Value> {
        let row = self.row(id).await?;
        let memory = memory_mb.unwrap_or(row.memory_mb as u32);
        let cpus = cpus.unwrap_or(row.cpus as u32);
        if !(wire::MIN_MEMORY_MB..=wire::MAX_MEMORY_MB).contains(&memory) {
            return Err(AppError::bad_request(format!(
                "memory must be between {} MB and {} MB",
                wire::MIN_MEMORY_MB,
                wire::MAX_MEMORY_MB
            )));
        }
        let max_cpus = host_cpus().max(1);
        if cpus < 1 || cpus > max_cpus {
            return Err(AppError::bad_request(format!(
                "CPUs must be between 1 and {max_cpus}"
            )));
        }
        sqlx::query("UPDATE linux_apps SET memory_mb = ?, cpus = ?, updated_at = ? WHERE id = ?")
            .bind(memory as i64)
            .bind(cpus as i64)
            .bind(db::now_ms())
            .bind(id)
            .execute(&self.db)
            .await?;
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("linuxapp.limits")
                .actor(user)
                .object("linux-app", id)
                .detail(format!("memory {memory} MB, {cpus} CPUs"))
                .success(),
        )
        .await;
        self.announce(id);
        self.get(id).await
    }

    // -----------------------------------------------------------------------
    // Install / update / uninstall
    // -----------------------------------------------------------------------

    /// Install the app file at `path` (a File Station path of `user`).
    pub async fn install(
        self: &Arc<Self>,
        user: &AuthUser,
        storage: &Storage,
        path: &str,
    ) -> AppResult<Value> {
        self.ensure_available().await?;
        crate::api::validate_logical_path(path)?;
        let resolved = storage.resolve(&self.db, user, path, Access::Read).await?;
        let file_name = resolved
            .physical
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| AppError::bad_request("choose a file"))?
            .to_string();
        let format = LinuxFormat::from_file_name(&file_name).ok_or_else(|| {
            AppError::bad_request("Choose a .deb package, an .AppImage or a .tar.gz archive.")
        })?;
        let meta = std::fs::symlink_metadata(&resolved.physical)
            .map_err(|_| AppError::NotFound("path not found"))?;
        if !meta.is_file() {
            return Err(AppError::bad_request("choose a file, not a folder"));
        }
        if meta.len() == 0 || meta.len() > wire::MAX_PACKAGE_BYTES {
            return Err(AppError::bad_request(
                "the file is empty or larger than 8 GB",
            ));
        }

        let id = self.new_id(&file_name).await?;
        let now = db::now_ms();
        let cpus = default_cpus();
        sqlx::query(
            "INSERT INTO linux_apps (id, name, format, file_name, version, status, memory_mb, cpus,
                 created_by, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'installing', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(display_name_from_file(&file_name))
        .bind(format.as_str())
        .bind(&file_name)
        .bind(version_from_file(&file_name))
        .bind(DEFAULT_MEMORY_MB as i64)
        .bind(cpus as i64)
        .bind(user.id)
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await?;

        let this = self.clone();
        let job_id_app = id.clone();
        let source = resolved.physical.clone();
        let actor = user.clone();
        let spec = JobSpec::new("linuxapps.install", move |ctx| async move {
            this.run_install(ctx, actor, job_id_app, source, format)
                .await
        })
        .actor(user)
        .message(format!("Installing {file_name}"));
        let job_id = self.jobs.submit(spec).await?;
        let _ = sqlx::query("UPDATE linux_apps SET job_id = ? WHERE id = ?")
            .bind(&job_id)
            .bind(&id)
            .execute(&self.db)
            .await;
        self.announce(&id);
        Ok(json!({"app": self.get(&id).await?, "jobId": job_id}))
    }

    async fn new_id(&self, file_name: &str) -> AppResult<String> {
        let base = id_from_file(file_name);
        for n in 1..1000 {
            let candidate = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM linux_apps WHERE id = ?")
                .bind(&candidate)
                .fetch_optional(&self.db)
                .await?;
            if taken.is_none() {
                return Ok(candidate);
            }
        }
        Err(AppError::conflict("too many apps with this name"))
    }

    fn staging_dir(&self, id: &str) -> PathBuf {
        self.config.linux_apps_dir().join(id)
    }

    async fn run_install(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        id: String,
        source: PathBuf,
        format: LinuxFormat,
    ) -> AppResult<Value> {
        ctx.progress(0, 3, "Copying the file").await;
        let staging = self.staging_dir(&id);
        let copied = {
            let staging = staging.clone();
            let cancel = ctx.cancel_flag();
            tokio::task::spawn_blocking(move || stage_file(&source, &staging, &cancel))
                .await
                .map_err(|e| AppError::internal(e.to_string()))
                .and_then(|r| r)
        };
        let sha1 = match copied {
            Ok(sha1) => sha1,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                self.install_failed(&actor, &id, &error).await;
                return Err(error);
            }
        };
        ctx.progress(
            1,
            3,
            "Installing (the first app also downloads the Linux desktop, about 1 GB)",
        )
        .await;
        let args = LinuxArgs {
            build: Some(1),
            format: Some(format),
            ..LinuxArgs::app(&id)
        };
        let outcome = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxBuild).linux(args))
            .await;
        let _ = std::fs::remove_dir_all(&staging);
        match outcome {
            Ok(data) => {
                self.apply_build(&id, 1, &data, Some(&sha1), None).await?;
                ctx.progress(3, 3, "Installed").await;
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.install")
                        .actor(&actor)
                        .object("linux-app", &id)
                        .success(),
                )
                .await;
                Ok(json!({"id": id}))
            }
            Err(error) => {
                let error = build_error(error);
                self.install_failed(&actor, &id, &error).await;
                Err(error)
            }
        }
    }

    async fn install_failed(&self, actor: &AuthUser, id: &str, error: &AppError) {
        let message = error.public_message();
        self.set_status(id, "failed", Some(&message), None).await;
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("linuxapp.install")
                .actor(actor)
                .object("linux-app", id)
                .failure()
                .detail(message),
        )
        .await;
    }

    /// Record what a successful build produced and make it current.
    async fn apply_build(
        &self,
        id: &str,
        build: u32,
        data: &Value,
        sha1: Option<&str>,
        file_name: Option<&str>,
    ) -> AppResult<()> {
        let row = self.row(id).await?;
        let meta = data.get("meta").cloned().unwrap_or(Value::Null);
        let text = |key: &str| meta.get(key).and_then(Value::as_str).map(str::to_string);
        let file_name = file_name.unwrap_or(&row.file_name).to_string();
        let name = text("name").unwrap_or_else(|| row.name.clone());
        let version = text("version")
            .or_else(|| version_from_file(&file_name))
            .or(row.version.clone());
        let (icon_type, icon) = decode_icon(meta.get("icon"));
        let (update_kind, update_ref) = match meta.get("update") {
            Some(update) if update.is_object() => (
                update
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some(update.to_string()),
            ),
            _ => (None, None),
        };
        let command = meta
            .get("exec")
            .filter(|v| v.is_array())
            .map(Value::to_string);
        sqlx::query(
            "UPDATE linux_apps SET name = ?, version = ?, package = ?, description = ?, command = ?,
                 launch_guess = ?, icon_type = COALESCE(?, icon_type), icon = COALESCE(?, icon),
                 build = ?, status = 'ready', error = NULL, job_id = NULL,
                 update_kind = ?, update_ref = ?, file_sha1 = COALESCE(?, file_sha1),
                 file_name = ?, latest_version = NULL, check_error = NULL,
                 size_bytes = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(name)
        .bind(version)
        .bind(text("package"))
        .bind(text("description"))
        .bind(command)
        .bind(meta.get("launchGuess").and_then(Value::as_bool).unwrap_or(false))
        .bind(icon_type)
        .bind(icon)
        .bind(build as i64)
        .bind(update_kind)
        .bind(update_ref)
        .bind(sha1)
        .bind(file_name)
        .bind(data.get("sizeBytes").and_then(Value::as_i64))
        .bind(db::now_ms())
        .bind(id)
        .execute(&self.db)
        .await?;
        self.announce(id);
        Ok(())
    }

    /// Ask the app's official source whether a newer version exists.
    pub async fn check_now(self: &Arc<Self>, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        if row.status != "ready" {
            return Err(AppError::conflict("wait until the app is installed"));
        }
        self.check(&row).await;
        self.get(id).await
    }

    async fn check(&self, row: &AppRow) {
        let outcome: Result<Option<String>, String> = match row.update_kind.as_deref() {
            Some("apt") => match &row.package {
                Some(package) => {
                    let args = LinuxArgs {
                        build: Some(row.build as u32),
                        package: Some(package.clone()),
                        ..LinuxArgs::app(&row.id)
                    };
                    self.docker
                        .call(WorkerRequest::new(WorkerOp::LinuxCheck).linux(args))
                        .await
                        .map(|data| {
                            data.get("candidate")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .map_err(|e| docker_message(&e))
                }
                None => Err("the package name is unknown".into()),
            },
            Some(_) => {
                let update = row
                    .update_ref
                    .as_deref()
                    .and_then(|r| serde_json::from_str::<Value>(r).ok())
                    .unwrap_or(Value::Null);
                let installed_sha1 = row.file_sha1.clone();
                let installed = row.version.clone();
                tokio::task::spawn_blocking(move || {
                    updates::latest(&update, installed_sha1.as_deref())
                        .map(|latest| latest.version_label(installed.as_deref()))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
            None => return,
        };
        let (latest, error) = match outcome {
            Ok(latest) => (latest, None),
            Err(error) => (row.latest_version.clone(), Some(error)),
        };
        let _ = sqlx::query(
            "UPDATE linux_apps SET latest_version = ?, checked_at = ?, check_error = ? WHERE id = ?",
        )
        .bind(latest)
        .bind(db::now_ms())
        .bind(error)
        .bind(&row.id)
        .execute(&self.db)
        .await;
        self.announce(&row.id);
    }

    async fn check_due(&self) {
        let due = db::now_ms() - CHECK_INTERVAL_MS;
        let rows = sqlx::query_as::<_, AppRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {APP_COLUMNS} FROM linux_apps
             WHERE status = 'ready' AND update_kind IS NOT NULL
               AND (checked_at IS NULL OR checked_at < ?)"
        )))
        .bind(due)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for row in rows {
            self.check(&row).await;
        }
    }

    /// Install the newest version from the app's official source.
    pub async fn update(self: &Arc<Self>, user: &AuthUser, id: &str) -> AppResult<Value> {
        self.ensure_available().await?;
        let row = self.row(id).await?;
        if row.status != "ready" {
            return Err(AppError::conflict(
                "wait until the current install or update finishes",
            ));
        }
        if row.update_kind.is_none() {
            return Err(AppError::bad_request(
                "this app has no known update source; install the new version from its file",
            ));
        }
        let this = self.clone();
        let actor = user.clone();
        let app_id = id.to_string();
        let spec = JobSpec::new("linuxapps.update", move |ctx| async move {
            this.run_update(ctx, actor, app_id).await
        })
        .actor(user)
        .message(format!("Updating {}", row.name));
        sqlx::query("UPDATE linux_apps SET status = 'updating', error = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        let job_id = self.jobs.submit(spec).await?;
        let _ = sqlx::query("UPDATE linux_apps SET job_id = ? WHERE id = ?")
            .bind(&job_id)
            .bind(id)
            .execute(&self.db)
            .await;
        self.announce(id);
        Ok(json!({"jobId": job_id}))
    }

    async fn run_update(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        id: String,
    ) -> AppResult<Value> {
        let row = self.row(&id).await?;
        let build = row.build as u32 + 1;
        let outcome = match row.update_kind.as_deref() {
            Some("apt") => {
                ctx.progress(0, 2, "Downloading and installing the newest version")
                    .await;
                let args = LinuxArgs {
                    build: Some(build),
                    format: Some(LinuxFormat::Deb),
                    source: Some(LinuxSource::Apt),
                    from_build: Some(row.build as u32),
                    package: row.package.clone(),
                    ..LinuxArgs::app(&id)
                };
                self.docker
                    .call(WorkerRequest::new(WorkerOp::LinuxBuild).linux(args))
                    .await
                    .map(|data| (data, None, None))
                    .map_err(build_error)
            }
            _ => self.download_and_build(&ctx, &row, build).await,
        };
        let _ = std::fs::remove_dir_all(self.staging_dir(&id));
        let entry = AuditLog::new("linuxapp.update")
            .actor(&actor)
            .object("linux-app", &id);
        match outcome {
            Ok((data, sha1, file_name)) => {
                self.apply_build(&id, build, &data, sha1.as_deref(), file_name.as_deref())
                    .await?;
                // Older images go once no running copy uses them.
                self.prune(&id).await;
                ctx.progress(2, 2, "Updated").await;
                audit::record_with_events(&self.db, &self.events, entry.success()).await;
                Ok(json!({"id": id, "build": build}))
            }
            Err(error) => {
                let message = error.public_message();
                self.set_status(
                    &id,
                    "ready",
                    Some(&format!("Update failed: {message}")),
                    None,
                )
                .await;
                audit::record_with_events(&self.db, &self.events, entry.failure().detail(message))
                    .await;
                Err(error)
            }
        }
    }

    async fn download_and_build(
        &self,
        ctx: &JobContext,
        row: &AppRow,
        build: u32,
    ) -> AppResult<(Value, Option<String>, Option<String>)> {
        ctx.progress(0, 2, "Downloading the newest version").await;
        let update = row
            .update_ref
            .as_deref()
            .and_then(|r| serde_json::from_str::<Value>(r).ok())
            .unwrap_or(Value::Null);
        let staging = self.staging_dir(&row.id);
        let cancel = ctx.cancel_flag();
        let installed_sha1 = row.file_sha1.clone();
        let (file_name, sha1) = tokio::task::spawn_blocking(move || {
            let latest = updates::latest(&update, installed_sha1.as_deref())?;
            std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
            let sha1 = updates::download(&latest, &staging.join("package"), &cancel)?;
            Ok::<_, String>((latest.file_name, sha1))
        })
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .map_err(AppError::unavailable)?;
        ctx.progress(1, 2, "Installing the newest version").await;
        let args = LinuxArgs {
            build: Some(build),
            format: Some(row.format()),
            ..LinuxArgs::app(&row.id)
        };
        let data = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxBuild).linux(args))
            .await
            .map_err(build_error)?;
        Ok((data, Some(sha1), Some(file_name)))
    }

    async fn prune(&self, id: &str) {
        let Ok(row) = self.row(id).await else {
            return;
        };
        if row.build < 1 {
            return;
        }
        let args = LinuxArgs {
            build: Some(row.build as u32),
            ..LinuxArgs::app(id)
        };
        let _ = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxPrune).linux(args))
            .await;
    }

    /// Remove an app: stop every copy, remove its images and, when asked,
    /// everyone's settings for it.
    pub async fn uninstall(
        &self,
        user: &AuthUser,
        id: &str,
        remove_data: bool,
    ) -> AppResult<Value> {
        let row = self.row(id).await?;
        if matches!(row.status.as_str(), "installing" | "updating") {
            return Err(AppError::conflict(
                "wait until the install or update finishes",
            ));
        }
        let sessions: Vec<Arc<Session>> = self
            .sessions()
            .values()
            .filter(|s| s.app_id == id)
            .cloned()
            .collect();
        for session in sessions {
            self.end(&session, "removed", None);
        }
        let mut removed = Value::Null;
        if self.config.docker_enabled {
            let args = LinuxArgs {
                remove_data: Some(remove_data),
                ..LinuxArgs::app(id)
            };
            removed = self
                .docker
                .call(WorkerRequest::new(WorkerOp::LinuxRemove).linux(args))
                .await?;
        }
        sqlx::query("DELETE FROM linux_apps WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        let _ = std::fs::remove_dir_all(self.staging_dir(id));
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("linuxapp.uninstall")
                .actor(user)
                .object("linux-app", id)
                .detail(if remove_data {
                    "with app data"
                } else {
                    "app data kept"
                })
                .success(),
        )
        .await;
        self.announce(id);
        Ok(json!({"ok": true, "removed": removed}))
    }

    // -----------------------------------------------------------------------
    // Running copies
    // -----------------------------------------------------------------------

    pub fn find_by_key(&self, key: &str) -> Option<Arc<Session>> {
        self.sessions().get(key).cloned()
    }

    fn find_by_id(&self, user: &AuthUser, session_id: &str) -> AppResult<Arc<Session>> {
        self.sessions()
            .values()
            .find(|s| s.id == session_id && s.user_id == user.id)
            .cloned()
            .ok_or(AppError::NotFound("this app is not running"))
    }

    pub fn session(&self, user: &AuthUser, session_id: &str) -> AppResult<Value> {
        Ok(self.find_by_id(user, session_id)?.json())
    }

    pub fn my_sessions(&self, user: &AuthUser) -> Vec<Value> {
        self.sessions()
            .values()
            .filter(|s| s.user_id == user.id)
            .map(|s| s.json())
            .collect()
    }

    /// Open an app for `user`: their running copy, or a new one.
    pub async fn open(self: &Arc<Self>, user: &AuthUser, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        if row.build < 1 || !matches!(row.status.as_str(), "ready" | "updating") {
            return Err(AppError::conflict("this app is not installed yet"));
        }
        if let Some(existing) = self
            .sessions()
            .values()
            .find(|s| s.app_id == id && s.user_id == user.id && s.lock().status != Status::Ended)
            .cloned()
        {
            return Ok(existing.json());
        }
        let available = available_memory();
        if available < MIN_FREE_MEMORY_BYTES {
            return Err(AppError::unavailable(format!(
                "The server has {} free and needs {} free to open another app. Close an app and try again.",
                gib(available),
                gib(MIN_FREE_MEMORY_BYTES)
            )));
        }
        self.ensure_available().await?;

        let key = random_hex(16);
        let password = random_hex(24);
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("rast:{password}"))
        );
        let now = Instant::now();
        let session = Arc::new(Session {
            id: uuid::Uuid::new_v4().to_string(),
            key: key.clone(),
            app_id: id.to_string(),
            app_name: row.name.clone(),
            user_id: user.id,
            authorization,
            state: Mutex::new(SessionState {
                status: Status::Starting,
                port: None,
                viewers: 0,
                last_input: now,
                empty_since: None,
                created: now,
                started_at_ms: db::now_ms(),
                reason: None,
                error: None,
            }),
        });
        {
            let mut sessions = self.sessions();
            // Another request for the same copy may have won the race.
            if let Some(existing) = sessions.values().find(|s| {
                s.app_id == id && s.user_id == user.id && s.lock().status != Status::Ended
            }) {
                return Ok(existing.json());
            }
            sessions.insert(key.clone(), session.clone());
        }
        let args = LinuxArgs {
            build: Some(row.build as u32),
            user_id: Some(user.id),
            username: Some(user.username.to_ascii_lowercase()),
            memory_mb: Some(row.memory_mb as u32),
            cpus: Some((row.cpus as u32).min(host_cpus().max(1))),
            password: Some(password),
            key: Some(key),
            title: Some(row.name.clone()),
            ..LinuxArgs::app(id)
        };
        let this = self.clone();
        let starting = session.clone();
        tokio::spawn(async move { this.start_copy(starting, args).await });
        Ok(session.json())
    }

    async fn start_copy(self: Arc<Self>, session: Arc<Session>, args: LinuxArgs) {
        let started = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxRun).linux(args))
            .await;
        let port = match started {
            Ok(data) => data
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok()),
            Err(error) => {
                self.end(&session, "error", Some(docker_message(&error)));
                return;
            }
        };
        let Some(port) = port else {
            self.end(
                &session,
                "error",
                Some("the app did not report its address".into()),
            );
            return;
        };
        let ended = {
            let mut state = session.lock();
            state.port = Some(port);
            state.status == Status::Ended
        };
        if ended {
            // Closed while it was starting.
            let _ = self.stop_container(&session.app_id, session.user_id).await;
            return;
        }
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if session.lock().status == Status::Ended {
                return;
            }
            if probe(port, &session.key, &session.authorization).await {
                break;
            }
            if Instant::now() > deadline {
                self.end(
                    &session,
                    "error",
                    Some("the app did not start in time".into()),
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        {
            let mut state = session.lock();
            if state.status != Status::Starting {
                return;
            }
            state.status = Status::Ready;
            state.last_input = Instant::now();
            if state.viewers == 0 {
                state.empty_since = Some(Instant::now());
            }
        }
        self.publish_session(&session);
        self.announce(&session.app_id);
    }

    fn publish_session(&self, session: &Session) {
        self.events
            .publish_user(SESSION_EVENT, session.user_id, session.json());
    }

    /// Close one of the person's copies now.
    pub fn close(self: &Arc<Self>, user: &AuthUser, session_id: &str) -> AppResult<Value> {
        let session = self.find_by_id(user, session_id)?;
        self.end(&session, "closed", None);
        Ok(json!({"ok": true}))
    }

    /// Mark a copy ended, forget its address and stop its container in the
    /// background.
    fn end(&self, session: &Arc<Session>, reason: &'static str, error: Option<String>) {
        {
            let mut state = session.lock();
            if state.status == Status::Ended {
                return;
            }
            state.status = Status::Ended;
            state.reason = Some(reason);
            state.error = error;
        }
        self.publish_session(session);
        self.announce(&session.app_id);
        let docker = self.docker.clone();
        let db = self.db.clone();
        let app = session.app_id.clone();
        let user_id = session.user_id;
        tokio::spawn(async move {
            let args = LinuxArgs {
                user_id: Some(user_id),
                ..LinuxArgs::app(&app)
            };
            if let Err(error) = docker
                .call(WorkerRequest::new(WorkerOp::LinuxStop).linux(args))
                .await
            {
                tracing::warn!(app = %app, user_id, "could not stop a Linux app: {error}");
            }
            // Images replaced by an update can go once their last copy stops.
            let build: Option<(i64,)> = sqlx::query_as("SELECT build FROM linux_apps WHERE id = ?")
                .bind(&app)
                .fetch_optional(&db)
                .await
                .ok()
                .flatten();
            if let Some((build,)) = build.filter(|(b,)| *b > 0) {
                let args = LinuxArgs {
                    build: Some(build as u32),
                    ..LinuxArgs::app(&app)
                };
                let _ = docker
                    .call(WorkerRequest::new(WorkerOp::LinuxPrune).linux(args))
                    .await;
            }
        });
    }

    async fn stop_container(&self, app: &str, user_id: i64) -> Result<Value, DockerError> {
        let args = LinuxArgs {
            user_id: Some(user_id),
            ..LinuxArgs::app(app)
        };
        self.docker
            .call(WorkerRequest::new(WorkerOp::LinuxStop).linux(args))
            .await
    }

    /// Apply the stop rules; with `reconcile`, also notice copies whose
    /// container stopped on its own (the app was closed, or ran out of memory).
    async fn sweep(self: &Arc<Self>, reconcile: bool) {
        let now = Instant::now();
        let mut to_end: Vec<(Arc<Session>, &'static str, Option<String>)> = Vec::new();
        let mut forget = Vec::new();
        for (key, session) in self.sessions().iter() {
            let state = session.lock();
            match state.status {
                Status::Ended => {
                    // Ended copies are kept briefly so their window can show why.
                    if now.duration_since(state.created) > Duration::from_secs(5)
                        && state.viewers == 0
                    {
                        forget.push(key.clone());
                    }
                }
                Status::Starting => {
                    if now.duration_since(state.created) > START_LIMIT + Duration::from_secs(30) {
                        to_end.push((
                            session.clone(),
                            "error",
                            Some("the app did not start".into()),
                        ));
                    }
                }
                Status::Ready => {
                    if state
                        .empty_since
                        .is_some_and(|since| now.duration_since(since) >= EMPTY_GRACE)
                    {
                        to_end.push((session.clone(), "closed", None));
                    } else if now.duration_since(state.last_input) >= IDLE_LIMIT {
                        to_end.push((session.clone(), "idle", None));
                    }
                }
            }
        }
        for (session, reason, error) in to_end {
            self.end(&session, reason, error);
        }
        if reconcile {
            self.reconcile().await;
        }
        let mut sessions = self.sessions();
        for key in forget {
            let ended_long_ago = sessions
                .get(&key)
                .map(|s| s.lock().status == Status::Ended)
                .unwrap_or(false);
            if ended_long_ago {
                sessions.remove(&key);
            }
        }
    }

    async fn reconcile(&self) {
        let ready: Vec<Arc<Session>> = self
            .sessions()
            .values()
            .filter(|s| {
                let state = s.lock();
                state.status == Status::Ready
                    && Instant::now().duration_since(state.created) > Duration::from_secs(20)
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            return;
        }
        let Ok(list) = self
            .docker
            .call(WorkerRequest::new(WorkerOp::LinuxList))
            .await
        else {
            return;
        };
        let running: Vec<(String, i64)> = containers_of(&list)
            .iter()
            .filter(|c| c.get("running").and_then(Value::as_bool) == Some(true))
            .filter_map(|c| {
                Some((
                    c.get("app")?.as_str()?.to_string(),
                    c.get("userId")?.as_i64()?,
                ))
            })
            .collect();
        for session in ready {
            if !running
                .iter()
                .any(|(app, user)| *app == session.app_id && *user == session.user_id)
            {
                self.end(&session, "exited", None);
            }
        }
    }
}

fn containers_of(list: &Value) -> Vec<Value> {
    list.get("containers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Is the app's page answering yet?
async fn probe(port: u16, key: &str, authorization: &str) -> bool {
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        let request = format!(
            "GET {}{key}/ HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {authorization}\r\n\r\n",
            wire::URL_PREFIX
        );
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut head = [0u8; 16];
        let n = stream.read(&mut head).await.ok()?;
        let line = String::from_utf8_lossy(&head[..n]).to_string();
        Some(line.starts_with("HTTP/1.1 200") || line.starts_with("HTTP/1.0 200"))
    };
    tokio::time::timeout(Duration::from_secs(3), attempt)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

fn build_error(error: DockerError) -> AppError {
    AppError::unavailable(docker_message(&error))
}

fn docker_message(error: &DockerError) -> String {
    match error {
        DockerError::Failed(message) | DockerError::BadRequest(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Copy the chosen file into the staging folder the Docker helper builds
/// from; returns its SHA-1 (AppImage update checks compare it).
fn stage_file(
    source: &Path,
    staging: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> AppResult<String> {
    use sha1::Digest;
    std::fs::create_dir_all(staging)
        .map_err(|e| AppError::internal(format!("cannot prepare the app folder: {e}")))?;
    let mut input = crate::storage::safe_path::open_read_no_follow(source)
        .map_err(|_| AppError::NotFound("path not found"))?;
    let target = staging.join("package");
    let partial = staging.join("package.partial");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&partial)
        .map_err(|e| AppError::internal(format!("cannot copy the app file: {e}")))?;
    let mut hasher = sha1::Sha1::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total: u64 = 0;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
        let n = input
            .read(&mut buffer)
            .map_err(|e| AppError::internal(format!("cannot read the app file: {e}")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > wire::MAX_PACKAGE_BYTES {
            return Err(AppError::bad_request("the file is larger than 8 GB"));
        }
        hasher.update(&buffer[..n]);
        output
            .write_all(&buffer[..n])
            .map_err(|e| AppError::internal(format!("cannot copy the app file: {e}")))?;
    }
    output
        .sync_all()
        .map_err(|e| AppError::internal(format!("cannot copy the app file: {e}")))?;
    std::fs::rename(&partial, &target)
        .map_err(|e| AppError::internal(format!("cannot copy the app file: {e}")))?;
    Ok(hex::encode(hasher.finalize()))
}

fn decode_icon(icon: Option<&Value>) -> (Option<String>, Option<Vec<u8>>) {
    let Some(icon) = icon.filter(|i| i.is_object()) else {
        return (None, None);
    };
    let kind = icon.get("type").and_then(Value::as_str);
    let data = icon
        .get("data")
        .and_then(Value::as_str)
        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok());
    match (kind, data) {
        (Some("png"), Some(bytes)) if bytes.starts_with(b"\x89PNG") => {
            (Some("png".into()), Some(bytes))
        }
        (Some("svg"), Some(bytes))
            if String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).contains("<svg") =>
        {
            (Some("svg".into()), Some(bytes))
        }
        _ => (None, None),
    }
}

/// Words of a file name that say nothing about the app.
const NOISE_WORDS: &[&str] = &[
    "amd64", "x86", "x64", "64", "x86_64", "arm64", "aarch64", "armhf", "i386", "linux", "current",
    "latest", "stable", "appimage", "portable", "setup", "release", "deb", "tar", "gz", "xz",
    "bz2", "tgz", "txz", "all", "bin",
];

fn file_words(file_name: &str) -> Vec<String> {
    let lower = file_name.to_ascii_lowercase();
    let stem = [
        ".tar.gz",
        ".tar.xz",
        ".tar.bz2",
        ".tgz",
        ".txz",
        ".tbz2",
        ".tar",
        ".deb",
        ".appimage",
    ]
    .iter()
    .find_map(|ext| lower.strip_suffix(ext))
    .unwrap_or(&lower)
    .to_string();
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// `google-chrome-stable_current_amd64.deb` → `google-chrome`: the words
/// before the version, without architecture or channel words.
pub fn id_from_file(file_name: &str) -> String {
    let mut words = Vec::new();
    for word in file_words(file_name) {
        let versionish = word.starts_with(|c: char| c.is_ascii_digit())
            || (word.starts_with('v') && word[1..].starts_with(|c: char| c.is_ascii_digit()));
        if versionish {
            break;
        }
        if !NOISE_WORDS.contains(&word.as_str()) {
            words.push(word);
        }
    }
    let mut id = String::new();
    for word in words {
        if id.len() + word.len() + 1 > 30 {
            break;
        }
        if !id.is_empty() {
            id.push('-');
        }
        id.push_str(&word);
    }
    if id.is_empty() {
        "app".to_string()
    } else {
        id
    }
}

/// A readable name until the app says its own.
fn display_name_from_file(file_name: &str) -> String {
    id_from_file(file_name)
        .split('-')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `Obsidian-1.9.12.AppImage` → `1.9.12`.
pub fn version_from_file(file_name: &str) -> Option<String> {
    let lower = file_name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let starts = bytes[i].is_ascii_digit()
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'v');
        if starts {
            let end = lower[i..]
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(lower.len(), |n| i + n);
            let candidate = lower[i..end].trim_end_matches('.');
            if candidate.contains('.') {
                return Some(candidate.to_string());
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    hex::encode(buffer)
}

fn host_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2)
}

/// Half the server's CPUs, at most [`DEFAULT_MAX_CPUS`].
pub fn default_cpus() -> u32 {
    (host_cpus() / 2).clamp(1, DEFAULT_MAX_CPUS)
}

fn available_memory() -> u64 {
    match sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
    ) {
        Ok(system) => system.available_memory(),
        // Unknown: do not block opening apps on a sampler problem.
        Err(_) => u64::MAX,
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names_come_from_the_file_name() {
        assert_eq!(
            id_from_file("google-chrome-stable_current_amd64.deb"),
            "google-chrome"
        );
        assert_eq!(id_from_file("Obsidian-1.9.12.AppImage"), "obsidian");
        assert_eq!(id_from_file("code_1.105.0-1759933565_amd64.deb"), "code");
        assert_eq!(id_from_file("firefox-140.0.tar.xz"), "firefox");
        assert_eq!(id_from_file("1234.deb"), "app");
        assert_eq!(
            display_name_from_file("google-chrome-stable_current_amd64.deb"),
            "Google Chrome"
        );
        assert_eq!(
            version_from_file("Obsidian-1.9.12.AppImage").as_deref(),
            Some("1.9.12")
        );
        assert_eq!(
            version_from_file("firefox-140.0.tar.xz").as_deref(),
            Some("140.0")
        );
        assert_eq!(
            version_from_file("google-chrome-stable_current_amd64.deb"),
            None
        );
        assert_eq!(
            version_from_file("app-v2.3.1-x86_64.AppImage").as_deref(),
            Some("2.3.1")
        );
    }

    #[test]
    fn icons_must_be_real_images() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nrest");
        assert_eq!(
            decode_icon(Some(&json!({"type": "png", "data": png})))
                .0
                .as_deref(),
            Some("png")
        );
        let fake = base64::engine::general_purpose::STANDARD.encode(b"<html>");
        assert_eq!(
            decode_icon(Some(&json!({"type": "png", "data": fake}))).0,
            None
        );
        assert_eq!(
            decode_icon(Some(&json!({"type": "svg", "data": fake}))).0,
            None
        );
    }

    #[test]
    fn default_limits_fit_the_server() {
        let cpus = default_cpus();
        assert!((1..=DEFAULT_MAX_CPUS).contains(&cpus));
    }
}
