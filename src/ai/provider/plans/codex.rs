//! ChatGPT's Codex plan: the OpenAI Responses wire as the Codex CLI sends
//! it — CLI identity headers, its "lite" models whose tools and
//! instructions are input items, its model list and its own refusal
//! wording. This is the plan that existed before the others and it keeps
//! behaving exactly as it did.

use serde_json::Value;

use super::super::models::models_url;
use super::super::{
    agent, read_limited, refusal as generic_refusal, ChatRequest, Turn, Wire, LIST_TIMEOUT,
};
use crate::ai::{Kind, Protocol, MAX_MODELS};

/// Models kept when ChatGPT's catalog cannot be read.
const MODELS: &[&str] = &["gpt-6.2-sol", "gpt-6-luna", "gpt-5.6-terra"];

/// Codex's models that take tools and instructions as input items, with
/// their own header (9router's "responses lite": GPT-6 Sol, Luna, Terra).
pub fn codex_lite(model: &str) -> bool {
    model.starts_with("gpt-6")
        && ["-sol", "-luna", "-terra"]
            .iter()
            .any(|m| model.contains(m))
}

/// What Codex's backend wants with every request, as the Codex CLI sends it.
pub(crate) fn codex_headers(
    model: &str,
    account: &str,
    session: &str,
) -> Vec<(&'static str, String)> {
    let version = crate::ai::signin::CODEX_VERSION;
    let mut out = vec![
        ("originator", "codex_cli_rs".to_string()),
        ("version", version.to_string()),
        ("User-Agent", format!("codex_cli_rs/{version}")),
    ];
    if !account.is_empty() {
        out.push(("ChatGPT-Account-ID", account.to_string()));
    }
    if !session.is_empty() {
        out.push(("session_id", session.to_string()));
    }
    if codex_lite(model) {
        out.push(("x-openai-internal-codex-responses-lite", "true".into()));
    }
    out
}

/// The models ChatGPT's Codex offers a signed-in plan
/// (`GET {base}/models?client_version=…`), in the order it gives them.
pub fn list_codex_models(
    base_url: &str,
    token: &str,
    account: &str,
) -> Result<Vec<String>, String> {
    let mut request = agent(LIST_TIMEOUT)
        .get(models_url(Kind::Codex, base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    for (name, value) in codex_headers("", account, "") {
        request = request.header(name, value);
    }
    let response = request
        .call()
        .map_err(|e| format!("cannot reach ChatGPT's Codex: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), 4 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(codex_refusal(status, &body));
    }
    parse_codex_models(&body)
}

/// Codex's model list: `models` (or `data`) of `{slug, display_name,
/// visibility}`; models it hides from pickers, and image or speech models,
/// are left out.
pub fn parse_codex_models(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "ChatGPT's Codex sent a model list RAST cannot read".to_string())?;
    let list = value
        .get("models")
        .or_else(|| value.get("data"))
        .and_then(Value::as_array)
        .ok_or("ChatGPT's Codex sent no model list")?;
    let mut out: Vec<String> = Vec::new();
    for model in list {
        let Some(id) = model
            .get("slug")
            .or_else(|| model.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let visibility = model
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or("list");
        let lower = id.to_ascii_lowercase();
        if visibility != "list"
            || ["image", "transcribe", "embed", "tts"]
                .iter()
                .any(|w| lower.contains(w))
        {
            continue;
        }
        if !out.iter().any(|m| m == id) {
            out.push(id.to_string());
        }
    }
    out.truncate(MAX_MODELS);
    Ok(out)
}

/// What ChatGPT's Codex said when it refused, in its own terms.
pub fn codex_refusal(status: u16, body: &str) -> String {
    let said = generic_refusal(status, body);
    let said = said.split_once("): ").map(|(_, s)| s).unwrap_or("");
    let what = match status {
        401 | 403 => "ChatGPT refused the sign-in; sign in again under AI Agent settings → Providers".to_string(),
        429 => "your ChatGPT plan's Codex limit is used up for now (see chatgpt.com/codex/settings/usage)".to_string(),
        404 => "ChatGPT's Codex does not offer this model to your plan".to_string(),
        _ => return generic_refusal(status, body),
    };
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

// ------------------------------------------------------------- plan hooks

/// Codex is the Responses wire for every model.
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Responses
}

/// The Codex CLI's identity with every request, account id included.
pub(crate) fn extra_headers(
    model: &str,
    session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    codex_headers(model, account, session)
}

/// Its body comes from the generic Responses builder.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, _body: &mut Value) {}

/// `GET {base}/models?client_version=…` with the CLI identity.
pub(crate) fn catalog(base_url: &str, token: &str, account: &str) -> Result<Vec<String>, String> {
    list_codex_models(base_url, token, account)
}

pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// `{base}/responses`, built generically.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

/// "ChatGPT refused the sign-in", "Codex limit is used up", …
pub(crate) fn refusal(status: u16, body: &str) -> Option<String> {
    Some(codex_refusal(status, body))
}
