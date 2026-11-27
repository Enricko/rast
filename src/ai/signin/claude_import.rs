//! Using the Claude Code login already on this server instead of a browser
//! round trip: the `claude` CLI keeps its OAuth session in
//! `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR/.credentials.json`
//! when set), so an administrator who signed in there can let RAST use the
//! same tokens.
//!
//! The file is only read, never written: Claude Code owns it and rotates the
//! tokens itself. macOS's Keychain copy is out of scope (RAST runs on a
//! server, and a Linux install keeps the file).
//! `RAST_CLAUDE_CREDENTIALS` overrides the path so tests never touch a real
//! login.

use std::{env, ffi::OsStr, fs, path::PathBuf};

use serde_json::Value;

use super::Credentials;

/// Where Claude Code keeps its login: the override first (tests), then the
/// directory the CLI was told to use, else `~/.claude`.
fn path() -> Result<PathBuf, String> {
    path_from(
        env::var_os("RAST_CLAUDE_CREDENTIALS").as_deref(),
        env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

/// The path rule itself, without the environment: an override path is taken
/// as-is, `CLAUDE_CONFIG_DIR` replaces `~`, and an empty value means "not
/// set" (the way `env::var_os` filters treat it). Pure, so tests can check
/// the precedence without touching the process environment.
fn path_from(
    override_path: Option<&OsStr>,
    config_dir: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(override_path) = override_path.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(override_path));
    }
    if let Some(dir) = config_dir.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join(".credentials.json"));
    }
    let home = home
        .filter(|home| !home.is_empty())
        .ok_or("cannot find Claude Code's login: HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".claude")
        .join(".credentials.json"))
}

/// Read the saved login and map it to RAST's [`Credentials`].
pub(crate) fn import(now_ms: i64) -> Result<Credentials, String> {
    let path = path()?;
    let text = fs::read_to_string(&path).map_err(|_| {
        format!(
            "Claude Code is not signed in on this server ({} cannot be read); sign in with the claude CLI first",
            path.display()
        )
    })?;
    let saved: Value = serde_json::from_str(&text).map_err(|_| {
        "Claude Code's saved login cannot be read; sign in with the claude CLI again".to_string()
    })?;
    credentials_from(&saved, now_ms)
}

/// Map the CLI's saved JSON to RAST's credentials. The file has one shape:
/// `{"claudeAiOauth": {accessToken, refreshToken, expiresAt, scopes,
/// subscriptionType, emailAddress?}}` (a flat blob without the wrapper is
/// accepted too, as the plugin does). Pure, so the mapping is testable
/// without a file.
fn credentials_from(saved: &Value, now_ms: i64) -> Result<Credentials, String> {
    // The wrapper is Claude Code's current shape; the flat one is read for
    // older files (and is what the DSH plugin also accepts).
    let oauth = saved
        .get("claudeAiOauth")
        .filter(|value| value.is_object())
        .unwrap_or(saved);
    let text_field = |key: &str| {
        oauth
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let access_token = text_field("accessToken")
        .ok_or("Claude Code's saved login has no access token; sign in with the claude CLI again")?
        .to_string();
    let refresh_token = text_field("refreshToken")
        .ok_or("Claude Code's saved login has no refresh token; sign in with the claude CLI again")?
        .to_string();
    // Claude Code stores milliseconds since the epoch; a login without an
    // expiry is treated as stale, so its first use refreshes it.
    let expires_at = oauth
        .get("expiresAt")
        .and_then(milliseconds)
        .unwrap_or(now_ms);
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at,
        refreshed_at: now_ms,
        // The file names no account id (Claude's wire does not need one); the
        // address and plan are shown on the settings page.
        account_id: String::new(),
        email: text_field("emailAddress")
            .or_else(|| text_field("email"))
            .unwrap_or_default()
            .to_string(),
        plan: text_field("subscriptionType")
            .unwrap_or_default()
            .to_string(),
        project_id: String::new(),
    })
}

/// A JSON number as milliseconds (Claude Code writes a whole number; accept
/// a float or a numeric string too).
fn milliseconds(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Tests that point the override somewhere share this lock: environment
    /// variables are process-wide, and the rest of the suite ignores this one.
    static ENV: Mutex<()> = Mutex::new(());

    /// Points `RAST_CLAUDE_CREDENTIALS` at `path` until dropped, so a failed
    /// assertion cannot leave the test suite reading a real login.
    struct Override;

    impl Override {
        fn set(path: &std::path::Path) -> Override {
            std::env::set_var("RAST_CLAUDE_CREDENTIALS", path);
            Override
        }
    }

    impl Drop for Override {
        fn drop(&mut self) {
            std::env::remove_var("RAST_CLAUDE_CREDENTIALS");
        }
    }

    /// The path rule: the test override wins, `CLAUDE_CONFIG_DIR` replaces
    /// `~`, `~` is the last resort, and no HOME is a clear error. Checked
    /// without setting any environment variable.
    #[test]
    fn the_login_path_follows_the_cli() {
        let override_path = PathBuf::from("/tmp/rast-test/credentials.json");
        let config_dir = PathBuf::from("/etc/claude-config");
        let home = PathBuf::from("/home/someone");
        assert_eq!(
            path_from(
                Some(override_path.as_os_str()),
                Some(config_dir.as_os_str()),
                Some(home.as_os_str()),
            )
            .unwrap(),
            override_path
        );
        assert_eq!(
            path_from(None, Some(config_dir.as_os_str()), Some(home.as_os_str())).unwrap(),
            config_dir.join(".credentials.json")
        );
        assert_eq!(
            path_from(None, None, Some(home.as_os_str())).unwrap(),
            home.join(".claude").join(".credentials.json")
        );
        let error = path_from(None, None, None).unwrap_err();
        assert!(error.contains("HOME is not set"), "{error}");
        // An empty value means "not set", as the env filter above treats it.
        assert_eq!(
            path_from(
                Some(OsStr::new("")),
                Some(OsStr::new("")),
                Some(home.as_os_str())
            )
            .unwrap(),
            home.join(".claude").join(".credentials.json")
        );
    }

    #[test]
    fn claude_codes_saved_login_is_mapped() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            json!({
                "claudeAiOauth": {
                    "accessToken": "at-secret",
                    "refreshToken": "rt-secret",
                    "expiresAt": 1_700_000_000_000_i64,
                    "scopes": ["user:inference"],
                    "subscriptionType": "max",
                    "emailAddress": "someone@example.com"
                }
            })
            .to_string(),
        )
        .unwrap();
        let _override = Override::set(&path);
        let credentials = import(1_000).unwrap();
        assert_eq!(credentials.access_token, "at-secret");
        assert_eq!(credentials.refresh_token, "rt-secret");
        assert_eq!(credentials.expires_at, 1_700_000_000_000);
        assert_eq!(credentials.refreshed_at, 1_000);
        assert_eq!(credentials.plan, "max");
        assert_eq!(credentials.email, "someone@example.com");
        assert!(credentials.needs_refresh(1_700_000_000_000));
    }

    /// The mapping itself, without a file: the flat blob, a missing expiry
    /// (treated as stale), a numeric-string expiry, and tokens that are
    /// blank rather than absent.
    #[test]
    fn the_saved_blob_maps_both_shapes() {
        let flat = json!({
            "accessToken": "at",
            "refreshToken": "rt",
            "expiresAt": "1700000000000",
            "subscriptionType": "pro"
        });
        let credentials = credentials_from(&flat, 42).unwrap();
        assert_eq!(credentials.access_token, "at");
        assert_eq!(credentials.refresh_token, "rt");
        assert_eq!(credentials.expires_at, 1_700_000_000_000);
        assert_eq!(credentials.refreshed_at, 42);
        assert_eq!(credentials.plan, "pro");
        assert_eq!(
            (credentials.account_id.as_str(), credentials.email.as_str()),
            ("", "")
        );
        // No expiry: stale at once, so its first use refreshes it.
        let no_expiry =
            credentials_from(&json!({"accessToken": "at", "refreshToken": "rt"}), 99).unwrap();
        assert_eq!(no_expiry.expires_at, 99);
        assert!(no_expiry.expired(99));
        // A token of spaces is no token.
        assert!(credentials_from(&json!({"accessToken": "  "}), 0).is_err());
    }

    #[test]
    fn a_missing_or_broken_login_is_a_clear_error() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        {
            let _override = Override::set(&dir.path().join("missing.json"));
            let error = import(0).unwrap_err();
            assert!(error.contains("Claude Code is not signed in"), "{error}");
        }
        {
            let path = dir.path().join(".credentials.json");
            std::fs::write(&path, "not json").unwrap();
            let _override = Override::set(&path);
            assert!(import(0).is_err());
            std::fs::write(
                &path,
                json!({"claudeAiOauth": {"accessToken": "at"}}).to_string(),
            )
            .unwrap();
            let error = import(0).unwrap_err();
            assert!(error.contains("no refresh token"), "{error}");
        }
    }
}
