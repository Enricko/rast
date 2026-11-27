//! Grok (X Premium, `grok`): the Responses wire with xAI's OAuth bearer.
//!
//! xAI's endpoint takes the same Responses shape Codex does, with three
//! differences this file makes: it wants no encrypted-reasoning `include`,
//! it names its reasoning levels `low`/`medium`/`high` (not Codex's
//! `xhigh`), and it takes an `effort` only when one was chosen. The request
//! URL needs no override — `https://api.x.ai/v1/responses` is exactly what
//! the generic `{base}/responses` builds from the kind's base URL — and the
//! model list comes from `GET {base}/models` with the OAuth bearer.

use serde_json::{json, Value};

use super::super::models::models_url;
use super::super::{
    agent, read_limited, refusal as generic_refusal, ChatRequest, Turn, Wire, LIST_TIMEOUT,
};
use crate::ai::{Kind, Protocol, MAX_MODELS, MAX_MODEL_NAME};

/// Models offered when xAI's catalog cannot be read (from the 9router
/// registry's xAI entry, newest first).
const MODELS: &[&str] = &["grok-4.7", "grok-4.6", "grok-4.5", "grok-code-fast-1"];

/// The protocol this plan speaks for `model` (fixed for nearly every plan;
/// Copilot may pick per model).
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Responses
}

/// Wire identity headers this plan needs beyond the generic ones: none. xAI
/// takes the OAuth access token as a plain `Authorization: Bearer` (the
/// generic `headers` already sends it) and asks for no CLI identity.
pub(crate) fn extra_headers(
    _model: &str,
    _session: &str,
    _account: &str,
) -> Vec<(&'static str, String)> {
    Vec::new()
}

/// A complete body for a plan whose request shape the generic builders in
/// [`super::super::request_body_for`] cannot make: none, the generic
/// Responses body plus [`tweak_body`]'s changes is what xAI takes.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

/// Last changes to a generic body: xAI's differences from the Codex CLI's
/// Responses body.
pub(crate) fn tweak_body(req: &ChatRequest, wire: Wire, body: &mut Value) {
    if wire.protocol != Protocol::Responses {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    // The generic builder is the Codex CLI's: it asks for encrypted reasoning
    // (Codex's `include`, useless to xAI and absent from the reference's grok
    // body) and always names an effort. xAI's endpoint uses its own default
    // when the person chose none, and the reference only sends `reasoning`
    // for a chosen effort (a model that does not reason would 400 otherwise).
    object.remove("include");
    if req.effort.is_none() {
        object.remove("reasoning");
    }
    // The reference sends both tool controls when tools exist, and neither
    // when they do not (xAI answered tool-less calls that carried them with a
    // 400, so the generic builder's "no tools, no controls" is kept).
    if object.contains_key("tools") {
        object.insert("parallel_tool_calls".into(), json!(true));
    }
    // xAI names its reasoning levels low/medium/high; "xhigh" is Codex's top
    // level, which the generic builder's mapping produces for `Max`.
    if let Some(reasoning) = body.get_mut("reasoning") {
        if reasoning.get("effort").and_then(Value::as_str) == Some("xhigh") {
            reasoning["effort"] = json!("high");
        }
    }
    // xAI takes the output cap on this wire; the generic Responses builder
    // (Codex's) leaves it to the provider's default.
    if let Some(max) = req.max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }
    // `prompt_cache_key` stays: the generic builder sets it from the request
    // session, which is exactly the cache-affinity hint the reference's grok
    // body sends (and asserts in its own tests); without it every turn's
    // prompt-cache hit is routing luck. It is a conversation id, not a token.
}

/// The plan's live model catalog (`GET {base}/models`, the shape xAI's API
/// uses): `data[].id`, chat models only.
pub(crate) fn catalog(base_url: &str, token: &str, _account: &str) -> Result<Vec<String>, String> {
    let response = agent(LIST_TIMEOUT)
        .get(models_url(Kind::Grok, base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("cannot reach xAI: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), 4 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(grok_refusal(status, &body));
    }
    parse_models(&body)
}

/// xAI's model list: `data[]` of `{id, …}`. The list also serves image,
/// video and embedding models the picker cannot chat with; those are left
/// out (the reference's `isChatModel` heuristic).
pub(crate) fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "xAI sent a model list RAST cannot read".to_string())?;
    let list = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("xAI sent no model list")?;
    let mut out: Vec<String> = Vec::new();
    for entry in list {
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_MODEL_NAME)
        else {
            continue;
        };
        if !is_chat_model(id) || out.iter().any(|model| model == id) {
            continue;
        }
        out.push(id.to_string());
    }
    if out.is_empty() {
        return Err("xAI offered no chat models".into());
    }
    out.truncate(MAX_MODELS);
    Ok(out)
}

/// Whether a model id is one to chat with: xAI's list also serves image,
/// video and embedding models (`grok-2-image-…`, `grok-imagine-…`, …).
fn is_chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !["imagine", "image-", "video", "embed"]
        .iter()
        .any(|word| id.contains(word))
}

/// Models to keep when the catalog cannot be read.
pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// Where generation goes when the generic URL is wrong: nowhere — the
/// generic `{base}/responses` is xAI's endpoint for every configured base
/// URL, so the override stays unused.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

/// The plan's own refusal wording (none: the generic one).
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    let what = match status {
        401 => "xAI refused the sign-in; sign in again under AI Agent settings → Providers",
        403 => {
            "your X plan does not include the API OAuth entitlement; an X Premium or xAI subscription with API access is required"
        }
        429 => "your Grok plan's rate limit is used up for now",
        _ => return None,
    };
    Some(with_said(what, status, body))
}

/// What xAI said about a refusal, in its own terms, appended to `what`.
fn with_said(what: &str, status: u16, body: &str) -> String {
    let said = generic_refusal(status, body);
    let said = said.split_once("): ").map(|(_, said)| said).unwrap_or("");
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

/// The catalog's refusal wording (the model-list fetch has no plan hint of
/// its own; 401/403 still say to sign in again).
fn grok_refusal(status: u16, body: &str) -> String {
    refusal(status, body).unwrap_or_else(|| generic_refusal(status, body))
}

#[cfg(test)]
mod tests {
    use super::super::super::{chat_url as generic_chat_url, request_body_for};
    use super::*;
    use crate::ai::provider::{ToolSpec, Turn};
    use crate::ai::Effort;
    use serde_json::json;

    fn request(effort: Option<Effort>, max_output_tokens: Option<i64>) -> ChatRequest {
        ChatRequest {
            kind: Kind::Grok,
            base_url: "https://api.x.ai/v1".into(),
            api_key: None,
            model: "grok-4.7".into(),
            system: "be helpful".into(),
            messages: vec![Turn::User("hello".into())],
            tools: Vec::new(),
            temperature: None,
            max_output_tokens,
            effort,
            session: "conversation-1".into(),
            account: String::new(),
        }
    }

    fn request_body(req: &ChatRequest) -> Value {
        request_body_for(
            req,
            Wire {
                protocol: Protocol::Responses,
                reasoning_back: false,
                usage_option: true,
                effort: req.effort,
            },
        )
    }

    /// The Responses endpoint is fixed for every model.
    #[test]
    fn every_model_speaks_responses() {
        for model in ["grok-4.7", "grok-4.6", "grok-code-fast-1", "anything"] {
            assert_eq!(protocol_for(model), Protocol::Responses);
        }
        // The generic URL already is xAI's endpoint, from the default base.
        assert_eq!(chat_url("https://api.x.ai/v1"), None);
        assert_eq!(
            generic_chat_url(Kind::Grok, "https://api.x.ai/v1", Protocol::Responses),
            "https://api.x.ai/v1/responses"
        );
    }

    /// xAI's additions and removals on the generic Responses body.
    #[test]
    fn the_body_is_xais_responses_shape() {
        let req = request(Some(Effort::High), Some(1_000));
        let body = request_body(&req);
        assert_eq!(body["model"], "grok-4.7");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["max_output_tokens"], 1_000);
        // Codex's encrypted-reasoning include is not part of xAI's body.
        assert!(body.get("include").is_none(), "{body}");
        // The session is the prompt-cache hint, as the reference sends it.
        assert_eq!(body["prompt_cache_key"], "conversation-1");
        // No tools: no tool controls either (xAI 400s on tool-less calls
        // that carry them).
        assert!(body.get("tools").is_none(), "{body}");
        assert!(body.get("tool_choice").is_none(), "{body}");
        assert!(body.get("parallel_tool_calls").is_none(), "{body}");
    }

    /// Without a chosen effort xAI keeps its own default; Codex's top level
    /// is lowered to xAI's.
    #[test]
    fn the_effort_is_xais_own() {
        let body = request_body(&request(None, None));
        assert!(body.get("reasoning").is_none(), "{body}");
        assert!(body.get("max_output_tokens").is_none(), "{body}");
        let body = request_body(&request(Some(Effort::Max), None));
        assert_eq!(body["reasoning"]["effort"], "high", "{body}");
        let body = request_body(&request(Some(Effort::Low), None));
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    /// Tools arrive with both controls, as the reference sends them.
    #[test]
    fn tools_come_with_their_controls() {
        let mut req = request(Some(Effort::Medium), None);
        req.tools = vec![ToolSpec {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object"}),
        }];
        let body = request_body(&req);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["tools"][0]["name"], "bash");
    }

    /// The model list keeps chat models in xAI's order, once each.
    #[test]
    fn the_model_list_is_read() {
        let body = json!({"data": [
            {"id": "grok-4.7", "object": "model"},
            {"id": "grok-code-fast-1"},
            {"id": "grok-2-image-1212"},
            {"id": "grok-imagine-video"},
            {"id": "grok-embed-1"},
            {"id": "grok-4.7"},
            {"id": ""},
            {"name": "no id"}
        ]})
        .to_string();
        assert_eq!(
            parse_models(&body).unwrap(),
            vec!["grok-4.7".to_string(), "grok-code-fast-1".to_string()]
        );
        assert!(parse_models("not json").is_err());
        assert!(parse_models(r#"{"object": "list"}"#).is_err());
        assert!(parse_models(r#"{"data": [{"id": "grok-imagine-video"}]}"#).is_err());
        assert_eq!(
            fallback_models(),
            &["grok-4.7", "grok-4.6", "grok-4.5", "grok-code-fast-1"]
        );
    }

    /// Refusals name what the X plan is missing, and hand the rest to the
    /// generic wording.
    #[test]
    fn refusals_say_what_to_do() {
        assert!(refusal(401, "").unwrap().contains("sign in again"));
        assert!(refusal(403, "").unwrap().contains("API OAuth entitlement"));
        assert!(refusal(429, "").unwrap().contains("limit"));
        assert!(refusal(401, r#"{"error":"expired"}"#)
            .unwrap()
            .contains("expired"));
        assert_eq!(refusal(500, ""), None);
    }
}
