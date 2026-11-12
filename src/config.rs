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
    pub default_quota_bytes: u64,
    pub secure_cookies: bool,
    pub job_concurrency: usize,
    pub setup_token_required: bool,
    pub trust_forwarded_headers: bool,
    pub packages_catalog_url: String,
    pub packages_catalog_keys: Vec<String>,
    pub packages_check_interval_minutes: u64,
    pub packages_github_token: Option<String>,
    pub packages_shell_path: bool,
    pub packages_system_wide: bool,
    /// Config file that was actually loaded, if any.
    pub loaded_from: Option<PathBuf>,
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
            .unwrap_or_else(|| crate::packages::catalog::OFFICIAL_CATALOG_URL.to_string());
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
            crate::packages::catalog::parse_trusted_key(key).map_err(ConfigError::Invalid)?;
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
            default_quota_bytes,
            secure_cookies,
            job_concurrency,
            setup_token_required,
            trust_forwarded_headers,
            packages_catalog_url,
            packages_catalog_keys,
            packages_check_interval_minutes,
            packages_github_token,
            packages_shell_path,
            packages_system_wide,
            loaded_from,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("rast.db")
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
    /// Linux app uploads waiting to be built: `<data_dir>/linux-apps`.
    pub fn linux_apps_dir(&self) -> PathBuf {
        self.data_dir.join("linux-apps")
    }
    pub fn secret_path(&self) -> PathBuf {
        self.data_dir.join("secret.key")
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
        let traverse = if self.packages_shell_path { 0o001 } else { 0 };
        let _ = std::fs::set_permissions(
            &self.data_dir,
            std::fs::Permissions::from_mode(0o750 | traverse),
        );
        Ok(())
    }
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
        let config = |shell: bool, system_wide: bool| {
            Config::from_parts(
                None,
                FileConfig {
                    data_dir: Some(dir.path().join("data")),
                    storage_dir: Some(dir.path().join("storage")),
                    packages_shell_path: Some(shell),
                    packages_system_wide: Some(system_wide),
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
            Some(crate::packages::catalog::OFFICIAL_CATALOG_URL)
        );
        assert_eq!(parsed.packages_check_interval_minutes, Some(360));
        assert!(!text.contains("packages_github_token"));
    }
}
