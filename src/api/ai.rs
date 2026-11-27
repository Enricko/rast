//! AI Agent API (`/api/ai/*`, app `agent`; see [`crate::ai`]).
//!
//! People who may open the app chat in their own conversations
//! (`/api/ai/options`, `/api/ai/conversations…`, `/api/ai/usage`).
//! Administrators also set up providers, agents, skills and limits, and read
//! every account's conversations (each read of someone else's conversation
//! is audited). Tool calls wait for the conversation's owner:
//! `POST /api/ai/conversations/{id}/tools/{callId}`.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    ai::{self, provider, run::ReplyJob, tools, Kind, LimitRule},
    audit::{self, AuditLog},
    auth::{AdminUser, AuthUser, ClientIp},
    db::now_ms,
    error::{AppError, AppResult},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ai/options", get(options))
        .route("/api/ai/usage", get(my_usage))
        .route(
            "/api/ai/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route(
            "/api/ai/conversations/{id}",
            get(get_conversation)
                .patch(update_conversation)
                .delete(delete_conversation),
        )
        .route("/api/ai/conversations/{id}/messages", post(send_message))
        .route("/api/ai/conversations/{id}/stop", post(stop_reply))
        .route(
            "/api/ai/conversations/{id}/tools/{call_id}",
            post(decide_tool),
        )
        .route("/api/ai/search", get(get_search).put(put_search))
        .route("/api/ai/search/test", post(test_search))
        .route("/api/ai/server-access", put(put_server_access))
        .route("/api/ai/admin/overview", get(admin_overview))
        .route("/api/ai/admin/conversations", get(admin_conversations))
        .route("/api/ai/admin/usage/{user_id}", get(admin_usage))
        .route("/api/ai/providers", post(create_provider))
        .route("/api/ai/providers/models", post(probe_models))
        .route("/api/ai/providers/sign-in", post(start_sign_in))
        .route("/api/ai/providers/sign-in/finish", post(finish_sign_in))
        .route("/api/ai/providers/sign-in/poll", post(poll_sign_in))
        .route(
            "/api/ai/providers/import/claude-code",
            post(import_claude_code),
        )
        .route("/api/ai/providers/{id}/sign-out", post(sign_out_provider))
        .route("/api/ai/providers/{id}/prices", put(put_model_price))
        .route("/api/ai/prices", get(get_prices))
        .route("/api/ai/prices/refresh", post(refresh_prices))
        .route(
            "/api/ai/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .route("/api/ai/agents", post(create_agent))
        .route(
            "/api/ai/agents/{id}",
            put(update_agent).delete(delete_agent),
        )
        .route("/api/ai/skills", post(create_skill))
        .route(
            "/api/ai/skills/{id}",
            put(update_skill).delete(delete_skill),
        )
        .route(
            "/api/ai/limits/{subject}",
            put(put_limit).delete(delete_limit),
        )
}

// ------------------------------------------------------------- helpers

fn required(value: &str, what: &str, max: usize) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::bad_request(format!("{what} is required")));
    }
    bounded(value, what, max)
}

fn bounded(value: &str, what: &str, max: usize) -> AppResult<String> {
    if value.chars().count() > max {
        return Err(AppError::bad_request(format!(
            "{what} is longer than {max} characters"
        )));
    }
    Ok(value.to_string())
}

fn models_list(models: &[String]) -> AppResult<Vec<String>> {
    if models.len() > ai::MAX_MODELS {
        return Err(AppError::bad_request(format!(
            "at most {} models per provider",
            ai::MAX_MODELS
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() {
            continue;
        }
        if model.len() > ai::MAX_MODEL_NAME || model.chars().any(char::is_control) {
            return Err(AppError::bad_request(format!(
                "{model:?} is not a model name"
            )));
        }
        if !out.iter().any(|m| m == model) {
            out.push(model.to_string());
        }
    }
    Ok(out)
}

/// Tell the app's open pages that the setup changed.
fn config_changed(state: &AppState, what: &str) {
    state.events.publish("ai.config", json!({ "what": what }));
}

async fn audit_admin(
    state: &AppState,
    admin: &AuthUser,
    ip: &ClientIp,
    action: &str,
    kind: &str,
    id: &str,
    name: &str,
) {
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new(action)
            .actor(admin)
            .object(kind, id)
            .path(name)
            .success()
            .ip(ip.as_string()),
    )
    .await;
}

/// The conversation, if `user` owns it (administrators: any, read-only).
async fn owned(
    state: &AppState,
    user: &AuthUser,
    id: &str,
    write: bool,
) -> AppResult<ai::Conversation> {
    let conversation = ai::conversation(&state.db, id)
        .await?
        .ok_or(AppError::NotFound("conversation not found"))?;
    if conversation.user_id == user.id {
        return Ok(conversation);
    }
    if user.is_admin() && !write {
        return Ok(conversation);
    }
    if user.is_admin() {
        return Err(AppError::Forbidden(
            "administrators can read other accounts' conversations but not write in them",
        ));
    }
    Err(AppError::NotFound("conversation not found"))
}

/// Provider and model a conversation uses, checked against the account's
/// limits: its own choice, else its agent's.
struct Resolved {
    provider: ai::Provider,
    model: String,
    agent: Option<ai::Agent>,
}

async fn resolve(
    state: &AppState,
    user: &AuthUser,
    agent_id: Option<&str>,
    provider_id: Option<&str>,
    model: &str,
) -> AppResult<Resolved> {
    let agent = match agent_id {
        Some(id) => {
            let agent = ai::agent(&state.db, id)
                .await?
                .ok_or_else(|| AppError::bad_request("that agent no longer exists"))?;
            if !agent.enabled && !user.is_admin() {
                return Err(AppError::bad_request("that agent is turned off"));
            }
            Some(agent)
        }
        None => None,
    };
    let provider_id = provider_id
        .map(str::to_string)
        .or_else(|| agent.as_ref().and_then(|a| a.provider_id.clone()))
        .ok_or_else(|| AppError::bad_request("pick a model first"))?;
    let model = if model.trim().is_empty() {
        agent.as_ref().map(|a| a.model.clone()).unwrap_or_default()
    } else {
        model.trim().to_string()
    };
    if model.is_empty() {
        return Err(AppError::bad_request("pick a model first"));
    }
    let provider = ai::provider(&state.db, &provider_id)
        .await?
        .ok_or_else(|| AppError::bad_request("that provider no longer exists"))?;
    if !provider.enabled {
        return Err(AppError::bad_request(format!(
            "{} is turned off",
            provider.name
        )));
    }
    if !provider.usable_by(user.id) {
        return Err(AppError::Forbidden(
            "only the account that signed in to this plan may use it",
        ));
    }
    if !user.is_admin() {
        let limits = ai::effective_limits(&state.db, user).await?;
        if !limits.allows(&provider.id) {
            return Err(AppError::Forbidden(
                "your account may not use this provider",
            ));
        }
        let agents_model = agent.as_ref().is_some_and(|a| {
            a.provider_id.as_deref() == Some(provider.id.as_str()) && a.model == model
        });
        if !agents_model && !provider.models.contains(&model) {
            return Err(AppError::bad_request(
                "that model is not offered; pick another one",
            ));
        }
    }
    Ok(Resolved {
        provider,
        model,
        agent,
    })
}

// --------------------------------------------------------------- people

/// GET /api/ai/options — what the account can chat with, and its allowance.
async fn options(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let limits = ai::effective_limits(&state.db, &user).await?;
    let providers: Vec<ai::Provider> = ai::providers(&state.db)
        .await?
        .into_iter()
        .filter(|p| p.enabled && limits.allows(&p.id) && p.usable_by(user.id))
        .collect();
    let agents: Vec<Value> = ai::agents(&state.db)
        .await?
        .into_iter()
        .filter(|a| a.enabled)
        .filter(|a| {
            a.provider_id
                .as_deref()
                .is_none_or(|id| providers.iter().any(|p| p.id == id))
        })
        .map(|a| a.public_json())
        .collect();
    let configured: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_providers")
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({
        "isAdmin": user.is_admin(),
        "configured": configured > 0,
        "providers": providers.iter().map(ai::Provider::public_json).collect::<Vec<_>>(),
        "agents": agents,
        "allowance": ai::allowance(&state.db, user.id, &limits).await?,
    })))
}

/// GET /api/ai/usage
async fn my_usage(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let limits = ai::effective_limits(&state.db, &user).await?;
    Ok(Json(json!({
        "limits": limits,
        "allowance": ai::allowance(&state.db, user.id, &limits).await?,
    })))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<i64>,
    #[serde(rename = "userId")]
    user_id: Option<i64>,
}

/// GET /api/ai/conversations — the account's own, newest first.
async fn list_conversations(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let pattern = format!("%{}%", query.q.as_deref().unwrap_or("").trim());
    let rows = sqlx::query(
        "SELECT c.*, (SELECT status FROM ai_messages m WHERE m.conversation_id = c.id
                      ORDER BY m.id DESC LIMIT 1) AS last_status
         FROM ai_conversations c WHERE c.user_id = ? AND c.title LIKE ?
         ORDER BY c.updated_at DESC LIMIT ?",
    )
    .bind(user.id)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(ai::conversation_from_row)
        .map(|c| {
            let running = state.ai.running(&c.id);
            c.json(None, running)
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NewConversation {
    agent_id: Option<String>,
    provider_id: Option<String>,
    #[serde(default)]
    model: String,
    /// How hard the model thinks (`""`: the provider's default).
    #[serde(default)]
    effort: String,
    /// Tools that run without asking from the start.
    #[serde(default)]
    allowed_tools: Vec<String>,
}

/// Tools that may run without asking, without repeats.
fn allowed_tool_list(list: &[String]) -> AppResult<Vec<String>> {
    let mut allowed: Vec<String> = Vec::new();
    for name in list {
        if !tools::may_skip_asking(name) {
            return Err(AppError::bad_request(format!(
                "{name:?} cannot run without asking"
            )));
        }
        if !allowed.contains(name) {
            allowed.push(name.clone());
        }
    }
    Ok(allowed)
}

/// POST /api/ai/conversations
async fn create_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<NewConversation>,
) -> AppResult<Json<Value>> {
    let effort = ai::Effort::parse(&body.effort).map_err(AppError::bad_request)?;
    let allowed_tools = allowed_tool_list(&body.allowed_tools)?;
    let resolved = resolve(
        &state,
        &user,
        body.agent_id.as_deref(),
        body.provider_id.as_deref(),
        &body.model,
    )
    .await?;
    let now = now_ms();
    let conversation = ai::Conversation {
        id: ai::new_id(),
        user_id: user.id,
        agent_id: resolved.agent.as_ref().map(|a| a.id.clone()),
        provider_id: Some(resolved.provider.id.clone()),
        model: resolved.model.clone(),
        title: String::new(),
        allowed_tools,
        effort,
        waiting: false,
        created_at: now,
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO ai_conversations (id, user_id, agent_id, provider_id, model, title, allowed_tools,
                                       effort, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, '', ?, ?, ?, ?)",
    )
    .bind(&conversation.id)
    .bind(user.id)
    .bind(&conversation.agent_id)
    .bind(&conversation.provider_id)
    .bind(&conversation.model)
    .bind(serde_json::to_string(&conversation.allowed_tools).unwrap_or_else(|_| "[]".into()))
    .bind(conversation.effort.map_or("", ai::Effort::as_str))
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await?;
    let value = conversation.json(None, false);
    state.events.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), false) }),
    );
    Ok(Json(json!({ "conversation": value })))
}

/// GET /api/ai/conversations/{id} — with its messages; a reply being
/// written comes with what it has so far (`seq`: deltas already in it).
async fn get_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let conversation = owned(&state, &user, &id, false).await?;
    let mut owner: Option<String> = None;
    if conversation.user_id != user.id {
        let name: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(conversation.user_id)
            .fetch_optional(&state.db)
            .await?;
        audit::record_with_events(
            &state.db,
            &state.events,
            AuditLog::new("ai.conversation.view")
                .actor(&user)
                .object("ai_conversation", &conversation.id)
                .path(format!(
                    "{} — {}",
                    name.as_deref().unwrap_or("?"),
                    conversation.title
                ))
                .success()
                .ip(ip.as_string()),
        )
        .await;
        owner = name;
    }
    let active = state.ai.get(&conversation.id);
    let mut messages: Vec<Value> = ai::messages(&state.db, &conversation.id)
        .await?
        .iter()
        .map(ai::Message::json)
        .collect();
    let mut seq = 0u64;
    if let Some(run) = &active {
        let snapshot = run.snapshot();
        seq = snapshot.seq;
        let message_id = run.message_id();
        for message in &mut messages {
            if message["id"] == message_id && message["status"] == "streaming" {
                message["content"] = Value::Array(snapshot.blocks.clone());
            }
        }
    }
    Ok(Json(json!({
        "conversation": conversation.json(owner.as_deref(), active.is_some()),
        "messages": messages,
        "seq": seq,
        "readOnly": conversation.user_id != user.id,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConversationPatch {
    title: Option<String>,
    /// Switch agent (`""`: none) and/or model.
    agent_id: Option<String>,
    provider_id: Option<String>,
    model: Option<String>,
    /// Tools that run without asking in this chat (`[]`: ask for every call).
    allowed_tools: Option<Vec<String>>,
    /// How hard the model thinks (`""`: the provider's default).
    effort: Option<String>,
}

/// PATCH /api/ai/conversations/{id}
async fn update_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<ConversationPatch>,
) -> AppResult<Json<Value>> {
    let mut conversation = owned(&state, &user, &id, true).await?;
    if let Some(title) = &body.title {
        conversation.title = bounded(title.trim(), "the title", 200)?;
    }
    if let Some(list) = &body.allowed_tools {
        conversation.allowed_tools = allowed_tool_list(list)?;
    }
    if let Some(effort) = &body.effort {
        conversation.effort = ai::Effort::parse(effort).map_err(AppError::bad_request)?;
    }
    if body.agent_id.is_some() || body.provider_id.is_some() || body.model.is_some() {
        let agent_id = match body.agent_id.as_deref() {
            Some("") => None,
            Some(id) => Some(id.to_string()),
            None => conversation.agent_id.clone(),
        };
        // A new agent brings its own model unless one is given.
        let agent_changed = body.agent_id.is_some() && agent_id != conversation.agent_id;
        let provider_id = body.provider_id.clone().or_else(|| {
            if agent_changed {
                None
            } else {
                conversation.provider_id.clone()
            }
        });
        let model = body.model.clone().unwrap_or_else(|| {
            if agent_changed {
                String::new()
            } else {
                conversation.model.clone()
            }
        });
        let resolved = resolve(
            &state,
            &user,
            agent_id.as_deref(),
            provider_id.as_deref(),
            &model,
        )
        .await?;
        conversation.agent_id = resolved.agent.map(|a| a.id);
        conversation.provider_id = Some(resolved.provider.id);
        conversation.model = resolved.model;
    }
    conversation.updated_at = now_ms();
    sqlx::query(
        "UPDATE ai_conversations SET title = ?, agent_id = ?, provider_id = ?, model = ?, allowed_tools = ?,
         effort = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&conversation.title)
    .bind(&conversation.agent_id)
    .bind(&conversation.provider_id)
    .bind(&conversation.model)
    .bind(serde_json::to_string(&conversation.allowed_tools).unwrap_or_else(|_| "[]".into()))
    .bind(conversation.effort.map_or("", ai::Effort::as_str))
    .bind(conversation.updated_at)
    .bind(&conversation.id)
    .execute(&state.db)
    .await?;
    let running = state.ai.running(&conversation.id);
    state.events.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), running) }),
    );
    Ok(Json(
        json!({ "conversation": conversation.json(None, running) }),
    ))
}

/// DELETE /api/ai/conversations/{id} (what it used stays counted).
async fn delete_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let conversation = owned(&state, &user, &id, true).await?;
    state.ai.stop(&conversation.id);
    sqlx::query("DELETE FROM ai_conversations WHERE id = ?")
        .bind(&conversation.id)
        .execute(&state.db)
        .await?;
    state.events.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": {"id": conversation.id, "userId": user.id}, "deleted": true }),
    );
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMessage {
    text: String,
}

/// What a run is checked against before it starts.
struct Prepared {
    resolved: Resolved,
    api_key: Option<String>,
    /// The signed-in account (ChatGPT's account id), "" none.
    account: String,
    limits: ai::Effective,
}

/// Check that `user` may go on in the conversation now: its agent and
/// model, the allowance, the provider's key.
async fn prepare(
    state: &AppState,
    user: &AuthUser,
    conversation: &ai::Conversation,
) -> AppResult<Prepared> {
    let resolved = resolve(
        state,
        user,
        conversation.agent_id.as_deref(),
        conversation.provider_id.as_deref(),
        &conversation.model,
    )
    .await?;
    let limits = ai::effective_limits(&state.db, user).await?;
    ai::check_allowance(&state.db, user.id, &limits).await?;
    let (api_key, account) = if resolved.provider.kind.signs_in() {
        let (token, account) =
            ai::signed_in_token(&state.db, &state.secrets, &resolved.provider).await?;
        (Some(token), account)
    } else {
        (resolved.provider.api_key(&state.secrets)?, String::new())
    };
    if api_key.is_none() && resolved.provider.kind.needs_key() {
        return Err(AppError::bad_request(format!(
            "{} has no API key yet; an administrator adds it in the AI Agent settings",
            resolved.provider.name
        )));
    }
    Ok(Prepared {
        resolved,
        api_key,
        account,
        limits,
    })
}

/// The run for reply `message_id` (its history: the messages before it).
async fn reply_job(
    state: &AppState,
    user: &AuthUser,
    conversation: &ai::Conversation,
    prepared: Prepared,
    message_id: i64,
    blocks: Vec<Value>,
    tokens: (i64, i64),
) -> AppResult<ReplyJob> {
    let Prepared {
        resolved,
        api_key,
        account,
        limits,
    } = prepared;
    let earlier: Vec<ai::Message> = ai::messages(&state.db, &conversation.id)
        .await?
        .into_iter()
        .filter(|m| m.id < message_id)
        .collect();
    let all_skills = ai::skills(&state.db).await?;
    let skills: Vec<ai::Skill> = resolved
        .agent
        .as_ref()
        .map(|agent| {
            agent
                .skills
                .iter()
                .filter_map(|id| all_skills.iter().find(|s| &s.id == id).cloned())
                .collect()
        })
        .unwrap_or_default();
    let available = tools::available(state, user)
        .await
        .restrict(resolved.agent.as_ref().and_then(|a| a.tools.as_deref()));
    let today = chrono::Local::now().format("%A, %-d %B %Y").to_string();
    let request = provider::ChatRequest {
        kind: resolved.provider.kind,
        base_url: resolved.provider.effective_base_url(),
        api_key,
        model: resolved.model.clone(),
        system: ai::system_prompt(resolved.agent.as_ref(), &skills, user, &today, &available),
        messages: ai::history(&earlier),
        tools: available.specs(),
        temperature: resolved.agent.as_ref().and_then(|a| a.temperature),
        max_output_tokens: resolved.agent.as_ref().and_then(|a| a.max_output_tokens),
        effort: conversation.effort,
        session: conversation.id.clone(),
        account,
    };
    Ok(ReplyJob {
        owner: user.clone(),
        conversation_id: conversation.id.clone(),
        message_id,
        provider_id: resolved.provider.id.clone(),
        model: resolved.model,
        request,
        tools: available,
        allowed_tools: conversation.allowed_tools.clone(),
        blocks,
        limits,
        tokens,
    })
}

async fn last_message(state: &AppState, conversation_id: &str) -> AppResult<Option<ai::Message>> {
    let row =
        sqlx::query("SELECT * FROM ai_messages WHERE conversation_id = ? ORDER BY id DESC LIMIT 1")
            .bind(conversation_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.as_ref().map(ai::message_from_row))
}

fn publish_message(state: &AppState, owner_id: i64, message: &ai::Message, running: bool) {
    state.events.publish_user_and_admin(
        "ai.message",
        owner_id,
        json!({"conversationId": message.conversation_id, "message": message.json(), "running": running, "seq": 0}),
    );
}

/// End a reply that waits for tool answers (stopped, its calls not run).
/// Call with `decide` held. Returns whether one waited.
async fn end_waiting(state: &AppState, conversation: &ai::Conversation) -> AppResult<bool> {
    let Some(mut message) = last_message(state, &conversation.id).await? else {
        return Ok(false);
    };
    if message.status != "waiting" {
        return Ok(false);
    }
    if let Some(blocks) = message.content.as_array_mut() {
        for block in blocks.iter_mut().filter(|b| ai::is_tool_block(b)) {
            if matches!(block["status"].as_str(), Some("pending" | "allowed")) {
                block["status"] = json!("skipped");
            }
        }
    }
    let now = now_ms();
    sqlx::query(
        "UPDATE ai_messages SET content = ?, status = 'stopped', finished_at = ? WHERE id = ?",
    )
    .bind(message.content.to_string())
    .bind(now)
    .bind(message.id)
    .execute(&state.db)
    .await?;
    message.status = "stopped".into();
    message.finished_at = Some(now);
    publish_message(state, conversation.user_id, &message, false);
    Ok(true)
}

/// POST /api/ai/conversations/{id}/messages — the reply is written in the
/// background (`ai.delta`, then `ai.message`). A reply waiting for tool
/// answers ends first (its calls are not run).
async fn send_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<NewMessage>,
) -> AppResult<Json<Value>> {
    let _deciding = state.ai.decide.lock().await;
    let mut conversation = owned(&state, &user, &id, true).await?;
    let text = body.text.trim_end().to_string();
    if text.trim().is_empty() {
        return Err(AppError::bad_request("write a message first"));
    }
    if text.len() > ai::MAX_MESSAGE {
        return Err(AppError::bad_request(format!(
            "messages are limited to {} KiB",
            ai::MAX_MESSAGE / 1024
        )));
    }
    let prepared = prepare(&state, &user, &conversation).await?;
    let reservation = state
        .ai
        .reserve(&conversation.id)
        .ok_or_else(|| AppError::conflict("a reply is still being written; stop it or wait"))?;
    end_waiting(&state, &conversation).await?;
    let (provider_id, model) = (
        prepared.resolved.provider.id.clone(),
        prepared.resolved.model.clone(),
    );

    let now = now_ms();
    let user_content = json!([{ "type": "text", "text": text }]);
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO ai_messages (conversation_id, role, content, status, created_at, finished_at)
         VALUES (?, 'user', ?, 'done', ?, ?) RETURNING id",
    )
    .bind(&conversation.id)
    .bind(user_content.to_string())
    .bind(now)
    .bind(now)
    .fetch_one(&state.db)
    .await?;
    let reply_id: i64 = sqlx::query_scalar(
        "INSERT INTO ai_messages (conversation_id, role, content, provider_id, model, status, created_at)
         VALUES (?, 'assistant', '[]', ?, ?, 'streaming', ?) RETURNING id",
    )
    .bind(&conversation.id)
    .bind(&provider_id)
    .bind(&model)
    .bind(now)
    .fetch_one(&state.db)
    .await?;
    if conversation.title.is_empty() {
        conversation.title = ai::title_from(&text);
    }
    conversation.updated_at = now;
    sqlx::query("UPDATE ai_conversations SET title = ?, updated_at = ? WHERE id = ?")
        .bind(&conversation.title)
        .bind(now)
        .bind(&conversation.id)
        .execute(&state.db)
        .await?;

    conversation.waiting = false;
    let job = reply_job(
        &state,
        &user,
        &conversation,
        prepared,
        reply_id,
        Vec::new(),
        (0, 0),
    )
    .await?;
    reservation.start(state.clone(), job);

    let sent = ai::message(&state.db, user_id).await?.map(|m| m.json());
    let reply = ai::message(&state.db, reply_id).await?.map(|m| m.json());
    let conversation_json = conversation.json(None, true);
    for message in [&sent, &reply].into_iter().flatten() {
        state.events.publish_user_and_admin(
            "ai.message",
            user.id,
            json!({"conversationId": conversation.id, "message": message, "running": true}),
        );
    }
    state.events.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), true) }),
    );
    Ok(Json(json!({
        "conversation": conversation_json,
        "message": sent,
        "reply": reply,
    })))
}

/// POST /api/ai/conversations/{id}/stop — also ends a reply waiting for
/// tool answers.
async fn stop_reply(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let _deciding = state.ai.decide.lock().await;
    let mut conversation = owned(&state, &user, &id, true).await?;
    if state.ai.stop(&conversation.id) {
        return Ok(Json(json!({ "stopped": true })));
    }
    let ended = end_waiting(&state, &conversation).await?;
    if ended {
        conversation.waiting = false;
        state.events.publish_user_and_admin(
            "ai.conversation",
            user.id,
            json!({ "conversation": conversation.json(Some(&user.username), false) }),
        );
    }
    Ok(Json(json!({ "stopped": ended })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolDecision {
    /// `allow` or `deny`.
    decision: String,
    /// With `allow`: let this tool run without asking for the rest of the
    /// chat (not the shell).
    #[serde(default)]
    always: bool,
}

/// POST /api/ai/conversations/{id}/tools/{callId} — the owner's answer to a
/// tool request. Once every call of the reply is answered it goes on.
async fn decide_tool(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, call_id)): Path<(String, String)>,
    Json(body): Json<ToolDecision>,
) -> AppResult<Json<Value>> {
    let allow = match body.decision.as_str() {
        "allow" => true,
        "deny" => false,
        _ => return Err(AppError::bad_request("the decision is allow or deny")),
    };
    let _deciding = state.ai.decide.lock().await;
    let mut conversation = owned(&state, &user, &id, true).await?;
    if state.ai.running(&conversation.id) {
        return Err(AppError::conflict("a reply is still being written"));
    }
    let mut message = last_message(&state, &conversation.id)
        .await?
        .filter(|m| m.role == "assistant" && m.status == "waiting")
        .ok_or_else(|| AppError::conflict("this chat is not waiting for an answer"))?;
    let mut blocks: Vec<Value> = message.content.as_array().cloned().unwrap_or_default();
    let at = blocks
        .iter()
        .position(|b| {
            ai::is_tool_block(b) && b["id"] == call_id.as_str() && b["status"] == "pending"
        })
        .ok_or_else(|| AppError::conflict("that request was already answered"))?;
    let name = blocks[at]["name"].as_str().unwrap_or("").to_string();
    let mut allowed_tools = conversation.allowed_tools.clone();
    if allow {
        blocks[at]["status"] = json!("allowed");
        if body.always && tools::may_skip_asking(&name) {
            for block in blocks.iter_mut().filter(|b| ai::is_tool_block(b)) {
                if block["name"] == name.as_str() && block["status"] == "pending" {
                    block["status"] = json!("allowed");
                }
            }
            if !allowed_tools.contains(&name) {
                allowed_tools.push(name.clone());
            }
        }
    } else {
        blocks[at]["status"] = json!("denied");
    }
    let save_allowed = |conversation: &mut ai::Conversation| {
        let changed = conversation.allowed_tools != allowed_tools;
        conversation.allowed_tools = allowed_tools.clone();
        changed
    };
    let answered = !blocks.iter().any(|b| b["status"] == "pending");
    if !answered {
        let changed = save_allowed(&mut conversation);
        message.content = Value::Array(blocks);
        sqlx::query("UPDATE ai_messages SET content = ? WHERE id = ?")
            .bind(message.content.to_string())
            .bind(message.id)
            .execute(&state.db)
            .await?;
        if changed {
            store_allowed_tools(&state, &conversation).await?;
        }
        publish_message(&state, user.id, &message, false);
        return Ok(Json(
            json!({ "message": message.json(), "conversation": conversation.json(None, false) }),
        ));
    }
    // Every call answered: the reply goes on (checked as a new message).
    let prepared = prepare(&state, &user, &conversation).await?;
    let reservation = state
        .ai
        .reserve(&conversation.id)
        .ok_or_else(|| AppError::conflict("a reply is still being written"))?;
    let changed = save_allowed(&mut conversation);
    if changed {
        store_allowed_tools(&state, &conversation).await?;
    }
    message.content = Value::Array(blocks.clone());
    message.status = "streaming".into();
    sqlx::query("UPDATE ai_messages SET content = ?, status = 'streaming' WHERE id = ?")
        .bind(message.content.to_string())
        .bind(message.id)
        .execute(&state.db)
        .await?;
    conversation.waiting = false;
    let job = reply_job(
        &state,
        &user,
        &conversation,
        prepared,
        message.id,
        blocks,
        (message.input_tokens, message.output_tokens),
    )
    .await?;
    // Before the run publishes anything of its own.
    publish_message(&state, user.id, &message, true);
    reservation.start(state.clone(), job);
    state.events.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), true) }),
    );
    Ok(Json(
        json!({ "message": message.json(), "conversation": conversation.json(None, true) }),
    ))
}

async fn store_allowed_tools(state: &AppState, conversation: &ai::Conversation) -> AppResult<()> {
    sqlx::query("UPDATE ai_conversations SET allowed_tools = ? WHERE id = ?")
        .bind(serde_json::to_string(&conversation.allowed_tools).unwrap_or_else(|_| "[]".into()))
        .bind(&conversation.id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// -------------------------------------------------------- administrators

/// GET /api/ai/admin/overview — the whole setup, for the settings page.
async fn admin_overview(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    let providers: Vec<Value> = ai::providers(&state.db)
        .await?
        .iter()
        .map(|p| p.admin_json(&state.secrets))
        .collect();
    let kinds: Vec<Value> = ai::KINDS
        .iter()
        .map(|k| {
            let plan = ai::signin::Plan::of_kind(k.kind);
            json!({
                "id": k.id,
                "label": k.label,
                "defaultBaseUrl": k.base_url,
                "needsKey": k.needs_key,
                "group": k.group,
                "keyUrl": k.key_url,
                "about": k.about,
                "signsIn": plan.is_some(),
                "signInFlow": plan.map(flow_name).unwrap_or(""),
                "site": plan.map(|p| p.site()).unwrap_or(""),
                "callbackHint": plan.map(|p| p.callback_hint()).unwrap_or(""),
                "risk": plan.map(|p| p.risk_note()).unwrap_or(""),
            })
        })
        .collect();
    let users: Vec<Value> = sqlx::query(
        "SELECT id, username, display_name, role, active FROM users ORDER BY username COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|row| {
        json!({
            "id": row.get::<i64, _>("id"),
            "username": row.get::<String, _>("username"),
            "displayName": row.get::<String, _>("display_name"),
            "isAdmin": row.get::<String, _>("role") == "admin",
            "active": row.get::<i64, _>("active") != 0,
        })
    })
    .collect();
    let groups: Vec<Value> =
        sqlx::query("SELECT id, name FROM groups ORDER BY name COLLATE NOCASE")
            .fetch_all(&state.db)
            .await?
            .iter()
            .map(|row| json!({"id": row.get::<i64, _>("id"), "name": row.get::<String, _>("name")}))
            .collect();
    let usage: Vec<Value> = ai::used_by_account(&state.db, &ai::periods())
        .await?
        .into_iter()
        .map(|(user_id, used, last_at)| {
            json!({
                "userId": user_id,
                "today": used.tokens[0],
                "week": used.tokens[1],
                "month": used.tokens[2],
                "costToday": ai::micros_to_usd(used.cost[0]),
                "costWeek": ai::micros_to_usd(used.cost[1]),
                "costMonth": ai::micros_to_usd(used.cost[2]),
                "lastAt": last_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "kinds": kinds,
        "providers": providers,
        "agents": ai::agents(&state.db).await?,
        "skills": ai::skills(&state.db).await?,
        "limits": ai::limit_rules(&state.db).await?,
        "users": users,
        "groups": groups,
        "usage": usage,
        "tools": tools::ALL.iter().map(|name| json!({"name": name, "label": tools::label(name)})).collect::<Vec<_>>(),
        "search": tools::search_config(&state.db).await.public_json(&state.secrets),
        "serverAccess": tools::server_access(&state.db).await,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageQuery {
    days: Option<u32>,
}

/// GET /api/ai/admin/usage/{userId}?days=30 — one account's use: its
/// allowance now, a total per day and per model.
async fn admin_usage(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(user_id): Path<i64>,
    Query(query): Query<UsageQuery>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let row = sqlx::query("SELECT id, username, display_name, role FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("no such account"))?;
    let is_admin = row.get::<String, _>("role") == "admin";
    let limits = ai::effective_limits_for(&state.db, user_id, is_admin).await?;
    let days = query.days.unwrap_or(30).clamp(1, 366);
    let mut history = ai::usage_history(&state.db, user_id, days).await?;
    history["user"] = json!({
        "id": user_id,
        "username": row.get::<String, _>("username"),
        "displayName": row.get::<String, _>("display_name"),
        "isAdmin": is_admin,
    });
    history["allowance"] = ai::allowance(&state.db, user_id, &limits).await?;
    history["providers"] = json!(limits.providers);
    Ok(Json(history))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerAccess {
    enabled: bool,
}

/// PUT /api/ai/server-access — whether accounts that are not
/// administrators may read the server and run read-only commands there.
async fn put_server_access(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<ServerAccess>,
) -> AppResult<Json<Value>> {
    tools::set_server_access(&state.db, body.enabled).await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.server_access.update",
        "ai_settings",
        "serverAccess",
        if body.enabled { "on" } else { "off" },
    )
    .await;
    config_changed(&state, "serverAccess");
    Ok(Json(json!({ "serverAccess": body.enabled })))
}

const SEARCH_KINDS: [&str; 4] = ["", "brave", "tavily", "searxng"];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchBody {
    /// `brave`, `tavily`, `searxng`, or `""` for none.
    kind: String,
    #[serde(default)]
    url: String,
    /// New key (absent or empty: keep the current one).
    api_key: Option<String>,
    #[serde(default)]
    remove_key: bool,
}

/// GET /api/ai/search — the web search service agents use.
async fn get_search(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    Ok(Json(
        json!({ "search": tools::search_config(&state.db).await.public_json(&state.secrets) }),
    ))
}

/// PUT /api/ai/search
async fn put_search(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<SearchBody>,
) -> AppResult<Json<Value>> {
    if !SEARCH_KINDS.contains(&body.kind.as_str()) {
        return Err(AppError::bad_request("pick Brave, Tavily or SearXNG"));
    }
    let current = tools::search_config(&state.db).await;
    let mut config = tools::SearchConfig {
        kind: body.kind.clone(),
        url: String::new(),
        key: if body.kind == current.kind {
            current.key
        } else {
            String::new()
        },
    };
    match body.kind.as_str() {
        "" => config.key.clear(),
        "searxng" => {
            let url = body.url.trim().trim_end_matches('/').to_string();
            if !ai::valid_base_url(&url) {
                return Err(AppError::bad_request(
                    "enter the SearXNG address, like http://127.0.0.1:8888",
                ));
            }
            config.url = url;
            config.key.clear();
        }
        _ => {
            if let Some(key) = body
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
            {
                if key.len() > 500 {
                    return Err(AppError::bad_request("that is not an API key"));
                }
                config.key = ai::seal_key(&state.secrets, key)?;
            } else if body.remove_key {
                config.key.clear();
            }
            if config.key.is_empty() {
                return Err(AppError::bad_request("enter the service's API key"));
            }
        }
    }
    tools::save_search_config(&state.db, &config).await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.search.update",
        "ai_search",
        "search",
        if config.kind.is_empty() {
            "none"
        } else {
            &config.kind
        },
    )
    .await;
    config_changed(&state, "search");
    Ok(Json(
        json!({ "search": config.public_json(&state.secrets) }),
    ))
}

/// POST /api/ai/search/test — one search with the saved service.
async fn test_search(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    let config = tools::search_config(&state.db).await;
    if !config.is_configured() {
        return Err(AppError::bad_request("connect a search service first"));
    }
    let key = config.key(&state.secrets);
    let hits = tokio::task::spawn_blocking(move || {
        tools::web_search(&config, key.as_deref(), "RAST self-hosted server")
    })
    .await
    .map_err(|e| AppError::Unavailable(e.to_string()))?
    .map_err(AppError::bad_request)?;
    Ok(Json(json!({
        "results": hits.len(),
        "first": hits.first().map(|h| json!({"title": h.title, "url": h.url})),
    })))
}

/// GET /api/ai/admin/conversations — every account's, newest first.
async fn admin_conversations(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let pattern = format!("%{}%", query.q.as_deref().unwrap_or("").trim());
    let rows = sqlx::query(
        "SELECT c.*, u.username AS owner,
                (SELECT COUNT(*) FROM ai_messages m WHERE m.conversation_id = c.id) AS message_count,
                (SELECT status FROM ai_messages m WHERE m.conversation_id = c.id
                 ORDER BY m.id DESC LIMIT 1) AS last_status
         FROM ai_conversations c JOIN users u ON u.id = c.user_id
         WHERE (? IS NULL OR c.user_id = ?) AND (c.title LIKE ? OR u.username LIKE ?)
         ORDER BY c.updated_at DESC LIMIT ?",
    )
    .bind(query.user_id)
    .bind(query.user_id)
    .bind(&pattern)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let conversation = ai::conversation_from_row(row);
            let mut value = conversation.json(
                Some(&row.get::<String, _>("owner")),
                state.ai.running(&conversation.id),
            );
            value["messageCount"] = json!(row.get::<i64, _>("message_count"));
            value
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderBody {
    kind: Option<String>,
    name: String,
    #[serde(default)]
    base_url: String,
    /// New key (absent or empty: keep the current one).
    api_key: Option<String>,
    #[serde(default)]
    remove_key: bool,
    #[serde(default)]
    models: Vec<String>,
    /// Prices an administrator set, per model (absent: keep the current ones).
    prices: Option<provider::Prices>,
    /// What the provider's model list gave when its models were loaded
    /// (absent: keep the current ones).
    list_prices: Option<provider::Prices>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

fn price_error() -> AppError {
    AppError::bad_request(format!(
        "prices are in USD per million tokens, from 0 to {}",
        ai::MAX_PRICE
    ))
}

/// Prices for the provider's models only, each within reason.
fn checked_prices(prices: &provider::Prices, models: &[String]) -> AppResult<provider::Prices> {
    let mut out = provider::Prices::new();
    for (model, price) in prices {
        if !models.contains(model) {
            continue;
        }
        if !price.valid() {
            return Err(price_error());
        }
        out.insert(model.clone(), price.clone());
    }
    Ok(out)
}

/// A model list's prices (every model it lists, not only those offered),
/// each within reason.
fn checked_list_prices(prices: &provider::Prices) -> AppResult<provider::Prices> {
    if prices.len() > 4 * ai::MAX_MODELS {
        return Err(AppError::bad_request("too many prices"));
    }
    if prices.iter().any(|(model, price)| {
        model.is_empty() || model.len() > ai::MAX_MODEL_NAME || !price.valid()
    }) {
        return Err(price_error());
    }
    Ok(prices.clone())
}

fn checked_base_url(raw: &str) -> AppResult<String> {
    let url = raw.trim().trim_end_matches('/').to_string();
    if !url.is_empty() && !ai::valid_base_url(&url) {
        return Err(AppError::bad_request(
            "the base URL must start with https:// or http://",
        ));
    }
    Ok(url)
}

fn checked_key(raw: Option<&str>) -> AppResult<Option<String>> {
    match raw.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) if key.len() > 4096 || key.chars().any(|c| c.is_control() || c == ' ') => {
            Err(AppError::bad_request("that does not look like an API key"))
        }
        other => Ok(other.map(str::to_string)),
    }
}

/// POST /api/ai/providers
async fn create_provider(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<ProviderBody>,
) -> AppResult<Json<Value>> {
    let kind = body
        .kind
        .as_deref()
        .and_then(Kind::parse)
        .ok_or_else(|| AppError::bad_request("pick the kind of provider"))?;
    if kind.signs_in() {
        return Err(AppError::bad_request(format!(
            "{} is added by signing in, not with a key",
            kind.label()
        )));
    }
    let name = required(&body.name, "the name", ai::MAX_NAME)?;
    let base_url = checked_base_url(&body.base_url)?;
    if base_url.is_empty() && kind.default_base_url().is_empty() {
        return Err(AppError::bad_request(
            "a custom provider needs its base URL",
        ));
    }
    let key = checked_key(body.api_key.as_deref())?;
    let models = models_list(&body.models)?;
    let prices = checked_prices(&body.prices.clone().unwrap_or_default(), &models)?;
    let list_prices = body
        .list_prices
        .as_ref()
        .map(checked_list_prices)
        .transpose()?;
    let id = ai::new_id();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO ai_providers (id, kind, name, base_url, api_key, models, prices, list_prices,
             prices_checked_at, enabled, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(kind.as_str())
    .bind(&name)
    .bind(&base_url)
    .bind(ai::seal_key(&state.secrets, key.as_deref().unwrap_or(""))?)
    .bind(serde_json::to_string(&models).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(&prices).unwrap_or_else(|_| "{}".into()))
    .bind(
        serde_json::to_string(&list_prices.clone().unwrap_or_default())
            .unwrap_or_else(|_| "{}".into()),
    )
    .bind(list_prices.as_ref().map(|_| now))
    .bind(body.enabled as i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.create",
        "ai_provider",
        &id,
        &name,
    )
    .await;
    config_changed(&state, "providers");
    let provider = ai::provider(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::internal("the provider was not saved"))?;
    Ok(Json(
        json!({ "provider": provider.admin_json(&state.secrets) }),
    ))
}

/// PUT /api/ai/providers/{id}
async fn update_provider(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<ProviderBody>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    if body
        .kind
        .as_deref()
        .is_some_and(|k| k != existing.kind.as_str())
    {
        return Err(AppError::bad_request(
            "a provider's kind cannot change; add a new one",
        ));
    }
    let name = required(&body.name, "the name", ai::MAX_NAME)?;
    let base_url = checked_base_url(&body.base_url)?;
    if base_url.is_empty() && existing.kind.default_base_url().is_empty() {
        return Err(AppError::bad_request(
            "a custom provider needs its base URL",
        ));
    }
    if existing.kind.signs_in()
        && (!base_url.is_empty() || body.api_key.as_deref().is_some_and(|k| !k.is_empty()))
    {
        return Err(AppError::bad_request(format!(
            "{} keeps its own address and signs in instead of using a key",
            existing.kind.label()
        )));
    }
    let models = models_list(&body.models)?;
    let prices = checked_prices(body.prices.as_ref().unwrap_or(&existing.prices), &models)?;
    let list_prices = body
        .list_prices
        .as_ref()
        .map(checked_list_prices)
        .transpose()?;
    let key = checked_key(body.api_key.as_deref())?;
    let sealed = match (&key, body.remove_key) {
        (Some(key), _) => ai::seal_key(&state.secrets, key)?,
        (None, true) => String::new(),
        (None, false) => existing.sealed_key().to_string(),
    };
    let now = now_ms();
    sqlx::query(
        "UPDATE ai_providers SET name = ?, base_url = ?, models = ?, prices = ?, enabled = ?, api_key = ?,
             list_prices = COALESCE(?, list_prices),
             prices_checked_at = CASE WHEN ? IS NULL THEN prices_checked_at ELSE ? END,
             updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&base_url)
    .bind(serde_json::to_string(&models).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(&prices).unwrap_or_else(|_| "{}".into()))
    .bind(body.enabled as i64)
    .bind(&sealed)
    .bind(list_prices.as_ref().map(|p| serde_json::to_string(p).unwrap_or_else(|_| "{}".into())))
    .bind(list_prices.as_ref().map(|_| now))
    .bind(now)
    .bind(now)
    .bind(&id)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.update",
        "ai_provider",
        &id,
        &name,
    )
    .await;
    config_changed(&state, "providers");
    let provider = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    Ok(Json(
        json!({ "provider": provider.admin_json(&state.secrets) }),
    ))
}

/// DELETE /api/ai/providers/{id} (agents and conversations using it keep
/// their messages and need another model).
async fn delete_provider(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    sqlx::query("DELETE FROM ai_providers WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.delete",
        "ai_provider",
        &id,
        &existing.name,
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(json!({ "deleted": true })))
}

/// GET /api/ai/prices — every provider's models with their prices and
/// where each comes from, and the built-in list.
async fn get_prices(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    Ok(Json(ai::prices::listing(&ai::providers(&state.db).await?)))
}

/// POST /api/ai/prices/refresh — ask every provider in use (not local)
/// for its list prices now.
async fn refresh_prices(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
) -> AppResult<Json<Value>> {
    let mut results = Vec::new();
    for p in ai::providers(&state.db)
        .await?
        .iter()
        .filter(|p| ai::prices_refreshable(p))
    {
        results.push(
            match ai::refresh_list_prices(&state.db, &state.secrets, p).await {
                Ok(priced) => json!({ "id": p.id, "name": p.name, "priced": priced }),
                Err(error) => json!({ "id": p.id, "name": p.name, "error": error }),
            },
        );
    }
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.prices.refresh",
        "ai_prices",
        "",
        "",
    )
    .await;
    config_changed(&state, "providers");
    let mut listing = ai::prices::listing(&ai::providers(&state.db).await?);
    listing["results"] = json!(results);
    Ok(Json(listing))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelPriceBody {
    model: String,
    /// null: use the provider's list or the built-in price again.
    price: Option<provider::Price>,
}

/// PUT /api/ai/providers/{id}/prices — set one model's price on a provider.
async fn put_model_price(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<ModelPriceBody>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    if !existing.models.contains(&body.model) {
        return Err(AppError::bad_request(
            "the provider does not offer that model",
        ));
    }
    let mut prices = existing.prices.clone();
    match body.price {
        Some(price) if !price.valid() => return Err(price_error()),
        Some(price) => {
            prices.insert(body.model.clone(), price);
        }
        None => {
            prices.remove(&body.model);
        }
    }
    sqlx::query("UPDATE ai_providers SET prices = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&prices).unwrap_or_else(|_| "{}".into()))
        .bind(now_ms())
        .bind(&id)
        .execute(&state.db)
        .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.price",
        "ai_provider",
        &id,
        &format!("{}: {}", existing.name, body.model),
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(ai::prices::listing(&ai::providers(&state.db).await?)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProbeBody {
    provider_id: Option<String>,
    kind: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

/// POST /api/ai/providers/models — test a provider (saved or being added)
/// and list its models.
async fn probe_models(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<ProbeBody>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let saved = match body.provider_id.as_deref() {
        Some(id) => Some(
            ai::provider(&state.db, id)
                .await?
                .ok_or(AppError::NotFound("provider not found"))?,
        ),
        None => None,
    };
    let kind = match (&saved, body.kind.as_deref()) {
        (Some(p), _) => p.kind,
        (None, Some(kind)) => {
            Kind::parse(kind).ok_or_else(|| AppError::bad_request("unknown provider kind"))?
        }
        (None, None) => return Err(AppError::bad_request("pick the kind of provider")),
    };
    if kind.signs_in() {
        let saved = saved.ok_or_else(|| AppError::bad_request("sign in first"))?;
        let (token, account) = ai::signed_in_token(&state.db, &state.secrets, &saved).await?;
        let base_url = saved.effective_base_url();
        let list = tokio::task::spawn_blocking(move || {
            provider::list_plan_models(kind, &base_url, &token, &account)
        })
        .await
        .map_err(|e| AppError::internal(format!("model list failed: {e}")))?
        .map_err(AppError::bad_request)?;
        // "built-in": the plan's own catalog could not be used; `note` says why.
        return Ok(Json(json!({
            "models": list.models,
            "prices": {},
            "source": if list.fallback_reason.is_some() { "built-in" } else { "live" },
            "note": list.fallback_reason,
        })));
    }
    let base_url = match body.base_url.as_deref().map(checked_base_url).transpose()? {
        Some(url) if !url.is_empty() => url,
        _ => match &saved {
            Some(p) => p.effective_base_url(),
            None => kind.default_base_url().to_string(),
        },
    };
    let key = match checked_key(body.api_key.as_deref())? {
        Some(key) => Some(key),
        None => match &saved {
            Some(p) => p.api_key(&state.secrets)?,
            None => None,
        },
    };
    let list =
        tokio::task::spawn_blocking(move || provider::list_models(kind, &base_url, key.as_deref()))
            .await
            .map_err(|e| AppError::internal(format!("model list failed: {e}")))?
            .map_err(AppError::bad_request)?;
    Ok(Json(
        json!({ "models": list.models, "prices": list.prices }),
    ))
}

// ------------------------------------------------------------- plan sign-in

/// What the settings page shows about a plan while signing in.
fn sign_in_meta(plan: ai::signin::Plan) -> Value {
    json!({
        "label": plan.label(),
        "site": plan.site(),
        "callbackHint": plan.callback_hint(),
        "risk": plan.risk_note(),
    })
}

fn flow_name(plan: ai::signin::Plan) -> &'static str {
    match plan.flow() {
        ai::signin::Flow::Browser => "browser",
        ai::signin::Flow::Device => "device",
    }
}

/// Store a finished sign-in: tokens sealed, the provider owned by the account
/// that signed in, with the models the plan offers (when they can be read).
/// Shared by every plan flow (browser paste, device poll, Claude Code import).
async fn store_signed_in(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    plan: ai::signin::Plan,
    provider_id: Option<&str>,
    name: &str,
    credentials: ai::signin::Credentials,
) -> AppResult<(ai::Provider, Option<Vec<String>>)> {
    let kind = plan.kind();
    // The models the plan offers (the sign-in stands even when this fails).
    let offered = {
        let (base_url, token, account) = (
            kind.default_base_url().to_string(),
            credentials.access_token.clone(),
            credentials.account_id.clone(),
        );
        tokio::task::spawn_blocking(move || {
            provider::list_plan_models(kind, &base_url, &token, &account)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .map(|list| list.models)
        .filter(|models| !models.is_empty())
    };
    let sealed = ai::seal_credentials(&state.secrets, &credentials)?;
    let now = now_ms();
    let id = match provider_id {
        Some(id) => {
            let existing = ai::provider(&state.db, id)
                .await?
                .ok_or(AppError::NotFound("provider not found"))?;
            if existing.kind != kind {
                return Err(AppError::bad_request("that provider is of another kind"));
            }
            // Signing in again re-reads the plan's catalog: the models the
            // account can use may have changed, and an older sign-in could
            // have stored only the built-in fallback list.
            let models = offered.clone().unwrap_or_else(|| existing.models.clone());
            sqlx::query(
                "UPDATE ai_providers SET sign_in = ?, owner_id = ?, models = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&sealed)
            .bind(user.id)
            .bind(serde_json::to_string(&models).unwrap_or_else(|_| "[]".into()))
            .bind(now)
            .bind(id)
            .execute(&state.db)
            .await?;
            id.to_string()
        }
        None => {
            let id = ai::new_id();
            sqlx::query(
                "INSERT INTO ai_providers (id, kind, name, base_url, api_key, models, prices, list_prices,
                     enabled, created_at, updated_at, owner_id, sign_in)
                 VALUES (?, ?, ?, '', '', ?, '{}', '{}', 1, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(kind.as_str())
            .bind(name)
            .bind(
                serde_json::to_string(&offered.clone().unwrap_or_default())
                    .unwrap_or_else(|_| "[]".into()),
            )
            .bind(now)
            .bind(now)
            .bind(user.id)
            .bind(&sealed)
            .execute(&state.db)
            .await?;
            id
        }
    };
    audit_admin(
        state,
        user,
        ip,
        "ai.provider.sign_in",
        "ai_provider",
        &id,
        name,
    )
    .await;
    config_changed(state, "providers");
    let provider = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    Ok((provider, offered))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignInStart {
    kind: String,
    /// Sign in again to this provider (else a new one is added).
    provider_id: Option<String>,
    #[serde(default)]
    name: String,
}

/// POST /api/ai/providers/sign-in — start signing in to a plan: a page to
/// open (browser flows) or a code to enter (device flows); see
/// [`ai::signin`].
async fn start_sign_in(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<SignInStart>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&body.kind)
        .filter(|k| k.signs_in())
        .ok_or_else(|| AppError::bad_request("that provider does not sign in"))?;
    let plan = kind
        .plan()
        .ok_or_else(|| AppError::bad_request("that provider does not sign in"))?;
    if let Some(id) = &body.provider_id {
        let existing = ai::provider(&state.db, id)
            .await?
            .ok_or(AppError::NotFound("provider not found"))?;
        if existing.kind != kind {
            return Err(AppError::bad_request("that provider is of another kind"));
        }
    }
    let name = if body.name.trim().is_empty() {
        kind.label().to_string()
    } else {
        required(&body.name, "the name", ai::MAX_NAME)?
    };
    let (user_id, provider_id) = (admin.0.id, body.provider_id.clone());
    let started = tokio::task::spawn_blocking(move || {
        ai::signin::start(plan, user_id, provider_id, name, now_ms())
    })
    .await
    .map_err(|e| AppError::internal(format!("sign-in failed: {e}")))?
    .map_err(AppError::bad_request)?;
    let mut out = match started {
        ai::signin::Started::Browser {
            url,
            state,
            redirect_uri,
        } => json!({
            "flow": "browser",
            "url": url,
            "state": state,
            "redirectUri": redirect_uri,
        }),
        ai::signin::Started::Device {
            state,
            user_code,
            verification_uri,
            interval,
            expires_in,
        } => json!({
            "flow": "device",
            "state": state,
            "userCode": user_code,
            "verificationUri": verification_uri,
            "interval": interval,
            "expiresIn": expires_in,
        }),
    };
    if let (Value::Object(fields), Value::Object(meta)) = (&mut out, sign_in_meta(plan)) {
        fields.extend(meta);
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignInFinish {
    /// The address the browser ended on after allowing it.
    callback: String,
    /// A new provider's name (absent: the one given when it started).
    #[serde(default)]
    name: Option<String>,
}

/// POST /api/ai/providers/sign-in/finish — the pasted address: trade its
/// code for tokens, then add the provider (or sign it in again) for the
/// account that signed in, with the models its plan offers.
async fn finish_sign_in(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<SignInFinish>,
) -> AppResult<Json<Value>> {
    if body.callback.len() > 8192 {
        return Err(AppError::bad_request("that address is too long"));
    }
    let callback = ai::signin::parse_callback(&body.callback).map_err(AppError::bad_request)?;
    let mut pending =
        ai::signin::take_pending(&callback.state, admin.0.id).map_err(AppError::bad_request)?;
    if let Some(name) = body.name.as_ref().filter(|n| !n.trim().is_empty()) {
        pending.name = required(name, "the name", ai::MAX_NAME)?;
    }
    let now = now_ms();
    let code = callback.code.clone();
    let exchange = pending.clone();
    let credentials =
        tokio::task::spawn_blocking(move || ai::signin::finish(&exchange, &code, now))
            .await
            .map_err(|e| AppError::internal(format!("sign-in failed: {e}")))?
            .map_err(AppError::bad_request)?;
    let (provider, offered) = store_signed_in(
        &state,
        &admin.0,
        &ip,
        pending.plan,
        pending.provider_id.as_deref(),
        &pending.name,
        credentials,
    )
    .await?;
    Ok(Json(json!({
        "provider": provider.admin_json(&state.secrets),
        "offered": offered,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignInPoll {
    state: String,
}

/// POST /api/ai/providers/sign-in/poll — one device flow's state: waiting,
/// or the provider it added. The page polls this every few seconds.
async fn poll_sign_in(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<SignInPoll>,
) -> AppResult<Json<Value>> {
    let state_id = body.state;
    let user_id = admin.0.id;
    let polled =
        tokio::task::spawn_blocking(move || ai::signin::poll(&state_id, user_id, now_ms()))
            .await
            .map_err(|e| AppError::internal(format!("sign-in failed: {e}")))?
            .map_err(AppError::bad_request)?;
    match polled {
        ai::signin::Poll::Waiting => Ok(Json(json!({"status": "waiting"}))),
        ai::signin::Poll::Failed(message) => Err(AppError::bad_request(message)),
        ai::signin::Poll::Done(signed_in) => {
            let (provider, offered) = store_signed_in(
                &state,
                &admin.0,
                &ip,
                signed_in.plan,
                signed_in.provider_id.as_deref(),
                &signed_in.name,
                signed_in.credentials,
            )
            .await?;
            Ok(Json(json!({
                "status": "done",
                "provider": provider.admin_json(&state.secrets),
                "offered": offered,
            })))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaudeImport {
    /// Sign in again to this provider (else a new one is added).
    provider_id: Option<String>,
    #[serde(default)]
    name: String,
}

/// POST /api/ai/providers/import/claude-code — use the Claude Code login
/// already on the server instead of a browser round trip.
async fn import_claude_code(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<ClaudeImport>,
) -> AppResult<Json<Value>> {
    let kind = Kind::Claude;
    if let Some(id) = &body.provider_id {
        let existing = ai::provider(&state.db, id)
            .await?
            .ok_or(AppError::NotFound("provider not found"))?;
        if existing.kind != kind {
            return Err(AppError::bad_request("that provider is of another kind"));
        }
    }
    let name = if body.name.trim().is_empty() {
        kind.label().to_string()
    } else {
        required(&body.name, "the name", ai::MAX_NAME)?
    };
    let now = now_ms();
    let credentials = tokio::task::spawn_blocking(move || ai::signin::import_claude_code(now))
        .await
        .map_err(|e| AppError::internal(format!("import failed: {e}")))?
        .map_err(AppError::bad_request)?;
    let (provider, offered) = store_signed_in(
        &state,
        &admin.0,
        &ip,
        ai::signin::Plan::Claude,
        body.provider_id.as_deref(),
        &name,
        credentials,
    )
    .await?;
    Ok(Json(json!({
        "provider": provider.admin_json(&state.secrets),
        "offered": offered,
    })))
}

/// POST /api/ai/providers/{id}/sign-out — forget a plan's tokens: the
/// provider stays (and its models), but nobody can chat with it until
/// someone signs in again.
async fn sign_out_provider(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    if !existing.kind.signs_in() {
        return Err(AppError::bad_request("that provider does not sign in"));
    }
    sqlx::query(
        "UPDATE ai_providers SET sign_in = '', owner_id = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.sign_out",
        "ai_provider",
        &id,
        &existing.name,
    )
    .await;
    config_changed(&state, "providers");
    let provider = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    Ok(Json(
        json!({"provider": provider.admin_json(&state.secrets)}),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    system_prompt: String,
    provider_id: Option<String>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    skills: Vec<String>,
    /// Tools it may use (absent or `null`: all).
    #[serde(default)]
    tools: Option<Vec<String>>,
    temperature: Option<f64>,
    max_output_tokens: Option<i64>,
    #[serde(default = "yes")]
    enabled: bool,
}

struct CheckedAgent {
    name: String,
    description: String,
    system_prompt: String,
    provider_id: Option<String>,
    model: String,
    skills: Vec<String>,
    tools: Option<String>,
}

async fn check_agent(state: &AppState, body: &AgentBody) -> AppResult<CheckedAgent> {
    let name = required(&body.name, "the name", ai::MAX_NAME)?;
    let description = bounded(
        body.description.trim(),
        "the description",
        ai::MAX_DESCRIPTION,
    )?;
    if body.system_prompt.len() > ai::MAX_PROMPT {
        return Err(AppError::bad_request(format!(
            "the personality is limited to {} KiB",
            ai::MAX_PROMPT / 1024
        )));
    }
    let provider_id = body.provider_id.clone().filter(|id| !id.is_empty());
    if let Some(id) = &provider_id {
        if ai::provider(&state.db, id).await?.is_none() {
            return Err(AppError::bad_request("that provider no longer exists"));
        }
    }
    let model = body.model.trim().to_string();
    if model.len() > ai::MAX_MODEL_NAME {
        return Err(AppError::bad_request("that is not a model name"));
    }
    if body.skills.len() > ai::MAX_AGENT_SKILLS {
        return Err(AppError::bad_request(format!(
            "an agent carries at most {} skills",
            ai::MAX_AGENT_SKILLS
        )));
    }
    let known = ai::skills(&state.db).await?;
    let mut skills: Vec<String> = Vec::new();
    for id in &body.skills {
        if !known.iter().any(|s| &s.id == id) {
            return Err(AppError::bad_request(
                "one of those skills no longer exists",
            ));
        }
        if !skills.contains(id) {
            skills.push(id.clone());
        }
    }
    if let Some(t) = body.temperature {
        if !(0.0..=2.0).contains(&t) {
            return Err(AppError::bad_request("temperature is between 0 and 2"));
        }
    }
    if let Some(max) = body.max_output_tokens {
        if !(1..=1_000_000).contains(&max) {
            return Err(AppError::bad_request(
                "the answer length is between 1 and 1,000,000 tokens",
            ));
        }
    }
    let tools = match &body.tools {
        None => None,
        Some(list) => {
            let mut picked: Vec<&str> = Vec::new();
            for name in list {
                if !tools::is_tool(name) {
                    return Err(AppError::bad_request(format!("there is no tool {name:?}")));
                }
                if !picked.contains(&name.as_str()) {
                    picked.push(name);
                }
            }
            Some(serde_json::to_string(&picked).unwrap_or_else(|_| "[]".into()))
        }
    };
    Ok(CheckedAgent {
        name,
        description,
        system_prompt: body.system_prompt.clone(),
        provider_id,
        model,
        skills,
        tools,
    })
}

/// POST /api/ai/agents
async fn create_agent(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<AgentBody>,
) -> AppResult<Json<Value>> {
    let agent = check_agent(&state, &body).await?;
    let id = ai::new_id();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO ai_agents (id, name, description, system_prompt, provider_id, model, skills, tools,
                                temperature, max_output_tokens, enabled, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&agent.name)
    .bind(&agent.description)
    .bind(&agent.system_prompt)
    .bind(&agent.provider_id)
    .bind(&agent.model)
    .bind(serde_json::to_string(&agent.skills).unwrap_or_else(|_| "[]".into()))
    .bind(&agent.tools)
    .bind(body.temperature)
    .bind(body.max_output_tokens)
    .bind(body.enabled as i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.agent.create",
        "ai_agent",
        &id,
        &agent.name,
    )
    .await;
    config_changed(&state, "agents");
    Ok(Json(json!({ "agent": ai::agent(&state.db, &id).await? })))
}

/// PUT /api/ai/agents/{id}
async fn update_agent(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<AgentBody>,
) -> AppResult<Json<Value>> {
    if ai::agent(&state.db, &id).await?.is_none() {
        return Err(AppError::NotFound("agent not found"));
    }
    let agent = check_agent(&state, &body).await?;
    sqlx::query(
        "UPDATE ai_agents SET name = ?, description = ?, system_prompt = ?, provider_id = ?, model = ?,
                skills = ?, tools = ?, temperature = ?, max_output_tokens = ?, enabled = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&agent.name)
    .bind(&agent.description)
    .bind(&agent.system_prompt)
    .bind(&agent.provider_id)
    .bind(&agent.model)
    .bind(serde_json::to_string(&agent.skills).unwrap_or_else(|_| "[]".into()))
    .bind(&agent.tools)
    .bind(body.temperature)
    .bind(body.max_output_tokens)
    .bind(body.enabled as i64)
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.agent.update",
        "ai_agent",
        &id,
        &agent.name,
    )
    .await;
    config_changed(&state, "agents");
    Ok(Json(json!({ "agent": ai::agent(&state.db, &id).await? })))
}

/// DELETE /api/ai/agents/{id}
async fn delete_agent(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let existing = ai::agent(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("agent not found"))?;
    sqlx::query("DELETE FROM ai_agents WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.agent.delete",
        "ai_agent",
        &id,
        &existing.name,
    )
    .await;
    config_changed(&state, "agents");
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    instructions: String,
}

fn check_skill(body: &SkillBody) -> AppResult<(String, String, String)> {
    let name = required(&body.name, "the name", ai::MAX_NAME)?;
    let description = bounded(
        body.description.trim(),
        "the description",
        ai::MAX_DESCRIPTION,
    )?;
    if body.instructions.trim().is_empty() {
        return Err(AppError::bad_request("write the skill's instructions"));
    }
    if body.instructions.len() > ai::MAX_INSTRUCTIONS {
        return Err(AppError::bad_request(format!(
            "a skill's instructions are limited to {} KiB",
            ai::MAX_INSTRUCTIONS / 1024
        )));
    }
    Ok((name, description, body.instructions.clone()))
}

/// POST /api/ai/skills
async fn create_skill(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<SkillBody>,
) -> AppResult<Json<Value>> {
    let (name, description, instructions) = check_skill(&body)?;
    let id = ai::new_id();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO ai_skills (id, name, description, instructions, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&description)
    .bind(&instructions)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.skill.create",
        "ai_skill",
        &id,
        &name,
    )
    .await;
    config_changed(&state, "skills");
    Ok(Json(json!({ "id": id })))
}

/// PUT /api/ai/skills/{id}
async fn update_skill(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<SkillBody>,
) -> AppResult<Json<Value>> {
    let (name, description, instructions) = check_skill(&body)?;
    let changed = sqlx::query(
        "UPDATE ai_skills SET name = ?, description = ?, instructions = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&description)
    .bind(&instructions)
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(AppError::NotFound("skill not found"));
    }
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.skill.update",
        "ai_skill",
        &id,
        &name,
    )
    .await;
    config_changed(&state, "skills");
    Ok(Json(json!({ "id": id })))
}

/// DELETE /api/ai/skills/{id} (agents stop carrying it).
async fn delete_skill(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM ai_skills WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;
    let name = name.ok_or(AppError::NotFound("skill not found"))?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM ai_skills WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    for row in sqlx::query("SELECT id, skills FROM ai_agents")
        .fetch_all(&mut *tx)
        .await?
    {
        let skills: Vec<String> =
            serde_json::from_str(&row.get::<String, _>("skills")).unwrap_or_default();
        if skills.contains(&id) {
            let kept: Vec<&String> = skills.iter().filter(|s| **s != id).collect();
            sqlx::query("UPDATE ai_agents SET skills = ? WHERE id = ?")
                .bind(serde_json::to_string(&kept).unwrap_or_else(|_| "[]".into()))
                .bind(row.get::<String, _>("id"))
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.skill.delete",
        "ai_skill",
        &id,
        &name,
    )
    .await;
    config_changed(&state, "skills");
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LimitBody {
    providers: Option<Vec<String>>,
    daily_tokens: Option<i64>,
    weekly_tokens: Option<i64>,
    monthly_tokens: Option<i64>,
    /// Spending limits in USD.
    daily_cost: Option<f64>,
    weekly_cost: Option<f64>,
    monthly_cost: Option<f64>,
}

async fn subject_name(state: &AppState, subject: &str) -> AppResult<String> {
    if !ai::valid_subject(subject) {
        return Err(AppError::bad_request(
            "limits are for 'default', 'user:<id>' or 'group:<id>'",
        ));
    }
    if subject == "default" {
        return Ok("everyone".into());
    }
    let (is_user, id) = match subject.split_once(':') {
        Some(("user", id)) => (true, id),
        Some((_, id)) => (false, id),
        None => return Err(AppError::bad_request("bad subject")),
    };
    let id: i64 = id.parse().map_err(|_| AppError::bad_request("bad id"))?;
    let name: Option<String> = if is_user {
        sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
    } else {
        sqlx::query_scalar("SELECT name FROM groups WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
    };
    name.ok_or(AppError::NotFound("no such account or group"))
}

/// PUT /api/ai/limits/{subject}
async fn put_limit(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(subject): Path<String>,
    Json(body): Json<LimitBody>,
) -> AppResult<Json<Value>> {
    let name = subject_name(&state, &subject).await?;
    if let Some(list) = &body.providers {
        let known = ai::providers(&state.db).await?;
        if list.iter().any(|id| !known.iter().any(|p| &p.id == id)) {
            return Err(AppError::bad_request(
                "one of those providers no longer exists",
            ));
        }
    }
    for value in [body.daily_tokens, body.weekly_tokens, body.monthly_tokens]
        .into_iter()
        .flatten()
    {
        if value < 0 {
            return Err(AppError::bad_request("allowances cannot be negative"));
        }
    }
    for value in [body.daily_cost, body.weekly_cost, body.monthly_cost]
        .into_iter()
        .flatten()
    {
        if !value.is_finite() || !(0.0..=1_000_000.0).contains(&value) {
            return Err(AppError::bad_request(
                "spending limits are in USD, from 0 to 1 000 000",
            ));
        }
    }
    // Whole micro-dollars, as they are stored.
    let usd = |v: Option<f64>| v.map(|v| ai::micros_to_usd(ai::usd_to_micros(v)));
    let rule = LimitRule {
        subject: subject.clone(),
        providers: body.providers.clone(),
        caps: ai::Caps {
            daily_tokens: body.daily_tokens,
            weekly_tokens: body.weekly_tokens,
            monthly_tokens: body.monthly_tokens,
            daily_cost: usd(body.daily_cost),
            weekly_cost: usd(body.weekly_cost),
            monthly_cost: usd(body.monthly_cost),
        },
    };
    ai::save_limit_rule(&state.db, &rule).await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.limit.update",
        "ai_limit",
        &subject,
        &name,
    )
    .await;
    config_changed(&state, "limits");
    Ok(Json(json!({ "limit": rule })))
}

/// DELETE /api/ai/limits/{subject}
async fn delete_limit(
    State(state): State<AppState>,
    admin: AdminUser,
    ip: ClientIp,
    Path(subject): Path<String>,
) -> AppResult<Json<Value>> {
    if !ai::valid_subject(&subject) {
        return Err(AppError::bad_request(
            "limits are for 'default', 'user:<id>' or 'group:<id>'",
        ));
    }
    let name = subject_name(&state, &subject)
        .await
        .unwrap_or_else(|_| subject.clone());
    sqlx::query("DELETE FROM ai_limits WHERE subject = ?")
        .bind(&subject)
        .execute(&state.db)
        .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.limit.delete",
        "ai_limit",
        &subject,
        &name,
    )
    .await;
    config_changed(&state, "limits");
    Ok(Json(json!({ "deleted": true })))
}
