//! Gemini CLI (`gemini-cli`): Cloud Code `v1internal` (see
//! [`super::super::cloud_code`]), sharing that wire module with
//! Antigravity. This file is the Gemini CLI's identity: its node client's
//! user agent and `X-Goog-Api-Client`, the production host, the catalog and
//! its refusal wording.
//!
//! Checked against the 2026-11-27 reference
//! (`9router/open-sse/providers/registry/gemini-cli.js`, its executor and
//! `open-sse/translator/request/openai-to-gemini.js`): the CLI talks to
//! `cloudcode-pa.googleapis.com`, wraps the Gemini request in
//! `{project, requestId, model, userAgent: "gemini-cli", request}` and names
//! itself `GeminiCLI/<version>/<model> (<os>; <arch>; terminal)` with the
//! SDK's `X-Goog-Api-Client`. Unlike Antigravity's IDE it has neither a daily
//! host nor the `requestType` bucket.

use serde_json::Value;

use super::super::cloud_code;
use super::super::{refusal as generic_refusal, ChatRequest, Turn, Wire};
use crate::ai::Protocol;

/// Models kept when the live catalog cannot be read (the CLI's own list in
/// the 9router registry; the live catalog replaces it at sign-in).
const MODELS: &[&str] = &[
    "gemini-3.1-pro-preview",
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

/// The configured origin with a trailing slash removed (the kind's default is
/// already the CLI's production host).
fn chat_base(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

// ------------------------------------------------------------- plan hooks

/// The protocol this plan speaks for `model` (fixed: Cloud Code).
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::CloudCode
}

/// The Gemini CLI's identity: its user agent names the model, and the SDK
/// marker plus the Cloud Code project travel with every request.
pub(crate) fn extra_headers(
    model: &str,
    _session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("User-Agent", cloud_code::gemini_cli_user_agent(model)),
        (
            "X-Goog-Api-Client",
            cloud_code::GEMINI_CLI_API_CLIENT.to_string(),
        ),
    ];
    if !account.is_empty() {
        out.push(("x-goog-user-project", account.to_string()));
    }
    out
}

/// The envelope is [`cloud_code::body`]'s.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, _body: &mut Value) {}

/// `POST {base}/v1internal:fetchAvailableModels {project}` with the bearer.
pub(crate) fn catalog(base_url: &str, token: &str, account: &str) -> Result<Vec<String>, String> {
    cloud_code::fetch_available_models(
        &chat_base(base_url),
        token,
        account,
        &extra_headers("", "", account),
    )
}

pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// `{base}/v1internal:streamGenerateContent?alt=sse`.
pub(crate) fn chat_url(base_url: &str) -> Option<String> {
    Some(format!(
        "{}/v1internal:streamGenerateContent?alt=sse",
        chat_base(base_url)
    ))
}

/// What Google said when it refused, in the CLI's own terms. A rejected
/// sign-in is the common failure; then the plan limit; then a project that
/// was never onboarded.
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => {
            "Google refused the Gemini CLI sign-in; sign in again under AI Agent settings → Providers"
                .to_string()
        }
        429 => "your Gemini CLI plan's limit is used up for now; try again later".to_string(),
        400 => "Google refused the request; the Cloud Code project may not be onboarded, so sign in again under AI Agent settings → Providers".to_string(),
        // Anything else is not about the sign-in: the generic refusal says it.
        _ => return None,
    };
    let said = generic_refusal(status, body);
    let said = said.split_once("): ").map(|(_, said)| said).unwrap_or("");
    Some(if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan addresses the CLI's production host and respects a configured
    /// origin, without doubling a trailing slash.
    #[test]
    fn the_plan_addresses_use_the_cli_host() {
        assert_eq!(
            chat_url("https://cloudcode-pa.googleapis.com").as_deref(),
            Some("https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            chat_url("https://cloudcode-pa.googleapis.com/").as_deref(),
            Some("https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            chat_url("https://proxy.example").as_deref(),
            Some("https://proxy.example/v1internal:streamGenerateContent?alt=sse")
        );
    }

    /// The wire presents as the Gemini CLI: its user agent names the model,
    /// the SDK marker and the Cloud Code project travel along.
    #[test]
    fn the_wire_presents_as_the_cli() {
        let headers = extra_headers("gemini-3-pro-preview", "conv-1", "project-1");
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let user_agent = find("User-Agent").expect("a user agent");
        assert!(
            user_agent.starts_with("GeminiCLI/0.34.0/gemini-3-pro-preview ("),
            "{user_agent}"
        );
        assert!(user_agent.ends_with("; terminal)"), "{user_agent}");
        assert_eq!(
            find("X-Goog-Api-Client"),
            Some("google-genai-sdk/1.41.0 gl-node/v22.19.0")
        );
        assert_eq!(find("x-goog-user-project"), Some("project-1"));
        // An unfilled model still gives the CLI's user-agent shape.
        assert!(extra_headers("", "", "")
            .iter()
            .any(|(name, value)| *name == "User-Agent" && value.contains("/unknown (")));
        assert!(extra_headers("", "", "")
            .iter()
            .all(|(name, _)| *name != "x-goog-user-project"));
    }

    /// The static fallback names the CLI's models, without duplicates.
    #[test]
    fn the_fallback_models_are_the_reference_list() {
        let models = fallback_models();
        assert!(!models.is_empty());
        assert!(models.contains(&"gemini-3-pro-preview"));
        assert!(models.contains(&"gemini-2.5-flash-lite"));
        let mut sorted = models.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), models.len());
        assert_eq!(protocol_for("gemini-3-pro-preview"), Protocol::CloudCode);
    }

    /// Refusals in the CLI's terms: a rejected sign-in, the plan limit, and a
    /// project that needs onboarding; anything else is generic.
    #[test]
    fn refusals_say_what_to_do() {
        let refused = refusal(401, r#"{"error":{"message":"invalid token"}}"#).unwrap();
        assert!(
            refused.contains("Gemini CLI sign-in; sign in again"),
            "{refused}"
        );
        assert!(refused.contains("invalid token"), "{refused}");
        assert!(refusal(403, "").unwrap().contains("sign in again"));
        let limited = refusal(429, "").unwrap();
        assert!(limited.contains("plan's limit is used up"), "{limited}");
        let project = refusal(400, "").unwrap();
        assert!(
            project.contains("project may not be onboarded"),
            "{project}"
        );
        assert_eq!(refusal(500, ""), None);
    }
}
