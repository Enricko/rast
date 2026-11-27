//! GitHub Copilot (`copilot`): the GitHub Copilot API
//! (`https://api.githubcopilot.com`) sent as VS Code's Copilot Chat — OAuth
//! bearer plus the editor identity headers — over the wire protocol each
//! model's catalog entry lists: chat completions for most models, the
//! Responses wire for the newer GPT families that only serve `/responses`.
//!
//! The live catalog is the authority on which wire a model speaks; its
//! `/models` answer lists `supported_endpoints` per model and [`catalog`]
//! remembers the answers for [`protocol_for`] (the request path cannot read
//! the catalog). A documented name heuristic covers the time before a catalog
//! has been read (a provider saved at sign-in, a server restart), and the
//! static fallback list keeps a provider usable when the catalog fails.
//!
//! Bodies need little fixing: the chat wire spells the output cap
//! `max_completion_tokens` (the newer OpenAI-family models reject the legacy
//! `max_tokens`), and the Responses wire takes the cap and only the reasoning
//! effort the catalog advertises.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};

use super::super::models::models_url;
use super::super::{
    agent, read_limited, refusal as generic_refusal, ChatRequest, Turn, Wire, LIST_TIMEOUT,
};
use crate::ai::{Effort, Kind, Protocol, MAX_MODELS, MAX_MODEL_NAME};

/// The editor identity Copilot's endpoints want with every request (pinned
/// fallback versions: the reference does not fetch them at runtime either).
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const API_VERSION: &str = "2026-06-01";

/// Models offered when Copilot's catalog cannot be read (from the 9router
/// registry's GitHub entry; a chat, a mini, a Responses-only and a Claude
/// model, the four wires the catalog serves).
const MODELS: &[&str] = &["gpt-5.4", "gpt-5-mini", "gpt-5.6-sol", "claude-sonnet-4.6"];

/// The protocol this plan speaks for `model`: the catalog entry's listed
/// endpoints decide (chat completions when listed, else the Responses wire).
pub(crate) fn protocol_for(model: &str) -> Protocol {
    let hints = WIRES.lock().unwrap_or_else(|e| e.into_inner());
    protocol_with_hints(model, &hints)
}

/// Wire identity headers this plan needs beyond the generic ones: the editor
/// identity. The generic `headers` already carries the Copilot token.
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    _account: &str,
) -> Vec<(&'static str, String)> {
    editor_headers()
}

/// A complete body for a plan whose request shape the generic builders in
/// [`super::super::request_body_for`] cannot make: none, the generic body
/// plus [`tweak_body`]'s changes is Copilot's shape.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

/// Last changes to a generic body: Copilot's spellings of the output cap,
/// and its Responses endpoint's effort handling.
pub(crate) fn tweak_body(req: &ChatRequest, wire: Wire, body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    match wire.protocol {
        Protocol::Chat => {
            // Copilot's newer OpenAI-family models reject the legacy
            // `max_tokens` outright; the whole catalog takes
            // `max_completion_tokens` (the reference renames it the same way).
            if let Some(max) = object.remove("max_tokens") {
                object.insert("max_completion_tokens".into(), max);
            }
            // Copilot publishes "xhigh" for its strongest reasoning models;
            // the generic builder maps `Max` to "high" outside OpenAI.
            if req.effort == Some(Effort::Max) && object.contains_key("reasoning_effort") {
                object.insert("reasoning_effort".into(), json!("xhigh"));
            }
        }
        Protocol::Responses => {
            // The generic Responses builder is Codex's: it leaves the output
            // cap to the provider and always names an effort. Copilot's
            // endpoint takes the cap and only the effort a model advertises
            // (the reference sends `reasoning` only for a chosen effort).
            if let Some(max) = req.max_output_tokens {
                object.insert("max_output_tokens".into(), json!(max));
            }
            if req.effort.is_none() {
                object.remove("reasoning");
            }
        }
        _ => {}
    }
}

/// The plan's live model catalog (`GET {base}/models`): Copilot's entry list
/// with each model's endpoints, picker visibility and policy.
pub(crate) fn catalog(base_url: &str, token: &str, _account: &str) -> Result<Vec<String>, String> {
    let mut request = agent(LIST_TIMEOUT)
        .get(models_url(Kind::Copilot, base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    for (name, value) in editor_headers() {
        request = request.header(name, value);
    }
    let response = request
        .call()
        .map_err(|e| format!("cannot reach GitHub Copilot: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), 8 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(copilot_refusal(status, &body));
    }
    let catalog = parse_catalog(&body)?;
    remember(&catalog);
    Ok(catalog.models)
}

/// What Copilot's catalog answer offers and how each model is served.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    /// The models the picker may offer, in Copilot's order.
    pub models: Vec<String>,
    /// Models whose entry lists `/responses` and not `/chat/completions`.
    pub responses_only: Vec<String>,
    /// Models whose entry lists `/chat/completions` (with or without
    /// `/responses`: chat is the default when a model serves both).
    pub chat: Vec<String>,
}

/// Copilot's model list: `data[]` of `{id, model_picker_enabled, policy,
/// supported_endpoints, …}`. Hidden, disabled and non-chat entries are left
/// out; an entry that lists neither endpoint is left to the heuristic.
pub(crate) fn parse_catalog(body: &str) -> Result<Catalog, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "GitHub Copilot sent a model list RAST cannot read".to_string())?;
    let list = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("GitHub Copilot sent no model list")?;
    let mut catalog = Catalog::default();
    for entry in list {
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_MODEL_NAME)
        else {
            continue;
        };
        // The picker's own rules: an entry meant to be offered, and not
        // disabled by policy.
        if entry.get("model_picker_enabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if entry.pointer("/policy/state").and_then(Value::as_str) == Some("disabled") {
            continue;
        }
        let endpoints: Vec<&str> = entry
            .get("supported_endpoints")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let has = |endpoint: &str| endpoints.contains(&endpoint);
        let chat = has("/chat/completions");
        let responses = has("/responses");
        // An entry that lists endpoints but can speak neither wire is not a
        // chat model (embeddings, websocket-only entries, …).
        if !endpoints.is_empty() && !chat && !responses {
            continue;
        }
        if catalog.models.iter().any(|model| model == id) {
            continue;
        }
        catalog.models.push(id.to_string());
        if chat {
            catalog.chat.push(id.to_string());
        } else if responses {
            catalog.responses_only.push(id.to_string());
        }
    }
    if catalog.models.is_empty() {
        return Err("GitHub Copilot offered no chat models".into());
    }
    catalog.models.truncate(MAX_MODELS);
    catalog.chat.truncate(MAX_MODELS);
    catalog.responses_only.truncate(MAX_MODELS);
    Ok(catalog)
}

/// What the last read catalog said about each model's wire.
#[derive(Debug, Default)]
struct WireHints {
    responses_only: HashSet<String>,
    chat: HashSet<String>,
}

/// The catalog's wire hints, read by [`protocol_for`] on the request path
/// (it has no catalog of its own to consult).
static WIRES: LazyLock<Mutex<WireHints>> = LazyLock::new(Default::default);

/// Remember a just-read catalog: which models it showed on which wire.
fn remember(catalog: &Catalog) {
    let mut hints = WIRES.lock().unwrap_or_else(|e| e.into_inner());
    for model in &catalog.chat {
        hints.chat.insert(model.clone());
        hints.responses_only.remove(model);
    }
    for model in &catalog.responses_only {
        hints.responses_only.insert(model.clone());
        hints.chat.remove(model);
    }
}

/// The wire for one model, given what a catalog said (if anything).
fn protocol_with_hints(model: &str, hints: &WireHints) -> Protocol {
    if hints.responses_only.contains(model) {
        return Protocol::Responses;
    }
    if hints.chat.contains(model) {
        return Protocol::Chat;
    }
    if model_takes_responses(model) {
        Protocol::Responses
    } else {
        Protocol::Chat
    }
}

/// The documented heuristic for when no catalog has been read: the newer GPT
/// families serve only `/responses` (the reference names gpt-5.5/5.6 and
/// routes its gpt-5.6 fixture there; the 9router registry's next families are
/// gpt-6/6.1), and `-codex` models are the other known Responses family
/// (9router learns to escalate them off `/chat/completions`). Everything else
/// defaults to the chat wire, as the reference does for unknown models.
fn model_takes_responses(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5.5")
        || model.starts_with("gpt-5.6")
        || model.starts_with("gpt-6")
        || model.contains("-codex")
}

/// Models to keep when the catalog cannot be read.
pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// Where generation goes when the generic URL is wrong: nowhere — the generic
/// `{base}/chat/completions` and `{base}/responses` are Copilot's endpoints
/// for the configured base URL, so the override stays unused.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

/// The plan's own refusal wording (none: the generic one).
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => {
            "GitHub Copilot refused the sign-in; sign in again under AI Agent settings → Providers"
        }
        404 => "GitHub Copilot does not offer this model to your plan",
        429 => "your GitHub Copilot rate limit is used up for now",
        _ => return None,
    };
    Some(with_said(what, status, body))
}

/// What Copilot said about a refusal, in its own terms, appended to `what`.
fn with_said(what: &str, status: u16, body: &str) -> String {
    let said = generic_refusal(status, body);
    let said = said.split_once("): ").map(|(_, said)| said).unwrap_or("");
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

/// The catalog's refusal wording (no sign-in hint of its own for plan losses;
/// 401/403 still say to sign in again through [`refusal`]).
fn copilot_refusal(status: u16, body: &str) -> String {
    refusal(status, body).unwrap_or_else(|| generic_refusal(status, body))
}

/// The editor identity every Copilot request carries.
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

#[cfg(test)]
mod tests {
    use super::super::super::{chat_url as generic_chat_url, request_body_for};
    use super::*;
    use crate::ai::provider::{ToolSpec, Turn};
    use serde_json::json;

    fn request(
        protocol: Protocol,
        effort: Option<Effort>,
        max_output_tokens: Option<i64>,
    ) -> ChatRequest {
        ChatRequest {
            kind: Kind::Copilot,
            base_url: "https://api.githubcopilot.com".into(),
            api_key: None,
            model: match protocol {
                Protocol::Responses => "gpt-5.6-sol".into(),
                _ => "gpt-5.4".into(),
            },
            system: "be helpful".into(),
            messages: vec![Turn::User("hello".into())],
            tools: Vec::new(),
            temperature: None,
            max_output_tokens,
            effort,
            session: "conversation-1".into(),
            account: "octocat".into(),
        }
    }

    fn request_body(req: &ChatRequest, protocol: Protocol) -> Value {
        request_body_for(
            req,
            Wire {
                protocol,
                reasoning_back: false,
                usage_option: true,
                effort: req.effort,
            },
        )
    }

    /// A catalog entry's listed endpoints pick the wire; when the catalog has
    /// not been read the documented family heuristic does; everything else is
    /// the chat wire.
    #[test]
    fn the_wire_follows_the_catalog_then_the_families() {
        let mut hints = WireHints::default();
        hints.responses_only.insert("gpt-5.6-sol".into());
        hints.chat.insert("o4-mini".into());
        assert_eq!(
            protocol_with_hints("gpt-5.6-sol", &hints),
            Protocol::Responses
        );
        assert_eq!(protocol_with_hints("o4-mini", &hints), Protocol::Chat);
        // No hint: the families the reference names are Responses-only, the
        // codex ones need `/responses` too, the rest chat.
        for model in ["gpt-5.6-sol", "gpt-5.5", "gpt-6-astra", "gpt-5.3-codex"] {
            assert_eq!(
                protocol_with_hints(model, &WireHints::default()),
                Protocol::Responses,
                "{model}"
            );
        }
        for model in ["gpt-5.4", "gpt-5-mini", "claude-sonnet-4.6", "kimi-k3"] {
            assert_eq!(
                protocol_with_hints(model, &WireHints::default()),
                Protocol::Chat,
                "{model}"
            );
        }
        // A catalog hint outranks the heuristic both ways.
        let mut hints = WireHints::default();
        hints.chat.insert("gpt-5.6-sol".into());
        assert_eq!(protocol_with_hints("gpt-5.6-sol", &hints), Protocol::Chat);
        assert_eq!(
            protocol_for("gpt-5.4"),
            Protocol::Chat,
            "the live registry knows nothing about it"
        );
    }

    /// The catalog keeps the picker's models in order and remembers which
    /// ones only serve `/responses`.
    #[test]
    fn the_catalog_is_read() {
        let body = json!({"data": [
            {
                "id": "gpt-4.1", "model_picker_enabled": true,
                "policy": {"state": "enabled"},
                "supported_endpoints": ["/chat/completions"]
            },
            {
                "id": "o4-mini", "model_picker_enabled": true,
                "policy": {"state": "enabled"},
                "supported_endpoints": ["/chat/completions", "/responses"]
            },
            {
                "id": "gpt-5.6-sol", "model_picker_enabled": true,
                "policy": {"state": "enabled"},
                "supported_endpoints": ["/responses", "ws:/responses"]
            },
            {"id": "picker-hidden", "model_picker_enabled": false, "policy": {"state": "enabled"}},
            {"id": "policy-disabled", "model_picker_enabled": true, "policy": {"state": "disabled"}},
            {"id": "ws-only", "model_picker_enabled": true, "policy": {"state": "enabled"},
             "supported_endpoints": ["ws:/responses"]},
            {"id": "gpt-5.1", "model_picker_enabled": true, "policy": {"state": "enabled"},
             "supported_endpoints": ["/chat/completions"]},
            {"id": "gpt-5.1", "model_picker_enabled": true, "policy": {"state": "enabled"},
             "supported_endpoints": ["/chat/completions"]},
            {"id": "no-endpoints", "model_picker_enabled": true, "policy": {"state": "enabled"}}
        ]})
        .to_string();
        let catalog = parse_catalog(&body).unwrap();
        assert_eq!(
            catalog.models,
            vec![
                "gpt-4.1",
                "o4-mini",
                "gpt-5.6-sol",
                "gpt-5.1",
                "no-endpoints"
            ]
        );
        assert_eq!(catalog.chat, vec!["gpt-4.1", "o4-mini", "gpt-5.1"]);
        assert_eq!(catalog.responses_only, vec!["gpt-5.6-sol"]);
        assert!(parse_catalog("not json").is_err());
        assert!(parse_catalog(r#"{"object": "list"}"#).is_err());
        assert!(
            parse_catalog(r#"{"data": [{"id": "x", "model_picker_enabled": false}]}"#).is_err()
        );
    }

    /// The chat wire renames the output cap and keeps Copilot's strongest
    /// effort; the Responses wire takes the cap and no unspecified effort.
    #[test]
    fn the_body_is_copilots_shape() {
        // Chat wire.
        let req = request(Protocol::Chat, Some(Effort::Max), Some(16_000));
        let body = request_body(&req, Protocol::Chat);
        assert_eq!(body["max_completion_tokens"], 16_000);
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // No tools, no tools field; effort "high" stays as it is.
        let body = request_body(
            &request(Protocol::Chat, Some(Effort::High), None),
            Protocol::Chat,
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("max_completion_tokens").is_none(), "{body}");

        // Responses wire: the cap is added, the default effort is not.
        let req = request(Protocol::Responses, None, Some(16_000));
        let body = request_body(&req, Protocol::Responses);
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["max_output_tokens"], 16_000);
        assert!(body.get("reasoning").is_none(), "{body}");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        // With a chosen effort the nested effort arrives.
        let body = request_body(
            &request(Protocol::Responses, Some(Effort::High), None),
            Protocol::Responses,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        // Tools use the Responses shape.
        let mut req = request(Protocol::Responses, None, None);
        req.tools = vec![ToolSpec {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object"}),
        }];
        let body = request_body(&req, Protocol::Responses);
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tool_choice"], "auto");
    }

    /// Both Copilot endpoints are the generic URLs for the kind's base.
    #[test]
    fn the_generic_urls_are_copilots() {
        assert_eq!(chat_url("https://api.githubcopilot.com"), None);
        assert_eq!(
            generic_chat_url(
                Kind::Copilot,
                "https://api.githubcopilot.com",
                Protocol::Chat
            ),
            "https://api.githubcopilot.com/chat/completions"
        );
        assert_eq!(
            generic_chat_url(
                Kind::Copilot,
                "https://api.githubcopilot.com",
                Protocol::Responses
            ),
            "https://api.githubcopilot.com/responses"
        );
        assert_eq!(
            models_url(Kind::Copilot, "https://api.githubcopilot.com"),
            "https://api.githubcopilot.com/models"
        );
    }

    /// Refusals tell the person to sign in again or wait, and hand the rest
    /// to the generic wording.
    #[test]
    fn refusals_say_what_to_do() {
        assert!(refusal(401, "").unwrap().contains("sign in again"));
        assert!(refusal(403, "").unwrap().contains("sign in again"));
        assert!(refusal(404, "").unwrap().contains("model"));
        assert!(refusal(429, "").unwrap().contains("rate limit"));
        assert!(refusal(429, r#"{"error":{"message":"slow down"}}"#)
            .unwrap()
            .contains("slow down"));
        assert_eq!(refusal(500, ""), None);
        assert_eq!(
            fallback_models(),
            &["gpt-5.4", "gpt-5-mini", "gpt-5.6-sol", "claude-sonnet-4.6"]
        );
    }

    /// The editor identity rides every request (the generic headers already
    /// carry the token).
    #[test]
    fn the_editor_identity_is_sent() {
        let headers = extra_headers("gpt-5.4", "session", "octocat");
        assert!(headers.contains(&("user-agent", "GitHubCopilotChat/0.35.0".to_string())));
        assert!(headers.contains(&("editor-version", "vscode/1.107.0".to_string())));
        assert!(headers.contains(&("editor-plugin-version", "copilot-chat/0.35.0".to_string())));
        assert!(headers.contains(&("copilot-integration-id", "vscode-chat".to_string())));
        assert!(headers.contains(&("openai-intent", "conversation-edits".to_string())));
        assert!(headers.contains(&("x-github-api-version", "2026-06-01".to_string())));
    }
}
