//! Kimi sign-in (a device flow): Kimi Code's device authorization (a short
//! code typed on its page) polled for the plan's tokens, which its coding
//! endpoint takes.
//!
//! Kimi ties a sign-in session to a stable device id: the same `X-Msh-*`
//! identity headers must go with the device-code request, every token poll
//! and every later API call, so the id is invented once at the start and
//! kept (in the pending entry while signing in, then in the credentials'
//! account id, where `provider/plans/kimi.rs` reads it from for the wire).
//!
//! One quirk of Kimi's token endpoint: it answers OAuth's "not yet"
//! (`authorization_pending`, `slow_down`) as a 200 with an `error` field
//! rather than a 400, so the poll reads the body before the status.

use serde_json::Value;

use super::{http, pkce, store, Credentials, Pending, Plan, Poll, SignedIn, Started};

/// Kimi Code's public OAuth client (from 9router's registry, CLIProxyAPI
/// parity).
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEVICE_CODE_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
const TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
/// Where a person types `user_code`; Kimi's answer usually names it, this is
/// the fallback 9router uses.
const AUTHORIZE_DEVICE_URL: &str = "https://www.kimi.com/code/authorize_device";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SITE: &str = "Kimi's sign-in";
/// What the `X-Msh-*` identity says RAST is.
const PLATFORM: &str = "RAST";
const DEFAULT_EXPIRES_IN: i64 = 3600;

/// A fresh device id for one sign-in (a UUID, as 9router invents it). It
/// stays the same for the whole session: Kimi remembers who polled which
/// code by it.
fn device_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The `X-Msh-*` identity headers Kimi's OAuth endpoints and API expect.
fn device_headers(id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Msh-Platform", PLATFORM.to_string()),
        ("X-Msh-Version", env!("CARGO_PKG_VERSION").to_string()),
        // Linux servers have no portable hostname in std; a constant is
        // enough (the reference sends the machine's name, which nothing
        // validates).
        ("X-Msh-Device-Name", "rast".to_string()),
        ("X-Msh-Device-Model", device_model()),
        ("X-Msh-Device-Id", id.to_string()),
    ]
}

fn device_model() -> String {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    };
    format!("{os} {}", std::env::consts::ARCH)
}

/// POST a form with the device identity headers, as every Kimi request is
/// made.
fn post(url: &str, id: &str, pairs: &[(&str, &str)]) -> Result<http::Reply, String> {
    let headers = device_headers(id);
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    http::post_form(url, SITE, &borrowed, pairs)
}

/// Start signing in: ask Kimi for a device code, then remember the code and
/// the device id until the person types it on Kimi's page (blocking).
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let id = device_id();
    let reply = post(DEVICE_CODE_URL, &id, &[("client_id", CLIENT_ID)])?;
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
        Plan::Kimi,
        user_id,
        provider_id,
        name,
        state.clone(),
        device_code,
        interval,
        expires_in,
    );
    // The "verifier" slot is free in a device flow without PKCE: it keeps the
    // device id that the polls must present.
    pending.verifier = id;
    store::insert(state.clone(), pending);
    Ok(Started::Device {
        state,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

/// One poll of Kimi's device flow: still waiting, refused, or the tokens
/// (blocking). The pending entry is left alone; `super::poll` consumes it
/// only once tokens arrive.
pub(crate) fn poll_token(pending: &Pending, now_ms: i64) -> Result<Poll, String> {
    let id = pending.verifier.as_str();
    let reply = post(
        TOKEN_URL,
        id,
        &[
            ("grant_type", GRANT_TYPE),
            ("client_id", CLIENT_ID),
            ("device_code", pending.device_code.as_str()),
        ],
    )?;
    match classify_poll(reply.status, &reply.value)? {
        Polled::Waiting => Ok(Poll::Waiting),
        Polled::Failed(message) => Ok(Poll::Failed(message)),
        Polled::Tokens => Ok(Poll::Done(Box::new(SignedIn {
            plan: Plan::Kimi,
            name: pending.name.clone(),
            provider_id: pending.provider_id.clone(),
            credentials: credentials_from(&reply.value, id, None, now_ms)?,
        }))),
    }
}

/// New tokens for old (blocking). The device id stays: Kimi's API calls need
/// it, and the reference refresh sends the same identity headers.
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let id = old.account_id.as_str();
    let pairs = refresh_pairs(old);
    let reply = post(TOKEN_URL, id, &pairs)?;
    if !(200..300).contains(&reply.status) {
        return Err(refused("refreshing the tokens", reply.status, &reply.value));
    }
    if let Some(error) = reply.value.get("error").and_then(Value::as_str) {
        return Err(format!("Kimi's sign-in has ended ({error})"));
    }
    credentials_from(&reply.value, id, Some(old), now_ms)
}

fn refresh_pairs(old: &Credentials) -> Vec<(&'static str, &str)> {
    vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", old.refresh_token.as_str()),
        ("client_id", CLIENT_ID),
    ]
}

/// Kimi's device-authorization answer: the code to poll, the code to show,
/// where to show it and how long it is good for.
fn parse_device_authorization(value: &Value) -> Result<(String, String, String, u64, u64), String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let device_code = text("device_code")
        .ok_or("Kimi's sign-in sent no device code")?
        .to_string();
    let user_code = text("user_code")
        .ok_or("Kimi's sign-in sent no code to enter")?
        .to_string();
    let verification_uri = text("verification_uri")
        .unwrap_or(AUTHORIZE_DEVICE_URL)
        .to_string();
    let interval = number(value, "interval").unwrap_or(5).max(1);
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
    /// Not on Kimi's page yet, or polling too fast.
    Waiting,
    /// The tokens are in the answer.
    Tokens,
    /// Kimi will not complete this sign-in.
    Failed(String),
}

/// Read a token answer: Kimi returns its OAuth "not yet" errors with a 200,
/// so the body is read before the status.
fn classify_poll(status: u16, value: &Value) -> Result<Polled, String> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Ok(match error {
            "authorization_pending" | "slow_down" => Polled::Waiting,
            "expired_token" => {
                Polled::Failed("the Kimi code expired; start the sign-in again".into())
            }
            "access_denied" => Polled::Failed("Kimi did not sign you in: access denied".into()),
            other => Polled::Failed(format!("Kimi did not sign you in: {other}")),
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
        return Err(format!("Kimi's sign-in failed (HTTP {status})"));
    }
    if (400..500).contains(&status) {
        return Ok(Polled::Failed(format!(
            "Kimi did not sign you in (HTTP {status})"
        )));
    }
    // A 200 without tokens: the person has not finished yet.
    Ok(Polled::Waiting)
}

fn credentials_from(
    value: &Value,
    id: &str,
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
        .ok_or("Kimi's sign-in sent no access token")?
        .to_string();
    let refresh_token = text("refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|credentials| credentials.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .ok_or("Kimi's sign-in sent no refresh token")?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN);
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        // The device id Kimi wants with every API call travels as the
        // account id, which `signed_in_token` hands to the wire.
        account_id: id.to_string(),
        email: old
            .map(|credentials| credentials.email.clone())
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
        format!("Kimi's sign-in {doing} was refused (HTTP {status})")
    } else {
        format!("Kimi's sign-in {doing} was refused (HTTP {status}): {said}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_authorization_says_where_to_type_the_code() {
        let value = serde_json::json!({
            "device_code": "dc",
            "user_code": "ABCD-1234",
            "verification_uri": "https://www.kimi.com/code/authorize_device",
            "expires_in": 900,
            "interval": 5
        });
        assert_eq!(
            parse_device_authorization(&value),
            Ok((
                "dc".to_string(),
                "ABCD-1234".to_string(),
                "https://www.kimi.com/code/authorize_device".to_string(),
                5,
                900
            ))
        );
        // Missing optional fields fall back to the reference's values.
        let value = serde_json::json!({"device_code": "dc", "user_code": "u"});
        let (_, _, verification_uri, interval, expires_in) =
            parse_device_authorization(&value).unwrap();
        assert_eq!(verification_uri, AUTHORIZE_DEVICE_URL);
        assert_eq!(interval, 5);
        assert_eq!(expires_in, 900);
        assert!(parse_device_authorization(&serde_json::json!({"user_code": "u"})).is_err());
        assert!(parse_device_authorization(&serde_json::json!({"device_code": "d"})).is_err());
    }

    #[test]
    fn a_poll_is_waiting_refused_or_done() {
        // Kimi answers "not yet" with a 200 and an error field.
        assert!(matches!(
            classify_poll(200, &serde_json::json!({"error": "authorization_pending"})),
            Ok(Polled::Waiting)
        ));
        assert!(matches!(
            classify_poll(200, &serde_json::json!({"error": "slow_down"})),
            Ok(Polled::Waiting)
        ));
        // And a real refusal with a 400.
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "expired_token"})),
            Ok(Polled::Failed(_))
        ));
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "access_denied"})),
            Ok(Polled::Failed(message)) if message.contains("access denied")
        ));
        assert!(matches!(
            classify_poll(400, &serde_json::json!({"error": "invalid_grant"})),
            Ok(Polled::Failed(message)) if message.contains("invalid_grant")
        ));
        // The tokens are in the body.
        assert!(matches!(
            classify_poll(
                200,
                &serde_json::json!({"access_token": "at", "refresh_token": "rt", "expires_in": 3600})
            ),
            Ok(Polled::Tokens)
        ));
        // A server error is a real error, an unexplained 4xx a failure.
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
    fn the_tokens_remember_the_device_id_and_can_be_refreshed() {
        let value = serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "expires_in": 3600,
            "scope": "openid"
        });
        let credentials = credentials_from(&value, "device-1", None, 1_000).expect("credentials");
        assert_eq!(credentials.access_token, "at");
        assert_eq!(credentials.refresh_token, "rt");
        assert_eq!(credentials.account_id, "device-1");
        assert_eq!(credentials.expires_at, 1_000 + 3_600_000);

        // A refresh keeps the device id and the old refresh token when Kimi
        // does not rotate it.
        let again = credentials_from(
            &serde_json::json!({"access_token": "at-2"}),
            "device-1",
            Some(&credentials),
            5_000,
        )
        .expect("credentials");
        assert_eq!(again.access_token, "at-2");
        assert_eq!(again.refresh_token, "rt");
        assert_eq!(again.account_id, "device-1");
        assert!(
            credentials_from(&serde_json::json!({"refresh_token": "rt"}), "d", None, 0).is_err()
        );

        // The refresh form is Kimi's device session, not a new code.
        let pairs = refresh_pairs(&credentials);
        assert!(pairs.contains(&("grant_type", "refresh_token")));
        assert!(pairs.contains(&("refresh_token", "rt")));
        assert!(pairs.contains(&("client_id", CLIENT_ID)));
    }

    #[test]
    fn the_identity_headers_carry_the_device_id() {
        let headers = device_headers("device-1");
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("X-Msh-Device-Id"), Some("device-1"));
        assert_eq!(value("X-Msh-Platform"), Some(PLATFORM));
        assert!(value("X-Msh-Version").is_some_and(|version| !version.is_empty()));
        assert!(value("X-Msh-Device-Model").is_some_and(|model| model.contains(' ')));
        assert!(value("X-Msh-Device-Name").is_some());
        assert_ne!(device_id(), device_id());
        assert_eq!(device_id().len(), 36, "a UUID");
    }
}
