//! Wire protocol between the unprivileged RAST web server and the privileged
//! native helper (`rast native-helper`).
//!
//! The protocol is JSON-lines over a Unix socket: one request object per line,
//! one response object per line, UTF-8 only. Every request carries an opaque
//! numeric `id` that the response echoes. The helper is the only component
//! allowed to provision Linux identities, start per-user graphical sessions
//! and per-application processes through systemd transient units, run host APT
//! transactions and change ownership of managed profile paths.
//!
//! Wire shape (protocol version 1):
//!
//! ```text
//! request:  {"id":<u64>,"op":"<name>", ...op fields}
//! response: {"id":<u64|null>,"ok":<bool>,"data":<object|null>,"error":{"code":"...","message":"..."}|null}
//! ```
//!
//! Requests are parsed in two stages: an envelope (`id`, `op`) is read from a
//! [`serde_json::Value`], then the remaining fields are deserialized into an
//! op-specific struct with `#[serde(deny_unknown_fields)]`. Unknown operations
//! and extra fields therefore fail instead of being silently ignored.
//!
//! Security model: the web server can only *ask* for one of the whitelisted
//! [`NativeOp`]s. Every argument is validated here (and again by the handler
//! that needs it): identifiers use tight alphabets, numbers are range checked,
//! environment/argv entries are length checked and never interpreted by a
//! shell, and every filesystem path is resolved against an explicit allow-list
//! of managed roots without following symlinks out of them.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire protocol version. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted length of one request line (256 KiB).
pub const MAX_REQUEST_LINE_BYTES: usize = 256 * 1024;

/// Maximum size of one response line (1 MiB).
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Runtime root for per-user native sessions.
///
/// Deliberately *not* `/run/rast`: that directory is `0750 rast-worker:rast`
/// and unprivileged app identities cannot traverse it. `/run/rast-linux` is
/// `0755 root:root` (see `deploy/rast-native.tmpfiles.conf`); only the
/// per-user subdirectories `/run/rast-linux/<uid>` are `0700 uid:gid`.
pub const DEFAULT_RUN_DIR: &str = "/run/rast-linux";

/// Environment variable naming the socket group (default `rast`).
pub const SOCKET_GROUP_ENV: &str = "RAST_NATIVE_SOCKET_GROUP";

/// Environment variable with the numeric uid allowed to connect.
pub const ALLOWED_UID_ENV: &str = "RAST_NATIVE_ALLOWED_UID";

/// Default group owning the helper socket (`rast`).
pub const DEFAULT_SOCKET_GROUP: &str = "rast";

/// Maximum number of environment variables accepted in one request.
pub const MAX_ENV_VARS: usize = 64;
/// Maximum length in bytes of one environment value.
pub const MAX_ENV_VALUE_BYTES: usize = 4096;
/// Maximum number of argv entries accepted in one request.
pub const MAX_ARGV_ENTRIES: usize = 64;
/// Maximum length in bytes of one argv entry.
pub const MAX_ARGV_BYTES: usize = 4096;
/// Maximum length of a systemd unit name (systemd's own limit is 256; RAST
/// names are much shorter and generated server side).
pub const MAX_UNIT_LEN: usize = 64;
/// Maximum length of one filesystem path string.
pub const MAX_PATH_BYTES: usize = 4096;
/// Highest accepted `userId` (uid must stay inside one million ids).
pub const MAX_USER_ID: i64 = 1_000_000;
/// Maximum number of package names in one `deb_remove` request.
pub const MAX_PACKAGES: usize = 200;
/// Maximum length of one Debian package name.
pub const MAX_PACKAGE_NAME_LEN: usize = 128;
/// Maximum size of an uploaded `.deb` (4 GiB).
pub const MAX_DEB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Bytes of command output returned to the caller (last lines only).
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Maximum length of an error message returned to the caller.
pub const MAX_ERROR_MESSAGE: usize = 2000;

/// Client-side deadline for cheap operations (ping, unit queries, sessions,
/// app start/stop, path operations, package policy).
pub const QUICK_OP_TIMEOUT: Duration = Duration::from_secs(15);
/// Client-side deadline for account provisioning. `groupadd`/`useradd`/
/// `userdel` can take several seconds each on hosts with slow NSS/audit
/// setups (measured ~8 s each on the RAST development host), so identity
/// operations get a larger deadline than the cheap operations.
pub const IDENTITY_OP_TIMEOUT: Duration = Duration::from_secs(120);
/// Client-side deadline for `deb_preview` (`apt-get -s`).
pub const DEB_PREVIEW_TIMEOUT: Duration = Duration::from_secs(120);
/// Client-side deadline for `deb_install`.
pub const DEB_INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Client-side deadline for `deb_remove`.
pub const DEB_REMOVE_TIMEOUT: Duration = Duration::from_secs(300);
/// Client-side deadline for `deb_download` (fetch one .deb from the host APT
/// repositories into the staging tree).
pub const DEB_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Client-side deadline for `apt_update` (server runs it with 180 s).
pub const APT_UPDATE_TIMEOUT: Duration = Duration::from_secs(190);

/// Client-side deadline for an operation name (used by generic JSON calls).
pub fn timeout_for_op(name: &str) -> Duration {
    match name {
        "deb_preview" => DEB_PREVIEW_TIMEOUT,
        "deb_install" => DEB_INSTALL_TIMEOUT,
        "deb_remove" => DEB_REMOVE_TIMEOUT,
        "deb_download" => DEB_DOWNLOAD_TIMEOUT,
        "apt_update" => APT_UPDATE_TIMEOUT,
        "identity_ensure" | "identity_remove" => IDENTITY_OP_TIMEOUT,
        _ => QUICK_OP_TIMEOUT,
    }
}

/// Numeric limits of an application or session, validated against the
/// configured maxima.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub memory_max_mb: u32,
    pub cpu_quota_percent: u32,
    pub tasks_max: u32,
}

impl ResourceLimits {
    /// Build the maximum limits from configuration values (cpus -> percent).
    pub fn from_config(memory_max_mb: u32, cpus: u32, tasks_max: u32) -> Self {
        Self {
            memory_max_mb,
            cpu_quota_percent: cpus.saturating_mul(100),
            tasks_max,
        }
    }
}

/// Identity derived from a RAST user id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityIds {
    pub user_id: i64,
    pub uid: u32,
    pub gid: u32,
}

/// Which filesystem operation a path is being validated for. The set of
/// allowed roots differs: home ACLs may also target the person's RAST home
/// under `<storage_dir>/homes`, while destructive/ownership operations stay
/// inside the Linux app/user trees and the runtime directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPurpose {
    /// Create a directory tree (final component may not exist).
    Mkdir,
    /// Change ownership (final component must exist, never a symlink).
    Chown,
    /// Remove recursively (a final symlink is unlinked, never followed).
    Remove,
    /// Change permissions.
    Chmod,
    /// Add a POSIX ACL entry.
    Acl,
}

/// Managed filesystem roots. Paths from requests are only ever resolved
/// relative to one of these.
#[derive(Debug, Clone)]
pub struct ManagedRoots {
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
    /// `<storage_dir>/homes`; only used by ACL/chmod operations.
    pub homes_dir: Option<PathBuf>,
}

impl ManagedRoots {
    pub fn new(data_dir: impl Into<PathBuf>, run_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            run_dir: run_dir.into(),
            homes_dir: None,
        }
    }

    pub fn with_homes(mut self, homes_dir: impl Into<PathBuf>) -> Self {
        self.homes_dir = Some(homes_dir.into());
        self
    }

    /// `<data_dir>/linux-apps`: uploaded installers, profiles, app trees.
    pub fn linux_apps(&self) -> PathBuf {
        self.data_dir.join("linux-apps")
    }

    /// `<data_dir>/linux-users`: homes of the unprivileged app identities.
    pub fn linux_users(&self) -> PathBuf {
        self.data_dir.join("linux-users")
    }

    /// `<data_dir>/linux-apps/profiles`: per-user application profiles.
    pub fn profiles(&self) -> PathBuf {
        self.linux_apps().join("profiles")
    }

    pub(crate) fn roots(&self, purpose: PathPurpose) -> Vec<PathBuf> {
        let mut roots = vec![self.linux_apps(), self.linux_users(), self.run_dir.clone()];
        if matches!(purpose, PathPurpose::Chmod | PathPurpose::Acl) {
            if let Some(homes) = &self.homes_dir {
                roots.push(homes.clone());
            }
        }
        roots
    }
}

/// Everything the pure validators need. Built from [`super::NativeOptions`].
#[derive(Debug, Clone)]
pub struct ValidationContext {
    pub roots: ManagedRoots,
    /// Configured `linux_xpra_bin`; session requests may only start this binary.
    pub xpra_bin: PathBuf,
    /// First uid handed out (`linux_uid_base`); uid = base + userId.
    pub uid_base: u32,
    /// Per-session maxima from configuration.
    pub session_limits: ResourceLimits,
    /// Per-application maxima from configuration.
    pub app_limits: ResourceLimits,
}

/// Operations accepted over the socket. Anything else is refused.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeOp {
    Ping,
    IdentityEnsure(IdentityEnsureOp),
    IdentityRemove(IdentityRemoveOp),
    SessionStart(SessionStartOp),
    SessionStop(SessionStopOp),
    UnitStatus(UnitStatusOp),
    UnitsList,
    AppStart(AppStartOp),
    AppStop(AppStopOp),
    DebPreview(DebPreviewOp),
    DebInstall(DebInstallOp),
    DebRemove(DebRemoveOp),
    DebDownload(DebDownloadOp),
    PackagePolicy(PackagePolicyOp),
    PathMkdir(PathMkdirOp),
    PathChown(PathChownOp),
    PathRemove(PathRemoveOp),
    PathChmod(PathChmodOp),
    PathAcl(PathAclOp),
    PathSymlink(PathSymlinkOp),
    ProfilePrepare(ProfilePrepareOp),
    ProfileCleanLocks(ProfileCleanLocksOp),
    ProfileRemove(ProfileRemoveOp),
    PackageInstalledList(PackageInstalledListOp),
    PackageFiles(PackageFilesOp),
    AptUpdate,
}

impl NativeOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::IdentityEnsure(_) => "identity_ensure",
            Self::IdentityRemove(_) => "identity_remove",
            Self::SessionStart(_) => "session_start",
            Self::SessionStop(_) => "session_stop",
            Self::UnitStatus(_) => "unit_status",
            Self::UnitsList => "units_list",
            Self::AppStart(_) => "app_start",
            Self::AppStop(_) => "app_stop",
            Self::DebPreview(_) => "deb_preview",
            Self::DebInstall(_) => "deb_install",
            Self::DebRemove(_) => "deb_remove",
            Self::DebDownload(_) => "deb_download",
            Self::PackagePolicy(_) => "package_policy",
            Self::PathMkdir(_) => "path_mkdir",
            Self::PathChown(_) => "path_chown",
            Self::PathRemove(_) => "path_remove",
            Self::PathChmod(_) => "path_chmod",
            Self::PathAcl(_) => "path_acl",
            Self::PathSymlink(_) => "path_symlink",
            Self::ProfilePrepare(_) => "profile_prepare",
            Self::ProfileCleanLocks(_) => "profile_clean_locks",
            Self::ProfileRemove(_) => "profile_remove",
            Self::PackageInstalledList(_) => "package_installed_list",
            Self::PackageFiles(_) => "package_files",
            Self::AptUpdate => "apt_update",
        }
    }

    /// Client-side deadline of this operation.
    pub fn timeout(&self) -> Duration {
        match self {
            Self::DebPreview(_) => DEB_PREVIEW_TIMEOUT,
            Self::DebInstall(_) => DEB_INSTALL_TIMEOUT,
            Self::DebRemove(_) => DEB_REMOVE_TIMEOUT,
            Self::DebDownload(_) => DEB_DOWNLOAD_TIMEOUT,
            Self::AptUpdate => APT_UPDATE_TIMEOUT,
            Self::IdentityEnsure(_) | Self::IdentityRemove(_) => IDENTITY_OP_TIMEOUT,
            _ => QUICK_OP_TIMEOUT,
        }
    }

    /// Whether the operation mutates host state and must be serialized with
    /// other long-running work.
    pub fn is_long_running(&self) -> bool {
        !matches!(
            self,
            Self::Ping
                | Self::UnitStatus(_)
                | Self::UnitsList
                | Self::PackagePolicy(_)
                | Self::PathMkdir(_)
                | Self::PathRemove(_)
                | Self::PathSymlink(_)
                | Self::PackageInstalledList(_)
                | Self::PackageFiles(_)
        )
    }

    /// Whether APT operations must hold the global package-manager lock.
    pub fn is_apt_op(&self) -> bool {
        matches!(
            self,
            Self::DebPreview(_)
                | Self::DebInstall(_)
                | Self::DebRemove(_)
                | Self::DebDownload(_)
                | Self::PackagePolicy(_)
                | Self::AptUpdate
        )
    }
}

/// One request: opaque id + operation.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeRequest {
    pub id: u64,
    pub op: NativeOp,
}

impl NativeRequest {
    pub fn new(id: u64, op: NativeOp) -> Self {
        Self { id, op }
    }

    pub fn name(&self) -> &'static str {
        self.op.name()
    }

    pub fn timeout(&self) -> Duration {
        self.op.timeout()
    }

    /// Serialize to one JSON line (including the trailing newline).
    pub fn to_line(&self) -> Result<Vec<u8>, NativeError> {
        let mut map = Map::new();
        map.insert("id".to_string(), Value::from(self.id));
        map.insert("op".to_string(), Value::String(self.op.name().to_string()));
        macro_rules! merge {
            ($op:expr) => {{
                let fields = serde_json::to_value($op).map_err(|error| {
                    NativeError::protocol(format!("cannot encode request: {error}"))
                })?;
                if let Value::Object(fields) = fields {
                    map.extend(fields);
                }
            }};
        }
        match &self.op {
            NativeOp::Ping | NativeOp::UnitsList | NativeOp::AptUpdate => {}
            NativeOp::IdentityEnsure(op) => merge!(op),
            NativeOp::IdentityRemove(op) => merge!(op),
            NativeOp::SessionStart(op) => merge!(op),
            NativeOp::SessionStop(op) => merge!(op),
            NativeOp::UnitStatus(op) => merge!(op),
            NativeOp::AppStart(op) => merge!(op),
            NativeOp::AppStop(op) => merge!(op),
            NativeOp::DebPreview(op) => merge!(op),
            NativeOp::DebInstall(op) => merge!(op),
            NativeOp::DebRemove(op) => merge!(op),
            NativeOp::DebDownload(op) => merge!(op),
            NativeOp::PackagePolicy(op) => merge!(op),
            NativeOp::PathMkdir(op) => merge!(op),
            NativeOp::PathChown(op) => merge!(op),
            NativeOp::PathRemove(op) => merge!(op),
            NativeOp::PathChmod(op) => merge!(op),
            NativeOp::PathAcl(op) => merge!(op),
            NativeOp::PathSymlink(op) => merge!(op),
            NativeOp::ProfilePrepare(op) => merge!(op),
            NativeOp::ProfileCleanLocks(op) => merge!(op),
            NativeOp::ProfileRemove(op) => merge!(op),
            NativeOp::PackageInstalledList(op) => merge!(op),
            NativeOp::PackageFiles(op) => merge!(op),
        }
        let mut line = serde_json::to_vec(&Value::Object(map))
            .map_err(|error| NativeError::protocol(format!("cannot encode request: {error}")))?;
        line.push(b'\n');
        Ok(line)
    }
}

/// Parse one request line. `Unsupported` means the operation is unknown (the
/// helper is older/newer than the server); `Invalid` means the line is
/// well-formed JSON but not an acceptable request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestParseError {
    Unsupported(String),
    Invalid(String),
}

impl RequestParseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::Invalid(_) => "invalid_request",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(message) | Self::Invalid(message) => message,
        }
    }

    /// Best-effort request id, echoed by the error response.
    pub fn id(&self) -> Option<u64> {
        None
    }
}

/// Operations without parameters reject every remaining field.
fn no_fields(rest: Value, op: &str) -> Result<(), RequestParseError> {
    match rest {
        Value::Object(map) if map.is_empty() => Ok(()),
        Value::Object(map) => Err(RequestParseError::Invalid(format!(
            "invalid `{op}` request: unknown field {:?}",
            map.keys().next().cloned().unwrap_or_default()
        ))),
        _ => Err(RequestParseError::Invalid(format!(
            "invalid `{op}` request: expected a JSON object"
        ))),
    }
}

fn op_from<T: DeserializeOwned>(rest: Value, op: &str) -> Result<T, RequestParseError> {
    serde_json::from_value(rest)
        .map_err(|error| RequestParseError::Invalid(format!("invalid `{op}` request: {error}")))
}

/// Parse one JSON-lines request. The id is returned separately so that a
/// request that fails op validation can still be answered on the same id.
pub fn parse_request_line(line: &[u8]) -> Result<NativeRequest, RequestParseError> {
    if line.is_empty() {
        return Err(RequestParseError::Invalid("empty request".into()));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|error| RequestParseError::Invalid(format!("malformed JSON request: {error}")))?;
    let Value::Object(mut map) = value else {
        return Err(RequestParseError::Invalid(
            "request must be a JSON object".into(),
        ));
    };
    let id = match map.remove("id") {
        Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
            RequestParseError::Invalid("`id` must be a non-negative integer".into())
        })?,
        Some(_) => {
            return Err(RequestParseError::Invalid(
                "`id` must be a non-negative integer".into(),
            ))
        }
        None => return Err(RequestParseError::Invalid("request requires `id`".into())),
    };
    let op = match map.remove("op") {
        Some(Value::String(op)) => op,
        Some(_) => return Err(RequestParseError::Invalid("`op` must be a string".into())),
        None => return Err(RequestParseError::Invalid("request requires `op`".into())),
    };
    let rest = Value::Object(map);
    let parsed = match op.as_str() {
        "ping" => no_fields(rest, "ping").map(|_| NativeOp::Ping)?,
        "identity_ensure" => NativeOp::IdentityEnsure(op_from(rest, "identity_ensure")?),
        "identity_remove" => NativeOp::IdentityRemove(op_from(rest, "identity_remove")?),
        "session_start" => NativeOp::SessionStart(op_from(rest, "session_start")?),
        "session_stop" => NativeOp::SessionStop(op_from(rest, "session_stop")?),
        "unit_status" => NativeOp::UnitStatus(op_from(rest, "unit_status")?),
        "units_list" => no_fields(rest, "units_list").map(|_| NativeOp::UnitsList)?,
        "app_start" => NativeOp::AppStart(op_from(rest, "app_start")?),
        "app_stop" => NativeOp::AppStop(op_from(rest, "app_stop")?),
        "deb_preview" => NativeOp::DebPreview(op_from(rest, "deb_preview")?),
        "deb_install" => NativeOp::DebInstall(op_from(rest, "deb_install")?),
        "deb_remove" => NativeOp::DebRemove(op_from(rest, "deb_remove")?),
        "deb_download" => NativeOp::DebDownload(op_from(rest, "deb_download")?),
        "package_policy" => NativeOp::PackagePolicy(op_from(rest, "package_policy")?),
        "path_mkdir" => NativeOp::PathMkdir(op_from(rest, "path_mkdir")?),
        "path_chown" => NativeOp::PathChown(op_from(rest, "path_chown")?),
        "path_remove" => NativeOp::PathRemove(op_from(rest, "path_remove")?),
        "path_chmod" => NativeOp::PathChmod(op_from(rest, "path_chmod")?),
        "path_acl" => NativeOp::PathAcl(op_from(rest, "path_acl")?),
        "path_symlink" => NativeOp::PathSymlink(op_from(rest, "path_symlink")?),
        "profile_prepare" => NativeOp::ProfilePrepare(op_from(rest, "profile_prepare")?),
        "profile_clean_locks" => NativeOp::ProfileCleanLocks(op_from(rest, "profile_clean_locks")?),
        "profile_remove" => NativeOp::ProfileRemove(op_from(rest, "profile_remove")?),
        "package_installed_list" => {
            NativeOp::PackageInstalledList(op_from(rest, "package_installed_list")?)
        }
        "package_files" => NativeOp::PackageFiles(op_from(rest, "package_files")?),
        "apt_update" => no_fields(rest, "apt_update").map(|_| NativeOp::AptUpdate)?,
        other => {
            return Err(RequestParseError::Unsupported(format!(
                "this native helper (RAST {}) does not know the operation '{other}'; \
                 restart rast-native.service so it runs the same version as RAST",
                crate::VERSION
            )))
        }
    };
    Ok(NativeRequest { id, op: parsed })
}

// ---------------------------------------------------------------------------
// Per-op request payloads (strict: unknown fields fail deserialization)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityEnsureOp {
    #[serde(rename = "userId")]
    pub user_id: i64,
    /// Optional convenience echo of the expected username. When present it must
    /// equal `rast-u<userId>`.
    #[serde(default, rename = "username", skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "IdentityRemoveOp", deny_unknown_fields)]
pub struct IdentityRemoveOp {
    #[serde(rename = "userId")]
    pub user_id: i64,
    #[serde(rename = "removeHome")]
    pub remove_home: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStartOp {
    #[serde(rename = "userId")]
    pub user_id: i64,
    pub display: u32,
    pub port: u32,
    #[serde(rename = "xpraBin")]
    pub xpra_bin: String,
    #[serde(rename = "runtimeDir")]
    pub runtime_dir: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(rename = "memoryMaxMb")]
    pub memory_max_mb: u32,
    #[serde(rename = "cpuQuotaPercent")]
    pub cpu_quota_percent: u32,
    #[serde(rename = "tasksMax")]
    pub tasks_max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStopOp {
    #[serde(rename = "userId")]
    pub user_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitStatusOp {
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppStartOp {
    #[serde(rename = "userId")]
    pub user_id: i64,
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    pub unit: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    #[serde(rename = "memoryMaxMb")]
    pub memory_max_mb: u32,
    #[serde(rename = "cpuQuotaPercent")]
    pub cpu_quota_percent: u32,
    #[serde(rename = "tasksMax")]
    pub tasks_max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppStopOp {
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebPreviewOp {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebInstallOp {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebRemoveOp {
    pub packages: Vec<String>,
    pub purge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebDownloadOp {
    /// Debian package name to fetch from the configured APT repositories.
    pub package: String,
    /// Existing (or to-be-created) directory under `<data_dir>/linux-apps`.
    pub dest: String,
    /// Optional exact version (`apt-get download pkg=version`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagePolicyOp {
    pub package: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMkdirOp {
    pub path: String,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathChownOp {
    pub path: String,
    pub uid: u32,
    pub gid: u32,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRemoveOp {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathChmodOp {
    pub path: String,
    #[serde(rename = "modeOctal")]
    pub mode_octal: u32,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathAclOp {
    pub path: String,
    pub uid: u32,
    pub perms: String,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathSymlinkOp {
    /// Symlink to create (inside a managed root; the parent must exist).
    pub link: String,
    /// Absolute symlink target inside the managed roots or a home directory.
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePrepareOp {
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(rename = "userId")]
    pub user_id: i64,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    #[serde(rename = "rastHome")]
    pub rast_home: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCleanLocksOp {
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(rename = "userId")]
    pub user_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRemoveOp {
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(rename = "userId")]
    pub user_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageInstalledListOp {
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFilesOp {
    pub package: String,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Error payload returned by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeErrorBody {
    pub code: String,
    pub message: String,
}

/// One response line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeResponse {
    /// Echoed request id; `null` when the id itself could not be read.
    pub id: Option<u64>,
    pub ok: bool,
    pub data: Option<Value>,
    pub error: Option<NativeErrorBody>,
}

impl NativeResponse {
    pub fn success(id: Option<u64>, data: Value) -> Self {
        Self {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(id: Option<u64>, error: &NativeError) -> Self {
        Self {
            id,
            ok: false,
            data: None,
            error: Some(NativeErrorBody {
                code: error.code.clone(),
                message: truncate_message(&error.message, MAX_ERROR_MESSAGE),
            }),
        }
    }

    /// Serialize to one JSON line (without the trailing newline).
    pub fn to_line(&self) -> Result<Vec<u8>, NativeError> {
        serde_json::to_vec(self)
            .map_err(|error| NativeError::protocol(format!("cannot encode response: {error}")))
    }
}

/// Error used by both the helper and its client. `code` is a stable
/// machine-readable identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeError {
    pub code: String,
    pub message: String,
}

impl NativeError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: truncate_message(&message.into(), MAX_ERROR_MESSAGE),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new("unavailable", message)
    }
    pub fn command_failed(message: impl Into<String>) -> Self {
        Self::new("command_failed", message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new("timeout", message)
    }
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new("protocol", message)
    }
    pub fn identity_conflict(message: impl Into<String>) -> Self {
        Self::new("identity_conflict", message)
    }
    pub fn identity_in_use(message: impl Into<String>) -> Self {
        Self::new("identity_in_use", message)
    }
    pub fn checksum_mismatch(message: impl Into<String>) -> Self {
        Self::new("checksum_mismatch", message)
    }
    pub fn invalid_deb(message: impl Into<String>) -> Self {
        Self::new("invalid_deb", message)
    }
    pub fn install_failed(message: impl Into<String>) -> Self {
        Self::new("install_failed", message)
    }
    pub fn apt_update_failed(message: impl Into<String>) -> Self {
        Self::new("apt_update_failed", message)
    }
    pub fn unsafe_removal(message: impl Into<String>) -> Self {
        Self::new("unsafe_removal", message)
    }
    pub fn acls_unavailable(message: impl Into<String>) -> Self {
        Self::new("acls_unavailable", message)
    }
    pub fn insufficient_memory(message: impl Into<String>) -> Self {
        Self::new("insufficient_memory", message)
    }
    pub fn unit_conflict(message: impl Into<String>) -> Self {
        Self::new("unit_conflict", message)
    }
    pub fn cwd_inaccessible(message: impl Into<String>) -> Self {
        Self::new("cwd_inaccessible", message)
    }
    pub fn xauthority_unavailable(message: impl Into<String>) -> Self {
        Self::new("xauthority_unavailable", message)
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for NativeError {}

impl From<std::io::Error> for NativeError {
    fn from(error: std::io::Error) -> Self {
        Self::unavailable(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Strip control characters and truncate on a UTF-8 boundary. Newlines and
/// tabs survive because command output wants them.
pub fn truncate_message(value: &str, max: usize) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    if cleaned.len() <= max {
        return cleaned;
    }
    let mut cut = max;
    while cut > 0 && !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &cleaned[..cut])
}

/// Keep the last `max_bytes` of command output, on line boundaries.
pub fn last_lines(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let target = text.len() - max_bytes;
    let mut start = 0usize;
    for (index, _) in text.char_indices() {
        if index <= target {
            start = index;
        } else {
            break;
        }
    }
    let start = text[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);
    let mut out = text[start..].to_string();
    if out.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Username of a RAST Linux identity: `rast-u<userId>`.
pub fn username_for(user_id: i64) -> String {
    format!("rast-u{user_id}")
}

/// `^rast-u[1-9][0-9]{0,9}$`: generated server side, never taken from JSON.
pub fn validate_username(username: &str) -> Result<(), NativeError> {
    const PREFIX: &str = "rast-u";
    let Some(digits) = username.strip_prefix(PREFIX) else {
        return Err(NativeError::invalid(format!(
            "invalid Linux username {username:?}"
        )));
    };
    if digits.is_empty() || digits.len() > 10 {
        return Err(NativeError::invalid("invalid Linux username"));
    }
    let bytes = digits.as_bytes();
    if !(b'1'..=b'9').contains(&bytes[0]) {
        return Err(NativeError::invalid(format!(
            "invalid Linux username {username:?}"
        )));
    }
    if !bytes[1..].iter().all(u8::is_ascii_digit) {
        return Err(NativeError::invalid(format!(
            "invalid Linux username {username:?}"
        )));
    }
    Ok(())
}

/// Derive and range-check the uid/gid of a RAST user id.
pub fn identity_ids(ctx: &ValidationContext, user_id: i64) -> Result<IdentityIds, NativeError> {
    if !(1..=MAX_USER_ID).contains(&user_id) {
        return Err(NativeError::invalid(format!(
            "userId must be between 1 and {MAX_USER_ID}"
        )));
    }
    let user_id_u32 = user_id as u32;
    let uid = ctx
        .uid_base
        .checked_add(user_id_u32)
        .ok_or_else(|| NativeError::invalid("userId overflows the configured uid range"))?;
    Ok(IdentityIds {
        user_id,
        uid,
        gid: uid,
    })
}

/// Unit regex `^rast-[a-z0-9][a-z0-9-]*$`, at most 64 characters.
pub fn validate_unit(unit: &str) -> Result<(), NativeError> {
    if unit.len() < 6 || unit.len() > MAX_UNIT_LEN {
        return Err(NativeError::invalid(format!(
            "invalid unit name {unit:?} (expected 6-{MAX_UNIT_LEN} characters)"
        )));
    }
    let Some(rest) = unit.strip_prefix("rast-") else {
        return Err(NativeError::invalid(format!(
            "invalid unit name {unit:?}: must start with rast-"
        )));
    };
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return Err(NativeError::invalid("invalid unit name"));
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Err(NativeError::invalid(format!(
            "invalid unit name {unit:?}: must start with a lowercase letter or digit"
        )));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(NativeError::invalid(format!(
            "invalid unit name {unit:?}: only lowercase letters, digits and '-' are allowed"
        )));
    }
    if unit.ends_with(".service") {
        return Err(NativeError::invalid(
            "unit names must not include the .service suffix",
        ));
    }
    Ok(())
}

/// Transient unit name of one user's graphical session.
pub fn session_unit(user_id: i64) -> Result<String, NativeError> {
    if !(1..=MAX_USER_ID).contains(&user_id) {
        return Err(NativeError::invalid(format!(
            "userId must be between 1 and {MAX_USER_ID}"
        )));
    }
    Ok(format!("rast-xpra-u{user_id}"))
}

/// Managed unit prefixes; `units_list`, `unit_status` and `app_stop` only ever
/// address these. In particular `rast-worker.service` (Docker) is never
/// reachable through the native helper.
pub const MANAGED_UNIT_PREFIXES: &[&str] = &["rast-xpra-u", "rast-app-"];

pub fn is_managed_unit(unit: &str) -> bool {
    MANAGED_UNIT_PREFIXES
        .iter()
        .any(|prefix| unit.starts_with(prefix))
}

/// Validate an app unit name: same regex plus the `rast-app-` prefix.
pub fn validate_app_unit(unit: &str) -> Result<(), NativeError> {
    validate_unit(unit)?;
    if !unit.starts_with("rast-app-") {
        return Err(NativeError::invalid(format!(
            "invalid application unit {unit:?}: must start with rast-app-"
        )));
    }
    Ok(())
}

/// Validate a unit that a read-only status query may address.
pub fn validate_managed_unit(unit: &str) -> Result<(), NativeError> {
    validate_unit(unit)?;
    if !is_managed_unit(unit) {
        return Err(NativeError::forbidden(format!(
            "{unit:?} is not a native helper unit"
        )));
    }
    Ok(())
}

pub fn validate_display(display: u32) -> Result<(), NativeError> {
    if !(100..=999).contains(&display) {
        return Err(NativeError::invalid(
            "display must be between 100 and 999".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_port(port: u32) -> Result<(), NativeError> {
    if !(1024..=65535).contains(&port) {
        return Err(NativeError::invalid("port must be between 1024 and 65535"));
    }
    Ok(())
}

/// App ids are server generated: lowercase alphanumerics with interior dashes.
pub fn validate_app_id(app_id: &str) -> Result<(), NativeError> {
    if app_id.is_empty() || app_id.len() > 64 {
        return Err(NativeError::invalid(
            "invalid appId: expected 1-64 characters",
        ));
    }
    let bytes = app_id.as_bytes();
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Err(NativeError::invalid(
            "invalid appId: must start with a lowercase letter or digit",
        ));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(NativeError::invalid(
            "invalid appId: only lowercase letters, digits and '-' are allowed",
        ));
    }
    Ok(())
}

/// Run ids are lowercase hex (server generated, bounded).
pub fn validate_run_id(run_id: &str) -> Result<(), NativeError> {
    if run_id.is_empty() || run_id.len() > 64 {
        return Err(NativeError::invalid(
            "invalid runId: expected 1-64 characters",
        ));
    }
    if !run_id
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(NativeError::invalid(
            "invalid runId: expected lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9+.-]{0,127}$` (Debian package name).
pub fn validate_package_name(name: &str) -> Result<(), NativeError> {
    if name.is_empty() || name.len() > MAX_PACKAGE_NAME_LEN {
        return Err(NativeError::invalid(format!(
            "invalid package name {name:?}: expected 1-{MAX_PACKAGE_NAME_LEN} characters"
        )));
    }
    let bytes = name.as_bytes();
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Err(NativeError::invalid(format!(
            "invalid package name {name:?}: must start with a lowercase letter or digit"
        )));
    }
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'.' | b'-')
    }) {
        return Err(NativeError::invalid(format!(
            "invalid package name {name:?}: only lowercase letters, digits and + . - are allowed"
        )));
    }
    Ok(())
}

/// Validate and de-duplicate a package list.
pub fn validate_packages(packages: &[String]) -> Result<Vec<String>, NativeError> {
    if packages.is_empty() {
        return Err(NativeError::invalid(
            "deb_remove requires at least one package",
        ));
    }
    if packages.len() > MAX_PACKAGES {
        return Err(NativeError::invalid(format!(
            "deb_remove accepts at most {MAX_PACKAGES} packages"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(packages.len());
    for package in packages {
        validate_package_name(package)?;
        if !out.contains(package) {
            out.push(package.clone());
        }
    }
    Ok(out)
}

/// Validate a requested limit set against its configured maximum.
pub fn validate_limits(
    requested: ResourceLimits,
    maximum: ResourceLimits,
    what: &str,
) -> Result<(), NativeError> {
    if !(128..=262_144).contains(&requested.memory_max_mb) {
        return Err(NativeError::invalid(
            "memoryMaxMb must be between 128 and 262144",
        ));
    }
    if !(1..=6400).contains(&requested.cpu_quota_percent) {
        return Err(NativeError::invalid(
            "cpuQuotaPercent must be between 1 and 6400",
        ));
    }
    if !(16..=131_072).contains(&requested.tasks_max) {
        return Err(NativeError::invalid(
            "tasksMax must be between 16 and 131072",
        ));
    }
    if requested.memory_max_mb > maximum.memory_max_mb {
        return Err(NativeError::invalid(format!(
            "{what} memoryMaxMb {} exceeds the configured maximum {}",
            requested.memory_max_mb, maximum.memory_max_mb
        )));
    }
    if requested.cpu_quota_percent > maximum.cpu_quota_percent {
        return Err(NativeError::invalid(format!(
            "{what} cpuQuotaPercent {} exceeds the configured maximum {}",
            requested.cpu_quota_percent, maximum.cpu_quota_percent
        )));
    }
    if requested.tasks_max > maximum.tasks_max {
        return Err(NativeError::invalid(format!(
            "{what} tasksMax {} exceeds the configured maximum {}",
            requested.tasks_max, maximum.tasks_max
        )));
    }
    Ok(())
}

/// Environment variables passed to a transient unit: `^[A-Z_][A-Z0-9_]{0,63}$`
/// keys, bounded values, no NUL bytes. There is no shell, so metacharacters in
/// values are allowed and never interpreted.
pub fn validate_env(env: &BTreeMap<String, String>) -> Result<(), NativeError> {
    if env.len() > MAX_ENV_VARS {
        return Err(NativeError::invalid(format!(
            "at most {MAX_ENV_VARS} environment variables are allowed"
        )));
    }
    for (key, value) in env {
        validate_env_key(key)?;
        if value.contains('\0') {
            return Err(NativeError::invalid(format!(
                "environment value for {key} contains a NUL byte"
            )));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(NativeError::invalid(format!(
                "environment value for {key} exceeds {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn validate_env_key(key: &str) -> Result<(), NativeError> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return Err(NativeError::invalid(format!(
            "invalid environment variable name {key:?}: expected 1-64 characters"
        )));
    }
    if !(bytes[0].is_ascii_uppercase() || bytes[0] == b'_') {
        return Err(NativeError::invalid(format!(
            "invalid environment variable name {key:?}: must start with A-Z or _"
        )));
    }
    if !bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Err(NativeError::invalid(format!(
            "invalid environment variable name {key:?}: only A-Z, 0-9 and _ are allowed"
        )));
    }
    Ok(())
}

/// Session argv entries (passed verbatim after the configured Xpra binary).
pub fn validate_session_args(args: &[String]) -> Result<(), NativeError> {
    if args.len() > MAX_ARGV_ENTRIES {
        return Err(NativeError::invalid(format!(
            "at most {MAX_ARGV_ENTRIES} session arguments are allowed"
        )));
    }
    for (index, arg) in args.iter().enumerate() {
        if arg.len() > MAX_ARGV_BYTES {
            return Err(NativeError::invalid(format!(
                "session argument {index} exceeds {MAX_ARGV_BYTES} bytes"
            )));
        }
        if arg.contains('\0') {
            return Err(NativeError::invalid(format!(
                "session argument {index} contains a NUL byte"
            )));
        }
    }
    Ok(())
}

/// Application argv: 1-64 entries, bounded, `argv[0]` absolute and inside an
/// allow-listed binary location. Arguments are separate `OsString`s and are
/// never interpreted by a shell.
pub fn validate_argv(argv: &[String]) -> Result<(), NativeError> {
    if argv.is_empty() {
        return Err(NativeError::invalid("argv requires at least one entry"));
    }
    if argv.len() > MAX_ARGV_ENTRIES {
        return Err(NativeError::invalid(format!(
            "argv accepts at most {MAX_ARGV_ENTRIES} entries"
        )));
    }
    for (index, arg) in argv.iter().enumerate() {
        if arg.len() > MAX_ARGV_BYTES {
            return Err(NativeError::invalid(format!(
                "argv entry {index} exceeds {MAX_ARGV_BYTES} bytes"
            )));
        }
        if arg.contains('\0') {
            return Err(NativeError::invalid(format!(
                "argv entry {index} contains a NUL byte"
            )));
        }
    }
    if !argv[0].starts_with('/') {
        return Err(NativeError::invalid(format!(
            "argv[0] {:?} must be an absolute path",
            argv[0]
        )));
    }
    Ok(())
}

/// Absolute binary locations a native application may be started from. The app
/// trees live under `<data_dir>/linux-apps` (profiles, AppImages, unpacked
/// installs); they are checked separately because they depend on config.
const ARGV0_PREFIXES: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/usr/games",
    "/opt",
    "/snap/bin",
];

/// Validate `argv[0]`: absolute, inside an allow-listed prefix, and after
/// canonicalisation still inside one (a symlink cannot smuggle in a binary from
/// an unrelated location). A non-existent target is left to systemd to report.
pub fn validate_argv0(ctx: &ValidationContext, argv0: &str) -> Result<(), NativeError> {
    if argv0.is_empty() || argv0.len() > MAX_PATH_BYTES {
        return Err(NativeError::invalid("invalid argv[0] path length"));
    }
    if argv0.contains('\0') {
        return Err(NativeError::invalid("argv[0] contains a NUL byte"));
    }
    let path = Path::new(argv0);
    if !path.is_absolute() {
        return Err(NativeError::invalid("argv[0] must be an absolute path"));
    }
    if !is_clean_absolute(path) {
        return Err(NativeError::invalid(
            "argv[0] must not contain '.' or '..' path segments",
        ));
    }
    let data_roots = [ctx.roots.linux_apps(), ctx.roots.linux_users()];
    let literal_ok = ARGV0_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
        || data_roots.iter().any(|root| path.starts_with(root));
    if !literal_ok {
        return Err(NativeError::forbidden(format!(
            "argv[0] {argv0:?} is not inside an allowed binary location"
        )));
    }
    match std::fs::canonicalize(path) {
        Ok(resolved) => {
            let allowed = ARGV0_PREFIXES
                .iter()
                .map(PathBuf::from)
                .chain(data_roots.iter().cloned())
                .collect::<Vec<_>>();
            let mut canonical_roots: Vec<PathBuf> = Vec::new();
            for prefix in &allowed {
                if let Ok(canonical) = std::fs::canonicalize(prefix) {
                    canonical_roots.push(canonical);
                }
            }
            if canonical_roots
                .iter()
                .any(|root| resolved.starts_with(root))
            {
                Ok(())
            } else {
                Err(NativeError::forbidden(format!(
                    "argv[0] {argv0:?} resolves outside the allowed binary locations"
                )))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NativeError::invalid(format!(
            "cannot resolve argv[0] {argv0:?}: {error}"
        ))),
    }
}

/// The configured Xpra binary. Requests may only start exactly this binary.
pub fn validate_xpra_bin(ctx: &ValidationContext, raw: &str) -> Result<PathBuf, NativeError> {
    let path = validate_absolute_path(raw)?;
    let configured = std::fs::canonicalize(&ctx.xpra_bin).map_err(|error| {
        NativeError::unavailable(format!(
            "configured xpra binary {} is not available: {error}",
            ctx.xpra_bin.display()
        ))
    })?;
    let requested = std::fs::canonicalize(&path).map_err(|_| {
        NativeError::invalid(format!(
            "xpraBin must be the configured Xpra binary ({})",
            ctx.xpra_bin.display()
        ))
    })?;
    if requested != configured {
        return Err(NativeError::forbidden(format!(
            "xpraBin must be {}",
            ctx.xpra_bin.display()
        )));
    }
    Ok(requested)
}

/// A session runtime directory under `<run_dir>` (created by the caller if
/// missing, but always validated as a clean, absolute path).
pub fn validate_runtime_dir(ctx: &ValidationContext, raw: &str) -> Result<PathBuf, NativeError> {
    let path = validate_managed_path(ctx, raw, PathPurpose::Mkdir)?;
    let run_root = canonical_root(&ctx.roots.run_dir);
    if let Some(root) = run_root {
        if !path.starts_with(&root) {
            return Err(NativeError::forbidden(
                "runtimeDir must be inside the RAST runtime directory",
            ));
        }
    } else if !path.starts_with(&ctx.roots.run_dir) {
        return Err(NativeError::forbidden(
            "runtimeDir must be inside the RAST runtime directory",
        ));
    }
    if raw.split('/').filter(|part| !part.is_empty()).count() < 3 {
        return Err(NativeError::invalid(
            "runtimeDir must be a per-user subdirectory of the runtime directory",
        ));
    }
    Ok(path)
}

/// Working directory of an application process: an existing directory inside a
/// managed root.
pub fn validate_cwd(ctx: &ValidationContext, raw: &str) -> Result<PathBuf, NativeError> {
    let path = validate_managed_path(ctx, raw, PathPurpose::Chown)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
        NativeError::invalid(format!(
            "cwd {raw:?} does not exist; create it before starting the application"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(NativeError::invalid("cwd must be a real directory"));
    }
    Ok(path)
}

/// Symlink target for `path_symlink`: an absolute, clean path inside a managed
/// root (or the person's RAST home). The target itself may not exist yet.
pub fn validate_symlink_target(ctx: &ValidationContext, raw: &str) -> Result<PathBuf, NativeError> {
    let path = validate_absolute_path(raw)?;
    for root in ctx.roots.roots(PathPurpose::Acl) {
        let canonical = std::fs::canonicalize(&root).ok();
        if path.starts_with(&root) {
            return Ok(path);
        }
        if let Some(canonical) = canonical {
            if path.starts_with(&canonical) {
                return Ok(path);
            }
        }
    }
    Err(NativeError::forbidden(format!(
        "symlink target {raw:?} is outside the managed directories"
    )))
}

/// Octal permission bits accepted by `path_mkdir` / `path_chmod`.
pub fn validate_mode(mode: u32) -> Result<(), NativeError> {
    if mode & !0o777 != 0 {
        return Err(NativeError::invalid(
            "mode must be between 0000 and 0777 (setuid/setgid/sticky are refused)",
        ));
    }
    Ok(())
}

/// `path_acl` permission letters: a non-empty subset of `rwx`, no duplicates.
pub fn validate_acl_perms(perms: &str) -> Result<(), NativeError> {
    if perms.is_empty() || perms.len() > 3 {
        return Err(NativeError::invalid(
            "perms must be a combination of r, w and x",
        ));
    }
    let mut seen = [false; 3];
    for byte in perms.bytes() {
        let index = match byte {
            b'r' => 0,
            b'w' => 1,
            b'x' => 2,
            _ => {
                return Err(NativeError::invalid(
                    "perms must be a combination of r, w and x",
                ))
            }
        };
        if seen[index] {
            return Err(NativeError::invalid("perms must not repeat a letter"));
        }
        seen[index] = true;
    }
    Ok(())
}

/// Managed uid/gid range: `uid_base + 1 ..= uid_base + MAX_USER_ID`. Ownership
/// changes cannot target arbitrary host accounts.
pub fn validate_managed_uid(
    ctx: &ValidationContext,
    uid: u32,
    gid: u32,
) -> Result<(), NativeError> {
    let low = ctx.uid_base.saturating_add(1);
    let high = ctx.uid_base.saturating_add(MAX_USER_ID as u32);
    if !(low..=high).contains(&uid) || !(low..=high).contains(&gid) {
        return Err(NativeError::forbidden(format!(
            "uid/gid {uid}/{gid} is outside the managed native range {low}..={high}"
        )));
    }
    Ok(())
}

/// Lowercase hexadecimal SHA-256.
pub fn validate_sha256(value: &str) -> Result<(), NativeError> {
    if value.len() != 64 {
        return Err(NativeError::invalid(
            "sha256 must be 64 lowercase hexadecimal characters",
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(NativeError::invalid(
            "sha256 must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

/// Debian package version string (`[epoch:]upstream[-revision]`), bounded and
/// free of shell/apt metacharacters. Used only to pin `apt-get download`.
pub fn validate_deb_version(version: &str) -> Result<(), NativeError> {
    if version.is_empty() || version.len() > 128 {
        return Err(NativeError::invalid(
            "invalid package version: expected 1-128 characters",
        ));
    }
    if !version.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'~' | b':' | b'-' | b'_')
    }) {
        return Err(NativeError::invalid(
            "invalid package version: only letters, digits and . + ~ : - _ are allowed",
        ));
    }
    Ok(())
}

/// Validate a `.deb` upload path. It must be absolute, end in `.deb`, be a
/// regular file (never a symlink) between 1 byte and 4 GiB, and live in the
/// staging tree under `<data_dir>/linux-apps` so the web server cannot point
/// the root helper at arbitrary files on the host.
pub fn validate_deb_path(ctx: &ValidationContext, raw: &str) -> Result<PathBuf, NativeError> {
    if raw.len() > MAX_PATH_BYTES {
        return Err(NativeError::invalid("path is too long"));
    }
    if raw.contains('\0') {
        return Err(NativeError::invalid("path contains a NUL byte"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() || !is_clean_absolute(path) {
        return Err(NativeError::invalid(
            "path must be an absolute path without '.' or '..' segments",
        ));
    }
    if !raw.ends_with(".deb") {
        return Err(NativeError::invalid_deb("package path must end with .deb"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| NativeError::invalid_deb(format!("cannot read {raw}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(NativeError::invalid_deb(
            "package path must be a regular file (symlinks are refused)",
        ));
    }
    if metadata.len() == 0 || metadata.len() > MAX_DEB_BYTES {
        return Err(NativeError::invalid_deb(format!(
            "package size {} bytes is outside the allowed range (1 byte .. 4 GiB)",
            metadata.len()
        )));
    }
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| NativeError::invalid_deb(format!("cannot resolve {raw}: {error}")))?;
    let apps_root = canonical_root(&ctx.roots.linux_apps())
        .ok_or_else(|| NativeError::unavailable("the Linux apps directory does not exist yet"))?;
    if !resolved.starts_with(&apps_root) {
        return Err(NativeError::forbidden(format!(
            "package path must be inside {}",
            ctx.roots.linux_apps().display()
        )));
    }
    Ok(resolved)
}

/// Resolve and validate a path from a request for one of the managed
/// operations.
pub fn validate_managed_path(
    ctx: &ValidationContext,
    raw: &str,
    purpose: PathPurpose,
) -> Result<PathBuf, NativeError> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES {
        return Err(NativeError::invalid("invalid path length"));
    }
    if raw.contains('\0') {
        return Err(NativeError::invalid("path contains a NUL byte"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(NativeError::invalid("path must be absolute"));
    }
    if !is_clean_absolute(path) {
        return Err(NativeError::invalid(
            "path must not contain '.' or '..' segments",
        ));
    }
    let mut resolved: Option<(PathBuf, PathBuf)> = None;
    for root in ctx.roots.roots(purpose) {
        let literal = root.clone();
        let Ok(canonical_root) = std::fs::canonicalize(&root) else {
            continue;
        };
        let matches = path.starts_with(&literal) || path.starts_with(&canonical_root);
        if !matches {
            continue;
        }
        // Removal must be able to unlink a final symlink itself, so only the
        // parent is resolved for that purpose.
        let subject = if purpose == PathPurpose::Remove {
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => continue,
            }
        } else {
            path
        };
        if let Some(mut candidate) = resolve_existing_prefix(subject, &canonical_root)? {
            if purpose == PathPurpose::Remove {
                if let Some(name) = path.file_name() {
                    candidate.push(name);
                } else {
                    continue;
                }
            }
            resolved = Some((candidate, canonical_root));
            break;
        }
    }
    let Some((candidate, canonical_root)) = resolved else {
        return Err(NativeError::forbidden(format!(
            "path {raw:?} is not inside a managed directory"
        )));
    };
    if candidate == canonical_root {
        return Err(NativeError::forbidden(
            "path must point inside a managed directory, not at its root",
        ));
    }
    // The final component may not be a symlink for operations that would
    // follow it. Removal unlinks a final symlink itself and is allowed.
    if purpose != PathPurpose::Remove {
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NativeError::forbidden("path must not be a symbolic link"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(NativeError::unavailable(format!(
                    "cannot inspect {}: {error}",
                    candidate.display()
                )))
            }
        }
    }
    Ok(candidate)
}

fn canonical_root(root: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(root).ok()
}

/// True when every component is a plain name (no `.`, no `..`, no empty
/// component except the leading slash, no trailing slash).
fn is_clean_absolute(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Some(text) = path.to_str() else {
        return false;
    };
    if text.len() > MAX_PATH_BYTES {
        return false;
    }
    if text.len() > 1 && text.ends_with('/') {
        return false;
    }
    let mut parts = path.components();
    matches!(parts.next(), Some(Component::RootDir))
        && parts.all(|component| matches!(component, Component::Normal(_)))
}

/// Resolve `path` (known to be under `root` textually) by canonicalising the
/// nearest existing ancestor and appending the remaining (clean) components.
/// This both blocks symlink escapes and allows paths whose final components do
/// not exist yet.
fn resolve_existing_prefix(
    path: &Path,
    canonical_root: &Path,
) -> Result<Option<PathBuf>, NativeError> {
    if !path.starts_with(canonical_root) {
        // The path is expressed with a non-canonical root prefix (e.g. the data
        // dir is a symlink). Rebuild the relative part from the textual root.
        return Ok(None);
    }
    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(NativeError::unavailable(format!(
                        "cannot resolve {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        let Some(name) = ancestor.file_name().map(|name| name.to_os_string()) else {
            return Ok(None);
        };
        tail.push(name);
        if !ancestor.pop() {
            return Ok(None);
        }
    }
    let canonical = std::fs::canonicalize(&ancestor).map_err(|error| {
        NativeError::unavailable(format!("cannot resolve {}: {error}", path.display()))
    })?;
    if !canonical.starts_with(canonical_root) {
        return Err(NativeError::forbidden("path escapes the managed directory"));
    }
    let mut out = canonical;
    for name in tail.into_iter().rev() {
        out.push(name);
    }
    Ok(Some(out))
}

/// Re-resolve a path whose textual prefix is the *literal* (possibly symlinked)
/// root rather than the canonical one. Returns `None` when the path does not
/// share this root textually.
pub fn resolve_path_under_literal_root(
    path: &Path,
    literal_root: &Path,
) -> Result<Option<PathBuf>, NativeError> {
    let Ok(canonical_root) = std::fs::canonicalize(literal_root) else {
        return Ok(None);
    };
    if !path.starts_with(literal_root) {
        return Ok(None);
    }
    let Ok(relative) = path.strip_prefix(literal_root) else {
        return Ok(None);
    };
    let canonical = canonical_root.join(relative);
    if !canonical.starts_with(&canonical_root) {
        return Ok(None);
    }
    resolve_existing_prefix(&canonical, &canonical_root)
}

/// Validate an absolute path string without touching the filesystem.
pub fn validate_absolute_path(raw: &str) -> Result<PathBuf, NativeError> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES {
        return Err(NativeError::invalid("invalid path length"));
    }
    if raw.contains('\0') {
        return Err(NativeError::invalid("path contains a NUL byte"));
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(NativeError::invalid(format!(
            "path {raw:?} must be absolute"
        )));
    }
    if !is_clean_absolute(&path) {
        return Err(NativeError::invalid(
            "path must not contain '.' or '..' segments",
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(data_dir: &Path, run_dir: &Path) -> ValidationContext {
        ValidationContext {
            roots: ManagedRoots::new(data_dir, run_dir),
            xpra_bin: PathBuf::from("/usr/bin/xpra"),
            uid_base: 200_000,
            session_limits: ResourceLimits::from_config(4096, 4, 2048),
            app_limits: ResourceLimits::from_config(2048, 2, 1024),
        }
    }

    fn ctx(temp: &tempfile::TempDir) -> ValidationContext {
        let data = temp.path().join("data");
        let run = temp.path().join("run");
        std::fs::create_dir_all(data.join("linux-apps")).unwrap();
        std::fs::create_dir_all(data.join("linux-users")).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        ctx_with(&data, &run)
    }

    #[test]
    fn request_line_roundtrip_for_every_op() {
        let requests = vec![
            NativeOp::Ping,
            NativeOp::IdentityEnsure(IdentityEnsureOp {
                user_id: 7,
                username: None,
            }),
            NativeOp::IdentityRemove(IdentityRemoveOp {
                user_id: 7,
                remove_home: true,
            }),
            NativeOp::SessionStart(SessionStartOp {
                user_id: 7,
                display: 100,
                port: 14500,
                xpra_bin: "/usr/bin/xpra".into(),
                runtime_dir: "/run/rast/xpra/u7".into(),
                args: vec!["--bind-tcp=127.0.0.1:14500".into()],
                env: BTreeMap::from([("DISPLAY".to_string(), ":100".to_string())]),
                memory_max_mb: 1024,
                cpu_quota_percent: 200,
                tasks_max: 512,
            }),
            NativeOp::SessionStop(SessionStopOp { user_id: 7 }),
            NativeOp::UnitStatus(UnitStatusOp {
                unit: "rast-app-demo".into(),
            }),
            NativeOp::UnitsList,
            NativeOp::AppStart(AppStartOp {
                user_id: 7,
                app_id: "demo".into(),
                run_id: "abc123".into(),
                unit: "rast-app-demo-abc123".into(),
                argv: vec!["/usr/bin/firefox".into(), "--new-window".into()],
                cwd: "/run/rast/apps/u7".into(),
                env: BTreeMap::new(),
                memory_max_mb: 512,
                cpu_quota_percent: 100,
                tasks_max: 128,
            }),
            NativeOp::AppStop(AppStopOp {
                unit: "rast-app-demo-abc123".into(),
            }),
            NativeOp::DebPreview(DebPreviewOp {
                path: "/var/lib/rast/linux-apps/staging/x.deb".into(),
                sha256: "a".repeat(64),
            }),
            NativeOp::DebInstall(DebInstallOp {
                path: "/var/lib/rast/linux-apps/staging/x.deb".into(),
                sha256: "a".repeat(64),
            }),
            NativeOp::DebRemove(DebRemoveOp {
                packages: vec!["google-chrome-stable".into()],
                purge: false,
            }),
            NativeOp::DebDownload(DebDownloadOp {
                package: "google-chrome-stable".into(),
                dest: "/var/lib/rast/linux-apps/staging".into(),
                version: Some("157.0.8103.78".into()),
            }),
            NativeOp::PackagePolicy(PackagePolicyOp {
                package: "google-chrome-stable".into(),
            }),
            NativeOp::PathMkdir(PathMkdirOp {
                path: "/var/lib/rast/linux-users/7".into(),
                uid: 200_007,
                gid: 200_007,
                mode: 0o700,
            }),
            NativeOp::PathChown(PathChownOp {
                path: "/var/lib/rast/linux-users/7".into(),
                uid: 200_007,
                gid: 200_007,
                recursive: true,
            }),
            NativeOp::PathRemove(PathRemoveOp {
                path: "/var/lib/rast/linux-users/7".into(),
            }),
            NativeOp::PathChmod(PathChmodOp {
                path: "/var/lib/rast/linux-users/7".into(),
                mode_octal: 0o700,
                recursive: true,
            }),
            NativeOp::PathAcl(PathAclOp {
                path: "/srv/rast/homes/alice".into(),
                uid: 200_007,
                perms: "rwx".into(),
                recursive: false,
            }),
            NativeOp::PackageInstalledList(PackageInstalledListOp {
                names: vec!["bash".into(), "curl".into()],
            }),
            NativeOp::PackageFiles(PackageFilesOp {
                package: "bash".into(),
            }),
            NativeOp::PathSymlink(PathSymlinkOp {
                link: "/var/lib/rast/linux-apps/profiles/7/desktop".into(),
                target: "/usr/share/applications".into(),
            }),
            NativeOp::ProfilePrepare(ProfilePrepareOp {
                app_id: "demo".into(),
                user_id: 7,
                uid: 200_007,
                gid: 200_007,
                home: "/var/lib/rast/linux-users/7".into(),
                rast_home: "/srv/rast/homes/alice".into(),
            }),
            NativeOp::ProfileRemove(ProfileRemoveOp {
                app_id: "demo".into(),
                user_id: 7,
            }),
            NativeOp::AptUpdate,
        ];
        for (index, op) in requests.into_iter().enumerate() {
            let request = NativeRequest::new(index as u64 + 1, op.clone());
            let line = request.to_line().unwrap();
            assert!(line.len() <= MAX_REQUEST_LINE_BYTES);
            assert!(line.ends_with(b"\n"));
            let parsed = parse_request_line(&line[..line.len() - 1]).unwrap();
            assert_eq!(parsed, request, "{:?}", op.name());
        }
    }

    #[test]
    fn wire_shape_uses_documented_camel_case_fields() {
        let request = NativeRequest::new(
            42,
            NativeOp::IdentityRemove(IdentityRemoveOp {
                user_id: 3,
                remove_home: true,
            }),
        );
        let line = request.to_line().unwrap();
        let value: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(value["id"], 42);
        assert_eq!(value["op"], "identity_remove");
        assert_eq!(value["userId"], 3);
        assert_eq!(value["removeHome"], true);
        assert!(value.get("user_id").is_none());
    }

    #[test]
    fn responses_have_the_documented_shape() {
        let ok = NativeResponse::success(Some(1), serde_json::json!({"version": 1}));
        let value: Value = serde_json::from_slice(&ok.to_line().unwrap()).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["version"], 1);
        assert!(value["error"].is_null());

        let err = NativeResponse::failure(Some(2), &NativeError::invalid("nope"));
        let value: Value = serde_json::from_slice(&err.to_line().unwrap()).unwrap();
        assert_eq!(value["id"], 2);
        assert_eq!(value["ok"], false);
        assert!(value["data"].is_null());
        assert_eq!(value["error"]["code"], "invalid_request");

        let unknown = NativeResponse::failure(None, &NativeError::unavailable("down"));
        let value: Value = serde_json::from_slice(&unknown.to_line().unwrap()).unwrap();
        assert!(value["id"].is_null());
    }

    #[test]
    fn unknown_operations_and_extra_fields_are_rejected() {
        match parse_request_line(br#"{"id":1,"op":"run_shell","cmd":"id"}"#) {
            Err(RequestParseError::Unsupported(message)) => {
                assert!(message.contains("run_shell"), "{message}");
                assert!(message.contains("restart rast-native.service"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        for line in [
            br#"{"id":1,"op":"ping","extra":true}"#.as_slice(),
            br#"{"id":1,"op":"identity_ensure","userId":3,"uid":1}"#.as_slice(),
            br#"{"id":1,"op":"path_remove","path":"/x","recursive":true}"#.as_slice(),
        ] {
            assert!(
                matches!(parse_request_line(line), Err(RequestParseError::Invalid(_))),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        for (line, code) in [
            (b"".as_slice(), "invalid_request"),
            (b"not json".as_slice(), "invalid_request"),
            (b"[]".as_slice(), "invalid_request"),
            (br#"{"op":"ping"}"#.as_slice(), "invalid_request"),
            (br#"{"id":1}"#.as_slice(), "invalid_request"),
            (br#"{"id":"one","op":"ping"}"#.as_slice(), "invalid_request"),
            (br#"{"id":-1,"op":"ping"}"#.as_slice(), "invalid_request"),
            (br#"{"id":1,"op":3}"#.as_slice(), "invalid_request"),
        ] {
            let error = parse_request_line(line).unwrap_err();
            assert_eq!(error.code(), code, "{:?}", String::from_utf8_lossy(line));
        }
        let parsed = parse_request_line(br#"{"id":18446744073709551615,"op":"ping"}"#).unwrap();
        assert_eq!(parsed.id, u64::MAX);
    }

    #[test]
    fn usernames_follow_the_identity_scheme() {
        assert_eq!(username_for(1), "rast-u1");
        assert_eq!(username_for(1_000_000), "rast-u1000000");
        for ok in ["rast-u1", "rast-u42", "rast-u1000000"] {
            assert!(validate_username(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "rast-u",
            "rast-u0",
            "rast-u01",
            "rast-u-1",
            "rast-u1x",
            "rast-U1",
            "alice",
            "rast-u12345678901",
        ] {
            assert!(validate_username(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn identity_ids_are_derived_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let ids = identity_ids(&ctx, 1).unwrap();
        assert_eq!(ids.uid, 200_001);
        assert_eq!(ids.gid, 200_001);
        let ids = identity_ids(&ctx, MAX_USER_ID).unwrap();
        assert_eq!(ids.uid, 1_200_000);
        for bad in [0, -1, MAX_USER_ID + 1] {
            assert!(identity_ids(&ctx, bad).is_err(), "{bad}");
        }
        let mut overflow = ctx.clone();
        overflow.uid_base = u32::MAX - 10;
        assert!(identity_ids(&overflow, 100).is_err());
    }

    #[test]
    fn unit_names_are_strict() {
        for ok in ["rast-xpra-u1", "rast-app-demo", "rast-a"] {
            assert!(validate_unit(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "rast-",
            "xpra-u1",
            "rast-APP",
            "rast-app_demo",
            "rast-app/x",
            "rast-app\0",
            "rast-a.service",
            &format!("rast-{}", "a".repeat(MAX_UNIT_LEN)),
        ] {
            assert!(validate_unit(bad).is_err(), "{bad:?}");
        }
        assert_eq!(session_unit(3).unwrap(), "rast-xpra-u3");
        assert!(session_unit(0).is_err());
        assert!(validate_app_unit("rast-app-demo").is_ok());
        assert!(validate_app_unit("rast-xpra-u1").is_err());
        assert!(validate_managed_unit("rast-xpra-u1").is_ok());
        assert!(validate_managed_unit("rast-worker").is_err());
        assert!(validate_managed_unit("rast-app-foo").is_ok());
    }

    #[test]
    fn display_and_port_ranges() {
        assert!(validate_display(100).is_ok());
        assert!(validate_display(999).is_ok());
        for bad in [0, 99, 1000, u32::MAX] {
            assert!(validate_display(bad).is_err(), "{bad}");
        }
        assert!(validate_port(1024).is_ok());
        assert!(validate_port(65535).is_ok());
        for bad in [0, 1023, 65536] {
            assert!(validate_port(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn app_ids_and_run_ids_are_strict_hex_and_slugs() {
        assert!(validate_app_id("demo").is_ok());
        assert!(validate_app_id("demo-app2").is_ok());
        for bad in ["", "Demo", "demo_app", "../demo", "-demo"] {
            assert!(validate_app_id(bad).is_err(), "{bad:?}");
        }
        assert!(validate_run_id("abc123").is_ok());
        assert!(validate_run_id(&"a".repeat(64)).is_ok());
        for bad in ["", "ABC", "abc-123", "../x", "abc\n", &"a".repeat(65)] {
            assert!(validate_run_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn package_names_are_strict_and_deduplicated() {
        for ok in [
            "bash",
            "google-chrome-stable",
            "libc6",
            "g++",
            "python3.12",
            "0ad",
        ] {
            assert!(validate_package_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-bash",
            "Bash",
            "a/b",
            "../x",
            "a b",
            "a;b",
            "a\0b",
            &"a".repeat(MAX_PACKAGE_NAME_LEN + 1),
        ] {
            assert!(validate_package_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_packages(&[]).is_err());
        let many: Vec<String> = (0..MAX_PACKAGES + 1).map(|_| "bash".to_string()).collect();
        assert!(validate_packages(&many).is_err());
        let dedup =
            validate_packages(&["bash".to_string(), "bash".to_string(), "curl".to_string()])
                .unwrap();
        assert_eq!(dedup, vec!["bash", "curl"]);
    }

    #[test]
    fn environment_variables_are_bounded() {
        let mut env = BTreeMap::new();
        env.insert("DISPLAY".to_string(), ":100".to_string());
        env.insert("XDG_RUNTIME_DIR".to_string(), "/run/user/1".to_string());
        env.insert("LC_ALL".to_string(), "C.UTF-8".to_string());
        assert!(validate_env(&env).is_ok());
        // Shell metacharacters are data, not code.
        env.insert("WEIRD".to_string(), "$(id); `id` | cat".to_string());
        assert!(validate_env(&env).is_ok());

        for bad_key in ["", "lower", "1KEY", "KEY-VAR", "KEY VAR", &"K".repeat(65)] {
            let mut env = BTreeMap::new();
            env.insert(bad_key.to_string(), "x".to_string());
            assert!(validate_env(&env).is_err(), "{bad_key:?}");
        }
        let mut env = BTreeMap::new();
        env.insert("KEY".to_string(), "a\0b".to_string());
        assert!(validate_env(&env).is_err());
        let mut env = BTreeMap::new();
        env.insert("KEY".to_string(), "a".repeat(MAX_ENV_VALUE_BYTES + 1));
        assert!(validate_env(&env).is_err());
        let env: BTreeMap<String, String> = (0..MAX_ENV_VARS + 1)
            .map(|index| (format!("KEY_{index}"), "v".to_string()))
            .collect();
        assert!(validate_env(&env).is_err());
    }

    #[test]
    fn argv_entries_are_bounded_and_never_shell_strings() {
        assert!(validate_argv(&["/usr/bin/firefox".into()]).is_ok());
        // Metacharacters are allowed: argv is passed to execve directly.
        assert!(validate_argv(&[
            "/usr/bin/printf".into(),
            "%s; rm -rf /".into(),
            "$(id)".into(),
            "|".into(),
        ])
        .is_ok());
        assert!(validate_argv(&[]).is_err());
        assert!(validate_argv(&["relative".into()]).is_err());
        assert!(validate_argv(&["/usr/bin/x".into(), "a\0b".into()]).is_err());
        assert!(validate_argv(&["/usr/bin/x".into(), "a".repeat(MAX_ARGV_BYTES + 1)]).is_err());
        let too_many: Vec<String> = (0..MAX_ARGV_ENTRIES + 1)
            .map(|_| "/usr/bin/x".to_string())
            .collect();
        assert!(validate_argv(&too_many).is_err());
    }

    #[test]
    fn session_arguments_allow_flags_but_not_nul() {
        assert!(validate_session_args(&["--bind-tcp=127.0.0.1:14500".into()]).is_ok());
        assert!(validate_session_args(&["a;b".into()]).is_ok());
        assert!(validate_session_args(&["a\0b".into()]).is_err());
        assert!(validate_session_args(&["a".repeat(MAX_ARGV_BYTES + 1)]).is_err());
    }

    #[test]
    fn argv0_allowlist_blocks_other_locations() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        for ok in [
            "/usr/bin/firefox",
            "/bin/sh",
            "/usr/local/bin/tool",
            "/opt/google/chrome/google-chrome",
            "/snap/bin/code",
        ] {
            assert!(validate_argv0(&ctx, ok).is_ok(), "{ok}");
        }
        for bad in [
            "/etc/passwd",
            "/home/alice/app",
            "/usr/binx/app",
            "relative/path",
            "/usr/../etc/passwd",
            "/usr/bin/a\0b",
        ] {
            assert!(validate_argv0(&ctx, bad).is_err(), "{bad:?}");
        }
        // Files in the managed app tree are allowed.
        let app = ctx.roots.linux_apps().join("profiles/u1/app");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&app, b"#!/bin/sh\n").unwrap();
        assert!(validate_argv0(&ctx, app.to_str().unwrap()).is_ok());
    }

    #[test]
    fn argv0_symlink_escape_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let link = temp.path().join("evil");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        // A symlink under /usr/bin is not writable by the web server, but a
        // managed-tree symlink pointing outside is caught by canonicalisation.
        let managed_link = ctx.roots.linux_apps().join("evil");
        std::os::unix::fs::symlink(&link, &managed_link).unwrap();
        assert!(validate_argv0(&ctx, managed_link.to_str().unwrap()).is_err());
    }

    #[test]
    fn xpra_binary_must_match_the_configured_one() {
        let temp = tempfile::tempdir().unwrap();
        let fake_xpra = temp.path().join("xpra");
        std::fs::write(&fake_xpra, b"#!/bin/sh\n").unwrap();
        let mut ctx = ctx(&temp);
        ctx.xpra_bin = fake_xpra.clone();
        assert_eq!(
            validate_xpra_bin(&ctx, fake_xpra.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(&fake_xpra).unwrap()
        );
        assert!(validate_xpra_bin(&ctx, "/usr/bin/other").is_err());
        // Relative paths are refused before any filesystem access.
        assert!(validate_xpra_bin(&ctx, "xpra").is_err());
        let missing = temp.path().join("missing");
        assert!(validate_xpra_bin(&ctx, missing.to_str().unwrap()).is_err());
    }

    #[test]
    fn limits_are_range_checked_and_capped_by_configuration() {
        let max = ResourceLimits::from_config(2048, 2, 1024);
        assert!(validate_limits(max, max, "app").is_ok());
        for bad in [
            ResourceLimits {
                memory_max_mb: 1,
                ..max
            },
            ResourceLimits {
                memory_max_mb: 262_145,
                ..max
            },
            ResourceLimits {
                cpu_quota_percent: 0,
                ..max
            },
            ResourceLimits {
                cpu_quota_percent: 6401,
                ..max
            },
            ResourceLimits {
                tasks_max: 15,
                ..max
            },
            ResourceLimits {
                tasks_max: 131_073,
                ..max
            },
        ] {
            assert!(validate_limits(bad, max, "app").is_err());
        }
        let beyond = ResourceLimits {
            memory_max_mb: max.memory_max_mb + 1,
            ..max
        };
        let error = validate_limits(beyond, max, "app").unwrap_err();
        assert!(error.message.contains("configured maximum"), "{error}");
    }

    #[test]
    fn sha256_and_deb_paths_are_strict() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        for bad in [
            "",
            &"A".repeat(64),
            &"a".repeat(63),
            &"g".repeat(64),
            "a".repeat(65).as_str(),
        ] {
            assert!(validate_sha256(bad).is_err(), "{bad:?}");
        }

        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let staging = ctx.roots.linux_apps().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = staging.join("app.deb");
        std::fs::write(&deb, b"not really a deb").unwrap();
        assert_eq!(
            validate_deb_path(&ctx, deb.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(&deb).unwrap()
        );
        // Wrong extension, symlink, directory, out-of-tree path.
        let wrong = staging.join("app.zip");
        std::fs::write(&wrong, b"x").unwrap();
        assert!(validate_deb_path(&ctx, wrong.to_str().unwrap()).is_err());
        let link = staging.join("link.deb");
        std::os::unix::fs::symlink(&deb, &link).unwrap();
        assert!(validate_deb_path(&ctx, link.to_str().unwrap()).is_err());
        let dir = staging.join("dir.deb");
        std::fs::create_dir(&dir).unwrap();
        assert!(validate_deb_path(&ctx, dir.to_str().unwrap()).is_err());
        let outside = temp.path().join("outside.deb");
        std::fs::write(&outside, b"x").unwrap();
        let error = validate_deb_path(&ctx, outside.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, "forbidden");
        assert!(validate_deb_path(&ctx, "relative.deb").is_err());
        assert!(validate_deb_path(&ctx, "/tmp/../etc/x.deb").is_err());
        let empty = staging.join("empty.deb");
        std::fs::write(&empty, b"").unwrap();
        let error = validate_deb_path(&ctx, empty.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, "invalid_deb");
    }

    #[test]
    fn managed_paths_reject_traversal_and_outside_paths() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let users = ctx.roots.linux_users();
        std::fs::create_dir_all(users.join("7")).unwrap();

        for (raw, purpose) in [
            ("../../etc/passwd", PathPurpose::Remove),
            ("/etc/passwd", PathPurpose::Remove),
            ("/var/lib/rast/linux-users/../../etc", PathPurpose::Remove),
            ("linux-users/7", PathPurpose::Remove),
            ("", PathPurpose::Remove),
            ("/var/lib/rast/linux-users/7\0", PathPurpose::Remove),
        ] {
            assert!(
                validate_managed_path(&ctx, raw, purpose).is_err(),
                "{raw:?}"
            );
        }
        let expected = std::fs::canonicalize(users.join("7")).unwrap();
        assert_eq!(
            validate_managed_path(&ctx, users.join("7").to_str().unwrap(), PathPurpose::Remove)
                .unwrap(),
            expected
        );
        // The root itself may never be a target.
        assert!(validate_managed_path(&ctx, users.to_str().unwrap(), PathPurpose::Remove).is_err());
        // Non-existent children are fine for creation.
        let nested = users.join("8").join("home");
        let resolved =
            validate_managed_path(&ctx, nested.to_str().unwrap(), PathPurpose::Mkdir).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(&users).unwrap().join("8/home")
        );
    }

    #[test]
    fn managed_paths_refuse_symlink_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let users = ctx.roots.linux_users();
        std::fs::create_dir_all(&users).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let escape = users.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        let target = escape.join("file");
        let error =
            validate_managed_path(&ctx, target.to_str().unwrap(), PathPurpose::Mkdir).unwrap_err();
        assert_eq!(error.code, "forbidden", "{error}");
        // A final symlink is refused for chown/mkdir but allowed for removal
        // (the link itself is unlinked, never its target).
        let link = users.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        for purpose in [
            PathPurpose::Mkdir,
            PathPurpose::Chown,
            PathPurpose::Chmod,
            PathPurpose::Acl,
        ] {
            assert!(
                validate_managed_path(&ctx, link.to_str().unwrap(), purpose).is_err(),
                "{purpose:?}"
            );
        }
        let resolved =
            validate_managed_path(&ctx, link.to_str().unwrap(), PathPurpose::Remove).unwrap();
        assert_eq!(resolved, link);
    }

    #[test]
    fn homes_are_only_reachable_by_acl_and_chmod() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx(&temp);
        let homes = temp.path().join("storage/homes");
        std::fs::create_dir_all(homes.join("alice")).unwrap();
        ctx.roots = ctx.roots.clone().with_homes(&homes);
        let alice = homes.join("alice");
        assert!(validate_managed_path(&ctx, alice.to_str().unwrap(), PathPurpose::Acl).is_ok());
        assert!(validate_managed_path(&ctx, alice.to_str().unwrap(), PathPurpose::Chmod).is_ok());
        for purpose in [PathPurpose::Mkdir, PathPurpose::Chown, PathPurpose::Remove] {
            assert!(
                validate_managed_path(&ctx, alice.to_str().unwrap(), purpose).is_err(),
                "{purpose:?}"
            );
        }
    }

    #[test]
    fn runtime_dir_and_cwd_need_real_directories() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        let run = ctx.roots.run_dir.clone();
        std::fs::create_dir_all(run.join("xpra/u7")).unwrap();
        let managed = run.join("xpra/u7");
        assert!(validate_runtime_dir(&ctx, managed.to_str().unwrap()).is_ok());
        assert!(validate_runtime_dir(&ctx, run.to_str().unwrap()).is_err());
        assert!(validate_runtime_dir(&ctx, "/etc/xpra").is_err());
        let cwd = ctx.roots.linux_users().join("7");
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(validate_cwd(&ctx, cwd.to_str().unwrap()).is_ok());
        let missing = ctx.roots.linux_users().join("8");
        assert!(validate_cwd(&ctx, missing.to_str().unwrap()).is_err());
        let file = ctx.roots.linux_users().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_cwd(&ctx, file.to_str().unwrap()).is_err());
    }

    #[test]
    fn modes_and_acl_perms_are_bounded() {
        for ok in [0o000, 0o700, 0o711, 0o755, 0o777] {
            assert!(validate_mode(ok).is_ok(), "{ok:o}");
        }
        for bad in [0o1000, 0o2000, 0o4000, 0o7777, 0o10000] {
            assert!(validate_mode(bad).is_err(), "{bad:o}");
        }
        for ok in ["r", "w", "x", "rw", "rx", "rwx", "xw"] {
            assert!(validate_acl_perms(ok).is_ok(), "{ok}");
        }
        for bad in ["", "rwxr", "rww", "a", "RX", "r-x"] {
            assert!(validate_acl_perms(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn managed_uid_range_is_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx(&temp);
        assert!(validate_managed_uid(&ctx, 200_001, 200_001).is_ok());
        assert!(validate_managed_uid(&ctx, 1_200_000, 1_200_000).is_ok());
        for (uid, gid) in [
            (0, 0),
            (200_000, 200_001),
            (1_200_001, 1_200_001),
            (1000, 1000),
        ] {
            assert!(validate_managed_uid(&ctx, uid, gid).is_err(), "{uid}/{gid}");
        }
    }

    #[test]
    fn timeouts_are_classified() {
        assert_eq!(NativeOp::Ping.timeout(), QUICK_OP_TIMEOUT);
        assert_eq!(
            NativeOp::DebPreview(DebPreviewOp {
                path: String::new(),
                sha256: String::new()
            })
            .timeout(),
            DEB_PREVIEW_TIMEOUT
        );
        assert_eq!(
            NativeOp::DebInstall(DebInstallOp {
                path: String::new(),
                sha256: String::new()
            })
            .timeout(),
            DEB_INSTALL_TIMEOUT
        );
        assert_eq!(
            NativeOp::DebRemove(DebRemoveOp {
                packages: Vec::new(),
                purge: false
            })
            .timeout(),
            DEB_REMOVE_TIMEOUT
        );
        assert_eq!(NativeOp::AptUpdate.timeout(), APT_UPDATE_TIMEOUT);
        assert!(NativeOp::DebInstall(DebInstallOp {
            path: String::new(),
            sha256: String::new()
        })
        .is_apt_op());
        assert!(!NativeOp::Ping.is_apt_op());
        assert!(NativeOp::SessionStart(SessionStartOp {
            user_id: 1,
            display: 100,
            port: 1024,
            xpra_bin: String::new(),
            runtime_dir: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            memory_max_mb: 128,
            cpu_quota_percent: 100,
            tasks_max: 16,
        })
        .is_long_running());
    }

    #[test]
    fn helpers_truncate_on_utf8_boundaries() {
        assert_eq!(truncate_message("hello", 10), "hello");
        assert_eq!(truncate_message("a\u{0}b", 10), "ab");
        let cut = truncate_message("日本語", 4);
        assert!(cut.is_char_boundary(cut.len() - '…'.len_utf8()));
        assert_eq!(last_lines("a\nb\nc", 100), "a\nb\nc");
        let tail = last_lines(&"x\n".repeat(10), 6);
        assert!(tail.len() <= 6, "{tail:?}");
        assert!(tail.ends_with('\n'), "{tail:?}");
    }

    #[test]
    fn error_display_is_stable() {
        let error = NativeError::invalid("bad input");
        assert_eq!(error.to_string(), "invalid_request: bad input");
        let error = NativeError::install_failed("apt lock");
        assert_eq!(error.code, "install_failed");
        assert!(std::error::Error::source(&error).is_none());
    }
}
