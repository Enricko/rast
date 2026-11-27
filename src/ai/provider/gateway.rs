//! Gateways that serve each model on its own protocol (OpenCode): reading
//! their model list, remembering which model goes over which wire, and
//! picking that model's wire for a request.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;

use super::models::fetch_models;
use super::{ChatRequest, Wire};
use crate::ai::{Kind, Protocol};

/// How long a gateway's model routes are used before they are read again.
pub(crate) const ROUTES_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// …and how long before trying again when they could not be read.
const ROUTES_RETRY: Duration = Duration::from_secs(60);

/// A model of a gateway that routes by model.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayModel {
    pub id: String,
    pub route: Route,
    /// A chat model RAST may call (not an embedding or image model, and not
    /// a free one, which only the gateway's own apps may use).
    pub usable: bool,
}

/// How a gateway serves one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub anthropic: bool,
    pub reasoning_back: bool,
}

impl Route {
    /// For a model the list did not have (or could not be read).
    pub(crate) fn guess(model: &str) -> Self {
        Self {
            anthropic: model.starts_with("claude-"),
            reasoning_back: false,
        }
    }
}

fn gateway_models(
    kind: Kind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<GatewayModel>, String> {
    parse_gateway_models(&fetch_models(kind, base_url, api_key)?)
}

/// A gateway's model list (OpenAI's list shape, each model with
/// `recommended_api`, `api`, `type` and `cost`).
pub fn parse_gateway_models(body: &str) -> Result<Vec<GatewayModel>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "the provider's model list is not JSON".to_string())?;
    let list = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "the provider did not send a model list".to_string())?;
    let price = |m: &Value, side: &str| {
        m.pointer(&format!("/cost/text/{side}/price"))
            .and_then(Value::as_f64)
    };
    Ok(list
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            if id.is_empty() || id.len() > super::super::MAX_MODEL_NAME {
                return None;
            }
            let api = m.get("api").cloned().unwrap_or(Value::Null);
            let chat_api = api.get("chat-completions").is_some();
            let anthropic = match m.get("recommended_api").and_then(Value::as_str) {
                Some(recommended) => recommended == "messages",
                None => api.get("messages").is_some() && !chat_api,
            };
            let reasoning_back = api
                .pointer("/chat-completions/compatibility/reasoning_field")
                .and_then(Value::as_str)
                == Some("reasoning_content");
            let chat = m
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "chat");
            let free = price(m, "input") == Some(0.0) && price(m, "output") == Some(0.0);
            Some(GatewayModel {
                id: id.to_string(),
                route: Route {
                    anthropic,
                    reasoning_back,
                },
                usable: chat && !free,
            })
        })
        .collect())
}

/// Gateways' routes by base URL, and until when they are used.
type Routes = HashMap<String, (Instant, HashMap<String, Route>)>;
static ROUTES: LazyLock<Mutex<Routes>> = LazyLock::new(Default::default);

pub(crate) fn remember_routes(base_url: &str, models: &[GatewayModel], keep: Duration) {
    let routes = models.iter().map(|m| (m.id.clone(), m.route)).collect();
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(base_url.to_string(), (Instant::now() + keep, routes));
}

/// How a gateway wants `req.model` sent (reads its model list when the
/// routes are old).
pub(crate) fn gateway_wire(req: &ChatRequest) -> Wire {
    let known = |routes: &Routes| {
        routes
            .get(&req.base_url)
            .filter(|(until, _)| *until > Instant::now())
            .map(|(_, models)| models.get(&req.model).copied())
    };
    let cached = known(&ROUTES.lock().unwrap_or_else(|e| e.into_inner()));
    let route = match cached {
        Some(route) => route,
        None => {
            match gateway_models(req.kind, &req.base_url, req.api_key.as_deref()) {
                Ok(models) => remember_routes(&req.base_url, &models, ROUTES_FOR),
                Err(_) => remember_routes(&req.base_url, &[], ROUTES_RETRY),
            }
            known(&ROUTES.lock().unwrap_or_else(|e| e.into_inner())).flatten()
        }
    }
    .unwrap_or_else(|| Route::guess(&req.model));
    Wire {
        protocol: if route.anthropic {
            Protocol::Anthropic
        } else {
            Protocol::Chat
        },
        reasoning_back: route.reasoning_back,
        usage_option: true,
        effort: None,
    }
}
