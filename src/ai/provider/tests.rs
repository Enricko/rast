//! Unit tests for the provider wire layer: bodies, streams, refusals, model
//! lists and gateway routing. They live apart from the code so each module
//! stays readable; nothing here touches the network (fake servers listen on
//! 127.0.0.1).

use std::sync::Mutex;

use serde_json::{json, Value};

use super::models::models_url;
use super::plans::codex::codex_headers;
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
        .line(
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
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
    assert!(
        !codex_headers("gpt-5.6-sol", "", "")
            .iter()
            .any(|(n, _)| *n == "x-openai-internal-codex-responses-lite"
                || *n == "ChatGPT-Account-ID")
    );
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

/// A plan's own catalog is used when it answers; otherwise RAST's built-in
/// list stands in, with the provider's reason kept for the page.
#[test]
fn plan_model_lists_say_where_they_come_from() {
    let live = plan_models_from(Kind::Codex, Ok(vec!["gpt-x".into()])).unwrap();
    assert_eq!(live.models, vec!["gpt-x"]);
    assert_eq!(live.fallback_reason, None);

    let refused = plan_models_from(
        Kind::Antigravity,
        Err("Google's Cloud Code refused the model list (HTTP 403): forbidden".into()),
    )
    .unwrap();
    assert_eq!(
        refused.models,
        plans::fallback_models(Kind::Antigravity)
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        refused.fallback_reason.as_deref(),
        Some("Google's Cloud Code refused the model list (HTTP 403): forbidden")
    );

    let empty = plan_models_from(Kind::Codex, Ok(Vec::new())).unwrap();
    assert!(empty.fallback_reason.unwrap().contains("listed no models"));
    // Kinds without a built-in list pass the error on.
    assert!(plan_models_from(Kind::OpenAi, Err("nope".into())).is_err());
}

fn with_files(kind: Kind, text: &str) -> ChatRequest {
    let file = |name: &str, mime: &str| Attachment {
        id: name.into(),
        name: name.into(),
        mime: mime.into(),
        size: 3,
        is_text: false,
        data: "QUJD".into(),
    };
    ChatRequest {
        kind,
        base_url: String::new(),
        api_key: Some("k".into()),
        model: "m".into(),
        system: String::new(),
        messages: vec![Turn::User(UserTurn {
            text: text.into(),
            files: vec![
                file("cat.png", "image/png"),
                file("doc.pdf", "application/pdf"),
                // Never read (text files join the text before this).
                Attachment {
                    is_text: true,
                    data: String::new(),
                    ..file("notes.txt", "text/plain")
                },
            ],
        })],
        tools: Vec::new(),
        temperature: None,
        effort: None,
        max_output_tokens: None,
        session: String::new(),
        account: String::new(),
    }
}

#[test]
fn files_go_in_each_protocols_shape() {
    let body = request_body(&with_files(Kind::OpenAi, "What is this?"));
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "What is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            {"type": "file", "file": {"filename": "doc.pdf", "file_data": "data:application/pdf;base64,QUJD"}},
        ])
    );
    let body = request_body(&with_files(Kind::Anthropic, "What is this?"));
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "QUJD"}, "title": "doc.pdf"},
            {"type": "text", "text": "What is this?"},
        ])
    );
    // Files alone: no empty text block.
    let body = request_body(&with_files(Kind::Anthropic, ""));
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
    let body = request_body(&with_files(Kind::Codex, "What is this?"));
    let input = body["input"].as_array().unwrap();
    let user = input.iter().find(|i| i["role"] == "user").unwrap();
    assert_eq!(
        user["content"],
        json!([
            {"type": "input_text", "text": "What is this?"},
            {"type": "input_image", "image_url": "data:image/png;base64,QUJD", "detail": "auto"},
            {"type": "input_file", "filename": "doc.pdf", "file_data": "data:application/pdf;base64,QUJD"},
        ])
    );
    // Without files a message stays plain text.
    let mut plain = with_files(Kind::OpenAi, "hi");
    plain.messages = vec![Turn::User("hi".into())];
    assert_eq!(request_body(&plain)["messages"][0]["content"], "hi");
    // Joined user turns keep every file.
    let turns = normalize(
        vec![
            Turn::User(UserTurn {
                text: "one".into(),
                files: vec![],
            }),
            with_files(Kind::OpenAi, "two").messages.remove(0),
        ],
        true,
    );
    let Turn::User(joined) = &turns[0] else {
        panic!("{turns:?}")
    };
    assert_eq!(joined.text, "one\n\ntwo");
    assert_eq!(joined.files.len(), 3);
}
