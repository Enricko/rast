//! Kimi plan (`kimi-plan`): OpenAI chat completions against Kimi Code's
//! endpoint with the plan's OAuth bearer.
//!
//! Kimi Code ties an OAuth session to a stable device id: every request must
//! carry the same `X-Msh-*` identity headers that the device-code request
//! and token polls carried (9router's registry and `buildKimiHeaders`). The
//! sign-in stores the device id as the credentials' account id, which
//! `signed_in_token` passes down here as `account`.
//!
//! The wire itself is the generic chat-completions one; the kind's base URL
//! already ends in `/v1`, so no separate generation URL is needed. The
//! fallback model list is 9router's registry list (Kimi Code serves those ids
//! on its coding endpoint).

use serde_json::Value;

use super::super::{agent, read_limited, ChatRequest, Turn, Wire, LIST_TIMEOUT};
use crate::ai::Protocol;

/// What the `X-Msh-*` identity says RAST is (Kimi's reference sends its own
/// client name; the value is informational).
const PLATFORM: &str = "RAST";
/// The most of a model list RAST reads.
const MODEL_LIST_LIMIT: u64 = 4 * 1024 * 1024;
/// Kimi Code's models, from 9router's registry, kept when the live list
/// cannot be read.
const MODELS: &[&str] = &[
    "kimi-k3",
    "k3",
    "k3.1-pro",
    "k3.1-pro-highspeed",
    "kimi-for-coding",
    "kimi-for-coding-highspeed",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
    "kimi-k2.5",
    "kimi-k2.5-thinking",
    "kimi-latest",
];

/// Kimi Code speaks chat completions for every model.
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Chat
}

/// The `X-Msh-*` session identity Kimi's API expects (the device id is the
/// account id the sign-in stored).
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    if account.is_empty() {
        // Nothing to identify the Kimi Code session by; the generic bearer
        // is all there is.
        return Vec::new();
    }
    vec![
        ("X-Msh-Platform", PLATFORM.to_string()),
        ("X-Msh-Version", env!("CARGO_PKG_VERSION").to_string()),
        ("X-Msh-Device-Name", "rast".to_string()),
        ("X-Msh-Device-Model", device_model()),
        ("X-Msh-Device-Id", account.to_string()),
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

/// The generic chat-completions body is what Kimi Code takes.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, _body: &mut Value) {}

/// Where Kimi Code's model list would be (`{base}/models`); the reference
/// has no live catalog (its registry carries the list), so a refusal simply
/// falls back to [`fallback_models`].
fn catalog_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

pub(crate) fn catalog(base_url: &str, token: &str, account: &str) -> Result<Vec<String>, String> {
    if base_url.is_empty() {
        return Err("set Kimi's base URL first".into());
    }
    let mut request = agent(LIST_TIMEOUT)
        .get(catalog_url(base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    for (name, value) in extra_headers("", "", account) {
        request = request.header(name, value);
    }
    let response = request
        .call()
        .map_err(|e| format!("cannot reach Kimi's model list: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), MODEL_LIST_LIMIT);
    if !(200..300).contains(&status) {
        return Err(refusal(status, &body)
            .unwrap_or_else(|| format!("Kimi's model list was refused (HTTP {status})")));
    }
    super::super::parse_models(&body)
}

pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// Kimi Code takes the generic `{base}/chat/completions`.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

pub(crate) fn refusal(status: u16, _body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => "Kimi refused the sign-in; sign in again under AI Agent settings → Providers",
        429 => "your Kimi plan's limit is used up for now",
        _ => return None,
    };
    Some(format!("{what} (HTTP {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_carries_the_sessions_device_identity() {
        let headers = extra_headers("any-model", "conv-1", "device-1");
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("X-Msh-Platform"), Some(PLATFORM));
        assert_eq!(value("X-Msh-Device-Id"), Some("device-1"));
        assert!(value("X-Msh-Version").is_some_and(|version| !version.is_empty()));
        assert!(value("X-Msh-Device-Model").is_some_and(|model| model.contains(' ')));
        assert!(value("X-Msh-Device-Name").is_some());
        // Without a device id there is no Kimi Code identity to send.
        assert!(extra_headers("m", "s", "").is_empty());
    }

    #[test]
    fn the_model_list_falls_back_to_the_registrys_models() {
        assert_eq!(
            catalog_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding/v1/models"
        );
        assert_eq!(
            catalog_url("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1/models"
        );
        assert!(catalog("", "token", "device").is_err());
        assert!(!fallback_models().is_empty());
        assert!(fallback_models().contains(&"kimi-for-coding"));
    }

    #[test]
    fn the_plan_keeps_the_generic_wire() {
        assert_eq!(protocol_for("any-model"), Protocol::Chat);
        assert!(chat_url("https://api.kimi.com/coding/v1").is_none());
        assert!(refusal(401, "").is_some_and(|message| message.contains("sign in again")));
        assert!(refusal(429, "").is_some_and(|message| message.contains("limit")));
        assert!(refusal(400, "").is_none());
    }
}
