//! Runtime environment for terminal shells.
//!
//! Composes the PATH and runtime variables for a shell from the managed
//! runtimes installed by Package Center (`<data_dir>/packages/runtimes/...`)
//! plus the operating-system runtimes detected by Package Center's probes.
//! Shared runtime trees are owned by RAST and read-only for users; caches and
//! package-manager state stay in the identity's private home (`HOME` is set by
//! the helper), so no shared writable state is ever exported.
//!
//! The environment a shell receives is chosen when the session starts. An
//! existing tab never changes underneath the user; the UI asks them to reopen
//! the tab instead (documented in docs/TERMINAL.md).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db,
    error::{AppError, AppResult},
    packages::{
        install::Layout,
        system::{self, SystemInstall},
    },
};

/// Settings key holding the JSON map `{ "<user id>": { "<runtime>": "<version>" } }`.
const DEFAULTS_SETTING: &str = "terminal.runtime_defaults";
/// How deep below a version root executables are searched (archives differ).
const MAX_SEARCH_DEPTH: usize = 3;

/// Name shown in the Terminal environment panel.
fn display_name(id: &str) -> &'static str {
    match id {
        "python" => "Python",
        "php" => "PHP CLI",
        "go" => "Go",
        "nodejs" => "Node.js",
        "rust" => "Rust toolchain",
        "java" => "Java (Temurin JDK)",
        "bun" => "Bun",
        "deno" => "Deno",
        "dotnet" => ".NET SDK",
        _ => "Runtime",
    }
}

/// Executable names used to find the bin directory of an installed version.
fn executables(id: &str) -> &'static [&'static str] {
    match id {
        "python" => &["python3", "python"],
        "php" => &["php"],
        "go" => &["go", "gofmt"],
        "nodejs" => &["node", "npm"],
        "rust" => &["rustc", "cargo"],
        "java" => &["java", "javac"],
        "bun" => &["bun"],
        "deno" => &["deno"],
        "dotnet" => &["dotnet"],
        _ => &[],
    }
}

/// Prefix environment variables for one selected runtime root.
fn runtime_env(id: &str, root: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match id {
        "java" => {
            env.insert("JAVA_HOME".to_string(), root.display().to_string());
        }
        "dotnet" => {
            env.insert("DOTNET_ROOT".to_string(), root.display().to_string());
            env.insert("DOTNET_CLI_TELEMETRY_OPTOUT".to_string(), "1".to_string());
            env.insert("DOTNET_NOLOGO".to_string(), "1".to_string());
        }
        "deno" => {
            // The module cache must stay writable per user; HOME does that.
            env.insert("DENO_NO_UPDATE_CHECK".to_string(), "1".to_string());
        }
        "rust" => {
            // The managed toolchain is a plain standalone prefix (not rustup);
            // CARGO_HOME defaults to ~/.cargo under the identity's HOME.
            env.insert("CARGO_NET_OFFLINE".to_string(), "false".to_string());
        }
        _ => {}
    }
    env
}

/// Find directories (relative to `root`) containing one of `names`.
/// Handles layouts with a wrapper directory (JDK/Go/dotnet archives) as well
/// as flat ones (Bun/Deno).
fn find_bin_dirs(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    if names.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut queue = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = queue.pop() {
        if names.iter().any(|name| dir.join(name).is_file()) {
            found.push(dir);
            continue;
        }
        if depth >= MAX_SEARCH_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if matches!(
                        name,
                        "bin"
                            | "lib"
                            | "share"
                            | "include"
                            | "man"
                            | "libexec"
                            | "sdk"
                            | "shared"
                            | "packs"
                            | "templates"
                    ) {
                        // Still look inside, but do not descend into huge trees
                        // unnecessarily.
                    }
                }
                queue.push((path, depth + 1));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// One runtime as shown in the Terminal environment panel.
#[derive(Debug, Clone)]
pub struct RuntimeEntry {
    pub id: String,
    pub source: String,
    pub current: Option<String>,
    pub versions: Vec<String>,
    pub default: Option<String>,
    pub selected: Option<String>,
    pub home: Option<String>,
    pub bin_dirs: Vec<String>,
    pub requirements: Vec<String>,
    pub missing: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Composed environment for one shell plus the panel data.
#[derive(Debug, Clone, Default)]
pub struct TerminalEnvironment {
    pub runtimes: Vec<RuntimeEntry>,
    pub defaults: BTreeMap<String, String>,
    pub path: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl TerminalEnvironment {
    /// The selection actually used by a new shell (defaults + overrides).
    pub fn selected_overrides(
        &self,
        overrides: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut selected = BTreeMap::new();
        for entry in &self.runtimes {
            let chosen = overrides
                .get(&entry.id)
                .or(entry.selected.as_ref())
                .or(entry.default.as_ref())
                .or(entry.current.as_ref())
                .cloned();
            if let Some(version) = chosen {
                selected.insert(entry.id.clone(), version);
            }
        }
        selected
    }

    /// Environment variables for one shell, including the composed PATH.
    pub fn shell_env(&self, selected: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let mut path: Vec<String> = Vec::new();
        for entry in &self.runtimes {
            let Some(version) = selected.get(&entry.id) else {
                continue;
            };
            if entry
                .selected
                .as_ref()
                .is_some_and(|value| value != version)
            {
                continue;
            }
            // Only the chosen version contributes to PATH.
            let chosen = entry
                .versions
                .iter()
                .find(|candidate| *candidate == version)
                .or(entry.versions.first());
            let Some(chosen) = chosen else { continue };
            let _ = chosen;
            if entry.source == "rast" {
                for hint in &entry.env {
                    env.insert(hint.0.clone(), hint.1.clone());
                }
                for bin in &entry.bin_dirs {
                    path.push(bin.clone());
                }
            } else if let Some(home) = &entry.home {
                let bin = Path::new(home).join("bin");
                if bin.is_dir() {
                    path.push(bin.display().to_string());
                } else {
                    path.push(home.clone());
                }
            }
        }
        path.extend(self.path.iter().cloned());
        path.dedup();
        env.insert("PATH".to_string(), path.join(":"));
        env
    }

    /// JSON for the Terminal environment panel.
    /// `root`: whether this person's shell is the administrator shell (a
    /// super administrator with `terminal_admin_root`).
    pub fn to_json(
        &self,
        terminal: &crate::config::TerminalConfig,
        enabled: bool,
        root: bool,
    ) -> Value {
        let runtimes: Vec<Value> = self
            .runtimes
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "name": display_name(&entry.id),
                    "source": entry.source,
                    "current": entry.current,
                    "versions": entry.versions,
                    "default": entry.default,
                    "selected": entry.selected,
                    "home": entry.home,
                    "binDirs": entry.bin_dirs,
                    "requirements": entry.requirements,
                    "missing": entry.missing,
                    "env": entry.env,
                })
            })
            .collect();
        json!({
            "enabled": enabled && crate::terminal::manager::supported(),
            "adminRoot": root && terminal.admin_account.is_none(),
            "adminAccount": terminal.admin_account.as_ref().filter(|_| root),
            "shell": if Path::new("/bin/bash").is_file() { "/bin/bash" } else { "/bin/sh" },
            "fallbackShell": "/bin/sh",
            "maxSessions": terminal.max_sessions,
            "graceSeconds": terminal.grace_seconds,
            "idleMinutes": terminal.idle_minutes,
            "runtimes": runtimes,
            "defaults": self.defaults,
            "pathPreview": self.path,
        })
    }
}

/// Read this user's saved defaults.
pub async fn read_defaults(pool: &SqlitePool, user_id: i64) -> BTreeMap<String, String> {
    let Some(raw) = db::get_setting(pool, DEFAULTS_SETTING).await else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return BTreeMap::new();
    };
    value
        .get(user_id.to_string())
        .and_then(|entry| entry.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Save this user's defaults (other users' entries are preserved).
pub async fn set_defaults(
    pool: &SqlitePool,
    user_id: i64,
    defaults: &BTreeMap<String, String>,
) -> AppResult<()> {
    let mut all: Value = db::get_setting(pool, DEFAULTS_SETTING)
        .await
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    if !all.is_object() {
        all = json!({});
    }
    let mut entry = serde_json::Map::new();
    for (runtime, version) in defaults {
        if runtime.len() > 64 || version.len() > 64 {
            return Err(AppError::bad_request("invalid runtime default"));
        }
        entry.insert(runtime.clone(), Value::String(version.clone()));
    }
    let _ = all
        .as_object_mut()
        .map(|map| map.insert(user_id.to_string(), Value::Object(entry)));
    db::set_setting(
        pool,
        DEFAULTS_SETTING,
        &serde_json::to_string(&all).unwrap_or_else(|_| "{}".into()),
    )
    .await
    .map_err(|error| AppError::internal(format!("cannot save runtime defaults: {error}")))
}

fn version_strings(entries: Vec<std::ffi::OsString>) -> Vec<String> {
    let mut versions: Vec<String> = entries
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| {
            name != "current"
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        })
        .collect();
    versions.sort_by(|left, right| version_cmp(left, right));
    versions.reverse();
    versions
}

fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    crate::packages::version::Version::parse(left)
        .and_then(|a| crate::packages::version::Version::parse(right).map(|b| a.cmp(&b)))
        .unwrap_or_else(|| left.cmp(right))
}

/// Installed versions of one managed runtime, newest first.
fn installed_versions(layout: &Layout, id: &str) -> Vec<String> {
    let dir = layout.package_dir(id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    version_strings(entries.flatten().map(|entry| entry.file_name()).collect())
}

/// The version the `current` symlink points at, if any.
fn current_version(layout: &Layout, id: &str) -> Option<String> {
    let link = layout.current_link(id);
    let target = std::fs::read_link(&link).ok()?;
    target
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// Build the environment and the panel data for `user_id`.
pub async fn environment(
    cfg: &Config,
    pool: &SqlitePool,
    user_id: i64,
    system_installs: &std::collections::HashMap<String, SystemInstall>,
) -> AppResult<TerminalEnvironment> {
    let layout = Layout::new(cfg.packages_dir());
    let defaults = read_defaults(pool, user_id).await;
    let mut runtimes: Vec<RuntimeEntry> = Vec::new();

    let managed_ids = [
        "python", "php", "go", "nodejs", "rust", "java", "bun", "deno", "dotnet",
    ];
    for id in managed_ids {
        let versions = installed_versions(&layout, id);
        if versions.is_empty() && !system_installs.contains_key(id) {
            continue;
        }
        let current = current_version(&layout, id);
        let default = defaults
            .get(id)
            .cloned()
            .filter(|version| versions.contains(version));
        let selected = default
            .clone()
            .or(current.clone())
            .or_else(|| versions.first().cloned());
        let home = selected
            .as_ref()
            .map(|version| layout.version_dir(id, version))
            .filter(|path| path.is_dir());
        let bin_dirs = home
            .as_ref()
            .map(|root| {
                find_bin_dirs(root, executables(id))
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let env = home
            .as_ref()
            .map(|root| runtime_env(id, root))
            .unwrap_or_default();
        let mut entry = RuntimeEntry {
            id: id.to_string(),
            source: "rast".to_string(),
            current: current.clone(),
            versions: versions.clone(),
            default,
            selected,
            home: home.map(|path| path.display().to_string()),
            bin_dirs,
            requirements: requirements(id),
            missing: missing_dependencies(id),
            env,
        };
        if entry.versions.is_empty() {
            // Only the operating-system runtime exists.
            if let Some(system) = system_installs.get(id) {
                entry.source = "system".to_string();
                entry.current = system.version.clone();
                entry.versions = system.version.clone().into_iter().collect::<Vec<String>>();
                entry.selected = system.version.clone();
                entry.home = Path::new(&system.real_path)
                    .parent()
                    .and_then(|bin| {
                        if bin.file_name().is_some_and(|name| name == "bin") {
                            bin.parent()
                        } else {
                            Some(bin)
                        }
                    })
                    .map(|path| path.display().to_string());
                entry.missing = missing_dependencies(id);
            }
        }
        runtimes.push(entry);
    }

    let mut path: Vec<String> = vec![
        "/usr/local/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/sbin".to_string(),
        "/usr/bin".to_string(),
        "/sbin".to_string(),
        "/bin".to_string(),
    ];
    let shim_dir = layout.bin_dir();
    if shim_dir.is_dir() {
        path.insert(0, shim_dir.display().to_string());
    }
    let env = TerminalEnvironment {
        runtimes,
        defaults,
        path,
        env: BTreeMap::new(),
    };
    Ok(env)
}

fn requirements(id: &str) -> Vec<String> {
    match id {
        "java" => vec!["glibc 2.17+".into()],
        "rust" => vec![
            "glibc 2.17+".into(),
            "a C toolchain (cc) for linking".into(),
        ],
        "dotnet" => vec!["ESL: glibc, libicu, libssl, zlib, tzdata".into()],
        "bun" => vec!["glibc 2.17+".into()],
        "deno" => vec!["glibc 2.27+".into()],
        _ => Vec::new(),
    }
}

/// Cheap, non-fatal dependency visibility for the environment panel.
fn missing_dependencies(id: &str) -> Vec<String> {
    let mut missing = Vec::new();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let on_path = |name: &str| {
        std::env::split_paths(&path_var).any(|dir| {
            let candidate = dir.join(name);
            candidate.is_file()
        })
    };
    match id {
        "rust" => {
            if !on_path("cc") && !on_path("gcc") && !on_path("clang") {
                missing.push("no C compiler/linker found (cc/gcc/clang)".to_string());
            }
        }
        "dotnet" => {
            if !lib_available("libicu") {
                missing.push("libicu is missing (the .NET runtime cannot start)".to_string());
            }
            if !lib_available("libssl") && !lib_available("libcrypto") {
                missing.push("libssl/libcrypto is missing".to_string());
            }
        }
        _ => {}
    }
    missing
}

fn lib_available(name: &str) -> bool {
    let Ok(output) = std::process::Command::new("ldconfig").arg("-p").output() else {
        // Unknown on systems without ldconfig; do not claim a problem.
        return true;
    };
    String::from_utf8_lossy(&output.stdout).contains(name)
}

/// Whether a given runtime/version pair is installed (used to validate
/// per-terminal overrides server-side).
pub fn version_installed(cfg: &Config, runtime: &str, version: &str) -> bool {
    let layout = Layout::new(cfg.packages_dir());
    installed_versions(&layout, runtime)
        .iter()
        .any(|candidate| candidate == version)
}

/// Convenience for the API layer.
pub fn primary_command(id: &str) -> Option<&'static str> {
    system::primary_command(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_sorted_numerically_newest_first() {
        let sorted = version_strings(vec!["1.9.0".into(), "1.10.0".into(), "1.2.0".into()]);
        assert_eq!(sorted, vec!["1.10.0", "1.9.0", "1.2.0"]);
    }

    #[test]
    fn bin_dirs_find_wrapped_and_flat_layouts() {
        let temp = tempfile::tempdir().unwrap();
        // Flat layout: <root>/bin/rustc
        let flat = temp.path().join("flat");
        std::fs::create_dir_all(flat.join("bin")).unwrap();
        std::fs::write(flat.join("bin/rustc"), b"").unwrap();
        assert_eq!(find_bin_dirs(&flat, &["rustc"]), vec![flat.join("bin")]);

        // Wrapped layout: <root>/jdk-25/bin/java
        let wrapped = temp.path().join("wrapped");
        std::fs::create_dir_all(wrapped.join("jdk-25/bin")).unwrap();
        std::fs::write(wrapped.join("jdk-25/bin/java"), b"").unwrap();
        assert_eq!(
            find_bin_dirs(&wrapped, &["java"]),
            vec![wrapped.join("jdk-25/bin")]
        );
        std::fs::write(wrapped.join("jdk-25/bin/javac"), b"").unwrap();
        assert_eq!(
            find_bin_dirs(&wrapped, &["java", "javac"]),
            vec![wrapped.join("jdk-25/bin")]
        );
    }

    #[test]
    fn runtime_env_sets_java_home_and_dotnet_root() {
        let root = PathBuf::from("/opt/runtimes/jdk-25");
        assert_eq!(
            runtime_env("java", &root)
                .get("JAVA_HOME")
                .map(String::as_str),
            Some("/opt/runtimes/jdk-25")
        );
        assert_eq!(
            runtime_env("dotnet", &root)
                .get("DOTNET_ROOT")
                .map(String::as_str),
            Some("/opt/runtimes/jdk-25")
        );
        assert!(runtime_env("deno", &root).contains_key("DENO_NO_UPDATE_CHECK"));
    }

    #[test]
    fn shell_env_prepends_selected_rast_bin_dirs_and_base_path() {
        let env = TerminalEnvironment {
            runtimes: vec![RuntimeEntry {
                id: "rust".into(),
                source: "rast".into(),
                current: Some("1.100.0".into()),
                versions: vec!["1.100.0".into()],
                default: None,
                selected: Some("1.100.0".into()),
                home: Some("/var/lib/rast/packages/runtimes/rust/1.100.0".into()),
                bin_dirs: vec!["/var/lib/rast/packages/runtimes/rust/1.100.0/bin".into()],
                requirements: Vec::new(),
                missing: Vec::new(),
                env: BTreeMap::new(),
            }],
            defaults: BTreeMap::new(),
            path: vec!["/usr/bin".into(), "/bin".into()],
            env: BTreeMap::new(),
        };
        let selected = env.selected_overrides(&BTreeMap::new());
        assert_eq!(selected.get("rust").map(String::as_str), Some("1.100.0"));
        let shell = env.shell_env(&selected);
        let path = shell.get("PATH").unwrap();
        assert!(path.starts_with("/var/lib/rast/packages/runtimes/rust/1.100.0/bin:"));
        assert!(path.ends_with("/usr/bin:/bin"));
    }

    #[test]
    fn overrides_win_over_defaults() {
        let env = TerminalEnvironment {
            runtimes: vec![RuntimeEntry {
                id: "python".into(),
                source: "rast".into(),
                current: None,
                versions: vec!["3.15.0".into(), "3.14.0".into()],
                default: Some("3.15.0".into()),
                selected: Some("3.15.0".into()),
                home: None,
                bin_dirs: Vec::new(),
                requirements: Vec::new(),
                missing: Vec::new(),
                env: BTreeMap::new(),
            }],
            ..Default::default()
        };
        let mut overrides = BTreeMap::new();
        overrides.insert("python".to_string(), "3.14.0".to_string());
        let selected = env.selected_overrides(&overrides);
        assert_eq!(selected.get("python").map(String::as_str), Some("3.14.0"));
    }
}
