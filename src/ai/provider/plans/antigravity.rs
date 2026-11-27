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
//! * The client presents as the current `agy` CLI (`antigravity/cli/1.5.5`,
//!   consumer auth) and opens the system prompt with its own identity, which
//!   the consumer endpoints check; the Cloud Code project travels in the
//!   envelope.

use serde_json::{json, Value};

use super::super::cloud_code::ANTIGRAVITY_USER_AGENT;
use super::super::{cloud_code, refusal as generic_refusal, ChatRequest, Turn, Wire};
use crate::ai::Protocol;

/// The host the IDE's chat traffic goes to.
const DAILY_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// The host the `kind` preset names (and what the onboarding uses).
const PROD_BASE: &str = "https://cloudcode-pa.googleapis.com";

/// Models kept when the live catalog cannot be read: the Antigravity wire ids
/// of the account catalog in 2026-11 (`fetchAvailableModels`), image/test
/// routes left out. The live list replaces these at sign-in.
const MODELS: &[&str] = &[
    "claude-opus-5-5-high",
    "claude-opus-5-5-low",
    "claude-opus-5-5-medium",
    "claude-sonnet-5-5-high",
    "claude-sonnet-5-5-low",
    "claude-sonnet-5-5-medium",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.5-flash-thinking",
    "gemini-3-flash",
    "gemini-3.1-flash-lite",
    "gemini-3.5-flash-lite",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-low",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-tiered",
    "gemini-4-argon-high",
    "gemini-4-argon-low",
    "gemini-4-argon-medium",
    "gemini-4-carbon-high",
    "gemini-4-carbon-low",
    "gemini-4-carbon-medium",
    "gemini-4-hydrogen-high",
    "gemini-4-hydrogen-low",
    "gemini-4-hydrogen-medium",
    "gemini-4-hydrogen-sonar",
    "gemini-4-hydrogen-tiered-high",
    "gemini-4-hydrogen-tiered-xhigh",
    "gemini-4-hydrogen-xhigh",
    "gemini-4-neon-high",
    "gemini-4-neon-low",
    "gemini-4-neon-medium",
    "gemini-4-neon-xhigh",
    "gpt-oss-120b-medium",
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

/// The IDE identity: only the client's user agent. The bearer token,
/// `Content-Type` and `Accept` come from the generic headers
/// (`headers`/`stream_chat`).
///
/// Deliberately no `x-goog-user-project`: that header names a real Google
/// Cloud quota project, and Antigravity's project is not one (for a consumer
/// account it is Google's shared `aicode-consumers`, and RAST may hold a
/// stable synthetic id). Sending it makes the host answer
/// `400 Project 'projects/…' not found or deleted` (or `403`) while the same
/// request without it succeeds — verified against the live host 2026-11-27.
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    _account: &str,
) -> Vec<(&'static str, String)> {
    vec![("User-Agent", ANTIGRAVITY_USER_AGENT.to_string())]
}

/// The envelope is [`cloud_code::body`]'s.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(req: &ChatRequest, wire: Wire, body: &mut Value) {
    if wire.protocol != Protocol::CloudCode {
        return;
    }
    let Some(request) = body.get_mut("request").and_then(Value::as_object_mut) else {
        return;
    };
    // The real client opens the system prompt with its own identity and keeps
    // the whole system text as the first user turn: the consumer endpoints
    // answer a long wire-native `systemInstruction` with a generic 429 (the
    // reference clients inline it the same way).
    request.remove("systemInstruction");
    let mut instruction = String::from(cloud_code::ANTIGRAVITY_IDENTITY);
    let system = req.system.trim();
    if !system.is_empty() {
        instruction.push_str("\n\n");
        instruction.push_str(system);
    }
    let part = json!({ "text": instruction });
    let contents = request.entry("contents").or_insert_with(|| json!([]));
    let Some(contents) = contents.as_array_mut() else {
        return;
    };
    match contents.first_mut().and_then(Value::as_object_mut) {
        Some(first) if first.get("role").and_then(Value::as_str) == Some("user") => {
            if let Some(parts) = first.get_mut("parts").and_then(Value::as_array_mut) {
                parts.insert(0, part);
            }
        }
        _ => contents.insert(0, json!({ "role": "user", "parts": [part] })),
    }
}

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
    let lower = body.to_ascii_lowercase();
    let what = match status {
        401 => "Google refused the Antigravity sign-in; sign in again under AI Agent settings → Providers".to_string(),
        403 if lower.contains("serviceusage")
            || lower.contains("permission")
            || lower.contains("aicode-consumers") =>
        {
            "Google refused the request for this account's Antigravity project: finish the Antigravity onboarding (open the Antigravity app once, signed in with the same account), then Sign out and Sign in again under AI Agent settings → Providers".to_string()
        }
        403 => "Google refused the request; if this keeps happening, Sign out and Sign in again under AI Agent settings → Providers".to_string(),
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
            Some("antigravity/cli/1.5.5 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)")
        );
        assert_eq!(find("x-goog-user-project"), None);
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
        assert!(models.contains(&"gemini-4-hydrogen-high"));
        assert!(models.contains(&"claude-sonnet-5-5-medium"));
        let mut sorted = models.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), models.len());
        assert_eq!(protocol_for("gemini-3-pro-preview"), Protocol::CloudCode);
    }

    /// The body opens every request with the client's identity and keeps the
    /// system text in the first user turn (a long wire-native
    /// `systemInstruction` gets a generic 429 from the consumer endpoints).
    #[test]
    fn the_body_presents_as_the_antigravity_client() {
        let req = ChatRequest {
            kind: crate::ai::Kind::Antigravity,
            base_url: PROD_BASE.into(),
            api_key: Some("token-1".into()),
            model: "gemini-3-pro-preview".into(),
            system: "be brief".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_output_tokens: None,
            effort: None,
            session: String::new(),
            account: "project-1".into(),
        };
        let wire = Wire {
            protocol: Protocol::CloudCode,
            ..Wire::of(crate::ai::Kind::Antigravity)
        };
        let mut body = json!({
            "project": "project-1",
            "request": {
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "systemInstruction": {"parts": [{"text": "be brief"}]},
            }
        });
        tweak_body(&req, wire, &mut body);
        assert!(body.pointer("/request/systemInstruction").is_none());
        assert_eq!(
            body.pointer("/request/contents/0/parts/0/text"),
            Some(&json!(format!(
                "{}\n\nbe brief",
                cloud_code::ANTIGRAVITY_IDENTITY
            )))
        );
        assert_eq!(
            body.pointer("/request/contents/0/parts/1/text"),
            Some(&json!("hi"))
        );
        // A request without user content still gets the identity.
        let mut empty = json!({"request": {"contents": []}});
        tweak_body(&req, wire, &mut empty);
        assert_eq!(
            empty.pointer("/request/contents/0/role"),
            Some(&json!("user"))
        );
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
        assert!(refusal(403, "").unwrap().contains("Sign in again"));
        let project = refusal(
            403,
            r#"{"error":{"message":"Caller does not have required permission to use project aicode-consumers"}}"#,
        )
        .unwrap();
        assert!(project.contains("Antigravity onboarding"), "{project}");
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
