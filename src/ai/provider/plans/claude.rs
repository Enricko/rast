//! Claude Pro/Max (`claude`): Anthropic Messages presented as Claude Code,
//! because the subscription endpoint only serves requests that look like the
//! CLI. A signed-in plan carries an OAuth bearer where an API key would go,
//! so the generic Anthropic headers already authenticate; what this file adds
//! is the CLI identity (`anthropic-beta`, user agent, `x-app`) and the one
//! body change the endpoint insists on: the Claude Code system block has to
//! come first, ahead of RAST's own prompt.
//!
//! The CLI version is a constant: the plugin resolves it from npm, but a
//! server should not call npm per request, and a version-gated model is out
//! of scope. Everything below was checked against the 2026-11-27 references
//! (`dsh-plugin-subscriptions/src/providers/claude.ts`,
//! `9router/open-sse/providers/shared.js` and
//! `9router/open-sse/providers/registry/claude.js`).

use serde_json::{json, Value};

use super::super::{
    agent, read_limited, refusal as generic_refusal, ChatRequest, Turn, Wire, LIST_TIMEOUT,
};
use crate::ai::{Protocol, MAX_MODELS, MAX_MODEL_NAME};

/// The Claude Code version presented to the endpoint: 9router's fallback
/// (`CLAUDE_CLI_VERSION`); the plugin normally resolves the live npm version.
const CLI_VERSION: &str = "2.1.283";

/// The beta flags the Claude Code CLI sends, as 9router's full fingerprint
/// (`CLAUDE_API_HEADERS`'s `Anthropic-Beta`): `oauth-2025-04-20` is what lets
/// the Messages API accept an OAuth bearer at all, `claude-code-20250219` is
/// the subscription client identity, the rest carry the CLI's capabilities.
const ANTHROPIC_BETA: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24,structured-outputs-2025-12-15,fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28";

/// The mandatory identity block: the subscription endpoint refuses a request
/// whose first system entry is not this (the plugin calls it REQUIRED).
pub(crate) const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Models kept when Claude's live catalog cannot be read: the built-in list
/// the DSH plugin ships for `claude` in the 2026-11-27 refs.
const MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-haiku-4-5-20251001",
];

/// The user agent the CLI sends; both the plugin and 9router spell the
/// identity suffix `(external, sdk-cli)`.
fn user_agent() -> String {
    format!("claude-cli/{CLI_VERSION} (external, sdk-cli)")
}

// ------------------------------------------------------------- plan hooks

/// Claude is the Anthropic Messages wire for every model.
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Anthropic
}

/// The Claude Code identity headers. The bearer token itself, the
/// `anthropic-version` marker and `Content-Type` come from the generic
/// headers; these are the ones only the subscription endpoint wants.
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    _account: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("anthropic-beta", ANTHROPIC_BETA.to_string()),
        ("User-Agent", user_agent()),
        ("x-app", "cli".to_string()),
        (
            "anthropic-dangerous-direct-browser-access",
            "true".to_string(),
        ),
    ]
}

/// Claude's body comes from the generic Anthropic builder; only the system
/// block is changed (below), and `max_tokens` keeps that builder's default
/// (16000 when the request names none), which the endpoint accepts.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, body: &mut Value) {
    prepend_identity(body);
}

/// Put the Claude Code identity block first in `system`, whatever shape the
/// body has there: the generic builder writes RAST's prompt as a string, but
/// an array of text blocks is equally valid Anthropic wire (and what a plan's
/// own builder may produce), so both are merged with the identity ahead.
fn prepend_identity(body: &mut Value) {
    let identity = json!({"type": "text", "text": IDENTITY});
    let system = match body.get_mut("system").map(Value::take) {
        Some(Value::Array(mut blocks)) => {
            blocks.insert(0, identity);
            Value::Array(blocks)
        }
        Some(Value::String(text)) if !text.trim().is_empty() => {
            json!([identity, {"type": "text", "text": text}])
        }
        // Empty, missing or a shape RAST does not write: the identity alone.
        _ => json!([identity]),
    };
    body["system"] = system;
}

/// `GET {base}/models?beta=true` with the bearer and the OAuth beta. The
/// default `base_url` already ends in `/v1`; a configured one is respected.
pub(crate) fn catalog(base_url: &str, token: &str, _account: &str) -> Result<Vec<String>, String> {
    if base_url.is_empty() {
        return Err("Claude has no address; set its base URL".into());
    }
    // The catalog only needs the OAuth beta (as the plugin sends it) and the
    // CLI's user agent; the messages identity headers are not wanted here.
    let request = agent(LIST_TIMEOUT)
        .get(catalog_url(base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", user_agent())
        .header("Accept", "application/json");
    let response = request
        .call()
        .map_err(|e| format!("cannot reach Claude: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), 4 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(refusal(status, &body).unwrap_or_else(|| generic_refusal(status, &body)));
    }
    parse_catalog(&body)
}

/// Where the catalog lives: `?beta=true` is part of the subscription
/// endpoint's address (the Messages API returns its own list without it).
fn catalog_url(base_url: &str) -> String {
    format!("{}/models?beta=true", base_url.trim_end_matches('/'))
}

/// Claude's model list: `data[]` of `{id, display_name, …}`. The order the
/// endpoint gives (newest family first) is kept.
fn parse_catalog(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "Claude sent a model list RAST cannot read".to_string())?;
    let list = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Claude sent no model list")?;
    let mut out: Vec<String> = Vec::new();
    for model in list {
        let Some(id) = model
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_MODEL_NAME)
        else {
            continue;
        };
        if !out.iter().any(|known| known == id) {
            out.push(id.to_string());
        }
    }
    out.truncate(MAX_MODELS);
    Ok(out)
}

/// Models to keep when the catalog cannot be read.
pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// `{base}/messages?beta=true`: the subscription endpoint's address, which
/// the generic `/messages` path does not carry.
pub(crate) fn chat_url(base_url: &str) -> Option<String> {
    Some(format!(
        "{}/messages?beta=true",
        base_url.trim_end_matches('/')
    ))
}

/// What Claude said when it refused, in the plan's own terms: a refused
/// sign-in is the common failure (the OAuth token is rejected once the plan
/// ends or the CLI identity is not presented).
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    Some(claude_refusal(status, body))
}

fn claude_refusal(status: u16, body: &str) -> String {
    let said = generic_refusal(status, body);
    let said = said.split_once("): ").map(|(_, s)| s).unwrap_or("");
    let what = match status {
        401 | 403 => {
            "Claude refused the sign-in; sign in again under AI Agent settings → Providers"
                .to_string()
        }
        429 => "your Claude plan's limit is used up for now; try again later".to_string(),
        _ => return generic_refusal(status, body),
    };
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::headers;
    use super::super::super::ToolSpec;
    use super::*;
    use crate::ai::{Effort, Kind};

    fn request() -> ChatRequest {
        ChatRequest {
            kind: Kind::Claude,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some("oauth-token".into()),
            model: "claude-sonnet-5".into(),
            system: "be brief".into(),
            messages: vec![Turn::User("hi".into())],
            tools: vec![ToolSpec {
                name: "get_time".into(),
                description: "the time".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
            temperature: Some(0.3),
            max_output_tokens: Some(32_000),
            effort: Some(Effort::High),
            session: String::new(),
            account: String::new(),
        }
    }

    /// The generic Anthropic builder plus this plan's tweak: the identity
    /// block leads the system prompt, tools and effort are untouched.
    #[test]
    fn the_identity_block_leads_the_system_prompt() {
        let req = request();
        let wire = Wire {
            effort: req.effort,
            ..Wire::of(Kind::Claude)
        };
        let body = super::super::super::request_body_for(&req, wire);
        assert_eq!(body["model"], "claude-sonnet-5");
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": IDENTITY},
                {"type": "text", "text": "be brief"},
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{
                "name": "get_time",
                "description": "the time",
                "input_schema": {"type": "object", "properties": {}}
            }])
        );
        // Message assembly and the effort knob are the generic builder's.
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_eq!(body["max_tokens"], 32_000);
        assert_eq!(body["stream"], true);
    }

    /// Without a configured maximum the generic builder's 16000 stands, and
    /// an empty system prompt still gets the identity block.
    #[test]
    fn an_empty_prompt_still_gets_the_identity() {
        let mut req = request();
        req.system = String::new();
        req.max_output_tokens = None;
        req.tools.clear();
        let body = super::super::super::request_body_for(&req, Wire::of(Kind::Claude));
        assert_eq!(body["system"], json!([{"type": "text", "text": IDENTITY}]));
        assert_eq!(body["max_tokens"], 16_000);
        assert!(body.get("tools").is_none());
    }

    /// A bank of system blocks (what an override body may hand over) keeps
    /// its order behind the identity; the identity is never duplicated away.
    #[test]
    fn an_array_system_prompt_is_prepended_to() {
        let mut body = json!({
            "model": "claude-sonnet-5",
            "system": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}],
            "messages": [],
        });
        tweak_body(&request(), Wire::of(Kind::Claude), &mut body);
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": IDENTITY},
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ])
        );
        // A body with no system at all gets the identity alone.
        let mut body = json!({"model": "claude-sonnet-5", "messages": []});
        tweak_body(&request(), Wire::of(Kind::Claude), &mut body);
        assert_eq!(body["system"], json!([{"type": "text", "text": IDENTITY}]));
    }

    /// The wire authenticates with the OAuth bearer (never `x-api-key`) and
    /// carries the Claude Code identity headers.
    #[test]
    fn the_wire_presents_as_claude_code() {
        let mut all = headers(Kind::Claude, Protocol::Anthropic, Some("oauth-token"), "");
        all.extend(extra_headers("", "", ""));
        let find = |name: &str| {
            all.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(find("Authorization").as_deref(), Some("Bearer oauth-token"));
        assert_eq!(find("x-api-key"), None);
        assert_eq!(find("anthropic-version").as_deref(), Some("2023-06-01"));
        let beta = find("anthropic-beta").expect("a beta header");
        assert!(beta.contains("oauth-2025-04-20"), "{beta}");
        assert!(beta.contains("claude-code-20250219"), "{beta}");
        assert_eq!(
            find("User-Agent").as_deref(),
            Some("claude-cli/2.1.283 (external, sdk-cli)")
        );
        assert_eq!(find("x-app").as_deref(), Some("cli"));
        assert_eq!(
            find("anthropic-dangerous-direct-browser-access").as_deref(),
            Some("true")
        );
    }

    /// Both the generation and the catalog addresses carry `?beta=true`, and
    /// a trailing slash in a configured base URL does not double up.
    #[test]
    fn the_plan_addresses_carry_beta() {
        assert_eq!(
            chat_url("https://api.anthropic.com/v1").as_deref(),
            Some("https://api.anthropic.com/v1/messages?beta=true")
        );
        assert_eq!(
            chat_url("https://api.anthropic.com/v1/").as_deref(),
            Some("https://api.anthropic.com/v1/messages?beta=true")
        );
        assert_eq!(
            catalog_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/models?beta=true"
        );
        assert_eq!(
            catalog_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/models?beta=true"
        );
    }

    /// The live catalog is `data[]` of `{id}`: ids in the order Claude gives
    /// them, duplicates dropped, unusable entries skipped.
    #[test]
    fn the_model_catalog_is_read() {
        let fixture = r#"{
            "data": [
                {"type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5"},
                {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5"},
                {"type": "model", "id": "claude-opus-5-5"},
                {"type": "model", "id": "  "},
                {"type": "model"}
            ],
            "has_more": false
        }"#;
        assert_eq!(
            parse_catalog(fixture).unwrap(),
            vec!["claude-opus-5-5", "claude-sonnet-5"]
        );
        assert_eq!(
            parse_catalog(r#"{"data": []}"#).unwrap(),
            Vec::<String>::new()
        );
        assert!(parse_catalog("not json").is_err());
        assert!(parse_catalog(r#"{"models": []}"#).is_err());
    }

    /// Refusals in Claude's terms: a rejected sign-in, then the plan limit;
    /// anything else is the generic wording.
    #[test]
    fn refusals_say_what_to_do() {
        let refused = refusal(401, r#"{"error":{"message":"invalid token"}}"#).unwrap();
        assert!(
            refused.contains(
                "Claude refused the sign-in; sign in again under AI Agent settings → Providers"
            ),
            "{refused}"
        );
        assert!(refused.contains("invalid token"), "{refused}");
        assert!(refusal(403, "").unwrap().contains("sign in again"));
        let limited = refusal(429, "").unwrap();
        assert!(limited.contains("plan's limit is used up"), "{limited}");
        // Anything else falls back to the generic wording.
        assert!(refusal(400, "")
            .unwrap()
            .contains("the provider refused the request"));
    }

    /// The static fallback names real Claude models and no duplicates.
    #[test]
    fn the_fallback_models_are_the_reference_list() {
        let models = fallback_models();
        assert!(!models.is_empty());
        assert!(models.contains(&"claude-opus-5-5"));
        assert!(models.contains(&"claude-haiku-4-5-20251001"));
        let mut sorted = models.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), models.len());
        assert_eq!(protocol_for("claude-sonnet-5"), Protocol::Anthropic);
    }
}
