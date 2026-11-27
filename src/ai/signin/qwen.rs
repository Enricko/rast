//! Qwen sign-in (a device flow): Qwen Chat's device code (a short code typed
//! on its page) polled for the plan's tokens, which the OpenAI-compatible
//! `portal.qwen.ai/v1` endpoint takes.
//!
//! The constants are Qwen Code's own (verified against
//! `QwenLM/qwen-code`, `packages/core/src/qwen/qwenOAuth2.ts`): the device
//! request and the token polls go to `chat.qwen.ai/api/v1/oauth2/…`, the
//! client id is Qwen Code's public one, and the flow uses PKCE — the verifier
//! is invented at the start and presented with every poll, the challenge went
//! with the device-code request. Qwen answers "not yet" as OAuth's
//! `authorization_pending` (HTTP 400) and "too fast" as `slow_down` (HTTP
//! 429); both mean keep waiting.
//!
//! Qwen Code's repository also has a newer premium "subscription" device
//! flow (`auth.qwen.ai`, client `qwen-code-cli`, scope `model:invoke
//! profile:read`). It is out of scope here: RAST's `qwen-plan` kind talks to
//! `portal.qwen.ai/v1`, which is what the classic Qwen Chat OAuth tokens in
//! this file are for.

use serde_json::Value;

use super::{http, jwt_claims, pkce, store, Credentials, Pending, Plan, Poll, SignedIn, Started};

const DEVICE_CODE_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/device/code";
const TOKEN_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/token";
const CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
const SCOPE: &str = "openid profile email model.completion";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SITE: &str = "Qwen's sign-in";
/// Seconds between polls when Qwen does not say: Qwen Code polls every two
/// seconds.
const DEFAULT_INTERVAL: u64 = 2;
const DEFAULT_EXPIRES_IN: i64 = 3600;

/// Start signing in: ask Qwen for a device code (blocking), then remember the
/// code, the PKCE verifier and how long the code is good for until the
/// person types it on Qwen's page.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    // Qwen Code tags the request so a retry can be told apart; harmless and
    // keeps RAST looking like the CLI.
    let request_id = uuid::Uuid::new_v4().to_string();
    let reply = http::post_form(
        DEVICE_CODE_URL,
        SITE,
        &[("x-request-id", request_id.as_str())],
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ],
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(refused(
            "asking for a device code",
            reply.status,
            &reply.value,
        ));
    }
    let (device_code, user_code, verification_uri, interval, expires_in) =
        parse_device_authorization(&reply.value)?;
    let state = pkce::state();
    let mut pending = Pending::device(
        Plan::Qwen,
        user_id,
        provider_id,
        name,
        state.clone(),
        device_code,
        interval,
        expires_in,
    );
    // Every poll must present the verifier that matches the challenge sent
    // with the device-code request; the pending entry keeps it in the free
    // "verifier" slot.
    pending.verifier = verifier;
    store::insert(state.clone(), pending);
    Ok(Started::Device {
        state,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

/// One poll of Qwen's device flow: still waiting, refused, or the tokens
/// (blocking). The pending entry is left alone; `super::poll` consumes it
/// only once tokens arrive.
pub(crate) fn poll_token(pending: &Pending, now_ms: i64) -> Result<Poll, String> {
    let pairs = poll_pairs(pending);
    let reply = http::post_form(TOKEN_URL, SITE, &[], &pairs)?;
    match classify_poll(reply.status, &reply.value)? {
        Polled::Waiting => Ok(Poll::Waiting),
        Polled::Failed(message) => Ok(Poll::Failed(message)),
        Polled::Tokens => Ok(Poll::Done(Box::new(SignedIn {
            plan: Plan::Qwen,
            name: pending.name.clone(),
            provider_id: pending.provider_id.clone(),
            credentials: credentials_from(&reply.value, None, now_ms)?,
        }))),
    }
}

/// New tokens for old (blocking). Qwen expires refresh tokens too: its
/// endpoint answers 400/401 for one that has ended, which the caller words
/// as "sign in again".
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let pairs = refresh_pairs(old);
    let reply = http::post_form(TOKEN_URL, SITE, &[], &pairs)?;
    if matches!(reply.status, 400 | 401) {
        return Err(
            "Qwen's sign-in has ended; sign in again under AI Agent settings → Providers".into(),
        );
    }
    if !(200..300).contains(&reply.status) {
        return Err(refused("refreshing the tokens", reply.status, &reply.value));
    }
    if let Some(error) = reply.value.get("error").and_then(Value::as_str) {
        return Err(format!("Qwen's sign-in has ended ({error})"));
    }
    credentials_from(&reply.value, Some(old), now_ms)
}

/// The form that asks for a device code.
#[cfg(test)]
fn device_pairs(challenge: &str) -> Vec<(&'static str, &str)> {
    vec![
        ("client_id", CLIENT_ID),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ]
}

/// The form that polls for tokens: the device code plus the PKCE verifier.
fn poll_pairs(pending: &Pending) -> Vec<(&'static str, &str)> {
    vec![
        ("grant_type", GRANT_TYPE),
        ("client_id", CLIENT_ID),
        ("device_code", pending.device_code.as_str()),
        ("code_verifier", pending.verifier.as_str()),
    ]
}

fn refresh_pairs(old: &Credentials) -> Vec<(&'static str, &str)> {
    vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", old.refresh_token.as_str()),
        ("client_id", CLIENT_ID),
    ]
}

/// Qwen's device-authorization answer: the code to poll, the code to show,
/// where to show it and how long both live.
fn parse_device_authorization(value: &Value) -> Result<(String, String, String, u64, u64), String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let device_code = text("device_code")
        .ok_or("Qwen's sign-in sent no device code")?
        .to_string();
    let user_code = text("user_code")
        .ok_or("Qwen's sign-in sent no code to enter")?
        .to_string();
    // The plain page is what the dialog links to and the code is typed on;
    // some answers only fill the complete one.
    let verification_uri = text("verification_uri")
        .or_else(|| text("verification_uri_complete"))
        .ok_or("Qwen's sign-in sent no page to enter the code on")?
        .to_string();
    let interval = number(value, "interval").unwrap_or(DEFAULT_INTERVAL).max(1);
    let expires_in = number(value, "expires_in").unwrap_or(900).max(30);
    Ok((
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
    ))
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).or_else(|| {
        value
            .get(key)
            .and_then(Value::as_i64)
            .filter(|number| *number > 0)
            .map(|number| number as u64)
    })
}

/// What one token poll says.
enum Polled {
    /// Not on Qwen's page yet, or polling too fast (Qwen's 400
    /// `authorization_pending` / 429 `slow_down`).
    Waiting,
    /// The tokens are in the answer.
    Tokens,
    /// Qwen will not complete this sign-in.
    Failed(String),
}

fn classify_poll(status: u16, value: &Value) -> Result<Polled, String> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Ok(match error {
            "authorization_pending" | "slow_down" => Polled::Waiting,
            "expired_token" => {
                Polled::Failed("the Qwen code expired; start the sign-in again".into())
            }
            "access_denied" => Polled::Failed("Qwen did not sign you in: access denied".into()),
            other => Polled::Failed(format!("Qwen did not sign you in: {other}")),
        });
    }
    if value
        .get("access_token")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
    {
        return Ok(Polled::Tokens);
    }
    if (500..600).contains(&status) {
        return Err(format!("Qwen's sign-in failed (HTTP {status})"));
    }
    if (400..500).contains(&status) {
        return Ok(Polled::Failed(format!(
            "Qwen did not sign you in (HTTP {status})"
        )));
    }
    Ok(Polled::Waiting)
}

fn credentials_from(
    value: &Value,
    old: Option<&Credentials>,
    now_ms: i64,
) -> Result<Credentials, String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let access_token = text("access_token")
        .ok_or("Qwen's sign-in sent no access token")?
        .to_string();
    let refresh_token = text("refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|credentials| credentials.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .ok_or("Qwen's sign-in sent no refresh token")?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN);
    // Who it is: Qwen's tokens carry the person's email as JWT claims; the
    // ID token when there is one, else the access token.
    let claims = text("id_token")
        .and_then(jwt_claims)
        .or_else(|| jwt_claims(&access_token))
        .unwrap_or(Value::Null);
    let claim = |key: &str| {
        claims
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        account_id: String::new(),
        email: claim("email")
            .or_else(|| claim("preferred_username"))
            .or_else(|| old.map(|credentials| credentials.email.clone()))
            .unwrap_or_default(),
        plan: old
            .map(|credentials| credentials.plan.clone())
            .unwrap_or_default(),
        project_id: old
            .map(|credentials| credentials.project_id.clone())
            .unwrap_or_default(),
    })
}

fn refused(doing: &str, status: u16, value: &Value) -> String {
    let said = value
        .get("error_description")
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if said.is_empty() {
        format!("Qwen's sign-in {doing} was refused (HTTP {status})")
    } else {
        format!("Qwen's sign-in {doing} was refused (HTTP {status}): {said}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn the_device_authorization_says_where_to_type_the_code() {
        let value = serde_json::json!({
            "device_code": "dc",
            "user_code": "QWEN-1234",
            "verification_uri": "https://chat.qwen.ai/device",
            "verification_uri_complete": "https://chat.qwen.ai/device?user_code=QWEN-1234",
            "expires_in": 1800
        });
        assert_eq!(
            parse_device_authorization(&value),
            Ok((
                "dc".to_string(),
                "QWEN-1234".to_string(),
                "https://chat.qwen.ai/device".to_string(),
                2,
                1800
            ))
        );
        // The complete address stands in when the plain one is missing, and
        // an interval Qwen may start sending is used.
        let value = serde_json::json!({
            "device_code": "dc",
            "user_code": "u",
            "verification_uri_complete": "https://chat.qwen.ai/device?user_code=u",
            "interval": 4
        });
        let (_, _, verification_uri, interval, expires_in) =
            parse_device_authorization(&value).unwrap();
        assert_eq!(verification_uri, "https://chat.qwen.ai/device?user_code=u");
        assert_eq!(interval, 4);
        assert_eq!(expires_in, 900);
        assert!(parse_device_authorization(
            &serde_json::json!({"device_code": "d", "user_code": "u"})
        )
        .is_err());
    }

    #[test]
    fn the_device_request_uses_pkce_and_the_clients_scope() {
        let pairs = device_pairs("the-challenge");
        assert!(pairs.contains(&("client_id", CLIENT_ID)));
        assert!(pairs.contains(&("scope", SCOPE)));
        assert!(pairs.contains(&("code_challenge", "the-challenge")));
        assert!(pairs.contains(&("code_challenge_method", "S256")));
    }

    #[test]
    fn a_poll_sends_the_device_code_and_its_verifier() {
        let pending = Pending::device(
            Plan::Qwen,
            7,
            None,
            "Qwen".into(),
            "the-state".into(),
            "dc".into(),
            2,
            900,
        );
        // A device pending has an empty verifier until the flow sets it; the
        // poll always presents whatever the flow stored.
        let pairs = poll_pairs(&pending);
        assert!(pairs.contains(&("grant_type", GRANT_TYPE)));
        assert!(pairs.contains(&("client_id", CLIENT_ID)));
        assert!(pairs.contains(&("device_code", "dc")));
        assert!(pairs.contains(&("code_verifier", "")));
    }

    #[test]
    fn a_poll_is_waiting_refused_or_done() {
        // Qwen's "not yet": a 400 with authorization_pending.
        assert!(matches!(
            classify_poll(
                400,
                &serde_json::json!({"error": "authorization_pending", "error_description": "wait"})
            ),
            Ok(Polled::Waiting)
        ));
        // Its "too fast": a 429 with slow_down.
        assert!(matches!(
            classify_poll(429, &serde_json::json!({"error": "slow_down"})),
            Ok(Polled::Waiting)
        ));
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "expired_token"})),
            Ok(Polled::Failed(message)) if message.contains("expired")
        ));
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "access_denied"})),
            Ok(Polled::Failed(message)) if message.contains("access denied")
        ));
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "invalid_grant"})),
            Ok(Polled::Failed(message)) if message.contains("invalid_grant")
        ));
        assert!(matches!(
            classify_poll(
                200,
                &serde_json::json!({"access_token": "at", "refresh_token": "rt", "expires_in": 3600})
            ),
            Ok(Polled::Tokens)
        ));
        assert!(classify_poll(500, &serde_json::json!({})).is_err());
        assert!(matches!(
            classify_poll(400, &serde_json::json!({})),
            Ok(Polled::Failed(_))
        ));
        assert!(matches!(
            classify_poll(200, &serde_json::json!({})),
            Ok(Polled::Waiting)
        ));
    }

    #[test]
    fn the_tokens_say_who_signed_in() {
        let id = jwt(serde_json::json!({"email": "someone@example.com"}));
        let value = serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id,
            "expires_in": 3600
        });
        let credentials = credentials_from(&value, None, 1_000).expect("credentials");
        assert_eq!(credentials.email, "someone@example.com");
        assert_eq!(credentials.expires_at, 1_000 + 3_600_000);
        assert_eq!(credentials.account_id, "");

        // The access token's claims name the person when there is no ID
        // token; a refresh without one keeps the old email.
        let access = jwt(serde_json::json!({"preferred_username": "someone"}));
        let value = serde_json::json!({"access_token": access, "refresh_token": "rt"});
        let credentials = credentials_from(&value, None, 1_000).expect("credentials");
        assert_eq!(credentials.email, "someone");
        let again = credentials_from(
            &serde_json::json!({"access_token": "at-2"}),
            Some(&credentials),
            5_000,
        )
        .expect("credentials");
        assert_eq!(again.email, "someone");
        assert_eq!(again.refresh_token, "rt");
        assert!(credentials_from(&serde_json::json!({"refresh_token": "rt"}), None, 0).is_err());
    }

    #[test]
    fn a_refresh_asks_for_new_tokens_for_the_client() {
        let old = Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            refreshed_at: 0,
            account_id: String::new(),
            email: String::new(),
            plan: String::new(),
            project_id: String::new(),
        };
        let pairs = refresh_pairs(&old);
        assert!(pairs.contains(&("grant_type", "refresh_token")));
        assert!(pairs.contains(&("refresh_token", "rt")));
        assert!(pairs.contains(&("client_id", CLIENT_ID)));
    }
}
