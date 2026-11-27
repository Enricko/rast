//! OpenAI-compatible chat completions: bodies for every kind that speaks
//! the shape (OpenAI, DeepSeek, OpenRouter, Ollama, the gateways and the
//! plans on chat completions), with their turns, tools and effort.

use serde_json::{json, Value};

use super::{ChatRequest, Turn, Wire};
use crate::ai::{Effort, Kind};

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

/// The body of a streamed chat completion over `wire`.
pub(crate) fn body(req: &ChatRequest, wire: Wire, normalized: &[Turn]) -> Value {
    let turns = openai_turns(wire.reasoning_back, normalized);
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
