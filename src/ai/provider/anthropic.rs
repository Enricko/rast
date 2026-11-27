//! Anthropic Messages: bodies for Claude's wire (tool use blocks, thinking
//! deltas asked for with `output_config.effort`).

use serde_json::{json, Value};

use super::ANTHROPIC_DEFAULT_MAX_TOKENS;
use super::{ChatRequest, Turn, Wire};

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
            Turn::User(user) => {
                // Images and PDFs first, then the words about them.
                let mut blocks: Vec<Value> = user
                    .ready_files()
                    .map(|file| {
                        let source =
                            json!({"type": "base64", "media_type": file.mime, "data": file.data});
                        if file.is_pdf() {
                            json!({"type": "document", "source": source, "title": file.name})
                        } else {
                            json!({"type": "image", "source": source})
                        }
                    })
                    .collect();
                let with_files = !blocks.is_empty();
                if !with_files || !user.text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": user.text}));
                }
                match out.last_mut() {
                    // After tool results: the same user turn.
                    Some(last) if last["role"] == "user" && last["content"].is_array() => {
                        if let Some(existing) = last["content"].as_array_mut() {
                            existing.extend(blocks);
                        }
                    }
                    _ if !with_files => {
                        out.push(json!({"role": "user", "content": user.text}));
                    }
                    _ => out.push(json!({"role": "user", "content": blocks})),
                }
            }
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

/// The body of a streamed Messages request over `wire`.
pub(crate) fn body(req: &ChatRequest, wire: Wire, normalized: &[Turn]) -> Value {
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_output_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS),
        "messages": anthropic_turns(normalized),
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
    body
}
