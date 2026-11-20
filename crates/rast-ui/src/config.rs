//! Persisted client settings: which RAST server to talk to and whether to keep
//! reconnecting when it goes away.
//!
//! Plain `key = value` text on purpose: no dependency, readable during setup, and
//! easy to edit by hand on a machine where the app cannot start.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The server the client tries when no settings file exists yet.
pub const DEFAULT_SERVER: &str = "desktop.lufria.tech:443";

const DIR: &str = "rast";
const FILE: &str = "client.conf";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `host[:port]`, optionally with an `https://` prefix.
    pub server: String,
    /// Keep probing and reconnect by itself when the server goes away.
    pub auto_reconnect: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: DEFAULT_SERVER.to_owned(),
            auto_reconnect: true,
        }
    }
}

impl Config {
    /// Where settings live: `RAST_CONFIG` if set, else the platform config dir.
    ///
    /// Linux: `$XDG_CONFIG_HOME/rast/client.conf` (or `~/.config/...`).
    /// macOS: `~/Library/Application Support/rast/client.conf`.
    /// Windows: `%APPDATA%\rast\client.conf`.
    pub fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("RAST_CONFIG") {
            return Some(PathBuf::from(path));
        }
        config_base().map(|base| base.join(DIR).join(FILE))
    }

    /// Load the user's settings, applying `RAST_AUTO_RECONNECT` when it is set.
    pub fn load() -> Self {
        let mut config = Self::path()
            .and_then(|path| Self::load_from(&path).ok().flatten())
            .unwrap_or_default();
        if let Some(value) = env_auto_reconnect() {
            config.auto_reconnect = value;
        }
        config
    }

    /// Read one settings file. `Ok(None)` when the file does not exist.
    pub fn load_from(path: &Path) -> io::Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::parse(&text))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Parse the settings format; unknown keys and malformed lines are ignored so
    /// a future version's file still loads in an older client.
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "server" => config.server = value.to_owned(),
                "auto_reconnect" => {
                    if let Some(on) = parse_bool(value) {
                        config.auto_reconnect = on;
                    }
                }
                _ => {}
            }
        }
        config
    }

    pub fn render(&self) -> String {
        format!(
            "# RAST client settings. Edited by the app's Settings screen; safe to edit by hand.\n\
             # server: host[:port], optionally with an https:// prefix.\n\
             server = {}\n\
             auto_reconnect = {}\n",
            self.server, self.auto_reconnect
        )
    }

    /// Save to the platform settings path, ignoring failures (the app still runs,
    /// the change just does not survive a restart).
    pub fn save(&self) {
        if let Some(path) = Self::path()
            && let Err(error) = self.save_to(&path)
        {
            eprintln!(
                "rast: could not save settings to {}: {error}",
                path.display()
            );
        }
    }

    /// Save atomically (temp file + rename), creating the directory.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("conf.tmp");
        fs::write(&temp, self.render())?;
        fs::rename(&temp, path)
    }
}

/// `RAST_AUTO_RECONNECT=0|false|no|off` turns automatic reconnection off for this
/// run, for setups where the setting cannot be changed from the app.
pub fn env_auto_reconnect() -> Option<bool> {
    parse_bool(&std::env::var("RAST_AUTO_RECONNECT").ok()?)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn config_base() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn config_base() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn config_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let config = Config {
            server: "192.0.2.10:8720".to_owned(),
            auto_reconnect: false,
        };
        assert_eq!(Config::parse(&config.render()), config);
    }

    #[test]
    fn comments_unknown_keys_and_noise_are_ignored() {
        let config = Config::parse(
            "# a comment\n\
             \n\
             server = example.test\n\
             future_key = whatever\n\
             not a key value line\n\
             auto_reconnect = off\n",
        );
        assert_eq!(config.server, "example.test");
        assert!(!config.auto_reconnect);
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join("rast-config-does-not-exist/client.conf");
        assert_eq!(
            Config::load_from(&path).expect("io error only for real errors"),
            None
        );
        assert_eq!(Config::default().server, DEFAULT_SERVER);
    }

    #[test]
    fn save_to_creates_the_directory_and_loads_back() {
        let base = std::env::temp_dir().join(format!("rast-config-test-{}", std::process::id()));
        let path = base.join("nested").join("client.conf");
        let config = Config {
            server: "example.test:443".to_owned(),
            auto_reconnect: true,
        };
        config.save_to(&path).expect("saving settings works");
        assert_eq!(
            Config::load_from(&path).expect("loading settings works"),
            Some(config)
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bool_spellings_are_accepted() {
        for on in ["1", "true", "yes", "on", "TRUE"] {
            assert_eq!(parse_bool(on), Some(true));
        }
        for off in ["0", "false", "no", "off", "FALSE"] {
            assert_eq!(parse_bool(off), Some(false));
        }
        assert_eq!(parse_bool("maybe"), None);
    }
}
