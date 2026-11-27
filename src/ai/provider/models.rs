//! Provider model lists: the catalogs each kind serves, the prices a list
//! may carry, and ChatGPT's Codex list (which has a shape of its own).

use serde_json::Value;

use super::gateway::{parse_gateway_models, remember_routes, ROUTES_FOR};
use super::{agent, headers, read_limited, refusal, LIST_TIMEOUT};
use crate::ai::prices::{self, Price, Prices};
use crate::ai::{Kind, Protocol};

/// A provider's models and the prices its list gives for them.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelList {
    pub models: Vec<String>,
    pub prices: Prices,
}

/// Where the model list is.
pub(crate) fn models_url(kind: Kind, base_url: &str) -> String {
    if kind == Kind::Codex {
        format!(
            "{base_url}/models?client_version={}",
            super::super::signin::CODEX_VERSION
        )
    } else if kind.routes_by_model() {
        format!("{base_url}/v1/models")
    } else if kind.protocol() == Protocol::Anthropic {
        format!("{base_url}/models?limit=1000")
    } else {
        format!("{base_url}/models")
    }
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
        .filter(|id| !id.is_empty() && id.len() <= super::super::MAX_MODEL_NAME)
        .collect();
    ids.sort();
    ids.dedup();
    ids.truncate(super::super::MAX_MODELS);
    Ok(ids)
}

/// The models a provider offers (`GET {base}/models`), sorted. Gateways
/// leave out what RAST cannot use: models that are not for chat, and free
/// ones only their own apps may call.
pub fn list_models(kind: Kind, base_url: &str, api_key: Option<&str>) -> Result<ModelList, String> {
    if base_url.is_empty() {
        return Err("set the provider's base URL first".into());
    }
    let body = fetch_models(kind, base_url, api_key)?;
    let models = if kind.routes_by_model() {
        let models = parse_gateway_models(&body)?;
        remember_routes(base_url, &models, ROUTES_FOR);
        let mut ids: Vec<String> = models
            .into_iter()
            .filter(|m| m.usable)
            .map(|m| m.id)
            .collect();
        ids.sort();
        ids.truncate(super::super::MAX_MODELS);
        ids
    } else {
        parse_models(&body)?
    };
    let mut prices = parse_prices(&body);
    prices.retain(|id, _| models.contains(id));
    Ok(ModelList { models, prices })
}

/// What a model list says its models cost, where it says so: OpenCode's
/// `cost.text` (USD per million tokens: `input.price`, `input.cache.read`,
/// `input.cache.write`, `output.price`) and OpenRouter's or Vercel's
/// `pricing` (USD per token, as text: `prompt` or `input`, `completion` or
/// `output`, `input_cache_read`, `input_cache_write`).
pub fn parse_prices(body: &str) -> Prices {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Prices::new();
    };
    let Some(list) = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
    else {
        return Prices::new();
    };
    let number = |v: Option<&Value>| -> Option<f64> {
        let v = v?;
        v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
    };
    list.iter()
        .filter_map(|m| {
            let id = m
                .get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)?;
            let round = |p: f64| (p * 1e6).round() / 1e6;
            let price = if let Some(text) = m.pointer("/cost/text") {
                // A write price per cache lifetime ({"5m": …, "1h": …}): the shortest.
                let write = text.pointer("/input/cache/write").and_then(|w| match w {
                    Value::Object(by_ttl) => by_ttl
                        .values()
                        .filter_map(|v| number(Some(v)))
                        .min_by(f64::total_cmp),
                    other => number(Some(other)),
                });
                let base = Price::new(
                    number(text.pointer("/input/price"))?,
                    number(text.pointer("/output/price"))?,
                    number(text.pointer("/input/cache/read")),
                    write,
                );
                Price {
                    tiers: prices::opencode_tiers(text, &base, |v| number(Some(v))),
                    multipliers: m
                        .pointer("/cost/multipliers")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(prices::opencode_multiplier)
                                .collect()
                        })
                        .unwrap_or_default(),
                    ..base
                }
            } else {
                let pricing = m.get("pricing")?;
                let per_token = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| number(pricing.get(*k)))
                        .map(|p| p * 1e6)
                };
                Price::new(
                    per_token(&["prompt", "input"])?,
                    per_token(&["completion", "output"])?,
                    per_token(&["input_cache_read"]),
                    per_token(&["input_cache_write"]),
                )
            };
            let price = Price {
                tiers: price
                    .tiers
                    .iter()
                    .map(|t| prices::Tier {
                        input: round(t.input),
                        output: round(t.output),
                        cached: t.cached.map(round),
                        cache_write: t.cache_write.map(round),
                        ..t.clone()
                    })
                    .collect(),
                multipliers: price.multipliers.clone(),
                ..Price::new(
                    round(price.input),
                    round(price.output),
                    price.cached.map(round),
                    price.cache_write.map(round),
                )
            };
            // Negative: "it depends" (OpenRouter's automatic routing).
            price.valid().then(|| (id.to_string(), price))
        })
        .collect()
}

pub(crate) fn fetch_models(
    kind: Kind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<String, String> {
    let mut request = agent(LIST_TIMEOUT).get(models_url(kind, base_url));
    for (name, value) in headers(kind, kind.protocol(), api_key, "") {
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
    Ok(body)
}
