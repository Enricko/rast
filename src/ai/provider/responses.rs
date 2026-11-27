//! OpenAI Responses: bodies for the kinds that stream its events (ChatGPT's
//! Codex backend today, xAI's Responses wire later). Codex's "lite" models
//! take their tools and instructions as the first input items; every other
//! model gets them as usual.

use serde_json::{json, Value};

use super::{ChatRequest, Turn, Wire};
use crate::ai::Effort;

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
pub(crate) fn body(req: &ChatRequest, wire: Wire, turns: &[Turn]) -> Value {
    let lite = super::plans::model_is_lite(req.kind, &req.model);
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
