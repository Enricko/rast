//! AI Agent API (`/api/ai/*`, app `agent`; see [`crate::ai`]).
//!
//! People who may open the app chat in their own conversations
//! (`/api/ai/options`, `/api/ai/conversations…`, `/api/ai/usage`).
//! Administrators also set up providers, agents, skills and limits, and read
//! every account's conversations (each read of someone else's conversation
//! is audited). Tool calls wait for the conversation's owner:
//! `POST /api/ai/conversations/{id}/tools/{callId}`.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    ai::{self, host::Agent, provider, run::ReplyJob, tools, Kind, LimitRule},
    audit::AuditLog,
    auth::{AdminUser, AuthUser, ClientIp},
    db::now_ms,
    error::{AppError, AppResult},
};

pub fn routes() -> Router<Agent> {
    Router::new()
        .route("/api/ai/options", get(options))
        .route("/api/ai/whats-new", post(whats_new_seen))
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
        .route("/api/ai/files", post(upload_file))
        .route("/api/ai/files/{id}", get(get_file).delete(delete_file))
        .route("/api/ai/conversations/{id}/stop", post(stop_reply))
        .route(
            "/api/ai/conversations/{id}/artifacts/{call_id}",
            get(get_artifact),
        )
        .route(
            "/api/ai/conversations/{id}/tools/{call_id}",
            post(decide_tool),
        )
        .route("/api/ai/search", get(get_search).put(put_search))
        .route("/api/ai/search/test", post(test_search))
        .route("/api/ai/server-access", put(put_server_access))
        .route("/api/ai/admin/overview", get(admin_overview))
        .route("/api/ai/admin/conversations", get(admin_conversations))
        .route("/api/ai/admin/usage", get(admin_usage_all))
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
        .route("/api/ai/providers/{id}/accounts", post(add_account))
        .route(
            "/api/ai/providers/{id}/accounts/{account_id}",
            put(update_account).delete(delete_account),
        )
        .route(
            "/api/ai/providers/{id}/accounts/{account_id}/limits",
            put(put_account_limits),
        )
        .route("/api/ai/admin/quota", get(admin_quota))
        .route("/api/ai/admin/quota/refresh", post(refresh_quota))
        .route("/api/ai/admin/rotation", put(put_rotation_for_all))
        .route("/api/ai/providers/{id}/rotation", put(put_rotation))
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
fn config_changed(state: &Agent, what: &str) {
    state.host.publish("ai.config", json!({ "what": what }));
}

async fn audit_admin(
    state: &Agent,
    admin: &AuthUser,
    ip: &ClientIp,
    action: &str,
    kind: &str,
    id: &str,
    name: &str,
) {
    state
        .host
        .audit(
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
    state: &Agent,
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
    state: &Agent,
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
    let mut provider = ai::provider(&state.db, &provider_id)
        .await?
        .ok_or_else(|| AppError::bad_request("that provider no longer exists"))?;
    // One of a provider's other accounts: its provider.
    if let Some(first) = provider.pool_id.clone() {
        provider = ai::provider(&state.db, &first)
            .await?
            .ok_or_else(|| AppError::bad_request("that provider no longer exists"))?;
    }
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
        let limits = ai::effective_limits(state, user).await?;
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
async fn options(State(state): State<Agent>, user: AuthUser) -> AppResult<Json<Value>> {
    let limits = ai::effective_limits(&state, &user).await?;
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
    let seen = crate::db::get_setting(&state.db, &whats_new_key(user.id)).await;
    Ok(Json(json!({
        "isAdmin": user.is_admin(),
        "version": ai::app::VERSION,
        "whatsNewSeen": seen,
        "configured": configured > 0,
        "providers": providers.iter().map(ai::Provider::public_json).collect::<Vec<_>>(),
        "agents": agents,
        "allowance": ai::allowance(&state.db, user.id, &limits).await?,
    })))
}

fn whats_new_key(user_id: i64) -> String {
    format!("ai.whats-new.{user_id}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WhatsNewSeen {
    version: String,
}

/// POST /api/ai/whats-new — this account read what is new in `version`
/// (the window shows it once per version).
async fn whats_new_seen(
    State(state): State<Agent>,
    user: AuthUser,
    Json(body): Json<WhatsNewSeen>,
) -> AppResult<Json<Value>> {
    let version = body.version.trim();
    if version.is_empty()
        || version.len() > 32
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(AppError::bad_request("version is like 1.3.0"));
    }
    crate::db::set_setting(&state.db, &whats_new_key(user.id), version).await?;
    Ok(Json(json!({ "whatsNewSeen": version })))
}

/// GET /api/ai/usage
async fn my_usage(State(state): State<Agent>, user: AuthUser) -> AppResult<Json<Value>> {
    let limits = ai::effective_limits(&state, &user).await?;
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
    State(state): State<Agent>,
    user: AuthUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let pattern = format!("%{}%", query.q.as_deref().unwrap_or("").trim());
    let rows = sqlx::query(
        "SELECT c.*, (SELECT status FROM ai_messages m WHERE m.conversation_id = c.id
                      ORDER BY m.id DESC LIMIT 1) AS last_status
         FROM ai_conversations c WHERE c.user_id = ? AND c.title LIKE ?
         ORDER BY c.pinned_at IS NULL, c.updated_at DESC LIMIT ?",
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
    State(state): State<Agent>,
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
        pinned_at: None,
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
    state.host.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), false) }),
    );
    Ok(Json(json!({ "conversation": value })))
}

/// GET /api/ai/conversations/{id} — with its messages; a reply being
/// written comes with what it has so far (`seq`: deltas already in it).
async fn get_conversation(
    State(state): State<Agent>,
    user: AuthUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let conversation = owned(&state, &user, &id, false).await?;
    let mut owner: Option<String> = None;
    if conversation.user_id != user.id {
        let name = state
            .host
            .account(conversation.user_id)
            .await?
            .map(|a| a.username);
        state
            .host
            .audit(
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
    /// Keep it at the top of the list (pinning alone leaves its place in
    /// time as it is).
    pinned: Option<bool>,
}

/// PATCH /api/ai/conversations/{id}
async fn update_conversation(
    State(state): State<Agent>,
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
    if let Some(pinned) = body.pinned {
        conversation.pinned_at = if pinned {
            conversation.pinned_at.or(Some(now_ms()))
        } else {
            None
        };
    }
    let only_pinned = body.pinned.is_some()
        && body.title.is_none()
        && body.agent_id.is_none()
        && body.provider_id.is_none()
        && body.model.is_none()
        && body.allowed_tools.is_none()
        && body.effort.is_none();
    if !only_pinned {
        conversation.updated_at = now_ms();
    }
    sqlx::query(
        "UPDATE ai_conversations SET title = ?, agent_id = ?, provider_id = ?, model = ?, allowed_tools = ?,
         effort = ?, pinned_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&conversation.title)
    .bind(&conversation.agent_id)
    .bind(&conversation.provider_id)
    .bind(&conversation.model)
    .bind(serde_json::to_string(&conversation.allowed_tools).unwrap_or_else(|_| "[]".into()))
    .bind(conversation.effort.map_or("", ai::Effort::as_str))
    .bind(conversation.pinned_at)
    .bind(conversation.updated_at)
    .bind(&conversation.id)
    .execute(&state.db)
    .await?;
    let running = state.ai.running(&conversation.id);
    state.host.publish_user_and_admin(
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
    State(state): State<Agent>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let conversation = owned(&state, &user, &id, true).await?;
    state.ai.stop(&conversation.id);
    ai::browser::close(&conversation.id);
    ai::files::remove_for_conversation(&state.db, state.data_dir(), &conversation.id).await?;
    sqlx::query("DELETE FROM ai_conversations WHERE id = ?")
        .bind(&conversation.id)
        .execute(&state.db)
        .await?;
    state.host.publish_user_and_admin(
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
    /// Files added with `POST /api/ai/files`, in the order shown.
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    name: Option<String>,
}

/// POST /api/ai/files?name= — a file for the next message (raw body): an
/// image, a PDF or a text file, up to 10 MiB.
async fn upload_file(
    State(state): State<Agent>,
    user: AuthUser,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let too_large = || AppError::PayloadTooLarge("files are limited to 10 MiB each");
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|l| l > ai::files::MAX_FILE_BYTES) {
        return Err(too_large());
    }
    // Bounded read: at most the limit and one chunk is ever held.
    let mut data: Vec<u8> = Vec::with_capacity(length.unwrap_or(0) as usize);
    let mut stream = body.into_data_stream();
    while let Some(frame) = stream.next().await {
        let bytes =
            frame.map_err(|e| AppError::bad_request(format!("upload stream failed: {e}")))?;
        if data.len() as u64 + bytes.len() as u64 > ai::files::MAX_FILE_BYTES {
            return Err(too_large());
        }
        data.extend_from_slice(&bytes);
    }
    if length.is_some_and(|l| l != data.len() as u64) {
        return Err(AppError::bad_request(
            "upload ended before the whole file arrived",
        ));
    }
    let file = ai::files::store(
        &state.db,
        state.data_dir(),
        user.id,
        query.name.as_deref(),
        &declared,
        &data,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(file.json())).into_response())
}

/// What an html or svg artifact may do in its frame: run its own scripts
/// and those of a few public CDNs, in an opaque origin (`sandbox` without
/// `allow-same-origin`, also when opened in a tab of its own), so it reaches
/// neither the session nor RAST's API; no other network.
const ARTIFACT_CSP: &str = "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads; \
default-src 'none'; \
script-src 'unsafe-inline' 'unsafe-eval' blob: https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com https://esm.sh; \
style-src 'unsafe-inline' https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com https://fonts.googleapis.com; \
font-src data: https://fonts.gstatic.com https://cdnjs.cloudflare.com https://cdn.jsdelivr.net; \
img-src data: blob: https:; media-src data: blob: https:; worker-src blob:; \
connect-src https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com https://esm.sh; \
frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

/// The page an html or svg artifact shows as.
fn artifact_page(kind: &str, content: &str) -> Option<String> {
    match kind {
        "html" => {
            let start = content.trim_start().get(..15).unwrap_or("").to_ascii_lowercase();
            Some(if start.starts_with("<!doctype") || start.starts_with("<html") {
                content.to_string()
            } else {
                format!("<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n{content}")
            })
        }
        "svg" => Some(format!(
            "<!doctype html>\n<meta charset=\"utf-8\">\n<style>html,body{{margin:0;height:100%}}body{{display:grid;place-items:center;background:#fff}}body>svg{{max-width:100%;max-height:100vh;height:auto}}</style>\n{content}"
        )),
        _ => None,
    }
}

/// GET /api/ai/conversations/{id}/artifacts/{callId} — an html or svg
/// artifact as a page, for whoever may read the chat, sandboxed by
/// [`ARTIFACT_CSP`] and framed only by RAST itself. A version never changes.
async fn get_artifact(
    State(state): State<Agent>,
    user: AuthUser,
    Path((id, call_id)): Path<(String, String)>,
) -> AppResult<Response> {
    let conversation = owned(&state, &user, &id, false).await?;
    let messages = ai::messages(&state.db, &conversation.id).await?;
    let block = messages
        .iter()
        .rev()
        .filter_map(|m| m.content.as_array())
        .flatten()
        .find(|b| {
            ai::is_tool_block(b)
                && b["name"] == tools::ARTIFACT
                && b["id"] == call_id.as_str()
                && b["status"] == "done"
        })
        .ok_or(AppError::NotFound("artifact not found"))?;
    let artifact = tools::check_artifact(&block["input"])
        .map_err(|_| AppError::NotFound("artifact not found"))?;
    let page = artifact_page(&artifact.kind, &artifact.content)
        .ok_or(AppError::NotFound("this artifact shows in the app"))?;
    let mut response = page.into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(ARTIFACT_CSP),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(response)
}

/// GET /api/ai/files/{id} — the file, for its owner (and administrators,
/// who may read every chat). Images show; other files download, as text.
/// A file never changes, so its id is its ETag; every use still checks the
/// session (`no-cache`), so another account on the same browser gets none.
async fn get_file(
    State(state): State<Agent>,
    user: AuthUser,
    Path(id): Path<String>,
    request_headers: HeaderMap,
) -> AppResult<Response> {
    let file = ai::files::get(&state.db, &id)
        .await?
        .filter(|f| f.user_id == user.id || user.is_admin())
        .ok_or(AppError::NotFound("file not found"))?;
    let etag = format!("\"{}\"", file.id);
    let private = |response: &mut Response| {
        let h = response.headers_mut();
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            h.insert(header::ETAG, value);
        }
    };
    let unchanged = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    if unchanged {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        private(&mut response);
        return Ok(response);
    }
    let bytes = tokio::fs::read(ai::files::path_of(state.data_dir(), file.user_id, &file.id))
        .await
        .map_err(|_| AppError::NotFound("file not found"))?;
    let (mime, disposition) = match file.kind.as_str() {
        "image" => (file.mime.as_str(), "inline"),
        "pdf" => ("application/pdf", "attachment"),
        _ => ("text/plain; charset=utf-8", "attachment"),
    };
    let ascii: String = file
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_ ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = file
        .name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let mut response = bytes.into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).map_err(|_| AppError::internal("bad type"))?,
    );
    if let Ok(value) = HeaderValue::from_str(&format!(
        "{disposition}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    )) {
        h.insert(header::CONTENT_DISPOSITION, value);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    private(&mut response);
    Ok(response)
}

/// DELETE /api/ai/files/{id} — take a file out of the message box (only
/// before it is sent).
async fn delete_file(
    State(state): State<Agent>,
    user: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    if !ai::files::remove_unsent(&state.db, state.data_dir(), user.id, &id).await? {
        return Err(AppError::NotFound("file not found"));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// What a run is checked against before it starts.
struct Prepared {
    resolved: Resolved,
    api_key: Option<String>,
    /// The signed-in account (ChatGPT's account id), "" none.
    account: String,
    /// The provider's account that answers, and the accounts taken.
    account_id: String,
    tried: Vec<String>,
    limits: ai::Effective,
}

/// Check that `user` may go on in the conversation now: its agent and
/// model, the allowance, the provider's key.
async fn prepare(
    state: &Agent,
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
    let limits = ai::effective_limits(state, user).await?;
    ai::check_allowance(&state.db, user.id, &limits).await?;
    // The account that answers (the provider's turn order; see ai::accounts).
    let mut tried = Vec::new();
    let picked = ai::accounts::pick(
        &state.db,
        &state.secrets,
        &resolved.provider,
        &mut tried,
        true,
    )
    .await?
    .ok_or_else(|| {
        AppError::bad_request(format!(
            "{} has no account that can answer; an administrator checks it in the AI Agent settings",
            resolved.provider.name
        ))
    })?;
    let (api_key, account, account_id) = (picked.api_key, picked.account_ref, picked.account.id);
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
        account_id,
        tried,
        limits,
    })
}

/// The run for reply `message_id` (its history: the messages before it).
async fn reply_job(
    state: &Agent,
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
        account_id,
        tried,
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
    let mut messages = ai::history(&earlier);
    ai::files::load(state.data_dir(), conversation.user_id, &mut messages).await;
    let request = provider::ChatRequest {
        kind: resolved.provider.kind,
        base_url: resolved.provider.effective_base_url(),
        api_key,
        model: resolved.model.clone(),
        system: ai::system_prompt(resolved.agent.as_ref(), &skills, user, &today, &available),
        messages,
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
        account_id,
        tried,
        model: resolved.model,
        request,
        tools: available,
        allowed_tools: conversation.allowed_tools.clone(),
        blocks,
        limits,
        tokens,
    })
}

async fn last_message(state: &Agent, conversation_id: &str) -> AppResult<Option<ai::Message>> {
    let row =
        sqlx::query("SELECT * FROM ai_messages WHERE conversation_id = ? ORDER BY id DESC LIMIT 1")
            .bind(conversation_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.as_ref().map(ai::message_from_row))
}

fn publish_message(state: &Agent, owner_id: i64, message: &ai::Message, running: bool) {
    state.host.publish_user_and_admin(
        "ai.message",
        owner_id,
        json!({"conversationId": message.conversation_id, "message": message.json(), "running": running, "seq": 0}),
    );
}

/// End a reply that waits for tool answers (stopped, its calls not run).
/// Call with `decide` held. Returns whether one waited.
async fn end_waiting(state: &Agent, conversation: &ai::Conversation) -> AppResult<bool> {
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
    State(state): State<Agent>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<NewMessage>,
) -> AppResult<Json<Value>> {
    let _deciding = state.ai.decide.lock().await;
    let mut conversation = owned(&state, &user, &id, true).await?;
    let text = body.text.trim_end().to_string();
    if text.trim().is_empty() && body.files.is_empty() {
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

    let files = ai::files::attach(&state.db, user.id, &conversation.id, &body.files).await?;
    let now = now_ms();
    let mut blocks: Vec<Value> = files.iter().map(ai::files::StoredFile::block).collect();
    blocks.push(json!({ "type": "text", "text": text }));
    let user_content = Value::Array(blocks);
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
        conversation.title = match files.first() {
            Some(file) if text.trim().is_empty() => ai::title_from(&file.name),
            _ => ai::title_from(&text),
        };
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
        state.host.publish_user_and_admin(
            "ai.message",
            user.id,
            json!({"conversationId": conversation.id, "message": message, "running": true}),
        );
    }
    state.host.publish_user_and_admin(
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
    State(state): State<Agent>,
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
        state.host.publish_user_and_admin(
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
    State(state): State<Agent>,
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
    state.host.publish_user_and_admin(
        "ai.conversation",
        user.id,
        json!({ "conversation": conversation.json(Some(&user.username), true) }),
    );
    Ok(Json(
        json!({ "message": message.json(), "conversation": conversation.json(None, true) }),
    ))
}

async fn store_allowed_tools(state: &Agent, conversation: &ai::Conversation) -> AppResult<()> {
    sqlx::query("UPDATE ai_conversations SET allowed_tools = ? WHERE id = ?")
        .bind(serde_json::to_string(&conversation.allowed_tools).unwrap_or_else(|_| "[]".into()))
        .bind(&conversation.id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// -------------------------------------------------------- administrators

/// GET /api/ai/admin/overview — the whole setup, for the settings page.
async fn admin_overview(State(state): State<Agent>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    let mut providers: Vec<Value> = Vec::new();
    for p in ai::providers(&state.db).await? {
        providers.push(provider_json(&state, &p).await?);
    }
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
    let users: Vec<Value> = state
        .host
        .accounts()
        .await?
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "username": a.username,
                "displayName": a.display_name,
                "isAdmin": a.is_admin(),
                "active": a.active,
            })
        })
        .collect();
    let groups: Vec<Value> = state
        .host
        .groups()
        .await?
        .iter()
        .map(|g| json!({"id": g.id, "name": g.name}))
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
                "estimateToday": ai::micros_to_usd(used.estimate[0]),
                "estimateWeek": ai::micros_to_usd(used.estimate[1]),
                "estimateMonth": ai::micros_to_usd(used.estimate[2]),
                "lastAt": last_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "kinds": kinds,
        "rotation": ai::accounts::default_rotation(&state.db).await,
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
        "browser": {"chrome": ai::browser::chrome().is_some()},
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageQuery {
    days: Option<u32>,
}

/// GET /api/ai/admin/usage?days=30 — everyone's use together: per day and
/// per model, with totals and what it is worth at API prices.
async fn admin_usage_all(
    State(state): State<Agent>,
    admin: AdminUser,
    Query(query): Query<UsageQuery>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let days = query.days.unwrap_or(30).clamp(1, 366);
    Ok(Json(ai::usage_history(&state.db, None, days).await?))
}

/// GET /api/ai/admin/usage/{userId}?days=30 — one account's use: its
/// allowance now, a total per day and per model.
async fn admin_usage(
    State(state): State<Agent>,
    admin: AdminUser,
    Path(user_id): Path<i64>,
    Query(query): Query<UsageQuery>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let account = state
        .host
        .account(user_id)
        .await?
        .ok_or(AppError::NotFound("no such account"))?;
    let is_admin = account.is_admin();
    let limits = ai::effective_limits_for(&state.db, user_id, is_admin, &account.groups).await?;
    let days = query.days.unwrap_or(30).clamp(1, 366);
    let mut history = ai::usage_history(&state.db, Some(user_id), days).await?;
    history["user"] = json!({
        "id": user_id,
        "username": account.username,
        "displayName": account.display_name,
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
    State(state): State<Agent>,
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
async fn get_search(State(state): State<Agent>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    Ok(Json(
        json!({ "search": tools::search_config(&state.db).await.public_json(&state.secrets) }),
    ))
}

/// PUT /api/ai/search
async fn put_search(
    State(state): State<Agent>,
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
async fn test_search(State(state): State<Agent>, admin: AdminUser) -> AppResult<Json<Value>> {
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
    State(state): State<Agent>,
    admin: AdminUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let text = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let pattern = format!("%{text}%");
    // Accounts RAST knows (a removed account's chats stay out), and those
    // whose name matches the search.
    let accounts = state.host.accounts().await?;
    let names: std::collections::HashMap<i64, String> = accounts
        .iter()
        .map(|a| (a.id, a.username.clone()))
        .collect();
    let known: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    let matching: Vec<i64> = accounts
        .iter()
        .filter(|a| a.username.to_lowercase().contains(&text))
        .map(|a| a.id)
        .collect();
    let rows = sqlx::query(
        "SELECT c.*,
                (SELECT COUNT(*) FROM ai_messages m WHERE m.conversation_id = c.id) AS message_count,
                (SELECT status FROM ai_messages m WHERE m.conversation_id = c.id
                 ORDER BY m.id DESC LIMIT 1) AS last_status
         FROM ai_conversations c
         WHERE c.user_id IN (SELECT value FROM json_each(?))
           AND (? IS NULL OR c.user_id = ?)
           AND (c.title LIKE ? OR c.user_id IN (SELECT value FROM json_each(?)))
         ORDER BY c.updated_at DESC LIMIT ?",
    )
    .bind(json!(known).to_string())
    .bind(query.user_id)
    .bind(query.user_id)
    .bind(&pattern)
    .bind(json!(matching).to_string())
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let conversation = ai::conversation_from_row(row);
            let owner = names.get(&conversation.user_id).map(String::as_str);
            let mut value = conversation.json(owner, state.ai.running(&conversation.id));
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
    /// How the accounts take turns (absent: keep it).
    rotation: Option<ai::accounts::Rotation>,
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
    State(state): State<Agent>,
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
             prices_checked_at, enabled, created_at, updated_at, rotation)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(ai::accounts::default_rotation(&state.db).await.to_json())
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
        json!({ "provider": provider_json(&state, &provider).await? }),
    ))
}

/// PUT /api/ai/providers/{id}
async fn update_provider(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<ProviderBody>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    if existing.pool_id.is_some() {
        return Err(AppError::bad_request(
            "that is an account of another provider; change it under the provider's accounts",
        ));
    }
    let rotation = match body.rotation {
        Some(rotation) if !rotation.valid() => {
            return Err(AppError::bad_request(format!(
                "replies in a row per account go from 1 to {}",
                ai::accounts::MAX_STICKY
            )))
        }
        Some(rotation) => rotation,
        None => existing.rotation,
    };
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
             rotation = ?, updated_at = ? WHERE id = ?",
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
    .bind(serde_json::to_string(&rotation).unwrap_or_else(|_| "{}".into()))
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
        json!({ "provider": provider_json(&state, &provider).await? }),
    ))
}

/// DELETE /api/ai/providers/{id} (agents and conversations using it keep
/// their messages and need another model). Its accounts go with it.
async fn delete_provider(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let existing = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    sqlx::query("DELETE FROM ai_providers WHERE id = ? OR pool_id = ?")
        .bind(&id)
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
async fn get_prices(State(state): State<Agent>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    Ok(Json(ai::prices::listing(&ai::providers(&state.db).await?)))
}

/// POST /api/ai/prices/refresh — ask every provider in use (not local)
/// for its list prices now.
async fn refresh_prices(
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
#[allow(clippy::too_many_arguments)] // where it goes: again, or into a provider
async fn store_signed_in(
    state: &Agent,
    user: &AuthUser,
    ip: &ClientIp,
    plan: ai::signin::Plan,
    provider_id: Option<&str>,
    add_to: Option<&str>,
    name: &str,
    credentials: ai::signin::Credentials,
) -> AppResult<(ai::Provider, Option<Vec<String>>)> {
    let kind = plan.kind();
    // Adding an account the provider already has signs that one in again.
    let mut provider_id = provider_id.map(str::to_string);
    if let (None, Some(first_id)) = (&provider_id, add_to) {
        if let Some(first) = ai::provider(&state.db, first_id).await? {
            for account in ai::accounts::of(&state.db, &first).await? {
                let same = account
                    .credentials(&state.secrets)
                    .ok()
                    .flatten()
                    .is_some_and(|c| !c.email.is_empty() && c.email == credentials.email);
                if same {
                    provider_id = Some(account.id.clone());
                    break;
                }
            }
        }
    }
    let provider_id = provider_id.as_deref();
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
            let position = match add_to {
                Some(first) => next_position(state, first).await?,
                None => 0,
            };
            sqlx::query(
                "INSERT INTO ai_providers (id, kind, name, base_url, api_key, models, prices, list_prices,
                     enabled, created_at, updated_at, owner_id, sign_in, pool_id, position, rotation)
                 VALUES (?, ?, ?, '', '', ?, '{}', '{}', 1, ?, ?, ?, ?, ?, ?, ?)",
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
            .bind(add_to)
            .bind(position)
            .bind(ai::accounts::default_rotation(&state.db).await.to_json())
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
    /// Add the account to this provider (one provider, several accounts).
    add_to: Option<String>,
    #[serde(default)]
    name: String,
}

/// Sign-ins started to add an account to a provider: sign-in state →
/// (provider id, when started).
type AddTo = std::collections::HashMap<String, (String, i64)>;
static ADD_TO: std::sync::LazyLock<std::sync::Mutex<AddTo>> =
    std::sync::LazyLock::new(Default::default);

fn remember_add_to(state: &str, provider_id: &str) {
    let now = now_ms();
    let mut map = ADD_TO.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, (_, at)| now - *at < 60 * 60 * 1000);
    map.insert(state.to_string(), (provider_id.to_string(), now));
}

fn take_add_to(state: &str) -> Option<String> {
    ADD_TO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(state)
        .map(|(id, _)| id)
}

/// POST /api/ai/providers/sign-in — start signing in to a plan: a page to
/// open (browser flows) or a code to enter (device flows); see
/// [`ai::signin`].
async fn start_sign_in(
    State(state): State<Agent>,
    admin: AdminUser,
    Json(body): Json<SignInStart>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&body.kind)
        .filter(|k| k.signs_in())
        .ok_or_else(|| AppError::bad_request("that provider does not sign in"))?;
    let plan = kind
        .plan()
        .ok_or_else(|| AppError::bad_request("that provider does not sign in"))?;
    for id in body.provider_id.iter().chain(body.add_to.iter()) {
        let existing = ai::provider(&state.db, id)
            .await?
            .ok_or(AppError::NotFound("provider not found"))?;
        if existing.kind != kind {
            return Err(AppError::bad_request("that provider is of another kind"));
        }
    }
    if let Some(id) = &body.add_to {
        let first = ai::provider(&state.db, id)
            .await?
            .ok_or(AppError::NotFound("provider not found"))?;
        if first.pool_id.is_some() {
            return Err(AppError::bad_request("add accounts to the provider itself"));
        }
        // A plan is one person's: its accounts are its owner's.
        if first.owner_id.is_some_and(|owner| owner != admin.0.id) {
            return Err(AppError::bad_request(
                "only the person who signed in to this provider adds accounts to it",
            ));
        }
        if ai::accounts::of(&state.db, &first).await?.len() >= ai::accounts::MAX_ACCOUNTS {
            return Err(AppError::bad_request(format!(
                "a provider has at most {} accounts",
                ai::accounts::MAX_ACCOUNTS
            )));
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
    if let (Some(add_to), Some(sign_in_state)) = (&body.add_to, out["state"].as_str()) {
        remember_add_to(sign_in_state, add_to);
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
    State(state): State<Agent>,
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
    let add_to = take_add_to(&callback.state);
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
        add_to.as_deref(),
        &pending.name,
        credentials,
    )
    .await?;
    Ok(Json(json!({
        "provider": provider_json(&state, &provider).await?,
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
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Json(body): Json<SignInPoll>,
) -> AppResult<Json<Value>> {
    let state_id = body.state;
    let sign_in_state = state_id.clone();
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
            let add_to = take_add_to(&sign_in_state);
            let (provider, offered) = store_signed_in(
                &state,
                &admin.0,
                &ip,
                signed_in.plan,
                signed_in.provider_id.as_deref(),
                add_to.as_deref(),
                &signed_in.name,
                signed_in.credentials,
            )
            .await?;
            Ok(Json(json!({
                "status": "done",
                "provider": provider_json(&state, &provider).await?,
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
    State(state): State<Agent>,
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
        None,
        &name,
        credentials,
    )
    .await?;
    Ok(Json(json!({
        "provider": provider_json(&state, &provider).await?,
        "offered": offered,
    })))
}

/// POST /api/ai/providers/{id}/sign-out — forget a plan's tokens: the
/// provider stays (and its models), but nobody can chat with it until
/// someone signs in again.
async fn sign_out_provider(
    State(state): State<Agent>,
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
    // A provider with other accounts stays its owner's: they still answer.
    let keeps_owner =
        existing.pool_id.is_some() || ai::accounts::of(&state.db, &existing).await?.len() > 1;
    sqlx::query(
        "UPDATE ai_providers SET sign_in = '', owner_id = CASE WHEN ? THEN owner_id END, updated_at = ?
         WHERE id = ?",
    )
    .bind(keeps_owner)
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
        json!({"provider": provider_json(&state, &provider).await?}),
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

async fn check_agent(state: &Agent, body: &AgentBody) -> AppResult<CheckedAgent> {
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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

async fn subject_name(state: &Agent, subject: &str) -> AppResult<String> {
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
        state.host.account(id).await?.map(|a| a.username)
    } else {
        state
            .host
            .groups()
            .await?
            .into_iter()
            .find(|g| g.id == id)
            .map(|g| g.name)
    };
    name.ok_or(AppError::NotFound("no such account or group"))
}

/// PUT /api/ai/limits/{subject}
async fn put_limit(
    State(state): State<Agent>,
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
    State(state): State<Agent>,
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

// ------------------------------------------------------------- accounts

/// A provider for the settings page, with its accounts (the provider first).
async fn provider_json(state: &Agent, provider: &ai::Provider) -> AppResult<Value> {
    let now = now_ms();
    // An account: its provider.
    let first = match &provider.pool_id {
        Some(id) => ai::provider(&state.db, id).await?,
        None => None,
    };
    let provider = first.as_ref().unwrap_or(provider);
    let mut json = provider.admin_json(&state.secrets);
    json["accounts"] = ai::accounts::of(&state.db, provider)
        .await?
        .iter()
        .map(|a| a.account_json(&state.secrets, now))
        .collect();
    Ok(json)
}

/// The provider an account route names (a provider, not one of its accounts).
async fn first_account(state: &Agent, id: &str) -> AppResult<ai::Provider> {
    let provider = ai::provider(&state.db, id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    if provider.pool_id.is_some() {
        return Err(AppError::bad_request("add accounts to the provider itself"));
    }
    Ok(provider)
}

async fn next_position(state: &Agent, first_id: &str) -> AppResult<i64> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(position) FROM ai_providers WHERE pool_id = ?")
            .bind(first_id)
            .fetch_one(&state.db)
            .await?;
    Ok(max.unwrap_or(0) + 1)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NewAccount {
    api_key: String,
}

/// POST /api/ai/providers/{id}/accounts — another API key for the provider
/// (plans add accounts by signing in with `addTo`).
async fn add_account(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<NewAccount>,
) -> AppResult<Json<Value>> {
    let first = first_account(&state, &id).await?;
    if first.kind.signs_in() {
        return Err(AppError::bad_request(format!(
            "{} accounts are added by signing in",
            first.kind.label()
        )));
    }
    let accounts = ai::accounts::of(&state.db, &first).await?;
    if accounts.len() >= ai::accounts::MAX_ACCOUNTS {
        return Err(AppError::bad_request(format!(
            "a provider has at most {} accounts",
            ai::accounts::MAX_ACCOUNTS
        )));
    }
    let key = checked_key(Some(&body.api_key))?
        .ok_or_else(|| AppError::bad_request("paste the account's API key"))?;
    let sealed = ai::seal_key(&state.secrets, &key)?;
    let account_id = ai::new_id();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO ai_providers (id, kind, name, base_url, api_key, models, prices, list_prices,
             enabled, created_at, updated_at, pool_id, position)
         VALUES (?, ?, ?, '', ?, '[]', '{}', '{}', 1, ?, ?, ?, ?)",
    )
    .bind(&account_id)
    .bind(first.kind.as_str())
    .bind(&first.name)
    .bind(&sealed)
    .bind(now)
    .bind(now)
    .bind(&first.id)
    .bind(next_position(&state, &first.id).await?)
    .execute(&state.db)
    .await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.account_add",
        "ai_provider",
        &account_id,
        &first.name,
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(
        json!({ "provider": provider_json(&state, &first).await? }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountChange {
    /// Set the account aside, or take it back.
    paused: Option<bool>,
    /// Let a resting account answer again now.
    #[serde(default)]
    wake: bool,
    /// "up" or "down" in the turn order.
    #[serde(rename = "move")]
    move_to: Option<String>,
}

/// Swap two accounts' credentials and state (the rows keep their ids, so
/// the provider stays the provider).
async fn swap_accounts(state: &Agent, a: &ai::Provider, b: &ai::Provider) -> AppResult<()> {
    let pick = |p: &ai::Provider| {
        (
            p.sealed_key().to_string(),
            p.sealed_sign_in().to_string(),
            p.owner_id,
            p.paused as i64,
            p.resting_until,
            p.resting_reason.clone(),
            serde_json::to_string(&p.quota).unwrap_or_else(|_| "{}".into()),
        )
    };
    let (va, vb) = (pick(a), pick(b));
    let mut tx = state.db.begin().await?;
    for (id, v) in [(&a.id, vb), (&b.id, va)] {
        sqlx::query(
            "UPDATE ai_providers SET api_key = ?, sign_in = ?, owner_id = ?, paused = ?,
                 resting_until = ?, resting_reason = ?, quota = ? WHERE id = ?",
        )
        .bind(v.0)
        .bind(v.1)
        .bind(v.2)
        .bind(v.3)
        .bind(v.4)
        .bind(v.5)
        .bind(v.6)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE ai_usage SET account_id = CASE account_id WHEN ?1 THEN ?2 ELSE ?1 END
         WHERE account_id IN (?1, ?2)",
    )
    .bind(&a.id)
    .bind(&b.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    ai::accounts::swap_kept_limits(&state.db, &a.id, &b.id).await
}

/// PUT /api/ai/providers/{id}/accounts/{account_id} — set an account aside
/// or take it back, let it answer again now, or move it in the order.
async fn update_account(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path((id, account_id)): Path<(String, String)>,
    Json(body): Json<AccountChange>,
) -> AppResult<Json<Value>> {
    let first = first_account(&state, &id).await?;
    let accounts = ai::accounts::of(&state.db, &first).await?;
    let at = accounts
        .iter()
        .position(|a| a.id == account_id)
        .ok_or(AppError::NotFound("account not found"))?;
    if let Some(paused) = body.paused {
        sqlx::query("UPDATE ai_providers SET paused = ? WHERE id = ?")
            .bind(paused as i64)
            .bind(&account_id)
            .execute(&state.db)
            .await?;
    }
    if body.wake {
        ai::accounts::wake(&state.db, &account_id).await?;
    }
    if let Some(direction) = body.move_to.as_deref() {
        let other = match direction {
            "up" if at > 0 => at - 1,
            "down" if at + 1 < accounts.len() => at + 1,
            "up" | "down" => at,
            _ => return Err(AppError::bad_request("move is \"up\" or \"down\"")),
        };
        if other != at {
            let (a, b) = (&accounts[at.min(other)], &accounts[at.max(other)]);
            if a.pool_id.is_none() {
                // The provider row stays first: its credentials move instead.
                swap_accounts(&state, a, b).await?;
            } else {
                let mut tx = state.db.begin().await?;
                for (row, position) in [(a, b.position), (b, a.position)] {
                    sqlx::query("UPDATE ai_providers SET position = ? WHERE id = ?")
                        .bind(position)
                        .bind(&row.id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                // Equal positions (older rows): number them in order first.
                if a.position == b.position {
                    let mut tx = state.db.begin().await?;
                    for (i, row) in accounts.iter().enumerate().skip(1) {
                        let i = i as i64;
                        let position = if row.id == a.id {
                            i + 1
                        } else if row.id == b.id {
                            i - 1
                        } else {
                            i
                        };
                        sqlx::query("UPDATE ai_providers SET position = ? WHERE id = ?")
                            .bind(position)
                            .bind(&row.id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    tx.commit().await?;
                }
            }
        }
    }
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.account_update",
        "ai_provider",
        &account_id,
        &first.name,
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(
        json!({ "provider": provider_json(&state, &first).await? }),
    ))
}

/// DELETE /api/ai/providers/{id}/accounts/{account_id} — remove one account
/// (the provider's own: the next account's credentials take its place).
async fn delete_account(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path((id, account_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let first = first_account(&state, &id).await?;
    let accounts = ai::accounts::of(&state.db, &first).await?;
    let account = accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or(AppError::NotFound("account not found"))?;
    let removed = if account.pool_id.is_none() {
        let Some(next) = accounts.get(1) else {
            return Err(AppError::bad_request(
                "this is the provider's only account; delete the provider instead",
            ));
        };
        swap_accounts(&state, account, next).await?;
        next.id.clone()
    } else {
        account.id.clone()
    };
    sqlx::query("DELETE FROM ai_providers WHERE id = ? AND pool_id IS NOT NULL")
        .bind(&removed)
        .execute(&state.db)
        .await?;
    ai::accounts::forget_kept_limits(&state.db, &removed).await;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.account_delete",
        "ai_provider",
        &account_id,
        &first.name,
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(
        json!({ "provider": provider_json(&state, &first).await? }),
    ))
}

/// Tokens an administrator can set as an account's limit, at most.
const MAX_KEPT_LIMIT: i64 = 1_000_000_000_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountLimits {
    /// Tokens per 5 hours (`null`: none).
    hours5: Option<i64>,
    /// Tokens per week (`null`: none).
    week: Option<i64>,
}

/// PUT /api/ai/providers/{id}/accounts/{account_id}/limits — the tokens per
/// 5 hours and per week RAST lets the account use (it rests at a limit and
/// the next account answers); `null` clears one.
async fn put_account_limits(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path((id, account_id)): Path<(String, String)>,
    Json(body): Json<AccountLimits>,
) -> AppResult<Json<Value>> {
    let first = first_account(&state, &id).await?;
    let accounts = ai::accounts::of(&state.db, &first).await?;
    if !accounts.iter().any(|a| a.id == account_id) {
        return Err(AppError::NotFound("account not found"));
    }
    for limit in [body.hours5, body.week].into_iter().flatten() {
        if !(1..=MAX_KEPT_LIMIT).contains(&limit) {
            return Err(AppError::bad_request(format!(
                "a limit goes from 1 to {MAX_KEPT_LIMIT} tokens"
            )));
        }
    }
    ai::accounts::set_kept_limits(&state.db, &account_id, body.hours5, body.week).await?;
    // A lower limit can already be reached.
    ai::accounts::keep_limits(&state.db, &account_id, now_ms()).await;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.account_limits",
        "ai_provider",
        &account_id,
        &first.name,
    )
    .await;
    config_changed(&state, "providers");
    let kept = ai::accounts::kept_limits(&state.db)
        .await
        .get(&account_id)
        .copied()
        .unwrap_or_default();
    Ok(Json(json!({ "limits": kept })))
}

/// GET /api/ai/admin/quota — every account's limits as its provider last
/// reported them, its rest, and what RAST counted for it (model calls and
/// tokens in the last 5 hours, 24 hours and 7 days, and per model).
async fn admin_quota(State(state): State<Agent>, admin: AdminUser) -> AppResult<Json<Value>> {
    let _ = admin;
    Ok(Json(quota_overview(&state, &Default::default()).await?))
}

/// Whether RAST can ask the account's provider for its limits on demand
/// (ChatGPT's Codex plans).
fn can_check(account: &ai::Provider) -> bool {
    account.kind == Kind::Codex
}

async fn quota_overview(
    state: &Agent,
    checks: &std::collections::HashMap<String, String>,
) -> AppResult<Value> {
    let now = now_ms();
    let (h5, d1, d7) = (now - 5 * 3_600_000, now - 86_400_000, now - 7 * 86_400_000);
    let rows = sqlx::query(
        "SELECT account_id, model,
             SUM(created_at >= ?1), SUM(CASE WHEN created_at >= ?1 THEN input_tokens + output_tokens ELSE 0 END),
             SUM(created_at >= ?2), SUM(CASE WHEN created_at >= ?2 THEN input_tokens + output_tokens ELSE 0 END),
             COUNT(*), SUM(input_tokens + output_tokens),
             MIN(CASE WHEN created_at >= ?1 THEN created_at END), MIN(created_at)
         FROM ai_usage WHERE created_at >= ?3 AND account_id IS NOT NULL
         GROUP BY account_id, model",
    )
    .bind(h5)
    .bind(d1)
    .bind(d7)
    .fetch_all(&state.db)
    .await?;
    // Per account: totals per window, and per model.
    #[derive(Default)]
    struct Counts {
        totals: [i64; 6],
        models: Vec<(String, [i64; 6])>,
        /// The oldest reply in the 5 hours and in the 7 days (0: none).
        oldest: [i64; 2],
    }
    let mut counted: std::collections::HashMap<String, Counts> = std::collections::HashMap::new();
    for row in &rows {
        let account: String = row.get(0);
        let model: String = row.get(1);
        let mut values = [0i64; 6];
        for (i, value) in values.iter_mut().enumerate() {
            *value = row.try_get::<i64, _>(i + 2).unwrap_or(0);
        }
        let entry = counted.entry(account).or_default();
        for (total, value) in entry.totals.iter_mut().zip(values) {
            *total += value;
        }
        for (i, oldest) in entry.oldest.iter_mut().enumerate() {
            if let Ok(Some(at)) = row.try_get::<Option<i64>, _>(i + 8) {
                if *oldest == 0 || at < *oldest {
                    *oldest = at;
                }
            }
        }
        entry.models.push((model, values));
    }
    let kept = ai::accounts::kept_limits(&state.db).await;
    let window = |v: &[i64; 6], at: usize| json!({"calls": v[at], "tokens": v[at + 1]});
    let mut providers = Vec::new();
    for provider in ai::providers(&state.db).await? {
        let accounts: Vec<Value> = ai::accounts::of(&state.db, &provider)
            .await?
            .iter()
            .map(|a| {
                let mut json = a.account_json(&state.secrets, now);
                let counts = counted.get(&a.id);
                let totals = counts.map(|c| c.totals).unwrap_or_default();
                json["counted"] = json!({
                    "hours5": window(&totals, 0),
                    "day": window(&totals, 2),
                    "week": window(&totals, 4),
                });
                let oldest = counts.map(|c| c.oldest).unwrap_or_default();
                for (key, at, length) in [
                    ("hours5", oldest[0], ai::accounts::HOURS5_MS),
                    ("week", oldest[1], ai::accounts::WEEK_MS),
                ] {
                    if at > 0 {
                        // The oldest reply in the window leaves it then.
                        json["counted"][key]["leavesAt"] = json!(at + length);
                    }
                }
                json["limits"] = json!(kept.get(&a.id).copied().unwrap_or_default());
                let mut models: Vec<&(String, [i64; 6])> = counts
                    .map(|c| c.models.iter().collect())
                    .unwrap_or_default();
                models.sort_by(|x, y| y.1[5].cmp(&x.1[5]).then_with(|| x.0.cmp(&y.0)));
                json["models"] = models
                    .iter()
                    .take(8)
                    .map(|(model, v)| {
                        json!({"model": model, "hours5": window(v, 0), "week": window(v, 4)})
                    })
                    .collect();
                json["canCheck"] = json!(can_check(a) && a.has_credentials());
                if let Some(error) = checks.get(&a.id) {
                    json["checkError"] = json!(error);
                }
                json
            })
            .collect();
        providers.push(json!({
            "id": provider.id,
            "name": provider.name,
            "kind": provider.kind.as_str(),
            "kindLabel": provider.kind.label(),
            "signsIn": provider.kind.signs_in(),
            "enabled": provider.enabled,
            "rotation": provider.rotation,
            "accounts": accounts,
        }));
    }
    Ok(json!({
        "providers": providers,
        "rotation": ai::accounts::default_rotation(&state.db).await,
        "now": now,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuotaRefresh {
    /// One account (absent: every account RAST can ask, unless asked in the
    /// last minute).
    account_id: Option<String>,
}

/// POST /api/ai/admin/quota/refresh — ask the providers that say on demand
/// (ChatGPT's Codex plans) for their accounts' limits now; returns the
/// quota page with any account whose provider could not be asked.
async fn refresh_quota(
    State(state): State<Agent>,
    admin: AdminUser,
    body: Option<Json<QuotaRefresh>>,
) -> AppResult<Json<Value>> {
    let _ = admin;
    let only = body.and_then(|Json(b)| b.account_id);
    let now = now_ms();
    let mut checks = std::collections::HashMap::new();
    for provider in ai::providers(&state.db).await? {
        for account in ai::accounts::of(&state.db, &provider).await? {
            let asked = only.as_deref().is_some_and(|id| id == account.id);
            if only.is_some() && !asked {
                continue;
            }
            if !can_check(&account) || !account.has_credentials() {
                continue;
            }
            // Every minute at most, unless this account was asked for.
            if !asked && now - account.quota.checked_at < 60_000 {
                continue;
            }
            let result = match ai::signed_in_token(&state.db, &state.secrets, &account).await {
                Ok((token, account_ref)) => {
                    let base = account.effective_base_url();
                    tokio::task::spawn_blocking(move || {
                        provider::plans::codex::codex_usage(&base, &token, &account_ref, now_ms())
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                }
                Err(error) => Err(error.public_message()),
            };
            match result {
                Ok((windows, _plan)) => {
                    let quota = provider::limits::Quota {
                        windows,
                        checked_at: now_ms(),
                    };
                    ai::accounts::store_quota(&state.db, &account.id, &quota).await;
                }
                Err(error) => {
                    checks.insert(account.id.clone(), error);
                }
            }
        }
    }
    Ok(Json(quota_overview(&state, &checks).await?))
}

/// PUT /api/ai/admin/rotation — round robin (or fill first) for every
/// provider's accounts, and for providers added later.
async fn put_rotation_for_all(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Json(rotation): Json<ai::accounts::Rotation>,
) -> AppResult<Json<Value>> {
    if !rotation.valid() {
        return Err(AppError::bad_request(format!(
            "replies in a row per account go from 1 to {}",
            ai::accounts::MAX_STICKY
        )));
    }
    ai::accounts::set_rotation_for_all(&state.db, rotation).await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.rotation_all",
        "ai_settings",
        "rotation",
        &rotation.to_json(),
    )
    .await;
    config_changed(&state, "providers");
    Ok(Json(json!({ "rotation": rotation })))
}

/// PUT /api/ai/providers/{id}/rotation — how one provider's accounts take
/// turns.
async fn put_rotation(
    State(state): State<Agent>,
    admin: AdminUser,
    ip: ClientIp,
    Path(id): Path<String>,
    Json(rotation): Json<ai::accounts::Rotation>,
) -> AppResult<Json<Value>> {
    let provider = first_account(&state, &id).await?;
    if !rotation.valid() {
        return Err(AppError::bad_request(format!(
            "replies in a row per account go from 1 to {}",
            ai::accounts::MAX_STICKY
        )));
    }
    ai::accounts::set_rotation(&state.db, &provider.id, rotation).await?;
    audit_admin(
        &state,
        &admin.0,
        &ip,
        "ai.provider.rotation",
        "ai_provider",
        &provider.id,
        &provider.name,
    )
    .await;
    config_changed(&state, "providers");
    let provider = ai::provider(&state.db, &id)
        .await?
        .ok_or(AppError::NotFound("provider not found"))?;
    Ok(Json(
        json!({ "provider": provider_json(&state, &provider).await? }),
    ))
}
