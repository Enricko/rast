//! Reading a provider's event stream line by line: the text and reasoning
//! deltas it carries, the usage it reports and the tool calls it asks for,
//! per protocol (chat completions, Messages, Responses, Cloud Code).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::cloud_code;
use super::{error_text, Delta, ToolCall, Usage};
use crate::ai::{Kind, Protocol};

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

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
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

/// Reads a provider's event stream line by line.
#[derive(Debug)]
pub struct StreamParser {
    protocol: Protocol,
    pub usage: Usage,
    pub done: bool,
    /// Calls being streamed, by the provider's index.
    calls: BTreeMap<u64, PartialCall>,
    /// Cloud Code's events carry their own state (see [`super::cloud_code`]).
    cloud: cloud_code::Parser,
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
            cloud: cloud_code::Parser::default(),
        }
    }

    /// The calls the reply asked for, in order (ids made up where the
    /// provider sent none).
    pub fn take_calls(&mut self) -> Vec<ToolCall> {
        if self.protocol == Protocol::CloudCode {
            return self.cloud.take_calls();
        }
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
        // Cloud Code wraps every event in `{"response": …}`: its parser owns
        // the Google-shaped deltas, calls and usage.
        if self.protocol == Protocol::CloudCode {
            let out = self.cloud.line(&event)?;
            self.usage = self.cloud.usage;
            self.done = self.cloud.done;
            return Ok(out);
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
