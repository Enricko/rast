//! Google Cloud Code `v1internal`: the wire the Antigravity IDE and the
//! Gemini CLI speak. A Gemini-shaped request travels in an envelope
//! (`{project, requestId, model, userAgent, request, …}`) and every server
//! event is a Gemini response wrapped once more: `{"response": {…}}`.
//!
//! The two clients share this module and differ only in the envelope's
//! identity (`userAgent`, `requestType`, how ids look) and the headers they
//! add (`plans::antigravity` and `plans::gemini_cli`); the contents, tools,
//! thinking budgets, usage and stream parsing are the same Gemini dialect,
//! checked against the 2026-11-27 references:
//! `dsh-plugin-subscriptions/src/translate/antigravity*.ts` and
//! `9router/open-sse/{translators,executors}`.
//!
//! Why the shapes are the way they are, where the references disagree or
//! Google is fussy:
//! * Gemini `contents` alternate `user`/`model`; tool results are `user`
//!   parts holding a `functionResponse` whose `response` must be a JSON
//!   object (a bare array or scalar is rejected as a protobuf `Struct`).
//! * Gemini 3 and 4 validate the replay of a `functionCall` with a
//!   `thoughtSignature`. RAST stores no signatures, so the first call of a
//!   turn carries Google's documented placeholder instead
//!   (`skip_thought_signature_validator`), as the plugin does.
//! * Tools take `parametersJsonSchema` for Gemini models and the older
//!   `parameters` subset for the Claude- and GPT-OSS-backed ones the
//!   Antigravity host serves; local `$ref`/`anyOf` are resolved because the
//!   host does not.
//! * Thinking is asked for with a model-family budget (the plugin's
//!   `antigravity-thinking.ts`), not the generic effort fields.
//! * Antigravity's official client omits `requestType` on chat traffic
//!   (9router's note) while the DSH plugin sends `"agent"`; RAST sends
//!   `"agent"` like the plugin. Only the IDE's envelope has those fields —
//!   the Gemini CLI's envelope is just identity + request.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use super::{
    agent, error_text, read_limited, ChatRequest, Delta, ToolCall, ToolResult, ToolSpec, Turn,
    Usage, Wire, LIST_TIMEOUT,
};
use crate::ai::{Kind, MAX_MODELS, MAX_MODEL_NAME};

/// Antigravity's bundled client identity: the Hub desktop build the IDE ships
/// (`antigravity/cli/1.3.4 (aidev_client; <platform>; <arch>)` in the plugin).
pub(crate) const ANTIGRAVITY_USER_AGENT: &str =
    "antigravity/cli/1.3.4 (aidev_client; linux; x86_64)";

/// The Gemini CLI version its user agent carries (9router registry: 0.34.0).
pub(crate) const GEMINI_CLI_VERSION: &str = "0.34.0";
/// The `X-Goog-Api-Client` the Gemini CLI's node SDK sends.
pub(crate) const GEMINI_CLI_API_CLIENT: &str = "google-genai-sdk/1.41.0 gl-node/v22.19.0";
/// The onboarding calls' identity for the Gemini CLI (9router's `gemini.js`).
pub(crate) const ONBOARDING_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";
pub(crate) const ONBOARDING_API_CLIENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";

/// Cloud Code's `ClientMetadata.ideType` for the Antigravity IDE.
const IDE_ANTIGRAVITY: u8 = 9;
/// Cloud Code's `ClientMetadata.pluginType` for a Gemini plugin.
const PLUGIN_GEMINI: u8 = 2;

/// The placeholder Google documents for a `functionCall` whose thought
/// signature RAST never saw (Gemini 3+ refuses an unsigned replay).
const THOUGHT_SIGNATURE_PLACEHOLDER: &str = "skip_thought_signature_validator";

/// Which Cloud Code client a request presents as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Client {
    /// The Antigravity IDE.
    Antigravity,
    /// The Gemini CLI (the same Cloud Code host, its own client identity).
    GeminiCli,
}

impl Client {
    pub(crate) fn of(kind: Kind) -> Client {
        if kind == Kind::GeminiCli {
            Client::GeminiCli
        } else {
            Client::Antigravity
        }
    }
}

/// Cloud Code's platform enum, as the clients send it (the IDE uses the
/// binary's `ClientMetadata.Platform` values). The server's own platform is
/// reported; an unknown one is `UNSPECIFIED`.
pub(crate) fn platform_enum() -> u8 {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => 1,
        ("macos", "aarch64") => 2,
        ("linux", "x86_64") => 3,
        ("linux", "aarch64") => 4,
        ("windows", "x86_64") => 5,
        _ => 0,
    }
}

/// The `metadata` every Cloud Code client sends to `loadCodeAssist` and
/// `onboardUser` (both Google plans use the same numeric enum form).
pub(crate) fn metadata() -> Value {
    json!({
        "ideType": IDE_ANTIGRAVITY,
        "platform": platform_enum(),
        "pluginType": PLUGIN_GEMINI,
    })
}

/// The Gemini CLI's user agent: `GeminiCLI/<version>/<model> (<os>; <arch>;
/// terminal)`, as the CLI and 9router spell it.
pub(crate) fn gemini_cli_user_agent(model: &str) -> String {
    let model = if model.trim().is_empty() {
        "unknown"
    } else {
        model.trim()
    };
    format!(
        "GeminiCLI/{GEMINI_CLI_VERSION}/{model} ({}; {}; terminal)",
        node_os(),
        node_arch()
    )
}

fn node_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    }
}

/// The body of a streamed Cloud Code generation over `wire`.
pub(crate) fn body(req: &ChatRequest, wire: Wire, turns: &[Turn]) -> Value {
    let client = Client::of(req.kind);
    let mut request = json!({ "contents": contents(turns, &req.model) });
    if !req.system.trim().is_empty() {
        request["systemInstruction"] = json!({"parts": [{"text": req.system}]});
    }
    if !req.tools.is_empty() {
        request["tools"] = json!([{"functionDeclarations": declarations(&req.tools, &req.model)}]);
        request["toolConfig"] = json!({"functionCallingConfig": {"mode": "VALIDATED"}});
    }
    let generation = generation_config(req, wire);
    if generation
        .as_object()
        .is_some_and(|fields| !fields.is_empty())
    {
        request["generationConfig"] = generation;
    }
    if !req.session.is_empty() {
        request["sessionId"] = json!(req.session);
    }
    match client {
        Client::Antigravity => json!({
            "project": req.account,
            "requestId": antigravity_request_id(turns.len()),
            "model": req.model,
            "userAgent": "antigravity",
            "requestType": "agent",
            "request": request,
        }),
        Client::GeminiCli => json!({
            "project": req.account,
            "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
            "model": req.model,
            "userAgent": "gemini-cli",
            "request": request,
        }),
    }
}

/// Antigravity's request id: `agent/<conversation>/<millis>/<trajectory>/<step>`
/// (the 9router executor recognises that shape and keeps a caller's id).
fn antigravity_request_id(contents: usize) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let step = contents.max(1) * 2 - 1;
    format!(
        "agent/{}/{millis}/{}/{step}",
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4()
    )
}

/// The Gemini generation config: the request's limits, the sampling Google
/// still takes, and the model's thinking budget.
fn generation_config(req: &ChatRequest, wire: Wire) -> Value {
    let mut config = Map::new();
    if let Some(max) = req.max_output_tokens {
        // The Antigravity host rejects more than 64k output tokens per stream
        // (9router's cap); the Gemini CLI has no such published limit.
        let max = if req.kind == Kind::Antigravity {
            max.clamp(1, 64_000)
        } else {
            max.max(1)
        };
        config.insert("maxOutputTokens".into(), json!(max));
    }
    // Gemini 4 dropped the sampling parameters: forwarding them fails.
    if let Some(temperature) = req.temperature.filter(|_| !is_gemini_4(&req.model)) {
        config.insert("temperature".into(), json!(temperature));
    }
    if let Some(effort) = wire.effort {
        if let Some(thinking) = thinking_config(&req.model, effort) {
            config.insert("thinkingConfig".into(), thinking);
        }
    }
    Value::Object(config)
}

fn is_gemini_4(model: &str) -> bool {
    model.trim().starts_with("gemini-4")
}

/// The thinking budget a model family takes (the plugin's
/// `antigravity-thinking.ts`): Claude- and GPT-OSS-backed models fix a budget,
/// Gemini 3.x/4 tier it by effort, and an unknown model is left at Google's
/// default rather than guessing a level it rejects.
fn thinking_config(model: &str, effort: crate::ai::Effort) -> Option<Value> {
    let model = model.trim();
    let level = effort.as_str();
    let budget = if model.starts_with("claude-") {
        1024
    } else if model.starts_with("gpt-oss-") {
        8192
    } else if model.starts_with("gemini-3.1-pro") || model == "gemini-pro-agent" {
        if level == "high" || level == "max" {
            10001
        } else {
            1001
        }
    } else if model.starts_with("gemini-3.5-flash") || model == "gemini-3-flash-agent" {
        match level {
            "high" | "max" => 10_000,
            "medium" => 4000,
            _ => 1000,
        }
    } else if model.starts_with("gemini-") {
        match level {
            // -1 lets the model decide how long to think (its "dynamic" budget).
            "high" | "max" => -1,
            "medium" => 4000,
            _ => 1000,
        }
    } else {
        return None;
    };
    Some(json!({"includeThoughts": true, "thinkingBudget": budget}))
}

// ------------------------------------------------------------- contents

fn contents(turns: &[Turn], model: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(turns.len());
    let signed = wants_thought_signature(model);
    for turn in turns {
        match turn {
            Turn::User(text) => push_content(&mut out, "user", text_parts(text)),
            Turn::Assistant { text, calls, .. } => {
                let mut parts = Vec::with_capacity(calls.len() + 1);
                if !text.trim().is_empty() {
                    parts.push(json!({"text": text}));
                }
                for (index, call) in calls.iter().enumerate() {
                    let mut part = json!({
                        "functionCall": {
                            "id": call.id,
                            "name": call.name,
                            "args": call_arguments(&call.input),
                        }
                    });
                    // Gemini 3/4 check the first replayed call of a turn.
                    if signed && index == 0 {
                        part["thoughtSignature"] = json!(THOUGHT_SIGNATURE_PLACEHOLDER);
                    }
                    parts.push(part);
                }
                push_content(&mut out, "model", parts);
            }
            Turn::Results(results) => {
                let parts = results
                    .iter()
                    .map(|result| {
                        json!({"functionResponse": {
                            "id": result.id,
                            "name": result.name,
                            "response": tool_response(result),
                        }})
                    })
                    .collect();
                push_content(&mut out, "user", parts);
            }
        }
    }
    if out.is_empty() {
        // Gemini needs at least one content, and the first one is the person's.
        out.push(json!({"role": "user", "parts": [{"text": "."}]}));
    }
    out
}

/// Join a turn's parts into the content with the same role (Gemini takes one
/// content per role; RAST's history may have two user turns in a row).
fn push_content(out: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last["role"] == role {
            if let Some(existing) = last["parts"].as_array_mut() {
                existing.extend(parts);
                return;
            }
        }
    }
    out.push(json!({"role": role, "parts": parts}));
}

fn text_parts(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({"text": text})]
    }
}

/// A replayed call's arguments: Gemini's `args` is a protobuf `Struct`, so
/// only a JSON object survives (the generic layer keeps invalid arguments as
/// a string; those replay empty).
fn call_arguments(input: &Value) -> Value {
    if input.is_object() {
        input.clone()
    } else {
        json!({})
    }
}

/// The `functionResponse.response` value: a JSON object as the tool returned
/// it, anything else wrapped under `output` (a bare array or scalar is
/// rejected). Errors carry `isError`, as the other Cloud Code clients send.
fn tool_response(result: &ToolResult) -> Value {
    let output = serde_json::from_str::<Value>(&result.output)
        .unwrap_or_else(|_| Value::String(result.output.clone()));
    let mut response = match output {
        Value::Object(object) => object,
        other => {
            let mut object = Map::new();
            object.insert("output".into(), other);
            object
        }
    };
    if result.is_error {
        response.insert("isError".into(), json!(true));
    }
    Value::Object(response)
}

/// Gemini 3 and 4 refuse an unsigned replayed function call.
fn wants_thought_signature(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("gemini-3") || model.starts_with("gemini-4")
}

// ---------------------------------------------------------------- tools

/// A tool's `functionDeclarations` entry: the JSON Schema for Gemini models,
/// the older reduced `parameters` for the Claude/GPT-OSS ones.
fn declarations(tools: &[ToolSpec], model: &str) -> Vec<Value> {
    let legacy = model.starts_with("claude-") || model.starts_with("gpt-oss-");
    tools
        .iter()
        .map(|tool| {
            let mut declaration = json!({
                "name": tool.name,
                "description": tool.description,
            });
            let parameters = tool_parameters(&tool.parameters, legacy);
            let field = if legacy {
                "parameters"
            } else {
                "parametersJsonSchema"
            };
            declaration[field] = parameters;
            declaration
        })
        .collect()
}

/// One tool's parameter schema for the host: local references resolved
/// (Google's JSON-schema parser does not fetch `$defs`), the root forced to
/// an object, and for the legacy subset only the keywords it knows.
fn tool_parameters(schema: &Value, legacy: bool) -> Value {
    let mut resolved = resolve_refs(schema, schema, &mut Vec::new());
    if !resolved.is_object() {
        return json!({"type": "object", "properties": {}});
    }
    let object = resolved.as_object_mut().expect("checked above");
    if !object.contains_key("type") {
        object.insert("type".into(), json!("object"));
    }
    if legacy {
        legacy_schema(&resolved)
    } else {
        resolved
    }
}

/// Resolve local `$ref`s (`#/…`) against the schema itself and drop the
/// definition containers, which the host rejects. A reference that cannot be
/// resolved (remote, a cycle) is dropped rather than failing the request.
fn resolve_refs(value: &Value, root: &Value, seen: &mut Vec<String>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_refs(item, root, seen))
                .collect(),
        ),
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                if let Some(mut target) = follow_ref(reference, root, seen) {
                    if let Some(object) = target.as_object_mut() {
                        for (key, item) in fields {
                            if key != "$ref" {
                                object.insert(key.clone(), resolve_refs(item, root, seen));
                            }
                        }
                    }
                    return target;
                }
            }
            let mut out = Map::new();
            for (key, item) in fields {
                if matches!(
                    key.as_str(),
                    "$schema" | "$id" | "$defs" | "definitions" | "$ref"
                ) {
                    continue;
                }
                out.insert(key.clone(), resolve_refs(item, root, seen));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn follow_ref(reference: &str, root: &Value, seen: &mut Vec<String>) -> Option<Value> {
    let path = reference.strip_prefix("#/")?;
    if seen.iter().any(|known| known == reference) {
        return None;
    }
    let mut target = root;
    for segment in path.split('/') {
        let key = segment.replace("~1", "/").replace("~0", "~");
        target = target.get(key.as_str())?;
    }
    seen.push(reference.to_string());
    let resolved = resolve_refs(target, root, seen);
    seen.pop();
    Some(resolved)
}

/// The reduced schema the Claude/GPT-OSS bridges take: type, description,
/// properties, required, items and enum only; `allOf` merged, `anyOf`/`oneOf`
/// widened so no alternative is made uncallable.
fn legacy_schema(value: &Value) -> Value {
    let Some(fields) = value.as_object() else {
        return value.clone();
    };
    let mut schema = Map::new();
    if let Some(members) = fields.get("allOf").and_then(Value::as_array) {
        // An intersection holds every member's constraints at once.
        for member in members {
            absorb(&mut schema, member);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        let Some(variants) = fields.get(keyword).and_then(Value::as_array) else {
            continue;
        };
        let variants: Vec<Value> = variants
            .iter()
            .filter(|variant| variant.get("type") != Some(&json!("null")))
            .cloned()
            .collect();
        if variants.len() == 1 {
            absorb(&mut schema, &variants[0]);
        } else if !variants.is_empty() {
            absorb(&mut schema, &widen(&variants));
        }
    }
    for (key, value) in fields {
        match key.as_str() {
            "type" => match value.as_array() {
                Some(types) => {
                    let types: Vec<&Value> = types
                        .iter()
                        .filter(|kind| *kind != &json!("null"))
                        .collect();
                    if types.len() == 1 {
                        schema.insert("type".into(), types[0].clone());
                    } else if !types.is_empty()
                        && types
                            .iter()
                            .all(|kind| matches!(kind.as_str(), Some("integer" | "number")))
                    {
                        schema.insert("type".into(), json!("number"));
                    }
                }
                None => {
                    schema.insert("type".into(), value.clone());
                }
            },
            "properties" => {
                if let Some(properties) = value.as_object() {
                    let kept: Map<String, Value> = properties
                        .iter()
                        .map(|(name, property)| (name.clone(), legacy_schema(property)))
                        .collect();
                    schema.insert("properties".into(), Value::Object(kept));
                }
            }
            "items" => {
                schema.insert("items".into(), legacy_schema(value));
            }
            "description" | "required" => {
                schema.insert(key.clone(), value.clone());
            }
            "enum"
                if value
                    .as_array()
                    .is_some_and(|choices| choices.iter().all(Value::is_string)) =>
            {
                schema.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    Value::Object(schema)
}

/// Fold schema alternatives into one shape never narrower than any of them:
/// an object keeps every property and only the fields all require, an array
/// widens its items, scalars of one type keep it.
fn widen(variants: &[Value]) -> Value {
    let members: Vec<&Map<String, Value>> = variants.iter().filter_map(Value::as_object).collect();
    if members.is_empty() || members.len() != variants.len() {
        return json!({});
    }
    let types: Vec<Option<&str>> = members
        .iter()
        .map(|member| member.get("type").and_then(Value::as_str))
        .collect();
    if types.iter().any(Option::is_none) {
        return json!({});
    }
    let kinds: Vec<&str> = types.into_iter().flatten().collect();
    let unique: Vec<&str> = {
        let mut known: Vec<&str> = Vec::new();
        for kind in kinds {
            if !known.contains(&kind) {
                known.push(kind);
            }
        }
        known
    };
    let kind = if unique.len() == 1 {
        unique[0]
    } else if unique
        .iter()
        .all(|kind| matches!(*kind, "integer" | "number"))
    {
        "number"
    } else {
        return json!({});
    };
    if kind == "object" {
        let mut properties = Map::new();
        for member in &members {
            let Some(fields) = member.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (name, property) in fields {
                match properties.get(name) {
                    Some(known) if known == property => {}
                    Some(known) => {
                        let merged = widen(&[known.clone(), property.clone()]);
                        properties.insert(name.clone(), merged);
                    }
                    None => {
                        properties.insert(name.clone(), property.clone());
                    }
                }
            }
        }
        let required = shared_required(&members);
        let mut object = Map::new();
        object.insert("type".into(), json!(kind));
        if !properties.is_empty() {
            object.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            object.insert("required".into(), json!(required));
        }
        return Value::Object(object);
    }
    if kind == "array" {
        let items: Vec<Value> = members
            .iter()
            .map(|member| member.get("items").cloned().unwrap_or_else(|| json!({})))
            .collect();
        return json!({"type": "array", "items": widen(&items)});
    }
    let choices: Vec<Option<Vec<Value>>> = members
        .iter()
        .map(|member| string_choices(member))
        .collect();
    if choices.iter().all(Option::is_some) {
        let mut known: Vec<Value> = Vec::new();
        for choice in choices.into_iter().flatten() {
            for value in choice {
                if !known.contains(&value) {
                    known.push(value);
                }
            }
        }
        return json!({"type": kind, "enum": known});
    }
    json!({ "type": kind })
}

fn shared_required(members: &[&Map<String, Value>]) -> Vec<String> {
    let Some(first) = members.first() else {
        return Vec::new();
    };
    let first: Vec<String> = first
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    first
        .into_iter()
        .filter(|name| {
            members.iter().all(|member| {
                member
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|names| names.iter().any(|known| known == name))
            })
        })
        .collect()
}

fn string_choices(member: &Map<String, Value>) -> Option<Vec<Value>> {
    if let Some(constant) = member.get("const") {
        return constant.as_str().map(|value| vec![json!(value)]);
    }
    let choices = member.get("enum")?.as_array()?;
    if choices.iter().all(Value::is_string) {
        Some(choices.clone())
    } else {
        None
    }
}

fn absorb(schema: &mut Map<String, Value>, extra: &Value) {
    let Some(fields) = extra.as_object() else {
        return;
    };
    for (key, value) in fields {
        match key.as_str() {
            "properties" => {
                let properties = schema.entry("properties").or_insert_with(|| json!({}));
                if let (Some(into), Some(from)) = (properties.as_object_mut(), value.as_object()) {
                    for (name, property) in from {
                        into.insert(name.clone(), property.clone());
                    }
                }
            }
            "required" => {
                let required = schema.entry("required").or_insert_with(|| json!([]));
                if let (Some(into), Some(from)) = (required.as_array_mut(), value.as_array()) {
                    for name in from {
                        if !into.contains(name) {
                            into.push(name.clone());
                        }
                    }
                }
            }
            _ => {
                schema.insert(key.clone(), value.clone());
            }
        }
    }
}

// --------------------------------------------------------------- catalog

/// `fetchAvailableModels`: Cloud Code's model catalog for a signed-in
/// account, `{models: {"<id>": {…}}}` (or an array of `{id|model|name}`).
/// `identity` is the client's headers (the host fingerprints the caller).
pub(crate) fn fetch_available_models(
    base_url: &str,
    token: &str,
    account: &str,
    identity: &[(&'static str, String)],
) -> Result<Vec<String>, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Google's Cloud Code has no address; set its base URL".into());
    }
    let url = format!("{base}/v1internal:fetchAvailableModels");
    let body = json!({"project": account}).to_string();
    let mut request = agent(LIST_TIMEOUT)
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json");
    for (name, value) in identity {
        request = request.header(*name, value.clone());
    }
    let response = request
        .send(body.as_bytes())
        .map_err(|e| format!("cannot reach Google's Cloud Code: {e}"))?;
    let status = response.status().as_u16();
    let text = read_limited(response.into_body(), 4 * 1024 * 1024);
    if !(200..300).contains(&status) {
        return Err(format!(
            "Google's Cloud Code refused the model list (HTTP {status})"
        ));
    }
    parse_available_models(&text)
}

/// The catalog's usable model ids: internal, disabled, image and test routes
/// are left out, duplicates dropped, in the order the host lists them.
pub(crate) fn parse_available_models(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "Google's Cloud Code sent a model list RAST cannot read".to_string())?;
    let models = value
        .get("models")
        .ok_or("Google's Cloud Code sent no model list")?;
    let mut out: Vec<String> = Vec::new();
    match models {
        Value::Object(entries) => {
            for (id, model) in entries {
                if !usable_model(id) || hidden(model) {
                    continue;
                }
                push_unique(&mut out, id);
            }
        }
        Value::Array(entries) => {
            for entry in entries {
                let Some(id) = entry
                    .get("id")
                    .or_else(|| entry.get("model"))
                    .or_else(|| entry.get("name"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if !usable_model(id) || hidden(entry) {
                    continue;
                }
                push_unique(&mut out, id);
            }
        }
        _ => return Err("Google's Cloud Code sent no model list".into()),
    }
    out.truncate(MAX_MODELS);
    Ok(out)
}

/// Whether the catalog hides this entry (`isInternal`, `disabled`,
/// `selectable: false`).
fn hidden(model: &Value) -> bool {
    model.get("isInternal").and_then(Value::as_bool) == Some(true)
        || model.get("disabled").and_then(Value::as_bool) == Some(true)
        || model.get("selectable").and_then(Value::as_bool) == Some(false)
}

/// Whether a catalog key names a model rather than a tab, test, image or
/// voice route (the same exclusions the references apply).
fn usable_model(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_MODEL_NAME {
        return false;
    }
    let lower = id.to_ascii_lowercase();
    if lower.starts_with("tab_")
        || lower
            .strip_prefix("chat_")
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        || lower.ends_with("-disabled")
        || lower.contains("-image")
        || lower.contains("-tts")
        || lower.contains("imagen")
        || lower.contains("embed")
    {
        return false;
    }
    true
}

fn push_unique(out: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if !out.iter().any(|known| known == id) {
        out.push(id.to_string());
    }
}

// --------------------------------------------------------------- events

/// Where an event's payload is: Cloud Code wraps every one in `{"response":
/// …}`, but some errors and raw answers arrive unwrapped.
fn response_of(event: &Value) -> &Value {
    match event.get("response") {
        Some(response) if response.is_object() => response,
        _ => event,
    }
}

/// Cloud Code's event stream: one `data:` payload at a time, keeping what
/// the events report across lines (usage, done, the calls the reply asked
/// for).
#[derive(Debug, Default)]
pub struct Parser {
    /// Tokens the provider reported.
    pub usage: Usage,
    /// The reply is done (`response.candidates[0].finishReason`).
    pub done: bool,
    /// Calls the reply asked for, in order.
    calls: Vec<ToolCall>,
}

impl Parser {
    /// One `data:` payload (already parsed as JSON).
    pub(crate) fn line(&mut self, event: &Value) -> Result<Vec<Delta>, String> {
        let response = response_of(event);
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            return Err(error_text(error));
        }
        if let Some(reason) = response
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            return Err(format!("Google blocked the request ({reason})"));
        }
        if let Some(metadata) = response
            .get("usageMetadata")
            .filter(|usage| usage.is_object())
        {
            self.usage = usage_of(metadata);
        }
        let mut out = Vec::new();
        let candidate = response.pointer("/candidates/0");
        if let Some(parts) = candidate
            .and_then(|candidate| candidate.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                self.part(part, &mut out);
            }
        }
        if let Some(reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            self.done = true;
            if matches!(
                reason,
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
            ) {
                return Err(format!("Google blocked the reply ({reason})"));
            }
        }
        Ok(out)
    }

    /// One content part: answer text, a thought (reasoning), or a complete
    /// function call. Images the model may return are not part of RAST's
    /// reply model and are left out.
    fn part(&mut self, part: &Value, out: &mut Vec<Delta>) {
        let thought = part.get("thought").and_then(Value::as_bool) == Some(true);
        if let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            out.push(if thought {
                Delta::Reasoning(text.to_string())
            } else {
                Delta::Text(text.to_string())
            });
        }
        if let Some(call) = part.get("functionCall").filter(|call| call.is_object()) {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = match call.get("args") {
                None | Some(Value::Null) => json!({}),
                Some(Value::Object(_)) => call["args"].clone(),
                Some(Value::String(text)) => serde_json::from_str(text)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({})),
                Some(_) => json!({}),
            };
            self.calls.push(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name,
                input,
            });
        }
    }

    /// The calls the reply asked for, in order (ids made up where the
    /// provider sent none or repeated one).
    pub fn take_calls(&mut self) -> Vec<ToolCall> {
        let mut used: Vec<String> = Vec::new();
        std::mem::take(&mut self.calls)
            .into_iter()
            .enumerate()
            .map(|(index, mut call)| {
                if call.id.is_empty() || used.contains(&call.id) {
                    let mut id = format!("call_{index}");
                    while used.contains(&id) {
                        id.push('_');
                    }
                    call.id = id;
                }
                used.push(call.id.clone());
                call
            })
            .collect()
    }
}

/// Gemini's usage metadata: `promptTokenCount` is all input (cached included,
/// as RAST counts it); `candidatesTokenCount` the reply's tokens;
/// `thoughtsTokenCount` is already part of the candidates count.
fn usage_of(metadata: &Value) -> Usage {
    let part = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0)
    };
    Usage {
        input: part("promptTokenCount"),
        output: part("candidatesTokenCount"),
        cache_read: part("cachedContentTokenCount"),
        cache_write: 0,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Effort, Protocol};

    fn request(kind: Kind, model: &str) -> ChatRequest {
        ChatRequest {
            kind,
            base_url: "https://cloudcode-pa.googleapis.com".into(),
            api_key: None,
            model: model.into(),
            system: "be brief".into(),
            messages: vec![Turn::User("hi".into())],
            tools: Vec::new(),
            temperature: Some(0.2),
            max_output_tokens: Some(8192),
            effort: None,
            session: "conv-1".into(),
            account: "project-1".into(),
        }
    }

    fn wire(kind: Kind, effort: Option<Effort>) -> Wire {
        Wire {
            protocol: Protocol::CloudCode,
            effort,
            ..Wire::of(kind)
        }
    }

    fn call(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    fn user(text: &str) -> Turn {
        Turn::User(text.into())
    }

    /// The envelope the Antigravity IDE's client sends: the Cloud Code
    /// project, the IDE identity, a request id in the shape the host expects
    /// and the Gemini request inside.
    #[test]
    fn the_antigravity_envelope_is_the_ides() {
        let req = request(Kind::Antigravity, "gemini-3-pro-preview");
        let envelope = body(
            &req,
            wire(Kind::Antigravity, Some(Effort::High)),
            &[user("hi")],
        );
        assert_eq!(envelope["project"], "project-1");
        assert_eq!(envelope["model"], "gemini-3-pro-preview");
        assert_eq!(envelope["userAgent"], "antigravity");
        assert_eq!(envelope["requestType"], "agent");
        let request_id = envelope["requestId"].as_str().expect("a request id");
        let pieces: Vec<&str> = request_id.split('/').collect();
        assert_eq!(pieces.len(), 5, "{request_id}");
        assert_eq!(pieces[0], "agent");
        assert!(
            pieces[2].chars().all(|c| c.is_ascii_digit()),
            "{request_id}"
        );
        assert_eq!(pieces[4], "1", "one content is one step");
        let inner = &envelope["request"];
        assert_eq!(
            inner["contents"],
            json!([{"role": "user", "parts": [{"text": "hi"}]}])
        );
        assert_eq!(
            inner["systemInstruction"],
            json!({"parts": [{"text": "be brief"}]})
        );
        assert_eq!(inner["sessionId"], "conv-1");
        assert_eq!(inner["generationConfig"]["maxOutputTokens"], 8192);
        assert_eq!(inner["generationConfig"]["temperature"], 0.2);
        // gemini-3.x budgets: high asks for the model's own dynamic budget.
        assert_eq!(
            inner["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingBudget": -1})
        );
    }

    /// The Gemini CLI's envelope names its own client and has none of the
    /// IDE's fields; the output-token cap applies only to Antigravity.
    #[test]
    fn the_gemini_cli_envelope_is_its_own() {
        let mut req = request(Kind::GeminiCli, "gemini-2.5-pro");
        req.max_output_tokens = Some(200_000);
        let envelope = body(&req, wire(Kind::GeminiCli, None), &[user("hi")]);
        assert_eq!(envelope["userAgent"], "gemini-cli");
        assert_eq!(envelope["project"], "project-1");
        assert_eq!(envelope["model"], "gemini-2.5-pro");
        assert!(envelope.get("requestType").is_none());
        let request_id = envelope["requestId"].as_str().expect("a request id");
        assert!(request_id.starts_with("agent-"), "{request_id}");
        assert_eq!(
            envelope.pointer("/request/generationConfig/maxOutputTokens"),
            Some(&json!(200_000))
        );
        assert_eq!(
            envelope.pointer("/request/generationConfig/temperature"),
            Some(&json!(0.2))
        );
        assert!(envelope
            .pointer("/request/generationConfig/thinkingConfig")
            .is_none());
        // Antigravity caps what the IDE's host takes (9router's 64000).
        let mut capped = request(Kind::Antigravity, "gemini-3-pro-preview");
        capped.max_output_tokens = Some(200_000);
        let envelope = body(&capped, wire(Kind::Antigravity, None), &[user("hi")]);
        assert_eq!(
            envelope.pointer("/request/generationConfig/maxOutputTokens"),
            Some(&json!(64_000))
        );
    }

    /// Tool history: calls with their ids and arguments, results as a JSON
    /// object under `functionResponse`, and the placeholder signature Gemini 3
    /// and 4 demand on the first replayed call.
    #[test]
    fn tool_history_is_gemini_shaped() {
        let turns = vec![
            user("what is it?"),
            Turn::Assistant {
                reasoning: "think".into(),
                text: "looking".into(),
                calls: vec![
                    call("c1", "read_file", json!({"path": "/a"})),
                    call("c2", "run", json!({"cmd": "ls"})),
                ],
            },
            Turn::Results(vec![
                ToolResult {
                    id: "c1".into(),
                    name: "read_file".into(),
                    output: r#"{"content":"hello"}"#.into(),
                    is_error: false,
                },
                ToolResult {
                    id: "c2".into(),
                    name: "run".into(),
                    output: "boom".into(),
                    is_error: true,
                },
            ]),
        ];
        let req = request(Kind::Antigravity, "gemini-3.1-pro-high");
        let envelope = body(&req, wire(Kind::Antigravity, None), &turns);
        let contents = envelope["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "user, model, user");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0], json!({"text": "looking"}));
        assert_eq!(
            contents[1]["parts"][1]["functionCall"],
            json!({"id": "c1", "name": "read_file", "args": {"path": "/a"}})
        );
        assert_eq!(
            contents[1]["parts"][1]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        // Only the first call of the turn needs the signature.
        assert!(contents[1]["parts"][2].get("thoughtSignature").is_none());
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"],
            json!({
                "id": "c1",
                "name": "read_file",
                "response": {"content": "hello"},
            })
        );
        // Invalid JSON and errors are wrapped, and marked.
        assert_eq!(
            contents[2]["parts"][1]["functionResponse"],
            json!({
                "id": "c2",
                "name": "run",
                "response": {"output": "boom", "isError": true},
            })
        );
        // A Gemini 2.5 model replays without a signature.
        let req = request(Kind::Antigravity, "gemini-2.5-pro");
        let envelope = body(&req, wire(Kind::Antigravity, None), &turns[..2]);
        assert!(envelope
            .pointer("/request/contents/1/parts/1/thoughtSignature")
            .is_none());
    }

    /// Gemini models get JSON Schema; the Claude/GPT-OSS ones take the older
    /// reduced `parameters` subset, with local references resolved.
    #[test]
    fn tool_schemas_follow_the_model_family() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "path": {"$ref": "#/$defs/path"},
                "mode": {"anyOf": [{"type": "string", "enum": ["read", "write"]}, {"type": "null"}]},
                "limit": {"type": ["integer", "number"]},
                "meta": {"type": "object", "additionalProperties": true},
            },
            "required": ["path"],
            "$defs": {"path": {"type": "string", "description": "a path"}},
        });
        let mut req = request(Kind::Antigravity, "gemini-3-pro-preview");
        req.tools = vec![ToolSpec {
            name: "read_file".into(),
            description: "read it".into(),
            parameters: schema,
        }];
        let envelope = body(&req, wire(Kind::Antigravity, None), &[]);
        let declaration = &envelope["request"]["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["name"], "read_file");
        assert_eq!(declaration["description"], "read it");
        let parameters = &declaration["parametersJsonSchema"];
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["properties"]["path"]["type"], "string");
        assert_eq!(parameters["properties"]["path"]["description"], "a path");
        assert!(parameters.get("$defs").is_none());
        // anyOf is valid JSON Schema and stays as it is.
        assert!(parameters["properties"]["mode"]["anyOf"].is_array());
        assert_eq!(
            envelope.pointer("/request/toolConfig/functionCallingConfig/mode"),
            Some(&json!("VALIDATED"))
        );
        // The Claude-backed models take the reduced subset under `parameters`.
        req.model = "claude-sonnet-4-5".into();
        let envelope = body(&req, wire(Kind::Antigravity, None), &[]);
        let declaration = &envelope["request"]["tools"][0]["functionDeclarations"][0];
        assert!(declaration.get("parametersJsonSchema").is_none());
        let parameters = &declaration["parameters"];
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["properties"]["path"]["type"], "string");
        assert_eq!(parameters["properties"]["path"]["description"], "a path");
        // One alternative left (the null branch dropped): its shape is used.
        assert_eq!(
            parameters["properties"]["mode"],
            json!({"type": "string", "enum": ["read", "write"]})
        );
        // An integer/number union widens to number; unknown keywords go.
        assert_eq!(parameters["properties"]["limit"]["type"], "number");
        assert_eq!(parameters["properties"]["meta"], json!({"type": "object"}));
        assert_eq!(parameters["required"], json!(["path"]));
    }

    /// The thinking budgets per model family (the plugin's mapping), and none
    /// for a model RAST does not know.
    #[test]
    fn thinking_budgets_follow_the_model_family() {
        let budget = |model: &str, effort: Effort| thinking_config(model, effort);
        assert_eq!(
            budget("claude-sonnet-4-5", Effort::Low),
            Some(json!({"includeThoughts": true, "thinkingBudget": 1024}))
        );
        assert_eq!(
            budget("gpt-oss-120b", Effort::Medium),
            Some(json!({"includeThoughts": true, "thinkingBudget": 8192}))
        );
        assert_eq!(
            budget("gemini-3.1-pro", Effort::High),
            Some(json!({"includeThoughts": true, "thinkingBudget": 10001}))
        );
        assert_eq!(
            budget("gemini-pro-agent", Effort::Low),
            Some(json!({"includeThoughts": true, "thinkingBudget": 1001}))
        );
        assert_eq!(
            budget("gemini-3.5-flash", Effort::Medium),
            Some(json!({"includeThoughts": true, "thinkingBudget": 4000}))
        );
        assert_eq!(
            budget("gemini-2.5-pro", Effort::High),
            Some(json!({"includeThoughts": true, "thinkingBudget": -1}))
        );
        assert_eq!(
            budget("gemini-4-hydrogen-high", Effort::Low),
            Some(json!({"includeThoughts": true, "thinkingBudget": 1000}))
        );
        assert_eq!(budget("mystery-model", Effort::High), None);
    }

    /// The stream: text, thoughts, complete calls, usage and the finish
    /// reason, one wrapped event at a time.
    #[test]
    fn the_event_stream_is_read() {
        let mut parser = Parser::default();
        let mut deltas = Vec::new();
        deltas.extend(
            parser
                .line(&json!({"response": {"candidates": [{"content": {"parts": [
                    {"text": "Hel", "thought": false},
                    {"text": "thinking", "thought": true},
                ]}}]}}))
                .unwrap(),
        );
        deltas.extend(
            parser
                .line(&json!({"response": {"candidates": [{"content": {"parts": [
                    {"text": "lo"},
                    {"functionCall": {"id": "c1", "name": "get_time", "args": {"zone": "UTC"}}},
                    {"functionCall": {"name": "no_id", "args": {}}},
                ]}}], "usageMetadata": {
                    "promptTokenCount": 12,
                    "candidatesTokenCount": 5,
                    "thoughtsTokenCount": 3,
                    "cachedContentTokenCount": 8,
                    "totalTokenCount": 20,
                }}}))
                .unwrap(),
        );
        assert_eq!(
            deltas,
            vec![
                Delta::Text("Hel".into()),
                Delta::Reasoning("thinking".into()),
                Delta::Text("lo".into()),
            ]
        );
        assert_eq!(
            parser.usage,
            Usage {
                input: 12,
                output: 5,
                cache_read: 8,
                cache_write: 0,
            }
        );
        assert!(!parser.done);
        let calls = parser.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], call("c1", "get_time", json!({"zone": "UTC"})));
        assert_eq!(calls[1].name, "no_id");
        assert!(!calls[1].id.is_empty(), "an id is made up");
        // The last event finishes the reply; unwrapped payloads are read too.
        let rest = parser
            .line(&json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]}))
            .unwrap();
        assert!(rest.is_empty());
        assert!(parser.done);
        assert!(parser.take_calls().is_empty(), "calls are taken once");
    }

    /// A provider error inside the envelope, and a reply the safety filter
    /// blocked, are reported as errors rather than empty replies.
    #[test]
    fn errors_and_blocks_are_reported() {
        let mut parser = Parser::default();
        assert_eq!(
            parser
                .line(&json!({"response": {"error": {"code": 429, "message": "quota used up"}}}))
                .unwrap_err(),
            "quota used up"
        );
        assert_eq!(
            parser
                .line(&json!({"candidates": [{"content": {"parts": []}, "finishReason": "SAFETY"}], "usageMetadata": {"promptTokenCount": 1}}))
                .unwrap_err(),
            "Google blocked the reply (SAFETY)"
        );
        assert!(parser.done);
        assert_eq!(
            parser
                .line(&json!({"response": {"promptFeedback": {"blockReason": "SAFETY"}}}))
                .unwrap_err(),
            "Google blocked the request (SAFETY)"
        );
    }

    /// The catalog: the host's `models` map, with internal/disabled/tab/image
    /// routes and duplicates left out; the array form and bad bodies too.
    #[test]
    fn the_model_catalog_is_read() {
        let fixture = r#"{
            "models": {
                "gemini-3-pro-high": {"displayName": "Gemini 3 Pro (High)"},
                "claude-sonnet-4-5": {"displayName": "Claude Sonnet 4.5"},
                "internal-model": {"displayName": "x", "isInternal": true},
                "retired-model": {"displayName": "x", "disabled": true},
                "gemini-3-flash-image": {"displayName": "image"},
                "tab_completion": {"displayName": "tab"},
                "chat_1234": {"displayName": "chat"},
                "gemini-3-flash": {"displayName": "Gemini 3 Flash"},
                "hidden": {"displayName": "x", "selectable": false}
            }
        }"#;
        // serde_json keeps object keys sorted (no preserve_order feature), so
        // the catalog comes out alphabetically; the order is not meaningful.
        assert_eq!(
            parse_available_models(fixture).unwrap(),
            vec!["claude-sonnet-4-5", "gemini-3-flash", "gemini-3-pro-high"]
        );
        let array = r#"{"models": [
            {"id": "gemini-3-pro-preview"},
            {"model": "gemini-2.5-pro"},
            {"name": "gemini-2.5-flash"},
            {"id": "gemini-2.5-pro"},
            {"id": "gemini-2.5-flash-image"}
        ]}"#;
        assert_eq!(
            parse_available_models(array).unwrap(),
            vec!["gemini-3-pro-preview", "gemini-2.5-pro", "gemini-2.5-flash"]
        );
        assert!(parse_available_models("not json").is_err());
        assert!(parse_available_models(r#"{"data": []}"#).is_err());
        assert_eq!(
            parse_available_models(r#"{"models": {}}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    /// The client identities are the constants the references pin (the Gemini
    /// CLI user agent names the model and the host's platform).
    #[test]
    fn client_identities_are_the_references() {
        assert!(ANTIGRAVITY_USER_AGENT.starts_with("antigravity/cli/1.3.4 (aidev_client; "));
        let user_agent = gemini_cli_user_agent("gemini-3-pro-preview");
        assert!(
            user_agent.starts_with("GeminiCLI/0.34.0/gemini-3-pro-preview ("),
            "{user_agent}"
        );
        assert!(user_agent.ends_with("; terminal)"), "{user_agent}");
        assert!(user_agent.contains(node_os()), "{user_agent}");
        assert!(gemini_cli_user_agent("").contains("/unknown ("));
        assert_eq!(metadata()["ideType"], 9);
        assert_eq!(metadata()["pluginType"], 2);
        assert!(platform_enum() <= 5);
        assert_eq!(Client::of(Kind::Antigravity), Client::Antigravity);
        assert_eq!(Client::of(Kind::GeminiCli), Client::GeminiCli);
    }
}
