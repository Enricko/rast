//! ChatGPT's Codex: the Codex CLI's public OAuth client, with PKCE, and the
//! token exchange behind the pasted callback. This is the reference
//! implementation every other plan module follows.
//!
//! OpenAI sends the browser back to `http://localhost:1455/auth/callback`
//! (the only address that client may use), which a server elsewhere never
//! receives: the person copies that page's address and pastes it into RAST,
//! which checks the `state` and trades the `code` for tokens.
//!
//! Access tokens last about an hour; RAST refreshes them ahead of time and
//! one refresh at a time, keeping the new refresh token at once (OpenAI
//! rotates refresh tokens and ends the whole session when an old one is used
//! again).

use serde_json::Value;

use super::{http, jwt_claims, pkce, store, Credentials, Pending, Plan, Started};

/// The Codex CLI's OAuth client (public; the same one OpenCode and 9router use).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Where OpenAI sends the browser back (fixed for this client).
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// The Codex CLI version Codex's backend is told (newer models need a
/// recent one), as 9router sends it.
pub const CODEX_VERSION: &str = "0.167.1";

/// Start signing in: the address to open in the browser. `state` names the
/// sign-in; the PKCE verifier stays on the server.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::state();
    let url = format!(
        "{AUTHORIZE_URL}?{}",
        http::form(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "codex_cli_rs"),
            ("state", &state),
        ])
    );
    store::insert(
        state.clone(),
        Pending::browser(
            Plan::Codex,
            user_id,
            provider_id,
            name,
            state.clone(),
            verifier,
            REDIRECT_URI.to_string(),
        ),
    );
    Ok(Started::Browser {
        url,
        state,
        redirect_uri: REDIRECT_URI.to_string(),
    })
}

fn token_request(pairs: &[(&str, &str)]) -> Result<Value, String> {
    let reply = http::post_form(TOKEN_URL, "OpenAI's sign-in", &[], pairs)?;
    if !(200..300).contains(&reply.status) {
        let said = reply.said();
        return Err(if said.is_empty() {
            format!("OpenAI's sign-in refused it (HTTP {})", reply.status)
        } else {
            format!(
                "OpenAI's sign-in refused it (HTTP {}): {said}",
                reply.status
            )
        });
    }
    Ok(reply.value)
}

/// Trade the pasted code for tokens (blocking).
pub(crate) fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    let body = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("redirect_uri", &pending.redirect_uri),
        ("code_verifier", &pending.verifier),
    ])?;
    credentials_from(&body, None, now_ms)
}

/// New tokens for old (blocking). The refresh token changes: keep the new one.
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let body = token_request(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", &old.refresh_token),
        ("scope", SCOPE),
    ])?;
    credentials_from(&body, Some(old), now_ms)
}

fn credentials_from(
    body: &Value,
    old: Option<&Credentials>,
    now_ms: i64,
) -> Result<Credentials, String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let access_token = text("access_token")
        .ok_or("OpenAI's sign-in sent no access token")?
        .to_string();
    let refresh_token = text("refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|o| o.refresh_token.clone()))
        .filter(|t| !t.is_empty())
        .ok_or("OpenAI's sign-in sent no refresh token")?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .unwrap_or(3600);
    // Who it is: the ID token's claims, else the access token's.
    let claims = text("id_token")
        .and_then(jwt_claims)
        .or_else(|| jwt_claims(&access_token))
        .unwrap_or(Value::Null);
    let auth = claims
        .get("https://api.openai.com/auth")
        .cloned()
        .unwrap_or(Value::Null);
    let claim =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let account_id = claim(&auth, "chatgpt_account_id")
        .or_else(|| claim(&claims, "account_id"))
        .or_else(|| old.map(|o| o.account_id.clone()))
        .unwrap_or_default();
    let email = claim(&claims, "email")
        .or_else(|| {
            jwt_claims(&access_token).and_then(|c| {
                claim(
                    &c.get("https://api.openai.com/profile")
                        .cloned()
                        .unwrap_or(Value::Null),
                    "email",
                )
            })
        })
        .or_else(|| old.map(|o| o.email.clone()))
        .unwrap_or_default();
    let plan = claim(&auth, "chatgpt_plan_type")
        .or_else(|| old.map(|o| o.plan.clone()))
        .unwrap_or_default();
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        account_id,
        email,
        plan,
        project_id: old.map(|o| o.project_id.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::signin::{start, take_pending};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = start(Plan::Codex, 7, None, "ChatGPT".into(), 1_000).unwrap()
        else {
            panic!("Codex signs in with a browser");
        };
        assert_eq!(redirect_uri, REDIRECT_URI);
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={state}")));
        // Only the account that started it, and only once.
        assert!(take_pending(&state, 8).is_err());
        let pending = take_pending(&state, 7).unwrap();
        assert_eq!(pending.name, "ChatGPT");
        assert_eq!(pending.plan, Plan::Codex);
        // The challenge in the address is the verifier's SHA-256.
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        assert!(url.contains(&format!("code_challenge={challenge}")));
        assert!(take_pending(&state, 7).is_err());
    }

    #[test]
    fn tokens_say_who_signed_in() {
        let id = jwt(json!({
            "email": "someone@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-1", "chatgpt_plan_type": "plus"}
        }));
        let body = json!({"access_token": "at", "refresh_token": "rt", "id_token": id, "expires_in": 3600});
        let creds = credentials_from(&body, None, 1_000).unwrap();
        assert_eq!(creds.account_id, "acc-1");
        assert_eq!(creds.email, "someone@example.com");
        assert_eq!(creds.plan, "plus");
        assert_eq!(creds.expires_at, 1_000 + 3_600_000);
        assert!(!creds.needs_refresh(1_000));
        assert!(creds.needs_refresh(1_000 + 3_600_000 - 5 * 60 * 1000));
        // A refresh without a new refresh token or ID token keeps the old ones.
        let again = credentials_from(&json!({"access_token": "at2"}), Some(&creds), 5_000).unwrap();
        assert_eq!(
            (again.refresh_token.as_str(), again.account_id.as_str()),
            ("rt", "acc-1")
        );
        assert!(credentials_from(&json!({"refresh_token": "x"}), None, 0).is_err());
        assert!(credentials_from(&json!({"access_token": "x"}), None, 0).is_err());
    }
}
