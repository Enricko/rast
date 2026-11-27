//! Model providers' wire protocols, streamed as server-sent events and read
//! on a blocking thread (`ureq`):
//!
//! * OpenAI-compatible chat completions (`POST {base}/chat/completions`):
//!   OpenAI, DeepSeek, OpenRouter, Ollama and custom endpoints. Reasoning
//!   models stream their thinking as `reasoning_content` (DeepSeek) or
//!   `reasoning` (OpenRouter); usage comes in the last chunk.
//! * Anthropic Messages (`POST {base}/messages`): text and thinking deltas,
//!   input tokens in `message_start`, output tokens in `message_delta`.
//!
//! * OpenAI Responses as ChatGPT's Codex backend takes it
//!   (`POST {base}/responses`, a signed-in plan; see [`super::signin`]):
//!   instructions apart from the input, function calls as items, reasoning
//!   summaries, usage in `response.completed`. Shaped as 9router sends it,
//!   including Codex's "lite" models, which take their tools and
//!   instructions as the first input items.
//!
//! Gateways that serve each model on its own protocol (OpenCode: Claude and
//! Qwen on Messages, the rest on chat completions, which translate GPT and
//! Gemini) say which in their model list; RAST reads it once in a while and
//! picks the protocol per model (see [`Kind::routes_by_model`]).
//!
//! Both carry tools: the model asks for calls (streamed as pieces of JSON),
//! the harness answers each with a result in the next request.
//!
//! Keys only go to the provider's own base URL; redirects are not followed.

//! Plan (signed-in subscription) kinds and Google Cloud Code have modules
//! of their own: [`plans`] decides a plan's wire details per request and
//! [`cloud_code`] shapes Google's `v1internal` requests and events.

mod anthropic;
mod chat;
pub mod cloud_code;
mod gateway;
mod models;
pub mod plans;
mod responses;
mod stream;
#[cfg(test)]
mod tests;

use std::{
    io::{BufRead, BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde_json::Value;

use super::{Effort, Kind, Protocol};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Until the provider starts answering (reasoning models think first).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
/// A whole reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Anthropic needs a maximum; enough for long answers.
const ANTHROPIC_DEFAULT_MAX_TOKENS: i64 = 16_000;

/// A tool the model may call: its name, what it does and its arguments'
/// JSON schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A call the model asked for (`input`: its arguments; a JSON string when
/// they were not valid JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
}

/// One turn of a conversation as providers see it.
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    User(String),
    Assistant {
        reasoning: String,
        text: String,
        calls: Vec<ToolCall>,
    },
    /// What the calls of the assistant turn before it returned.
    Results(Vec<ToolResult>),
}

impl Turn {
    /// About how much it weighs (for estimates and history budgets).
    pub fn chars(&self) -> usize {
        match self {
            Self::User(text) => text.len(),
            Self::Assistant {
                reasoning,
                text,
                calls,
            } => {
                reasoning.len()
                    + text.len()
                    + calls
                        .iter()
                        .map(|c| c.name.len() + c.input.to_string().len())
                        .sum::<usize>()
            }
            Self::Results(results) => results.iter().map(|r| r.output.len() + 20).sum(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub kind: Kind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub system: String,
    pub messages: Vec<Turn>,
    /// Empty: the model may not call tools (calls in the history are then
    /// told as text).
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<i64>,
    /// How hard the model thinks (none: the provider's default).
    pub effort: Option<Effort>,
    /// A stable id for the conversation, for gateways that ask for one
    /// (OpenCode Go refuses requests without it); "" none.
    pub session: String,
    /// The signed-in account, for providers that want it with each
    /// request (ChatGPT's account id); "" none.
    pub account: String,
}

/// How one request goes out: the protocol and its details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    pub protocol: Protocol,
    /// Send the model's reasoning back with its tool calls (`reasoning_content`).
    pub reasoning_back: bool,
    /// Ask for usage in the stream (`stream_options`); a few servers refuse it.
    pub usage_option: bool,
    /// The effort asked for (lowered when a model refuses it).
    pub effort: Option<Effort>,
}

impl Wire {
    /// What a kind uses for every model.
    pub fn of(kind: Kind) -> Self {
        Self {
            protocol: kind.protocol(),
            reasoning_back: kind.wants_reasoning_back(),
            usage_option: true,
            effort: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

/// Tokens of one reply as the provider reports them. `input` is all input;
/// `cache_read` and `cache_write` are the parts of it read back from the
/// provider's prompt cache and written to it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input <= 0 && self.output <= 0
    }
}

/// How a streamed reply ended.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    pub usage: Usage,
    /// Calls the model asked for (none when it was stopped early).
    pub calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatError {
    pub message: String,
    /// The provider or model refused the tools (try again without them).
    pub tools_refused: bool,
}

impl From<String> for ChatError {
    fn from(message: String) -> Self {
        Self {
            message,
            tools_refused: false,
        }
    }
}

impl From<&str> for ChatError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// Make turns fit what providers accept: the history starts with the person,
/// turns of one role are joined, empty replies are left out, every call has
/// a result, and without tools calls and results are told as text.
pub fn normalize(turns: Vec<Turn>, with_tools: bool) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::with_capacity(turns.len());
    for turn in turns {
        let turn = match turn {
            Turn::Assistant {
                reasoning,
                mut text,
                calls,
            } if !with_tools && !calls.is_empty() => {
                for call in &calls {
                    let input: String = call.input.to_string().chars().take(600).collect();
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&format!("[I used the tool {} with {input}]", call.name));
                }
                Turn::Assistant {
                    reasoning,
                    text,
                    calls: Vec::new(),
                }
            }
            Turn::Results(results) if !with_tools => Turn::User(
                results
                    .iter()
                    .map(|r| {
                        let output: String = r.output.chars().take(4000).collect();
                        format!(
                            "[{} {}: {output}]",
                            r.name,
                            if r.is_error { "failed" } else { "returned" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            ),
            other => other,
        };
        match (out.last_mut(), turn) {
            (_, Turn::Assistant { text, calls, .. })
                if text.trim().is_empty() && calls.is_empty() => {}
            (Some(Turn::User(last)), Turn::User(text)) => {
                last.push_str("\n\n");
                last.push_str(&text);
            }
            (
                Some(Turn::Assistant {
                    reasoning: last_reasoning,
                    text: last_text,
                    calls: last_calls,
                }),
                Turn::Assistant {
                    reasoning,
                    text,
                    calls,
                },
            ) if last_calls.is_empty() => {
                if !reasoning.is_empty() {
                    if !last_reasoning.is_empty() {
                        last_reasoning.push_str("\n\n");
                    }
                    last_reasoning.push_str(&reasoning);
                }
                if !text.is_empty() {
                    if !last_text.is_empty() {
                        last_text.push_str("\n\n");
                    }
                    last_text.push_str(&text);
                }
                *last_calls = calls;
            }
            (Some(Turn::Results(last)), Turn::Results(results)) => last.extend(results),
            (_, turn) => out.push(turn),
        }
    }
    // Pair calls with results: answered in the next turn, nothing else.
    let mut paired: Vec<Turn> = Vec::with_capacity(out.len());
    let mut iter = out.into_iter().peekable();
    while let Some(turn) = iter.next() {
        match turn {
            Turn::Assistant {
                calls,
                reasoning,
                text,
            } if !calls.is_empty() => {
                let mut given = match iter.peek() {
                    Some(Turn::Results(_)) => match iter.next() {
                        Some(Turn::Results(results)) => results,
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                let results: Vec<ToolResult> = calls
                    .iter()
                    .map(|call| match given.iter().position(|r| r.id == call.id) {
                        Some(at) => given.remove(at),
                        None => ToolResult {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            output: "Not run.".into(),
                            is_error: true,
                        },
                    })
                    .collect();
                paired.push(Turn::Assistant {
                    reasoning,
                    text,
                    calls,
                });
                paired.push(Turn::Results(results));
            }
            Turn::Results(_) => {} // answers no call
            other => paired.push(other),
        }
    }
    while paired.first().is_some_and(|t| !matches!(t, Turn::User(_))) {
        paired.remove(0);
    }
    paired
}

/// A provider's HTTP agent: no redirects (a key must not follow one), the
/// timeouts above.
pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .user_agent(concat!("RAST/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// The headers every request carries: the protocol's version marker, the
/// kind's authentication (an API key or a plan's access token), OpenRouter's
/// attribution and a gateway's session id.
pub(crate) fn headers(
    kind: Kind,
    protocol: Protocol,
    api_key: Option<&str>,
    session: &str,
) -> Vec<(&'static str, String)> {
    let mut out = vec![("Content-Type", "application/json".to_string())];
    if protocol == Protocol::Anthropic {
        out.push(("anthropic-version", "2023-06-01".into()));
    }
    if let Some(key) = api_key {
        match kind {
            Kind::Anthropic => out.push(("x-api-key", key.to_string())),
            // Anthropic-compatible servers read one or the other.
            Kind::CustomAnthropic => {
                out.push(("x-api-key", key.to_string()));
                out.push(("Authorization", format!("Bearer {key}")));
            }
            _ => out.push(("Authorization", format!("Bearer {key}"))),
        }
    }
    if kind == Kind::OpenRouter {
        out.push(("HTTP-Referer", "https://github.com/Enricko/rast".into()));
        out.push(("X-Title", "RAST".into()));
    }
    if kind.routes_by_model() && !session.is_empty() {
        out.push(("x-opencode-session", session.to_string()));
    }
    out
}

/// Where replies are asked for. A plan may send them elsewhere (Cloud
/// Code's `v1internal:streamGenerateContent`); without a plan URL the
/// generic paths below apply.
fn chat_url(kind: Kind, base_url: &str, protocol: Protocol) -> String {
    if let Some(url) = plans::chat_url(kind, base_url) {
        return url;
    }
    match (kind.routes_by_model(), protocol) {
        (true, Protocol::Anthropic) => format!("{base_url}/anthropic/v1/messages"),
        (true, Protocol::Chat) => format!("{base_url}/openai/v1/chat/completions"),
        (false, Protocol::Anthropic) => format!("{base_url}/messages"),
        (false, Protocol::Chat) => format!("{base_url}/chat/completions"),
        (_, Protocol::Responses) => format!("{base_url}/responses"),
        // Cloud Code plans fill this in through `plans::chat_url`.
        (_, Protocol::CloudCode) => String::new(),
    }
}

/// The request body for a streamed reply, as the kind sends it (a plan may
/// pick the protocol per model).
pub fn request_body(req: &ChatRequest) -> Value {
    request_body_for(req, wire_for(req))
}

/// The body of a streamed reply over `wire`: a plan's own builder when it
/// has one, else the protocol's generic one, then the plan's last tweaks.
pub fn request_body_for(req: &ChatRequest, wire: Wire) -> Value {
    let turns = normalize(req.messages.clone(), !req.tools.is_empty());
    let mut body = match plans::override_body(req.kind, req, wire, &turns) {
        Some(body) => body,
        None => match wire.protocol {
            Protocol::Responses => responses::body(req, wire, &turns),
            Protocol::Anthropic => anthropic::body(req, wire, &turns),
            Protocol::CloudCode => cloud_code::body(req, wire, &turns),
            Protocol::Chat => chat::body(req, wire, &turns),
        },
    };
    plans::tweak_body(req.kind, req, wire, &mut body);
    body
}

/// How `req` goes out unless a gateway says otherwise: the kind's protocol,
/// or the plan's pick for this model (Copilot may route to Responses).
fn wire_for(req: &ChatRequest) -> Wire {
    Wire {
        protocol: plans::protocol_for(req.kind, &req.model),
        ..Wire::of(req.kind)
    }
}

fn error_text(error: &Value) -> String {
    match error {
        Value::String(text) => text.clone(),
        _ => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    }
}

/// What a provider said when it refused (status and its message).
pub fn refusal(status: u16, body: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .map(error_text)
                .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let what = match status {
        401 | 403 => "the provider refused the API key",
        404 => "the provider does not know this model or address",
        429 => "the provider's rate limit or credit is used up",
        500..=599 => "the provider had an error",
        _ => "the provider refused the request",
    };
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
}

fn read_limited(body: ureq::Body, limit: u64) -> String {
    let mut text = String::new();
    let _ = body.into_reader().take(limit).read_to_string(&mut text);
    text
}

/// Whether a refusal is about the tools (the model or server has none).
pub fn refuses_tools(status: u16, body: &str) -> bool {
    matches!(status, 400 | 404 | 422) && body.to_ascii_lowercase().contains("tool")
}

/// Stream a reply. `on_delta` gets the text as it comes; returns the usage
/// the provider reported (zero when it reports none) and the calls it asked
/// for. Stops early, without an error, once `cancel` is set.
pub fn stream_chat(
    req: &ChatRequest,
    cancel: &AtomicBool,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<Outcome, ChatError> {
    if req.base_url.is_empty() {
        return Err("the provider has no address; set its base URL".into());
    }
    let mut wire = if req.kind.routes_by_model() {
        gateway::gateway_wire(req)
    } else {
        wire_for(req)
    };
    wire.effort = req.effort;
    let url = chat_url(req.kind, &req.base_url, wire.protocol);
    let response = loop {
        let body = request_body_for(req, wire).to_string();
        let mut request = agent(REPLY_TIMEOUT).post(&url);
        for (name, value) in headers(
            req.kind,
            wire.protocol,
            req.api_key.as_deref(),
            &req.session,
        ) {
            request = request.header(name, value);
        }
        // A plan's identity headers (the Codex CLI, Claude Code, Copilot's
        // editor); additive to the generic ones, which carry the token.
        for (name, value) in plans::extra_headers(req.kind, &req.model, &req.session, &req.account)
        {
            request = request.header(name, value);
        }
        request = request.header("Accept", "text/event-stream");
        let response = request
            .send(body.as_bytes())
            .map_err(|e| format!("cannot reach the provider at {}: {e}", req.base_url))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            break response;
        }
        let body = read_limited(response.into_body(), 16 * 1024);
        if wire.usage_option && refuses_usage_option(status, &body) {
            // Usage then comes in the last chunk anyway, or not at all.
            wire.usage_option = false;
            continue;
        }
        if let Some(effort) = wire.effort.filter(|_| refuses_effort(status, &body)) {
            // A model without that level (or without effort at all).
            wire.effort = effort.fallback();
            continue;
        }
        return Err(ChatError {
            tools_refused: !req.tools.is_empty() && refuses_tools(status, &body),
            message: plans::refusal(req.kind, status, &body)
                .unwrap_or_else(|| refusal(status, &body)),
        });
    };
    let mut parser = StreamParser::for_protocol(wire.protocol);
    let reader = BufReader::new(response.into_body().into_reader());
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Outcome {
                usage: parser.usage,
                calls: Vec::new(),
            });
        }
        let line = line.map_err(|e| format!("the provider's answer broke off: {e}"))?;
        for delta in parser.line(&line)? {
            on_delta(delta);
        }
        if parser.done && wire.protocol != Protocol::Chat {
            break;
        }
    }
    Ok(Outcome {
        usage: parser.usage,
        calls: parser.take_calls(),
    })
}

/// Whether a refusal is about `stream_options` (servers that do not know it).
pub fn refuses_usage_option(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422) && body.contains("stream_options")
}

/// Whether a refusal is about the effort asked for (`output_config.effort`,
/// `reasoning_effort` or `reasoning`): models without it, or without that
/// level.
pub fn refuses_effort(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422) && (body.contains("effort") || body.contains("reasoning"))
}

/// The models a signed-in plan offers: [`plans::catalog`] (blocking HTTP),
/// with its static fallback list when the catalog cannot be read.
pub fn list_plan_models(
    kind: Kind,
    base_url: &str,
    token: &str,
    account: &str,
) -> Result<Vec<String>, String> {
    let fallback = || {
        plans::fallback_models(kind)
            .iter()
            .map(|model| model.to_string())
            .take(super::MAX_MODELS)
            .collect::<Vec<_>>()
    };
    match plans::catalog(kind, base_url, token, account) {
        Ok(models) if !models.is_empty() => Ok(models),
        Ok(_) => Ok(fallback()),
        Err(error) if plans::fallback_models(kind).is_empty() => Err(error),
        Err(_) => Ok(fallback()),
    }
}

pub use super::prices::{Price, Prices};

pub use gateway::{parse_gateway_models, GatewayModel, Route};
pub use models::{list_models, parse_models, parse_prices, ModelList};
pub use plans::codex::{codex_lite, codex_refusal, list_codex_models, parse_codex_models};
pub use stream::StreamParser;
