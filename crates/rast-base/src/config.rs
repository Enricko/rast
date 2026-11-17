//! Configuration loading.
//!
//! Precedence (lowest to highest): built-in defaults -> TOML config file ->
//! `RAST_*` environment variables -> explicit CLI flags (applied by the caller).
//!
//! Example `/etc/rast/rast.toml`:
//!
//! ```toml
//! bind = "127.0.0.1:8720"
//! public_url = "https://rast.example.com"
//! data_dir = "/var/lib/rast"
//! storage_dir = "/srv/rast"
//! trusted_proxies = ["127.0.0.1/32", "::1/128"]
//! session_ttl_hours = 168
//! monitor_interval_secs = 5
//! ```

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/rast/rast.toml";
pub const DEFAULT_DATA_DIR: &str = "/var/lib/rast";
pub const DEFAULT_STORAGE_DIR: &str = "/srv/rast";
pub const DEFAULT_BIND: &str = "127.0.0.1:8720";
pub const DEFAULT_WORKER_SOCKET: &str = "/run/rast/worker.sock";
pub const DEFAULT_SESSION_TTL_HOURS: i64 = 168;
pub const DEFAULT_MONITOR_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const DEFAULT_DB_MAX_CONNECTIONS: u32 = 8;
pub const DEFAULT_QUOTA_BYTES: u64 = 20 * 1024 * 1024 * 1024;
pub const DEFAULT_JOB_CONCURRENCY: usize = 2;
pub const DEFAULT_NATIVE_HELPER_SOCKET: &str = "/run/rast/native.sock";
pub const DEFAULT_LINUX_XPRA_BIN: &str = "/usr/bin/xpra";
pub const DEFAULT_LINUX_UID_BASE: u32 = 200000;
pub const DEFAULT_LINUX_IDLE_MINUTES: u64 = 15;
pub const DEFAULT_LINUX_SESSION_MEMORY_MB: u32 = 4096;
pub const DEFAULT_LINUX_APP_MEMORY_MB: u32 = 2048;
pub const DEFAULT_LINUX_APP_CPUS: u32 = 2;
pub const DEFAULT_LINUX_APP_TASKS: u32 = 1024;
pub const DEFAULT_LINUX_ADMISSION_RESERVE_MB: u32 = 1024;

/// Default CPU limit of one user's graphical session: a quarter of the
/// server's CPUs, at least two and at most eight.
fn default_linux_cpus() -> u32 {
    let available = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    (available / 4).clamp(2, 8)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Optional section of the TOML config file. All fields are optional so that
/// configuration files stay forward/backward compatible across releases.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub public_url: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub storage_dir: Option<PathBuf>,
    pub trusted_proxies: Option<Vec<String>>,
    pub session_ttl_hours: Option<i64>,
    pub monitor_interval_secs: Option<u64>,
    pub max_upload_bytes: Option<u64>,
    pub log_level: Option<String>,
    pub db_max_connections: Option<u32>,
    pub worker_socket: Option<PathBuf>,
    pub docker_enabled: Option<bool>,
    pub default_quota_bytes: Option<u64>,
    pub secure_cookies: Option<bool>,
    pub job_concurrency: Option<usize>,
    /// File Station shows the server's own file system to super
    /// administrators (default: true).
    pub files_root_enabled: Option<bool>,
    /// Require an operator-provided setup token to create the first admin via
    /// the web UI (`rast setup-token` prints one). CLI creation always works.
    pub setup_token_required: Option<bool>,
    /// Keep this false unless RAST is behind a reverse proxy you control.
    pub trust_forwarded_headers: Option<bool>,
    /// App Center catalog (signed). `https://…` or a local `file:///…` mirror.
    pub packages_catalog_url: Option<String>,
    /// Extra trusted catalog keys, `"<key-id>:<base64 Ed25519 public key>"`
    /// (for mirrors and test catalogs). The official key is always trusted.
    pub packages_catalog_keys: Option<Vec<String>>,
    /// Automatic catalog/update check interval in minutes (0 disables).
    pub packages_check_interval_minutes: Option<u64>,
    /// Optional GitHub token for "Install from GitHub" API lookups.
    pub packages_github_token: Option<String>,
    /// Put the App Center defaults on every login shell's PATH
    /// (`/etc/profile.d/rast-packages.sh`, written by `rast packages path sync`).
    pub packages_shell_path: Option<bool>,
    /// Also link the defaults into /usr/local/bin so sudo, services and
    /// scripts run them (needs `packages_shell_path`; root-run link step).
    pub packages_system_wide: Option<bool>,

    // ---- Native Linux apps (Xpra) --------------------------------------
    /// Native Linux app execution (default: on; Docker stays available for
    /// apps that have not been migrated yet).
    pub native_enabled: Option<bool>,
    /// Offer the Linux apps workflow at all (default: on). Set to `false` to
    /// quarantine the whole feature — native and legacy Docker-backed
    /// installs alike — while keeping its implementation in the binary.
    pub linux_apps_enabled: Option<bool>,
    /// Unix socket of the privileged native helper (`rast native-helper`).
    pub native_helper_socket: Option<PathBuf>,
    /// Xpra server binary used for per-user graphical sessions.
    pub linux_xpra_bin: Option<PathBuf>,
    /// First uid assigned to a RAST user's unprivileged Linux identity.
    pub linux_uid_base: Option<u32>,
    /// Extra idle minutes before a user's graphical session is stopped after
    /// its last application exited (an empty, viewer-less session is always
    /// reclaimed after a short reload grace; 0 stops it right after that).
    pub linux_idle_minutes: Option<u64>,
    /// Memory limit of one user's graphical session (MiB).
    pub linux_session_memory_mb: Option<u32>,
    /// CPU limit of one user's graphical session (in CPUs).
    pub linux_session_cpus: Option<u32>,
    /// Process (task) limit of one user's graphical session.
    pub linux_session_tasks: Option<u32>,
    /// Default memory limit of one native application (MiB).
    pub linux_app_memory_mb: Option<u32>,
    /// Default CPU limit of one native application (in CPUs).
    pub linux_app_cpus: Option<u32>,
    /// Default process (task) limit of one native application.
    pub linux_app_tasks: Option<u32>,
    /// Free memory kept in reserve when admitting a new native application.
    pub linux_admission_reserve_mb: Option<u32>,
    /// The native pilot (ADR 0007): when set, native apps, one-HOME mode,
    /// shared folder groups, kernel quota limits and the migration commands
    /// apply only to accounts and shared folders whose names start with one
    /// of these prefixes (`[]`: nobody). Absent: no restriction.
    pub native_pilot_prefixes: Option<Vec<String>>,

    // ---- Native Terminal (PTY) ------------------------------------------
    /// Enable the server-wide Terminal application (admins must still grant
    /// the app to groups; ordinary accounts are denied by default).
    pub terminal_enabled: Option<bool>,
    /// Administrators get a root shell in Terminal (default: true). Selected
    /// ordinary users always get their unprivileged `rast-u<id>` identity.
    pub terminal_admin_root: Option<bool>,
    /// Maximum concurrent terminal sessions per user.
    pub terminal_max_sessions: Option<u32>,
    /// Seconds a disconnected terminal stays alive for reconnection.
    pub terminal_grace_seconds: Option<u32>,
    /// Minutes a terminal nobody is viewing (page reloaded, browser closed)
    /// keeps running without output before it is closed (0 = close right
    /// after the reconnect grace period).
    pub terminal_idle_minutes: Option<u32>,
    /// Memory limit of one terminal session (MiB).
    pub terminal_memory_mb: Option<u32>,
    /// CPU limit of one terminal session (in CPUs).
    pub terminal_cpus: Option<u32>,
    /// Process (task) limit of one terminal session.
    pub terminal_tasks: Option<u32>,
    /// Linux login account administrators' terminals run as, instead of root.
    pub terminal_admin_account: Option<String>,
    /// Extra directories at the end of an administrator root shell's PATH.
    pub terminal_path_extra: Option<Vec<String>>,
}

/// Fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub public_url: Option<String>,
    pub data_dir: PathBuf,
    pub storage_dir: PathBuf,
    pub trusted_proxies: Vec<IpNet>,
    pub session_ttl_hours: i64,
    pub monitor_interval_secs: u64,
    pub max_upload_bytes: u64,
    pub log_level: String,
    pub db_max_connections: u32,
    pub worker_socket: PathBuf,
    pub docker_enabled: bool,
    /// Whether App Center offers the Linux apps workflow (native or Docker).
    pub linux_apps_enabled: bool,
    pub default_quota_bytes: u64,
    pub secure_cookies: bool,
    pub job_concurrency: usize,
    /// File Station's root file system, for super administrators.
    pub files_root_enabled: bool,
    pub setup_token_required: bool,
    pub trust_forwarded_headers: bool,
    pub packages_catalog_url: String,
    pub packages_catalog_keys: Vec<String>,
    pub packages_check_interval_minutes: u64,
    pub packages_github_token: Option<String>,
    pub packages_shell_path: bool,
    pub packages_system_wide: bool,
    /// Native Linux app execution through the privileged helper.
    pub native_enabled: bool,
    pub native_helper_socket: PathBuf,
    pub linux_xpra_bin: PathBuf,
    pub linux_uid_base: u32,
    pub linux_idle_minutes: u64,
    pub linux_session_memory_mb: u32,
    pub linux_session_cpus: u32,
    pub linux_session_tasks: u32,
    pub linux_app_memory_mb: u32,
    pub linux_app_cpus: u32,
    pub linux_app_tasks: u32,
    pub linux_admission_reserve_mb: u32,
    /// Which accounts and shared folders the native features apply to
    /// (`native_pilot_prefixes`, ADR 0007).
    pub native_pilot: NativePilot,
    /// Native Terminal settings (PTY shells under the per-user identity).
    pub terminal: TerminalConfig,
    /// Config file that was actually loaded, if any.
    pub loaded_from: Option<PathBuf>,
}

/// Settings for the native Terminal application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    pub enabled: bool,
    /// Whether administrators get a root shell (full server access).
    pub admin_root: bool,
    pub max_sessions: u32,
    pub grace_seconds: u32,
    pub idle_minutes: u32,
    pub memory_mb: u32,
    pub cpus: u32,
    pub tasks: u32,
    /// With `admin_root`: the Linux login account (in sudo/admin/wheel) the
    /// administrators' shells run as, with its own login environment.
    pub admin_account: Option<String>,
    /// Absolute directories appended to the PATH of administrator root
    /// shells, for tools installed in another account's home.
    pub path_extra: Vec<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            admin_root: true,
            max_sessions: 8,
            grace_seconds: 30,
            idle_minutes: 1440,
            memory_mb: 1024,
            cpus: 2,
            tasks: 512,
            path_extra: Vec::new(),
            admin_account: None,
        }
    }
}

/// Longest native pilot prefix (a username is at most 32 characters).
pub const MAX_PILOT_PREFIX: usize = 32;
/// Most native pilot prefixes.
pub const MAX_PILOT_PREFIXES: usize = 16;

/// The native pilot (`native_pilot_prefixes`, ADR 0007): which accounts and
/// shared folders the native features apply to while native execution is
/// tried on a server that also serves real accounts.
///
/// Off (the default): every account and shared folder, as before. On:
/// only accounts whose username starts with one of the prefixes (usernames
/// are stored in lower case), and only shared folders whose name **and**
/// container both start with one (case-sensitive), so a shared folder
/// renamed across the boundary is outside the pilot in both directions.
/// An empty list puts nobody in the pilot. The pilot only ever takes
/// native features away from accounts and shared folders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativePilot {
    prefixes: Option<Vec<String>>,
}

impl NativePilot {
    /// No pilot: the native features apply to everyone.
    pub fn off() -> Self {
        Self::default()
    }

    /// A pilot of these prefixes (validated: 1 to [`MAX_PILOT_PREFIX`]
    /// characters of the username charset in lower case, starting with a
    /// letter or digit, at most [`MAX_PILOT_PREFIXES`]). Duplicates go.
    pub fn with_prefixes<I, S>(prefixes: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out: Vec<String> = Vec::new();
        for prefix in prefixes {
            let prefix = prefix.as_ref().trim();
            let valid = !prefix.is_empty()
                && prefix.len() <= MAX_PILOT_PREFIX
                && prefix
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && prefix.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
                });
            if !valid {
                return Err(format!(
                    "native_pilot_prefixes: {prefix:?} is not a valid prefix (1-{MAX_PILOT_PREFIX} \
                     lower-case letters, digits, '.', '-' or '_', starting with a letter or digit)"
                ));
            }
            if !out.iter().any(|known| known == prefix) {
                out.push(prefix.to_string());
            }
        }
        if out.len() > MAX_PILOT_PREFIXES {
            return Err(format!(
                "native_pilot_prefixes: at most {MAX_PILOT_PREFIXES} prefixes"
            ));
        }
        Ok(Self {
            prefixes: Some(out),
        })
    }

    /// Whether the pilot restricts anything.
    pub fn is_active(&self) -> bool {
        self.prefixes.is_some()
    }

    /// The prefixes, `None` when the pilot is off.
    pub fn prefixes(&self) -> Option<&[String]> {
        self.prefixes.as_deref()
    }

    fn matches(&self, name: &str) -> bool {
        match &self.prefixes {
            None => true,
            Some(prefixes) => prefixes.iter().any(|prefix| name.starts_with(prefix)),
        }
    }

    /// Whether the native features apply to the account `username`
    /// (compared in lower case, as usernames are stored).
    pub fn allows_user(&self, username: &str) -> bool {
        !self.is_active() || self.matches(&username.trim().to_ascii_lowercase())
    }

    /// Whether the native features apply to a shared folder: its name and
    /// its container must both start with a prefix.
    pub fn allows_share(&self, name: &str, container: &str) -> bool {
        self.matches(name) && self.matches(container)
    }

    /// Whether a shared folder's name alone is in the pilot (a first check
    /// before its container is known).
    pub fn allows_share_name(&self, name: &str) -> bool {
        self.matches(name)
    }

    /// One line for `rast doctor` and the start-up log.
    pub fn describe(&self) -> String {
        match &self.prefixes {
            None => "off: all accounts and shared folders".to_string(),
            Some(prefixes) if prefixes.is_empty() => {
                "on: nobody (native_pilot_prefixes = [])".to_string()
            }
            Some(prefixes) => format!(
                "on: only accounts and shared folders whose names start with {}",
                prefixes
                    .iter()
                    .map(|prefix| format!("{prefix:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl Config {
    /// Resolve configuration from the default locations and environment.
    pub fn load(explicit_path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = Self::resolve_path(explicit_path);
        let file = match &path {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|source| ConfigError::Read {
                    path: p.clone(),
                    source,
                })?;
                let parsed: FileConfig =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: p.clone(),
                        source,
                    })?;
                Some(parsed)
            }
            None => None,
        };
        Self::from_parts(path, file.unwrap_or_default(), |k| std::env::var(k).ok())
    }

    fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.to_path_buf());
        }
        if let Some(p) = non_empty_env("RAST_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let default = PathBuf::from(DEFAULT_CONFIG_PATH);
        if default.exists() {
            Some(default)
        } else {
            None
        }
    }

    /// Build a config from a parsed file + an environment lookup.
    /// `env` is injectable for tests.
    pub fn from_parts(
        loaded_from: Option<PathBuf>,
        file: FileConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let get_str = |file_field: Option<String>, env_key: &str| -> Option<String> {
            non_empty_env_in(&env, env_key).or(file_field)
        };
        let get_parsed = |file_field: Option<String>,
                          env_key: &str,
                          what: &str|
         -> Result<Option<String>, ConfigError> {
            let raw = get_str(file_field, env_key);
            if let Some(v) = &raw {
                if v.trim().is_empty() {
                    return Err(ConfigError::Invalid(format!("{env_key} cannot be empty")));
                }
                let _ = what;
            }
            Ok(raw)
        };

        let bind_raw = get_parsed(file.bind, "RAST_BIND", "socket address")?
            .unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind = SocketAddr::from_str(&bind_raw)
            .map_err(|e| ConfigError::Invalid(format!("invalid bind address {bind_raw:?}: {e}")))?;

        let public_url = get_str(file.public_url, "RAST_PUBLIC_URL")
            .map(|u| u.trim_end_matches('/').to_string());
        if let Some(url) = &public_url {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(ConfigError::Invalid(format!(
                    "public_url must start with http:// or https://, got {url:?}"
                )));
            }
        }

        let data_dir = get_str(
            file.data_dir.map(|p| p.display().to_string()),
            "RAST_DATA_DIR",
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        let storage_dir = get_str(
            file.storage_dir.map(|p| p.display().to_string()),
            "RAST_STORAGE_DIR",
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_DIR));

        let trusted_raw = get_str(
            file.trusted_proxies.map(|v| v.join(",")),
            "RAST_TRUSTED_PROXIES",
        );
        let mut trusted_proxies = Vec::new();
        if let Some(raw) = trusted_raw {
            for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let net = IpNet::from_str(part).map_err(|e| {
                    ConfigError::Invalid(format!("invalid trusted proxy CIDR {part:?}: {e}"))
                })?;
                trusted_proxies.push(net);
            }
        }

        let session_ttl_hours =
            parse_env_num(&env, "RAST_SESSION_TTL_HOURS", file.session_ttl_hours)?
                .unwrap_or(DEFAULT_SESSION_TTL_HOURS);
        if !(1..=24 * 365).contains(&session_ttl_hours) {
            return Err(ConfigError::Invalid(
                "session_ttl_hours must be between 1 and 8760".into(),
            ));
        }

        let monitor_interval_secs = parse_env_num(
            &env,
            "RAST_MONITOR_INTERVAL_SECS",
            file.monitor_interval_secs,
        )?
        .unwrap_or(DEFAULT_MONITOR_INTERVAL_SECS);
        if !(1..=300).contains(&monitor_interval_secs) {
            return Err(ConfigError::Invalid(
                "monitor_interval_secs must be between 1 and 300".into(),
            ));
        }

        let max_upload_bytes = parse_env_num(&env, "RAST_MAX_UPLOAD_BYTES", file.max_upload_bytes)?
            .unwrap_or(DEFAULT_MAX_UPLOAD_BYTES);
        if !(1024 * 1024..=1024u64.pow(4)).contains(&max_upload_bytes) {
            return Err(ConfigError::Invalid(
                "max_upload_bytes must be between 1 MiB and 1 TiB".into(),
            ));
        }

        let log_level = get_str(file.log_level, "RAST_LOG_LEVEL").unwrap_or_else(|| "info".into());

        let db_max_connections =
            parse_env_num(&env, "RAST_DB_MAX_CONNECTIONS", file.db_max_connections)?
                .unwrap_or(DEFAULT_DB_MAX_CONNECTIONS);
        if !(1..=64).contains(&db_max_connections) {
            return Err(ConfigError::Invalid(
                "db_max_connections must be between 1 and 64".into(),
            ));
        }

        let worker_socket = get_str(
            file.worker_socket.map(|p| p.display().to_string()),
            "RAST_WORKER_SOCKET",
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKER_SOCKET));

        let docker_enabled =
            parse_env_bool(&env, "RAST_DOCKER_ENABLED", file.docker_enabled)?.unwrap_or(false);

        let default_quota_bytes =
            parse_env_num(&env, "RAST_DEFAULT_QUOTA_BYTES", file.default_quota_bytes)?
                .unwrap_or(DEFAULT_QUOTA_BYTES);

        let secure_cookies = parse_env_bool(&env, "RAST_SECURE_COOKIES", file.secure_cookies)?
            .unwrap_or_else(|| {
                public_url
                    .as_deref()
                    .is_some_and(|u| u.starts_with("https://"))
            });

        let job_concurrency = parse_env_num(&env, "RAST_JOB_CONCURRENCY", file.job_concurrency)?
            .unwrap_or(DEFAULT_JOB_CONCURRENCY);
        if !(1..=8).contains(&job_concurrency) {
            return Err(ConfigError::Invalid(
                "job_concurrency must be between 1 and 8".into(),
            ));
        }

        let files_root_enabled =
            parse_env_bool(&env, "RAST_FILES_ROOT_ENABLED", file.files_root_enabled)?
                .unwrap_or(true);

        // Safe default: the first-run web setup requires an operator-generated
        // token unless an operator explicitly opts out. This prevents the first
        // visitor of a public deployment from taking over the installation.
        let setup_token_required =
            parse_env_bool(&env, "RAST_SETUP_TOKEN_REQUIRED", file.setup_token_required)?
                .unwrap_or(true);

        let trust_forwarded_headers = parse_env_bool(
            &env,
            "RAST_TRUST_FORWARDED_HEADERS",
            file.trust_forwarded_headers,
        )?
        .unwrap_or(!trusted_proxies.is_empty());

        let packages_catalog_url = get_str(file.packages_catalog_url, "RAST_PACKAGES_CATALOG_URL")
            .unwrap_or_else(|| OFFICIAL_CATALOG_URL.to_string());
        if !(packages_catalog_url.starts_with("https://")
            || packages_catalog_url.starts_with("file:///"))
        {
            return Err(ConfigError::Invalid(
                "packages_catalog_url must be an https:// or file:/// URL".into(),
            ));
        }
        let packages_catalog_keys: Vec<String> = get_str(
            file.packages_catalog_keys.map(|v| v.join(",")),
            "RAST_PACKAGES_CATALOG_KEYS",
        )
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
        for key in &packages_catalog_keys {
            parse_trusted_key(key).map_err(ConfigError::Invalid)?;
        }
        let packages_check_interval_minutes = parse_env_num(
            &env,
            "RAST_PACKAGES_CHECK_INTERVAL_MINUTES",
            file.packages_check_interval_minutes,
        )?
        .unwrap_or(360);
        if packages_check_interval_minutes > 7 * 24 * 60 {
            return Err(ConfigError::Invalid(
                "packages_check_interval_minutes must be between 0 and 10080".into(),
            ));
        }
        let packages_github_token =
            get_str(file.packages_github_token, "RAST_PACKAGES_GITHUB_TOKEN");
        let packages_shell_path =
            parse_env_bool(&env, "RAST_PACKAGES_SHELL_PATH", file.packages_shell_path)?
                .unwrap_or(true);
        let packages_system_wide =
            parse_env_bool(&env, "RAST_PACKAGES_SYSTEM_WIDE", file.packages_system_wide)?
                .unwrap_or(false);

        if data_dir.as_os_str().is_empty() || storage_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "data_dir and storage_dir must not be empty".into(),
            ));
        }

        // ---- Native Linux apps -----------------------------------------
        let linux_xpra_bin = get_str(
            file.linux_xpra_bin.map(|p| p.display().to_string()),
            "RAST_LINUX_XPRA_BIN",
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LINUX_XPRA_BIN));
        // Native execution is the default when Xpra is installed; forcing it
        // on/off is an operator choice. Without Xpra the Docker backend stays
        // the default so app installs keep working.
        let native_enabled = parse_env_bool(&env, "RAST_NATIVE_ENABLED", file.native_enabled)?
            .unwrap_or_else(|| {
                cfg!(target_os = "linux") && std::path::Path::new(&linux_xpra_bin).exists()
            });
        let linux_apps_enabled =
            parse_env_bool(&env, "RAST_LINUX_APPS_ENABLED", file.linux_apps_enabled)?
                .unwrap_or(true);
        let native_helper_socket = get_str(
            file.native_helper_socket.map(|p| p.display().to_string()),
            "RAST_NATIVE_HELPER_SOCKET",
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_NATIVE_HELPER_SOCKET));
        let linux_uid_base = parse_env_num(&env, "RAST_LINUX_UID_BASE", file.linux_uid_base)?
            .unwrap_or(DEFAULT_LINUX_UID_BASE);
        if !(1000..=2_000_000_000).contains(&linux_uid_base) {
            return Err(ConfigError::Invalid(
                "linux_uid_base must be between 1000 and 2000000000".into(),
            ));
        }
        let linux_idle_minutes =
            parse_env_num(&env, "RAST_LINUX_IDLE_MINUTES", file.linux_idle_minutes)?
                .unwrap_or(DEFAULT_LINUX_IDLE_MINUTES);
        if linux_idle_minutes > 7 * 24 * 60 {
            return Err(ConfigError::Invalid(
                "linux_idle_minutes must be between 0 and 10080".into(),
            ));
        }
        let linux_session_memory_mb = parse_env_num(
            &env,
            "RAST_LINUX_SESSION_MEMORY_MB",
            file.linux_session_memory_mb,
        )?
        .unwrap_or(DEFAULT_LINUX_SESSION_MEMORY_MB);
        if !(512..=262144).contains(&linux_session_memory_mb) {
            return Err(ConfigError::Invalid(
                "linux_session_memory_mb must be between 512 and 262144".into(),
            ));
        }
        let linux_session_cpus =
            parse_env_num(&env, "RAST_LINUX_SESSION_CPUS", file.linux_session_cpus)?
                .unwrap_or_else(default_linux_cpus);
        if !(1..=256).contains(&linux_session_cpus) {
            return Err(ConfigError::Invalid(
                "linux_session_cpus must be between 1 and 256".into(),
            ));
        }
        let linux_session_tasks =
            parse_env_num(&env, "RAST_LINUX_SESSION_TASKS", file.linux_session_tasks)?
                .unwrap_or(2048);
        if !(64..=131072).contains(&linux_session_tasks) {
            return Err(ConfigError::Invalid(
                "linux_session_tasks must be between 64 and 131072".into(),
            ));
        }
        let linux_app_memory_mb =
            parse_env_num(&env, "RAST_LINUX_APP_MEMORY_MB", file.linux_app_memory_mb)?
                .unwrap_or(DEFAULT_LINUX_APP_MEMORY_MB);
        if !(128..=262144).contains(&linux_app_memory_mb) {
            return Err(ConfigError::Invalid(
                "linux_app_memory_mb must be between 128 and 262144".into(),
            ));
        }
        let linux_app_cpus = parse_env_num(&env, "RAST_LINUX_APP_CPUS", file.linux_app_cpus)?
            .unwrap_or(DEFAULT_LINUX_APP_CPUS);
        if !(1..=256).contains(&linux_app_cpus) {
            return Err(ConfigError::Invalid(
                "linux_app_cpus must be between 1 and 256".into(),
            ));
        }
        let linux_app_tasks = parse_env_num(&env, "RAST_LINUX_APP_TASKS", file.linux_app_tasks)?
            .unwrap_or(DEFAULT_LINUX_APP_TASKS);
        if !(16..=131072).contains(&linux_app_tasks) {
            return Err(ConfigError::Invalid(
                "linux_app_tasks must be between 16 and 131072".into(),
            ));
        }
        let linux_admission_reserve_mb = parse_env_num(
            &env,
            "RAST_LINUX_ADMISSION_RESERVE_MB",
            file.linux_admission_reserve_mb,
        )?
        .unwrap_or(DEFAULT_LINUX_ADMISSION_RESERVE_MB);
        if !(0..=1_048_576).contains(&linux_admission_reserve_mb) {
            return Err(ConfigError::Invalid(
                "linux_admission_reserve_mb must be between 0 and 1048576".into(),
            ));
        }
        // An empty variable counts as unset (like every RAST_* variable); a
        // value of only commas is the empty list (nobody).
        let native_pilot = match non_empty_env_in(&env, "RAST_NATIVE_PILOT_PREFIXES") {
            Some(raw) => NativePilot::with_prefixes(
                raw.split(',')
                    .map(str::trim)
                    .filter(|prefix| !prefix.is_empty()),
            ),
            None => match file.native_pilot_prefixes {
                Some(prefixes) => NativePilot::with_prefixes(prefixes),
                None => Ok(NativePilot::off()),
            },
        }
        .map_err(ConfigError::Invalid)?;

        let terminal_path_extra: Vec<String> = get_str(
            file.terminal_path_extra.map(|v| v.join(":")),
            "RAST_TERMINAL_PATH_EXTRA",
        )
        .map(|raw| {
            raw.split(':')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
        if terminal_path_extra.len() > 16
            || terminal_path_extra.iter().any(|dir| {
                !dir.starts_with('/')
                    || dir.len() > 256
                    || dir.split('/').any(|part| part == "..")
                    || dir.chars().any(|c| c.is_control() || c == ':')
            })
        {
            return Err(ConfigError::Invalid(
                "terminal_path_extra must be at most 16 absolute directories without '..'".into(),
            ));
        }
        let terminal_admin_account =
            get_str(file.terminal_admin_account, "RAST_TERMINAL_ADMIN_ACCOUNT")
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty() && name != "root");
        if terminal_admin_account.as_ref().is_some_and(|name| {
            name.len() > 32
                || name.starts_with('-')
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        }) {
            return Err(ConfigError::Invalid(
                "terminal_admin_account must be a Linux account name".into(),
            ));
        }
        let terminal = TerminalConfig {
            admin_account: terminal_admin_account,
            path_extra: terminal_path_extra,
            enabled: parse_env_bool(&env, "RAST_TERMINAL_ENABLED", file.terminal_enabled)?
                .unwrap_or(true),
            admin_root: parse_env_bool(&env, "RAST_TERMINAL_ADMIN_ROOT", file.terminal_admin_root)?
                .unwrap_or(true),
            max_sessions: parse_env_num(
                &env,
                "RAST_TERMINAL_MAX_SESSIONS",
                file.terminal_max_sessions,
            )?
            .unwrap_or(8)
            .clamp(1, 64),
            grace_seconds: parse_env_num(
                &env,
                "RAST_TERMINAL_GRACE_SECONDS",
                file.terminal_grace_seconds,
            )?
            .unwrap_or(30)
            .clamp(0, 300),
            idle_minutes: parse_env_num(
                &env,
                "RAST_TERMINAL_IDLE_MINUTES",
                file.terminal_idle_minutes,
            )?
            .unwrap_or(1440)
            .clamp(0, 10_080),
            memory_mb: parse_env_num(&env, "RAST_TERMINAL_MEMORY_MB", file.terminal_memory_mb)?
                .unwrap_or(1024)
                .clamp(64, 1_048_576),
            cpus: parse_env_num(&env, "RAST_TERMINAL_CPUS", file.terminal_cpus)?
                .unwrap_or(2)
                .clamp(1, 256),
            tasks: parse_env_num(&env, "RAST_TERMINAL_TASKS", file.terminal_tasks)?
                .unwrap_or(512)
                .clamp(16, 65_536),
        };

        Ok(Self {
            bind,
            public_url,
            data_dir,
            storage_dir,
            trusted_proxies,
            session_ttl_hours,
            monitor_interval_secs,
            max_upload_bytes,
            log_level,
            db_max_connections,
            worker_socket,
            docker_enabled,
            linux_apps_enabled,
            default_quota_bytes,
            secure_cookies,
            job_concurrency,
            files_root_enabled,
            setup_token_required,
            trust_forwarded_headers,
            packages_catalog_url,
            packages_catalog_keys,
            packages_check_interval_minutes,
            packages_github_token,
            packages_shell_path,
            packages_system_wide,
            native_enabled,
            native_helper_socket,
            linux_xpra_bin,
            linux_uid_base,
            linux_idle_minutes,
            linux_session_memory_mb,
            linux_session_cpus,
            linux_session_tasks,
            linux_app_memory_mb,
            linux_app_cpus,
            linux_app_tasks,
            linux_admission_reserve_mb,
            native_pilot,
            terminal,
            loaded_from,
        })
    }

    /// `<data_dir>/private` (0700): the database and the master key (ADR
    /// 0006). The data directory itself stays traversable for the native
    /// app identities and package users, so a file directly in it can be
    /// named, and hard-linked elsewhere with `fs.protected_hardlinks = 0`.
    pub fn private_dir(&self) -> PathBuf {
        self.data_dir.join(PRIVATE_DIR)
    }

    /// A file of [`private_dir`](Self::private_dir), or its old place
    /// directly in the data directory while an older release's file there
    /// has not been moved yet (`rast serve` moves it under the instance
    /// lock: [`crate::db::move_into_private`]).
    fn private_file(&self, name: &str) -> PathBuf {
        let private = self.private_dir().join(name);
        let legacy = self.data_dir.join(name);
        if !is_regular_file(&private) && is_regular_file(&legacy) {
            legacy
        } else {
            private
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.private_file("rast.db")
    }
    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
    pub fn apps_dir(&self) -> PathBuf {
        self.data_dir.join("apps")
    }
    /// App Center root: `<data_dir>/packages`.
    pub fn packages_dir(&self) -> PathBuf {
        self.data_dir.join("packages")
    }
    /// Linux app storage: `<data_dir>/linux-apps` (staging, managed installs,
    /// per-user profiles, migration backups).
    pub fn linux_apps_dir(&self) -> PathBuf {
        self.data_dir.join("linux-apps")
    }
    /// Managed native application directories.
    pub fn linux_app_install_dir(&self) -> PathBuf {
        self.linux_apps_dir().join("apps")
    }
    /// Per-user application profiles (settings, sign-ins).
    pub fn linux_profiles_dir(&self) -> PathBuf {
        self.linux_apps_dir().join("profiles")
    }
    /// Unprivileged Linux identities' homes.
    pub fn linux_users_dir(&self) -> PathBuf {
        self.data_dir.join("linux-users")
    }
    /// Uploads and staging area for native installs.
    pub fn linux_staging_dir(&self) -> PathBuf {
        self.linux_apps_dir().join("staging")
    }
    /// Docker -> native migration backups.
    pub fn linux_migration_dir(&self) -> PathBuf {
        self.linux_apps_dir().join("migrations")
    }
    pub fn secret_path(&self) -> PathBuf {
        self.private_file("secret.key")
    }

    /// Give the database and the master key in `private` a link at their
    /// old place (`rast.db -> private/rast.db`) when that name is free, so
    /// an older release (a downgrade) and scripts find them. The link is
    /// harmless where others can traverse: following it needs `private`.
    pub fn link_legacy_names(&self) -> std::io::Result<()> {
        for name in LINKED_FILES {
            let legacy = self.data_dir.join(name);
            if !is_regular_file(&self.private_dir().join(name)) || legacy.symlink_metadata().is_ok()
            {
                continue;
            }
            match std::os::unix::fs::symlink(format!("{PRIVATE_DIR}/{name}"), &legacy) {
                Ok(()) => self.adopt(&legacy),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// A CLI run as root (`sudo rast update`) gives what it creates in the
    /// data directory to the directory's owner, the service account, so
    /// the server can still use it. Best effort, never follows a link.
    fn adopt(&self, path: &std::path::Path) {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let (Ok(owner), Ok(meta)) = (
            std::fs::metadata(&self.data_dir),
            std::fs::symlink_metadata(path),
        ) else {
            return;
        };
        if owner.uid() != 0 && meta.uid() == 0 {
            let _ = std::os::unix::fs::lchown(path, Some(owner.uid()), Some(owner.gid()));
        }
    }
    pub fn homes_dir(&self) -> PathBuf {
        self.storage_dir.join("homes")
    }
    pub fn shared_dir(&self) -> PathBuf {
        self.storage_dir.join("shared")
    }
    pub fn trash_dir(&self) -> PathBuf {
        self.storage_dir.join(".trash")
    }
    pub fn home_for(&self, username: &str) -> PathBuf {
        self.homes_dir().join(username)
    }

    /// Create all directories with restrictive permissions where possible.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            self.data_dir.clone(),
            self.private_dir(),
            self.backup_dir(),
            self.apps_dir(),
            self.homes_dir(),
            self.shared_dir(),
            self.trash_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        // Data dir contains secrets: 0750. Storage roots stay 0755 so that
        // per-user homes (0700) inherit sane defaults. With the Package
        // Center shell PATH, others keep the traverse bit (no listing, no
        // reading): resetting it would hide every installed runtime from
        // other users until the next `rast packages path sync`.
        // App identities must be able to traverse the data directory to reach
        // their managed trees (linux-apps/linux-users), exactly like package
        // runtimes need it with packages_shell_path.
        let traverse = if self.packages_shell_path || self.native_enabled {
            0o001
        } else {
            0
        };
        let _ = std::fs::set_permissions(
            &self.data_dir,
            std::fs::Permissions::from_mode(0o750 | traverse),
        );
        // What others must never name, even with the data directory
        // traversable: the database and the key, and the backups (which
        // hold both). The work folders of backups and uploads lose any
        // access for others too.
        for dir in [self.private_dir(), self.backup_dir()] {
            let mode = std::fs::symlink_metadata(&dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
            self.adopt(&dir);
        }
        let tmp = self.data_dir.join("tmp");
        if let Ok(meta) = std::fs::symlink_metadata(&tmp) {
            let mode = meta.permissions().mode();
            if meta.is_dir() && mode & 0o007 != 0 {
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode & 0o770))?;
            }
        }
        self.link_legacy_names()
    }
}

/// The folder of the data directory that holds its secret files.
pub const PRIVATE_DIR: &str = "private";
/// The files that live in [`PRIVATE_DIR`], in the order a move takes them
/// (the database's WAL files before the database).
pub const PRIVATE_FILES: &[&str] = &["rast.db-wal", "rast.db-shm", "rast.db", "secret.key"];
/// The files that keep a link at their old place.
pub const LINKED_FILES: &[&str] = &["rast.db", "secret.key"];

fn is_regular_file(path: &std::path::Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_file())
}

impl FileConfig {
    /// Parse a TOML config file body.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    non_empty_env_in(&|k| std::env::var(k).ok(), key)
}

fn non_empty_env_in(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key).filter(|v| !v.trim().is_empty())
}

fn parse_env_num<T>(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
    file_value: Option<T>,
) -> Result<Option<T>, ConfigError>
where
    T: FromStr + Copy,
    T::Err: std::fmt::Display,
{
    if let Some(raw) = env(key).filter(|v| !v.trim().is_empty()) {
        return raw
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|e| ConfigError::Invalid(format!("invalid {key}={raw:?}: {e}")));
    }
    Ok(file_value)
}

fn parse_env_bool(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
    file_value: Option<bool>,
) -> Result<Option<bool>, ConfigError> {
    if let Some(raw) = env(key).filter(|v| !v.trim().is_empty()) {
        let normalized = raw.trim().to_ascii_lowercase();
        let val = match normalized.as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                return Err(ConfigError::Invalid(format!(
                    "invalid {key}={other:?}: expected true/false"
                )))
            }
        };
        return Ok(Some(val));
    }
    Ok(file_value)
}

/// Basic sanity checks used by `rast doctor`.
pub fn warnings(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if cfg.public_url.is_none() {
        out.push(
            "public_url is not set; secure cookies will be disabled unless explicitly enabled"
                .into(),
        );
    }
    if cfg.bind.ip().is_unspecified() && cfg.trusted_proxies.is_empty() {
        out.push(
            "bind address is reachable from the network but no trusted proxies are configured; \
             make sure a firewall restricts access"
                .into(),
        );
    }
    if !cfg.secure_cookies && cfg.public_url.as_deref() == Some("https://") {
        out.push("secure_cookies is disabled while public_url is HTTPS".into());
    }
    if cfg.storage_dir.starts_with(&cfg.data_dir) {
        out.push("storage_dir is inside data_dir; separate volumes are recommended".into());
    }
    if cfg.packages_system_wide && !cfg.packages_shell_path {
        out.push(
            "packages_system_wide needs packages_shell_path; the App Center defaults are not linked into /usr/local/bin"
                .into(),
        );
    }
    out
}

impl Config {
    /// Whether the App Center defaults are linked into /usr/local/bin.
    pub fn packages_links_enabled(&self) -> bool {
        self.packages_shell_path && self.packages_system_wide
    }
}

/// Serialize a config to TOML; used by the installer/doctor to render examples.
pub fn to_toml_example(cfg: &Config) -> String {
    #[derive(Serialize)]
    struct Example<'a> {
        bind: String,
        public_url: Option<&'a str>,
        data_dir: String,
        storage_dir: String,
        trusted_proxies: Vec<String>,
        session_ttl_hours: i64,
        monitor_interval_secs: u64,
        max_upload_bytes: u64,
        log_level: &'a str,
        db_max_connections: u32,
        worker_socket: String,
        docker_enabled: bool,
        linux_apps_enabled: bool,
        default_quota_bytes: u64,
        secure_cookies: bool,
        job_concurrency: usize,
        setup_token_required: bool,
        trust_forwarded_headers: bool,
        packages_catalog_url: &'a str,
        packages_catalog_keys: &'a [String],
        packages_check_interval_minutes: u64,
        packages_shell_path: bool,
        packages_system_wide: bool,
        // packages_github_token is a secret and is never written here.
    }
    let ex = Example {
        bind: cfg.bind.to_string(),
        public_url: cfg.public_url.as_deref(),
        data_dir: cfg.data_dir.display().to_string(),
        storage_dir: cfg.storage_dir.display().to_string(),
        trusted_proxies: cfg.trusted_proxies.iter().map(|n| n.to_string()).collect(),
        session_ttl_hours: cfg.session_ttl_hours,
        monitor_interval_secs: cfg.monitor_interval_secs,
        max_upload_bytes: cfg.max_upload_bytes,
        log_level: &cfg.log_level,
        db_max_connections: cfg.db_max_connections,
        worker_socket: cfg.worker_socket.display().to_string(),
        docker_enabled: cfg.docker_enabled,
        linux_apps_enabled: cfg.linux_apps_enabled,
        default_quota_bytes: cfg.default_quota_bytes,
        secure_cookies: cfg.secure_cookies,
        job_concurrency: cfg.job_concurrency,
        setup_token_required: cfg.setup_token_required,
        trust_forwarded_headers: cfg.trust_forwarded_headers,
        packages_catalog_url: &cfg.packages_catalog_url,
        packages_catalog_keys: &cfg.packages_catalog_keys,
        packages_check_interval_minutes: cfg.packages_check_interval_minutes,
        packages_shell_path: cfg.packages_shell_path,
        packages_system_wide: cfg.packages_system_wide,
    };
    let body = toml::to_string_pretty(&ex).unwrap_or_default();
    format!(
        "# RAST configuration — see https://github.com/Enricko/rast/blob/main/docs/CONFIGURATION.md\n{body}"
    )
}

/// Default package catalog location.
pub const OFFICIAL_CATALOG_URL: &str =
    "https://github.com/Enricko/rast-packages/releases/download/catalog/catalog.json";
#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub id: String,
    pub key: Vec<u8>,
    pub official: bool,
}

/// Parse `"<key-id>:<base64>"` (configuration format).
pub fn parse_trusted_key(raw: &str) -> Result<TrustedKey, String> {
    let (id, b64) = raw
        .split_once(':')
        .ok_or_else(|| format!("catalog key {raw:?} must look like <key-id>:<base64>"))?;
    let key = B64
        .decode(b64.trim())
        .map_err(|_| format!("catalog key {id:?} is not valid base64"))?;
    if key.len() != 32 || id.trim().is_empty() {
        return Err(format!(
            "catalog key {id:?} must be a 32-byte Ed25519 public key"
        ));
    }
    Ok(TrustedKey {
        id: id.trim().to_string(),
        key,
        official: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::from_parts(None, FileConfig::default(), no_env).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8720".parse().unwrap());
        assert_eq!(cfg.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(cfg.storage_dir, PathBuf::from(DEFAULT_STORAGE_DIR));
        assert!(!cfg.secure_cookies);
        assert!(!cfg.docker_enabled);
        assert!(cfg.linux_apps_enabled);
    }

    #[test]
    fn terminal_admin_root_is_on_by_default_and_configurable() {
        let cfg = Config::from_parts(None, FileConfig::default(), no_env).unwrap();
        assert!(cfg.terminal.enabled);
        assert!(cfg.terminal.admin_root);
        let file = FileConfig::from_toml_str("terminal_admin_root = false").unwrap();
        let cfg = Config::from_parts(None, file, no_env).unwrap();
        assert!(!cfg.terminal.admin_root);
        let env = |k: &str| match k {
            "RAST_TERMINAL_ADMIN_ROOT" => Some("false".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, FileConfig::default(), env).unwrap();
        assert!(!cfg.terminal.admin_root);
    }

    #[test]
    fn terminal_path_extra_takes_absolute_directories_only() {
        let no_env = |_: &str| None;
        let file = FileConfig {
            terminal_path_extra: Some(vec!["/home/ada/.local/bin".into()]),
            ..FileConfig::default()
        };
        let cfg = Config::from_parts(None, file, no_env).unwrap();
        assert_eq!(cfg.terminal.path_extra, vec!["/home/ada/.local/bin"]);
        let env = |k: &str| match k {
            "RAST_TERMINAL_PATH_EXTRA" => Some("/opt/a/bin:/opt/b/bin".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, FileConfig::default(), env).unwrap();
        assert_eq!(cfg.terminal.path_extra, vec!["/opt/a/bin", "/opt/b/bin"]);
        for bad in ["bin", "/home/ada/../root/bin"] {
            let file = FileConfig {
                terminal_path_extra: Some(vec![bad.into()]),
                ..FileConfig::default()
            };
            assert!(Config::from_parts(None, file, no_env).is_err());
        }
    }

    #[test]
    fn the_native_pilot_is_off_unless_configured() {
        let cfg = Config::from_parts(None, FileConfig::default(), no_env).unwrap();
        assert!(!cfg.native_pilot.is_active());
        assert_eq!(cfg.native_pilot, NativePilot::off());
        assert!(cfg.native_pilot.allows_user("alice"));
        assert!(cfg.native_pilot.allows_share("Test", "Test"));
        assert_eq!(
            cfg.native_pilot.describe(),
            "off: all accounts and shared folders"
        );
    }

    #[test]
    fn the_native_pilot_comes_from_the_file_or_the_environment() {
        let file =
            FileConfig::from_toml_str(r#"native_pilot_prefixes = ["t-", "pilot."]"#).unwrap();
        let cfg = Config::from_parts(None, file, no_env).unwrap();
        let pilot = &cfg.native_pilot;
        assert!(pilot.is_active());
        assert_eq!(pilot.prefixes().unwrap(), ["t-", "pilot."]);
        // Usernames compare in lower case, as they are stored.
        assert!(pilot.allows_user("t-alice"));
        assert!(pilot.allows_user("T-Alice"));
        assert!(pilot.allows_user("pilot.bob"));
        assert!(!pilot.allows_user("alice"));
        assert!(!pilot.allows_user("at-alice"));
        // Shared folders: name and container, case-sensitive.
        assert!(pilot.allows_share("t-share", "t-share"));
        assert!(pilot.allows_share("t-renamed", "t-share"));
        assert!(!pilot.allows_share("T-share", "T-share"));
        assert!(!pilot.allows_share("Test", "Test"));
        // Renamed across the boundary, in either direction: outside.
        assert!(!pilot.allows_share("t-test", "Test"));
        assert!(!pilot.allows_share("Share", "t-share"));
        assert!(pilot.allows_share_name("t-test"));
        assert!(!pilot.allows_share_name("Test"));
        assert!(pilot.describe().contains("\"t-\", \"pilot.\""));

        // The environment wins over the file.
        let file = FileConfig::from_toml_str(r#"native_pilot_prefixes = ["t-"]"#).unwrap();
        let env = |k: &str| match k {
            "RAST_NATIVE_PILOT_PREFIXES" => Some(" x-, y- ,x-".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, file, env).unwrap();
        assert_eq!(cfg.native_pilot.prefixes().unwrap(), ["x-", "y-"]);
        assert!(!cfg.native_pilot.allows_user("t-alice"));
        // An empty variable is unset; the file applies.
        let file = FileConfig::from_toml_str(r#"native_pilot_prefixes = ["t-"]"#).unwrap();
        let env = |k: &str| match k {
            "RAST_NATIVE_PILOT_PREFIXES" => Some("  ".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, file, env).unwrap();
        assert_eq!(cfg.native_pilot.prefixes().unwrap(), ["t-"]);
    }

    #[test]
    fn an_empty_native_pilot_lets_nobody_in() {
        let file = FileConfig::from_toml_str("native_pilot_prefixes = []").unwrap();
        let cfg = Config::from_parts(None, file, no_env).unwrap();
        let pilot = &cfg.native_pilot;
        assert!(pilot.is_active());
        assert!(pilot.prefixes().unwrap().is_empty());
        assert!(!pilot.allows_user("t-alice"));
        assert!(!pilot.allows_user("admin"));
        assert!(!pilot.allows_share("t-share", "t-share"));
        assert!(pilot.describe().contains("nobody"));
        // Only commas in the environment: the empty list too.
        let env = |k: &str| match k {
            "RAST_NATIVE_PILOT_PREFIXES" => Some(",".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, FileConfig::default(), env).unwrap();
        assert!(cfg.native_pilot.is_active());
        assert!(!cfg.native_pilot.allows_user("t-alice"));
    }

    #[test]
    fn native_pilot_prefixes_are_validated() {
        for bad in [
            r#"[""]"#,
            r#"["T-"]"#,
            r#"["-t"]"#,
            r#"["t/"]"#,
            r#"["t -"]"#,
            r#"["tü-"]"#,
            r#"["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]"#,
        ] {
            let file =
                FileConfig::from_toml_str(&format!("native_pilot_prefixes = {bad}")).unwrap();
            let error = Config::from_parts(None, file, no_env).unwrap_err();
            assert!(
                error.to_string().contains("native_pilot_prefixes"),
                "{bad}: {error}"
            );
        }
        let many: Vec<String> = (0..17).map(|n| format!("p{n}-")).collect();
        assert!(NativePilot::with_prefixes(&many).is_err());
        assert!(NativePilot::with_prefixes(&many[..16]).is_ok());
        let longest = "a".repeat(MAX_PILOT_PREFIX);
        assert!(NativePilot::with_prefixes([longest.as_str()]).is_ok());
        let env = |k: &str| match k {
            "RAST_NATIVE_PILOT_PREFIXES" => Some("t-,Bad".into()),
            _ => None,
        };
        assert!(Config::from_parts(None, FileConfig::default(), env).is_err());
        // An unknown key is still refused (forward compatibility of the
        // file is the operator's job: ADR 0007).
        assert!(FileConfig::from_toml_str("native_pilot = [\"t-\"]").is_err());
    }

    #[test]
    fn linux_apps_can_be_quarantined() {
        let file = FileConfig::from_toml_str("linux_apps_enabled = false").unwrap();
        let cfg = Config::from_parts(None, file, no_env).unwrap();
        assert!(!cfg.linux_apps_enabled);
    }

    #[test]
    fn env_overrides_file() {
        let file = FileConfig::from_toml_str(
            r#"
            bind = "127.0.0.1:9000"
            log_level = "warn"
            "#,
        )
        .unwrap();
        let env = |k: &str| match k {
            "RAST_BIND" => Some("0.0.0.0:8721".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, file, env).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:8721".parse().unwrap());
        assert_eq!(cfg.log_level, "warn");
    }

    #[test]
    fn ensure_dirs_keeps_the_traverse_bit_for_the_package_shell_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Pin the native backend off: on a host with Xpra installed the
        // native-enabled default would add the same traverse bit and make this
        // test depend on the machine.
        let config = |shell: bool, system_wide: bool| {
            Config::from_parts(
                None,
                FileConfig {
                    data_dir: Some(dir.path().join("data")),
                    storage_dir: Some(dir.path().join("storage")),
                    packages_shell_path: Some(shell),
                    packages_system_wide: Some(system_wide),
                    native_enabled: Some(false),
                    ..Default::default()
                },
                no_env,
            )
            .unwrap()
        };
        let mode = || {
            std::fs::metadata(dir.path().join("data"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        let on = config(true, true);
        on.ensure_dirs().unwrap();
        assert_eq!(mode(), 0o751);
        assert!(on.packages_links_enabled());
        assert!(warnings(&on)
            .iter()
            .all(|w| !w.contains("packages_system_wide")));
        let off = config(false, true);
        off.ensure_dirs().unwrap();
        assert_eq!(mode(), 0o750);
        assert!(!off.packages_links_enabled(), "needs the shell PATH");
        assert!(warnings(&off)
            .iter()
            .any(|w| w.contains("packages_system_wide")));
        assert!(!config(true, false).packages_links_enabled());
    }

    #[test]
    fn ensure_dirs_adds_the_traverse_bit_when_native_apps_are_enabled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.path().join("data")),
                storage_dir: Some(dir.path().join("storage")),
                packages_shell_path: Some(false),
                native_enabled: Some(true),
                ..Default::default()
            },
            no_env,
        )
        .unwrap();
        cfg.ensure_dirs().unwrap();
        // App identities must traverse the data directory to reach profiles.
        let mode = std::fs::metadata(dir.path().join("data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o751);
    }

    #[test]
    fn setup_token_required_by_default() {
        let cfg = Config::from_parts(None, FileConfig::default(), no_env).unwrap();
        assert!(cfg.setup_token_required);
        let env = |k: &str| match k {
            "RAST_SETUP_TOKEN_REQUIRED" => Some("false".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, FileConfig::default(), env).unwrap();
        assert!(!cfg.setup_token_required);
    }

    #[test]
    fn secure_cookies_auto_for_https() {
        let env = |k: &str| match k {
            "RAST_PUBLIC_URL" => Some("https://rast.example.com".into()),
            _ => None,
        };
        let cfg = Config::from_parts(None, FileConfig::default(), env).unwrap();
        assert!(cfg.secure_cookies);
        assert_eq!(cfg.public_url.as_deref(), Some("https://rast.example.com"));
    }

    #[test]
    fn rejects_bad_cidr() {
        let env = |k: &str| match k {
            "RAST_TRUSTED_PROXIES" => Some("not-a-cidr".into()),
            _ => None,
        };
        assert!(Config::from_parts(None, FileConfig::default(), env).is_err());
    }

    #[test]
    fn rejects_out_of_range_interval() {
        let env = |k: &str| match k {
            "RAST_MONITOR_INTERVAL_SECS" => Some("0".into()),
            _ => None,
        };
        assert!(Config::from_parts(None, FileConfig::default(), env).is_err());
    }

    #[test]
    fn toml_roundtrip_example_parses() {
        let cfg = Config::from_parts(None, FileConfig::default(), no_env).unwrap();
        let text = to_toml_example(&cfg);
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.bind.as_deref(), Some(DEFAULT_BIND));
        assert_eq!(
            parsed.packages_catalog_url.as_deref(),
            Some(OFFICIAL_CATALOG_URL)
        );
        assert_eq!(parsed.packages_check_interval_minutes, Some(360));
        assert!(!text.contains("packages_github_token"));
    }
}
