//! Google Antigravity sign-in (a browser flow): Google OAuth with PKCE, the
//! pasted loopback callback, then the Cloud Code handshake (`loadCodeAssist` /
//! `onboardUser`) that turns the Google account into the project requests go
//! to. The shared flow lives in [`google`], which this file owns and the
//! Gemini CLI reuses; this file is the Antigravity OAuth client and identity.
//!
//! Checked against the 2026-11-27 references
//! (`dsh-plugin-subscriptions/src/providers/antigravity*.ts` and
//! `9router/src/lib/oauth/services/antigravity.js`): the public desktop
//! client below, the six scopes, `access_type=offline&prompt=consent`, and
//! the Cloud Code backend on `cloudcode-pa.googleapis.com` for the handshake
//! (the daily generation host refuses it).

use super::google::{self, Config, Identity};
use super::{Credentials, Pending, Plan, Started};

/// Where Google sends the browser back; a server elsewhere never receives it,
/// so the person pastes the address.
const REDIRECT_URI: &str = "http://localhost:56123/oauth-callback";
/// The client's scopes: the `aicode` scope the Antigravity CLI requests
/// (without it Google treats the token as a plain consumer one — it answers
/// the shared `aicode-consumers` project, which generation refuses), Cloud
/// Code, its logs and experiments, plus identity.
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/aicode",
    "openid",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// The sign-in's own details: the rest is Google OAuth, and the OAuth client
/// (the app's public one, or the operator's own) comes from the server's
/// environment; see [`google::client_from_env`].
fn config() -> Result<Config, String> {
    let (client_id, client_secret) = google::client_from_env("ANTIGRAVITY", "Antigravity")?;
    Ok(Config {
        client_id,
        client_secret,
        scopes: SCOPES,
        redirect_uri: REDIRECT_URI,
        label: "Antigravity",
        app: "Antigravity",
        identity: Identity::Antigravity,
    })
}

/// Start signing in: the Google consent page, opened in the browser.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    now_ms: i64,
) -> Result<Started, String> {
    google::start(
        Plan::Antigravity,
        &config()?,
        user_id,
        provider_id,
        name,
        now_ms,
    )
}

/// Trade the pasted code for tokens and find the account's Cloud Code
/// project (blocking).
pub(crate) fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    google::exchange(&config()?, pending, code, now_ms)
}

/// New tokens for old (blocking).
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    google::refresh(&config()?, old, now_ms)
}

/// Whether the stored sign-in carries a project that is missing or Google's
/// shared consumer one (the older sign-in flow could store that, and
/// generation against it is refused).
pub(crate) fn needs_repair(old: &Credentials) -> bool {
    let project = if old.project_id.is_empty() {
        old.account_id.as_str()
    } else {
        old.project_id.as_str()
    };
    project.trim().is_empty() || project.trim().eq_ignore_ascii_case("aicode-consumers")
}

/// Find a usable project with the tokens the sign-in already has (blocking,
/// one request): the Cloud Code project Google names, else the account's
/// stable synthetic id. The tokens themselves are untouched.
pub(crate) fn repair(old: &Credentials, _now_ms: i64) -> Result<Option<Credentials>, String> {
    let project = google::repair_project(&config()?, &old.access_token, &old.email);
    if project.is_empty() || project == old.project_id {
        return Ok(None);
    }
    let mut fresh = old.clone();
    fresh.account_id = project.clone();
    fresh.project_id = project;
    Ok(Some(fresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real OAuth client comes from the server's environment; the tests
    /// use a fake one so they never need a credential.
    fn test_client() {
        static SET: std::sync::Once = std::sync::Once::new();
        SET.call_once(|| {
            std::env::set_var("RAST_ANTIGRAVITY_CLIENT_ID", "test-antigravity-client");
            std::env::set_var("RAST_ANTIGRAVITY_CLIENT_SECRET", "test-secret");
        });
    }
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest, Sha256};

    /// The sign-in starts with PKCE and the plan's own client, scopes and
    /// return address; nothing is fetched, and the pending entry answers once
    /// and only to the person who started it.
    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        test_client();
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = crate::ai::signin::start(Plan::Antigravity, 7, None, "Antigravity".into(), 1_000)
            .unwrap()
        else {
            panic!("Antigravity signs in with a browser");
        };
        assert_eq!(redirect_uri, REDIRECT_URI);
        assert!(
            url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
            "{url}"
        );
        assert!(
            url.contains(&format!("client_id={}", config().unwrap().client_id)),
            "{url}"
        );
        assert!(
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A56123%2Foauth-callback"),
            "{url}"
        );
        assert!(url.contains("access_type=offline"), "{url}");
        assert!(url.contains("prompt=consent"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        for scope in [
            "openid",
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform",
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcclog",
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fexperimentsandconfigs",
        ] {
            assert!(url.contains(scope), "missing {scope} in {url}");
        }
        assert!(url.contains(&format!("state={state}")), "{url}");
        // Only the account that started it, and only once.
        assert!(crate::ai::signin::take_pending(&state, 8).is_err());
        let pending = crate::ai::signin::take_pending(&state, 7).unwrap();
        assert_eq!(pending.plan, Plan::Antigravity);
        assert_eq!(pending.name, "Antigravity");
        assert_eq!(pending.state, state);
        // The challenge in the address is the verifier's SHA-256.
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        assert!(
            url.contains(&format!("code_challenge={challenge}")),
            "{url}"
        );
        assert!(crate::ai::signin::take_pending(&state, 7).is_err());
    }

    /// A stored sign-in with no project, or with Google's shared consumer
    /// project, is re-read before the next request; a real project is left
    /// alone.
    #[test]
    fn a_shared_or_missing_project_is_repaired() {
        let credentials = |project: &str| Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            refreshed_at: 0,
            account_id: project.into(),
            email: "someone@example.com".into(),
            plan: String::new(),
            project_id: project.into(),
        };
        assert!(needs_repair(&credentials("aicode-consumers")));
        assert!(needs_repair(&credentials(" AICODE-CONSUMERS ")));
        assert!(needs_repair(&credentials("")));
        assert!(!needs_repair(&credentials("gen-lang-client-123")));
        assert!(!needs_repair(&credentials(
            "00000000-0000-5000-8000-000000000000"
        )));
    }

    /// The client the references pin, and the identity the Cloud Code
    /// handshake presents (the IDE's user agent, not the node client's).
    #[test]
    fn the_client_is_the_antigravity_desktop_one() {
        test_client();
        let config = config().expect("the test client");
        assert_eq!(
            config.client_id,
            std::env::var("RAST_ANTIGRAVITY_CLIENT_ID").unwrap()
        );
        assert_eq!(
            config.client_secret,
            std::env::var("RAST_ANTIGRAVITY_CLIENT_SECRET").unwrap()
        );
        assert_eq!(config.redirect_uri, "http://localhost:56123/oauth-callback");
        assert_eq!(config.scopes.len(), 7);
        assert!(config
            .scopes
            .contains(&"https://www.googleapis.com/auth/aicode"));
        assert!(config
            .scopes
            .contains(&"https://www.googleapis.com/auth/cloud-platform"));
        assert_eq!(config.identity, Identity::Antigravity);
        assert_eq!(config.label, "Antigravity");
    }
}
