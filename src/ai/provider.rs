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

use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read},
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock, Mutex,
    },
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use super::{Effort, Kind, Protocol};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Until the provider starts answering (reasoning models think first).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
/// A whole reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Anthropic needs a maximum; enough for long answers.
const ANTHROPIC_DEFAULT_MAX_TOKENS: i64 = 16_000;
/// How long a gateway's model routes are used before they are read again.
const ROUTES_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// …and how long before trying again when they could not be read.
const ROUTES_RETRY: Duration = Duration::from_secs(60);

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

/// Anthropic's usage: input in three parts, which RAST adds up.
fn anthropic_usage(usage: &Value, into: &mut Usage) {
    let part = |key: &str| usage.get(key).and_then(Value::as_i64);
    let (plain, read, write) = (
        part("input_tokens"),
        part("cache_read_input_tokens"),
        part("cache_creation_input_tokens"),
    );
    if plain.is_some() || read.is_some() || write.is_some() {
        into.cache_read = read.unwrap_or(0);
        into.cache_write = write.unwrap_or(0);
        into.input = plain.unwrap_or(0) + into.cache_read + into.cache_write;
    }
    if let Some(output) = part("output_tokens") {
        into.output = output;
    }
}

/// An OpenAI-style usage: cached input as OpenAI, DeepSeek, Kimi and the
/// gateways report it (all part of `prompt_tokens`).
fn chat_usage(usage: &Value, into: &mut Usage) {
    let first = |paths: &[&str]| {
        paths
            .iter()
            .find_map(|p| usage.pointer(p).and_then(Value::as_i64))
    };
    if let Some(input) = first(&["/prompt_tokens"]) {
        into.input = input;
    }
    if let Some(output) = first(&["/completion_tokens"]) {
        into.output = output;
    }
    if let Some(read) = first(&[
        "/prompt_tokens_details/cached_tokens",
        "/prompt_cache_hit_tokens",
        "/cached_tokens",
        "/cache_read_input_tokens",
    ]) {
        into.cache_read = read;
    }
    if let Some(write) = first(&[
        "/prompt_tokens_details/cache_write_tokens",
        "/prompt_tokens_details/cache_creation_tokens",
        "/cache_creation_input_tokens",
    ]) {
        into.cache_write = write;
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

/// Anthropic takes tool ids of letters, digits, `_` and `-` only.
fn anthropic_id(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "call".into()
    } else {
        clean
    }
}

fn anthropic_turns(turns: &[Turn]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(turns.len());
    for turn in turns {
        match turn {
            Turn::User(text) => match out.last_mut() {
                // After tool results: the same user turn.
                Some(last) if last["role"] == "user" && last["content"].is_array() => {
                    if let Some(blocks) = last["content"].as_array_mut() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                _ => out.push(json!({"role": "user", "content": text})),
            },
            Turn::Assistant { text, calls, .. } => {
                if calls.is_empty() {
                    out.push(json!({"role": "assistant", "content": text}));
                    continue;
                }
                let mut blocks = Vec::with_capacity(calls.len() + 1);
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for call in calls {
                    let input = if call.input.is_object() {
                        call.input.clone()
                    } else {
                        json!({})
                    };
                    blocks.push(json!({"type": "tool_use", "id": anthropic_id(&call.id), "name": call.name, "input": input}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Turn::Results(results) => {
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        let mut block = json!({"type": "tool_result", "tool_use_id": anthropic_id(&r.id), "content": r.output});
                        if r.is_error {
                            block["is_error"] = json!(true);
                        }
                        block
                    })
                    .collect();
                out.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    out
}

fn openai_turns(reasoning_back: bool, turns: &[Turn]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(turns.len());
    for turn in turns {
        match turn {
            Turn::User(text) => out.push(json!({"role": "user", "content": text})),
            Turn::Assistant {
                reasoning,
                text,
                calls,
            } => {
                let mut message = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    if text.is_empty() {
                        message["content"] = Value::Null;
                    }
                    message["tool_calls"] = calls
                        .iter()
                        .map(|call| {
                            let arguments = match &call.input {
                                Value::String(raw) => raw.clone(),
                                other => other.to_string(),
                            };
                            json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": arguments}})
                        })
                        .collect();
                }
                // Thinking models (DeepSeek, Kimi, GLM) want their
                // reasoning back once tools are in play.
                if reasoning_back && !reasoning.is_empty() {
                    message["reasoning_content"] = json!(reasoning);
                }
                out.push(message);
            }
            Turn::Results(results) => {
                for r in results {
                    let content = if r.is_error {
                        format!("Error: {}", r.output)
                    } else {
                        r.output.clone()
                    };
                    out.push(json!({"role": "tool", "tool_call_id": r.id, "content": content}));
                }
            }
        }
    }
    out
}

fn agent(timeout: Duration) -> ureq::Agent {
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

fn headers(
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

/// Where replies are asked for.
fn chat_url(kind: Kind, base_url: &str, protocol: Protocol) -> String {
    match (kind.routes_by_model(), protocol) {
        (true, Protocol::Anthropic) => format!("{base_url}/anthropic/v1/messages"),
        (true, Protocol::Chat) => format!("{base_url}/openai/v1/chat/completions"),
        (false, Protocol::Anthropic) => format!("{base_url}/messages"),
        (false, Protocol::Chat) => format!("{base_url}/chat/completions"),
        (_, Protocol::Responses) => format!("{base_url}/responses"),
    }
}

/// Codex's models that take tools and instructions as input items, with
/// their own header (9router's "responses lite": GPT-6 Sol, Luna, Terra).
pub fn codex_lite(model: &str) -> bool {
    model.starts_with("gpt-6")
        && ["-sol", "-luna", "-terra"]
            .iter()
            .any(|m| model.contains(m))
}

/// What Codex's backend wants with every request, as the Codex CLI sends it.
fn codex_headers(model: &str, account: &str, session: &str) -> Vec<(&'static str, String)> {
    let version = super::signin::CODEX_VERSION;
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

/// Where the model list is.
fn models_url(kind: Kind, base_url: &str) -> String {
    if kind == Kind::Codex {
        format!(
            "{base_url}/models?client_version={}",
            super::signin::CODEX_VERSION
        )
    } else if kind.routes_by_model() {
        format!("{base_url}/v1/models")
    } else if kind.protocol() == Protocol::Anthropic {
        format!("{base_url}/models?limit=1000")
    } else {
        format!("{base_url}/models")
    }
}

/// The request body for a streamed reply, as the kind sends every model.
pub fn request_body(req: &ChatRequest) -> Value {
    request_body_for(req, Wire::of(req.kind))
}

/// The request body for a streamed reply over `wire`.
pub fn request_body_for(req: &ChatRequest, wire: Wire) -> Value {
    let normalized = normalize(req.messages.clone(), !req.tools.is_empty());
    if wire.protocol == Protocol::Responses {
        return responses_body(req, wire, &normalized);
    }
    if wire.protocol == Protocol::Anthropic {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS),
            "messages": anthropic_turns(&normalized),
            "stream": true,
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(effort) = wire.effort {
            body["output_config"] = json!({"effort": effort.as_str()});
        }
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
                .collect();
        }
        return body;
    }
    let turns = openai_turns(wire.reasoning_back, &normalized);
    let mut messages = Vec::with_capacity(turns.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    messages.extend(turns);
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });
    if wire.usage_option {
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = wire.effort {
        let level = match effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            // OpenAI's own top level; elsewhere the highest common one.
            Effort::Max if req.kind == Kind::OpenAi => "xhigh",
            Effort::Max => "high",
        };
        if req.kind == Kind::OpenRouter {
            body["reasoning"] = json!({"effort": level});
        } else {
            body["reasoning_effort"] = json!(level);
        }
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
    }
    if let Some(max) = req.max_output_tokens {
        // OpenAI's newer models refuse `max_tokens`.
        let field = if req.kind == Kind::OpenAi {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[field] = json!(max);
    }
    body
}

/// Turns as Responses input items (no ids: nothing is stored upstream).
fn responses_input(turns: &[Turn]) -> Vec<Value> {
    let mut out = Vec::with_capacity(turns.len());
    for turn in turns {
        match turn {
            Turn::User(text) => out.push(json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}],
            })),
            Turn::Assistant { text, calls, .. } => {
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in calls {
                    let arguments = match &call.input {
                        Value::String(raw) => raw.clone(),
                        other => other.to_string(),
                    };
                    out.push(json!({
                        "type": "function_call", "call_id": call.id,
                        "name": call.name, "arguments": arguments,
                    }));
                }
            }
            Turn::Results(results) => {
                for r in results {
                    out.push(json!({
                        "type": "function_call_output", "call_id": r.id, "output": r.output,
                    }));
                }
            }
        }
    }
    out
}

/// A Responses request as Codex's backend takes it (9router's shape).
fn responses_body(req: &ChatRequest, wire: Wire, turns: &[Turn]) -> Value {
    let lite = codex_lite(&req.model);
    let level = match wire.effort {
        Some(Effort::Low) => "low",
        None | Some(Effort::Medium) => "medium",
        Some(Effort::High) => "high",
        Some(Effort::Max) if lite => "max",
        Some(Effort::Max) => "xhigh",
    };
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
        .collect();
    let instructions = if req.system.trim().is_empty() {
        "You are a helpful assistant."
    } else {
        req.system.as_str()
    };
    let mut input = Vec::with_capacity(turns.len() + 2);
    if lite {
        input.push(json!({"type": "additional_tools", "role": "developer", "tools": tools}));
        input.push(json!({
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": instructions}],
        }));
    }
    input.extend(responses_input(turns));
    if input.is_empty() {
        input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "..."}]}));
    }
    let mut body = json!({
        "model": req.model,
        "input": input,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if lite {
        body["instructions"] = json!("");
        body["tools"] = Value::Null;
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(false);
        body["reasoning"] = json!({"effort": level, "context": "all_turns"});
    } else {
        body["instructions"] = json!(instructions);
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        body["reasoning"] = json!({"effort": level, "summary": "auto"});
    }
    if !req.session.is_empty() {
        body["prompt_cache_key"] = json!(req.session);
    }
    body
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Reads a provider's event stream line by line.
#[derive(Debug)]
pub struct StreamParser {
    protocol: Protocol,
    pub usage: Usage,
    pub done: bool,
    /// Calls being streamed, by the provider's index.
    calls: BTreeMap<u64, PartialCall>,
}

impl StreamParser {
    pub fn new(kind: Kind) -> Self {
        Self::for_protocol(kind.protocol())
    }

    pub fn for_protocol(protocol: Protocol) -> Self {
        Self {
            protocol,
            usage: Usage::default(),
            done: false,
            calls: BTreeMap::new(),
        }
    }

    /// The calls the reply asked for, in order (ids made up where the
    /// provider sent none).
    pub fn take_calls(&mut self) -> Vec<ToolCall> {
        let mut used: Vec<String> = Vec::new();
        std::mem::take(&mut self.calls)
            .into_values()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty() || !c.arguments.is_empty())
            .map(|(n, c)| {
                let mut id = c.id;
                if id.is_empty() || used.contains(&id) {
                    let mut raw = [0u8; 6];
                    rand::RngCore::fill_bytes(&mut rand::rng(), &mut raw);
                    id = format!("call_{n}_{}", hex::encode(raw));
                }
                used.push(id.clone());
                let input = if c.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&c.arguments).unwrap_or(Value::String(c.arguments))
                };
                ToolCall {
                    id,
                    name: c.name,
                    input,
                }
            })
            .collect()
    }

    /// One line of the stream: the deltas it carries, or the provider's error.
    pub fn line(&mut self, line: &str) -> Result<Vec<Delta>, String> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(Vec::new()); // `event:` names, `:` comments, blank lines
        };
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(Vec::new());
        };
        if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
            return Err(error_text(error));
        }
        let mut out = Vec::new();
        if self.protocol == Protocol::Responses {
            self.responses_event(&event, &mut out)?;
            return Ok(out);
        }
        if self.protocol == Protocol::Anthropic {
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    anthropic_usage(&event["message"]["usage"], &mut self.usage);
                }
                Some("content_block_start") => {
                    let block = &event["content_block"];
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        self.calls.insert(
                            index,
                            PartialCall {
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: String::new(),
                            },
                        );
                    }
                }
                Some("content_block_delta") => {
                    let delta = &event["delta"];
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => push_text(&mut out, delta.get("text"), false),
                        Some("thinking_delta") => push_text(&mut out, delta.get("thinking"), true),
                        Some("input_json_delta") => {
                            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                            if let (Some(call), Some(part)) = (
                                self.calls.get_mut(&index),
                                delta.get("partial_json").and_then(Value::as_str),
                            ) {
                                call.arguments.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    // Totals so far; some gateways send the input here only.
                    anthropic_usage(&event["usage"], &mut self.usage);
                }
                Some("message_stop") => self.done = true,
                _ => {}
            }
            return Ok(out);
        }
        if let Some(delta) = event.pointer("/choices/0/delta") {
            push_text(&mut out, delta.get("reasoning_content"), true);
            push_text(&mut out, delta.get("reasoning"), true);
            push_text(&mut out, delta.get("content"), false);
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, piece) in calls.iter().enumerate() {
                    let index = piece
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(position as u64);
                    let call = self.calls.entry(index).or_default();
                    if let Some(id) = piece.get("id").and_then(Value::as_str) {
                        if call.id.is_empty() {
                            call.id = id.to_string();
                        }
                    }
                    let function = &piece["function"];
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if !call.name.ends_with(name) {
                            call.name.push_str(name);
                        }
                    }
                    match function.get("arguments") {
                        Some(Value::String(part)) => call.arguments.push_str(part),
                        Some(object @ Value::Object(_)) => call.arguments = object.to_string(),
                        _ => {}
                    }
                }
            }
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            chat_usage(usage, &mut self.usage);
        }
        Ok(out)
    }
}

impl StreamParser {
    /// One Responses event (`type` names it).
    fn responses_event(&mut self, event: &Value, out: &mut Vec<Delta>) -> Result<(), String> {
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => push_text(out, event.get("delta"), false),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                push_text(out, event.get("delta"), true)
            }
            // Summaries come in parts: keep them apart.
            "response.reasoning_summary_part.added"
                if event
                    .get("summary_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0 =>
            {
                out.push(Delta::Reasoning("\n\n".into()))
            }
            "response.output_item.added" | "response.output_item.done" => {
                let item = &event["item"];
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call = self.calls.entry(index).or_default();
                    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("");
                    if call.id.is_empty() {
                        call.id = text("call_id").to_string();
                    }
                    if call.name.is_empty() {
                        call.name = text("name").to_string();
                    }
                    // The finished item has all the arguments.
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        if !arguments.is_empty() {
                            call.arguments = arguments.to_string();
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(part) = event.get("delta").and_then(Value::as_str) {
                    self.calls
                        .entry(index)
                        .or_default()
                        .arguments
                        .push_str(part);
                }
            }
            "response.completed" => {
                let usage = &event["response"]["usage"];
                let part = |path: &str| usage.pointer(path).and_then(Value::as_i64);
                if let Some(input) = part("/input_tokens") {
                    self.usage.input = input;
                    self.usage.cache_read =
                        part("/input_tokens_details/cached_tokens").unwrap_or(0);
                }
                if let Some(output) = part("/output_tokens") {
                    self.usage.output = output;
                }
                self.done = true;
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                return Err(if error.is_null() {
                    "the reply failed".into()
                } else {
                    error_text(error)
                });
            }
            "response.incomplete" => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                return Err(format!("the reply stopped before it was done ({reason})"));
            }
            "error" => {
                return Err(event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the provider had an error")
                    .to_string())
            }
            _ => {}
        }
        Ok(())
    }
}

fn push_text(out: &mut Vec<Delta>, value: Option<&Value>, reasoning: bool) {
    if let Some(text) = value.and_then(Value::as_str).filter(|t| !t.is_empty()) {
        out.push(if reasoning {
            Delta::Reasoning(text.to_string())
        } else {
            Delta::Text(text.to_string())
        });
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
        gateway_wire(req)
    } else {
        Wire::of(req.kind)
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
        if req.kind == Kind::Codex {
            for (name, value) in codex_headers(&req.model, &req.account, &req.session) {
                request = request.header(name, value);
            }
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
            message: if req.kind == Kind::Codex {
                codex_refusal(status, &body)
            } else {
                refusal(status, &body)
            },
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

/// What ChatGPT's Codex said when it refused, in its own terms.
pub fn codex_refusal(status: u16, body: &str) -> String {
    let said = refusal(status, body);
    let said = said.split_once("): ").map(|(_, s)| s).unwrap_or("");
    let what = match status {
        401 | 403 => "ChatGPT refused the sign-in; sign in again under AI Agent settings → Providers".to_string(),
        429 => "your ChatGPT plan's Codex limit is used up for now (see chatgpt.com/codex/settings/usage)".to_string(),
        404 => "ChatGPT's Codex does not offer this model to your plan".to_string(),
        _ => return refusal(status, body),
    };
    if said.is_empty() {
        format!("{what} (HTTP {status})")
    } else {
        format!("{what} (HTTP {status}): {said}")
    }
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

pub use super::prices::{Price, Prices};

/// A provider's models and the prices its list gives for them.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelList {
    pub models: Vec<String>,
    pub prices: Prices,
}

/// The models a provider offers (`GET {base}/models`), sorted. Gateways
/// leave out what RAST cannot use: models that are not for chat, and free
/// ones only their own apps may call.
pub fn list_models(kind: Kind, base_url: &str, api_key: Option<&str>) -> Result<ModelList, String> {
    if base_url.is_empty() {
        return Err("set the provider's base URL first".into());
    }
    let body = fetch_models(kind, base_url, api_key)?;
    let models = if kind.routes_by_model() {
        let models = parse_gateway_models(&body)?;
        remember_routes(base_url, &models, ROUTES_FOR);
        let mut ids: Vec<String> = models
            .into_iter()
            .filter(|m| m.usable)
            .map(|m| m.id)
            .collect();
        ids.sort();
        ids.truncate(super::MAX_MODELS);
        ids
    } else {
        parse_models(&body)?
    };
    let mut prices = parse_prices(&body);
    prices.retain(|id, _| models.contains(id));
    Ok(ModelList { models, prices })
}

/// What a model list says its models cost, where it says so: OpenCode's
/// `cost.text` (USD per million tokens: `input.price`, `input.cache.read`,
/// `input.cache.write`, `output.price`) and OpenRouter's or Vercel's
/// `pricing` (USD per token, as text: `prompt` or `input`, `completion` or
/// `output`, `input_cache_read`, `input_cache_write`).
pub fn parse_prices(body: &str) -> Prices {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Prices::new();
    };
    let Some(list) = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
    else {
        return Prices::new();
    };
    let number = |v: Option<&Value>| -> Option<f64> {
        let v = v?;
        v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
    };
    list.iter()
        .filter_map(|m| {
            let id = m
                .get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)?;
            let round = |p: f64| (p * 1e6).round() / 1e6;
            let price = if let Some(text) = m.pointer("/cost/text") {
                // A write price per cache lifetime ({"5m": …, "1h": …}): the shortest.
                let write = text.pointer("/input/cache/write").and_then(|w| match w {
                    Value::Object(by_ttl) => by_ttl
                        .values()
                        .filter_map(|v| number(Some(v)))
                        .min_by(f64::total_cmp),
                    other => number(Some(other)),
                });
                let base = Price::new(
                    number(text.pointer("/input/price"))?,
                    number(text.pointer("/output/price"))?,
                    number(text.pointer("/input/cache/read")),
                    write,
                );
                Price {
                    tiers: super::prices::opencode_tiers(text, &base, |v| number(Some(v))),
                    multipliers: m
                        .pointer("/cost/multipliers")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(super::prices::opencode_multiplier)
                                .collect()
                        })
                        .unwrap_or_default(),
                    ..base
                }
            } else {
                let pricing = m.get("pricing")?;
                let per_token = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| number(pricing.get(*k)))
                        .map(|p| p * 1e6)
                };
                Price::new(
                    per_token(&["prompt", "input"])?,
                    per_token(&["completion", "output"])?,
                    per_token(&["input_cache_read"]),
                    per_token(&["input_cache_write"]),
                )
            };
            let price = Price {
                tiers: price
                    .tiers
                    .iter()
                    .map(|t| super::prices::Tier {
                        input: round(t.input),
                        output: round(t.output),
                        cached: t.cached.map(round),
                        cache_write: t.cache_write.map(round),
                        ..t.clone()
                    })
                    .collect(),
                multipliers: price.multipliers.clone(),
                ..Price::new(
                    round(price.input),
                    round(price.output),
                    price.cached.map(round),
                    price.cache_write.map(round),
                )
            };
            // Negative: "it depends" (OpenRouter's automatic routing).
            price.valid().then(|| (id.to_string(), price))
        })
        .collect()
}

fn fetch_models(kind: Kind, base_url: &str, api_key: Option<&str>) -> Result<String, String> {
    let mut request = agent(LIST_TIMEOUT).get(models_url(kind, base_url));
    for (name, value) in headers(kind, kind.protocol(), api_key, "") {
        if name != "Content-Type" {
            request = request.header(name, value);
        }
    }
    let response = request
        .call()
        .map_err(|e| format!("cannot reach the provider at {base_url}: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), 8 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(refusal(status, &body));
    }
    Ok(body)
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
    out.truncate(super::MAX_MODELS);
    Ok(out)
}

/// A model of a gateway that routes by model.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayModel {
    pub id: String,
    pub route: Route,
    /// A chat model RAST may call (not an embedding or image model, and not
    /// a free one, which only the gateway's own apps may use).
    pub usable: bool,
}

/// How a gateway serves one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub anthropic: bool,
    pub reasoning_back: bool,
}

impl Route {
    /// For a model the list did not have (or could not be read).
    fn guess(model: &str) -> Self {
        Self {
            anthropic: model.starts_with("claude-"),
            reasoning_back: false,
        }
    }
}

fn gateway_models(
    kind: Kind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<GatewayModel>, String> {
    parse_gateway_models(&fetch_models(kind, base_url, api_key)?)
}

/// A gateway's model list (OpenAI's list shape, each model with
/// `recommended_api`, `api`, `type` and `cost`).
pub fn parse_gateway_models(body: &str) -> Result<Vec<GatewayModel>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "the provider's model list is not JSON".to_string())?;
    let list = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "the provider did not send a model list".to_string())?;
    let price = |m: &Value, side: &str| {
        m.pointer(&format!("/cost/text/{side}/price"))
            .and_then(Value::as_f64)
    };
    Ok(list
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            if id.is_empty() || id.len() > super::MAX_MODEL_NAME {
                return None;
            }
            let api = m.get("api").cloned().unwrap_or(Value::Null);
            let chat_api = api.get("chat-completions").is_some();
            let anthropic = match m.get("recommended_api").and_then(Value::as_str) {
                Some(recommended) => recommended == "messages",
                None => api.get("messages").is_some() && !chat_api,
            };
            let reasoning_back = api
                .pointer("/chat-completions/compatibility/reasoning_field")
                .and_then(Value::as_str)
                == Some("reasoning_content");
            let chat = m
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "chat");
            let free = price(m, "input") == Some(0.0) && price(m, "output") == Some(0.0);
            Some(GatewayModel {
                id: id.to_string(),
                route: Route {
                    anthropic,
                    reasoning_back,
                },
                usable: chat && !free,
            })
        })
        .collect())
}

/// Gateways' routes by base URL, and until when they are used.
type Routes = HashMap<String, (Instant, HashMap<String, Route>)>;
static ROUTES: LazyLock<Mutex<Routes>> = LazyLock::new(Default::default);

fn remember_routes(base_url: &str, models: &[GatewayModel], keep: Duration) {
    let routes = models.iter().map(|m| (m.id.clone(), m.route)).collect();
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(base_url.to_string(), (Instant::now() + keep, routes));
}

/// How a gateway wants `req.model` sent (reads its model list when the
/// routes are old).
fn gateway_wire(req: &ChatRequest) -> Wire {
    let known = |routes: &Routes| {
        routes
            .get(&req.base_url)
            .filter(|(until, _)| *until > Instant::now())
            .map(|(_, models)| models.get(&req.model).copied())
    };
    let cached = known(&ROUTES.lock().unwrap_or_else(|e| e.into_inner()));
    let route = match cached {
        Some(route) => route,
        None => {
            match gateway_models(req.kind, &req.base_url, req.api_key.as_deref()) {
                Ok(models) => remember_routes(&req.base_url, &models, ROUTES_FOR),
                Err(_) => remember_routes(&req.base_url, &[], ROUTES_RETRY),
            }
            known(&ROUTES.lock().unwrap_or_else(|e| e.into_inner())).flatten()
        }
    }
    .unwrap_or_else(|| Route::guess(&req.model));
    Wire {
        protocol: if route.anthropic {
            Protocol::Anthropic
        } else {
            Protocol::Chat
        },
        reasoning_back: route.reasoning_back,
        usage_option: true,
        effort: None,
    }
}

pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "the provider's model list is not JSON".to_string())?;
    let list = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| "the provider did not send a model list".to_string())?;
    let mut ids: Vec<String> = list
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty() && id.len() <= super::MAX_MODEL_NAME)
        .collect();
    ids.sort();
    ids.dedup();
    ids.truncate(super::MAX_MODELS);
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(kind: Kind, lines: &[&str]) -> (Vec<Delta>, Usage, bool) {
        let mut parser = StreamParser::new(kind);
        let mut out = Vec::new();
        for line in lines {
            out.extend(parser.line(line).unwrap());
        }
        (out, parser.usage, parser.done)
    }

    #[test]
    fn openai_compatible_streams_are_read() {
        let (deltas, usage, done) = feed(
            Kind::DeepSeek,
            &[
                ": keep-alive",
                "",
                r#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
                r#"data: {"choices":[{"delta":{"reasoning_content":"Let me think"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
                r#"data:{"choices":[{"delta":{"content":"lo"}}]}"#,
                r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_cache_hit_tokens":8}}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(
            deltas,
            vec![
                Delta::Reasoning("Let me think".into()),
                Delta::Text("Hel".into()),
                Delta::Text("lo".into())
            ]
        );
        assert_eq!(
            usage,
            Usage {
                input: 12,
                output: 5,
                cache_read: 8,
                cache_write: 0,
            }
        );
        // OpenAI and the gateways: cached input under prompt_tokens_details.
        let (_, usage, _) = feed(
            Kind::OpenRouter,
            &[
                r#"data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}}}"#,
            ],
        );
        assert_eq!((usage.cache_read, usage.cache_write), (60, 30));
        assert!(done);
        let mut parser = StreamParser::new(Kind::OpenRouter);
        let err = parser
            .line(r#"data: {"error":{"message":"Insufficient credits","code":402}}"#)
            .unwrap_err();
        assert_eq!(err, "Insufficient credits");
    }

    #[test]
    fn anthropic_streams_are_read() {
        let (deltas, usage, done) = feed(
            Kind::Anthropic,
            &[
                "event: message_start",
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3,"output_tokens":1}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        );
        assert_eq!(
            deltas,
            vec![Delta::Reasoning("hmm".into()), Delta::Text("Hi".into())]
        );
        assert_eq!(
            usage,
            Usage {
                input: 28,
                output: 9,
                cache_read: 5,
                cache_write: 3,
            }
        );
        assert!(done);
        // A gateway that sends the input with the totals at the end.
        let (_, usage, _) = feed(
            Kind::Anthropic,
            &[
                r#"data: {"type":"message_delta","usage":{"input_tokens":4,"cache_read_input_tokens":40,"output_tokens":2}}"#,
            ],
        );
        assert_eq!((usage.input, usage.cache_read, usage.output), (44, 40, 2));
        let mut parser = StreamParser::new(Kind::Anthropic);
        let err = parser
            .line(r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .unwrap_err();
        assert_eq!(err, "Overloaded");
    }

    fn codex_request(model: &str) -> ChatRequest {
        ChatRequest {
            kind: Kind::Codex,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            api_key: Some("token".into()),
            model: model.into(),
            system: "Be brief.".into(),
            messages: vec![
                Turn::User("files?".into()),
                Turn::Assistant {
                    reasoning: "think".into(),
                    text: "Looking.".into(),
                    calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "list_files".into(),
                        input: json!({"path": "/"}),
                    }],
                },
                Turn::Results(vec![ToolResult {
                    id: "call_1".into(),
                    name: "list_files".into(),
                    output: "a.txt".into(),
                    is_error: false,
                }]),
            ],
            tools: vec![ToolSpec {
                name: "list_files".into(),
                description: "List".into(),
                parameters: json!({"type": "object"}),
            }],
            temperature: Some(0.3),
            effort: Some(Effort::Max),
            max_output_tokens: Some(500),
            session: "conv-1".into(),
            account: "acc-1".into(),
        }
    }

    #[test]
    fn codex_bodies_are_shaped_like_the_codex_cli() {
        let req = codex_request("gpt-5.6-terra");
        assert_eq!(
            chat_url(req.kind, &req.base_url, Protocol::Responses),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let wire = |req: &ChatRequest| Wire {
            effort: req.effort,
            ..Wire::of(req.kind)
        };
        let body = request_body_for(&req, wire(&req));
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["prompt_cache_key"], "conv-1");
        assert_eq!(
            body["reasoning"],
            json!({"effort": "xhigh", "summary": "auto"})
        );
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "name": "list_files", "description": "List", "parameters": {"type": "object"}, "strict": false}])
        );
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "files?"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Looking."}]},
                {"type": "function_call", "call_id": "call_1", "name": "list_files", "arguments": "{\"path\":\"/\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"},
            ])
        );
        // Codex refuses what the Codex CLI does not send.
        for key in [
            "temperature",
            "max_output_tokens",
            "max_tokens",
            "messages",
            "stream_options",
        ] {
            assert!(body.get(key).is_none(), "{key} is not sent");
        }

        // "Lite" models take their tools and instructions as input items.
        let req = codex_request("gpt-6.1-sol");
        assert!(
            codex_lite("gpt-6.1-sol")
                && codex_lite("gpt-6-luna")
                && !codex_lite("gpt-6.2-astra")
                && !codex_lite("gpt-5.6-sol")
        );
        let body = request_body_for(&req, wire(&req));
        // No effort asked for: Codex's default.
        assert_eq!(request_body(&req)["reasoning"]["effort"], "medium");
        assert_eq!(body["instructions"], "");
        assert_eq!(body["tools"], Value::Null);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "max", "context": "all_turns"})
        );
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["tools"][0]["name"], "list_files");
        assert_eq!(
            body["input"][1],
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Be brief."}]})
        );
        assert_eq!(body["input"][2]["role"], "user");

        let headers = codex_headers("gpt-6.1-sol", "acc-1", "conv-1");
        let has = |name: &str, value: &str| headers.iter().any(|(n, v)| *n == name && v == value);
        assert!(
            has("ChatGPT-Account-ID", "acc-1")
                && has("session_id", "conv-1")
                && has("originator", "codex_cli_rs")
        );
        assert!(has("x-openai-internal-codex-responses-lite", "true"));
        assert!(!codex_headers("gpt-5.6-sol", "", "")
            .iter()
            .any(|(n, _)| *n == "x-openai-internal-codex-responses-lite"
                || *n == "ChatGPT-Account-ID"));
    }

    #[test]
    fn codex_streams_are_read() {
        let mut parser = StreamParser::for_protocol(Protocol::Responses);
        let mut deltas = Vec::new();
        for line in [
            "event: response.created",
            r#"data: {"type":"response.created","response":{"id":"r1"}}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"Plan"}"#,
            r#"data: {"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"More"}"#,
            r#"data: {"type":"response.output_text.delta","output_index":1,"delta":"Hi"}"#,
            r#"data: {"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_9","name":"list_files","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"/\"}"}"#,
            r#"data: {"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_9","name":"list_files","arguments":"{\"path\":\"/\"}"}}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":9}}}"#,
        ] {
            deltas.extend(parser.line(line).unwrap());
        }
        assert_eq!(
            deltas,
            vec![
                Delta::Reasoning("Plan".into()),
                Delta::Reasoning("\n\n".into()),
                Delta::Reasoning("More".into()),
                Delta::Text("Hi".into()),
            ]
        );
        assert!(parser.done);
        assert_eq!(
            parser.usage,
            Usage {
                input: 120,
                output: 9,
                cache_read: 100,
                cache_write: 0
            }
        );
        assert_eq!(
            parser.take_calls(),
            vec![ToolCall {
                id: "call_9".into(),
                name: "list_files".into(),
                input: json!({"path": "/"})
            }]
        );

        let mut parser = StreamParser::for_protocol(Protocol::Responses);
        let failed = parser
            .line(r#"data: {"type":"response.failed","response":{"error":{"code":"server_error","message":"Try later"}}}"#)
            .unwrap_err();
        assert!(failed.contains("Try later"), "{failed}");
        let mut parser = StreamParser::for_protocol(Protocol::Responses);
        let cut = parser
            .line(r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#)
            .unwrap_err();
        assert!(cut.contains("max_output_tokens"));
    }

    #[test]
    fn codex_models_and_refusals_are_read() {
        let body = r#"{"models":[
            {"slug":"gpt-6.2-sol","display_name":"GPT-6.2 Sol","visibility":"list"},
            {"slug":"gpt-5.6-terra","visibility":"list"},
            {"slug":"codex-auto-review","visibility":"hide"},
            {"slug":"gpt-image-2"},
            {"id":"gpt-6-luna"},
            {"slug":"gpt-6.2-sol"}
        ]}"#;
        assert_eq!(
            parse_codex_models(body).unwrap(),
            vec!["gpt-6.2-sol", "gpt-5.6-terra", "gpt-6-luna"]
        );
        assert!(parse_codex_models("{}").is_err());
        assert!(parse_codex_models("nope").is_err());
        assert!(
            codex_refusal(401, r#"{"error":{"message":"Token expired"}}"#)
                .starts_with("ChatGPT refused the sign-in; sign in again")
        );
        assert!(codex_refusal(429, "{}").contains("Codex limit is used up"));
        assert_eq!(codex_refusal(500, "{}"), refusal(500, "{}"));
        assert_eq!(
            models_url(Kind::Codex, "https://chatgpt.com/backend-api/codex"),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                super::super::signin::CODEX_VERSION
            )
        );
    }

    #[test]
    fn bodies_follow_each_protocol() {
        let mut req = ChatRequest {
            kind: Kind::OpenAi,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("k".into()),
            model: "gpt-x".into(),
            system: "Be brief.".into(),
            messages: vec![Turn::User("hi".into())],
            tools: Vec::new(),
            temperature: Some(0.2),
            effort: None,
            max_output_tokens: Some(100),
            session: String::new(),
            account: String::new(),
        };
        let body = request_body(&req);
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(body["max_completion_tokens"], 100);
        assert_eq!(body["stream_options"]["include_usage"], true);
        req.kind = Kind::DeepSeek;
        assert_eq!(request_body(&req)["max_tokens"], 100);
        req.kind = Kind::Anthropic;
        req.max_output_tokens = None;
        let body = request_body(&req);
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(body["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);
        let h = headers(Kind::Anthropic, Protocol::Anthropic, Some("sk-ant"), "");
        assert!(h.contains(&("x-api-key", "sk-ant".to_string())));
        assert!(!h.iter().any(|(name, _)| *name == "Authorization"));
    }

    #[test]
    fn tool_calls_are_collected_from_both_protocols() {
        let mut parser = StreamParser::new(Kind::OpenAi);
        for line in [
            r#"data: {"choices":[{"delta":{"content":"Let me look."}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"list_files","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"/\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"fetch_url","arguments":"not json"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            parser.line(line).unwrap();
        }
        let calls = parser.take_calls();
        assert_eq!(
            calls,
            vec![
                ToolCall {
                    id: "call_a".into(),
                    name: "list_files".into(),
                    input: json!({"path": "/"})
                },
                ToolCall {
                    id: "call_b".into(),
                    name: "fetch_url".into(),
                    input: json!("not json")
                },
            ]
        );
        // Ollama: the whole call at once, no id.
        let mut parser = StreamParser::new(Kind::Ollama);
        parser
            .line(r#"data: {"choices":[{"delta":{"tool_calls":[{"function":{"name":"read_file","arguments":"{\"path\":\"/a\"}"}}]}}]}"#)
            .unwrap();
        let calls = parser.take_calls();
        assert_eq!(calls[0].name, "read_file");
        assert!(calls[0].id.starts_with("call_0_"));

        let mut parser = StreamParser::new(Kind::Anthropic);
        for line in [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sure"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"run_command","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ls"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"}"}}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"list_files","input":{}}}"#,
            r#"data: {"type":"message_stop"}"#,
        ] {
            parser.line(line).unwrap();
        }
        assert_eq!(
            parser.take_calls(),
            vec![
                ToolCall {
                    id: "toolu_1".into(),
                    name: "run_command".into(),
                    input: json!({"command": "ls"})
                },
                ToolCall {
                    id: "toolu_2".into(),
                    name: "list_files".into(),
                    input: json!({})
                },
            ]
        );
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "list_files".into(),
            input: json!({"path": "/"}),
        }
    }

    fn result(id: &str) -> ToolResult {
        ToolResult {
            id: id.into(),
            name: "list_files".into(),
            output: "a.txt".into(),
            is_error: false,
        }
    }

    #[test]
    fn turns_are_normalized() {
        let assistant = |text: &str, calls: Vec<ToolCall>| Turn::Assistant {
            reasoning: String::new(),
            text: text.into(),
            calls,
        };
        let turns = vec![
            assistant("hello", vec![]),
            Turn::Results(vec![result("stray")]),
            Turn::User("one".into()),
            Turn::User("two".into()),
            assistant("", vec![]),
            assistant("looking", vec![call("a"), call("b")]),
            Turn::Results(vec![result("b")]),
            assistant("", vec![call("c")]),
        ];
        let out = normalize(turns.clone(), true);
        let mut not_run = result("a");
        not_run.output = "Not run.".into();
        not_run.is_error = true;
        let mut c_not_run = not_run.clone();
        c_not_run.id = "c".into();
        assert_eq!(
            out,
            vec![
                Turn::User("one\n\ntwo".into()),
                assistant("looking", vec![call("a"), call("b")]),
                Turn::Results(vec![not_run, result("b")]),
                assistant("", vec![call("c")]),
                Turn::Results(vec![c_not_run]),
            ]
        );
        // Without tools, calls and results become text.
        let flat = normalize(
            vec![
                Turn::User("hi".into()),
                assistant("ok", vec![call("a")]),
                Turn::Results(vec![result("a")]),
                Turn::User("next".into()),
            ],
            false,
        );
        assert_eq!(
            flat,
            vec![
                Turn::User("hi".into()),
                assistant(
                    "ok\n\n[I used the tool list_files with {\"path\":\"/\"}]",
                    vec![]
                ),
                Turn::User("[list_files returned: a.txt]\n\nnext".into()),
            ]
        );
    }

    #[test]
    fn tool_turns_follow_each_protocol() {
        let mut req = ChatRequest {
            kind: Kind::DeepSeek,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: None,
            model: "deepseek-reasoner".into(),
            system: String::new(),
            messages: vec![
                Turn::User("files?".into()),
                Turn::Assistant {
                    reasoning: "think".into(),
                    text: String::new(),
                    calls: vec![call("call.1")],
                },
                Turn::Results(vec![ToolResult {
                    id: "call.1".into(),
                    name: "list_files".into(),
                    output: "denied".into(),
                    is_error: true,
                }]),
                Turn::User("ok".into()),
            ],
            tools: vec![ToolSpec {
                name: "list_files".into(),
                description: "List".into(),
                parameters: json!({"type": "object"}),
            }],
            temperature: None,
            effort: None,
            max_output_tokens: None,
            session: String::new(),
            account: String::new(),
        };
        let body = request_body(&req);
        assert_eq!(body["tools"][0]["function"]["name"], "list_files");
        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": null, "reasoning_content": "think",
                   "tool_calls": [{"id": "call.1", "type": "function", "function": {"name": "list_files", "arguments": "{\"path\":\"/\"}"}}]})
        );
        assert_eq!(
            body["messages"][2],
            json!({"role": "tool", "tool_call_id": "call.1", "content": "Error: denied"})
        );
        assert_eq!(
            body["messages"][3],
            json!({"role": "user", "content": "ok"})
        );
        req.kind = Kind::Anthropic;
        let body = request_body(&req);
        assert_eq!(
            body["tools"][0],
            json!({"name": "list_files", "description": "List", "input_schema": {"type": "object"}})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "list_files", "input": {"path": "/"}}]})
        );
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "denied", "is_error": true},
                {"type": "text", "text": "ok"}
            ]})
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert!(refuses_tools(
            400,
            r#"{"error":"llama2 does not support tools"}"#
        ));
        assert!(!refuses_tools(401, "tool"));
    }

    #[test]
    fn refusals_and_model_lists_are_read() {
        assert_eq!(
            refusal(401, r#"{"error":{"message":"Invalid API key"}}"#),
            "the provider refused the API key (HTTP 401): Invalid API key"
        );
        assert_eq!(
            parse_models(r#"{"data":[{"id":"b"},{"id":"a"},{"id":"a"}]}"#).unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(
            parse_models(r#"{"models":[{"name":"llama3"}]}"#).unwrap(),
            vec!["llama3"]
        );
        assert!(parse_models("[]").is_err());
        let prices = parse_prices(
            &json!({"data": [
                {"id": "claude-x", "cost": {"text": {"input": {"price": 2}, "output": {"price": 10}}}},
                {"id": "glm-x", "cost": {"text": {"input": {"price": 1.4, "cache": {"read": 0.26}}, "output": {"price": 4.4}}}},
                {"id": "gpt-y", "cost": {"text": {"input": {"price": 0.2, "cache": {"read": 0.02, "write": {"30m": 0.25, "12h": 0.5}}}, "output": {"price": 1.2}}}},
                {"id": "openai/gpt-x", "pricing": {"prompt": "0.0000011", "completion": "0.0000044", "input_cache_read": "0.000000275"}},
                {"id": "vercel/x", "pricing": {"input": "0.000003", "output": "0.000015", "input_cache_write": "0.00000375"}},
                {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}},
                {"id": "plain"}
            ]})
            .to_string(),
        );
        assert_eq!(
            prices,
            Prices::from([
                ("claude-x".to_string(), Price::new(2.0, 10.0, None, None)),
                ("glm-x".to_string(), Price::new(1.4, 4.4, Some(0.26), None)),
                (
                    "gpt-y".to_string(),
                    Price::new(0.2, 1.2, Some(0.02), Some(0.25))
                ),
                (
                    "openai/gpt-x".to_string(),
                    Price::new(1.1, 4.4, Some(0.275), None)
                ),
                (
                    "vercel/x".to_string(),
                    Price::new(3.0, 15.0, None, Some(3.75))
                ),
            ])
        );
        // OpenCode Go's DeepSeek and Grok: peak hours and long prompts.
        let prices = parse_prices(
            &json!({"data": [
                {"id": "ds", "cost": {"text": {"input": {"price": 0.15, "cache": {"read": 0.003}}, "output": {"price": 0.6}},
                    "multipliers": [{"when": {"utc_days": {"in": ["mon", "tue", "wed", "thu", "fri"]}, "any": [{"all": [{"utc_hours": {"gte": "01:00"}}, {"utc_hours": {"lt": "04:00"}}]}]}, "factor": 2}]}},
                {"id": "grok", "cost": {"text": {"input": {"price": 2, "cache": {"read": 0.5}, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 4, "cache": {"read": 1}}]},
                    "output": {"price": 6, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 12}]}}}}
            ]})
            .to_string(),
        );
        assert_eq!(prices["ds"].multipliers.len(), 1);
        assert_eq!(prices["ds"].multipliers[0].factor, 2.0);
        assert_eq!(prices["ds"].multipliers[0].days.len(), 5);
        assert_eq!(prices["grok"].tiers.len(), 1);
        assert_eq!(
            (
                prices["grok"].tiers[0].above,
                prices["grok"].tiers[0].output
            ),
            (200_000, 12.0)
        );
    }

    #[test]
    fn every_kind_has_a_preset() {
        let mut ids = std::collections::HashSet::new();
        for info in super::super::KINDS.iter() {
            assert!(ids.insert(info.id), "{} listed twice", info.id);
            assert_eq!(Kind::parse(info.id), Some(info.kind));
            assert_eq!(info.kind.as_str(), info.id);
            let custom = info.group == "custom";
            assert_eq!(info.base_url.is_empty(), custom, "{}", info.id);
            if !custom {
                assert!(super::super::valid_base_url(info.base_url), "{}", info.id);
            }
            assert!(info.key_url.is_empty() || info.key_url.starts_with("https://"));
            assert!(
                ["maker", "plan", "gateway", "fast", "local", "custom"].contains(&info.group),
                "{}",
                info.id
            );
        }
        assert_eq!(Kind::CustomAnthropic.protocol(), Protocol::Anthropic);
        assert_eq!(Kind::Gemini.protocol(), Protocol::Chat);
        assert_eq!(Kind::Codex.protocol(), Protocol::Responses);
        assert!(Kind::Codex.signs_in() && !Kind::Codex.needs_key() && !Kind::OpenAi.signs_in());
    }

    #[test]
    fn gateways_route_each_model() {
        let base = "https://opencode.ai/inference";
        assert_eq!(
            chat_url(Kind::OpenCode, base, Protocol::Anthropic),
            "https://opencode.ai/inference/anthropic/v1/messages"
        );
        assert_eq!(
            chat_url(
                Kind::OpenCodeGo,
                "https://opencode.ai/inference/go",
                Protocol::Chat
            ),
            "https://opencode.ai/inference/go/openai/v1/chat/completions"
        );
        assert_eq!(
            models_url(Kind::OpenCode, base),
            "https://opencode.ai/inference/v1/models"
        );
        assert_eq!(
            chat_url(Kind::Groq, "https://api.groq.com/openai/v1", Protocol::Chat),
            "https://api.groq.com/openai/v1/chat/completions"
        );
        let models = parse_gateway_models(
            &json!({"object": "list", "data": [
                {"id": "claude-sonnet-5-5", "type": "chat", "recommended_api": "messages",
                 "api": {"messages": {}}, "cost": {"text": {"input": {"price": 2}, "output": {"price": 10}}}},
                {"id": "qwen3.8-max", "type": "chat", "recommended_api": "chat-completions",
                 "api": {"chat-completions": {"compatibility": {"reasoning_field": "reasoning_content"}}}},
                {"id": "gpt-6-sol", "type": "chat", "recommended_api": "responses", "api": {"responses": {}}},
                {"id": "big-pickle", "type": "chat", "api": {"chat-completions": {}},
                 "cost": {"text": {"input": {"price": 0}, "output": {"price": 0}}}},
                {"id": "text-embedding-3-small", "type": "embedding", "api": {"embeddings": {}}}
            ]})
            .to_string(),
        )
        .unwrap();
        let find = |id: &str| models.iter().find(|m| m.id == id).unwrap().clone();
        assert!(find("claude-sonnet-5-5").route.anthropic);
        assert!(find("claude-sonnet-5-5").usable);
        let qwen = find("qwen3.8-max");
        assert!(!qwen.route.anthropic && qwen.route.reasoning_back);
        assert!(
            !find("gpt-6-sol").route.anthropic,
            "GPT goes through chat completions"
        );
        assert!(!find("big-pickle").usable, "free models are OpenCode's own");
        assert!(!find("text-embedding-3-small").usable);
        assert!(Route::guess("claude-x").anthropic && !Route::guess("kimi-k3").anthropic);

        let h = headers(Kind::OpenCodeGo, Protocol::Anthropic, Some("k"), "conv-1");
        assert!(h.contains(&("Authorization", "Bearer k".to_string())));
        assert!(h.contains(&("x-opencode-session", "conv-1".to_string())));
        assert!(h.iter().any(|(name, _)| *name == "anthropic-version"));
        assert!(!headers(Kind::OpenAi, Protocol::Chat, Some("k"), "conv-1")
            .iter()
            .any(|(name, _)| *name == "x-opencode-session"));
        let h = headers(Kind::CustomAnthropic, Protocol::Anthropic, Some("k"), "");
        assert!(h.contains(&("x-api-key", "k".to_string())));
        assert!(h.contains(&("Authorization", "Bearer k".to_string())));
    }

    /// Requests the fake gateway got: request line, session header, body.
    type GatewayLog = std::sync::Arc<Mutex<Vec<(String, String, Value)>>>;

    /// A gateway like OpenCode on a local port: its model list, Messages for
    /// Claude, and chat completions that refuse `stream_options` once.
    fn fake_gateway() -> (String, GatewayLog) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/inference", listener.local_addr().unwrap());
        let log: GatewayLog = Default::default();
        let seen = log.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let (mut session, mut length) = (String::new(), 0usize);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "x-opencode-session" => session = value.trim().to_string(),
                        "content-length" => length = value.trim().parse().unwrap(),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                seen.lock()
                    .unwrap()
                    .push((line.trim().to_string(), session, body.clone()));
                let reply = |status: &str, kind: &str, text: String| {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    )
                };
                let out = if line.starts_with("GET /inference/v1/models") {
                    reply(
                        "200 OK",
                        "application/json",
                        json!({"data": [
                            {"id": "claude-x", "type": "chat", "recommended_api": "messages"},
                            {"id": "kimi-x", "type": "chat", "recommended_api": "chat-completions"}
                        ]})
                        .to_string(),
                    )
                } else if line.starts_with("POST /inference/anthropic/v1/messages")
                    && body.pointer("/output_config/effort") == Some(&json!("max"))
                {
                    reply("400 Bad Request", "application/json",
                        r#"{"type":"error","error":{"type":"invalid_request_error","message":"output_config.effort: 'max' is not supported for this model"}}"#.into())
                } else if line.starts_with("POST /inference/anthropic/v1/messages") {
                    reply("200 OK", "text/event-stream", [
                        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from messages"}}"#,
                        r#"data: {"type":"message_stop"}"#,
                    ].map(|l| format!("{l}\n\n")).concat())
                } else if body.get("stream_options").is_some() {
                    reply("400 Bad Request", "application/json",
                        r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#.into())
                } else {
                    reply("200 OK", "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"from chat\"}}]}\n\ndata: [DONE]\n\n".into())
                };
                let _ = stream.write_all(out.as_bytes());
            }
        });
        (base, log)
    }

    #[test]
    fn gateways_get_each_model_on_its_protocol() {
        let (base, log) = fake_gateway();
        let ask = |model: &str| {
            let req = ChatRequest {
                kind: Kind::OpenCode,
                base_url: base.clone(),
                api_key: Some("k".into()),
                model: model.into(),
                system: String::new(),
                messages: vec![Turn::User("hi".into())],
                tools: Vec::new(),
                temperature: None,
                effort: None,
                max_output_tokens: None,
                session: "conv-7".into(),
                account: String::new(),
            };
            let mut text = String::new();
            stream_chat(&req, &AtomicBool::new(false), &mut |delta| {
                if let Delta::Text(t) = delta {
                    text.push_str(&t);
                }
            })
            .map(|_| text)
        };
        assert_eq!(ask("claude-x").unwrap(), "from messages");
        assert_eq!(ask("kimi-x").unwrap(), "from chat");
        let seen = log.lock().unwrap().clone();
        let lines: Vec<&str> = seen.iter().map(|(line, _, _)| line.as_str()).collect();
        assert!(
            lines[0].starts_with("GET /inference/v1/models"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("POST /inference/anthropic/v1/messages"));
        // The list is read once; chat completions are asked again without
        // `stream_options` after refusing it.
        assert!(lines[2].starts_with("POST /inference/openai/v1/chat/completions"));
        assert!(seen[2].2.get("stream_options").is_some());
        assert!(lines[3].starts_with("POST /inference/openai/v1/chat/completions"));
        assert!(seen[3].2.get("stream_options").is_none());
        assert_eq!(lines.len(), 4);
        assert!(seen[1..].iter().all(|(_, session, _)| session == "conv-7"));
        assert_eq!(
            list_models(Kind::OpenCode, &base, Some("k"))
                .unwrap()
                .models,
            vec!["claude-x".to_string(), "kimi-x".to_string()]
        );
    }

    #[test]
    fn effort_goes_out_as_each_protocol_asks() {
        let mut req = ChatRequest {
            kind: Kind::Anthropic,
            base_url: String::new(),
            api_key: None,
            model: "m".into(),
            system: String::new(),
            messages: vec![Turn::User("hi".into())],
            tools: Vec::new(),
            temperature: None,
            effort: Some(Effort::Max),
            max_output_tokens: None,
            session: String::new(),
            account: String::new(),
        };
        let body = |req: &ChatRequest| {
            let mut wire = Wire::of(req.kind);
            wire.effort = req.effort;
            request_body_for(req, wire)
        };
        assert_eq!(body(&req)["output_config"], json!({"effort": "max"}));
        assert!(body(&req).get("reasoning_effort").is_none());
        req.kind = Kind::OpenAi;
        assert_eq!(body(&req)["reasoning_effort"], "xhigh");
        req.kind = Kind::DeepSeek;
        assert_eq!(body(&req)["reasoning_effort"], "high");
        req.effort = Some(Effort::Low);
        assert_eq!(body(&req)["reasoning_effort"], "low");
        assert!(body(&req).get("output_config").is_none());
        req.kind = Kind::OpenRouter;
        assert_eq!(body(&req)["reasoning"], json!({"effort": "low"}));
        assert!(body(&req).get("reasoning_effort").is_none());
        // None: the provider's default, nothing sent.
        req.effort = None;
        for kind in [Kind::OpenRouter, Kind::Anthropic, Kind::Ollama] {
            req.kind = kind;
            let sent = body(&req);
            for field in ["reasoning", "reasoning_effort", "output_config"] {
                assert!(sent.get(field).is_none(), "{kind:?} sent {field}");
            }
        }
        assert_eq!(Effort::parse(""), Ok(None));
        assert_eq!(Effort::parse("max"), Ok(Some(Effort::Max)));
        assert!(Effort::parse("extreme").is_err());
    }

    #[test]
    fn a_refused_effort_is_asked_again_lower() {
        let (base, log) = fake_gateway();
        let req = ChatRequest {
            kind: Kind::OpenCode,
            base_url: base.clone(),
            api_key: Some("k".into()),
            model: "claude-x".into(),
            system: String::new(),
            messages: vec![Turn::User("hi".into())],
            tools: Vec::new(),
            temperature: None,
            effort: Some(Effort::Max),
            max_output_tokens: None,
            session: String::new(),
            account: String::new(),
        };
        let mut text = String::new();
        stream_chat(&req, &AtomicBool::new(false), &mut |delta| {
            if let Delta::Text(t) = delta {
                text.push_str(&t);
            }
        })
        .unwrap();
        assert_eq!(text, "from messages");
        let seen = log.lock().unwrap().clone();
        let efforts: Vec<&Value> = seen
            .iter()
            .filter(|(line, _, _)| line.starts_with("POST"))
            .map(|(_, _, body)| &body["output_config"]["effort"])
            .collect();
        assert_eq!(efforts, [&json!("max"), &json!("high")]);
        assert!(refuses_effort(
            400,
            r#"{"error":{"message":"reasoning_effort is not supported"}}"#
        ));
        assert!(!refuses_effort(401, "effort"));
        assert!(!refuses_effort(400, "model not found"));
    }
}
