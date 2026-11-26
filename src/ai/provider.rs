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
//! Both carry tools: the model asks for calls (streamed as pieces of JSON),
//! the harness answers each with a result in the next request.
//!
//! Keys only go to the provider's own base URL; redirects are not followed.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde_json::{json, Value};

use super::Kind;

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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
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

fn openai_turns(kind: Kind, turns: &[Turn]) -> Vec<Value> {
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
                // DeepSeek's thinking models want their reasoning back
                // once tools are in play.
                if kind == Kind::DeepSeek && !reasoning.is_empty() {
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

fn headers(kind: Kind, api_key: Option<&str>) -> Vec<(&'static str, String)> {
    let mut out = vec![("Content-Type", "application/json".to_string())];
    match kind {
        Kind::Anthropic => {
            out.push(("anthropic-version", "2023-06-01".into()));
            if let Some(key) = api_key {
                out.push(("x-api-key", key.to_string()));
            }
        }
        _ => {
            if let Some(key) = api_key {
                out.push(("Authorization", format!("Bearer {key}")));
            }
        }
    }
    if kind == Kind::OpenRouter {
        out.push(("HTTP-Referer", "https://github.com/Enricko/rast".into()));
        out.push(("X-Title", "RAST".into()));
    }
    out
}

/// The request body for a streamed reply.
pub fn request_body(req: &ChatRequest) -> Value {
    let normalized = normalize(req.messages.clone(), !req.tools.is_empty());
    if req.kind == Kind::Anthropic {
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
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
                .collect();
        }
        return body;
    }
    let turns = openai_turns(req.kind, &normalized);
    let mut messages = Vec::with_capacity(turns.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    messages.extend(turns);
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
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

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Reads a provider's event stream line by line.
#[derive(Debug)]
pub struct StreamParser {
    anthropic: bool,
    pub usage: Usage,
    pub done: bool,
    /// Calls being streamed, by the provider's index.
    calls: BTreeMap<u64, PartialCall>,
}

impl StreamParser {
    pub fn new(kind: Kind) -> Self {
        Self {
            anthropic: kind == Kind::Anthropic,
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
        if self.anthropic {
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    let usage = &event["message"]["usage"];
                    self.usage.input = [
                        "input_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ]
                    .iter()
                    .filter_map(|k| usage.get(*k).and_then(Value::as_i64))
                    .sum();
                    if let Some(output) = usage.get("output_tokens").and_then(Value::as_i64) {
                        self.usage.output = output;
                    }
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
                    if let Some(output) =
                        event["usage"].get("output_tokens").and_then(Value::as_i64)
                    {
                        self.usage.output = output;
                    }
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
            if let Some(input) = usage.get("prompt_tokens").and_then(Value::as_i64) {
                self.usage.input = input;
            }
            if let Some(output) = usage.get("completion_tokens").and_then(Value::as_i64) {
                self.usage.output = output;
            }
        }
        Ok(out)
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
    let url = if req.kind == Kind::Anthropic {
        format!("{}/messages", req.base_url)
    } else {
        format!("{}/chat/completions", req.base_url)
    };
    let body = request_body(req).to_string();
    let mut request = agent(REPLY_TIMEOUT).post(&url);
    for (name, value) in headers(req.kind, req.api_key.as_deref()) {
        request = request.header(name, value);
    }
    request = request.header("Accept", "text/event-stream");
    let response = request
        .send(body.as_bytes())
        .map_err(|e| format!("cannot reach the provider at {}: {e}", req.base_url))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = read_limited(response.into_body(), 16 * 1024);
        return Err(ChatError {
            tools_refused: !req.tools.is_empty() && refuses_tools(status, &body),
            message: refusal(status, &body),
        });
    }
    let mut parser = StreamParser::new(req.kind);
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
        if parser.done && req.kind == Kind::Anthropic {
            break;
        }
    }
    Ok(Outcome {
        usage: parser.usage,
        calls: parser.take_calls(),
    })
}

/// The models a provider offers (`GET {base}/models`), sorted.
pub fn list_models(
    kind: Kind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    if base_url.is_empty() {
        return Err("set the provider's base URL first".into());
    }
    let url = if kind == Kind::Anthropic {
        format!("{base_url}/models?limit=1000")
    } else {
        format!("{base_url}/models")
    };
    let mut request = agent(LIST_TIMEOUT).get(&url);
    for (name, value) in headers(kind, api_key) {
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
    parse_models(&body)
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
                r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5}}"#,
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
                output: 5
            }
        );
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
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
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
                input: 25,
                output: 9
            }
        );
        assert!(done);
        let mut parser = StreamParser::new(Kind::Anthropic);
        let err = parser
            .line(r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .unwrap_err();
        assert_eq!(err, "Overloaded");
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
            max_output_tokens: Some(100),
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
        let h = headers(Kind::Anthropic, Some("sk-ant"));
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
            max_output_tokens: None,
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
    }
}
