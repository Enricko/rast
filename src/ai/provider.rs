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
//! Keys only go to the provider's own base URL; redirects are not followed.

use std::{
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

#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// `user` or `assistant`.
    pub role: &'static str,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub kind: Kind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
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
    let turns: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!({"role": m.role, "content": m.text}))
        .collect();
    if req.kind == Kind::Anthropic {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS),
            "messages": turns,
            "stream": true,
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        return body;
    }
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

/// Reads a provider's event stream line by line.
#[derive(Debug)]
pub struct StreamParser {
    anthropic: bool,
    pub usage: Usage,
    pub done: bool,
}

impl StreamParser {
    pub fn new(kind: Kind) -> Self {
        Self {
            anthropic: kind == Kind::Anthropic,
            usage: Usage::default(),
            done: false,
        }
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
                Some("content_block_delta") => {
                    let delta = &event["delta"];
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => push_text(&mut out, delta.get("text"), false),
                        Some("thinking_delta") => push_text(&mut out, delta.get("thinking"), true),
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

/// Stream a reply. `on_delta` gets the text as it comes; returns the usage
/// the provider reported (zero when it reports none). Stops early, without
/// an error, once `cancel` is set.
pub fn stream_chat(
    req: &ChatRequest,
    cancel: &AtomicBool,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<Usage, String> {
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
        return Err(refusal(
            status,
            &read_limited(response.into_body(), 16 * 1024),
        ));
    }
    let mut parser = StreamParser::new(req.kind);
    let reader = BufReader::new(response.into_body().into_reader());
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let line = line.map_err(|e| format!("the provider's answer broke off: {e}"))?;
        for delta in parser.line(&line)? {
            on_delta(delta);
        }
        if parser.done && req.kind == Kind::Anthropic {
            break;
        }
    }
    Ok(parser.usage)
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
            messages: vec![ChatMessage {
                role: "user",
                text: "hi".into(),
            }],
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
