//! Google Antigravity (`antigravity`): Cloud Code `v1internal` (see
//! [`super::super::cloud_code`]). The IDE's identity headers, the generation
//! and catalog addresses, `fetchAvailableModels` and the plan's own refusal
//! wording; the Gemini CLI shares the wire module through its own file.
//!
//! Checked against the 2026-11-27 references
//! (`dsh-plugin-subscriptions/src/providers/antigravity.ts`,
//! `9router/open-sse/providers/registry/antigravity.js` and the executor):
//! * Chat and catalog traffic go to `daily-cloudcode-pa.googleapis.com` (the
//!   host the IDE is served from; 9router pins it because production
//!   rate-limits the IDE), while `loadCodeAssist`/`onboardUser` stay on
//!   production — the daily host refuses those (see `ai::signin::antigravity`).
//! * The client presents as the bundled Hub desktop build
//!   (`antigravity/cli/1.3.4 (aidev_client; …)`), and the Cloud Code project
//!   travels both in the envelope and as `x-goog-user-project`.

use serde_json::Value;

use super::super::cloud_code::ANTIGRAVITY_USER_AGENT;
use super::super::{cloud_code, refusal as generic_refusal, ChatRequest, Turn, Wire};
use crate::ai::Protocol;

/// The host the IDE's chat traffic goes to.
const DAILY_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// The host the `kind` preset names (and what the onboarding uses).
const PROD_BASE: &str = "https://cloudcode-pa.googleapis.com";

/// Models kept when the live catalog cannot be read (the current Antigravity
/// catalogue's own wire ids; the live list replaces these at sign-in).
const MODELS: &[&str] = &[
    "gemini-3.1-pro-high",
    "gemini-3.1-pro-low",
    "gemini-3-flash-preview",
    "claude-sonnet-4-5",
    "gpt-oss-120b",
];

/// Where chat and catalog traffic goes: the IDE's daily host unless the
/// administrator configured another origin.
fn chat_base(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() || base == PROD_BASE {
        DAILY_BASE.to_string()
    } else {
        base.to_string()
    }
}

// ------------------------------------------------------------- plan hooks

/// The protocol this plan speaks for `model` (fixed: Cloud Code).
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::CloudCode
}

/// The IDE identity: the client's user agent and the Cloud Code project.
/// The bearer token, `Content-Type` and `Accept` come from the generic
/// headers (`headers`/`stream_chat`).
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    let mut out = vec![("User-Agent", ANTIGRAVITY_USER_AGENT.to_string())];
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

/// `POST {daily}/v1internal:fetchAvailableModels {project}` with the bearer.
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

/// What Google said when it refused, in the plan's own terms: a rejected
/// sign-in is the common failure, then the plan limit, then a project that
/// was never onboarded.
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => {
            "Google refused the Antigravity sign-in; sign in again under AI Agent settings → Providers"
                .to_string()
        }
        429 => "Google's Antigravity limit is used up for now; try again later".to_string(),
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

    /// Chat and catalog traffic goes to the IDE's daily host, and a
    /// configured origin is respected (with or without a trailing slash).
    #[test]
    fn the_plan_addresses_use_the_ides_host() {
        assert_eq!(
            chat_url(PROD_BASE).as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            chat_url("https://daily-cloudcode-pa.googleapis.com/").as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            chat_url("https://proxy.example/v1").as_deref(),
            Some("https://proxy.example/v1/v1internal:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            chat_base(PROD_BASE),
            "https://daily-cloudcode-pa.googleapis.com"
        );
        assert_eq!(chat_base(""), "https://daily-cloudcode-pa.googleapis.com");
        assert_eq!(chat_base("https://proxy.example"), "https://proxy.example");
    }

    /// The wire presents as the Antigravity client and names the Cloud Code
    /// project; without a project (an unfinished sign-in) the header is left
    /// out rather than sent empty.
    #[test]
    fn the_wire_presents_as_the_ide() {
        let headers = extra_headers("gemini-3-pro-preview", "conv-1", "project-1");
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(
            find("User-Agent"),
            Some("antigravity/cli/1.3.4 (aidev_client; linux; x86_64)")
        );
        assert_eq!(find("x-goog-user-project"), Some("project-1"));
        assert!(extra_headers("", "", "")
            .iter()
            .all(|(name, _)| *name != "x-goog-user-project"));
        // The OAuth bearer and Content-Type come from the generic headers.
        let generic = super::super::super::headers(
            crate::ai::Kind::Antigravity,
            Protocol::CloudCode,
            Some("token-1"),
            "",
        );
        assert!(generic
            .iter()
            .any(|(name, value)| *name == "Authorization" && value == "Bearer token-1"));
        assert!(generic
            .iter()
            .any(|(name, value)| *name == "Content-Type" && value == "application/json"));
    }

    /// The static fallback names Antigravity wire ids, without duplicates.
    #[test]
    fn the_fallback_models_are_the_reference_list() {
        let models = fallback_models();
        assert!(!models.is_empty());
        assert!(models.contains(&"gemini-3.1-pro-high"));
        assert!(models.contains(&"claude-sonnet-4-5"));
        let mut sorted = models.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), models.len());
        assert_eq!(protocol_for("gemini-3-pro-preview"), Protocol::CloudCode);
    }

    /// Refusals in Antigravity's terms: a rejected sign-in, the plan limit,
    /// and a project that needs onboarding; anything else is generic.
    #[test]
    fn refusals_say_what_to_do() {
        let refused = refusal(401, r#"{"error":{"message":"invalid token"}}"#).unwrap();
        assert!(
            refused.contains("Antigravity sign-in; sign in again"),
            "{refused}"
        );
        assert!(refused.contains("invalid token"), "{refused}");
        assert!(refusal(403, "").unwrap().contains("sign in again"));
        let limited = refusal(429, "").unwrap();
        assert!(limited.contains("limit is used up"), "{limited}");
        let project = refusal(400, "").unwrap();
        assert!(
            project.contains("project may not be onboarded"),
            "{project}"
        );
        assert_eq!(refusal(500, ""), None);
    }
}
