//! Plan (signed-in subscription) wire details, one file per plan.
//!
//! Every hook below is dispatched from this file; a provider agent fills its
//! own `plans/<x>.rs` and touches nothing else. The arms keep the default
//! for the 27 API-key kinds harmless: the kind's own protocol, no extra
//! headers or bodies, no catalog (so [`fallback_models`] is used) and the
//! generic generation URL.

use serde_json::Value;

use super::{ChatRequest, Turn, Wire};
use crate::ai::{Kind, Protocol};

mod antigravity;
mod claude;
pub(crate) mod codex;
mod copilot;
mod gemini_cli;
mod grok;
mod iflow;
mod kimi;
mod qwen;

/// The protocol `kind` speaks for `model` (Copilot routes each model to the
/// endpoint it lists; every other plan uses its kind's protocol).
pub fn protocol_for(kind: Kind, model: &str) -> Protocol {
    match kind {
        Kind::Codex => codex::protocol_for(model),
        Kind::Claude => claude::protocol_for(model),
        Kind::Grok => grok::protocol_for(model),
        Kind::Copilot => copilot::protocol_for(model),
        Kind::Antigravity => antigravity::protocol_for(model),
        Kind::GeminiCli => gemini_cli::protocol_for(model),
        Kind::QwenPlan => qwen::protocol_for(model),
        Kind::IFlow => iflow::protocol_for(model),
        Kind::KimiPlan => kimi::protocol_for(model),
        _ => kind.protocol(),
    }
}

/// Headers a plan's wire wants beyond the generic ones, which carry the
/// access token: Codex CLI identity, Claude Code's betas, Copilot's editor,
/// Google's project. `account` is `signed_in_token`'s account (Codex's
/// ChatGPT account id, Google's project id, Copilot's login, "" otherwise).
pub fn extra_headers(
    kind: Kind,
    model: &str,
    session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    match kind {
        Kind::Codex => codex::extra_headers(model, session, account),
        Kind::Claude => claude::extra_headers(model, session, account),
        Kind::Grok => grok::extra_headers(model, session, account),
        Kind::Copilot => copilot::extra_headers(model, session, account),
        Kind::Antigravity => antigravity::extra_headers(model, session, account),
        Kind::GeminiCli => gemini_cli::extra_headers(model, session, account),
        Kind::QwenPlan => qwen::extra_headers(model, session, account),
        Kind::IFlow => iflow::extra_headers(model, session, account),
        Kind::KimiPlan => kimi::extra_headers(model, session, account),
        _ => Vec::new(),
    }
}

/// A complete body for a plan whose request shape the generic builders in
/// [`super::request_body_for`] cannot make (none: build it generically).
pub fn override_body(kind: Kind, req: &ChatRequest, wire: Wire, turns: &[Turn]) -> Option<Value> {
    match kind {
        Kind::Codex => codex::override_body(req, wire, turns),
        Kind::Claude => claude::override_body(req, wire, turns),
        Kind::Grok => grok::override_body(req, wire, turns),
        Kind::Copilot => copilot::override_body(req, wire, turns),
        Kind::Antigravity => antigravity::override_body(req, wire, turns),
        Kind::GeminiCli => gemini_cli::override_body(req, wire, turns),
        Kind::QwenPlan => qwen::override_body(req, wire, turns),
        Kind::IFlow => iflow::override_body(req, wire, turns),
        Kind::KimiPlan => kimi::override_body(req, wire, turns),
        _ => None,
    }
}

/// Last changes to a generic body (a field the plan's backend wants
/// differently).
pub fn tweak_body(kind: Kind, req: &ChatRequest, wire: Wire, body: &mut Value) {
    match kind {
        Kind::Codex => codex::tweak_body(req, wire, body),
        Kind::Claude => claude::tweak_body(req, wire, body),
        Kind::Grok => grok::tweak_body(req, wire, body),
        Kind::Copilot => copilot::tweak_body(req, wire, body),
        Kind::Antigravity => antigravity::tweak_body(req, wire, body),
        Kind::GeminiCli => gemini_cli::tweak_body(req, wire, body),
        Kind::QwenPlan => qwen::tweak_body(req, wire, body),
        Kind::IFlow => iflow::tweak_body(req, wire, body),
        Kind::KimiPlan => kimi::tweak_body(req, wire, body),
        _ => {}
    }
}

/// The plan's live model catalog (blocking HTTP; used at sign-in and by the
/// admin "Load models" button).
pub fn catalog(
    kind: Kind,
    base_url: &str,
    token: &str,
    account: &str,
) -> Result<Vec<String>, String> {
    match kind {
        Kind::Codex => codex::catalog(base_url, token, account),
        Kind::Claude => claude::catalog(base_url, token, account),
        Kind::Grok => grok::catalog(base_url, token, account),
        Kind::Copilot => copilot::catalog(base_url, token, account),
        Kind::Antigravity => antigravity::catalog(base_url, token, account),
        Kind::GeminiCli => gemini_cli::catalog(base_url, token, account),
        Kind::QwenPlan => qwen::catalog(base_url, token, account),
        Kind::IFlow => iflow::catalog(base_url, token, account),
        Kind::KimiPlan => kimi::catalog(base_url, token, account),
        _ => Err(format!("{} is not a signed-in plan", kind.label())),
    }
}

/// Models a plan offers when its catalog cannot be read (empty: none).
pub fn fallback_models(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Codex => codex::fallback_models(),
        Kind::Claude => claude::fallback_models(),
        Kind::Grok => grok::fallback_models(),
        Kind::Copilot => copilot::fallback_models(),
        Kind::Antigravity => antigravity::fallback_models(),
        Kind::GeminiCli => gemini_cli::fallback_models(),
        Kind::QwenPlan => qwen::fallback_models(),
        Kind::IFlow => iflow::fallback_models(),
        Kind::KimiPlan => kimi::fallback_models(),
        _ => &[],
    }
}

/// Where a plan's generation goes when the generic URLs are wrong (Cloud
/// Code's `{base}/v1internal:streamGenerateContent?alt=sse`).
pub fn chat_url(kind: Kind, base_url: &str) -> Option<String> {
    match kind {
        Kind::Codex => codex::chat_url(base_url),
        Kind::Claude => claude::chat_url(base_url),
        Kind::Grok => grok::chat_url(base_url),
        Kind::Copilot => copilot::chat_url(base_url),
        Kind::Antigravity => antigravity::chat_url(base_url),
        Kind::GeminiCli => gemini_cli::chat_url(base_url),
        Kind::QwenPlan => qwen::chat_url(base_url),
        Kind::IFlow => iflow::chat_url(base_url),
        Kind::KimiPlan => kimi::chat_url(base_url),
        _ => None,
    }
}

/// A plan's own refusal wording (none: the generic one).
pub(crate) fn refusal(kind: Kind, status: u16, body: &str) -> Option<String> {
    match kind {
        Kind::Codex => codex::refusal(status, body),
        Kind::Claude => claude::refusal(status, body),
        Kind::Grok => grok::refusal(status, body),
        Kind::Copilot => copilot::refusal(status, body),
        Kind::Antigravity => antigravity::refusal(status, body),
        Kind::GeminiCli => gemini_cli::refusal(status, body),
        Kind::QwenPlan => qwen::refusal(status, body),
        Kind::IFlow => iflow::refusal(status, body),
        Kind::KimiPlan => kimi::refusal(status, body),
        _ => None,
    }
}

/// Whether `model` is one of Codex's "lite" models (tools and instructions
/// as input items): only Codex has them, so the generic Responses builder
/// asks here.
pub(crate) fn model_is_lite(kind: Kind, model: &str) -> bool {
    match kind {
        Kind::Codex => codex::codex_lite(model),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{ChatRequest, StreamParser, Wire};
    use super::*;

    fn request(kind: Kind) -> ChatRequest {
        ChatRequest {
            kind,
            base_url: String::new(),
            api_key: None,
            model: "any-model".into(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_output_tokens: None,
            effort: None,
            session: String::new(),
            account: String::new(),
        }
    }

    #[test]
    fn plans_speak_their_kind_protocol() {
        for (kind, protocol) in [
            (Kind::Codex, Protocol::Responses),
            (Kind::Claude, Protocol::Anthropic),
            (Kind::Grok, Protocol::Responses),
            (Kind::Copilot, Protocol::Chat),
            (Kind::Antigravity, Protocol::CloudCode),
            (Kind::GeminiCli, Protocol::CloudCode),
            (Kind::QwenPlan, Protocol::Chat),
            (Kind::IFlow, Protocol::Chat),
            (Kind::KimiPlan, Protocol::Chat),
        ] {
            assert_eq!(protocol_for(kind, "any-model"), protocol, "{kind:?}");
        }
        // Keyed kinds keep the protocol their own kind says.
        for kind in [Kind::OpenAi, Kind::Anthropic, Kind::OpenCode, Kind::Gemini] {
            assert_eq!(protocol_for(kind, "any-model"), kind.protocol());
        }
    }

    #[test]
    fn codex_identity_goes_through_the_dispatch() {
        // stream_chat merges these with the generic headers; Codex adds the
        // CLI identity, the account id and, for lite models, its header.
        let headers = extra_headers(Kind::Codex, "gpt-5.6-sol", "conv-1", "acc-1");
        let has = |name: &str, value: &str| headers.iter().any(|(n, v)| *n == name && v == value);
        assert!(has("originator", "codex_cli_rs"));
        assert!(has("ChatGPT-Account-ID", "acc-1"));
        assert!(has("session_id", "conv-1"));
        assert!(!has("x-openai-internal-codex-responses-lite", "true"));
        let lite = extra_headers(Kind::Codex, "gpt-6.1-sol", "", "");
        assert!(lite.iter().any(
            |(name, value)| *name == "x-openai-internal-codex-responses-lite" && value == "true"
        ));
        // A keyed kind has no identity headers of its own.
        assert!(extra_headers(Kind::OpenAi, "gpt-6.1-sol", "conv-1", "acc-1").is_empty());
    }

    #[test]
    fn keyed_kinds_keep_the_generic_wire() {
        let req = request(Kind::OpenAi);
        let wire = Wire::of(req.kind);
        assert!(override_body(req.kind, &req, wire, &[]).is_none());
        let mut body = json!({"model": "gpt-x", "stream": true});
        tweak_body(req.kind, &req, wire, &mut body);
        assert_eq!(body, json!({"model": "gpt-x", "stream": true}));
        assert_eq!(fallback_models(req.kind), &[] as &[&str]);
        assert!(catalog(req.kind, "", "", "").is_err());
        assert!(chat_url(req.kind, "https://example.invalid").is_none());
        assert!(refusal(req.kind, 401, "{}").is_none());
        assert!(!model_is_lite(req.kind, "gpt-6.1-sol"));
    }

    #[test]
    fn codex_lite_bodies_are_asked_for_generically() {
        assert!(model_is_lite(Kind::Codex, "gpt-6.1-sol"));
        assert!(!model_is_lite(Kind::Codex, "gpt-5.6-terra"));
        // The live catalog is read at sign-in; the static list is there for
        // when it cannot be.
        assert!(!fallback_models(Kind::Codex).is_empty());
    }

    #[test]
    fn cloud_code_goes_through_its_own_wire() {
        // The Google wire fills in the envelope and the events; the dispatch
        // must reach it from the generic paths either way.
        let mut parser = StreamParser::for_protocol(Protocol::CloudCode);
        assert!(parser.take_calls().is_empty());
        assert!(parser.usage.is_empty());
        assert!(request_body_shape_for_cloud_code());
        // A plan supplies the Cloud Code generation URL itself, never a
        // made-up generic one.
        assert_eq!(
            chat_url(Kind::Antigravity, "https://cloudcode-pa.googleapis.com"),
            antigravity::chat_url("https://cloudcode-pa.googleapis.com")
        );
    }

    /// A Cloud Code request goes through `request_body_for`'s dispatch.
    fn request_body_shape_for_cloud_code() -> bool {
        let req = request(Kind::Antigravity);
        let wire = Wire {
            protocol: Protocol::CloudCode,
            ..Wire::of(req.kind)
        };
        super::super::request_body_for(&req, wire).is_object()
    }
}
