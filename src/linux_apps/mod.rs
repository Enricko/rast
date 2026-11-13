//! Linux desktop apps.
//!
//! An administrator installs a Linux app (`.deb`, AppImage or a portable
//! archive) from File Station. The Docker helper builds it into an image
//! (see [`crate::docker::linux`]); opening the app starts one container for
//! that person and RAST shows the app's streamed desktop in a window through
//! [`proxy`].
//!
//! An installed app is visible to every signed-in user unless its
//! [`Visibility`] is restricted to administrators; everyone who can see an app
//! opens their own copy. Installing, updating, removing, limiting and changing
//! an app's visibility are administrator actions.
//!
//! Rules for running copies ("sessions"):
//! * one copy per person per app (opening it again shows the same copy);
//! * a copy only starts when the server has [`MIN_FREE_MEMORY_BYTES`] free;
//! * it stops [`EMPTY_GRACE`] after its last window closed, after
//!   [`IDLE_LIMIT`] without keyboard or mouse input, or when the app itself
//!   is closed;
//! * copies do not survive a restart of RAST (their addresses and passwords
//!   live only in memory), so leftovers are stopped at start-up.

pub mod native_apps;
pub mod native_archive;
pub mod native_proxy;
pub mod native_session;
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

use self::native_apps::{NativeApps, StagedFile};
use self::native_session::{LaunchRequest as NativeLaunchRequest, NativeSessions};

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
/// After an app's stream goes away, how long to keep checking whether the app
/// itself exited (its own close button) before ending the session early. The
/// container needs a few seconds to stop, and a reload reconnects within the
/// first checks.
const EXIT_CHECK_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(3),
];
const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// Every this many sweeps the running containers are compared with Docker.
const RECONCILE_EVERY: u32 = 3;
/// Update sources are asked at most this often (an administrator can ask
/// at any time).
/// Ordinary flags for native Chromium apps (the sandbox is never disabled).
/// `--hide-crash-restore-bubble` matters: after a session restart Chromium
/// otherwise draws a "Restore pages?" bubble over the top-right of the window,
/// which looks like an invisible border and swallows clicks on the profile and
/// menu buttons underneath it.
const CHROMIUM_LAUNCH_FLAGS: [&str; 4] = [
    "--no-first-run",
    "--no-default-browser-check",
    "--password-store=basic",
    "--hide-crash-restore-bubble",
];

const CHECK_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const CHECK_TICK: Duration = Duration::from_secs(60 * 60);
/// Default memory limit of a new app, in MiB.
pub const DEFAULT_MEMORY_MB: u32 = 4096;
/// Default CPU limit: half the server's CPUs, at most this many.
pub const DEFAULT_MAX_CPUS: u32 = 6;

/// Event: the list of Linux apps changed (sent to every client; each client
/// reloads the list it may see).
pub const APPS_EVENT: &str = "linuxapps.changed";
/// Owner event: one of the person's running copies changed.
pub const SESSION_EVENT: &str = "linuxapps.session";

/// Linux apps are offered on Linux hosts only.
pub const SUPPORTED: bool = cfg!(target_os = "linux");

/// Who can see and open an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Every signed-in user (the default).
    Users,
    /// Administrators only.
    Admins,
}

impl Visibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Admins => "admins",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "users" => Some(Self::Users),
            "admins" => Some(Self::Admins),
            _ => None,
        }
    }
}

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
    /// Host port of the container's window control service, when the image
    /// has one (per-window streaming).
    control_port: Option<u16>,
    /// Host ports of the per-window VNC servers, by slot.
    vnc_ports: Vec<u16>,
    /// Window id -> stream (host port and its VNC password).
    window_streams: HashMap<String, WindowStream>,
    /// The image streams each app window separately (per-window streaming).
    separate_windows: bool,
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

    /// The app's window control service, when the image provides one.
    pub fn control_port(&self) -> Option<u16> {
        self.lock().control_port
    }

    /// The VNC stream of one of the app's windows.
    fn window_stream(&self, window: &str) -> Option<WindowStream> {
        self.lock().window_streams.get(window).cloned()
    }

    /// The app page's Basic credentials, also used for the window service.
    fn authorization(&self) -> String {
        self.authorization.clone()
    }

    /// Host port of a VNC server slot the window control service reported.
    pub fn vnc_host_port(&self, slot: usize) -> Option<u16> {
        self.lock().vnc_ports.get(slot).copied()
    }

    pub fn remember_window_stream(&self, window: &str, port: u16, password: String) {
        self.lock()
            .window_streams
            .insert(window.to_string(), WindowStream { port, password });
    }

    pub fn forget_window_stream(&self, window: &str) {
        self.lock().window_streams.remove(window);
    }

    /// The secret part of the app's address.
    pub(crate) fn key(&self) -> &str {
        &self.key
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
            // The image can stream each app window on its own.
            "separateWindows": state.separate_windows,
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
    /// `users` or `admins`; see [`Visibility`].
    visibility: String,
    /// `docker` (legacy images; kept until migrated) or `native` (host
    /// execution in the person's shared graphical session).
    backend: String,
    install_path: Option<String>,
    previous_path: Option<String>,
    launcher: Option<String>,
    architecture: Option<String>,
    package_version: Option<String>,
    package_preexisting: Option<i64>,
    dependencies: Option<String>,
    packages_added: Option<String>,
    compatibility: Option<String>,
    install_source: Option<String>,
    migration_state: Option<String>,
    migration_error: Option<String>,
    migration_backup: Option<String>,
    migrated_at: Option<i64>,
    docker_volume: Option<String>,
    created_at: i64,
    updated_at: i64,
}

const APP_COLUMNS: &str = "id, name, format, file_name, version, package, description, command, \
    launch_guess, icon_type, build, status, error, job_id, update_kind, update_ref, file_sha1, \
    latest_version, checked_at, check_error, memory_mb, cpus, size_bytes, visibility, backend, \
    install_path, previous_path, launcher, architecture, package_version, package_preexisting, \
    dependencies, packages_added, compatibility, install_source, migration_state, \
    migration_error, migration_backup, migrated_at, docker_volume, created_at, updated_at";

impl AppRow {
    fn format(&self) -> LinuxFormat {
        LinuxFormat::parse(&self.format).unwrap_or(LinuxFormat::Deb)
    }

    /// The app's visibility; anything unexpected is treated as everyone.
    fn visibility(&self) -> Visibility {
        Visibility::parse(&self.visibility).unwrap_or(Visibility::Users)
    }

    /// Can `user` see this app? Administrators see everything; everyone else
    /// sees apps shared with all users once they have a finished build.
    fn visible_to(&self, user: &AuthUser) -> bool {
        user.is_admin()
            || (self.visibility() == Visibility::Users
                // Docker builds count with `build`; native apps are ready
                // without a build number.
                && (self.build > 0 || self.is_native())
                && matches!(self.status.as_str(), "ready" | "updating"))
    }

    /// The execution backend: `native`, or `docker` for legacy apps.
    fn is_native(&self) -> bool {
        self.backend == "native"
    }

    /// A JSON value from one of the optional JSON columns.
    fn json_column(value: &Option<String>) -> Value {
        value
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null)
    }

    /// Native install/migration details the App Center shows.
    fn native_json(&self) -> Value {
        if !self.is_native() {
            return Value::Null;
        }
        json!({
            "installPath": self.install_path,
            "previousPath": self.previous_path,
            "launcher": Self::json_column(&self.launcher),
            "architecture": self.architecture,
            "packageVersion": self.package_version,
            "preexistingPackage": self.package_preexisting.map(|v| v != 0),
            "dependencies": Self::json_column(&self.dependencies),
            "packagesAdded": Self::json_column(&self.packages_added),
            "compatibility": Self::json_column(&self.compatibility),
            "installSource": self.install_source,
            "migration": {
                "state": self.migration_state,
                "error": self.migration_error,
                "backup": self.migration_backup,
                "migratedAt": self.migrated_at,
                "dockerVolume": self.docker_volume,
            },
        })
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
            "backend": self.backend,
            "native": self.native_json(),
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
            "visibility": self.visibility().as_str(),
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
    /// Running copies by their secret key (legacy Docker backend).
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Native backend: one shared graphical session per person.
    native: Arc<NativeSessions>,
    /// Native backend: install/update/uninstall/migration of host apps.
    native_apps: Arc<NativeApps>,
    /// Whether native execution is enabled (the default on Linux).
    native_enabled: bool,
}

impl LinuxApps {
    pub fn new(
        config: Arc<Config>,
        db: SqlitePool,
        docker: Arc<DockerManager>,
        events: EventBus,
        jobs: Arc<JobManager>,
        native: Arc<NativeSessions>,
        native_apps: Arc<NativeApps>,
    ) -> Arc<Self> {
        let native_enabled = config.native_enabled && SUPPORTED;
        Arc::new(Self {
            config,
            db,
            docker,
            events,
            jobs,
            sessions: Mutex::new(HashMap::new()),
            native,
            native_apps,
            native_enabled,
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
        if self.native_enabled {
            self.native.start().await;
        }
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
    // Native backend (host execution, one shared graphical session per person)
    // -----------------------------------------------------------------------

    /// Is the native backend configured and available on this server?
    pub fn native_available(&self) -> bool {
        self.native_enabled
    }

    /// The manager of the person's shared graphical session.
    pub fn native_sessions(&self) -> &Arc<NativeSessions> {
        &self.native
    }

    /// Inspect an uploaded or downloaded package without changing anything.
    pub async fn native_preview(
        &self,
        actor: &AuthUser,
        storage: &Storage,
        path: Option<&str>,
        url: Option<&str>,
    ) -> AppResult<Value> {
        self.ensure_native()?;
        actor.require_admin()?;
        let (source, file_name, format) = self.native_source(actor, storage, path, url).await?;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stage = self.stage_native_source(source, file_name, cancel).await?;
        let result = self.native_apps.preview(&stage.staged, format).await;
        stage.cleanup();
        Ok(json!({"preview": result?}))
    }

    /// Resolve a preview/install source into a local file or download URL.
    async fn native_source(
        &self,
        user: &AuthUser,
        storage: &Storage,
        path: Option<&str>,
        url: Option<&str>,
    ) -> AppResult<(NativeSource, String, LinuxFormat)> {
        match (path, url) {
            (Some(path), None) => {
                let path = path.trim();
                let resolved = storage.resolve(&self.db, user, path, Access::Read).await?;
                let file_name = resolved
                    .physical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string();
                let (file_name, format) = self.native_file_kind(&file_name)?;
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
                Ok((NativeSource::File(resolved.physical), file_name, format))
            }
            (None, Some(url)) => {
                let url = url.trim();
                if url.len() > 2048 {
                    return Err(AppError::bad_request("the link is too long"));
                }
                crate::packages::net::https_host(url)
                    .map_err(|_| AppError::bad_request("Enter a link that starts with https://"))?;
                let file_name = file_name_from_url(url).ok_or_else(|| {
                    AppError::bad_request(
                        "The link must end in the file's name: .deb, .AppImage or .tar.gz.",
                    )
                })?;
                let format = self.native_file_kind(&file_name)?.1;
                let checked = url.to_string();
                tokio::task::spawn_blocking(move || {
                    crate::packages::net::OriginPolicy::any_public().allows(&checked)
                })
                .await
                .map_err(|e| AppError::internal(e.to_string()))?
                .map_err(AppError::bad_request)?;
                Ok((NativeSource::Url(url.to_string()), file_name, format))
            }
            _ => Err(AppError::bad_request("send either `path` or `url`")),
        }
    }

    /// The original file name and format of a Linux app package.
    fn native_file_kind(&self, file_name: &str) -> AppResult<(String, LinuxFormat)> {
        let name = file_name.trim();
        if name.is_empty() {
            return Err(AppError::bad_request("choose a file"));
        }
        let format = LinuxFormat::from_file_name(name).ok_or_else(|| {
            AppError::bad_request("Choose a .deb package, an .AppImage or a .tar.gz archive.")
        })?;
        Ok((name.to_string(), format))
    }

    /// Copy or download a package into the native staging area.
    async fn stage_native_source(
        &self,
        source: NativeSource,
        file_name: String,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> AppResult<NativeStage> {
        match source {
            NativeSource::File(path) => {
                // The helper only accepts .deb files under the RAST data
                // directory, so every source is copied into managed staging.
                let (staged, dir) = self
                    .native_apps
                    .copy_into_staging(&path, &file_name)
                    .await?;
                Ok(NativeStage {
                    staged,
                    temp: Some(dir),
                })
            }
            NativeSource::Url(url) => {
                let temp = self
                    .config
                    .linux_staging_dir()
                    .join(format!("download-{}", uuid::Uuid::new_v4()));
                let staging = temp.clone();
                let url_for_download = url.clone();
                let downloaded = tokio::task::spawn_blocking(move || {
                    download_file(&url_for_download, &staging, &cancel)
                })
                .await
                .map_err(|e| AppError::internal(e.to_string()))
                .and_then(|result| result);
                if let Err(error) = downloaded {
                    let _ = std::fs::remove_dir_all(&temp);
                    return Err(error);
                }
                // Copy into managed staging before the download dir goes away.
                let copied = self
                    .native_apps
                    .copy_into_staging(&temp.join("package"), &file_name)
                    .await;
                let _ = std::fs::remove_dir_all(&temp);
                let (staged, dir) = copied?;
                Ok(NativeStage {
                    staged,
                    temp: Some(dir),
                })
            }
        }
    }

    /// Install a native package (job body). The service owns the app row.
    #[allow(clippy::too_many_arguments)]
    async fn run_native_install(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        source: NativeSource,
        file_name: String,
        format: LinuxFormat,
        source_label: &'static str,
        launcher: Option<Value>,
    ) -> AppResult<Value> {
        ctx.progress(0, 3, "Preparing the installation").await;
        let cancel = ctx.cancel_flag();
        let stage = self
            .stage_native_source(source, file_name.clone(), cancel)
            .await?;
        ctx.progress(1, 3, "Inspecting the package before changing the system")
            .await;
        let outcome = self
            .native_apps
            .install(
                actor.id,
                &stage.staged,
                format,
                source_label,
                &ctx,
                launcher.as_ref(),
            )
            .await;
        match outcome {
            Ok(app) => {
                let id = app
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                ctx.progress(3, 3, "Installed").await;
                stage.cleanup();
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.install")
                        .actor(&actor)
                        .object("linux-app", &id)
                        .detail(format!("native, {source_label}"))
                        .success(),
                )
                .await;
                self.announce(&id);
                Ok(app)
            }
            Err(error) => {
                stage.cleanup();
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.install")
                        .actor(&actor)
                        .object("linux-app", file_name.as_str())
                        .detail(error.to_string())
                        .failure(),
                )
                .await;
                Err(error)
            }
        }
    }

    /// One of the person's own native sessions, if it is running.
    pub async fn native_session_json(&self, user: &AuthUser) -> AppResult<Option<Value>> {
        if !self.native_enabled {
            return Ok(None);
        }
        self.native.session_json(user).await
    }

    /// Launch an app into the person's shared native session (lazily started).
    pub async fn native_launch(&self, user: &AuthUser, id: &str) -> AppResult<Value> {
        self.ensure_native()?;
        let row = self.row(id).await?;
        if !row.visible_to(user) {
            return Err(AppError::NotFound("Linux app not found"));
        }
        if !row.is_native() {
            return Err(AppError::conflict(
                "This app still runs on the legacy Docker backend; migrate it first.",
            ));
        }
        if !is_openable_status(&row.status) {
            return Err(AppError::conflict(
                "Wait until the app finishes installing or updating.",
            ));
        }
        let request = self.native_launch_request(&row)?;
        let (run, session) = self.native.launch(user, request).await?;
        Ok(json!({"run": run, "session": session}))
    }

    /// Stop the person's whole native session (all their running apps).
    pub async fn native_close_session(self: &Arc<Self>, user: &AuthUser) -> AppResult<Value> {
        self.ensure_native()?;
        if self.native.session_json(user).await?.is_none() {
            return Err(AppError::NotFound("No graphical session is running"));
        }
        self.native.close_session(user).await
    }

    /// Terminate one app running in the person's native session.
    pub async fn native_close_run(
        self: &Arc<Self>,
        user: &AuthUser,
        run_id: &str,
    ) -> AppResult<Value> {
        self.ensure_native()?;
        self.native.close_app(user, run_id).await
    }

    /// Build the launch request for a native app from its record.
    fn native_launch_request(&self, row: &AppRow) -> AppResult<NativeLaunchRequest> {
        let launcher = AppRow::json_column(&row.launcher);
        let command = launcher
            .get("argv")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|argv| !argv.is_empty())
            .or_else(|| {
                row.command
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<Vec<String>>(text).ok())
                    .filter(|argv| !argv.is_empty())
            })
            .ok_or_else(|| {
                AppError::conflict(
                    "This app has no recorded launcher; reinstall it to fix its launch command.",
                )
            })?;
        let cwd = launcher
            .get("workingDir")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let chromium = launcher
            .get("chromium")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut command = command;
        if chromium {
            for flag in CHROMIUM_LAUNCH_FLAGS {
                if !command.iter().any(|argument| argument == flag) {
                    command.push(flag.to_string());
                }
            }
            // Without a session bus Chromium retries D-Bus calls with long
            // timeouts at startup (minutes on a large migrated profile).
            // dbus-run-session gives the app a private, working session bus.
            let dbus_session = Path::new("/usr/bin/dbus-run-session");
            if dbus_session.is_file() {
                let mut wrapped = vec![dbus_session.display().to_string(), "--".to_string()];
                wrapped.append(&mut command);
                command = wrapped;
            }
        }
        Ok(NativeLaunchRequest {
            app_id: row.id.clone(),
            app_name: row.name.clone(),
            command,
            env: Vec::new(),
            cwd,
            memory_mb: u32::try_from(row.memory_mb).unwrap_or(self.config.linux_app_memory_mb),
            cpus: u32::try_from(row.cpus).unwrap_or(self.config.linux_app_cpus),
            tasks: self.config.linux_app_tasks,
            chromium,
        })
    }

    /// Return an actionable error when the native backend is disabled.
    fn ensure_native(&self) -> AppResult<()> {
        if self.native_enabled {
            Ok(())
        } else {
            Err(native_unavailable())
        }
    }

    /// Migrate one legacy Docker-backed app to native execution.
    pub async fn native_migrate(
        self: &Arc<Self>,
        actor: &AuthUser,
        id: &str,
        path: Option<&str>,
        url: Option<&str>,
        storage: std::sync::Arc<Storage>,
    ) -> AppResult<Value> {
        self.ensure_native()?;
        actor.require_admin()?;
        let row = self.row(id).await?;
        if row.is_native() {
            return Err(AppError::conflict("This app already runs natively."));
        }
        if !matches!(row.status.as_str(), "ready" | "updating") {
            return Err(AppError::conflict(
                "Wait until the app finishes installing or updating.",
            ));
        }
        if !self.config.docker_enabled {
            return Err(AppError::unavailable(
                "The Docker backend is not enabled on this server, so there is nothing to migrate.",
            ));
        }
        // Resolve an optional replacement package. A File Station file is
        // staged now (fast); an HTTPS link is downloaded inside the job so the
        // API answers immediately.
        let replacement = match (path, url) {
            (Some(path), None) => {
                let (source, file_name, format) = self
                    .native_source(actor, &storage, Some(path), None)
                    .await?;
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stage = self.stage_native_source(source, file_name, cancel).await?;
                MigrationInput::Staged(stage, format)
            }
            (None, Some(url)) => {
                let (source, file_name, format) = self.native_url_source(url).await?;
                MigrationInput::Download(source, file_name, format)
            }
            (None, None) => MigrationInput::None,
            _ => return Err(AppError::bad_request("send either `path` or `url`")),
        };
        let this = self.clone();
        let job_actor = actor.clone();
        let app_id = id.to_string();
        let name = row.name.clone();
        let spec = JobSpec::new("linuxapps.migrate", move |ctx| async move {
            this.run_native_migrate(ctx, job_actor, app_id, replacement)
                .await
        })
        .actor(actor)
        .message(format!("Migrating {name} to native execution"));
        let job_id = self.jobs.submit(spec).await?;
        Ok(json!({"jobId": job_id}))
    }

    async fn run_native_migrate(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        app_id: String,
        replacement: MigrationInput,
    ) -> AppResult<Value> {
        let (holder, format) = match replacement {
            MigrationInput::Staged(stage, format) => (Some(stage), Some(format)),
            MigrationInput::Download(source, file_name, format) => {
                ctx.progress(0, 3, "Downloading the replacement package")
                    .await;
                let stage = self
                    .stage_native_source(source, file_name, ctx.cancel_flag())
                    .await?;
                (Some(stage), Some(format))
            }
            MigrationInput::None => (None, None),
        };
        let staged_file = holder.as_ref().map(|stage| stage.staged.clone());
        let outcome = self
            .native_apps
            .migrate_legacy(actor.id, &app_id, staged_file.as_ref(), format, &ctx)
            .await;
        if let Some(holder) = holder {
            holder.cleanup();
        }
        match outcome {
            Ok(app) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.migrate")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail("docker to native")
                        .success(),
                )
                .await;
                self.announce(&app_id);
                Ok(app)
            }
            Err(error) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.migrate")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail(error.to_string())
                        .failure(),
                )
                .await;
                self.announce(&app_id);
                Err(error)
            }
        }
    }

    /// Roll a migrated app back to the Docker backend (its data was kept).
    pub async fn native_rollback(self: &Arc<Self>, actor: &AuthUser, id: &str) -> AppResult<Value> {
        self.ensure_native()?;
        actor.require_admin()?;
        let app = self.native_apps.rollback_migration(actor.id, id).await?;
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("linuxapp.rollback")
                .actor(actor)
                .object("linux-app", id)
                .detail("native back to docker")
                .success(),
        )
        .await;
        self.announce(id);
        Ok(app)
    }

    /// Deliberate post-migration cleanup of retained legacy data.
    pub async fn native_cleanup(
        self: &Arc<Self>,
        actor: &AuthUser,
        id: &str,
        remove_backup: bool,
    ) -> AppResult<Value> {
        self.ensure_native()?;
        actor.require_admin()?;
        let app = self
            .native_apps
            .cleanup_migration(actor.id, id, remove_backup)
            .await?;
        audit::record_with_events(
            &self.db,
            &self.events,
            AuditLog::new("linuxapp.cleanup")
                .actor(actor)
                .object("linux-app", id)
                .detail(if remove_backup {
                    "retained docker data removed"
                } else {
                    "migration backup kept"
                })
                .success(),
        )
        .await;
        self.announce(id);
        Ok(app)
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
        if self.native_enabled {
            for (app_id, running) in self.native.running_counts() {
                *counts.entry(app_id).or_insert(0) += running;
            }
        }
        counts
    }

    /// The app list as `user` may see it: everything for an administrator,
    /// only shared apps with a finished build for everyone else.
    pub async fn list(&self, user: &AuthUser) -> AppResult<Vec<Value>> {
        let sql = if user.is_admin() {
            format!("SELECT {APP_COLUMNS} FROM linux_apps ORDER BY name COLLATE NOCASE, id")
        } else {
            format!(
                "SELECT {APP_COLUMNS} FROM linux_apps \
                 WHERE visibility = 'users' AND (build > 0 OR backend = 'native') \
                   AND status IN ('ready', 'updating') \
                 ORDER BY name COLLATE NOCASE, id"
            )
        };
        let rows = sqlx::query_as::<_, AppRow>(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.db)
            .await?;
        let counts = self.running_counts();
        Ok(rows
            .iter()
            .map(|row| row.json(counts.get(&row.id).copied().unwrap_or(0)))
            .collect())
    }

    pub async fn get(&self, user: &AuthUser, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        if !row.visible_to(user) {
            return Err(AppError::NotFound("Linux app not found"));
        }
        let running = self.running_counts().get(id).copied().unwrap_or(0);
        Ok(row.json(running))
    }

    /// The app regardless of who may see it: for administrator and background
    /// paths that already worked out the row.
    async fn any_json(&self, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        let running = self.running_counts().get(id).copied().unwrap_or(0);
        Ok(row.json(running))
    }

    pub async fn icon(&self, user: &AuthUser, id: &str) -> AppResult<(String, Vec<u8>)> {
        let row = self.row(id).await?;
        if !row.visible_to(user) {
            return Err(AppError::NotFound("Linux app not found"));
        }
        let icon: Option<(Option<String>, Option<Vec<u8>>)> =
            sqlx::query_as("SELECT icon_type, icon FROM linux_apps WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.db)
                .await?;
        match icon {
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

    /// Tell every connected client that an app changed. Everyone reloads the
    /// list they may see, so a hidden app never appears from the event itself.
    fn announce(&self, id: &str) {
        self.events.publish(APPS_EVENT, json!({"id": id}));
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
        self.get(user, id).await
    }

    /// Change who can see and open an app (administrators only).
    pub async fn set_visibility(
        &self,
        user: &AuthUser,
        id: &str,
        visibility: Visibility,
    ) -> AppResult<Value> {
        user.require_admin()?;
        let row = self.row(id).await?;
        if row.visibility() != visibility {
            sqlx::query("UPDATE linux_apps SET visibility = ?, updated_at = ? WHERE id = ?")
                .bind(visibility.as_str())
                .bind(db::now_ms())
                .bind(id)
                .execute(&self.db)
                .await?;
            let detail = match visibility {
                Visibility::Users => "visible to every user",
                Visibility::Admins => "administrators only",
            };
            audit::record_with_events(
                &self.db,
                &self.events,
                AuditLog::new("linuxapp.visibility")
                    .actor(user)
                    .object("linux-app", id)
                    .detail(detail)
                    .success(),
            )
            .await;
            self.announce(id);
        }
        self.get(user, id).await
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
        self.install_with(user, storage, path, None).await
    }

    /// Install with an explicit launcher choice for ambiguous archives.
    pub async fn install_with(
        self: &Arc<Self>,
        user: &AuthUser,
        storage: &Storage,
        path: &str,
        launcher: Option<Value>,
    ) -> AppResult<Value> {
        if self.native_enabled {
            self.ensure_native()?;
            let (source, file_name, format) =
                self.native_source(user, storage, Some(path), None).await?;
            return self
                .begin_native_install(user.clone(), source, file_name, format, "file", launcher)
                .await;
        }
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

        self.begin_install(
            user,
            file_name,
            format,
            PackageSource::File(resolved.physical),
        )
        .await
    }

    /// Install the app file behind an `https://` link (downloaded by the server).
    pub async fn install_url(self: &Arc<Self>, user: &AuthUser, url: &str) -> AppResult<Value> {
        self.install_url_with(user, url, None).await
    }

    /// Install from an `https://` link with an explicit launcher choice.
    pub async fn install_url_with(
        self: &Arc<Self>,
        user: &AuthUser,
        url: &str,
        launcher: Option<Value>,
    ) -> AppResult<Value> {
        if self.native_enabled {
            self.ensure_native()?;
            let (source, file_name, format) = self.native_url_source(url).await?;
            return self
                .begin_native_install(user.clone(), source, file_name, format, "url", launcher)
                .await;
        }
        self.ensure_available().await?;
        if url.len() > 2048 {
            return Err(AppError::bad_request("the link is too long"));
        }
        crate::packages::net::https_host(url)
            .map_err(|_| AppError::bad_request("Enter a link that starts with https://"))?;
        let file_name = file_name_from_url(url).ok_or_else(|| {
            AppError::bad_request(
                "The link must end in the file's name: .deb, .AppImage or .tar.gz.",
            )
        })?;
        let format = LinuxFormat::from_file_name(&file_name).ok_or_else(|| {
            AppError::bad_request(
                "The link must end in the file's name: .deb, .AppImage or .tar.gz.",
            )
        })?;
        // Refuse links into this server or its network before anything is recorded.
        let checked = url.to_string();
        tokio::task::spawn_blocking(move || {
            crate::packages::net::OriginPolicy::any_public().allows(&checked)
        })
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .map_err(AppError::bad_request)?;
        self.begin_install(user, file_name, format, PackageSource::Url(url.to_string()))
            .await
    }

    /// Validate an `https://` link and derive its file name and format.
    ///
    /// The origin policy check resolves the URL's host, so it runs on the
    /// blocking pool instead of the async runtime.
    async fn native_url_source(&self, url: &str) -> AppResult<(NativeSource, String, LinuxFormat)> {
        let url = url.trim();
        if url.len() > 2048 {
            return Err(AppError::bad_request("the link is too long"));
        }
        crate::packages::net::https_host(url)
            .map_err(|_| AppError::bad_request("Enter a link that starts with https://"))?;
        let file_name = file_name_from_url(url).ok_or_else(|| {
            AppError::bad_request(
                "The link must end in the file's name: .deb, .AppImage or .tar.gz.",
            )
        })?;
        let format = self.native_file_kind(&file_name)?.1;
        let checked = url.to_string();
        tokio::task::spawn_blocking(move || {
            crate::packages::net::OriginPolicy::any_public().allows(&checked)
        })
        .await
        .map_err(|error| AppError::internal(error.to_string()))?
        .map_err(AppError::bad_request)?;
        Ok((NativeSource::Url(url.to_string()), file_name, format))
    }

    /// Start a native install job; the NativeApps service owns the app row.
    async fn begin_native_install(
        self: &Arc<Self>,
        user: AuthUser,
        source: NativeSource,
        file_name: String,
        format: LinuxFormat,
        source_label: &'static str,
        launcher: Option<Value>,
    ) -> AppResult<Value> {
        let this = self.clone();
        let actor = user.clone();
        let job_file_name = file_name.clone();
        let spec = JobSpec::new("linuxapps.install", move |ctx| async move {
            this.run_native_install(
                ctx,
                actor,
                source,
                job_file_name,
                format,
                source_label,
                launcher,
            )
            .await
        })
        .actor(&user)
        .message(format!("Installing {file_name}"));
        let job_id = self.jobs.submit(spec).await?;
        Ok(json!({"jobId": job_id, "installing": true}))
    }

    async fn begin_install(
        self: &Arc<Self>,
        user: &AuthUser,
        file_name: String,
        format: LinuxFormat,
        source: PackageSource,
    ) -> AppResult<Value> {
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
        Ok(json!({"app": self.get(user, &id).await?, "jobId": job_id}))
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
        source: PackageSource,
        format: LinuxFormat,
    ) -> AppResult<Value> {
        let downloading = matches!(source, PackageSource::Url(_));
        ctx.progress(
            0,
            3,
            if downloading {
                "Downloading the file"
            } else {
                "Copying the file"
            },
        )
        .await;
        let staging = self.staging_dir(&id);
        let copied = {
            let staging = staging.clone();
            let cancel = ctx.cancel_flag();
            tokio::task::spawn_blocking(move || match source {
                PackageSource::File(path) => stage_file(&path, &staging, &cancel),
                PackageSource::Url(url) => download_file(&url, &staging, &cancel),
            })
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
        if row.is_native() {
            self.ensure_native()?;
            self.native_apps.check(id).await?;
            self.announce(id);
            return self.any_json(id).await;
        }
        self.check(&row).await;
        self.any_json(id).await
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
             WHERE status = 'ready' AND backend = 'docker' AND update_kind IS NOT NULL
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
        self.update_with_source(user, id, None, None, None).await
    }

    /// Update from the official source, or from an uploaded file / HTTPS link.
    pub async fn update_with_source(
        self: &Arc<Self>,
        user: &AuthUser,
        id: &str,
        path: Option<&str>,
        url: Option<&str>,
        storage: Option<&Storage>,
    ) -> AppResult<Value> {
        let row = self.row(id).await?;
        if row.status != "ready" {
            return Err(AppError::conflict(
                "wait until the current install or update finishes",
            ));
        }
        if row.is_native() {
            self.ensure_native()?;
            return match (path, url, storage) {
                (Some(path), None, Some(storage)) => {
                    let (source, file_name, format) =
                        self.native_source(user, storage, Some(path), None).await?;
                    self.begin_native_update(
                        user.clone(),
                        id.to_string(),
                        source,
                        file_name,
                        format,
                        "file",
                    )
                    .await
                }
                (None, Some(url), _) => {
                    let (source, file_name, format) = self.native_url_source(url).await?;
                    self.begin_native_update(
                        user.clone(),
                        id.to_string(),
                        source,
                        file_name,
                        format,
                        "url",
                    )
                    .await
                }
                (None, None, _) => {
                    let this = self.clone();
                    let actor = user.clone();
                    let app_id = id.to_string();
                    let name = row.name.clone();
                    let spec = JobSpec::new("linuxapps.update", move |ctx| async move {
                        this.run_native_update_upstream(ctx, actor, app_id).await
                    })
                    .actor(user)
                    .message(format!("Updating {name}"));
                    let job_id = self.jobs.submit(spec).await?;
                    Ok(json!({"jobId": job_id}))
                }
                _ => Err(AppError::bad_request("send either `path` or `url`")),
            };
        }
        self.ensure_available().await?;
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

    /// Start a native update job from a staged file/link.
    async fn begin_native_update(
        self: &Arc<Self>,
        user: AuthUser,
        app_id: String,
        source: NativeSource,
        file_name: String,
        format: LinuxFormat,
        source_label: &'static str,
    ) -> AppResult<Value> {
        let this = self.clone();
        let actor = user.clone();
        let label = source_label;
        let spec = JobSpec::new("linuxapps.update", move |ctx| async move {
            this.run_native_update_source(ctx, actor, app_id, source, file_name, format, label)
                .await
        })
        .actor(&user)
        .message("Updating a native Linux app");
        let job_id = self.jobs.submit(spec).await?;
        Ok(json!({"jobId": job_id}))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_native_update_source(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        app_id: String,
        source: NativeSource,
        file_name: String,
        format: LinuxFormat,
        source_label: &'static str,
    ) -> AppResult<Value> {
        ctx.progress(0, 3, "Preparing the update").await;
        let cancel = ctx.cancel_flag();
        let stage = self.stage_native_source(source, file_name, cancel).await?;
        ctx.progress(1, 3, "Inspecting the new version").await;
        let outcome = self
            .native_apps
            .update_from(
                actor.id,
                &app_id,
                &stage.staged,
                format,
                source_label,
                &ctx,
                None,
            )
            .await;
        stage.cleanup();
        match outcome {
            Ok(app) => {
                ctx.progress(3, 3, "Updated").await;
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.update")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail(format!("native, {source_label}"))
                        .success(),
                )
                .await;
                self.announce(&app_id);
                Ok(app)
            }
            Err(error) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.update")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail(error.to_string())
                        .failure(),
                )
                .await;
                Err(error)
            }
        }
    }

    async fn run_native_update_upstream(
        self: Arc<Self>,
        ctx: JobContext,
        actor: AuthUser,
        app_id: String,
    ) -> AppResult<Value> {
        let outcome = self
            .native_apps
            .update_from_upstream(actor.id, &app_id, &ctx)
            .await;
        match outcome {
            Ok(app) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.update")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail("native, official source")
                        .success(),
                )
                .await;
                self.announce(&app_id);
                Ok(app)
            }
            Err(error) => {
                audit::record_with_events(
                    &self.db,
                    &self.events,
                    AuditLog::new("linuxapp.update")
                        .actor(&actor)
                        .object("linux-app", &app_id)
                        .detail(error.to_string())
                        .failure(),
                )
                .await;
                Err(error)
            }
        }
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
        if row.is_native() {
            self.ensure_native()?;
            // Stop every run first so no process keeps a removed launcher.
            self.native.stop_app_runs(id).await;
            let removed = self.native_apps.uninstall(user.id, id, remove_data).await?;
            let _ = std::fs::remove_dir_all(self.staging_dir(id));
            audit::record_with_events(
                &self.db,
                &self.events,
                AuditLog::new("linuxapp.uninstall")
                    .actor(user)
                    .object("linux-app", id)
                    .detail(if remove_data {
                        "native, with app data"
                    } else {
                        "native, app data kept"
                    })
                    .success(),
            )
            .await;
            self.announce(id);
            return Ok(json!({"ok": true, "removed": removed}));
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

    pub async fn session(&self, user: &AuthUser, session_id: &str) -> AppResult<Value> {
        if session_id == "native" {
            return self
                .native_session_json(user)
                .await?
                .ok_or(AppError::NotFound("this app is not running"));
        }
        Ok(self.find_by_id(user, session_id)?.json())
    }

    /// Stop every running copy of an account that was just deactivated or
    /// revoked (its apps stay on disk; only the processes/sessions end).
    pub async fn stop_user_sessions(&self, user_id: i64) {
        let sessions: Vec<Arc<Session>> = self
            .sessions()
            .values()
            .filter(|session| session.user_id == user_id && session.lock().status != Status::Ended)
            .cloned()
            .collect();
        for session in sessions {
            self.end(&session, "removed", None);
        }
        if self.native_enabled {
            if let Err(error) = self.native.close_for_user(user_id).await {
                tracing::warn!(
                    user_id,
                    error = %error,
                    "could not stop the native session of a deactivated account"
                );
            }
        }
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
        if !row.visible_to(user) {
            return Err(AppError::NotFound("Linux app not found"));
        }
        // Native apps launch into the person's shared session and return that
        // session (the run detail is available from `/launch`).
        if row.is_native() {
            let launched = self.native_launch(user, id).await?;
            return Ok(launched.get("session").cloned().unwrap_or(Value::Null));
        }
        if (row.build < 1 && !row.is_native())
            || !matches!(row.status.as_str(), "ready" | "updating")
        {
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
                control_port: None,
                vnc_ports: Vec::new(),
                window_streams: HashMap::new(),
                separate_windows: false,
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
            Ok(ref data) => data
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok()),
            Err(error) => {
                self.end(&session, "error", Some(docker_message(&error)));
                return;
            }
        };
        let (control_port, vnc_ports) = match &started {
            Ok(data) => {
                let control = data
                    .get("controlPort")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok());
                let vnc = data
                    .get("vncPorts")
                    .and_then(Value::as_array)
                    .map(|ports| {
                        ports
                            .iter()
                            .filter_map(|port| port.as_u64())
                            .filter_map(|port| u16::try_from(port).ok())
                            .collect::<Vec<u16>>()
                    })
                    .unwrap_or_default();
                (control, vnc)
            }
            Err(_) => (None, Vec::new()),
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
            state.control_port = control_port;
            state.vnc_ports = vnc_ports;
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
        // Old images have the ports but no window service: ask it.
        let separate_windows = match control_port {
            Some(control) => matches!(
                tokio::time::timeout(
                    Duration::from_secs(2),
                    control_request(control, &session.authorization(), "GET", "/health"),
                )
                .await,
                Ok(Ok(_))
            ),
            None => false,
        };
        {
            let mut state = session.lock();
            if state.status != Status::Starting {
                return;
            }
            state.separate_windows = separate_windows;
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
    pub async fn close(self: &Arc<Self>, user: &AuthUser, session_id: &str) -> AppResult<Value> {
        if session_id == "native" {
            return self.native_close_session(user).await;
        }
        let session = self.find_by_id(user, session_id)?;
        self.end(&session, "closed", None);
        Ok(json!({"ok": true}))
    }

    /// One of the person's native sessions by its secret URL key (the native
    /// proxy authorizes the request itself before using this).
    pub fn find_native_session(
        &self,
        key: &str,
    ) -> Option<Arc<self::native_session::NativeSession>> {
        self.native.find_session(key)
    }

    // -----------------------------------------------------------------------
    // Separate windows (per-window streaming)
    // -----------------------------------------------------------------------

    /// The app's windows, as the container's window control service sees them.
    pub async fn windows(&self, user: &AuthUser, session_id: &str) -> AppResult<Value> {
        let session = self.find_by_id(user, session_id)?;
        let port = session.control_port().ok_or_else(no_window_service)?;
        control_request(port, &session.authorization(), "GET", "/windows").await
    }

    /// The VNC stream of `window`, starting it if needed.
    pub(crate) async fn window_stream_port(
        &self,
        session: &Arc<Session>,
        window: &str,
    ) -> AppResult<WindowStream> {
        if let Some(stream) = session.window_stream(window) {
            return Ok(stream);
        }
        if !validate_window_id(window) {
            return Err(AppError::bad_request("invalid window id"));
        }
        let control = session.control_port().ok_or_else(no_window_service)?;
        let started = control_request(
            control,
            &session.authorization(),
            "POST",
            &format!("/vnc/{window}"),
        )
        .await?;
        let slot = started
            .get("slot")
            .and_then(Value::as_u64)
            .and_then(|slot| usize::try_from(slot).ok())
            .ok_or_else(|| AppError::unavailable("the window stream did not report its slot"))?;
        let port = session
            .vnc_host_port(slot)
            .ok_or_else(|| AppError::unavailable("no VNC port is published for this slot"))?;
        let password = started
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let stream = WindowStream { port, password };
        session.remember_window_stream(window, port, stream.password.clone());
        Ok(stream)
    }

    /// Start (or find) the VNC stream of one of the app's windows and return
    /// the address a viewer connects to.
    pub async fn start_window_stream(
        &self,
        user: &AuthUser,
        session_id: &str,
        window: &str,
    ) -> AppResult<Value> {
        if !validate_window_id(window) {
            return Err(AppError::bad_request("invalid window id"));
        }
        let session = self.find_by_id(user, session_id)?;
        let stream = self.window_stream_port(&session, window).await?;
        Ok(stream_address(&session, window, &stream))
    }

    /// Close one of the app's windows the way its own close button would.
    pub async fn close_window(
        &self,
        user: &AuthUser,
        session_id: &str,
        window: &str,
    ) -> AppResult<Value> {
        if !validate_window_id(window) {
            return Err(AppError::bad_request("invalid window id"));
        }
        let session = self.find_by_id(user, session_id)?;
        let port = session.control_port().ok_or_else(no_window_service)?;
        let result = control_request(
            port,
            &session.authorization(),
            "POST",
            &format!("/close/{window}"),
        )
        .await?;
        session.forget_window_stream(window);
        Ok(result)
    }

    /// Stop the VNC stream of a window (its RAST window was closed).
    pub async fn stop_window_stream(
        &self,
        user: &AuthUser,
        session_id: &str,
        window: &str,
    ) -> AppResult<Value> {
        if !validate_window_id(window) {
            return Err(AppError::bad_request("invalid window id"));
        }
        let session = self.find_by_id(user, session_id)?;
        let port = session.control_port().ok_or_else(no_window_service)?;
        let result = control_request(
            port,
            &session.authorization(),
            "DELETE",
            &format!("/vnc/{window}"),
        )
        .await;
        session.forget_window_stream(window);
        result
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

    /// The app's stream went away: when the app itself has exited (its own
    /// close button, or a crash) end the copy and tell the window, so the
    /// window closes instead of staying blank until the periodic sweep.
    /// A reload or a network blip reconnects and keeps the copy running.
    pub fn watch_exit(self: &Arc<Self>, session: Arc<Session>) {
        let this = self.clone();
        tokio::spawn(async move {
            for delay in EXIT_CHECK_DELAYS {
                tokio::time::sleep(delay).await;
                {
                    let state = session.lock();
                    if state.status == Status::Ended || state.viewers > 0 {
                        return;
                    }
                }
                let Ok(list) = this
                    .docker
                    .call(WorkerRequest::new(WorkerOp::LinuxList))
                    .await
                else {
                    return;
                };
                if container_running(&list, &session.app_id, session.user_id) {
                    continue;
                }
                // A viewer may have reconnected while Docker answered.
                let stale = {
                    let state = session.lock();
                    state.status != Status::Ended && state.viewers == 0
                };
                if stale {
                    this.end(&session, "exited", None);
                }
                return;
            }
        });
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

/// One running window stream: where the VNC server listens and the password
/// the viewer needs (the stream is not open to other containers).
#[derive(Debug, Clone)]
pub(crate) struct WindowStream {
    port: u16,
    password: String,
}

/// `/close`, `/vnc` and the RAST windows address one X11 window by its id.
pub fn validate_window_id(window: &str) -> bool {
    window.strip_prefix("0x").is_some_and(|hex| {
        !hex.is_empty() && hex.len() <= 16 && hex.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Images built before per-window streaming have no window service.
fn no_window_service() -> AppError {
    AppError::unavailable("this app's image does not support separate windows; use Reinstall on it")
}

/// The address a noVNC viewer connects to for one of the app's windows.
fn stream_address(session: &Session, window: &str, stream: &WindowStream) -> Value {
    json!({
        "url": format!("{}{}/vnc/{window}/", wire::URL_PREFIX, session.key()),
        "password": stream.password,
    })
}

/// One JSON request to the app container's window control service.
async fn control_request(
    port: u16,
    authorization: &str,
    method: &str,
    path: &str,
) -> Result<Value, AppError> {
    let no_service = || AppError::unavailable("the app's window service is not answering");
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return Err(no_service()),
    };
    let head = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return Err(no_service());
    }
    let mut response = Vec::new();
    match tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await {
        Ok(Ok(_)) => {}
        _ => return Err(no_service()),
    }
    let Some(split) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return Err(no_service());
    };
    let (head_bytes, body) = response.split_at(split + 4);
    let head_text = String::from_utf8_lossy(head_bytes);
    let status: u16 = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the window service refused the request");
        return Err(AppError::unavailable(message.to_string()));
    }
    Ok(value)
}

/// Is one person's copy of `app` in the worker's running list?
fn container_running(list: &Value, app: &str, user_id: i64) -> bool {
    containers_of(list).iter().any(|container| {
        container.get("running").and_then(Value::as_bool) == Some(true)
            && container.get("app").and_then(Value::as_str) == Some(app)
            && container.get("userId").and_then(Value::as_i64) == Some(user_id)
    })
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

/// Where a native package comes from before it is staged.
enum NativeSource {
    File(PathBuf),
    Url(String),
}

/// Replacement package supplied for a migration.
enum MigrationInput {
    None,
    Staged(NativeStage, LinuxFormat),
    Download(NativeSource, String, LinuxFormat),
}

/// A staged native package and the temporary directory to clean up.
struct NativeStage {
    staged: StagedFile,
    temp: Option<PathBuf>,
}

impl NativeStage {
    fn cleanup(self) {
        if let Some(dir) = self.temp {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Can an app with this status be opened?
fn is_openable_status(status: &str) -> bool {
    matches!(status, "ready" | "updating")
}

/// The native backend is not configured on this server.
fn native_unavailable() -> AppError {
    AppError::unavailable(
        "native Linux apps are not available on this server; install the native helper          (see docs/INSTALL.md, \"Native Linux apps (Xpra)\") or set native_enabled = true",
    )
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
/// Where an app's file comes from.
enum PackageSource {
    /// A file in File Station (already resolved for the installing account).
    File(PathBuf),
    /// An `https://` link on the public internet.
    Url(String),
}

/// The file name a download link ends in (`…/app_1.0_amd64.deb?x=1`).
fn file_name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let name = path.strip_prefix("https://")?.split_once('/')?.1;
    let name = name.rsplit('/').next()?;
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'));
    ok.then(|| name.to_string())
}

/// Download the app file into `staging/package`; returns its SHA-1. Blocks.
fn download_file(
    url: &str,
    staging: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> AppResult<String> {
    std::fs::create_dir_all(staging)
        .map_err(|e| AppError::internal(format!("cannot prepare the app folder: {e}")))?;
    let cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
    let done = crate::packages::net::download_hashed(
        url,
        &crate::packages::net::OriginPolicy::any_public(),
        &staging.join("package"),
        None,
        wire::MAX_PACKAGE_BYTES,
        &cancelled,
    )
    .map_err(|message| {
        if cancelled() {
            AppError::Cancelled
        } else {
            AppError::bad_request(format!("The download failed: {message}"))
        }
    })?;
    if done.size == 0 {
        return Err(AppError::bad_request("The link returned an empty file."));
    }
    Ok(done.sha1)
}

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
    fn chromium_flags_keep_the_sandbox_and_hide_the_restore_bubble() {
        assert!(CHROMIUM_LAUNCH_FLAGS.contains(&"--hide-crash-restore-bubble"));
        assert!(CHROMIUM_LAUNCH_FLAGS.contains(&"--no-first-run"));
        assert!(!CHROMIUM_LAUNCH_FLAGS
            .iter()
            .any(|flag| flag.contains("no-sandbox")));
    }

    #[tokio::test]
    async fn window_service_requests_are_read() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0u8; 1024];
            let read = socket.read(&mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.starts_with("GET /windows "), "{request}");
            assert!(
                request
                    .lines()
                    .any(|line| line == "Authorization: Basic abc"),
                "{request}"
            );
            socket
                .write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"windows\":[{\"id\":\"0x1\"}]}")
                .await
                .expect("write");
        });
        let value = control_request(port, "Basic abc", "GET", "/windows")
            .await
            .expect("json");
        assert_eq!(value["windows"][0]["id"], "0x1");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn window_service_errors_are_reported() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(b"HTTP/1.0 500 Internal Server Error\r\n\r\n{\"error\":\"no free window stream\"}")
                .await
                .expect("write");
        });
        let error = control_request(port, "Basic abc", "POST", "/vnc/0x1")
            .await
            .expect_err("must fail");
        assert!(
            error.to_string().contains("no free window stream"),
            "{error}"
        );
        server.await.expect("server");
    }

    #[test]
    fn window_ids_are_hex() {
        assert!(validate_window_id("0x1200002"));
        assert!(validate_window_id("0x1a"));
        for bad in ["", "0x", "1200002", "0xzz", "0x1234567890abcdef0", "0x1/2"] {
            assert!(!validate_window_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn visibility_round_trips() {
        assert_eq!(Visibility::parse("users"), Some(Visibility::Users));
        assert_eq!(Visibility::parse("admins"), Some(Visibility::Admins));
        for bad in ["", "everyone", "admin", "Users", " users "] {
            assert_eq!(Visibility::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Visibility::Users.as_str(), "users");
        assert_eq!(Visibility::Admins.as_str(), "admins");
    }

    #[test]
    fn links_must_end_in_a_file_name() {
        assert_eq!(
            file_name_from_url("https://dl.example.com/linux/app_1.0_amd64.deb?x=1#y").as_deref(),
            Some("app_1.0_amd64.deb")
        );
        for bad in [
            "https://example.com",
            "https://example.com/",
            "https://example.com/dir/",
            "https://example.com/a%20b.deb",
            "http://example.com/app.deb",
        ] {
            assert!(file_name_from_url(bad).is_none(), "{bad}");
        }
    }

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

    #[test]
    fn running_copies_are_found_by_app_and_user() {
        let list = json!({"containers": [
            {"app": "chrome", "userId": 1, "running": true},
            {"app": "chrome", "userId": 2, "running": false},
            {"app": "firefox", "userId": 1, "running": true},
        ]});
        assert!(container_running(&list, "chrome", 1));
        assert!(!container_running(&list, "chrome", 2));
        assert!(!container_running(&list, "chrome", 3));
        assert!(container_running(&list, "firefox", 1));
        assert!(!container_running(&json!({}), "chrome", 1));
    }
}
