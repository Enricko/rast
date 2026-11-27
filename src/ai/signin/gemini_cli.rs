//! Gemini CLI sign-in (a browser flow): Google OAuth with PKCE and the
//! pasted loopback callback, then the same Cloud Code handshake Antigravity
//! uses (`loadCodeAssist` / `onboardUser`) with the Gemini CLI's own client,
//! scopes and onboarding identity.
//!
//! Checked against the 2026-11-27 reference
//! (`9router/src/lib/oauth/services/gemini.js` and
//! `9router/open-sse/providers/registry/gemini-cli.js`): that public client,
//! its three scopes and `http://localhost:8085/oauth2callback`, the node
//! client's user agent and `X-Goog-Api-Client` while onboarding, and the
//! production Cloud Code host (the CLI has no daily host).

use super::google::{self, Config, Identity};
use super::{Credentials, Pending, Plan, Started};

/// Where Google sends the browser back; a server elsewhere never receives it,
/// so the person pastes the address.
const REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
/// The app's scopes.
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// The sign-in's own details: the rest is Google OAuth, and the OAuth client
/// (the app's public one, or the operator's own) comes from the server's
/// environment; see [`google::client_from_env`].
fn config() -> Result<Config, String> {
    let (client_id, client_secret) = google::client_from_env("GEMINI_CLI", "Gemini CLI")?;
    Ok(Config {
        client_id,
        client_secret,
        scopes: SCOPES,
        redirect_uri: REDIRECT_URI,
        label: "Gemini CLI",
        app: "the Gemini CLI",
        identity: Identity::GeminiCli,
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
        Plan::GeminiCli,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The real OAuth client comes from the server's environment; the tests
    /// use a fake one so they never need a credential.
    fn test_client() {
        static SET: std::sync::Once = std::sync::Once::new();
        SET.call_once(|| {
            std::env::set_var("RAST_GEMINI_CLI_CLIENT_ID", "test-gemini-client");
            std::env::set_var("RAST_GEMINI_CLI_CLIENT_SECRET", "test-secret");
        });
    }

    /// The sign-in starts with PKCE and the CLI's client, scopes and return
    /// address; the pending entry answers once for the person who started it.
    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        test_client();
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = crate::ai::signin::start(Plan::GeminiCli, 7, None, "Gemini CLI".into(), 1_000).unwrap()
        else {
            panic!("Gemini CLI signs in with a browser");
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
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8085%2Foauth2callback"),
            "{url}"
        );
        assert!(url.contains("access_type=offline"), "{url}");
        assert!(url.contains("prompt=consent"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(
            url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.profile"),
            "{url}"
        );
        assert!(url.contains(&format!("state={state}")), "{url}");
        assert!(crate::ai::signin::take_pending(&state, 8).is_err());
        let pending = crate::ai::signin::take_pending(&state, 7).unwrap();
        assert_eq!(pending.plan, Plan::GeminiCli);
        assert_eq!(pending.name, "Gemini CLI");
        assert_eq!(pending.state, state);
        assert!(crate::ai::signin::take_pending(&state, 7).is_err());
    }

    /// The client the reference pins, and the identity the Cloud Code
    /// handshake presents (the CLI's node client, which asks for `mode: 1`).
    #[test]
    fn the_client_is_the_gemini_cli_one() {
        test_client();
        let config = config().expect("the test client");
        assert_eq!(
            config.client_id,
            std::env::var("RAST_GEMINI_CLI_CLIENT_ID").unwrap()
        );
        assert_eq!(
            config.client_secret,
            std::env::var("RAST_GEMINI_CLI_CLIENT_SECRET").unwrap()
        );
        assert_eq!(config.redirect_uri, "http://localhost:8085/oauth2callback");
        assert_eq!(config.scopes.len(), 3);
        assert!(config
            .scopes
            .contains(&"https://www.googleapis.com/auth/cloud-platform"));
        assert_eq!(config.identity, Identity::GeminiCli);
        assert_eq!(config.label, "Gemini CLI");
    }
}
