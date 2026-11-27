//! Signing in to a provider with a person's own plan instead of an API key:
//! ChatGPT's Codex, the way 9router (<https://github.com/decolua/9router>)
//! and OpenCode do it: the Codex CLI's public OAuth client, with PKCE.
//!
//! OpenAI sends the browser back to `http://localhost:1455/auth/callback`
//! (the only address that client may use), which a server elsewhere never
//! receives: the person copies that page's address and pastes it into RAST,
//! which checks the `state` and trades the `code` for tokens.
//!
//! Access tokens last about an hour; RAST refreshes them ahead of time and
//! one refresh at a time, keeping the new refresh token at once (OpenAI
//! rotates refresh tokens and ends the whole session when an old one is used
//! again). Tokens are stored encrypted, never logged or shown.

use std::{
    collections::HashMap,
    io::Read,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The Codex CLI's OAuth client (public; the same one OpenCode and 9router use).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Where OpenAI sends the browser back (fixed for this client).
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// The Codex CLI version Codex's backend is told (newer models need a
/// recent one), as 9router sends it.
pub const CODEX_VERSION: &str = "0.167.1";
/// How long a started sign-in waits for its pasted address.
const PENDING_FOR: Duration = Duration::from_secs(20 * 60);
const MAX_PENDING: usize = 64;
/// Refresh when the access token has less than this left…
const REFRESH_AHEAD_MS: i64 = 10 * 60 * 1000;
/// …or when the last refresh is this old (refresh tokens go stale).
const REFRESH_EVERY_MS: i64 = 8 * 24 * 60 * 60 * 1000;
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A signed-in plan's tokens and who it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since the epoch.
    pub expires_at: i64,
    pub refreshed_at: i64,
    /// ChatGPT's account (workspace) id, sent with every request.
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub email: String,
    /// "plus", "pro", "team", …
    #[serde(default)]
    pub plan: String,
}

impl Credentials {
    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        self.expires_at - now_ms < REFRESH_AHEAD_MS || now_ms - self.refreshed_at > REFRESH_EVERY_MS
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        self.expires_at <= now_ms
    }
}

/// A sign-in started and waiting for its address to be pasted.
#[derive(Debug, Clone)]
pub struct Pending {
    verifier: String,
    pub user_id: i64,
    /// Signing in again to this provider (else a new one).
    pub provider_id: Option<String>,
    pub name: String,
    started: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

fn random_url_safe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// `application/x-www-form-urlencoded` (also the query string).
fn form(pairs: &[(&str, &str)]) -> String {
    fn encode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            ) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Start signing in: the address to open in the browser. `state` names the
/// sign-in; the PKCE verifier stays on the server.
pub fn start(user_id: i64, provider_id: Option<String>, name: String) -> (String, String) {
    let verifier = random_url_safe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_safe(24);
    let url = format!(
        "{AUTHORIZE_URL}?{}",
        form(&[
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
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
    if pending.len() >= MAX_PENDING {
        if let Some(oldest) = pending
            .iter()
            .min_by_key(|(_, p)| p.started)
            .map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
    }
    pending.insert(
        state.clone(),
        Pending {
            verifier,
            user_id,
            provider_id,
            name,
            started: Instant::now(),
        },
    );
    (url, state)
}

/// What the pasted address carries.
#[derive(Debug, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// Read the address the browser ended on (the whole address, or just its
/// `?code=…&state=…` part).
pub fn parse_callback(text: &str) -> Result<Callback, String> {
    let text = text.trim();
    let query = match text.split_once('?') {
        Some((_, query)) => query,
        None if text.contains("code=") => text,
        None => {
            return Err(
                "paste the whole address of the page ChatGPT sent you to (it starts with http://localhost:1455/auth/callback?code=)"
                    .into(),
            )
        }
    };
    let query = query.split('#').next().unwrap_or("");
    let mut fields: HashMap<String, String> = HashMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        fields.insert(decode(key), decode(value));
    }
    if let Some(error) = fields.get("error") {
        let said = fields
            .get("error_description")
            .filter(|d| !d.is_empty())
            .unwrap_or(error);
        return Err(format!("ChatGPT did not sign you in: {said}"));
    }
    match (fields.remove("code"), fields.remove("state")) {
        (Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
            Ok(Callback { code, state })
        }
        _ => Err("that address has no sign-in code; paste the address of the page ChatGPT sent you to after you allowed it".into()),
    }
}

/// The sign-in a pasted address answers (once; only for who started it).
pub fn take_pending(state: &str, user_id: i64) -> Result<Pending, String> {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
    match pending.get(state) {
        Some(p) if p.user_id == user_id => Ok(pending.remove(state).expect("present")),
        _ => Err("this sign-in is not the one started here or it timed out; start again".into()),
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TOKEN_TIMEOUT))
        .user_agent(concat!("RAST/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn token_request(pairs: &[(&str, &str)]) -> Result<Value, String> {
    let response = agent()
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(form(pairs).as_bytes())
        .map_err(|e| format!("cannot reach OpenAI's sign-in: {e}"))?;
    let status = response.status().as_u16();
    let mut text = String::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(256 * 1024)
        .read_to_string(&mut text);
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let said = body
            .get("error_description")
            .or_else(|| body.pointer("/error/message"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(if said.is_empty() {
            format!("OpenAI's sign-in refused it (HTTP {status})")
        } else {
            format!("OpenAI's sign-in refused it (HTTP {status}): {said}")
        });
    }
    Ok(body)
}

/// Trade the pasted code for tokens (blocking).
pub fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    let body = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", &pending.verifier),
    ])?;
    credentials_from(&body, None, now_ms)
}

/// New tokens for old (blocking). The refresh token changes: keep the new one.
pub fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let body = token_request(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", &old.refresh_token),
        ("scope", SCOPE),
    ])?;
    credentials_from(&body, Some(old), now_ms)
}

/// A JWT's claims, unverified: the tokens come straight from OpenAI over
/// TLS, and RAST only reads who signed in from them.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        let (url, state) = start(7, None, "ChatGPT".into());
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={state}")));
        // Only the account that started it, and only once.
        assert!(take_pending(&state, 8).is_err());
        let pending = take_pending(&state, 7).unwrap();
        assert_eq!(pending.name, "ChatGPT");
        // The challenge in the address is the verifier's SHA-256.
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        assert!(url.contains(&format!("code_challenge={challenge}")));
        assert!(take_pending(&state, 7).is_err());
    }

    #[test]
    fn the_pasted_address_is_read() {
        let got = parse_callback(
            " http://localhost:1455/auth/callback?code=ac_1-x%2By&scope=openid&state=abc_D ",
        );
        assert_eq!(
            got,
            Ok(Callback {
                code: "ac_1-x+y".into(),
                state: "abc_D".into()
            })
        );
        assert_eq!(
            parse_callback("code=c1&state=s1"),
            Ok(Callback {
                code: "c1".into(),
                state: "s1".into()
            })
        );
        let denied = parse_callback("http://localhost:1455/auth/callback?error=access_denied&error_description=User+cancelled&state=s");
        assert_eq!(
            denied,
            Err("ChatGPT did not sign you in: User cancelled".into())
        );
        assert!(parse_callback("https://chatgpt.com/").is_err());
        assert!(parse_callback("http://localhost:1455/auth/callback?state=s").is_err());
        assert_eq!(decode("a%2"), "a%2");
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
