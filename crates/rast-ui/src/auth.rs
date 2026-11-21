//! Device-local account: the app-unlock password kept on this device.
//!
//! This is deliberately **not** server authentication. The RAST server will
//! authenticate users itself when the server phase lands; this device login is
//! the local unlock for the standalone/offline shell (the app must be usable
//! when the server is away, and the browser preview has no server at all).
//!
//! Storage: the username and an **Argon2id** PHC hash. The password itself is
//! never stored, never logged and never sent anywhere. Natively that is one
//! small file, mode 0600 on Unix; in the browser it is this browser's
//! `localStorage` (a per-browser device, weakened by the lack of file
//! permissions — a real deployment should use the server's HttpOnly session
//! cookie once it exists). Rationale and the server-side recommendation are in
//! docs/AUTH-STORAGE.md.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use std::fs;

/// Minimum local password length. Device unlock, not a server credential.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Sanity cap so a paste cannot make hashing hang for seconds.
pub const MAX_PASSWORD_LEN: usize = 1024;
/// Longest accepted username.
pub const MAX_USERNAME_LEN: usize = 64;

/// A device-local account record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAccount {
    pub username: String,
    /// Argon2id PHC string (`$argon2id$v=19$m=…$salt$hash`).
    pub phc: String,
}

impl DeviceAccount {
    /// Default location: `RAST_ACCOUNT` if set, else
    /// `<config dir>/rast/device-account`.
    pub fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("RAST_ACCOUNT") {
            return Some(PathBuf::from(path));
        }
        #[cfg(target_arch = "wasm32")]
        {
            // No files in the browser: the account lives in localStorage. The
            // marker path keeps the caller's Option<PathBuf> shape unchanged.
            Some(PathBuf::from("browser-local-storage"))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::config::Config::path().map(|config| config.with_file_name("device-account"))
        }
    }

    /// Load the account from its default location.
    pub fn load() -> Option<Self> {
        Self::path().and_then(|path| Self::load_from(&path))
    }

    /// Read the device account. `None` when none exists or it is malformed.
    pub fn load_from(path: &Path) -> Option<Self> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            Self::load_browser().ok().flatten()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let text = fs::read_to_string(path).ok()?;
            Self::parse(&text)
        }
    }

    /// Parse the small `key = value` format; unknown keys and comments are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        let mut username = None;
        let mut phc = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            match key.trim() {
                "username" => username = Some(value.to_owned()),
                "hash" => phc = Some(value.to_owned()),
                _ => {}
            }
        }
        Some(Self {
            username: username?,
            phc: phc?,
        })
    }

    pub fn render(&self) -> String {
        format!(
            "# RAST device account. Argon2id hash only: the password is never stored.\n\
             username = {}\n\
             hash = {}\n",
            self.username, self.phc
        )
    }

    /// Create an account from a username and password, hashing with Argon2id.
    pub fn create(username: &str, password: &str) -> Result<Self, String> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let phc = hash_password(password)?;
        Ok(Self { username, phc })
    }

    /// Check a password against the stored hash.
    pub fn verify(&self, password: &str) -> Result<bool, String> {
        verify_password(password, &self.phc)
    }

    /// Save the account. Natively atomic (temp file + rename) with owner-only
    /// permissions on Unix; in the browser to this browser's localStorage.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            self.save_browser()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            let temp = path.with_extension("account.tmp");
            fs::write(&temp, self.render())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&temp)?.permissions();
                permissions.set_mode(0o600);
                fs::set_permissions(&temp, permissions)?;
            }
            fs::rename(&temp, path)
        }
    }
    #[cfg(target_arch = "wasm32")]
    pub fn load_browser() -> io::Result<Option<Self>> {
        let raw = browser_storage()?
            .get_item("rast.device-account")
            .map_err(|_| io::Error::other("Could not read the local login."))?;
        raw.map(|record| {
            Self::parse(&record)
                .ok_or_else(|| io::Error::other("The saved local login could not be read."))
        })
        .transpose()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save_browser(&self) -> io::Result<()> {
        browser_storage()?
            .set_item("rast.device-account", &self.render())
            .map_err(|_| io::Error::other("The browser refused to save the local login."))
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_storage() -> io::Result<web_sys::Storage> {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .ok_or_else(|| {
            io::Error::other("Browser storage is unavailable. Allow site data for RAST and reload.")
        })
}

fn validate_username(username: &str) -> Result<String, String> {
    let username = username.trim();
    if username.is_empty() {
        return Err("Enter a username.".to_owned());
    }
    if username.chars().count() > MAX_USERNAME_LEN {
        return Err(format!(
            "The username may be at most {MAX_USERNAME_LEN} characters."
        ));
    }
    if username.chars().any(|c| c.is_control()) {
        return Err("The username contains control characters.".to_owned());
    }
    Ok(username.to_owned())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "Use at least {MIN_PASSWORD_LEN} characters for the device password."
        ));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!(
            "The password may be at most {MAX_PASSWORD_LEN} bytes."
        ));
    }
    Ok(())
}

/// Argon2id with the crate defaults (m=19456 KiB, t=2, p=1).
pub fn hash_password(password: &str) -> Result<String, String> {
    use argon2::Argon2;
    use argon2::password_hash::{PasswordHasher, SaltString};

    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes)
        .map_err(|error| format!("cannot read random bytes: {error}"))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|error| format!("cannot encode salt: {error}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| format!("cannot hash the password: {error}"))
}

pub fn verify_password(password: &str, phc: &str) -> Result<bool, String> {
    use argon2::Argon2;
    use argon2::password_hash::{PasswordHash, PasswordVerifier};

    let parsed = PasswordHash::new(phc)
        .map_err(|error| format!("the stored hash is unreadable: {error}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_account_verifies_the_right_password_only() {
        let account = DeviceAccount::create("admin", "correct horse").expect("account is created");
        assert!(account.phc.starts_with("$argon2id$"), "Argon2id PHC string");
        assert!(
            !account.phc.contains("correct horse"),
            "no plaintext in the hash"
        );
        assert!(account.verify("correct horse").expect("verify runs"));
        assert!(!account.verify("wrong horse").expect("verify runs"));
    }

    #[test]
    fn short_passwords_and_empty_usernames_are_rejected() {
        assert!(DeviceAccount::create("admin", "short").is_err());
        assert!(DeviceAccount::create("   ", "correct horse").is_err());
        assert!(DeviceAccount::create(&"x".repeat(MAX_USERNAME_LEN + 1), "correct horse").is_err());
    }

    #[test]
    fn the_file_round_trips_and_only_holds_the_hash() {
        let base = std::env::temp_dir().join(format!("rast-auth-test-{}", std::process::id()));
        let path = base.join("device-account");
        let account = DeviceAccount::create("admin", "correct horse").expect("account is created");
        account.save_to(&path).expect("account file is written");
        let raw = fs::read_to_string(&path).expect("account file is readable");
        assert!(
            !raw.contains("correct horse"),
            "the password is never written"
        );
        assert_eq!(DeviceAccount::load_from(&path), Some(account));
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn the_account_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("rast-auth-perm-{}", std::process::id()));
        let path = base.join("device-account");
        DeviceAccount::create("admin", "correct horse")
            .expect("account is created")
            .save_to(&path)
            .expect("account file is written");
        let mode = fs::metadata(&path)
            .expect("file exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "only the owner can read the device login"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_malformed_file_is_rejected_instead_of_panicking() {
        assert_eq!(DeviceAccount::parse("username = admin\n"), None);
        assert_eq!(DeviceAccount::parse("hash = $argon2id$…\n"), None);
        assert_eq!(DeviceAccount::parse(""), None);
    }
}
