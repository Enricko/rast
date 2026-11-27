//! Qwen Chat plan (`qwen-plan`): OpenAI chat completions against
//! `portal.qwen.ai/v1` with the plan's OAuth bearer (the API-key kind `qwen`
//! stays as it is).
//!
//! Qwen's OAuth plan endpoint is plain OpenAI-compatible: the generic bearer
//! is the whole authentication, and no extra identity header is verified in
//! Qwen Code's source (`QwenLM/qwen-code`, `qwenOAuth2.ts` and the DashScope
//! provider). The wire is the generic chat-completions one, so this file only
//! carries the catalog and the plan's own refusal wording; the model list
//! comes from `{base}/models`.
//!
//! Qwen Code also has a newer premium "subscription" flow
//! (`auth.qwen.ai`, client `qwen-code-cli`); that is out of scope for this
//! kind, whose base URL is the classic Qwen Chat portal the tokens in
//! `signin/qwen.rs` are for.

use serde_json::Value;

use super::super::{agent, read_limited, ChatRequest, Turn, Wire, LIST_TIMEOUT};
use crate::ai::Protocol;

/// The most of a model list RAST reads.
const MODEL_LIST_LIMIT: u64 = 4 * 1024 * 1024;
/// Qwen Chat's classic OAuth models, kept when the live list cannot be read
/// (Qwen Code defaults to `coder-model` and offers the vision alias).
const MODELS: &[&str] = &[
    "coder-model",
    "vision-model",
    "qwen3-coder-plus",
    "qwen3-max",
    "qwen3-235b-a22b",
];

/// Qwen Chat speaks chat completions for every model.
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Chat
}

/// Qwen's portal takes the generic bearer and asks for no other identity
/// header that Qwen Code's source shows.
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    _account: &str,
) -> Vec<(&'static str, String)> {
    Vec::new()
}

/// The generic chat-completions body is what the portal takes.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, _body: &mut Value) {}

/// Where the portal's model list is (`{base}/models`).
fn catalog_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

pub(crate) fn catalog(base_url: &str, token: &str, _account: &str) -> Result<Vec<String>, String> {
    if base_url.is_empty() {
        return Err("set Qwen's base URL first".into());
    }
    let response = agent(LIST_TIMEOUT)
        .get(catalog_url(base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("cannot reach Qwen's model list: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), MODEL_LIST_LIMIT);
    if !(200..300).contains(&status) {
        return Err(refusal(status, &body)
            .unwrap_or_else(|| format!("Qwen's model list was refused (HTTP {status})")));
    }
    super::super::parse_models(&body)
}

pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// The portal takes the generic `{base}/chat/completions`.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

pub(crate) fn refusal(status: u16, _body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => "Qwen refused the sign-in; sign in again under AI Agent settings → Providers",
        429 => "your Qwen plan's limit is used up for now",
        _ => return None,
    };
    Some(format!("{what} (HTTP {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_list_is_the_portals() {
        assert_eq!(
            catalog_url("https://portal.qwen.ai/v1"),
            "https://portal.qwen.ai/v1/models"
        );
        assert_eq!(
            catalog_url("https://portal.qwen.ai/v1/"),
            "https://portal.qwen.ai/v1/models"
        );
        assert!(catalog("", "token", "").is_err());
        assert!(!fallback_models().is_empty());
    }

    #[test]
    fn the_plan_keeps_the_generic_wire() {
        assert_eq!(protocol_for("any-model"), Protocol::Chat);
        assert!(extra_headers("m", "s", "a").is_empty());
        assert!(chat_url("https://portal.qwen.ai/v1").is_none());
        assert!(refusal(401, "").is_some_and(|message| message.contains("sign in again")));
        assert!(refusal(429, "").is_some_and(|message| message.contains("limit")));
        assert!(refusal(400, "").is_none());
    }
}
