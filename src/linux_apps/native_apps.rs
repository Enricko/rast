//! Native Linux application installation, update, removal and migration.
//!
//! Linux desktop apps no longer run inside a Docker image that is built per
//! app. This service installs an app on the host instead:
//!
//! * `.deb` packages are installed through the privileged native helper with
//!   the host package manager (`apt-get`), which keeps dependencies and host
//!   consistency in one place;
//! * portable archives (`.tar.gz`, `.tar.xz`, `.tar.bz2`, `.tar`) are unpacked
//!   into a managed versioned directory ([`super::native_archive`] does the safe
//!   extraction and launcher detection);
//! * AppImages are unpacked with their own `--appimage-extract` mode as the
//!   unprivileged `rast` user and installed into a managed directory.
//!
//! The service owns every write to `linux_apps` rows for native apps, the
//! on-disk layout under `<data_dir>/linux-apps` and the Docker backend's
//! volume data during a migration. Starting apps (Xpra sessions) is the
//! session manager's job: this module only prepares files, metadata and the
//! launcher command.
//!
//! Docker remains available for apps whose row has `backend = 'docker'`; a
//! deliberate migration can move one app at a time to the native backend and
//! roll it back, keeping the Docker volume untouched as recovery data.
//!
//! ## Managed layout
//!
//! ```text
//! <data_dir>/linux-apps/
//! ├── staging/<uuid>/…          uploads and temporary unpacking
//! ├── apps/<app-id>/<version>/  portable and AppImage installs (versioned)
//! ├── profiles/<app-id>/<user>/{home,config,data,cache,state,tmp}
//! └── migrations/<app-id>/<user>/volume   legacy volume during a migration
//! ```

use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use super::{native_archive, updates};
use crate::{
    config::Config,
    db,
    docker::{linux::LinuxFormat, DockerError, DockerManager},
    error::{AppError, AppResult},
    events::EventBus,
    jobs::JobContext,
    native::client::NativeClient,
};

/// Largest staged application file accepted (4 GiB).
pub const MAX_STAGED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Longest accepted uploaded file name.
pub const MAX_FILE_NAME: usize = 200;
/// Largest number of user profiles migrated in one operation.
pub const MAX_MIGRATION_USERS: usize = 500;
/// Largest number of package names recorded in `packages_added`.
pub const MAX_ADDED_PACKAGES: usize = 200;
/// Largest number of files written while copying one migrated profile.
const MAX_PROFILE_FILES: usize = 200_000;
/// Largest number of bytes written while copying one migrated profile.
const MAX_PROFILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Staged files
// ---------------------------------------------------------------------------

/// An uploaded (or downloaded) application file waiting to be installed.
///
/// The bytes live under `<data_dir>/linux-apps/staging/…`; the struct carries
/// the SHA-256 that was computed while staging, so a job can re-verify that the
/// file did not change before the privileged helper reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub path: PathBuf,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
}

impl StagedFile {
    /// Validate `path` and compute its SHA-256. Blocks; call
    /// [`StagedFile::stage_async`] from async code.
    pub fn stage(path: impl Into<PathBuf>, file_name: Option<&str>) -> AppResult<Self> {
        let path = path.into();
        let file_name = match file_name {
            Some(name) => name.to_string(),
            None => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .ok_or_else(|| AppError::bad_request("the uploaded file has no name"))?,
        };
        validate_file_name(&file_name)?;
        let meta = fs::symlink_metadata(&path).map_err(|_| AppError::NotFound("path not found"))?;
        if meta.file_type().is_symlink() {
            return Err(AppError::bad_request(
                "symbolic links cannot be installed; upload the file itself",
            ));
        }
        if !meta.is_file() {
            return Err(AppError::bad_request("choose a file, not a folder"));
        }
        if meta.len() == 0 {
            return Err(AppError::bad_request("the file is empty"));
        }
        if meta.len() > MAX_STAGED_BYTES {
            return Err(AppError::bad_request(
                "the file is larger than 4 GB; upload a smaller package",
            ));
        }
        let (size, sha256) = sha256_file(&path)?;
        Ok(Self {
            path,
            file_name,
            size,
            sha256,
        })
    }

    /// Async wrapper around [`StagedFile::stage`].
    pub async fn stage_async(path: impl Into<PathBuf>, file_name: Option<&str>) -> AppResult<Self> {
        let path = path.into();
        let file_name = file_name.map(str::to_string);
        tokio::task::spawn_blocking(move || StagedFile::stage(path, file_name.as_deref())).await?
    }

    /// Re-check that the staged file is still a regular file of the recorded
    /// size with the recorded name.
    pub fn validate(&self) -> AppResult<()> {
        validate_file_name(&self.file_name)?;
        let meta =
            fs::symlink_metadata(&self.path).map_err(|_| AppError::NotFound("path not found"))?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(AppError::bad_request(
                "the uploaded file is no longer a regular file",
            ));
        }
        if meta.len() != self.size {
            return Err(AppError::bad_request(
                "the uploaded file changed after it was staged; upload it again",
            ));
        }
        Ok(())
    }

    /// Re-compute the SHA-256 of the staged file and compare it with the
    /// recorded digest. Blocks.
    pub fn verify(&self) -> AppResult<()> {
        self.validate()?;
        let (size, sha256) = sha256_file(&self.path)?;
        if size != self.size || !sha256.eq_ignore_ascii_case(&self.sha256) {
            return Err(AppError::bad_request(
                "the uploaded file changed after it was staged; upload it again",
            ));
        }
        Ok(())
    }

    /// The staged path as the helper wants it (an absolute string).
    fn helper_path(&self) -> AppResult<String> {
        let path = fs::canonicalize(&self.path)
            .map_err(|_| AppError::bad_request("the uploaded file is missing"))?;
        if !path.is_absolute() {
            return Err(AppError::internal("the staging path is not absolute"));
        }
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| AppError::bad_request("the uploaded file has an unsupported name"))
    }

    /// The SHA-256 as the helper wants it (lowercase hex).
    fn helper_sha256(&self) -> AppResult<&str> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AppError::internal("the staged file's SHA-256 is invalid"));
        }
        Ok(&self.sha256)
    }
}

/// Validate an uploaded file name (`[A-Za-z0-9._+-]`, not hidden).
pub fn validate_file_name(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > MAX_FILE_NAME {
        return Err(AppError::bad_request(
            "the file name is empty or too long (200 characters at most)",
        ));
    }
    if name.starts_with('.') {
        return Err(AppError::bad_request("the file name starts with a dot"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
    {
        return Err(AppError::bad_request(
            "the file name contains unsupported characters",
        ));
    }
    Ok(())
}

/// SHA-256 and size of a file, streamed (never loads it whole).
pub fn sha256_file(path: &Path) -> AppResult<(u64, String)> {
    use sha2::Digest as _;
    let mut file = crate::storage::safe_path::open_read_no_follow(path)
        .map_err(|_| AppError::NotFound("path not found"))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::internal(format!("cannot read the file: {error}")))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_STAGED_BYTES {
            return Err(AppError::bad_request(
                "the file is larger than 4 GB; upload a smaller package",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex::encode(hasher.finalize())))
}

/// SHA-1 and size of a file (AppImage update checks compare SHA-1 digests).
pub fn sha1_file(path: &Path) -> AppResult<(u64, String)> {
    use sha1::Digest as _;
    let mut file = crate::storage::safe_path::open_read_no_follow(path)
        .map_err(|_| AppError::NotFound("path not found"))?;
    let mut hasher = sha1::Sha1::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::internal(format!("cannot read the file: {error}")))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex::encode(hasher.finalize())))
}

// ---------------------------------------------------------------------------
// Privileged helper
// ---------------------------------------------------------------------------

/// Error returned by the privileged native helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

impl HostError {
    /// The user-facing message, without the machine-readable code.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The slice of the privileged native helper the install service needs.
///
/// The real implementation talks to `rast-native.service` through
/// [`NativeClient`]; tests use a fake. Keeping it a trait lets the install
/// logic be tested without root or a running helper.
#[async_trait]
pub trait HostHelper: Send + Sync {
    async fn call(&self, op: &str, params: Value) -> Result<Value, HostError>;
}

#[async_trait]
impl HostHelper for NativeClient {
    async fn call(&self, op: &str, params: Value) -> Result<Value, HostError> {
        self.call_json(op, params).await.map_err(|error| HostError {
            code: error.code,
            message: error.message,
        })
    }
}

/// Turn a helper failure into a client-facing error with the helper's
/// actionable message.
fn helper_error(op: &str, error: HostError) -> AppError {
    let message = if error.message.trim().is_empty() {
        format!("the privileged system helper failed to run `{op}`")
    } else {
        error.message
    };
    match error.code.as_str() {
        // User/service decisions: surface the message as-is.
        "unsafe_removal" => AppError::conflict(message),
        "invalid_request" | "checksum_mismatch" | "invalid_deb" | "not_found"
        | "identity_conflict" | "identity_in_use" | "identity_missing" => {
            AppError::bad_request(message)
        }
        // The helper is not available or the operation failed on the host.
        _ => AppError::unavailable(message),
    }
}

/// One identity of the unprivileged application user.
#[derive(Debug, Clone)]
struct LinuxIdentity {
    uid: u32,
    gid: u32,
}

// ---------------------------------------------------------------------------
// Legacy Docker backend (migration)
// ---------------------------------------------------------------------------

/// One per-person data volume of a Docker-backed app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyVolume {
    pub user_id: i64,
    pub name: String,
}

/// The Docker operations a Docker-to-native migration needs.
///
/// Implemented by [`DockerLegacy`] over the Docker worker; tests use a fake.
#[async_trait]
pub trait LegacyDocker: Send + Sync {
    /// Every running and stopped legacy Linux app container.
    async fn list_containers(&self) -> Result<Value, String>;
    /// Stop and remove one person's copy of an app (its data volume stays).
    async fn stop_container(&self, app: &str, user_id: i64) -> Result<(), String>;
    /// The per-person data volumes of one app, with their owner ids.
    async fn volumes(&self, app: &str) -> Result<Vec<LegacyVolume>, String>;
    /// Copy one person's volume into the empty directory `dest` (read only).
    async fn volume_export(&self, app: &str, user_id: i64, dest: &Path) -> Result<Value, String>;
    /// Deliberately remove one person's retained volume (cleanup only).
    async fn volume_remove(&self, app: &str, user_id: i64) -> Result<(), String>;
}

/// Adapter over the Docker worker used by the migration methods.
pub struct DockerLegacy {
    docker: Arc<DockerManager>,
}

impl DockerLegacy {
    pub fn new(docker: Arc<DockerManager>) -> Self {
        Self { docker }
    }
}

/// Wrap the Docker worker as the migration backend.
pub fn docker_legacy(docker: Arc<DockerManager>) -> Arc<dyn LegacyDocker> {
    Arc::new(DockerLegacy { docker })
}

fn docker_message(error: &DockerError) -> String {
    match error {
        DockerError::Failed(message) | DockerError::BadRequest(message) => message.clone(),
        other => other.to_string(),
    }
}

#[async_trait]
impl LegacyDocker for DockerLegacy {
    async fn list_containers(&self) -> Result<Value, String> {
        self.docker
            .linux_list()
            .await
            .map_err(|e| docker_message(&e))
    }

    async fn stop_container(&self, app: &str, user_id: i64) -> Result<(), String> {
        self.docker
            .linux_stop(app, user_id)
            .await
            .map(|_| ())
            .map_err(|e| docker_message(&e))
    }

    async fn volumes(&self, app: &str) -> Result<Vec<LegacyVolume>, String> {
        let value = self
            .docker
            .linux_volume_list(app)
            .await
            .map_err(|e| docker_message(&e))?;
        Ok(crate::docker::linux::legacy_volume_names(&value)
            .into_iter()
            .map(|(user_id, name)| LegacyVolume { user_id, name })
            .collect())
    }

    async fn volume_export(&self, app: &str, user_id: i64, dest: &Path) -> Result<Value, String> {
        self.docker
            .linux_volume_export(app, user_id, dest)
            .await
            .map_err(|e| docker_message(&e))
    }

    async fn volume_remove(&self, app: &str, user_id: i64) -> Result<(), String> {
        self.docker
            .linux_volume_remove(app, user_id)
            .await
            .map(|_| ())
            .map_err(|e| docker_message(&e))
    }
}

/// The Docker backend is not wired: migration methods return an actionable
/// error instead of pretending to work.
struct UnavailableLegacy;

fn legacy_unavailable() -> String {
    "The Docker backend is not available to the native app service, so legacy apps cannot be \
     migrated or rolled back. Wire it with NativeApps::with_legacy."
        .to_string()
}

#[async_trait]
impl LegacyDocker for UnavailableLegacy {
    async fn list_containers(&self) -> Result<Value, String> {
        Err(legacy_unavailable())
    }
    async fn stop_container(&self, _app: &str, _user_id: i64) -> Result<(), String> {
        Err(legacy_unavailable())
    }
    async fn volumes(&self, _app: &str) -> Result<Vec<LegacyVolume>, String> {
        Err(legacy_unavailable())
    }
    async fn volume_export(
        &self,
        _app: &str,
        _user_id: i64,
        _dest: &Path,
    ) -> Result<Value, String> {
        Err(legacy_unavailable())
    }
    async fn volume_remove(&self, _app: &str, _user_id: i64) -> Result<(), String> {
        Err(legacy_unavailable())
    }
}

// ---------------------------------------------------------------------------
// Database row
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
struct NativeRow {
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
    visibility: String,
    created_at: i64,
    updated_at: i64,
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
    #[allow(dead_code)]
    migrated_at: Option<i64>,
    docker_volume: Option<String>,
}

const ROW_COLUMNS: &str = "id, name, format, file_name, version, package, description, command, \
     launch_guess, icon_type, build, status, error, job_id, update_kind, update_ref, file_sha1, \
     latest_version, checked_at, check_error, memory_mb, cpus, size_bytes, visibility, created_at, \
     updated_at, backend, install_path, previous_path, launcher, architecture, package_version, \
     package_preexisting, dependencies, packages_added, compatibility, install_source, \
     migration_state, migration_error, migration_backup, migrated_at, docker_volume";

fn parse_json(raw: Option<&str>) -> Value {
    raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null)
}

fn parse_json_array(raw: Option<&str>) -> Value {
    match parse_json(raw) {
        Value::Array(items) => Value::Array(items),
        _ => Value::Array(Vec::new()),
    }
}

fn value_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl NativeRow {
    fn format(&self) -> LinuxFormat {
        LinuxFormat::parse(&self.format).unwrap_or(LinuxFormat::Deb)
    }

    fn is_native(&self) -> bool {
        self.backend == "native"
    }

    fn update_available(&self) -> bool {
        match (&self.latest_version, &self.version) {
            (Some(latest), Some(current)) => updates::is_newer(latest, current),
            (Some(latest), None) => latest == updates::NEWER_BUILD,
            _ => false,
        }
    }

    fn launcher_value(&self) -> Value {
        parse_json(self.launcher.as_deref())
    }

    fn migration_value(&self) -> Value {
        match self.migration_state.as_deref() {
            Some(state) => json!({
                "state": state,
                "error": self.migration_error,
                "backup": parse_json(self.migration_backup.as_deref()),
            }),
            None => Value::Null,
        }
    }

    fn native_value(&self) -> Value {
        if !self.is_native() {
            return Value::Null;
        }
        json!({
            "installPath": self.install_path,
            "previousPath": self.previous_path,
            "launcher": self.launcher_value(),
            "architecture": self.architecture,
            "packageVersion": self.package_version,
            "preexistingPackage": self.package_preexisting.map(|value| value != 0),
            "dependencies": parse_json_array(self.dependencies.as_deref()),
            "packagesAdded": parse_json_array(self.packages_added.as_deref()),
            "compatibility": parse_json(self.compatibility.as_deref()),
            "installSource": self.install_source,
            "migration": self.migration_value(),
        })
    }

    fn update_value(&self) -> Value {
        json!({
            "source": self.update_kind,
            "latestVersion": self.latest_version,
            "available": self.update_available(),
            "checkedAt": self.checked_at,
            "error": self.check_error,
        })
    }

    /// The JSON shape the API returns for an app: exactly the keys of the
    /// existing `AppRow::json` plus `backend` and `native`.
    fn json(&self, running: usize) -> Value {
        let command = parse_json(self.command.as_deref());
        let command = if command.is_array() {
            command
        } else {
            Value::Array(Vec::new())
        };
        json!({
            "id": self.id,
            "desktopId": format!("linux:{}", self.id),
            "name": self.name,
            "format": self.format(),
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
            "update": self.update_value(),
            "memoryMb": self.memory_mb,
            "cpus": self.cpus,
            "sizeBytes": self.size_bytes,
            "visibility": self.visibility,
            "running": running,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "backend": self.backend,
            "native": self.native_value(),
        })
    }
}

// ---------------------------------------------------------------------------
// Prepared installs
// ---------------------------------------------------------------------------

/// A staged `.deb` that passed the read-only preview.
struct DebPrepared {
    preview: Value,
    packages_before: BTreeSet<String>,
}

/// A portable archive or AppImage unpacked in a temporary staging directory.
struct PortablePrepared {
    /// Removes the staging directory when the install/update finishes.
    _work: TempWork,
    root: PathBuf,
    discovery: native_archive::Discovery,
    version: Option<String>,
    architecture: Option<String>,
    update_kind: Option<String>,
    update_ref: Option<Value>,
    file_sha1: Option<String>,
    #[allow(dead_code)]
    is_appimage: bool,
}

enum Prepared {
    Deb(DebPrepared),
    Portable(Box<PortablePrepared>),
}

/// Everything recorded about a successfully installed app.
#[derive(Debug, Clone)]
struct AppMetadata {
    name: String,
    version: Option<String>,
    package: Option<String>,
    description: Option<String>,
    command: Value,
    launch_guess: bool,
    icon: Option<(String, Vec<u8>)>,
    launcher: Value,
    architecture: Option<String>,
    package_version: Option<String>,
    package_preexisting: Option<bool>,
    dependencies: Option<Value>,
    packages_added: Option<Value>,
    compatibility: Value,
    update_kind: Option<String>,
    update_ref: Option<Value>,
    file_sha1: Option<String>,
    size_bytes: Option<u64>,
    install_path: Option<PathBuf>,
}

/// A temporary, private directory that is removed when it goes out of scope.
struct TempWork {
    path: PathBuf,
}

impl TempWork {
    fn create(parent: &Path, prefix: &str) -> AppResult<Self> {
        let path = parent.join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&path).map_err(|error| {
            AppError::internal(format!("cannot prepare a staging folder: {error}"))
        })?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            AppError::internal(format!("cannot secure a staging folder: {error}"))
        })?;
        Ok(Self { path })
    }
}

impl Drop for TempWork {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Native Linux app installation service.
pub struct NativeApps {
    config: Arc<Config>,
    db: SqlitePool,
    helper: Arc<dyn HostHelper>,
    events: EventBus,
    legacy: Arc<dyn LegacyDocker>,
    /// Serialises package-manager based installs of this server, so two
    /// installs cannot interleave their apt preview/install sequences.
    package_lock: tokio::sync::Mutex<()>,
}

impl NativeApps {
    /// Build the service for a server without a Docker backend (migration
    /// methods then return an actionable error).
    pub fn new<H: HostHelper + 'static>(
        config: Arc<Config>,
        db: SqlitePool,
        helper: H,
        events: EventBus,
    ) -> Arc<Self> {
        Self::build(
            config,
            db,
            Arc::new(helper),
            events,
            Arc::new(UnavailableLegacy),
        )
    }

    /// Build the service with the legacy Docker backend available for
    /// Docker-to-native migrations and rollbacks.
    pub fn with_legacy<H: HostHelper + 'static>(
        config: Arc<Config>,
        db: SqlitePool,
        helper: H,
        events: EventBus,
        legacy: Arc<dyn LegacyDocker>,
    ) -> Arc<Self> {
        Self::build(config, db, Arc::new(helper), events, legacy)
    }

    fn build(
        config: Arc<Config>,
        db: SqlitePool,
        helper: Arc<dyn HostHelper>,
        events: EventBus,
        legacy: Arc<dyn LegacyDocker>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            helper,
            events,
            legacy,
            package_lock: tokio::sync::Mutex::new(()),
        })
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    fn ensure_enabled(&self) -> AppResult<()> {
        if !cfg!(target_os = "linux") {
            return Err(AppError::unavailable(
                "Linux apps run on Linux servers only",
            ));
        }
        if !self.config.native_enabled {
            return Err(AppError::unavailable(
                "Native Linux apps are disabled in this server's configuration.",
            ));
        }
        Ok(())
    }

    async fn row(&self, id: &str) -> AppResult<NativeRow> {
        sqlx::query_as::<_, NativeRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {ROW_COLUMNS} FROM linux_apps WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(AppError::NotFound("Linux app not found"))
    }

    async fn running_count(&self, id: &str) -> usize {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT user_id) FROM linux_runs WHERE app_id = ? \
             AND status IN ('launching', 'running')",
        )
        .bind(id)
        .fetch_one(&self.db)
        .await
        .unwrap_or(0) as usize
    }

    async fn json_by_id(&self, id: &str) -> AppResult<Value> {
        let row = self.row(id).await?;
        let running = self.running_count(id).await;
        Ok(row.json(running))
    }

    fn announce(&self, id: &str) {
        self.events.publish(super::APPS_EVENT, json!({"id": id}));
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

    async fn set_migration_state(&self, id: &str, state: Option<&str>, error: Option<&str>) {
        let _ = sqlx::query(
            "UPDATE linux_apps SET migration_state = ?, migration_error = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(state)
        .bind(error)
        .bind(db::now_ms())
        .bind(id)
        .execute(&self.db)
        .await;
        self.announce(id);
    }

    async fn new_id(&self, file_name: &str) -> AppResult<String> {
        let base = super::id_from_file(file_name);
        for attempt in 1..1000 {
            let candidate = if attempt == 1 {
                base.clone()
            } else {
                format!("{base}-{attempt}")
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

    fn staging_root(&self) -> AppResult<PathBuf> {
        let linux_apps = self.config.linux_apps_dir();
        let dir = self.config.linux_staging_dir();
        for path in [&linux_apps, &dir] {
            fs::create_dir_all(path).map_err(|error| staging_error(path, &error))?;
        }
        Ok(dir)
    }

    /// Copy an uploaded File Station file (or a downloaded temporary file)
    /// into the server's private staging area and stage it.
    ///
    /// The privileged helper only accepts package files below
    /// `<data_dir>/linux-apps`, so the copy is required before an install; the
    /// returned directory must stay alive until the job finishes and is then
    /// removed by the caller.
    pub async fn copy_into_staging(
        &self,
        source: &Path,
        file_name: &str,
    ) -> AppResult<(StagedFile, PathBuf)> {
        validate_file_name(file_name)?;
        let staging = self.staging_root()?;
        let source = source.to_path_buf();
        let file_name = file_name.to_string();
        tokio::task::spawn_blocking(move || -> AppResult<(StagedFile, PathBuf)> {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let dir = staging.join(format!("stage-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&dir).map_err(|error| {
                AppError::internal(format!("cannot prepare the staging folder: {error}"))
            })?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
                AppError::internal(format!("cannot secure the staging folder: {error}"))
            })?;
            let target = dir.join(&file_name);
            let mut input = crate::storage::safe_path::open_read_no_follow(&source)
                .map_err(|_| AppError::NotFound("path not found"))?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o640)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&target)
                .map_err(|error| {
                    AppError::internal(format!("cannot copy the app file: {error}"))
                })?;
            let mut buffer = vec![0u8; 1024 * 1024];
            let mut total = 0u64;
            loop {
                let read = input.read(&mut buffer).map_err(|error| {
                    AppError::internal(format!("cannot read the app file: {error}"))
                })?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                if total > MAX_STAGED_BYTES {
                    let _ = fs::remove_dir_all(&dir);
                    return Err(AppError::bad_request(
                        "the file is larger than 4 GB; upload a smaller package",
                    ));
                }
                use std::io::Write as _;
                output.write_all(&buffer[..read]).map_err(|error| {
                    AppError::internal(format!("cannot copy the app file: {error}"))
                })?;
            }
            output.sync_all().map_err(|error| {
                AppError::internal(format!("cannot copy the app file: {error}"))
            })?;
            let staged = StagedFile::stage(&target, Some(&file_name))?;
            Ok((staged, dir))
        })
        .await?
    }

    async fn staged_verify(&self, staged: &StagedFile) -> AppResult<()> {
        let staged = staged.clone();
        tokio::task::spawn_blocking(move || staged.verify()).await?
    }

    /// Cancel flag of a job, or a flag that never fires outside a job.
    fn cancel_flag(job: Option<&JobContext>) -> Arc<std::sync::atomic::AtomicBool> {
        job.map(|job| job.cancel_flag())
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }

    // -----------------------------------------------------------------------
    // Helper operations
    // -----------------------------------------------------------------------

    async fn call_helper(&self, op: &str, params: Value) -> AppResult<Value> {
        self.helper
            .call(op, params)
            .await
            .map_err(|error| helper_error(op, error))
    }

    async fn deb_preview_call(&self, staged: &StagedFile) -> AppResult<Value> {
        self.call_helper(
            "deb_preview",
            json!({"path": staged.helper_path()?, "sha256": staged.helper_sha256()?}),
        )
        .await
    }

    async fn deb_install_call(&self, staged: &StagedFile) -> AppResult<Value> {
        self.call_helper(
            "deb_install",
            json!({"path": staged.helper_path()?, "sha256": staged.helper_sha256()?}),
        )
        .await
    }

    async fn deb_remove_call(&self, packages: &[String], purge: bool) -> Result<Value, HostError> {
        self.helper
            .call("deb_remove", json!({"packages": packages, "purge": purge}))
            .await
    }

    async fn package_policy(&self, package: &str) -> Result<Value, HostError> {
        self.helper
            .call("package_policy", json!({"package": package}))
            .await
    }

    async fn package_installed_list(&self, names: &[String]) -> AppResult<BTreeSet<String>> {
        let mut names: Vec<String> = names.iter().take(64).cloned().collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            return Ok(BTreeSet::new());
        }
        let value = self
            .call_helper("package_installed_list", json!({"names": names}))
            .await?;
        Ok(
            value_strings(value.get("installed").unwrap_or(&Value::Null))
                .into_iter()
                .collect(),
        )
    }

    async fn package_files(&self, package: &str) -> AppResult<Vec<PathBuf>> {
        let value = self
            .call_helper("package_files", json!({"package": package}))
            .await?;
        Ok(value_strings(value.get("files").unwrap_or(&Value::Null))
            .into_iter()
            .map(PathBuf::from)
            .collect())
    }

    async fn identity_ensure(&self, user_id: i64) -> AppResult<LinuxIdentity> {
        let value = self
            .call_helper("identity_ensure", json!({"userId": user_id}))
            .await?;
        let uid = value
            .get("uid")
            .and_then(Value::as_u64)
            .and_then(|uid| u32::try_from(uid).ok())
            .ok_or_else(|| AppError::internal("the native helper reported no uid"))?;
        let gid = value
            .get("gid")
            .and_then(Value::as_u64)
            .and_then(|gid| u32::try_from(gid).ok())
            .unwrap_or(uid);
        Ok(LinuxIdentity { uid, gid })
    }

    async fn path_chmod(&self, path: &Path, mode: u32, recursive: bool) -> AppResult<()> {
        self.call_helper(
            "path_chmod",
            json!({
                "path": path.to_string_lossy(),
                "modeOctal": mode,
                "recursive": recursive,
            }),
        )
        .await?;
        Ok(())
    }

    async fn path_chown(&self, path: &Path, uid: u32, gid: u32, recursive: bool) -> AppResult<()> {
        self.call_helper(
            "path_chown",
            json!({
                "path": path.to_string_lossy(),
                "uid": uid,
                "gid": gid,
                "recursive": recursive,
            }),
        )
        .await?;
        Ok(())
    }

    async fn path_remove(&self, path: &Path) -> AppResult<()> {
        self.call_helper("path_remove", json!({"path": path.to_string_lossy()}))
            .await?;
        Ok(())
    }

    async fn path_remove_quiet(&self, path: &Path) {
        if let Err(error) = self.path_remove(path).await {
            tracing::debug!(
                path = %path.display(),
                "could not remove a staged directory: {error}"
            );
        }
    }

    async fn apt_update(&self) -> AppResult<Value> {
        self.call_helper("apt_update", json!({})).await
    }

    async fn deb_download(
        &self,
        package: &str,
        version: Option<&str>,
        dest: &Path,
    ) -> AppResult<Value> {
        let mut params = json!({
            "package": package,
            "dest": dest.to_string_lossy(),
        });
        if let Some(version) = version {
            params["version"] = Value::String(version.to_string());
        }
        self.call_helper("deb_download", params).await
    }

    // -----------------------------------------------------------------------
    // Preview
    // -----------------------------------------------------------------------

    /// Inspect an uploaded/downloaded file without changing the system.
    ///
    /// For `.deb` packages this asks the privileged helper for the package
    /// metadata, the simulated transaction and the compatibility result. For
    /// archives and AppImages it unpacks the file in a private staging
    /// directory (never as root) and reports the launcher, icon, update source
    /// and the candidates an administrator can choose from.
    pub async fn preview(&self, staged: &StagedFile, format: LinuxFormat) -> AppResult<Value> {
        self.ensure_enabled()?;
        staged.validate()?;
        let mut response = match format {
            LinuxFormat::Deb => {
                let preview = self.deb_preview_call(staged).await?;
                self.deb_preview_json(staged, &preview, "file").await
            }
            LinuxFormat::Tarball | LinuxFormat::Appimage => {
                let prepared = self.prepare_portable(staged, format, None).await?;
                self.portable_preview_json(staged, format, &prepared)?
            }
        };
        // Detect a possible duplicate install so the App Center can offer the
        // existing app as an update target instead of installing it twice.
        let duplicate = self
            .duplicate_for_preview(format, &response)
            .await
            .unwrap_or(Value::Null);
        response["duplicate"] = duplicate;
        Ok(response)
    }

    /// Find an existing app that the uploaded package probably corresponds to.
    async fn duplicate_for_preview(
        &self,
        format: LinuxFormat,
        preview: &Value,
    ) -> AppResult<Value> {
        match format {
            LinuxFormat::Deb => {
                let Some(package) = preview
                    .pointer("/deb/package")
                    .and_then(Value::as_str)
                    .filter(|package| !package.is_empty())
                else {
                    return Ok(Value::Null);
                };
                let Some(existing) = self.find_duplicate_package(package).await? else {
                    return Ok(Value::Null);
                };
                let version = preview.pointer("/deb/version").and_then(Value::as_str);
                Ok(duplicate_json(&existing, version, "package"))
            }
            LinuxFormat::Tarball | LinuxFormat::Appimage => {
                let name = preview
                    .pointer("/app/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let program = preview
                    .pointer("/app/launcher/argv/0")
                    .and_then(Value::as_str)
                    .map(basename);
                let Some(existing) = self
                    .find_duplicate_portable(name, program.as_deref())
                    .await?
                else {
                    return Ok(Value::Null);
                };
                let version = preview.pointer("/app/version").and_then(Value::as_str);
                Ok(duplicate_json(&existing, version, "name"))
            }
        }
    }

    async fn find_duplicate_package(&self, package: &str) -> AppResult<Option<DuplicateApp>> {
        let row: Option<(String, String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT id, name, backend, version, status FROM linux_apps WHERE package = ? \
             ORDER BY CASE backend WHEN 'native' THEN 0 ELSE 1 END, updated_at DESC LIMIT 1",
        )
        .bind(package)
        .fetch_optional(&self.db)
        .await?;
        Ok(
            row.map(|(id, name, backend, version, status)| DuplicateApp {
                id,
                name,
                backend,
                version,
                status,
            }),
        )
    }

    async fn find_duplicate_portable(
        &self,
        name: &str,
        program: Option<&str>,
    ) -> AppResult<Option<DuplicateApp>> {
        type AppRow = (
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        );
        let wanted_name = simplify_name(name);
        let wanted_program = program.map(|program| simplify_name(&basename(program)));
        let rows: Vec<AppRow> = sqlx::query_as(
            "SELECT id, name, backend, version, status, command FROM linux_apps \
                 WHERE backend = 'native'",
        )
        .fetch_all(&self.db)
        .await?;
        for (id, existing_name, backend, version, status, command) in rows {
            let name_matches =
                !wanted_name.is_empty() && simplify_name(&existing_name) == wanted_name;
            let program_matches = wanted_program.as_deref().is_some_and(|wanted| {
                parse_json(command.as_deref())
                    .as_array()
                    .and_then(|argv| argv.first())
                    .and_then(Value::as_str)
                    .map(|program| simplify_name(&basename(program)) == wanted)
                    .unwrap_or(false)
            });
            if name_matches || program_matches {
                return Ok(Some(DuplicateApp {
                    id,
                    name: existing_name,
                    backend,
                    version,
                    status,
                }));
            }
        }
        Ok(None)
    }

    async fn deb_preview_json(&self, staged: &StagedFile, preview: &Value, source: &str) -> Value {
        let compatible = preview
            .get("compatible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reasons = value_strings(preview.get("reasons").unwrap_or(&Value::Null));
        let mut warnings = value_strings(preview.get("warnings").unwrap_or(&Value::Null));
        let package = preview
            .get("package")
            .and_then(Value::as_str)
            .map(str::to_string);
        let version = preview
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut description = preview
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let discovered = self.deb_preview_discovery(staged, preview).await;
        warnings.extend(discovered.warnings);
        if description.is_none() {
            description = discovered.comment.clone();
        }
        let name = discovered
            .name
            .clone()
            .or_else(|| package.clone())
            .unwrap_or_else(|| super::display_name_from_file(&staged.file_name));
        let has_repository = preview.get("repository").and_then(Value::as_str).is_some();
        let update = has_repository.then(|| {
            json!({"kind": "apt", "package": package, "latestVersion": preview.get("candidate")})
        });
        // Both the flat helper keys and the nested transaction shape the App
        // Center reads are provided.
        let mut deb = preview.clone();
        if let Some(object) = deb.as_object_mut() {
            object.insert(
                "transaction".to_string(),
                json!({
                    "installs": preview.get("installs").cloned().unwrap_or(json!([])),
                    "removals": preview.get("removals").cloned().unwrap_or(json!([])),
                    "upgrades": preview.get("upgrades").cloned().unwrap_or(json!([])),
                    "downloadBytes": preview.get("downloadBytes").cloned().unwrap_or(json!(0)),
                }),
            );
        }
        json!({
            "format": LinuxFormat::Deb,
            "fileName": staged.file_name,
            "sizeBytes": staged.size,
            "sha256": staged.sha256,
            "source": source,
            "app": {
                "name": name,
                "version": version,
                "package": package,
                "description": description,
                "icon": discovered.icon,
                "launcher": discovered.launcher,
                "launchGuess": false,
            },
            "candidates": [],
            "needsChoice": false,
            "compatibility": {
                "ok": compatible,
                "reasons": reasons,
                "warnings": warnings,
            },
            "deb": deb,
            "update": update,
            "warnings": warnings,
            "architecture": preview.get("architecture").cloned().unwrap_or(Value::Null),
            "hostArchitecture": host_architecture(),
            "hostDistribution": host_distribution(),
        })
    }

    /// What the preview can already say about the `.deb`'s launcher and icon.
    ///
    /// For a package the host already has installed, the real files are read.
    /// Otherwise the package's data archive is unpacked into a private
    /// temporary folder (never installed, never executed as root) and the
    /// desktop entry is inspected; if `dpkg-deb` is not available the preview
    /// says so in a warning instead of failing.
    async fn deb_preview_discovery(
        &self,
        staged: &StagedFile,
        preview: &Value,
    ) -> DebPreviewDiscovery {
        let mut warnings: Vec<String> = Vec::new();
        let package = preview
            .get("package")
            .and_then(Value::as_str)
            .map(str::to_string);
        let preexisting = preview
            .get("preexisting")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if preexisting {
            if let Some(package) = &package {
                match self.package_files(package).await {
                    Ok(files) => {
                        let discovery = native_archive::discover_installed_package(package, &files);
                        if let Some(launcher) = discovery.launcher {
                            return DebPreviewDiscovery {
                                launcher: launcher.to_json(),
                                icon: discovery
                                    .icon
                                    .as_ref()
                                    .map(|icon| icon.to_json())
                                    .unwrap_or(Value::Null),
                                name: launcher.name.clone(),
                                comment: launcher.comment.clone(),
                                warnings,
                            };
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "the installed package's files are not readable: {}",
                        error.public_message()
                    )),
                }
            }
        }
        match self.extract_deb_tree(staged).await {
            Ok(Some((root, _work))) => {
                let hint = super::id_from_file(&staged.file_name);
                let discovery = native_archive::discover_archive(&root, &hint);
                let icon = discovery
                    .icon
                    .as_ref()
                    .map(|icon| icon.to_json())
                    .unwrap_or(Value::Null);
                if let Some(entry) = discovery.entry.clone() {
                    let launcher = deb_entry_launcher_json(&root, &entry, discovery.chromium);
                    return DebPreviewDiscovery {
                        launcher,
                        icon,
                        name: entry.name.clone(),
                        comment: entry.comment.clone(),
                        warnings,
                    };
                }
                if let Some(launcher) = discovery.launcher.as_ref() {
                    return DebPreviewDiscovery {
                        launcher: relative_launcher_json_display(&root, launcher),
                        icon,
                        name: launcher.name.clone(),
                        comment: launcher.comment.clone(),
                        warnings,
                    };
                }
                warnings.push(native_archive::SOURCE_ARCHIVE_MESSAGE.to_string());
            }
            Ok(None) => warnings.push(
                "the package's launcher is detected during installation (dpkg-deb is not \
                 available on this server for an inspection-only preview)"
                    .to_string(),
            ),
            Err(error) => warnings.push(format!(
                "the package contents could not be inspected: {}",
                error.public_message()
            )),
        }
        DebPreviewDiscovery {
            launcher: Value::Null,
            icon: Value::Null,
            name: None,
            comment: None,
            warnings,
        }
    }

    /// Unpack a `.deb`'s data archive into a private temporary folder so its
    /// desktop entry can be inspected without installing anything.
    async fn extract_deb_tree(
        &self,
        staged: &StagedFile,
    ) -> AppResult<Option<(PathBuf, TempWork)>> {
        let work = TempWork::create(&self.staging_root()?, "deb-preview")?;
        let root = work.path.join("root");
        fs::create_dir_all(&root).map_err(|error| {
            AppError::unavailable(format!("cannot prepare a preview folder: {error}"))
        })?;
        let image = staged.path.clone();
        let target = root.clone();
        let outcome = tokio::task::spawn_blocking(move || run_dpkg_deb_extract(&image, &target))
            .await
            .map_err(|error| AppError::internal(error.to_string()))?;
        match outcome {
            Ok(()) => {}
            Err(message) if message == MISSING_DPKG_DEB => return Ok(None),
            Err(message) => {
                return Err(AppError::bad_request(format!(
                    "the package contents could not be inspected: {message}"
                )))
            }
        }
        if directory_size(&root) > MAX_PREVIEW_BYTES {
            return Err(AppError::bad_request(
                "the package contents are too large to inspect before installing",
            ));
        }
        Ok(Some((root, work)))
    }

    fn portable_preview_json(
        &self,
        staged: &StagedFile,
        format: LinuxFormat,
        prepared: &PortablePrepared,
    ) -> AppResult<Value> {
        let discovery = &prepared.discovery;
        let Some(launcher) = discovery.launcher.as_ref() else {
            return Err(AppError::bad_request(
                native_archive::SOURCE_ARCHIVE_MESSAGE,
            ));
        };
        let candidates: Vec<Value> = discovery
            .candidates
            .iter()
            .map(|candidate| relative_candidate_json(&prepared.root, candidate))
            .collect();
        let launcher_json = relative_launcher_json(&prepared.root, launcher);
        let name = launcher
            .name
            .clone()
            .or_else(|| super::version_from_file(&staged.file_name))
            .unwrap_or_else(|| super::display_name_from_file(&staged.file_name));
        let mut reasons: Vec<String> = Vec::new();
        if let Some(architecture) = &prepared.architecture {
            if architecture != host_architecture() {
                reasons.push(format!(
                    "this program is built for {architecture} but this server is {}",
                    host_architecture()
                ));
            }
        }
        let version = prepared
            .version
            .clone()
            .or_else(|| super::version_from_file(&staged.file_name));
        let update = prepared.update_ref.clone().unwrap_or(Value::Null);
        let mut warnings: Vec<String> = Vec::new();
        if discovery.ambiguous {
            warnings.push(
                "the archive contains more than one program; choose the one to start".to_string(),
            );
        }
        if discovery.source_like {
            warnings.push("the archive also contains source-code files".to_string());
        }
        let compatibility = json!({
            "ok": reasons.is_empty(),
            "reasons": reasons,
            "warnings": warnings,
        });
        let icon = discovery
            .icon
            .as_ref()
            .map(|icon| icon.to_json())
            .unwrap_or(Value::Null);
        Ok(json!({
            "format": format,
            "fileName": staged.file_name,
            "sizeBytes": staged.size,
            "sha256": staged.sha256,
            "source": "file",
            "app": {
                "name": name,
                "version": version,
                "package": Value::Null,
                "description": launcher.comment,
                "icon": icon,
                "launcher": launcher_json,
                "launchGuess": discovery.launch_guess,
            },
            "candidates": candidates,
            "needsChoice": discovery.ambiguous,
            "compatibility": compatibility,
            "deb": Value::Null,
            "update": update,
            "warnings": warnings,
            "architecture": prepared.architecture,
            "hostArchitecture": host_architecture(),
            "hostDistribution": host_distribution(),
        }))
    }

    // -----------------------------------------------------------------------
    // Install
    // -----------------------------------------------------------------------

    /// Install a staged file as a new app.
    ///
    /// `source` is `file` or `url`; `launcher_choice` selects one of the
    /// `candidates` returned by [`NativeApps::preview`] for portable archives
    /// that contain several programs.
    pub async fn install(
        &self,
        actor_id: i64,
        staged: &StagedFile,
        format: LinuxFormat,
        source: &str,
        job: &JobContext,
        launcher_choice: Option<&Value>,
    ) -> AppResult<Value> {
        self.ensure_enabled()?;
        let source = validate_source(source)?;
        self.staged_verify(staged).await?;
        job.progress(0, 3, "Checking the uploaded app").await;
        job.check_cancelled()?;

        // The row exists while the job runs, so App Center can show progress;
        // a failure from here on is recorded on the row, never silent.
        let id = self.new_id(&staged.file_name).await?;
        self.insert_installing(actor_id, &id, staged, format, source, &job.id)
            .await?;

        let prepared = match format {
            LinuxFormat::Deb => {
                let prepared = self.prepare_deb(staged, job).await;
                match prepared {
                    Ok(prepared) => Prepared::Deb(prepared),
                    Err(error) => {
                        self.cleanup_install_files(&id);
                        let message = error.public_message();
                        self.set_status(&id, "failed", Some(&message), None).await;
                        return Err(error);
                    }
                }
            }
            LinuxFormat::Tarball | LinuxFormat::Appimage => {
                match self.prepare_portable(staged, format, Some(job)).await {
                    Ok(prepared) => Prepared::Portable(Box::new(prepared)),
                    Err(error) => {
                        self.cleanup_install_files(&id);
                        let message = error.public_message();
                        self.set_status(&id, "failed", Some(&message), None).await;
                        return Err(error);
                    }
                }
            }
        };
        job.check_cancelled()?;
        job.progress(1, 3, "Installing the app").await;

        let outcome = self
            .finish_install(&id, staged, format, source, prepared, launcher_choice, job)
            .await;
        match outcome {
            Ok(metadata) => {
                if let Err(error) = self.apply_metadata(&id, &metadata, 1, None, source).await {
                    self.cleanup_install_files(&id);
                    let message = error.public_message();
                    self.set_status(&id, "failed", Some(&message), None).await;
                    return Err(error);
                }
                job.progress(3, 3, "Installed").await;
                self.announce(&id);
                self.json_by_id(&id).await
            }
            Err(error) => {
                self.cleanup_install_files(&id);
                let message = error.public_message();
                self.set_status(&id, "failed", Some(&message), None).await;
                Err(error)
            }
        }
    }

    async fn insert_installing(
        &self,
        actor_id: i64,
        id: &str,
        staged: &StagedFile,
        format: LinuxFormat,
        source: &str,
        job_id: &str,
    ) -> AppResult<()> {
        let now = db::now_ms();
        let memory = self.config.linux_app_memory_mb as i64;
        let cpus = self.config.linux_app_cpus as i64;
        sqlx::query(
            "INSERT INTO linux_apps (id, name, format, file_name, version, status, backend,
                 install_source, memory_mb, cpus, created_by, created_at, updated_at, job_id)
             VALUES (?, ?, ?, ?, ?, 'installing', 'native', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(super::display_name_from_file(&staged.file_name))
        .bind(format.as_str())
        .bind(&staged.file_name)
        .bind(super::version_from_file(&staged.file_name))
        .bind(source)
        .bind(memory)
        .bind(cpus)
        .bind(actor_id)
        .bind(now)
        .bind(now)
        .bind(job_id)
        .execute(&self.db)
        .await?;
        self.announce(id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_install(
        &self,
        app_id: &str,
        staged: &StagedFile,
        format: LinuxFormat,
        source: &str,
        prepared: Prepared,
        launcher_choice: Option<&Value>,
        job: &JobContext,
    ) -> AppResult<AppMetadata> {
        match prepared {
            Prepared::Deb(prepared) => {
                job.progress(2, 3, "Installing the package and its dependencies")
                    .await;
                let install = {
                    let _guard = self.package_lock.lock().await;
                    self.deb_install_call(staged).await?
                };
                self.deb_metadata(staged, source, &prepared, &install, None)
                    .await
            }
            Prepared::Portable(prepared) => {
                job.progress(2, 3, "Unpacking the app").await;
                let mut launcher = choose_initial_launcher(&prepared, launcher_choice)?;
                self.ensure_architecture(&prepared, &launcher)?;
                let final_dir = self.activate_portable(app_id, &prepared, &mut launcher, 1)?;
                let metadata =
                    self.portable_metadata(staged, format, source, &prepared, launcher, &final_dir);
                Ok(metadata)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Update
    // -----------------------------------------------------------------------

    /// Update an existing native app from a staged file or URL.
    ///
    /// The new version is staged and validated before the app's row is
    /// switched; the previous version directory is kept for rollback.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_from(
        &self,
        actor_id: i64,
        app_id: &str,
        staged: &StagedFile,
        format: LinuxFormat,
        source: &str,
        job: &JobContext,
        launcher_choice: Option<&Value>,
    ) -> AppResult<Value> {
        let _ = actor_id;
        self.ensure_enabled()?;
        let source = validate_source(source)?;
        let row = self.row(app_id).await?;
        if !row.is_native() {
            return Err(AppError::conflict(
                "This app still runs on the Docker backend. Migrate it to native first.",
            ));
        }
        if row.status != "ready" {
            return Err(AppError::conflict(
                "Wait until the current install or update finishes.",
            ));
        }
        if row.format() != format {
            return Err(AppError::bad_request(format!(
                "This app was installed as a {}; choose a {} file.",
                row.format().as_str(),
                row.format().as_str()
            )));
        }
        self.staged_verify(staged).await?;
        self.set_status(app_id, "updating", None, Some(&job.id))
            .await;

        let prepared = match format {
            LinuxFormat::Deb => self.prepare_deb(staged, job).await.map(Prepared::Deb),
            LinuxFormat::Tarball | LinuxFormat::Appimage => self
                .prepare_portable(staged, format, Some(job))
                .await
                .map(|prepared| Prepared::Portable(Box::new(prepared))),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let message = error.public_message();
                self.set_status(
                    app_id,
                    "ready",
                    Some(&format!("Update failed: {message}")),
                    None,
                )
                .await;
                return Err(error);
            }
        };
        let old_command = parse_json(row.command.as_deref());
        let outcome = match prepared {
            Prepared::Deb(prepared) => {
                let install = {
                    let _guard = self.package_lock.lock().await;
                    self.deb_install_call(staged).await
                };
                match install {
                    Ok(install) => {
                        self.deb_metadata(staged, source, &prepared, &install, Some(&row))
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
            Prepared::Portable(prepared) => {
                let chosen = match launcher_choice {
                    Some(choice) => {
                        launcher_from_choice(choice, &prepared.root, prepared.discovery.chromium)
                    }
                    None => choose_update_launcher(&prepared, &old_command),
                };
                match chosen {
                    Ok(mut launcher) => {
                        if let Err(error) = self.ensure_architecture(&prepared, &launcher) {
                            Err(error)
                        } else {
                            let build = row.build as u32 + 1;
                            match self.activate_portable(app_id, &prepared, &mut launcher, build) {
                                Ok(final_dir) => Ok(self.portable_metadata(
                                    staged, format, source, &prepared, launcher, &final_dir,
                                )),
                                Err(error) => Err(error),
                            }
                        }
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match outcome {
            Ok(metadata) => {
                let build = row.build as u32 + 1;
                let previous = row.install_path.clone();
                if let Err(error) = self
                    .apply_metadata(app_id, &metadata, build, previous.as_deref(), source)
                    .await
                {
                    let message = error.public_message();
                    self.set_status(
                        app_id,
                        "ready",
                        Some(&format!("Update failed: {message}")),
                        None,
                    )
                    .await;
                    return Err(error);
                }
                job.progress(3, 3, "Updated").await;
                self.announce(app_id);
                self.json_by_id(app_id).await
            }
            Err(error) => {
                let message = error.public_message();
                self.set_status(
                    app_id,
                    "ready",
                    Some(&format!("Update failed: {message}")),
                    None,
                )
                .await;
                Err(error)
            }
        }
    }

    /// Update from the app's upstream source (apt/GitHub/zsync).
    pub async fn update_from_upstream(
        &self,
        actor_id: i64,
        app_id: &str,
        job: &JobContext,
    ) -> AppResult<Value> {
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if !row.is_native() {
            return Err(AppError::conflict(
                "This app still runs on the Docker backend. Migrate it to native first.",
            ));
        }
        if row.status != "ready" {
            return Err(AppError::conflict(
                "Wait until the current install or update finishes.",
            ));
        }
        match row.update_kind.as_deref() {
            Some("apt") => {
                let package = row
                    .package
                    .clone()
                    .ok_or_else(|| AppError::conflict("This app has no package name recorded."))?;
                job.progress(0, 3, "Checking the package source").await;
                let _ = self.apt_update().await;
                let policy = self
                    .package_policy(&package)
                    .await
                    .map_err(|error| helper_error("package_policy", error))?;
                let installed = row
                    .version
                    .clone()
                    .or_else(|| {
                        policy
                            .get("installed")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let candidate = policy
                    .get("candidate")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        AppError::conflict(format!(
                            "The package {package} is no longer offered by this server's APT sources."
                        ))
                    })?;
                if !installed.is_empty() && !updates::is_newer(&candidate, &installed) {
                    let mut app = self.json_by_id(app_id).await?;
                    app["updated"] = Value::Bool(false);
                    app["message"] = Value::String(format!(
                        "The app is already at the newest version ({installed})."
                    ));
                    return Ok(app);
                }
                job.progress(1, 3, "Downloading the newest package").await;
                let work = TempWork::create(&self.staging_root()?, "update")?;
                let data = self
                    .deb_download(&package, Some(&candidate), &work.path)
                    .await?;
                let path = data.get("path").and_then(Value::as_str).ok_or_else(|| {
                    AppError::unavailable("the helper did not report the downloaded package")
                })?;
                let staged = StagedFile::stage(path, None)?;
                job.progress(2, 3, "Installing the newest package").await;
                self.update_from(
                    actor_id,
                    app_id,
                    &staged,
                    LinuxFormat::Deb,
                    "url",
                    job,
                    None,
                )
                .await
            }
            Some("github") | Some("zsync") => {
                if row.format() != LinuxFormat::Appimage {
                    return Err(AppError::conflict(
                        "This app's update source only applies to AppImages.",
                    ));
                }
                job.progress(0, 3, "Finding the newest version").await;
                let update = parse_json(row.update_ref.as_deref());
                let installed_sha1 = row.file_sha1.clone();
                let target = TempWork::create(&self.staging_root()?, "update")?;
                let target_path = target.path.join("package");
                let cancel = Self::cancel_flag(Some(job));
                let (file_name, download_path) = tokio::task::spawn_blocking(move || {
                    let latest = updates::latest(&update, installed_sha1.as_deref())?;
                    let target = target_path.clone();
                    updates::download(&latest, &target, &cancel)?;
                    Ok::<_, String>((latest.file_name, target))
                })
                .await
                .map_err(|error| AppError::internal(error.to_string()))?
                .map_err(AppError::unavailable)?;
                let staged = StagedFile::stage(&download_path, Some(&file_name))?;
                job.progress(2, 3, "Installing the newest version").await;
                self.update_from(
                    actor_id,
                    app_id,
                    &staged,
                    LinuxFormat::Appimage,
                    "url",
                    job,
                    None,
                )
                .await
            }
            _ => Err(AppError::conflict(
                "This app has no update source. Upload the newer version from the app's details.",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Update check
    // -----------------------------------------------------------------------

    /// Ask the update source for the newest version and record it.
    pub async fn check(&self, app_id: &str) -> AppResult<Value> {
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if !row.is_native() {
            return Err(AppError::conflict(
                "Docker-backed apps are checked by the Docker worker.",
            ));
        }
        if row.status != "ready" {
            return Err(AppError::conflict(
                "Wait until the install or update finishes.",
            ));
        }
        let (latest, error) = match row.update_kind.as_deref() {
            Some("apt") => self.check_apt(&row).await,
            Some("github") | Some("zsync") => self.check_appimage(&row).await,
            _ => (None, None),
        };
        let _ = sqlx::query(
            "UPDATE linux_apps SET latest_version = ?, checked_at = ?, check_error = ?, \
             updated_at = ? WHERE id = ?",
        )
        .bind(latest)
        .bind(db::now_ms())
        .bind(error)
        .bind(db::now_ms())
        .bind(app_id)
        .execute(&self.db)
        .await;
        self.announce(app_id);
        let row = self.row(app_id).await?;
        let available = row.update_available();
        let message = if row.update_kind.is_none() {
            Some("This app has no update source; upload a newer file or URL to update it.")
        } else if available {
            None
        } else {
            Some("The app is up to date.")
        };
        Ok(json!({
            "app": self.json_by_id(app_id).await?,
            "update": {
                "source": row.update_kind,
                "latestVersion": row.latest_version,
                "available": available,
                "checkedAt": row.checked_at,
                "error": row.check_error,
                "message": message,
            },
        }))
    }

    async fn check_apt(&self, row: &NativeRow) -> (Option<String>, Option<String>) {
        let Some(package) = row.package.clone() else {
            return (None, Some("The app's package name is unknown.".into()));
        };
        let refresh_error = self.apt_update().await.err();
        let policy = match self.package_policy(&package).await {
            Ok(policy) => policy,
            Err(error) => {
                return (
                    row.latest_version.clone(),
                    Some(helper_error("package_policy", error).public_message()),
                )
            }
        };
        let installed = policy
            .get("installed")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| row.version.clone());
        let Some(installed) = installed else {
            return (
                row.latest_version.clone(),
                Some("The package is no longer installed on this server.".into()),
            );
        };
        let candidate = policy
            .get("candidate")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| installed.clone());
        let latest = if updates::is_newer(&candidate, &installed) {
            candidate
        } else {
            installed
        };
        let error = refresh_error.map(|error| {
            format!(
                "The package sources could not be refreshed; the newest known version is {latest}: {}",
                error.public_message()
            )
        });
        (Some(latest), error)
    }

    async fn check_appimage(&self, row: &NativeRow) -> (Option<String>, Option<String>) {
        let update = parse_json(row.update_ref.as_deref());
        if !update.is_object() {
            return (
                row.latest_version.clone(),
                Some("The app's update source is missing.".into()),
            );
        }
        let installed_sha1 = row.file_sha1.clone();
        let installed = row.version.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            updates::latest(&update, installed_sha1.as_deref())
                .map(|latest| latest.version_label(installed.as_deref()))
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        match outcome {
            Ok(latest) => (latest, None),
            Err(error) => (row.latest_version.clone(), Some(error)),
        }
    }

    // -----------------------------------------------------------------------
    // Uninstall
    // -----------------------------------------------------------------------

    /// Remove a native app: packages only when RAST added them, managed
    /// directories, and (on request) everyone's profiles.
    ///
    /// Running sessions must be stopped by the caller before this is called.
    pub async fn uninstall(
        &self,
        actor_id: i64,
        app_id: &str,
        remove_data: bool,
    ) -> AppResult<Value> {
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if !row.is_native() {
            return Err(AppError::conflict(
                "This app still runs on the Docker backend; uninstall it through the Docker backend.",
            ));
        }
        if matches!(row.status.as_str(), "installing" | "updating") {
            return Err(AppError::conflict(
                "Wait until the current install or update finishes.",
            ));
        }
        let mut removed_packages: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        if row.format() == LinuxFormat::Deb && row.package_preexisting != Some(1) {
            let packages = value_strings(&parse_json_array(row.packages_added.as_deref()));
            let packages: Vec<String> = packages.into_iter().take(MAX_ADDED_PACKAGES).collect();
            if !packages.is_empty() {
                match self.deb_remove_call(&packages, false).await {
                    Ok(data) => {
                        removed_packages =
                            value_strings(data.get("removed").unwrap_or(&Value::Null))
                    }
                    Err(error) if error.code == "unsafe_removal" => {
                        return Err(AppError::conflict(format!(
                            "Cannot remove the packages RAST installed for this app because other \
                             software still needs them: {}",
                            error.message
                        )))
                    }
                    Err(error) => {
                        return Err(helper_error("deb_remove", error));
                    }
                }
            }
        }

        let profiles = self.profile_users(app_id).await;
        let mut removed_profiles: Vec<i64> = Vec::new();
        if remove_data {
            for user_id in &profiles {
                let path = self.profile_dir(app_id, *user_id);
                if let Err(error) = self.path_remove(&path).await {
                    warnings.push(format!(
                        "could not remove the app data of user {user_id}: {}",
                        error.public_message()
                    ));
                } else {
                    removed_profiles.push(*user_id);
                }
            }
        }
        self.cleanup_install_files(app_id);
        let _ = fs::remove_dir_all(self.config.linux_apps_dir().join(app_id));
        let _ = fs::remove_dir_all(self.config.linux_migration_dir().join(app_id));
        sqlx::query("DELETE FROM linux_apps WHERE id = ?")
            .bind(app_id)
            .execute(&self.db)
            .await?;
        self.announce(app_id);
        let _ = actor_id;
        Ok(json!({
            "ok": true,
            "appId": app_id,
            "removedPackages": removed_packages,
            "removedProfiles": removed_profiles,
            "warnings": warnings,
        }))
    }

    /// User ids that have a profile directory for `app_id`.
    async fn profile_users(&self, app_id: &str) -> Vec<i64> {
        let mut users: Vec<i64> = Vec::new();
        let root = self.config.linux_profiles_dir().join(app_id);
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                if let Ok(user_id) = entry.file_name().to_string_lossy().parse::<i64>() {
                    users.push(user_id);
                }
            }
        }
        if let Ok(rows) =
            sqlx::query_scalar::<_, i64>("SELECT DISTINCT user_id FROM linux_runs WHERE app_id = ?")
                .bind(app_id)
                .fetch_all(&self.db)
                .await
        {
            users.extend(rows);
        }
        if let Ok(rows) = sqlx::query_scalar::<_, i64>("SELECT user_id FROM linux_identities")
            .fetch_all(&self.db)
            .await
        {
            users.extend(rows);
        }
        users.sort_unstable();
        users.dedup();
        users
    }

    // -----------------------------------------------------------------------
    // Activation and metadata
    // -----------------------------------------------------------------------

    /// Move an unpacked app into its managed version directory and point the
    /// launcher inside it.
    fn activate_portable(
        &self,
        app_id: &str,
        prepared: &PortablePrepared,
        launcher: &mut native_archive::Launcher,
        build: u32,
    ) -> AppResult<PathBuf> {
        let target = self.install_target(app_id, prepared.version.as_deref(), build)?;
        fs::rename(&prepared.root, &target).map_err(|error| {
            AppError::internal(format!(
                "cannot move the app into its install folder: {error}"
            ))
        })?;
        if let Err(error) = native_archive::make_install_dir(&target) {
            let _ = fs::remove_dir_all(&target);
            return Err(AppError::internal(error));
        }
        launcher.rebase(&prepared.root, &target);
        if let Err(message) = native_archive::validate_launcher(&target, launcher) {
            let _ = fs::remove_dir_all(&target);
            return Err(AppError::bad_request(message));
        }
        Ok(target)
    }

    fn install_target(
        &self,
        app_id: &str,
        version: Option<&str>,
        build: u32,
    ) -> AppResult<PathBuf> {
        let apps_root = self.config.linux_app_install_dir();
        let app_dir = apps_root.join(app_id);
        fs::create_dir_all(&app_dir).map_err(|error| {
            AppError::internal(format!("cannot create the install folder: {error}"))
        })?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&apps_root, fs::Permissions::from_mode(0o755)).ok();
        fs::set_permissions(&app_dir, fs::Permissions::from_mode(0o755)).ok();
        let base = version
            .and_then(version_component)
            .unwrap_or_else(|| format!("build-{build}"));
        let mut target = app_dir.join(&base);
        let mut attempt = 1u32;
        while target.exists() {
            target = app_dir.join(format!("{base}-{attempt}"));
            attempt += 1;
            if attempt > 1000 {
                return Err(AppError::internal(
                    "cannot find an unused version folder for the app",
                ));
            }
        }
        Ok(target)
    }

    fn cleanup_install_files(&self, app_id: &str) {
        let _ = fs::remove_dir_all(self.config.linux_app_install_dir().join(app_id));
        let _ = fs::remove_dir_all(self.config.linux_apps_dir().join(app_id));
    }

    /// Check that the app's program matches the server's architecture.
    fn ensure_architecture(
        &self,
        prepared: &PortablePrepared,
        launcher: &native_archive::Launcher,
    ) -> AppResult<()> {
        let Some(program) = launcher.argv.first() else {
            return Err(AppError::bad_request("the app's launcher has no command"));
        };
        let architecture = native_archive::elf_architecture(Path::new(program))
            .map(str::to_string)
            .or_else(|| prepared.architecture.clone());
        if let Some(architecture) = architecture {
            if architecture != host_architecture() {
                return Err(AppError::bad_request(format!(
                    "This program is built for {architecture}, but this server is {}. Upload a \
                     build for {}.",
                    host_architecture(),
                    host_architecture()
                )));
            }
        }
        Ok(())
    }

    /// Store a successful install/update in the app's row.
    async fn apply_metadata(
        &self,
        id: &str,
        metadata: &AppMetadata,
        build: u32,
        previous_path: Option<&str>,
        source: &str,
    ) -> AppResult<()> {
        let (icon_type, icon) = match &metadata.icon {
            Some((kind, bytes)) => (Some(kind.clone()), Some(bytes.clone())),
            None => (None, None),
        };
        let package_preexisting =
            metadata
                .package_preexisting
                .map(|preexisting| if preexisting { 1i64 } else { 0i64 });
        sqlx::query(
            "UPDATE linux_apps SET name = ?, version = ?, package = ?, description = ?, \
             command = ?, launch_guess = ?, icon_type = COALESCE(?, icon_type), \
             icon = COALESCE(?, icon), build = ?, status = 'ready', error = NULL, job_id = NULL, \
             update_kind = ?, update_ref = ?, file_sha1 = COALESCE(?, file_sha1), \
             architecture = ?, package_version = ?, package_preexisting = ?, dependencies = ?, \
             packages_added = ?, compatibility = ?, install_path = ?, previous_path = ?, \
             launcher = ?, install_source = ?, size_bytes = ?, backend = 'native', \
             latest_version = NULL, checked_at = NULL, check_error = NULL, updated_at = ? \
             WHERE id = ?",
        )
        .bind(&metadata.name)
        .bind(&metadata.version)
        .bind(&metadata.package)
        .bind(&metadata.description)
        .bind(metadata.command.to_string())
        .bind(metadata.launch_guess)
        .bind(icon_type)
        .bind(icon)
        .bind(build as i64)
        .bind(&metadata.update_kind)
        .bind(metadata.update_ref.as_ref().map(Value::to_string))
        .bind(&metadata.file_sha1)
        .bind(&metadata.architecture)
        .bind(&metadata.package_version)
        .bind(package_preexisting)
        .bind(metadata.dependencies.as_ref().map(Value::to_string))
        .bind(metadata.packages_added.as_ref().map(Value::to_string))
        .bind(metadata.compatibility.to_string())
        .bind(
            metadata
                .install_path
                .as_ref()
                .map(|path| path.display().to_string()),
        )
        .bind(previous_path)
        .bind(metadata.launcher.to_string())
        .bind(source)
        .bind(metadata.size_bytes.map(|size| size as i64))
        .bind(db::now_ms())
        .bind(id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Preparation helpers
    // -----------------------------------------------------------------------

    async fn prepare_deb(&self, staged: &StagedFile, job: &JobContext) -> AppResult<DebPrepared> {
        job.progress(0, 3, "Checking the package").await;
        let preview = self.deb_preview_call(staged).await?;
        let compatible = preview
            .get("compatible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !compatible {
            return Err(AppError::bad_request(deb_incompatible_message(&preview)));
        }
        let names = transaction_package_names(&preview);
        let packages_before = self.package_installed_list(&names).await?;
        Ok(DebPrepared {
            preview,
            packages_before,
        })
    }

    async fn prepare_portable(
        &self,
        staged: &StagedFile,
        format: LinuxFormat,
        job: Option<&JobContext>,
    ) -> AppResult<PortablePrepared> {
        type PortableOutput = (
            PathBuf,
            native_archive::Discovery,
            Option<Value>,
            Option<String>,
            Option<String>,
        );
        let work = TempWork::create(&self.staging_root()?, "native")?;
        let path = staged.path.clone();
        let file_name = staged.file_name.clone();
        let hint = super::id_from_file(&staged.file_name);
        let cancel = Self::cancel_flag(job);
        let work_path = work.path.clone();
        let is_appimage = format == LinuxFormat::Appimage;
        let output = tokio::task::spawn_blocking(move || -> Result<PortableOutput, String> {
            let cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
            match format {
                LinuxFormat::Appimage => {
                    let run = native_archive::extract_appimage(
                        &path, &file_name, &work_path, &cancelled,
                    )?;
                    let update_kind = run
                        .update
                        .as_ref()
                        .and_then(|update| update.get("kind"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let version = run.discovery.entry.as_ref().and_then(|entry| {
                        entry
                            .appimage_version
                            .clone()
                            .or_else(|| entry.version.clone())
                    });
                    let (app_launcher, app_icon, app_entry, app_chromium) = (
                        Some(run.discovery.launcher.clone()),
                        run.discovery.icon.clone(),
                        run.discovery.entry.clone(),
                        run.discovery.chromium,
                    );
                    let discovery = native_archive::Discovery {
                        launcher: app_launcher,
                        icon: app_icon,
                        candidates: Vec::new(),
                        launch_guess: false,
                        ambiguous: false,
                        source_like: false,
                        chromium: app_chromium,
                        entry: app_entry,
                    };
                    Ok((run.root, discovery, run.update, update_kind, version))
                }
                _ => {
                    let tree = native_archive::extract_portable(
                        &path, &file_name, &work_path, &cancelled,
                    )?;
                    let discovery = native_archive::discover_archive(&tree.root, &hint);
                    Ok((tree.root, discovery, None, None, None))
                }
            }
        })
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
        match output {
            Ok((root, discovery, update, update_kind, version)) => {
                let architecture = native_archive::elf_architecture(&root)
                    .map(str::to_string)
                    .or_else(|| {
                        discovery
                            .launcher
                            .as_ref()
                            .and_then(|launcher| launcher.argv.first())
                            .and_then(|program| {
                                native_archive::elf_architecture(Path::new(program))
                            })
                            .map(str::to_string)
                    });
                let file_sha1 = if is_appimage {
                    sha1_file(&staged.path).ok().map(|(_, sha1)| sha1)
                } else {
                    None
                };
                let version = version.or_else(|| super::version_from_file(&staged.file_name));
                let update_ref = update;
                Ok(PortablePrepared {
                    _work: work,
                    root,
                    discovery,
                    version,
                    architecture,
                    update_kind,
                    update_ref,
                    file_sha1,
                    is_appimage,
                })
            }
            Err(message) => {
                if message == "cancelled" {
                    Err(AppError::Cancelled)
                } else {
                    Err(AppError::bad_request(message))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Metadata
    // -----------------------------------------------------------------------

    async fn deb_metadata(
        &self,
        staged: &StagedFile,
        _source: &str,
        prepared: &DebPrepared,
        install: &Value,
        previous: Option<&NativeRow>,
    ) -> AppResult<AppMetadata> {
        let preview = &prepared.preview;
        let package = install
            .get("package")
            .and_then(Value::as_str)
            .filter(|package| !package.is_empty())
            .map(str::to_string)
            .or_else(|| {
                preview
                    .get("package")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                AppError::unavailable("the package manager did not report the package name")
            })?;
        let files = self.package_files(&package).await?;
        let discovery = native_archive::discover_installed_package(&package, &files);
        let launcher = discovery.launcher.ok_or_else(|| {
            AppError::bad_request(format!(
                "The package {package} installs no desktop launcher, so RAST cannot open it."
            ))
        })?;
        let policy = self.package_policy(&package).await.unwrap_or(Value::Null);
        let version = install
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .map(str::to_string)
            .or_else(|| {
                preview
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                policy
                    .get("installed")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| previous.and_then(|row| row.package_version.clone()));
        let preexisting = install
            .get("preexisting")
            .and_then(Value::as_bool)
            .or_else(|| preview.get("preexisting").and_then(Value::as_bool))
            .or_else(|| {
                previous
                    .and_then(|row| row.package_preexisting)
                    .map(|value| value != 0)
            })
            .unwrap_or(false);

        // Which packages did this install add? The transaction's package set
        // minus the packages that were already installed before it ran.
        let mut transaction = transaction_package_names(install);
        for name in value_strings(install.get("packages").unwrap_or(&Value::Null)) {
            if !transaction.contains(&name) {
                transaction.push(name);
            }
        }
        let mut added: Vec<String> = transaction
            .iter()
            .filter(|name| !prepared.packages_before.contains(*name))
            .cloned()
            .collect();
        if !preexisting && !added.contains(&package) {
            added.push(package.clone());
        }
        // Keep the packages a previous install added: a dependency that is
        // still installed must remain removable when the app is removed.
        if let Some(previous) = previous {
            for name in value_strings(&parse_json_array(previous.packages_added.as_deref())) {
                if !added.contains(&name) {
                    added.push(name);
                }
            }
        }
        added.sort();
        added.dedup();
        added.truncate(MAX_ADDED_PACKAGES);

        let sources_added = value_strings(install.get("aptSourcesAdded").unwrap_or(&Value::Null));
        let has_repository = policy.get("repository").and_then(Value::as_str).is_some();
        let update_kind = if !sources_added.is_empty() || has_repository {
            Some("apt".to_string())
        } else {
            None
        };
        let name = launcher
            .name
            .clone()
            .unwrap_or_else(|| super::display_name_from_file(&staged.file_name));
        let description = launcher.comment.clone().or_else(|| {
            preview
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let compatibility = json!({
            "ok": preview.get("compatible").and_then(Value::as_bool).unwrap_or(false),
            "reasons": preview.get("reasons").cloned().unwrap_or(Value::Array(Vec::new())),
            "warnings": preview.get("warnings").cloned().unwrap_or(Value::Array(Vec::new())),
            "hostArchitecture": preview.get("nativeArch").cloned().unwrap_or(Value::Null),
        });
        let icon = discovery
            .icon
            .as_ref()
            .map(|icon| (icon.kind.to_string(), icon.bytes.clone()));
        Ok(AppMetadata {
            name,
            version: version.clone(),
            description,
            package: Some(package.clone()),
            command: json!(launcher.argv),
            launch_guess: false,
            icon,
            launcher: launcher.to_json(),
            architecture: preview
                .get("architecture")
                .and_then(Value::as_str)
                .map(str::to_string),
            package_version: version,
            package_preexisting: Some(preexisting),
            dependencies: preview.get("depends").cloned().filter(Value::is_array),
            packages_added: Some(json!(added)),
            compatibility,
            update_kind,
            update_ref: None,
            file_sha1: None,
            size_bytes: Some(staged.size),
            install_path: None,
        })
    }

    fn portable_metadata(
        &self,
        staged: &StagedFile,
        format: LinuxFormat,
        _source: &str,
        prepared: &PortablePrepared,
        launcher: native_archive::Launcher,
        final_dir: &Path,
    ) -> AppMetadata {
        let size_bytes = directory_size(final_dir);
        let name = launcher
            .name
            .clone()
            .or_else(|| super::version_from_file(&staged.file_name))
            .unwrap_or_else(|| super::display_name_from_file(&staged.file_name));
        let version = prepared
            .version
            .clone()
            .or_else(|| super::version_from_file(&staged.file_name));
        let _ = format;
        AppMetadata {
            name,
            version,
            package: None,
            description: launcher.comment.clone(),
            command: json!(launcher.argv),
            launch_guess: prepared.discovery.launch_guess,
            icon: prepared
                .discovery
                .icon
                .as_ref()
                .map(|icon| (icon.kind.to_string(), icon.bytes.clone())),
            launcher: launcher.to_json(),
            architecture: prepared.architecture.clone(),
            package_version: None,
            package_preexisting: None,
            dependencies: None,
            packages_added: None,
            compatibility: json!({"ok": true, "reasons": [], "warnings": []}),
            update_kind: prepared.update_kind.clone(),
            update_ref: prepared.update_ref.clone(),
            file_sha1: prepared.file_sha1.clone(),
            size_bytes: Some(size_bytes),
            install_path: Some(final_dir.to_path_buf()),
        }
    }

    // -----------------------------------------------------------------------
    // Docker -> native migration
    // -----------------------------------------------------------------------

    /// Migrate one Docker-backed app to the native backend.
    ///
    /// `staged` is an optional replacement file the administrator uploaded
    /// (the original upload may be gone); `format` is derived from it when not
    /// given. The Docker data volumes are copied (never modified) and the
    /// containers are stopped only after the replacement source is validated.
    pub async fn migrate_legacy(
        &self,
        actor_id: i64,
        app_id: &str,
        staged: Option<&StagedFile>,
        format: Option<LinuxFormat>,
        job: &JobContext,
    ) -> AppResult<Value> {
        let _ = actor_id;
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if row.is_native() {
            return Err(AppError::conflict(
                "This app already runs on the native backend.",
            ));
        }
        if row.status != "ready" || row.build < 1 {
            return Err(AppError::conflict("Wait until the app is installed."));
        }
        match row.migration_state.as_deref() {
            Some("migrated") => return Err(AppError::conflict("This app is already migrated.")),
            Some("prepared") => {
                return Err(AppError::conflict(
                    "A migration of this app is already prepared. Retry after it finishes, or \
                     restart RAST.",
                ))
            }
            _ => {}
        }
        let format = match (staged, format) {
            (Some(staged), Some(format)) => {
                if format != row.format() {
                    return Err(AppError::bad_request(format!(
                        "This app is a {} app; the replacement file must be a {}.",
                        row.format().as_str(),
                        row.format().as_str()
                    )));
                }
                if LinuxFormat::from_file_name(&staged.file_name) != Some(format) {
                    return Err(AppError::bad_request(
                        "the replacement file does not match its declared format",
                    ));
                }
                format
            }
            (Some(staged), None) => LinuxFormat::from_file_name(&staged.file_name)
                .filter(|format| *format == row.format())
                .ok_or_else(|| {
                    AppError::bad_request(format!(
                        "the replacement file must be a {} file",
                        row.format().as_str()
                    ))
                })?,
            (None, Some(format)) if format == row.format() => format,
            (None, Some(_)) | (None, None) => row.format(),
        };
        if let Some(staged) = staged {
            staged.validate()?;
            self.staged_verify(staged).await?;
        }
        // Fail early when Docker is not wired instead of leaving a state behind.
        if let Err(message) = self.legacy.list_containers().await {
            let error = AppError::unavailable(message);
            self.set_migration_state(app_id, Some("failed"), Some(&error.public_message()))
                .await;
            return Err(error);
        }
        self.set_migration_state(app_id, Some("prepared"), None)
            .await;
        match self.run_migration(app_id, &row, format, staged, job).await {
            Ok((metadata, record)) => {
                let build = row.build as u32 + 1;
                if let Err(error) = self
                    .apply_metadata(app_id, &metadata, build, None, "migrated")
                    .await
                {
                    self.set_migration_state(app_id, Some("failed"), Some(&error.public_message()))
                        .await;
                    return Err(error);
                }
                let _ = sqlx::query(
                    "UPDATE linux_apps SET backend = 'native', migration_state = 'migrated', \
                     migration_error = NULL, migration_backup = ?, docker_volume = ?, \
                     migrated_at = ?, updated_at = ? WHERE id = ?",
                )
                .bind(record.backup.to_string())
                .bind(record.docker_volume.to_string())
                .bind(db::now_ms())
                .bind(db::now_ms())
                .bind(app_id)
                .execute(&self.db)
                .await;
                self.announce(app_id);
                self.json_by_id(app_id).await
            }
            Err(error) => {
                self.set_migration_state(app_id, Some("failed"), Some(&error.public_message()))
                    .await;
                Err(error)
            }
        }
    }

    /// The real migration work: clean up staged copies when a step fails.
    async fn run_migration(
        &self,
        app_id: &str,
        row: &NativeRow,
        format: LinuxFormat,
        replacement: Option<&StagedFile>,
        job: &JobContext,
    ) -> AppResult<(AppMetadata, MigrationRecord)> {
        let mut exported: Vec<PathBuf> = Vec::new();
        let mut installed_dir: Option<PathBuf> = None;
        let result = self
            .run_migration_steps(
                app_id,
                row,
                format,
                replacement,
                job,
                &mut exported,
                &mut installed_dir,
            )
            .await;
        if result.is_err() {
            for dir in &exported {
                self.path_remove_quiet(dir).await;
            }
            if let Some(dir) = &installed_dir {
                let _ = fs::remove_dir_all(dir);
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_migration_steps(
        &self,
        app_id: &str,
        row: &NativeRow,
        format: LinuxFormat,
        replacement: Option<&StagedFile>,
        job: &JobContext,
        exported_dirs: &mut Vec<PathBuf>,
        installed_dir: &mut Option<PathBuf>,
    ) -> AppResult<(AppMetadata, MigrationRecord)> {
        job.progress(0, 6, "Finding the original install file")
            .await;
        let source = self.migration_source(row, replacement, format, job).await?;
        job.check_cancelled()?;

        // Read the new version and validate it before touching anything.
        job.progress(1, 6, "Checking the app").await;
        let prepared = match format {
            LinuxFormat::Deb => Prepared::Deb(self.prepare_deb(&source.staged, job).await?),
            LinuxFormat::Tarball | LinuxFormat::Appimage => Prepared::Portable(Box::new(
                self.prepare_portable(&source.staged, format, Some(job))
                    .await?,
            )),
        };
        job.check_cancelled()?;

        let volumes = self
            .legacy
            .volumes(app_id)
            .await
            .map_err(AppError::unavailable)?;
        if volumes.len() > MAX_MIGRATION_USERS {
            return Err(AppError::unavailable(format!(
                "too many people use this app to migrate it at once ({} > {MAX_MIGRATION_USERS})",
                volumes.len()
            )));
        }
        let containers = self
            .legacy
            .list_containers()
            .await
            .map_err(AppError::unavailable)?;

        // A previous native run may have left app data in a profile. Without a
        // privileged backup/copy operation the migration cannot preserve it,
        // so refuse before changing anything and tell the admin what to do.
        let mut existing_profiles: Vec<i64> = Vec::new();
        for volume in &volumes {
            if fs::symlink_metadata(self.profile_dir(app_id, volume.user_id)).is_ok() {
                existing_profiles.push(volume.user_id);
            }
        }
        if !existing_profiles.is_empty() {
            let users = existing_profiles
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::conflict(format!(
                "The app already has native data for user(s) {users} from an earlier native run. \
                 Run the migration cleanup for this app first if you want to replace it with the \
                 Docker data; nothing was changed."
            )));
        }

        // Stop every running (and leftover) copy of the app.
        job.progress(2, 6, "Stopping running copies").await;
        for (app, user_id) in containers_of(&containers) {
            if app != app_id {
                continue;
            }
            self.legacy
                .stop_container(app_id, user_id)
                .await
                .map_err(|message| {
                    AppError::unavailable(format!(
                        "could not stop the running copy of user {user_id}: {message}"
                    ))
                })?;
        }
        job.check_cancelled()?;

        // Copy each person's Docker volume into a private migration folder.
        // The volume itself is read-only and never modified.
        job.progress(3, 6, "Copying people's app data").await;
        let migration_root = self.config.linux_migration_dir().join(app_id);
        let mut exported: Vec<(i64, String, PathBuf)> = Vec::new();
        for volume in &volumes {
            job.check_cancelled()?;
            let user_root = migration_root.join(volume.user_id.to_string());
            let dest = user_root.join("volume");
            if user_root.exists() {
                // A leftover from an interrupted attempt: the worker requires
                // an empty directory, so remove it (the helper can delete files
                // the worker created).
                let _ = fs::remove_dir_all(&user_root);
                if user_root.exists() {
                    self.path_remove_quiet(&user_root).await;
                }
            }
            fs::create_dir_all(&dest).map_err(|error| {
                AppError::internal(format!("cannot prepare the migration folder: {error}"))
            })?;
            self.legacy
                .volume_export(app_id, volume.user_id, &dest)
                .await
                .map_err(|message| {
                    AppError::unavailable(format!(
                        "could not copy the app data of user {}: {message}",
                        volume.user_id
                    ))
                })?;
            exported_dirs.push(dest.clone());
            exported.push((volume.user_id, volume.name.clone(), dest));
        }

        // Install the application files now that the data is safe.
        job.progress(4, 6, "Installing the native app").await;
        let metadata = match prepared {
            Prepared::Deb(prepared) => {
                let install = {
                    let _guard = self.package_lock.lock().await;
                    self.deb_install_call(&source.staged).await?
                };
                self.deb_metadata(&source.staged, "migrated", &prepared, &install, Some(row))
                    .await?
            }
            Prepared::Portable(prepared) => {
                let old_command = parse_json(row.command.as_deref());
                let mut launcher = choose_update_launcher(&prepared, &old_command)?;
                self.ensure_architecture(&prepared, &launcher)?;
                let build = row.build as u32 + 1;
                let final_dir = self.activate_portable(app_id, &prepared, &mut launcher, build)?;
                *installed_dir = Some(final_dir.clone());
                self.portable_metadata(
                    &source.staged,
                    format,
                    "migrated",
                    &prepared,
                    launcher,
                    &final_dir,
                )
            }
        };

        // Move the copies into the native profile layout and hand them to the
        // person's unprivileged identity.
        job.progress(5, 6, "Moving app data to the native profiles")
            .await;
        let mut profile_users: Vec<Value> = Vec::new();
        for (user_id, _volume_name, export_dir) in &exported {
            job.check_cancelled()?;
            let identity = self.identity_ensure(*user_id).await?;
            let profile = self.profile_dir(app_id, *user_id);
            self.create_profile_dirs(&profile)?;
            // The export belongs to the worker; make it readable for the
            // unprivileged server before copying it.
            self.path_chmod(export_dir, 0o755, true).await?;
            let export_clone = export_dir.clone();
            let profile_clone = profile.clone();
            tokio::task::spawn_blocking(move || {
                map_volume_into_profile(&export_clone, &profile_clone)
            })
            .await
            .map_err(|error| AppError::internal(error.to_string()))?
            .map_err(AppError::bad_request)?;
            self.path_chown(&profile, identity.uid, identity.gid, true)
                .await?;
            self.path_remove_quiet(export_dir).await;
            profile_users.push(json!({
                "userId": user_id,
                "profile": profile.display().to_string(),
            }));
        }

        let volumes_value: Vec<Value> = volumes
            .iter()
            .map(|volume| json!({"userId": volume.user_id, "name": volume.name}))
            .collect();
        let record = MigrationRecord {
            backup: json!({
                "volumes": volumes_value,
                "profiles": profile_users,
                "replacedProfiles": Vec::<i64>::new(),
                "source": metadata.install_path.as_ref().map(|path| path.display().to_string()),
            }),
            docker_volume: json!(volumes_value),
        };
        job.progress(6, 6, "Migrated").await;
        Ok((metadata, record))
    }

    /// The file to install the native app from during a migration.
    async fn migration_source(
        &self,
        row: &NativeRow,
        replacement: Option<&StagedFile>,
        format: LinuxFormat,
        job: &JobContext,
    ) -> AppResult<MigrationSource> {
        if let Some(staged) = replacement {
            return Ok(MigrationSource {
                staged: staged.clone(),
                _work: None,
            });
        }
        // (a) The file the Docker image was built from may still be staged.
        let legacy_path = self.config.linux_apps_dir().join(&row.id).join("package");
        let is_legacy_file = fs::symlink_metadata(&legacy_path)
            .map(|meta| meta.file_type().is_file() && !meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_legacy_file {
            let staged = StagedFile::stage(&legacy_path, Some(&row.file_name))?;
            return Ok(MigrationSource {
                staged,
                _work: None,
            });
        }
        // (b) `.deb` apps can be reinstalled from the host's APT sources.
        if row.update_kind.as_deref() == Some("apt") {
            if let Some(package) = row.package.clone() {
                job.progress(1, 6, "Downloading the package from the APT sources")
                    .await;
                let _ = self.apt_update().await;
                let policy = self
                    .package_policy(&package)
                    .await
                    .map_err(|error| helper_error("package_policy", error))?;
                if let Some(candidate) = policy
                    .get("candidate")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    let work = TempWork::create(&self.staging_root()?, "migrate")?;
                    let data = self
                        .deb_download(&package, Some(&candidate), &work.path)
                        .await?;
                    let path = data
                        .get("path")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .ok_or_else(|| {
                            AppError::unavailable(
                                "the helper did not report the downloaded package",
                            )
                        })?;
                    let staged = StagedFile::stage(&path, None)?;
                    return Ok(MigrationSource {
                        staged,
                        _work: Some(work),
                    });
                }
            }
        }
        // (c) AppImages can be fetched from their update source.
        if matches!(row.update_kind.as_deref(), Some("github") | Some("zsync"))
            && row.format() == LinuxFormat::Appimage
            && format == LinuxFormat::Appimage
        {
            job.progress(1, 6, "Downloading the app's current version")
                .await;
            let update = parse_json(row.update_ref.as_deref());
            let installed_sha1 = row.file_sha1.clone();
            let work = TempWork::create(&self.staging_root()?, "migrate")?;
            let target_path = work.path.join("package");
            let cancel = Self::cancel_flag(Some(job));
            let (file_name, download_path) = tokio::task::spawn_blocking(move || {
                let latest = updates::latest(&update, installed_sha1.as_deref())?;
                let target = target_path.clone();
                updates::download(&latest, &target, &cancel)?;
                Ok::<_, String>((latest.file_name, target))
            })
            .await
            .map_err(|error| AppError::internal(error.to_string()))?
            .map_err(AppError::unavailable)?;
            let staged = StagedFile::stage(&download_path, Some(&file_name))?;
            return Ok(MigrationSource {
                staged,
                _work: Some(work),
            });
        }
        // (d) Nothing to install from: say exactly what the admin can do.
        Err(AppError::conflict(format!(
            "The original install file ({}) is no longer on the server and this app has no \
             update source RAST can download from. Upload the file again from the app's \
             details and retry the migration; nothing was changed.",
            row.file_name
        )))
    }

    fn profile_dir(&self, app_id: &str, user_id: i64) -> PathBuf {
        self.config
            .linux_profiles_dir()
            .join(app_id)
            .join(user_id.to_string())
    }

    fn create_profile_dirs(&self, profile: &Path) -> AppResult<()> {
        use std::os::unix::fs::PermissionsExt;
        for subdir in ["home", "config", "data", "cache", "state", "tmp"] {
            let path = profile.join(subdir);
            fs::create_dir_all(&path).map_err(|error| {
                AppError::internal(format!("cannot prepare the app profile: {error}"))
            })?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).ok();
        }
        Ok(())
    }

    /// Roll a migrated app back to the Docker backend (data was retained).
    pub async fn rollback_migration(&self, actor_id: i64, app_id: &str) -> AppResult<Value> {
        let _ = actor_id;
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if row.migration_state.as_deref() != Some("migrated") {
            return Err(AppError::conflict(
                "Only a migrated app can be rolled back to the Docker backend.",
            ));
        }
        let volumes = self
            .legacy
            .volumes(app_id)
            .await
            .map_err(AppError::unavailable)?;
        let recorded = recorded_volumes(row.docker_volume.as_deref());
        let mut warnings: Vec<String> = Vec::new();
        for volume in &recorded {
            if !volumes.iter().any(|known| known.user_id == volume.user_id) {
                warnings.push(format!(
                    "the Docker data volume of user {} is missing; that person's old settings \
                     cannot come back",
                    volume.user_id
                ));
            }
        }
        sqlx::query(
            "UPDATE linux_apps SET backend = 'docker', migration_state = 'rolled_back', \
             updated_at = ? WHERE id = ?",
        )
        .bind(db::now_ms())
        .bind(app_id)
        .execute(&self.db)
        .await?;
        self.announce(app_id);
        Ok(json!({
            "ok": true,
            "appId": app_id,
            "backend": "docker",
            "migrationState": "rolled_back",
            "warnings": warnings,
            "app": self.json_by_id(app_id).await?,
        }))
    }

    /// Deliberate post-migration cleanup of retained Docker artifacts.
    pub async fn cleanup_migration(
        &self,
        actor_id: i64,
        app_id: &str,
        remove_backup: bool,
    ) -> AppResult<Value> {
        let _ = actor_id;
        self.ensure_enabled()?;
        let row = self.row(app_id).await?;
        if !matches!(
            row.migration_state.as_deref(),
            Some("migrated") | Some("rolled_back")
        ) {
            return Err(AppError::conflict(
                "Only a migrated or rolled-back app has migration data to clean up.",
            ));
        }
        if !remove_backup {
            return Ok(json!({
                "ok": true,
                "appId": app_id,
                "removedVolumes": [],
                "removedProfiles": [],
                "warnings": [],
            }));
        }
        let mut removed_volumes: Vec<String> = Vec::new();
        let mut removed_profiles: Vec<i64> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let recorded = recorded_volumes(row.docker_volume.as_deref());
        for volume in &recorded {
            match self.legacy.volume_remove(app_id, volume.user_id).await {
                Ok(()) => removed_volumes.push(volume.name.clone()),
                Err(message) => warnings.push(format!(
                    "could not remove the retained Docker volume of user {}: {message}",
                    volume.user_id
                )),
            }
        }
        // After a rollback the native profiles are retained backup data, not
        // live data: a deliberate cleanup removes them. A migrated app's live
        // profile is never removed.
        if row.backend != "native" {
            for volume in &recorded {
                let path = self.profile_dir(app_id, volume.user_id);
                let exists = fs::symlink_metadata(&path).is_ok();
                if !exists {
                    continue;
                }
                match self.path_remove(&path).await {
                    Ok(()) => removed_profiles.push(volume.user_id),
                    Err(error) => warnings.push(format!(
                        "could not remove the retained profile of user {}: {}",
                        volume.user_id,
                        error.public_message()
                    )),
                }
            }
        }
        let _ = fs::remove_dir_all(self.config.linux_migration_dir().join(app_id));
        sqlx::query(
            "UPDATE linux_apps SET migration_backup = NULL, docker_volume = NULL, updated_at = ? \
             WHERE id = ?",
        )
        .bind(db::now_ms())
        .bind(app_id)
        .execute(&self.db)
        .await?;
        self.announce(app_id);
        Ok(json!({
            "ok": true,
            "appId": app_id,
            "removedVolumes": removed_volumes,
            "removedProfiles": removed_profiles,
            "warnings": warnings,
        }))
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Validate the install source recorded on the row.
fn validate_source(source: &str) -> AppResult<&str> {
    match source {
        "file" | "url" | "migrated" => Ok(source),
        _ => Err(AppError::bad_request("invalid install source")),
    }
}

fn deb_incompatible_message(preview: &Value) -> String {
    let mut parts = value_strings(preview.get("reasons").unwrap_or(&Value::Null));
    if let Some(error) = preview.get("error").and_then(Value::as_str) {
        if !error.trim().is_empty() {
            parts.push(error.trim().to_string());
        }
    }
    if parts.is_empty() {
        "This package cannot be installed on this server.".to_string()
    } else {
        format!(
            "This package cannot be installed on this server: {}.",
            parts.join("; ")
        )
    }
}

/// Package names of an install transaction (`installs` objects or strings).
fn transaction_package_names(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(items) = value.get("installs").and_then(Value::as_array) {
        for item in items {
            match item {
                Value::String(name) => names.push(name.clone()),
                Value::Object(_) => {
                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                        names.push(name.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(package) = value.get("package").and_then(Value::as_str) {
        if !package.is_empty() && !names.contains(&package.to_string()) {
            names.push(package.to_string());
        }
    }
    names.sort();
    names.dedup();
    names.truncate(64);
    names
}

/// A sanitised directory component for a version (`1.2.3`, `v2`, …).
fn version_component(version: &str) -> Option<String> {
    let mut component: String = version
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+' | '_'))
        .take(48)
        .collect();
    while component.starts_with('.') {
        component.remove(0);
    }
    if component.is_empty() {
        None
    } else {
        Some(component)
    }
}

fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "i386",
        "arm" => "armhf",
        "powerpc64" => "ppc64el",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        other => other,
    }
}

fn host_distribution() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "PRETTY_NAME").then(|| value.trim().trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "unknown Linux distribution".to_string())
}

/// Size of a directory tree in bytes (symlinks are not followed).
fn directory_size(root: &Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(64)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| value.to_string())
}

/// A preview launcher with paths made relative to the app root.
fn relative_launcher_json(root: &Path, launcher: &native_archive::Launcher) -> Value {
    let mut argv = launcher.argv.clone();
    if let Some(first) = argv.first_mut() {
        *first = relative_program(root, first);
    }
    let working_dir = launcher.working_dir.as_ref().map(|dir| {
        if Path::new(dir).is_absolute() {
            relative_program(root, dir)
        } else {
            dir.clone()
        }
    });
    json!({
        "argv": argv,
        "desktopEntry": launcher.desktop_entry,
        "name": launcher.name,
        "comment": launcher.comment,
        "icon": launcher.icon,
        "terminal": launcher.terminal,
        "workingDir": working_dir,
        "chromium": launcher.chromium,
    })
}

fn relative_candidate_json(root: &Path, candidate: &native_archive::Candidate) -> Value {
    let mut argv = candidate.argv.clone();
    if let Some(first) = argv.first_mut() {
        *first = relative_program(root, first);
    }
    json!({
        "name": candidate.name,
        "argv": argv,
        "desktopEntry": candidate.desktop_entry,
    })
}

fn relative_program(root: &Path, program: &str) -> String {
    let path = Path::new(program);
    match path.strip_prefix(root) {
        Ok(rest) if !rest.as_os_str().is_empty() => rest.display().to_string(),
        _ => program.to_string(),
    }
}

/// Pick the launcher for a new install: the admin's explicit choice, the
/// detected default, or a clear error.
fn choose_initial_launcher(
    prepared: &PortablePrepared,
    choice: Option<&Value>,
) -> AppResult<native_archive::Launcher> {
    if let Some(choice) = choice {
        return launcher_from_choice(choice, &prepared.root, prepared.discovery.chromium);
    }
    if prepared.discovery.ambiguous {
        return Err(AppError::bad_request(
            "This archive contains more than one program; choose the program to start.",
        ));
    }
    prepared
        .discovery
        .launcher
        .clone()
        .ok_or_else(|| AppError::bad_request(native_archive::SOURCE_ARCHIVE_MESSAGE))
}

/// Build a launcher from the admin's selection JSON.
fn launcher_from_choice(
    choice: &Value,
    root: &Path,
    chromium: bool,
) -> AppResult<native_archive::Launcher> {
    let argv: Vec<String> = choice
        .get("argv")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let Some(program) = argv.first().cloned() else {
        return Err(AppError::bad_request("the chosen program has no command"));
    };
    let resolved = if Path::new(&program).is_absolute() {
        let resolved = fs::canonicalize(&program).map_err(|_| {
            AppError::bad_request(format!(
                "the chosen program {program} is not in the archive"
            ))
        })?;
        let canonical_root =
            fs::canonicalize(root).map_err(|error| AppError::internal(error.to_string()))?;
        if !resolved.starts_with(&canonical_root) {
            return Err(AppError::bad_request(
                "the chosen program is outside the archive",
            ));
        }
        resolved
    } else {
        native_archive::resolve_program(root, &program).ok_or_else(|| {
            AppError::bad_request(format!(
                "the chosen program {program} is not in the archive"
            ))
        })?
    };
    if !resolved.is_file() {
        return Err(AppError::bad_request("the chosen program is not a file"));
    }
    let mut argv = argv;
    argv[0] = resolved.display().to_string();
    let desktop_entry = choice
        .get("desktopEntry")
        .and_then(Value::as_str)
        .filter(|entry| native_archive::is_safe_relative(entry))
        .map(str::to_string);
    let name = choice
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(native_archive::Launcher {
        argv,
        desktop_entry,
        name,
        comment: None,
        icon: None,
        terminal: false,
        working_dir: None,
        chromium,
    })
}

/// Pick a launcher for an update by matching the app's previous command.
fn choose_update_launcher(
    prepared: &PortablePrepared,
    old_command: &Value,
) -> AppResult<native_archive::Launcher> {
    let old_program = old_command
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .map(basename);
    if let Some(old_program) = &old_program {
        if let Some(launcher) = &prepared.discovery.launcher {
            if launcher
                .argv
                .first()
                .map(|program| basename(program) == *old_program)
                .unwrap_or(false)
            {
                return Ok(launcher.clone());
            }
        }
        if let Some(candidate) = prepared.discovery.candidates.iter().find(|candidate| {
            candidate
                .argv
                .first()
                .map(|program| basename(program) == *old_program)
                .unwrap_or(false)
        }) {
            return Ok(native_archive::Launcher {
                argv: candidate.argv.clone(),
                desktop_entry: candidate.desktop_entry.clone(),
                name: candidate.name.clone(),
                comment: None,
                icon: None,
                terminal: false,
                working_dir: None,
                chromium: prepared.discovery.chromium,
            });
        }
    }
    if prepared.discovery.ambiguous {
        return Err(AppError::bad_request(
            "This archive contains more than one program and none matches the app's current \
             launcher. Upload an archive that starts the same program.",
        ));
    }
    prepared
        .discovery
        .launcher
        .clone()
        .ok_or_else(|| AppError::bad_request(native_archive::SOURCE_ARCHIVE_MESSAGE))
}

/// An existing app that an upload probably corresponds to.
#[derive(Debug, Clone)]
struct DuplicateApp {
    id: String,
    name: String,
    backend: String,
    version: Option<String>,
    status: String,
}

/// The `duplicate` object of a preview: the App Center offers the existing
/// app as the update target instead of creating a second install.
fn duplicate_json(
    existing: &DuplicateApp,
    uploaded_version: Option<&str>,
    matched_by: &str,
) -> Value {
    let can_update = match (uploaded_version, existing.version.as_deref()) {
        (Some(newer), Some(current)) => updates::is_newer(newer, current),
        (Some(_), None) => true,
        _ => false,
    };
    let reason = if can_update {
        Value::Null
    } else {
        Value::String(match (uploaded_version, existing.version.as_deref()) {
            (Some(newer), Some(current)) => format!(
                "the uploaded version {newer} is not newer than the installed version {current}"
            ),
            _ => "the versions cannot be compared; use the existing app's Update action only if                   this is a newer version"
                .to_string(),
        })
    };
    json!({
        "appId": existing.id,
        "name": existing.name,
        "backend": existing.backend,
        "version": existing.version,
        "status": existing.status,
        "matchedBy": matched_by,
        "canUpdate": can_update,
        "reason": reason,
    })
}

/// Case- and punctuation-insensitive app name for duplicate detection.
fn simplify_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// What the `.deb` preview can already say about the app.
#[derive(Debug, Clone)]
struct DebPreviewDiscovery {
    launcher: Value,
    icon: Value,
    name: Option<String>,
    comment: Option<String>,
    warnings: Vec<String>,
}

/// Largest data archive the preview unpacks (`dpkg-deb -x`).
const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Marker returned by the extractor when the tool is missing.
const MISSING_DPKG_DEB: &str = "missing:dpkg-deb";

/// The staging directory must be writable by the `rast` service account;
/// say exactly that when it is not.
fn staging_error(path: &Path, error: &std::io::Error) -> AppError {
    AppError::unavailable(format!(
        "cannot use the staging folder {}: {error}. The directory {} must be writable by the \
         rast service user (0750 rast:rast); check the ownership of the RAST data directory.",
        path.display(),
        path.parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_else(|| path.display().to_string())
    ))
}

/// `dpkg-deb -x <image> <target>`: unpack the data archive without installing.
fn run_dpkg_deb_extract(image: &Path, target: &Path) -> Result<(), String> {
    let output = std::process::Command::new("dpkg-deb")
        .arg("-x")
        .arg(image)
        .arg(target)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MISSING_DPKG_DEB.to_string())
        }
        Err(error) => return Err(format!("dpkg-deb could not be started: {error}")),
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("dpkg-deb failed");
    Err(line.to_string())
}

/// Preview launcher from an unpacked `.deb` desktop entry: an absolute `Exec`
/// program is the path the app will have after installation, so it is shown
/// as-is; a relative one is resolved inside the unpacked tree for display.
fn deb_entry_launcher_json(
    root: &Path,
    entry: &native_archive::DesktopEntry,
    chromium: bool,
) -> Value {
    let mut argv = native_archive::exec_argv(entry.exec.as_deref().unwrap_or(""));
    if let Some(program) = argv.first_mut() {
        if !Path::new(program.as_str()).is_absolute() {
            if let Some(resolved) = native_archive::resolve_program(root, program) {
                *program = relative_program(root, &resolved.display().to_string());
            }
        }
    }
    let desktop_entry = entry
        .path
        .strip_prefix(root)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| entry.path.display().to_string());
    json!({
        "argv": argv,
        "desktopEntry": desktop_entry,
        "name": entry.name,
        "comment": entry.comment,
        "icon": entry.icon,
        "terminal": entry.terminal,
        "workingDir": entry.working_dir,
        "chromium": chromium,
    })
}

/// A launcher with paths made relative for display (the tree is temporary).
fn relative_launcher_json_display(root: &Path, launcher: &native_archive::Launcher) -> Value {
    let mut argv = launcher.argv.clone();
    if let Some(first) = argv.first_mut() {
        *first = relative_program(root, first);
    }
    json!({
        "argv": argv,
        "desktopEntry": launcher.desktop_entry,
        "name": launcher.name,
        "comment": launcher.comment,
        "icon": launcher.icon,
        "terminal": launcher.terminal,
        "workingDir": launcher.working_dir,
        "chromium": launcher.chromium,
    })
}

/// `[{"userId":1,"name":"volume"}]` from the `docker_volume` column.
fn recorded_volumes(raw: Option<&str>) -> Vec<LegacyVolume> {
    let value = parse_json(raw);
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let user_id = item.get("userId").and_then(Value::as_i64)?;
                    let name = item.get("name").and_then(Value::as_str)?.to_string();
                    Some(LegacyVolume { user_id, name })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Containers of the legacy app from a `linux-list` response.
fn containers_of(value: &Value) -> Vec<(String, i64)> {
    value
        .get("containers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let app = item.get("app").and_then(Value::as_str)?.to_string();
                    let user_id = item.get("userId").and_then(Value::as_i64)?;
                    Some((app, user_id))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Copy one person's exported Docker volume (`/config`) into the native
/// profile layout. The Docker volume holds a container HOME, so the XDG
/// folders are mapped onto the profile's `config`/`data`/`cache`/`state`
/// directories and everything else goes into `home`. Symlinks are copied as
/// symlinks (never followed); special files abort the copy.
fn map_volume_into_profile(source: &Path, profile: &Path) -> Result<(u64, u64), String> {
    let mut stats = (0u64, 0u64);
    let home = profile.join("home");
    fs::create_dir_all(&home).map_err(|error| format!("cannot prepare the profile: {error}"))?;
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(source)
        .map_err(|error| format!("cannot read the saved app data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read the saved app data: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect the saved app data: {error}"))?;
        if meta.file_type().is_symlink() {
            copy_profile_symlink(&path, &home.join(&name))?;
            stats.0 += 1;
            continue;
        }
        if meta.is_file() {
            copy_profile_file(&path, &home.join(&name), &mut stats)?;
            continue;
        }
        if !meta.is_dir() {
            return Err(format!(
                "the saved app data contains a special file ({name}); the app cannot be migrated"
            ));
        }
        match name.as_str() {
            ".config" => merge_profile_dir(&path, &profile.join("config"), &mut stats)?,
            ".cache" => merge_profile_dir(&path, &profile.join("cache"), &mut stats)?,
            ".local" => {
                let mut children: Vec<fs::DirEntry> = fs::read_dir(&path)
                    .map_err(|error| format!("cannot read the saved app data: {error}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| format!("cannot read the saved app data: {error}"))?;
                children.sort_by_key(|entry| entry.file_name());
                for child in children {
                    let child_name = child.file_name().to_string_lossy().to_string();
                    let child_path = child.path();
                    let child_meta = fs::symlink_metadata(&child_path)
                        .map_err(|error| format!("cannot inspect the saved app data: {error}"))?;
                    if child_meta.is_dir() {
                        let target = match child_name.as_str() {
                            "share" => profile.join("data"),
                            "state" => profile.join("state"),
                            "cache" => profile.join("cache"),
                            _ => home.join(".local").join(&child_name),
                        };
                        merge_profile_dir(&child_path, &target, &mut stats)?;
                    } else if child_meta.is_file() {
                        copy_profile_file(
                            &child_path,
                            &home.join(".local").join(&child_name),
                            &mut stats,
                        )?;
                    } else if child_meta.file_type().is_symlink() {
                        copy_profile_symlink(&child_path, &home.join(".local").join(&child_name))?;
                        stats.0 += 1;
                    }
                }
            }
            _ => merge_profile_dir(&path, &home.join(&name), &mut stats)?,
        }
    }
    Ok(stats)
}

fn merge_profile_dir(source: &Path, dest: &Path, stats: &mut (u64, u64)) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dest).map_err(|error| format!("cannot prepare the profile: {error}"))?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o700)).ok();
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(source)
        .map_err(|error| format!("cannot read the saved app data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read the saved app data: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let target = dest.join(&name);
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect the saved app data: {error}"))?;
        if meta.file_type().is_symlink() {
            copy_profile_symlink(&path, &target)?;
            stats.0 += 1;
        } else if meta.is_dir() {
            merge_profile_dir(&path, &target, stats)?;
        } else if meta.is_file() {
            copy_profile_file(&path, &target, stats)?;
        } else {
            return Err(format!(
                "the saved app data contains a special file ({}); the app cannot be migrated",
                path.display()
            ));
        }
    }
    Ok(())
}

fn copy_profile_file(source: &Path, dest: &Path, stats: &mut (u64, u64)) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if stats.0 >= MAX_PROFILE_FILES as u64 || stats.1 >= MAX_PROFILE_BYTES {
        return Err("the saved app data is too large to migrate".to_string());
    }
    if let Ok(meta) = fs::symlink_metadata(dest) {
        if meta.is_dir() {
            return Err("the saved app data contains a file where a folder is".to_string());
        }
        fs::remove_file(dest).map_err(|error| format!("cannot replace a profile file: {error}"))?;
    }
    let bytes =
        fs::copy(source, dest).map_err(|error| format!("cannot copy the app data: {error}"))?;
    let mode = fs::metadata(source)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    fs::set_permissions(
        dest,
        fs::Permissions::from_mode(if mode { 0o755 } else { 0o644 }),
    )
    .ok();
    stats.0 += 1;
    stats.1 = stats.1.saturating_add(bytes);
    Ok(())
}

fn copy_profile_symlink(source: &Path, dest: &Path) -> Result<(), String> {
    let target =
        fs::read_link(source).map_err(|error| format!("cannot read a profile link: {error}"))?;
    if let Ok(meta) = fs::symlink_metadata(dest) {
        if meta.is_dir() {
            return Err("the saved app data contains a link where a folder is".to_string());
        }
        fs::remove_file(dest).map_err(|error| format!("cannot replace a profile link: {error}"))?;
    }
    std::os::unix::fs::symlink(&target, dest)
        .map_err(|error| format!("cannot create a profile link: {error}"))
}

// ---------------------------------------------------------------------------
// Migration internals
// ---------------------------------------------------------------------------

/// Where a migration copies the Docker volume data and what it retains.
#[derive(Debug, Clone)]
struct MigrationRecord {
    backup: Value,
    docker_volume: Value,
}

/// A staged file plus whatever keeps its staging directory alive.
struct MigrationSource {
    staged: StagedFile,
    _work: Option<TempWork>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};
    use crate::events::EventBus;
    use crate::jobs::{JobManager, JobSpec};
    use std::future::Future;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    // -----------------------------------------------------------------
    // Fakes
    // -----------------------------------------------------------------

    type Handler = Arc<dyn Fn(&str, &Value) -> Result<Value, HostError> + Send + Sync>;
    type CallLog = Arc<Mutex<Vec<(String, Value)>>>;

    #[derive(Clone)]
    struct FakeHelper {
        calls: CallLog,
        handler: Handler,
    }

    #[async_trait]
    impl HostHelper for FakeHelper {
        async fn call(&self, op: &str, params: Value) -> Result<Value, HostError> {
            self.calls
                .lock()
                .unwrap()
                .push((op.to_string(), params.clone()));
            (self.handler)(op, &params)
        }
    }

    impl FakeHelper {
        fn calls(&self, op: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(candidate, _)| candidate == op)
                .map(|(_, params)| params.clone())
                .collect()
        }
    }

    fn ok(data: Value) -> Result<Value, HostError> {
        Ok(data)
    }

    fn helper_failure(code: &str, message: &str) -> Result<Value, HostError> {
        Err(HostError {
            code: code.to_string(),
            message: message.to_string(),
        })
    }

    #[derive(Default)]
    struct LegacyState {
        volumes: Vec<LegacyVolume>,
        containers: Value,
        source: Option<PathBuf>,
        stopped: Vec<(String, i64)>,
        exported: Vec<(i64, PathBuf)>,
        removed: Vec<i64>,
    }

    struct FakeLegacy {
        state: Arc<Mutex<LegacyState>>,
    }

    impl FakeLegacy {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(Mutex::new(LegacyState {
                    containers: json!({"containers": []}),
                    ..LegacyState::default()
                })),
            })
        }

        fn state(&self) -> std::sync::MutexGuard<'_, LegacyState> {
            self.state.lock().unwrap()
        }
    }

    fn copy_tree(source: &Path, dest: &Path) -> Result<(), String> {
        fs::create_dir_all(dest).map_err(|error| error.to_string())?;
        for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let from = entry.path();
            let to = dest.join(entry.file_name());
            let meta = fs::symlink_metadata(&from).map_err(|error| error.to_string())?;
            if meta.is_dir() {
                copy_tree(&from, &to)?;
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(&from).map_err(|error| error.to_string())?;
                std::os::unix::fs::symlink(target, &to).map_err(|error| error.to_string())?;
            } else {
                fs::copy(&from, &to).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    #[async_trait]
    impl LegacyDocker for FakeLegacy {
        async fn list_containers(&self) -> Result<Value, String> {
            Ok(self.state().containers.clone())
        }
        async fn stop_container(&self, app: &str, user_id: i64) -> Result<(), String> {
            self.state().stopped.push((app.to_string(), user_id));
            Ok(())
        }
        async fn volumes(&self, _app: &str) -> Result<Vec<LegacyVolume>, String> {
            Ok(self.state().volumes.clone())
        }
        async fn volume_export(
            &self,
            _app: &str,
            user_id: i64,
            dest: &Path,
        ) -> Result<Value, String> {
            let source = self
                .state()
                .source
                .clone()
                .ok_or_else(|| "no export source configured".to_string())?;
            copy_tree(&source, dest)?;
            self.state().exported.push((user_id, dest.to_path_buf()));
            Ok(json!({"files": 1, "bytes": 10}))
        }
        async fn volume_remove(&self, _app: &str, user_id: i64) -> Result<(), String> {
            self.state().removed.push(user_id);
            Ok(())
        }
    }

    // -----------------------------------------------------------------
    // Test environment
    // -----------------------------------------------------------------

    struct TestEnv {
        _dir: tempfile::TempDir,
        config: Arc<Config>,
        db: SqlitePool,
        apps: Arc<NativeApps>,
        helper: FakeHelper,
        legacy: Arc<FakeLegacy>,
        jobs: Arc<JobManager>,
    }

    async fn test_env(handler: Handler) -> TestEnv {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(
            Config::from_parts(
                None,
                FileConfig {
                    data_dir: Some(dir.path().join("data")),
                    storage_dir: Some(dir.path().join("storage")),
                    native_enabled: Some(true),
                    ..FileConfig::default()
                },
                |_| None,
            )
            .unwrap(),
        );
        config.ensure_dirs().unwrap();
        let db = crate::db::connect(&config.db_path(), 2).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, display_name, password_hash, role, created_at, updated_at)
             VALUES ('admin', 'Admin', 'x', 'admin', 0, 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
        let helper = FakeHelper { calls, handler };
        let legacy = FakeLegacy::new();
        let events = EventBus::new();
        let jobs = JobManager::new(db.clone(), events.clone(), 1);
        let apps = NativeApps::with_legacy(
            config.clone(),
            db.clone(),
            helper.clone(),
            events,
            legacy.clone(),
        );
        TestEnv {
            _dir: dir,
            config,
            db,
            apps,
            helper,
            legacy,
            jobs,
        }
    }

    /// Run `run(apps, ctx)` inside a real job and return its result.
    async fn in_job<F, Fut>(env: &TestEnv, kind: &str, run: F) -> Result<Value, String>
    where
        F: FnOnce(Arc<NativeApps>, JobContext) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<Value>> + Send + 'static,
    {
        let apps = env.apps.clone();
        let (tx, rx) = oneshot::channel();
        let spec = JobSpec::new(kind, move |ctx: JobContext| async move {
            let outcome = run(apps, ctx).await;
            let _ = tx.send(match outcome {
                Ok(value) => Ok(value),
                Err(error) => Err(error.public_message()),
            });
            Ok::<Value, AppError>(json!({}))
        });
        env.jobs.submit(spec).await.expect("submit job");
        tokio::time::timeout(Duration::from_secs(20), rx)
            .await
            .expect("job timed out")
            .expect("job result")
    }

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    fn write_file(path: &Path, data: &[u8], mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn fake_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    /// A fake installed-package file list: a program, a desktop entry and an icon.
    fn deb_files(root: &Path) -> Vec<PathBuf> {
        let program = root.join("usr/bin/demo");
        write_file(&program, b"\x7fELF-fake-demo", 0o755);
        let desktop = root.join("usr/share/applications/demo.desktop");
        write_file(
            &desktop,
            b"[Desktop Entry]\nType=Application\nName=Demo\nComment=A demo app\n\
              Exec=demo --flag %U\nIcon=demo\n",
            0o644,
        );
        let icon = root.join("usr/share/icons/hicolor/128x128/apps/demo.png");
        write_file(&icon, &fake_png(128, 128), 0o644);
        vec![desktop, icon, program]
    }

    /// The canned helper answers for one `.deb` package.
    fn deb_handler(
        files: Arc<Vec<PathBuf>>,
        package: &'static str,
        version: &'static str,
        preexisting: bool,
        candidate: Arc<Mutex<String>>,
        unsafe_remove: Arc<AtomicBool>,
    ) -> Handler {
        Arc::new(move |op: &str, params: &Value| match op {
            "deb_preview" => ok(json!({
                "compatible": true,
                "package": package,
                "version": version,
                "architecture": "amd64",
                "nativeArch": "amd64",
                "description": "A demo app",
                "depends": ["libc6 (>= 2.31)"],
                "conflicts": [],
                "installs": [{"name": package, "version": version}],
                "removals": [],
                "upgrades": [],
                "downloadBytes": 1024,
                "warnings": [],
                "reasons": [],
                "preexisting": preexisting,
                "installedVersion": if preexisting { Value::String(version.to_string()) } else { Value::Null },
                "candidate": candidate.lock().unwrap().clone(),
                "repository": "https://example.org stable/main",
                "source": "https://example.org/stable",
                "error": Value::Null,
            })),
            "deb_install" => ok(json!({
                "packages": [package],
                "package": package,
                "version": version,
                "architecture": "amd64",
                "installs": [{"name": package, "version": version}],
                "removals": [],
                "preexisting": preexisting,
                "aptSourcesAdded": ["/etc/apt/sources.list.d/example.list"],
                "output": "Setting up demo",
            })),
            "package_policy" => ok(json!({
                "installed": version,
                "candidate": candidate.lock().unwrap().clone(),
                "architecture": "amd64",
                "repository": "https://example.org stable/main",
                "source": "https://example.org/stable",
            })),
            "package_installed_list" => {
                let names: Vec<Value> = params
                    .get("names")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let installed: Vec<Value> = if preexisting {
                    names
                        .into_iter()
                        .filter(|name| name.as_str() == Some(package))
                        .collect()
                } else {
                    Vec::new()
                };
                ok(json!({"installed": installed}))
            }
            "package_files" => ok(json!({
                "files": files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            })),
            "deb_remove" => {
                if unsafe_remove.load(Ordering::Relaxed) {
                    helper_failure(
                        "unsafe_removal",
                        "the transaction would also remove packages that were not requested: libc6",
                    )
                } else {
                    ok(
                        json!({"removed": params.get("packages").cloned().unwrap_or(Value::Array(Vec::new()))}),
                    )
                }
            }
            "apt_update" => ok(json!({"ok": true, "updated": true, "output": ""})),
            "identity_ensure" => {
                let user_id = params.get("userId").and_then(Value::as_i64).unwrap_or(1);
                ok(json!({
                    "userId": user_id,
                    "username": format!("rast-u{user_id}"),
                    "uid": 200000 + user_id,
                    "gid": 200000 + user_id,
                    "home": "/srv/rast/linux-users",
                    "created": true,
                }))
            }
            "path_chmod" | "path_chown" | "path_remove" => ok(json!({"ok": true})),
            _ => helper_failure("unsupported", &format!("unexpected helper op {op}")),
        })
    }

    fn stage(env: &TestEnv, source: &Path, file_name: &str) -> StagedFile {
        let staging = env.config.linux_staging_dir();
        fs::create_dir_all(&staging).unwrap();
        let dest = staging.join(file_name);
        fs::copy(source, &dest).unwrap();
        StagedFile::stage(&dest, Some(file_name)).unwrap()
    }

    /// Build a `.tar.gz` with one top-level directory.
    fn build_tar(path: &Path, top: &str, files: &[(&str, &[u8], u32)]) {
        use flate2::{write::GzEncoder, Compression as GzCompression};
        use std::io::Write;
        let mut plain = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut plain);
            let mut dir = tar::Header::new_old();
            dir.set_entry_type(tar::EntryType::Directory);
            dir.set_mode(0o755);
            dir.set_size(0);
            dir.set_cksum();
            builder
                .append_data(&mut dir, format!("{top}/"), io::empty())
                .unwrap();
            for (relative, data, mode) in files {
                let mut header = tar::Header::new_old();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(data.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("{top}/{relative}"), *data)
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = GzEncoder::new(fs::File::create(path).unwrap(), GzCompression::fast());
        encoder.write_all(&plain).unwrap();
        encoder.finish().unwrap();
    }

    fn demo_tar(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        let top = format!("demo-{version}");
        build_tar(
            &path,
            &top,
            &[
                ("bin/demo", b"\x7fELF-fake-demo", 0o755),
                (
                    "usr/share/applications/demo.desktop",
                    format!(
                        "[Desktop Entry]\nType=Application\nName=Demo {version}\n\
                         Exec=demo --version\n"
                    )
                    .as_bytes(),
                    0o644,
                ),
            ],
        );
        path
    }

    async fn insert_docker_row(
        env: &TestEnv,
        id: &str,
        format: LinuxFormat,
        update_kind: Option<&str>,
    ) {
        let now = db::now_ms();
        sqlx::query(
            "INSERT INTO linux_apps (id, name, format, file_name, version, status, backend, build,
                 package, update_kind, memory_mb, cpus, created_at, updated_at)
             VALUES (?, 'Demo', ?, 'demo_1.0_amd64.deb', '1.0', 'ready', 'docker', 1, 'demo', ?,
                 2048, 2, ?, ?)",
        )
        .bind(id)
        .bind(format.as_str())
        .bind(update_kind)
        .bind(now)
        .bind(now)
        .execute(&env.db)
        .await
        .unwrap();
    }

    async fn row_status(env: &TestEnv, id: &str) -> Option<(String, Option<String>, String)> {
        sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT status, error, backend FROM linux_apps WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&env.db)
        .await
        .unwrap()
    }

    async fn app_count(env: &TestEnv) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM linux_apps")
            .fetch_one(&env.db)
            .await
            .unwrap()
    }

    // -----------------------------------------------------------------
    // Staged files
    // -----------------------------------------------------------------

    #[test]
    fn staged_files_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("app.tar.gz");
        fs::write(&real, b"data").unwrap();
        let staged = StagedFile::stage(&real, Some("Demo-1.0.tar.gz")).unwrap();
        assert_eq!(staged.size, 4);
        assert_eq!(staged.sha256.len(), 64);
        assert!(staged.verify().is_ok());

        let link = dir.path().join("link.tar.gz");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = StagedFile::stage(&link, None).unwrap_err();
        assert!(error.public_message().contains("symbolic links"), "{error}");

        let error = StagedFile::stage(&real, Some("../evil.deb")).unwrap_err();
        let message = error.public_message();
        assert!(
            message.contains("unsupported characters") || message.contains("dot"),
            "{message}"
        );

        fs::write(&real, b"changed!").unwrap();
        let error = staged.verify().unwrap_err();
        assert!(error.public_message().contains("changed"), "{error}");
    }

    // -----------------------------------------------------------------
    // Preview
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn preview_deb_reports_the_transaction_without_creating_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let files = Arc::new(deb_files(dir.path()));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let preview = env.apps.preview(&staged, LinuxFormat::Deb).await.unwrap();
        assert_eq!(preview["format"], "deb");
        assert_eq!(preview["compatibility"]["ok"], true);
        assert_eq!(preview["deb"]["installs"][0]["name"], "demo");
        assert_eq!(preview["deb"]["architecture"], "amd64");
        assert_eq!(preview["update"]["kind"], "apt");
        assert_eq!(app_count(&env).await, 0, "preview must not create a row");
    }

    #[tokio::test]
    async fn preview_portable_reports_launcher_and_candidates() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = demo_tar(dir.path(), "Demo-1.0.tar.gz", "1.0");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        let preview = env
            .apps
            .preview(&staged, LinuxFormat::Tarball)
            .await
            .unwrap();
        assert_eq!(preview["format"], "tarball");
        assert_eq!(preview["app"]["name"], "Demo 1.0");
        assert_eq!(preview["app"]["launcher"]["argv"][0], "bin/demo");
        assert_eq!(preview["app"]["launchGuess"], false);
        assert_eq!(preview["needsChoice"], false);
        assert_eq!(app_count(&env).await, 0);
    }

    // -----------------------------------------------------------------
    // Install
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn files_are_copied_into_the_private_staging_area() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        // A File Station file lives outside the data directory; the helper only
        // accepts package files below <data>/linux-apps, so it must be copied.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Demo-1.0.tar.gz");
        fs::write(&source, b"archive bytes").unwrap();
        let (staged, stage_dir) = env
            .apps
            .copy_into_staging(&source, "Demo-1.0.tar.gz")
            .await
            .unwrap();
        assert!(staged.path.starts_with(env.config.linux_staging_dir()));
        assert_eq!(fs::read(&staged.path).unwrap(), b"archive bytes");
        assert!(stage_dir.is_dir());
        assert_eq!(staged.file_name, "Demo-1.0.tar.gz");
        assert!(staged.verify().is_ok());

        let error = env
            .apps
            .copy_into_staging(&source, "../evil")
            .await
            .unwrap_err();
        assert!(error.public_message().contains("dot"), "{error}");
    }

    #[tokio::test]
    async fn install_deb_creates_a_ready_native_row() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let env = test_env(deb_handler(
            files.clone(),
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let app = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        assert_eq!(app["id"], "demo");
        assert_eq!(app["backend"], "native");
        assert_eq!(app["status"], "ready");
        assert_eq!(app["error"], Value::Null);
        assert_eq!(app["package"], "demo");
        assert_eq!(app["native"]["packageVersion"], "1.0");
        assert_eq!(app["native"]["preexistingPackage"], false);
        assert_eq!(app["native"]["installSource"], "file");
        assert_eq!(app["native"]["packagesAdded"][0], "demo");
        assert_eq!(app["update"]["source"], "apt");
        assert_eq!(app["launchGuess"], false);
        assert!(app["iconUrl"].as_str().is_some());
        let expected_program = fixture.join("usr/bin/demo").display().to_string();
        assert_eq!(
            app["native"]["launcher"]["argv"][0],
            expected_program.as_str()
        );
        assert_eq!(app["command"][0], expected_program.as_str());
        assert_eq!(app["native"]["migration"], Value::Null);
        let status = row_status(&env, "demo").await.unwrap();
        assert_eq!(status.0, "ready");
        assert_eq!(status.2, "native");
    }

    #[tokio::test]
    async fn install_failure_marks_the_row_failed() {
        let dir = tempfile::tempdir().unwrap();
        let files = Arc::new(deb_files(dir.path()));
        let handler = deb_handler(
            files,
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = handler.clone();
        let env = test_env(Arc::new(move |op: &str, params: &Value| {
            if op == "deb_install" {
                helper_failure("install_failed", "the disk is full")
            } else {
                inner(op, params)
            }
        }))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let error = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "url", &ctx, None)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("disk is full"), "{error}");
        let status = row_status(&env, "demo").await.unwrap();
        assert_eq!(status.0, "failed");
        assert!(status.1.unwrap_or_default().contains("disk is full"));
    }

    #[tokio::test]
    async fn install_portable_then_update_keeps_the_previous_version() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = demo_tar(dir.path(), "Demo-1.0.tar.gz", "1.0");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        let app = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        assert_eq!(app["backend"], "native");
        assert_eq!(app["version"], "1.0");
        let old_dir = app["native"]["installPath"].as_str().unwrap().to_string();
        assert!(Path::new(&old_dir).is_dir());
        assert!(Path::new(&old_dir).join("bin/demo").is_file());
        let old_launcher = app["native"]["launcher"]["argv"][0]
            .as_str()
            .unwrap()
            .to_string();
        assert!(old_launcher.starts_with(&old_dir), "{old_launcher}");
        assert_eq!(app["native"]["previousPath"], Value::Null);
        let build: (i64,) = sqlx::query_as("SELECT build FROM linux_apps WHERE id = 'demo'")
            .fetch_one(&env.db)
            .await
            .unwrap();
        assert_eq!(build.0, 1);
        assert_eq!(app["update"]["source"], Value::Null);

        let archive = demo_tar(dir.path(), "Demo-2.0.tar.gz", "2.0");
        let staged = stage(&env, &archive, "Demo-2.0.tar.gz");
        let app = in_job(&env, "linuxapps.update", move |apps, ctx| async move {
            apps.update_from(1, "demo", &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .expect("update");
        assert_eq!(app["version"], "2.0");
        let build: (i64,) = sqlx::query_as("SELECT build FROM linux_apps WHERE id = 'demo'")
            .fetch_one(&env.db)
            .await
            .unwrap();
        assert_eq!(build.0, 2);
        let new_dir = app["native"]["installPath"].as_str().unwrap().to_string();
        assert_ne!(new_dir, old_dir);
        assert_eq!(app["native"]["previousPath"], old_dir);
        assert!(
            Path::new(&old_dir).join("bin/demo").is_file(),
            "old version kept"
        );
        assert!(Path::new(&new_dir).join("bin/demo").is_file());
        assert_eq!(app["name"], "Demo 2.0");
        assert_eq!(app["status"], "ready");
    }

    #[tokio::test]
    async fn ambiguous_archives_require_a_launcher_choice() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("Multi-1.0.tar.gz");
        build_tar(
            &archive,
            "multi-1.0",
            &[
                ("one", b"\x7fELF-one", 0o755),
                ("two", b"\x7fELF-two", 0o755),
            ],
        );
        let staged = stage(&env, &archive, "Multi-1.0.tar.gz");
        let preview = env
            .apps
            .preview(&staged, LinuxFormat::Tarball)
            .await
            .unwrap();
        assert_eq!(preview["needsChoice"], true);
        assert!(preview["candidates"].as_array().unwrap().len() >= 2);

        let staged = stage(&env, &archive, "Multi-1.0.tar.gz");
        let error = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("more than one program"), "{error}");
        let status = row_status(&env, "multi").await.unwrap();
        assert_eq!(status.0, "failed");

        let staged = stage(&env, &archive, "Multi-1.0.tar.gz");
        let choice = json!({"argv": ["two"], "name": "Two"});
        let app = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(
                1,
                &staged,
                LinuxFormat::Tarball,
                "file",
                &ctx,
                Some(&choice),
            )
            .await
        })
        .await
        .expect("install with choice");
        let launcher = app["native"]["launcher"]["argv"][0].as_str().unwrap();
        assert!(launcher.ends_with("/two"), "{launcher}");
    }

    #[tokio::test]
    async fn source_archives_are_rejected_with_an_actionable_message() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("Source-1.0.tar.gz");
        build_tar(
            &archive,
            "source-1.0",
            &[
                (
                    "CMakeLists.txt",
                    b"cmake_minimum_required(VERSION 3.0)\n",
                    0o644,
                ),
                ("src/main.c", b"int main(){return 0;}\n", 0o644),
            ],
        );
        let staged = stage(&env, &archive, "Source-1.0.tar.gz");
        let error = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("source archive"), "{error}");
        let status = row_status(&env, "source").await.unwrap();
        assert_eq!(status.0, "failed");
        assert!(status.1.unwrap_or_default().contains("source archive"));
    }

    // -----------------------------------------------------------------
    // Uninstall
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn uninstall_does_not_remove_a_preexisting_package() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            true,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let app = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        assert_eq!(app["native"]["preexistingPackage"], true);
        let removed = env.apps.uninstall(1, "demo", false).await.unwrap();
        assert_eq!(removed["removedPackages"].as_array().unwrap().len(), 0);
        assert!(env.helper.calls("deb_remove").is_empty());
        assert!(row_status(&env, "demo").await.is_none());
    }

    #[tokio::test]
    async fn uninstall_keeps_the_row_when_packages_cannot_be_removed_safely() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let unsafe_remove = Arc::new(AtomicBool::new(false));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            unsafe_remove.clone(),
        ))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        unsafe_remove.store(true, Ordering::Relaxed);
        let error = env.apps.uninstall(1, "demo", false).await.unwrap_err();
        assert!(error.public_message().contains("other software"), "{error}");
        assert!(row_status(&env, "demo").await.is_some(), "row must stay");
    }

    // -----------------------------------------------------------------
    // Update check
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn check_records_the_apt_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let candidate = Arc::new(Mutex::new("1.0".to_string()));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            false,
            candidate.clone(),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");

        *candidate.lock().unwrap() = "2.0".to_string();
        let checked = env.apps.check("demo").await.unwrap();
        assert_eq!(checked["update"]["latestVersion"], "2.0");
        assert_eq!(checked["update"]["available"], true);
        assert_eq!(checked["update"]["source"], "apt");
        assert_eq!(checked["update"]["error"], Value::Null);
        assert_eq!(checked["app"]["update"]["available"], true);
    }

    #[tokio::test]
    async fn check_without_an_update_source_is_not_an_error() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = demo_tar(dir.path(), "Demo-1.0.tar.gz", "1.0");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        let checked = env.apps.check("demo").await.unwrap();
        assert_eq!(checked["update"]["source"], Value::Null);
        assert_eq!(checked["update"]["available"], false);
        assert_eq!(checked["update"]["error"], Value::Null);
        assert!(checked["update"]["message"]
            .as_str()
            .unwrap()
            .contains("no update source"));
    }

    // -----------------------------------------------------------------
    // Migration
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn migration_moves_a_docker_app_to_native_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        // The Docker volume being migrated.
        let volume_source = dir.path().join("volume");
        write_file(
            &volume_source.join(".config/demo/settings.json"),
            br#"{"theme":"dark"}"#,
            0o644,
        );
        write_file(&volume_source.join("Desktop/note.txt"), b"hello", 0o644);
        let env = test_env(deb_handler(
            files,
            "demo",
            "2.0",
            false,
            Arc::new(Mutex::new("2.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        {
            let mut state = env.legacy.state();
            state.volumes = vec![LegacyVolume {
                user_id: 1,
                name: "rast-linux-demo-u1".to_string(),
            }];
            state.containers = json!({"containers": [
                {"app": "demo", "userId": 1, "state": "running", "running": true},
            ]});
            state.source = Some(volume_source.clone());
        }
        // The original upload is still staged, so migration can reinstall it.
        let legacy_package = env.config.linux_apps_dir().join("demo").join("package");
        fs::create_dir_all(legacy_package.parent().unwrap()).unwrap();
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        fs::copy(&source, &legacy_package).unwrap();
        insert_docker_row(&env, "demo", LinuxFormat::Deb, Some("apt")).await;

        let app = in_job(&env, "linuxapps.migrate", move |apps, ctx| async move {
            apps.migrate_legacy(1, "demo", None, None, &ctx).await
        })
        .await
        .expect("migration");
        assert_eq!(app["backend"], "native");
        assert_eq!(app["status"], "ready");
        assert_eq!(app["native"]["migration"]["state"], "migrated");
        assert_eq!(app["native"]["installSource"], "migrated");
        assert_eq!(app["native"]["packageVersion"], "2.0");
        let docker_volume = &app["native"]["migration"]["backup"]["volumes"][0]["name"];
        assert_eq!(docker_volume, "rast-linux-demo-u1");
        // The container was stopped and the volume exported, never modified.
        assert!(env
            .legacy
            .state()
            .stopped
            .iter()
            .any(|(app, user)| app == "demo" && *user == 1));
        assert_eq!(env.legacy.state().exported.len(), 1);
        // The profile carries the app settings in the native layout.
        let profile = env.config.linux_profiles_dir().join("demo").join("1");
        assert_eq!(
            fs::read_to_string(profile.join("config/demo/settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
        assert_eq!(
            fs::read_to_string(profile.join("home/Desktop/note.txt")).unwrap(),
            "hello"
        );
        assert!(!env.helper.calls("identity_ensure").is_empty());
        assert!(!env.helper.calls("path_chown").is_empty());

        let rolled = env.apps.rollback_migration(1, "demo").await.unwrap();
        assert_eq!(rolled["backend"], "docker");
        assert_eq!(rolled["migrationState"], "rolled_back");
        assert_eq!(rolled["app"]["backend"], "docker");

        let cleaned = env.apps.cleanup_migration(1, "demo", true).await.unwrap();
        assert_eq!(cleaned["removedVolumes"][0], "rast-linux-demo-u1");
        assert!(env.legacy.state().removed.contains(&1));
        let row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT docker_volume, migration_backup FROM linux_apps WHERE id = 'demo'",
        )
        .fetch_one(&env.db)
        .await
        .unwrap();
        assert!(row.0.is_none() && row.1.is_none());
    }

    #[tokio::test]
    async fn migration_without_a_source_keeps_the_docker_backend() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        insert_docker_row(&env, "demo", LinuxFormat::Tarball, None).await;
        let error = in_job(&env, "linuxapps.migrate", move |apps, ctx| async move {
            apps.migrate_legacy(1, "demo", None, None, &ctx).await
        })
        .await
        .unwrap_err();
        assert!(
            error.contains("no longer on the server"),
            "actionable message: {error}"
        );
        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT backend, migration_state, migration_error FROM linux_apps WHERE id = 'demo'",
        )
        .fetch_one(&env.db)
        .await
        .unwrap();
        assert_eq!(row.0, "docker");
        assert_eq!(row.1.as_deref(), Some("failed"));
        assert!(row
            .2
            .unwrap_or_default()
            .contains("no longer on the server"));
    }

    #[tokio::test]
    async fn migration_accepts_a_replacement_upload() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let env = test_env(deb_handler(
            files,
            "demo",
            "2.0",
            false,
            Arc::new(Mutex::new("2.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        insert_docker_row(&env, "demo", LinuxFormat::Deb, Some("apt")).await;
        let source = dir.path().join("demo_2.0_amd64.deb");
        fs::write(&source, b"FAKE DEB 2").unwrap();
        let staged = stage(&env, &source, "demo_2.0_amd64.deb");
        let app = in_job(&env, "linuxapps.migrate", move |apps, ctx| async move {
            apps.migrate_legacy(1, "demo", Some(&staged), Some(LinuxFormat::Deb), &ctx)
                .await
        })
        .await
        .expect("migration with replacement");
        assert_eq!(app["backend"], "native");
        assert_eq!(app["native"]["migration"]["state"], "migrated");
        assert_eq!(app["native"]["packageVersion"], "2.0");
    }

    // -----------------------------------------------------------------
    // Preview: transaction, launcher, duplicate detection, failures
    // -----------------------------------------------------------------

    fn elf_with_machine(machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        header[6] = 1;
        header[16..18].copy_from_slice(&2u16.to_le_bytes());
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header
    }

    async fn insert_native_row(
        env: &TestEnv,
        id: &str,
        format: LinuxFormat,
        package: Option<&str>,
        version: &str,
    ) {
        let now = db::now_ms();
        sqlx::query(
            "INSERT INTO linux_apps (id, name, format, file_name, version, status, backend, build,
                 package, update_kind, memory_mb, cpus, created_at, updated_at)
             VALUES (?, 'Demo', ?, 'demo_1.0_amd64.deb', ?, 'ready', 'native', 1, ?, 'apt',
                 2048, 2, ?, ?)",
        )
        .bind(id)
        .bind(format.as_str())
        .bind(version)
        .bind(package)
        .bind(now)
        .bind(now)
        .execute(&env.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn deb_preview_has_transaction_launcher_icon_and_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        // The host already has version 1.0; the upload is version 2.0.
        let env = test_env(deb_handler(
            files.clone(),
            "demo",
            "2.0",
            true,
            Arc::new(Mutex::new("2.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        insert_native_row(&env, "demo", LinuxFormat::Deb, Some("demo"), "1.0").await;
        let source = dir.path().join("demo_2.0_amd64.deb");
        fs::write(&source, b"FAKE DEB 2").unwrap();
        let staged = stage(&env, &source, "demo_2.0_amd64.deb");
        let preview = env.apps.preview(&staged, LinuxFormat::Deb).await.unwrap();

        // The App Center reads the nested transaction shape.
        assert_eq!(preview["deb"]["transaction"]["installs"][0]["name"], "demo");
        assert_eq!(preview["deb"]["transaction"]["downloadBytes"], 1024);
        // ... and the legacy flat keys stay.
        assert_eq!(preview["deb"]["installs"][0]["name"], "demo");
        // The launcher/icon of the installed package are shown.
        assert_eq!(
            preview["app"]["launcher"]["argv"][0],
            fixture.join("usr/bin/demo").display().to_string()
        );
        assert_eq!(preview["app"]["icon"]["type"], "png");
        // The existing app is offered as the update target.
        assert_eq!(preview["duplicate"]["appId"], "demo");
        assert_eq!(preview["duplicate"]["backend"], "native");
        assert_eq!(preview["duplicate"]["canUpdate"], true);
        assert_eq!(preview["duplicate"]["reason"], Value::Null);
    }

    #[tokio::test]
    async fn deb_preview_marks_an_older_upload_as_not_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            true,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        insert_native_row(&env, "demo", LinuxFormat::Deb, Some("demo"), "2.0").await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let preview = env.apps.preview(&staged, LinuxFormat::Deb).await.unwrap();
        assert_eq!(preview["duplicate"]["canUpdate"], false);
        assert!(preview["duplicate"]["reason"]
            .as_str()
            .unwrap()
            .contains("not newer"));
    }

    #[tokio::test]
    async fn portable_preview_detects_an_existing_app() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = demo_tar(dir.path(), "Demo-1.0.tar.gz", "1.0");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        let preview = env
            .apps
            .preview(&staged, LinuxFormat::Tarball)
            .await
            .unwrap();
        assert_eq!(preview["duplicate"]["appId"], "demo");
        assert_eq!(preview["duplicate"]["canUpdate"], false);
    }

    #[tokio::test]
    async fn source_archive_preview_is_four_hundred_with_the_actionable_message() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("Source-1.0.tar.gz");
        build_tar(
            &archive,
            "source-1.0",
            &[
                (
                    "CMakeLists.txt",
                    b"cmake_minimum_required(VERSION 3.0)\n",
                    0o644,
                ),
                ("src/main.c", b"int main(){return 0;}\n", 0o644),
            ],
        );
        let staged = stage(&env, &archive, "Source-1.0.tar.gz");
        let error = env
            .apps
            .preview(&staged, LinuxFormat::Tarball)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)), "{error}");
        assert_eq!(
            error.public_message(),
            native_archive::SOURCE_ARCHIVE_MESSAGE
        );
    }

    #[tokio::test]
    async fn portable_preview_reports_a_foreign_architecture_as_blocking() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("Foreign-1.0.tar.gz");
        let binary = elf_with_machine(183); // arm64
        build_tar(
            &archive,
            "foreign-1.0",
            &[("foreign", binary.as_slice(), 0o755)],
        );
        let staged = stage(&env, &archive, "Foreign-1.0.tar.gz");
        let preview = env
            .apps
            .preview(&staged, LinuxFormat::Tarball)
            .await
            .unwrap();
        assert_eq!(preview["compatibility"]["ok"], false);
        let reasons = preview["compatibility"]["reasons"].as_array().unwrap();
        assert!(
            reasons
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("built for arm64")),
            "{reasons:?}"
        );
        assert_eq!(preview["duplicate"], Value::Null);
    }

    #[tokio::test]
    async fn staging_permission_problems_are_actionable() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        // Simulate <data>/linux-apps being an unusable entry (e.g. owned by
        // root 0711): creating the staging directory below it fails.
        let linux_apps = env.config.linux_apps_dir();
        fs::create_dir_all(linux_apps.parent().unwrap()).unwrap();
        fs::write(&linux_apps, b"not a directory").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Demo-1.0.tar.gz");
        fs::write(&source, b"bytes").unwrap();
        let error = env
            .apps
            .copy_into_staging(&source, "Demo-1.0.tar.gz")
            .await
            .unwrap_err();
        let message = error.public_message();
        assert!(message.contains("writable by the rast"), "{message}");
        assert!(message.contains("0750 rast:rast"), "{message}");
    }

    #[tokio::test]
    async fn install_re_verifies_the_staged_file_inside_the_job() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let archive = demo_tar(dir.path(), "Demo-1.0.tar.gz", "1.0");
        let staged = stage(&env, &archive, "Demo-1.0.tar.gz");
        // Simulate the file changing between staging and the job (TOCTOU).
        fs::write(&staged.path, b"tampered").unwrap();
        let error = in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Tarball, "file", &ctx, None)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(app_count(&env).await, 0, "no row may be created");
    }

    #[tokio::test]
    async fn failed_update_keeps_the_previous_version_ready() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let incompatible = Arc::new(AtomicBool::new(false));
        let inner = deb_handler(
            files,
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        );
        let incompatible_flag = incompatible.clone();
        let env = test_env(Arc::new(move |op: &str, params: &Value| {
            let outcome = inner.as_ref()(op, params);
            if incompatible_flag.load(Ordering::Relaxed) && op == "deb_preview" {
                if let Ok(mut data) = outcome {
                    data["compatible"] = Value::Bool(false);
                    data["reasons"] = json!(["the new version needs a newer system library"]);
                    return Ok(data);
                }
            }
            outcome
        }))
        .await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        in_job(&env, "linuxapps.install", move |apps, ctx| async move {
            apps.install(1, &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .expect("install");
        let source = dir.path().join("demo_1.5_amd64.deb");
        fs::write(&source, b"FAKE DEB NEW").unwrap();
        let staged = stage(&env, &source, "demo_1.5_amd64.deb");
        incompatible.store(true, Ordering::Relaxed);
        let error = in_job(&env, "linuxapps.update", move |apps, ctx| async move {
            apps.update_from(1, "demo", &staged, LinuxFormat::Deb, "file", &ctx, None)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("newer system library"), "{error}");
        let status = row_status(&env, "demo").await.unwrap();
        assert_eq!(status.0, "ready", "the installed app must stay usable");
        assert!(status.1.unwrap_or_default().contains("Update failed"));
        let version: (Option<String>, i64) =
            sqlx::query_as("SELECT version, build FROM linux_apps WHERE id = 'demo'")
                .fetch_one(&env.db)
                .await
                .unwrap();
        assert_eq!(version.0.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn migration_refuses_to_replace_existing_native_data() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let files = Arc::new(deb_files(&fixture));
        let env = test_env(deb_handler(
            files,
            "demo",
            "1.0",
            false,
            Arc::new(Mutex::new("1.0".into())),
            Arc::new(AtomicBool::new(false)),
        ))
        .await;
        {
            let mut state = env.legacy.state();
            state.volumes = vec![LegacyVolume {
                user_id: 1,
                name: "rast-linux-demo-u1".to_string(),
            }];
            state.source = Some(volume_source(dir.path()));
        }
        // Native data from an earlier native run.
        fs::create_dir_all(env.config.linux_profiles_dir().join("demo/1")).unwrap();
        insert_docker_row(&env, "demo", LinuxFormat::Deb, Some("apt")).await;
        let source = dir.path().join("demo_1.0_amd64.deb");
        fs::write(&source, b"FAKE DEB").unwrap();
        let staged = stage(&env, &source, "demo_1.0_amd64.deb");
        let error = in_job(&env, "linuxapps.migrate", move |apps, ctx| async move {
            apps.migrate_legacy(1, "demo", Some(&staged), Some(LinuxFormat::Deb), &ctx)
                .await
        })
        .await
        .unwrap_err();
        assert!(error.contains("already has native data"), "{error}");
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT backend, migration_state FROM linux_apps WHERE id = 'demo'")
                .fetch_one(&env.db)
                .await
                .unwrap();
        assert_eq!(row.0, "docker");
        assert_eq!(row.1.as_deref(), Some("failed"));
        assert!(
            env.helper.calls("deb_install").is_empty(),
            "nothing may be installed"
        );
    }

    fn volume_source(dir: &Path) -> PathBuf {
        let volume = dir.join("volume-source");
        write_file(
            &volume.join(".config/demo/settings.json"),
            br#"{"theme":"dark"}"#,
            0o644,
        );
        volume
    }

    #[tokio::test]
    async fn rollback_requires_a_migrated_app() {
        let env = test_env(Arc::new(|op: &str, _: &Value| {
            helper_failure("unsupported", &format!("unexpected helper op {op}"))
        }))
        .await;
        insert_docker_row(&env, "demo", LinuxFormat::Deb, None).await;
        let error = env.apps.rollback_migration(1, "demo").await.unwrap_err();
        assert!(error.public_message().contains("migrated"), "{error}");
    }
}
