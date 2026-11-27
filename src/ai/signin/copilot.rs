//! GitHub Copilot sign-in (a device flow): GitHub's device code (a short
//! code typed on github.com's page) polled for a GitHub token, which is then
//! exchanged for the short-lived Copilot API token the editor endpoints take.
//!
//! GitHub cannot redirect to a server elsewhere, so it shows a code instead:
//! the person types it at `https://github.com/login/device`, RAST polls
//! GitHub's token endpoint every few seconds (the interval GitHub asked for),
//! and the moment the code is approved RAST exchanges the GitHub token at
//! `https://api.github.com/copilot_internal/v2/token`. Two token generations
//! are in play, exactly as the reference plugin keeps them: the long-lived
//! GitHub OAuth token (stored as the refresh token) and the ~30-minute Copilot
//! API token (the access token). A refresh is a fresh exchange of the same
//! GitHub token.
//!
//! The GitHub login and email are read once at sign-in for the settings page
//! (best effort: a profile lookup failure must not fail the sign-in).

use serde_json::Value;

use super::{http, pkce, store, Credentials, Pending, Plan, Poll, SignedIn, Started};

/// The VS Code Copilot Chat GitHub App's client id (public; the reference
/// plugin and 9router use it; the app is pre-authorized for the Copilot
/// token exchange, a self-registered OAuth app is not).
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const USER_URL: &str = "https://api.github.com/user";
const SCOPE: &str = "read:user";

/// GitHub's defaults when the device-code answer leaves them out (GitHub's
/// own device codes live 15 minutes).
const DEFAULT_INTERVAL_SECS: u64 = 5;
const DEFAULT_EXPIRES_IN_SECS: u64 = 900;
/// The Copilot token usually lives ~30 minutes; when GitHub sends no expiry,
/// only 25 are assumed (the reference's conservative fallback).
const ASSUMED_TOKEN_MS: i64 = 25 * 60 * 1000;

/// The editor identity every Copilot endpoint wants (the Copilot API rejects
/// traffic without one). Versions are the reference plugin's pinned fallback:
/// they are not fetched at runtime.
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const API_VERSION: &str = "2026-06-01";

/// Start signing in: ask GitHub for a device code, remember it under the
/// sign-in's `state` until a poll finishes, and show the code to type.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let reply = http::post_form(
        DEVICE_CODE_URL,
        "GitHub's sign-in",
        &[],
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )?;
    if !(200..300).contains(&reply.status) {
        let said = reply.said();
        return Err(if said.is_empty() {
            format!(
                "GitHub did not give RAST a device code (HTTP {})",
                reply.status
            )
        } else {
            format!(
                "GitHub did not give RAST a device code (HTTP {}): {said}",
                reply.status
            )
        });
    }
    let device = parse_device_code(&reply.value)?;
    let state = pkce::state();
    store::insert(
        state.clone(),
        Pending::device(
            Plan::Copilot,
            user_id,
            provider_id,
            name,
            state.clone(),
            device.device_code,
            device.interval,
            device.expires_in,
        ),
    );
    Ok(Started::Device {
        state,
        user_code: device.user_code,
        verification_uri: device.verification_uri,
        interval: device.interval,
        expires_in: device.expires_in,
    })
}

/// What GitHub's device-code answer describes.
#[derive(Debug, PartialEq, Eq)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

/// Read one device-code answer; a missing piece is GitHub's error, not a
/// sign-in RAST can show.
fn parse_device_code(value: &Value) -> Result<DeviceCode, String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let device_code = text("device_code").ok_or("GitHub's sign-in sent no device code")?;
    let user_code = text("user_code").ok_or("GitHub's sign-in sent no code to type")?;
    let verification_uri =
        text("verification_uri").ok_or("GitHub's sign-in sent no page to type the code on")?;
    let interval = value
        .get("interval")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS);
    Ok(DeviceCode {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

/// What one poll of GitHub's token endpoint answered.
enum PollReply {
    /// Not approved yet (or GitHub asked for a slower pace).
    Waiting,
    /// A GitHub OAuth token.
    Token(String),
    /// GitHub will not release one (expired code, denial, …).
    Failed(String),
}

/// Read one poll answer: `authorization_pending`/`slow_down` wait,
/// `expired_token`/`access_denied` fail with clear wording, anything else
/// with GitHub's own explanation.
fn poll_reply(reply: &http::Reply) -> PollReply {
    if let Some(token) = reply
        .value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    {
        return PollReply::Token(token.to_string());
    }
    let error = reply
        .value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("");
    let said = reply
        .value
        .get("error_description")
        .and_then(Value::as_str)
        .filter(|said| !said.is_empty())
        .unwrap_or(error);
    match error {
        // RFC 8628: keep waiting; `slow_down` asks for a slower pace, which
        // the sign-in manager enforces as a minimum interval already.
        "authorization_pending" | "slow_down" => PollReply::Waiting,
        "expired_token" => PollReply::Failed(
            "GitHub's sign-in code expired before it was approved; start again".into(),
        ),
        "access_denied" => PollReply::Failed("GitHub declined the sign-in; start again".into()),
        "" if (200..300).contains(&reply.status) => {
            PollReply::Failed("GitHub's sign-in did not answer with a token; start again".into())
        }
        _ => PollReply::Failed(if said.is_empty() {
            format!(
                "GitHub's sign-in could not be checked (HTTP {})",
                reply.status
            )
        } else {
            format!("GitHub's sign-in failed: {said}")
        }),
    }
}

/// The token form one poll POSTs.
fn poll_form(device_code: &str) -> [(&'static str, &str); 3] {
    [
        ("client_id", CLIENT_ID),
        ("device_code", device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ]
}

/// One poll of GitHub's device flow. Waiting until the person approves the
/// code; then the GitHub token is exchanged for the Copilot token and the
/// sign-in is done.
pub(crate) fn poll_token(pending: &Pending, now_ms: i64) -> Result<Poll, String> {
    let reply = http::post_form(
        ACCESS_TOKEN_URL,
        "GitHub's sign-in",
        &[],
        &poll_form(&pending.device_code),
    )?;
    let github_token = match poll_reply(&reply) {
        PollReply::Waiting => return Ok(Poll::Waiting),
        PollReply::Failed(message) => return Ok(Poll::Failed(message)),
        PollReply::Token(token) => token,
    };
    let (copilot_token, expires_at) = exchange_copilot_token(&github_token, now_ms)?;
    // Who it is: best effort, as the reference reads it once at sign-in.
    let (login, email) = github_profile(&github_token);
    Ok(Poll::Done(Box::new(SignedIn {
        plan: Plan::Copilot,
        name: pending.name.clone(),
        provider_id: pending.provider_id.clone(),
        credentials: Credentials {
            access_token: copilot_token,
            refresh_token: github_token,
            expires_at,
            refreshed_at: now_ms,
            account_id: login,
            email,
            // The Copilot endpoints say nothing about the plan.
            plan: String::new(),
            project_id: String::new(),
        },
    })))
}

/// New tokens for old (blocking): the long-lived GitHub token is exchanged
/// again for a fresh Copilot token (which is what expires).
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let (copilot_token, expires_at) = exchange_copilot_token(&old.refresh_token, now_ms)?;
    Ok(Credentials {
        access_token: copilot_token,
        refresh_token: old.refresh_token.clone(),
        expires_at,
        refreshed_at: now_ms,
        account_id: old.account_id.clone(),
        email: old.email.clone(),
        plan: old.plan.clone(),
        project_id: old.project_id.clone(),
    })
}

/// The editor identity the Copilot API demands with every request.
fn editor_headers() -> Vec<(&'static str, String)> {
    vec![
        ("user-agent", USER_AGENT.to_string()),
        ("editor-version", EDITOR_VERSION.to_string()),
        ("editor-plugin-version", EDITOR_PLUGIN_VERSION.to_string()),
        ("copilot-integration-id", "vscode-chat".to_string()),
        ("openai-intent", "conversation-edits".to_string()),
        ("x-github-api-version", API_VERSION.to_string()),
    ]
}

/// Exchange a GitHub token for the Copilot API token: its value and expiry
/// (milliseconds since the epoch).
fn exchange_copilot_token(github_token: &str, now_ms: i64) -> Result<(String, i64), String> {
    let authorization = format!("Bearer {github_token}");
    let identity = editor_headers();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", authorization.as_str()),
        ("Accept", "application/json"),
    ];
    headers.extend(identity.iter().map(|(name, value)| (*name, value.as_str())));
    let reply = http::get_json(COPILOT_TOKEN_URL, "GitHub Copilot", &headers)?;
    exchange_tokens(&reply, now_ms)
}

/// Read the Copilot token exchange's answer, or word its refusal (a 401/403
/// means the GitHub token is gone or the account has no Copilot: sign in
/// again).
fn exchange_tokens(reply: &http::Reply, now_ms: i64) -> Result<(String, i64), String> {
    if !(200..300).contains(&reply.status) {
        return Err(copilot_refusal(reply.status, reply_said(reply)));
    }
    let token = reply
        .value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("GitHub Copilot's sign-in sent no token")?
        .to_string();
    let expires_at = reply
        .value
        .get("expires_at")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds * 1000)
        .unwrap_or(now_ms + ASSUMED_TOKEN_MS);
    Ok((token, expires_at))
}

/// GitHub's own explanation of a refusal: an OAuth error, else GitHub API's
/// top-level `message` (which the shared [`http::Reply::said`] does not read).
fn reply_said(reply: &http::Reply) -> &str {
    let said = reply.said();
    if !said.is_empty() {
        return said;
    }
    reply
        .value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// What GitHub Copilot said when it refused the exchange.
fn copilot_refusal(status: u16, said: &str) -> String {
    let what = match status {
        401 | 403 => {
            "GitHub Copilot's sign-in has ended; sign in again under AI Agent settings → Providers"
        }
        _ => "GitHub Copilot refused the sign-in",
    }
    .to_string();
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

/// The GitHub login and email behind the token, for the settings page.
/// Best effort: any failure leaves them empty and the sign-in stands.
fn github_profile(github_token: &str) -> (String, String) {
    let found = |reply: &http::Reply, key: &str| {
        reply
            .value
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let authorization = format!("Bearer {github_token}");
    let profile = http::get_json(
        USER_URL,
        "GitHub",
        &[
            ("Authorization", authorization.as_str()),
            ("Accept", "application/json"),
        ],
    );
    let Ok(reply) = profile else {
        return (String::new(), String::new());
    };
    if !(200..300).contains(&reply.status) {
        return (String::new(), String::new());
    }
    let login = found(&reply, "login");
    let email = found(&reply, "email");
    (login, email)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(status: u16, value: Value) -> http::Reply {
        http::Reply { status, value }
    }

    /// GitHub's device-code answer is read into the four facts the dialog
    /// shows, with GitHub's defaults for a missing interval or lifetime.
    #[test]
    fn the_device_code_is_read() {
        let value = json!({
            "device_code": "dc-1",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5,
        });
        assert_eq!(
            parse_device_code(&value).unwrap(),
            DeviceCode {
                device_code: "dc-1".into(),
                user_code: "ABCD-1234".into(),
                verification_uri: "https://github.com/login/device".into(),
                interval: 5,
                expires_in: 900,
            }
        );
        // Missing interval/lifetime: GitHub's defaults, never zero.
        let value = json!({
            "device_code": "dc-2",
            "user_code": "EFGH-5678",
            "verification_uri": "https://github.com/login/device",
        });
        let device = parse_device_code(&value).unwrap();
        assert_eq!(device.interval, DEFAULT_INTERVAL_SECS);
        assert_eq!(device.expires_in, DEFAULT_EXPIRES_IN_SECS);
        // A missing code or page is an error the person can understand.
        assert_eq!(
            parse_device_code(&json!({"user_code": "x", "verification_uri": "y"})).unwrap_err(),
            "GitHub's sign-in sent no device code"
        );
        assert_eq!(
            parse_device_code(&json!({"device_code": "d", "verification_uri": "y"})).unwrap_err(),
            "GitHub's sign-in sent no code to type"
        );
        assert_eq!(
            parse_device_code(&json!({"device_code": "d", "user_code": "x"})).unwrap_err(),
            "GitHub's sign-in sent no page to type the code on"
        );
    }

    /// The poll POSTs the RFC 8628 grant for this client and code.
    #[test]
    fn the_poll_form_is_the_device_grant() {
        assert_eq!(
            poll_form("dc-1"),
            [
                ("client_id", CLIENT_ID),
                ("device_code", "dc-1"),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]
        );
    }

    /// A poll's answer: pending waits, a token finishes, an expired code or a
    /// denial fails with clear wording (and never consumes the pending).
    #[test]
    fn a_poll_is_read() {
        assert!(matches!(
            poll_reply(&reply(200, json!({"error": "authorization_pending"}))),
            PollReply::Waiting
        ));
        assert!(matches!(
            poll_reply(&reply(200, json!({"error": "slow_down"}))),
            PollReply::Waiting
        ));
        match poll_reply(&reply(200, json!({"access_token": "gh-token"}))) {
            PollReply::Token(token) => assert_eq!(token, "gh-token"),
            _ => panic!("a token answer is a token"),
        }
        match poll_reply(&reply(200, json!({"error": "expired_token"}))) {
            PollReply::Failed(message) => assert!(message.contains("expired"), "{message}"),
            _ => panic!("an expired code fails"),
        }
        match poll_reply(&reply(200, json!({"error": "access_denied"}))) {
            PollReply::Failed(message) => assert!(message.contains("declined"), "{message}"),
            _ => panic!("a denial fails"),
        }
        // Other errors keep GitHub's own explanation.
        match poll_reply(&reply(
            400,
            json!({"error": "bad_verification_code", "error_description": "The device code is invalid"}),
        )) {
            PollReply::Failed(message) => assert!(message.contains("The device code is invalid")),
            _ => panic!("an unknown error fails"),
        }
        match poll_reply(&reply(502, json!({}))) {
            PollReply::Failed(message) => assert!(message.contains("HTTP 502"), "{message}"),
            _ => panic!("a server error fails"),
        }
        assert!(matches!(
            poll_reply(&reply(200, json!({}))),
            PollReply::Failed(_)
        ));
    }

    /// The Copilot token exchange maps its answer: the token, its expiry in
    /// milliseconds, and — the first time — GitHub's own explanation is
    /// kept; a 401/403 means sign in again.
    #[test]
    fn the_copilot_token_exchange_is_read() {
        let (token, expires_at) = exchange_tokens(
            &reply(
                200,
                json!({"token": "copilot-token", "expires_at": 2_000_000_000}),
            ),
            1_000,
        )
        .unwrap();
        assert_eq!(token, "copilot-token");
        assert_eq!(expires_at, 2_000_000_000_000);
        // No expiry: a conservative 25 minutes.
        let (_, expires_at) =
            exchange_tokens(&reply(200, json!({"token": "copilot-token"})), 1_000).unwrap();
        assert_eq!(expires_at, 1_000 + ASSUMED_TOKEN_MS);
        assert_eq!(
            exchange_tokens(&reply(200, json!({"expires_at": 5})), 0).unwrap_err(),
            "GitHub Copilot's sign-in sent no token"
        );
        let denied =
            exchange_tokens(&reply(401, json!({"message": "Bad credentials"})), 0).unwrap_err();
        assert!(denied.contains("sign in again"), "{denied}");
        assert!(denied.contains("Bad credentials"), "{denied}");
        let failed = exchange_tokens(&reply(500, json!({})), 0).unwrap_err();
        assert!(failed.contains("HTTP 500"), "{failed}");
    }

    /// The Copilot endpoints are presented to as the VS Code Copilot Chat
    /// extension; the same identity goes on the model list.
    #[test]
    fn the_editor_identity_is_sent() {
        assert_eq!(
            editor_headers(),
            vec![
                ("user-agent", "GitHubCopilotChat/0.35.0".to_string()),
                ("editor-version", "vscode/1.107.0".to_string()),
                ("editor-plugin-version", "copilot-chat/0.35.0".to_string()),
                ("copilot-integration-id", "vscode-chat".to_string()),
                ("openai-intent", "conversation-edits".to_string()),
                ("x-github-api-version", "2026-06-01".to_string()),
            ]
        );
    }
}
