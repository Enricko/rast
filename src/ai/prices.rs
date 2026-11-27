//! What models cost, in USD per million tokens (the same number as
//! micro-dollars per token).
//!
//! A model's price on a provider comes from, first to last:
//! 1. the price an administrator set for it there (Settings → Prices);
//! 2. the price the provider's own model list gives (OpenCode, OpenRouter
//!    and others that publish one), kept when the models are loaded and
//!    fetched again every day;
//! 3. the built-in list below, unless the provider runs on the server or the
//!    network (Ollama, LM Studio, vLLM, llama.cpp): those models are free.
//!
//! Cached input (read back from the provider's prompt cache) and input
//! written to the cache have their own prices where the provider bills them
//! differently; reasoning counts as output. A price may also cost more for
//! long prompts ([`Tier`]) and at certain times ([`Multiplier`]: OpenCode's
//! peak hours, for example), as the provider's list says.
//!
//! The built-in list is 9router's price table (open-sse/providers/pricing.js,
//! <https://github.com/decolua/9router>, MIT License, Copyright (c) 2024-2026
//! decolua and contributors) with Anthropic's published prices for Claude:
//! exact model names first, then name patterns, first match wins.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::provider::Usage;
use super::Provider;

/// One model's price (USD per million tokens).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawPrice")]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Cached input read back (absent: the input price).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<f64>,
    /// Input written to the cache (absent: the input price).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Other prices for long prompts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<Tier>,
    /// Factors at certain times, all that apply multiplied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub multipliers: Vec<Multiplier>,
}

/// The prices for prompts of more than `above` input tokens (the highest
/// such tier wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tier {
    pub above: i64,
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// A factor on the whole price at certain times, in UTC: on `days` (`mon`
/// … `sun`; none = every day), within `hours` (`["01:00", "04:00"]`, from
/// the first up to the second, across midnight when the second is earlier;
/// none = all day), from the date `from` up to `until` (YYYY-MM-DD).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Multiplier {
    pub factor: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hours: Vec<[String; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// "HH:MM" as minutes after midnight (up to 24:00).
fn minutes(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (m < 60 && h * 60 + m <= 24 * 60).then_some(h * 60 + m)
}

fn valid_date(date: &str) -> bool {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
}

impl Multiplier {
    pub fn applies(&self, at: DateTime<Utc>) -> bool {
        let date = at.format("%Y-%m-%d").to_string();
        if self
            .from
            .as_deref()
            .is_some_and(|from| date.as_str() < from)
            || self
                .until
                .as_deref()
                .is_some_and(|until| date.as_str() >= until)
        {
            return false;
        }
        let day = DAYS[at.weekday().num_days_from_monday() as usize];
        if !self.days.is_empty() && !self.days.iter().any(|d| d.eq_ignore_ascii_case(day)) {
            return false;
        }
        let now = at.hour() * 60 + at.minute();
        self.hours.is_empty()
            || self
                .hours
                .iter()
                .any(|[start, end]| match (minutes(start), minutes(end)) {
                    (Some(s), Some(e)) if s <= e => s <= now && now < e,
                    (Some(s), Some(e)) => now >= s || now < e,
                    _ => false,
                })
    }

    fn valid(&self) -> bool {
        self.factor.is_finite()
            && (0.0..=100.0).contains(&self.factor)
            && self
                .days
                .iter()
                .all(|d| DAYS.contains(&d.to_ascii_lowercase().as_str()))
            && self
                .hours
                .iter()
                .all(|[s, e]| minutes(s).is_some() && minutes(e).is_some())
            && self.from.as_deref().is_none_or(valid_date)
            && self.until.as_deref().is_none_or(valid_date)
    }
}

/// Prices per model.
pub type Prices = BTreeMap<String, Price>;

/// A price as stored or sent: `{input, output, cached?, cacheWrite?}`, or
/// the older `[input, output]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPrice {
    List(Vec<f64>),
    Object(PriceObject),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PriceObject {
    input: f64,
    output: f64,
    #[serde(default)]
    cached: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    #[serde(default)]
    tiers: Vec<Tier>,
    #[serde(default)]
    multipliers: Vec<Multiplier>,
}

impl TryFrom<RawPrice> for Price {
    type Error = String;

    fn try_from(raw: RawPrice) -> Result<Self, String> {
        match raw {
            RawPrice::List(list) => match list.as_slice() {
                [input, output] => Ok(Price::new(*input, *output, None, None)),
                [input, output, cached, write] => {
                    Ok(Price::new(*input, *output, Some(*cached), Some(*write)))
                }
                _ => Err("a price is {input, output, cached, cacheWrite}".into()),
            },
            RawPrice::Object(p) => Ok(Price {
                tiers: p.tiers,
                multipliers: p.multipliers,
                ..Price::new(p.input, p.output, p.cached, p.cache_write)
            }),
        }
    }
}

/// What a model costs when it costs nothing.
pub const FREE: Price = Price::new(0.0, 0.0, Some(0.0), Some(0.0));

impl Price {
    pub const fn new(
        input: f64,
        output: f64,
        cached: Option<f64>,
        cache_write: Option<f64>,
    ) -> Self {
        Self {
            input,
            output,
            cached,
            cache_write,
            tiers: Vec::new(),
            multipliers: Vec::new(),
        }
    }

    /// Every part a number of dollars from 0 to [`super::MAX_PRICE`], at
    /// most 20 tiers and 20 multipliers, each well formed.
    pub fn valid(&self) -> bool {
        let dollars = |parts: [Option<f64>; 4]| {
            parts
                .into_iter()
                .flatten()
                .all(|p| p.is_finite() && (0.0..=super::MAX_PRICE).contains(&p))
        };
        dollars([
            Some(self.input),
            Some(self.output),
            self.cached,
            self.cache_write,
        ]) && self.tiers.len() <= 20
            && self.multipliers.len() <= 20
            && self.tiers.iter().all(|t| {
                t.above >= 0 && dollars([Some(t.input), Some(t.output), t.cached, t.cache_write])
            })
            && self.multipliers.iter().all(Multiplier::valid)
    }

    /// What a reply made at `at` cost, in micro-dollars. `usage.input`
    /// counts all input, cached input included; it also picks the tier.
    pub fn cost_micros(&self, usage: &Usage, at: DateTime<Utc>) -> i64 {
        let input = usage.input.max(0);
        let read = usage.cache_read.clamp(0, input);
        let write = usage.cache_write.clamp(0, input - read);
        let plain = input - read - write;
        let (i, o, c, w) = match self
            .tiers
            .iter()
            .filter(|t| input > t.above)
            .max_by_key(|t| t.above)
        {
            Some(t) => (t.input, t.output, t.cached, t.cache_write),
            None => (self.input, self.output, self.cached, self.cache_write),
        };
        let factor: f64 = self
            .multipliers
            .iter()
            .filter(|m| m.applies(at))
            .map(|m| m.factor)
            .product();
        ((plain as f64 * i
            + read as f64 * c.unwrap_or(i)
            + write as f64 * w.unwrap_or(i)
            + usage.output.max(0) as f64 * o)
            * factor)
            .round() as i64
    }
}

/// OpenCode's price conditions as RAST's: `{"when": {...}, "factor": n}`
/// with `utc_days.in`, `utc_hours` (`gte`/`lt`, alone or in `any` of
/// `all`) and `date` (`gte`/`lt`). Anything else is left out rather than
/// guessed.
pub fn opencode_multiplier(value: &Value) -> Option<Multiplier> {
    let factor = value.get("factor")?.as_f64()?;
    let mut m = Multiplier {
        factor,
        days: Vec::new(),
        hours: Vec::new(),
        from: None,
        until: None,
    };
    for (key, cond) in value.get("when")?.as_object()? {
        match key.as_str() {
            "utc_days" => {
                m.days = cond
                    .get("in")?
                    .as_array()?
                    .iter()
                    .map(|d| d.as_str().map(str::to_ascii_lowercase))
                    .collect::<Option<_>>()?;
            }
            "utc_hours" => m.hours.push(hour_range(&json!({ "utc_hours": cond }))?),
            "date" => {
                let ops = cond.as_object()?;
                if ops.keys().any(|k| k != "gte" && k != "lt") {
                    return None;
                }
                m.from = ops.get("gte").and_then(Value::as_str).map(str::to_string);
                m.until = ops.get("lt").and_then(Value::as_str).map(str::to_string);
            }
            "any" => {
                for alt in cond.as_array()? {
                    m.hours.push(hour_range(alt)?);
                }
            }
            _ => return None,
        }
    }
    m.valid().then_some(m)
}

/// `{"all": [{"utc_hours": {"gte": a}}, {"utc_hours": {"lt": b}}]}` or
/// `{"utc_hours": {"gte": a, "lt": b}}` as `[a, b]`.
fn hour_range(value: &Value) -> Option<[String; 2]> {
    let parts: Vec<&Value> = match value.get("all") {
        Some(all) => all.as_array()?.iter().collect(),
        None => vec![value],
    };
    let (mut start, mut end) = (None, None);
    for part in parts {
        let part = part.as_object()?;
        if part.len() != 1 {
            return None;
        }
        for (op, at) in part.get("utc_hours")?.as_object()? {
            match op.as_str() {
                "gte" => start = Some(at.as_str()?.to_string()),
                "lt" => end = Some(at.as_str()?.to_string()),
                _ => return None,
            }
        }
    }
    Some([
        start.unwrap_or_else(|| "00:00".into()),
        end.unwrap_or_else(|| "24:00".into()),
    ])
}

/// OpenCode's long-prompt prices: `input.overrides` and `output.overrides`
/// of `{"when": {"context": {"gt": n}}, "price": p, "cache": {...}}`.
pub fn opencode_tiers(
    text: &Value,
    base: &Price,
    number: impl Fn(&Value) -> Option<f64>,
) -> Vec<Tier> {
    let above = |o: &Value| -> Option<i64> {
        let context = o.pointer("/when/context")?.as_object()?;
        if o.get("when")?.as_object()?.len() != 1 || context.len() != 1 {
            return None;
        }
        context
            .get("gt")
            .and_then(Value::as_i64)
            .or_else(|| context.get("gte").and_then(Value::as_i64).map(|n| n - 1))
    };
    let overrides = |side: &str| -> Vec<(i64, &Value)> {
        text.pointer(&format!("/{side}/overrides"))
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|o| Some((above(o)?, o))).collect())
            .unwrap_or_default()
    };
    let (inputs, outputs) = (overrides("input"), overrides("output"));
    let mut thresholds: Vec<i64> = inputs.iter().chain(&outputs).map(|(n, _)| *n).collect();
    thresholds.sort_unstable();
    thresholds.dedup();
    thresholds
        .into_iter()
        .map(|n| {
            let input = inputs.iter().find(|(m, _)| *m == n).map(|(_, o)| *o);
            let output = outputs.iter().find(|(m, _)| *m == n).map(|(_, o)| *o);
            let write = input
                .and_then(|o| o.pointer("/cache/write"))
                .and_then(|w| match w {
                    Value::Object(by_ttl) => {
                        by_ttl.values().filter_map(&number).min_by(f64::total_cmp)
                    }
                    other => number(other),
                });
            Tier {
                above: n,
                input: input
                    .and_then(|o| o.get("price"))
                    .and_then(&number)
                    .unwrap_or(base.input),
                output: output
                    .and_then(|o| o.get("price"))
                    .and_then(&number)
                    .unwrap_or(base.output),
                cached: input
                    .and_then(|o| o.pointer("/cache/read"))
                    .and_then(&number)
                    .or(base.cached),
                cache_write: write.or(base.cache_write),
            }
        })
        .collect()
}

/// Where a model's price comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An administrator set it for this provider.
    Set,
    /// The provider's model list.
    List,
    /// The built-in list (the entry that matched: a name or a pattern).
    BuiltIn(&'static str),
    /// The provider runs locally: free.
    Local,
    /// A signed-in plan: paid for by the plan, counted as free.
    Plan,
    /// Nobody knows: counted as free.
    None,
}

impl Source {
    pub fn id(self) -> &'static str {
        match self {
            Source::Set => "set",
            Source::List => "list",
            Source::BuiltIn(_) => "built-in",
            Source::Local => "local",
            Source::Plan => "plan",
            Source::None => "none",
        }
    }
}

/// A model's price on a provider and where it comes from.
pub fn resolve(provider: &Provider, model: &str) -> (Option<Price>, Source) {
    match provider.prices.get(model) {
        Some(price) => (Some(price.clone()), Source::Set),
        None => fallback(provider, model),
    }
}

/// What the model's tokens are worth at API prices, for estimates: like
/// [`resolve`], except that a signed-in plan (counted as free) is priced as
/// the same model would be with a key: the list's price or the built-in one.
pub fn estimate(provider: &Provider, model: &str) -> Option<Price> {
    match resolve(provider, model) {
        (_, Source::Plan) => built_in(model).map(|(price, _)| price),
        (price, _) => price,
    }
}

/// The price a model has when no administrator set one.
pub fn fallback(provider: &Provider, model: &str) -> (Option<Price>, Source) {
    if let Some(price) = provider.list_prices.get(model) {
        return (Some(price.clone()), Source::List);
    }
    if provider.kind.is_local() {
        return (Some(FREE), Source::Local);
    }
    if provider.kind.signs_in() {
        return (Some(FREE), Source::Plan);
    }
    match built_in(model) {
        Some((price, entry)) => (Some(price), Source::BuiltIn(entry)),
        None => (None, Source::None),
    }
}

/// The built-in price for a model id, with the entry that matched. Ids are
/// compared without case, a vendor prefix (`anthropic/…`), a variant
/// (`…:nitro`), a date (`…-20250929`) or an effort (`…-high`); free
/// variants (`…:free`, `…-free`) cost nothing.
pub fn built_in(model: &str) -> Option<(Price, &'static str)> {
    let lower = model.trim().to_ascii_lowercase();
    if lower.ends_with(":free") {
        return Some((FREE, "*:free"));
    }
    if lower.ends_with("-free") {
        return Some((FREE, "*-free"));
    }
    let names = candidates(&lower);
    for name in &names {
        if let Some(row) = MODELS.iter().find(|row| row.0.eq_ignore_ascii_case(name)) {
            return Some((price_of(row), row.0));
        }
    }
    for name in &names {
        if let Some(row) = PATTERNS.iter().find(|row| glob(row.0, name)) {
            return Some((price_of(row), row.0));
        }
    }
    None
}

type Row = (&'static str, f64, f64, f64, f64);

fn price_of(row: &Row) -> Price {
    Price::new(row.1, row.2, Some(row.3), Some(row.4))
}

/// The names to look a model up by, most exact first.
fn candidates(lower: &str) -> Vec<String> {
    let full = lower.split(':').next().unwrap_or(lower);
    let base = full.rsplit('/').next().unwrap_or(full).to_string();
    let mut out = vec![base.clone()];
    if full != base {
        out.push(full.to_string());
    }
    let undated = without_date(&base);
    // Claude written with dots (OpenRouter's claude-sonnet-4.5).
    let undotted = if undated.starts_with("claude-") {
        undated.replace('.', "-")
    } else {
        undated.clone()
    };
    let plain = EFFORTS
        .iter()
        .find_map(|suffix| undotted.strip_suffix(suffix))
        .map(str::to_string);
    for name in [Some(undated), Some(undotted), plain].into_iter().flatten() {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

const EFFORTS: [&str; 8] = [
    "-thinking",
    "-xhigh",
    "-high",
    "-medium",
    "-low",
    "-minimal",
    "-max",
    "-none",
];

/// `…-20250929` or `…-2025-09-29` left out.
fn without_date(name: &str) -> String {
    let bytes = name.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let n = bytes.len();
    if n > 9 && bytes[n - 9] == b'-' && digits(&bytes[n - 8..]) {
        return name[..n - 9].to_string();
    }
    if n > 11
        && bytes[n - 11] == b'-'
        && bytes[n - 6] == b'-'
        && bytes[n - 3] == b'-'
        && digits(&bytes[n - 10..n - 6])
        && digits(&bytes[n - 5..n - 3])
        && digits(&bytes[n - 2..])
    {
        return name[..n - 11].to_string();
    }
    name.to_string()
}

/// `*` matches anything, without case.
fn glob(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// For administrators: every provider's models with their prices, and the
/// built-in list.
pub fn listing(providers: &[Provider]) -> Value {
    let row_json = |key: &str, row: &Row| json!({ key: row.0, "input": row.1, "output": row.2, "cached": row.3, "cacheWrite": row.4 });
    let entry = |source: Source| match source {
        Source::BuiltIn(entry) => Some(entry),
        _ => None,
    };
    json!({
        "providers": providers.iter().map(|p| {
            let models: Vec<Value> = p.models.iter().map(|model| {
                let (price, source) = resolve(p, model);
                let mut item = json!({
                    "model": model,
                    "price": price,
                    "source": source.id(),
                    "match": entry(source),
                });
                if source == Source::Set {
                    let (price, source) = fallback(p, model);
                    item["fallback"] = json!({ "price": price, "source": source.id(), "match": entry(source) });
                }
                item
            }).collect();
            json!({
                "id": p.id,
                "name": p.name,
                "kind": p.kind.as_str(),
                "kindLabel": p.kind.label(),
                "enabled": p.enabled,
                "local": p.kind.is_local(),
                "plan": p.kind.signs_in(),
                "checkedAt": p.prices_checked_at,
                "models": models,
            })
        }).collect::<Vec<_>>(),
        "builtIn": {
            "models": MODELS.iter().map(|row| row_json("name", row)).collect::<Vec<_>>(),
            "patterns": PATTERNS.iter().map(|row| row_json("pattern", row)).collect::<Vec<_>>(),
        },
    })
}

/// The built-in prices by model name: input, output, cached input, input
/// written to the cache (USD per million tokens). From 9router's table
/// (commit 5ae63d2); Claude rows are Anthropic's published prices.
#[rustfmt::skip]
const MODELS: &[(&str, f64, f64, f64, f64)] = &[
    // Anthropic / Claude (Anthropic's published prices)
    ("claude-fable-5-2", 8.0, 40.0, 0.2, 10.0),
    ("claude-mythos-5-2", 8.0, 40.0, 0.2, 10.0),
    ("claude-fable-5-1", 10.0, 50.0, 0.25, 12.5),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25, 12.5),
    ("claude-fable-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-mythos-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-opus-5-5", 4.0, 20.0, 0.2, 5.0),
    ("claude-opus-5", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-8", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-7", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-6", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-5", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-1", 15.0, 75.0, 1.5, 18.75),
    ("claude-opus-4", 15.0, 75.0, 1.5, 18.75),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.1, 2.5),
    ("claude-sonnet-5", 2.0, 10.0, 0.2, 2.5),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75),
    ("claude-sonnet-4-5", 3.0, 15.0, 0.3, 3.75),
    ("claude-sonnet-4", 3.0, 15.0, 0.3, 3.75),
    ("claude-haiku-5-5", 0.1, 0.5, 0.01, 0.125),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1, 1.25),
    // OpenAI / GPT
    ("gpt-3.5-turbo", 0.5, 1.5, 0.25, 0.5),
    ("gpt-4", 2.5, 10.0, 1.25, 2.5),
    ("gpt-4-turbo", 10.0, 30.0, 5.0, 10.0),
    ("gpt-4o", 2.5, 10.0, 1.25, 2.5),
    ("gpt-4o-mini", 0.15, 0.6, 0.075, 0.15),
    ("gpt-4.1", 2.5, 10.0, 1.25, 2.5),
    ("gpt-5", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5-mini", 0.25, 2.0, 0.125, 0.25),
    ("gpt-5-codex", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5.1", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5.1-codex", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5.1-codex-mini", 1.5, 6.0, 0.75, 1.5),
    ("gpt-5.1-codex-mini-high", 2.0, 8.0, 1.0, 2.0),
    ("gpt-5.1-codex-max", 8.0, 32.0, 4.0, 8.0),
    ("gpt-5.2", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.2-codex", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.3-codex", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.3-codex-spark", 3.0, 12.0, 0.3, 3.0),
    ("gpt-5.6", 2.5, 15.0, 0.25, 2.5),
    ("gpt-5.6-luna", 1.0, 6.0, 0.1, 1.0),
    ("gpt-5.6-terra", 2.5, 15.0, 0.25, 2.5),
    ("gpt-5.6-sol", 5.0, 30.0, 0.5, 5.0),
    ("gpt-6-astra", 10.0, 50.0, 1.0, 12.5),
    ("gpt-6.1-sol", 2.0, 10.0, 0.1, 2.5),
    ("gpt-6.1-terra", 1.0, 5.0, 0.05, 1.25),
    ("gpt-6-sol", 2.0, 10.0, 0.2, 2.5),
    ("gpt-6-luna", 0.1, 0.5, 0.01, 0.125),
    ("o1", 15.0, 60.0, 7.5, 15.0),
    ("o1-mini", 3.0, 12.0, 1.5, 3.0),
    // Gemini
    ("gemini-3.8-flash", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.8-flash-high", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.8-flash-medium", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.8-flash-low", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.7-flash", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.7-flash-high", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.7-flash-medium", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.7-flash-low", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.6-flash", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.6-flash-high", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.6-flash-medium", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.6-flash-low", 1.5, 7.5, 0.15, 1.875),
    ("gemini-3.5-flash-lite", 0.3, 2.5, 0.03, 0.375),
    ("gemini-3.5-flash-high", 0.5, 3.0, 0.03, 0.5),
    ("gemini-3-flash-preview", 0.5, 3.0, 0.03, 0.5),
    ("gemini-3-pro-preview", 2.0, 12.0, 0.25, 2.0),
    ("gemini-3.1-pro-low", 2.0, 12.0, 0.25, 2.0),
    ("gemini-3.1-pro-high", 4.0, 18.0, 0.5, 4.0),
    ("gemini-pro-agent", 4.0, 18.0, 0.5, 4.0),
    ("gemini-3-flash-agent", 0.5, 3.0, 0.03, 0.5),
    ("gemini-3.5-flash-low", 0.5, 3.0, 0.03, 0.5),
    ("gemini-3.5-flash-extra-low", 0.5, 3.0, 0.03, 0.5),
    ("gemini-3-flash", 0.5, 3.0, 0.03, 0.5),
    ("gemini-2.5-pro", 2.0, 12.0, 0.25, 2.0),
    ("gemini-2.5-flash", 0.3, 2.5, 0.03, 0.3),
    ("gemini-2.5-flash-lite", 0.15, 1.25, 0.015, 0.15),
    // Qwen
    ("qwen3-coder-plus", 1.0, 4.0, 0.5, 1.0),
    ("qwen3-coder-flash", 0.5, 2.0, 0.25, 0.5),
    // Kimi
    ("kimi-k3", 3.0, 15.0, 0.3, 3.0),
    ("k3", 3.0, 15.0, 0.3, 3.0),
    ("kimi-k3.1-pro", 2.0, 10.0, 0.2, 2.0),
    ("kimi-k2.7-code", 0.95, 4.0, 0.19, 0.95),
    ("kimi-k2.7-code-highspeed", 1.9, 8.0, 0.38, 1.9),
    ("kimi-for-coding", 0.95, 4.0, 0.19, 0.95),
    ("kimi-for-coding-highspeed", 1.9, 8.0, 0.38, 1.9),
    ("kimi-k2", 1.0, 4.0, 0.5, 1.0),
    ("kimi-k2-thinking", 1.5, 6.0, 0.75, 1.5),
    ("kimi-k2.5", 1.2, 4.8, 0.6, 1.2),
    ("kimi-k2.5-thinking", 1.8, 7.2, 0.9, 1.8),
    ("kimi-k2.6", 1.0, 4.0, 0.5, 1.0),
    ("kimi-latest", 1.0, 4.0, 0.5, 1.0),
    // DeepSeek
    ("deepseek-chat", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-reasoner", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-r1", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v3.2-chat", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v3.2-reasoner", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v4-flash", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v4.1-flash", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-flash", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v4-pro", 0.435, 0.87, 0.003625, 0.435),
    // GLM
    ("glm-4.6", 0.5, 2.0, 0.25, 0.5),
    ("glm-4.6v", 0.75, 3.0, 0.375, 0.75),
    ("glm-4.7", 0.75, 3.0, 0.375, 0.75),
    ("glm-5", 1.0, 4.0, 0.5, 1.0),
    // MiniMax
    ("MiniMax-M3", 0.3, 1.2, 0.06, 0.3),
    ("MiniMax-M2.1", 0.5, 2.0, 0.25, 0.5),
    ("MiniMax-M2.5", 0.5, 2.0, 0.25, 0.5),
    ("MiniMax-M2.7", 0.5, 2.0, 0.25, 0.5),
    // Grok
    ("grok-code-fast-1", 0.5, 2.0, 0.25, 0.5),
    // Muse (Meta Model API)
    ("muse-spark-1.3", 1.25, 4.25, 0.15, 0.0),
    ("muse-spark-1.2", 1.25, 4.25, 0.15, 0.0),
    ("muse-spark-1.1", 1.25, 4.25, 0.15, 0.0),
    ("muse-spark-1.3-contributor", 0.1, 0.2, 0.002, 0.0),
    ("muse-spark-1.2-contributor", 0.1, 0.2, 0.002, 0.0),
    // OpenRouter fallback
    ("auto", 2.0, 8.0, 1.0, 2.0),
    // Misc
    ("oswe-vscode-prime", 1.0, 4.0, 0.5, 1.0),
    ("gpt-oss-120b-medium", 0.5, 2.0, 0.25, 0.5),
    ("vision-model", 1.5, 6.0, 0.75, 1.5),
    ("coder-model", 1.5, 6.0, 0.75, 1.5),
];

/// Name patterns (`*` matches anything), first match wins.
#[rustfmt::skip]
const PATTERNS: &[(&str, f64, f64, f64, f64)] = &[
    // Codex variants
    ("*-codex-xhigh", 10.0, 40.0, 5.0, 10.0),
    ("*-codex-high", 8.0, 32.0, 4.0, 8.0),
    ("*-codex-max", 8.0, 32.0, 4.0, 8.0),
    ("*-codex-mini-*", 1.5, 6.0, 0.75, 1.5),
    ("*-codex-mini", 1.5, 6.0, 0.75, 1.5),
    ("*-codex-low", 1.75, 14.0, 0.175, 1.75),
    ("*-codex-none", 1.75, 14.0, 0.175, 1.75),
    ("*-codex-spark", 3.0, 12.0, 0.3, 3.0),
    ("codex-*", 1.75, 14.0, 0.175, 1.75),
    ("*-codex", 1.75, 14.0, 0.175, 1.75),
    // Claude
    ("claude-opus-*", 5.0, 25.0, 0.5, 6.25),
    ("claude-sonnet-*", 3.0, 15.0, 0.3, 3.75),
    ("claude-haiku-*", 1.0, 5.0, 0.1, 1.25),
    ("claude-*", 3.0, 15.0, 0.3, 3.75),
    // Gemini (specific first, generic last)
    ("gemini-*-flash-lite", 0.15, 1.25, 0.015, 0.15),
    ("gemini-*-flash", 0.3, 2.5, 0.03, 0.3),
    ("gemini-*-pro", 2.0, 12.0, 0.25, 2.0),
    ("gemini-4-hydrogen*", 1.5, 7.5, 0.075, 1.5),
    ("gemini-4-*", 2.0, 10.0, 0.1, 0.1875),
    ("gemini-3-*", 0.5, 3.0, 0.03, 0.5),
    ("gemini-2.5-*", 0.3, 2.5, 0.03, 0.3),
    ("gemini-*", 0.5, 3.0, 0.03, 0.5),
    // GPT (specific first, generic last)
    ("gpt-5.6-*", 2.5, 15.0, 0.25, 2.5),
    ("gpt-5.3-*", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.2-*", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.1-*", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5-*", 1.25, 10.0, 0.625, 1.25),
    ("gpt-5*", 1.25, 10.0, 0.625, 1.25),
    ("gpt-4o-*", 0.15, 0.6, 0.075, 0.15),
    ("gpt-4o", 2.5, 10.0, 1.25, 2.5),
    ("gpt-4*", 2.5, 10.0, 1.25, 2.5),
    // o1 / o-series
    ("o1-*", 3.0, 12.0, 1.5, 3.0),
    ("o1", 15.0, 60.0, 7.5, 15.0),
    ("o3-*", 10.0, 40.0, 5.0, 10.0),
    ("o4-*", 2.0, 8.0, 1.0, 2.0),
    // Qwen
    ("qwen3-coder-*", 1.0, 4.0, 0.5, 1.0),
    ("qwen*-coder-*", 1.0, 4.0, 0.5, 1.0),
    ("qwen*", 0.5, 2.0, 0.25, 0.5),
    // Kimi
    ("kimi-*-thinking", 1.8, 7.2, 0.9, 1.8),
    ("kimi-k3*", 3.0, 15.0, 0.3, 3.0),
    ("kimi-k2*", 1.2, 4.8, 0.6, 1.2),
    ("kimi-*", 1.0, 4.0, 0.5, 1.0),
    // DeepSeek
    ("deepseek-*reasoner*", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-r*", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-v*", 0.14, 0.28, 0.0028, 0.14),
    ("deepseek-*", 0.14, 0.28, 0.0028, 0.14),
    // GLM
    ("glm-5*", 1.0, 4.0, 0.5, 1.0),
    ("glm-4*", 0.75, 3.0, 0.375, 0.75),
    ("glm-*", 0.5, 2.0, 0.25, 0.5),
    // MiniMax
    ("MiniMax-*", 0.5, 2.0, 0.25, 0.5),
    ("minimax-*", 0.5, 2.0, 0.25, 0.5),
    // Grok
    ("grok-code-*", 0.5, 2.0, 0.25, 0.5),
    ("grok-*", 0.5, 2.0, 0.25, 0.5),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn prices_read_both_ways() {
        let prices: Prices = serde_json::from_str(
            r#"{"a": [1, 2], "b": {"input": 3, "output": 15, "cached": 0.3, "cacheWrite": 3.75}, "c": [1, 2, 0.1, 1.25]}"#,
        )
        .unwrap();
        assert_eq!(prices["a"], Price::new(1.0, 2.0, None, None));
        assert_eq!(prices["b"], Price::new(3.0, 15.0, Some(0.3), Some(3.75)));
        assert_eq!(prices["c"], Price::new(1.0, 2.0, Some(0.1), Some(1.25)));
        assert_eq!(
            serde_json::to_value(&prices["b"]).unwrap(),
            json!({"input": 3.0, "output": 15.0, "cached": 0.3, "cacheWrite": 3.75})
        );
        assert_eq!(
            serde_json::to_value(&prices["a"]).unwrap(),
            json!({"input": 1.0, "output": 2.0})
        );
        assert!(serde_json::from_str::<Price>("[1]").is_err());
        assert!(
            serde_json::from_str::<Price>(r#"{"input": 1, "output": 2, "reasoning": 3}"#).is_err()
        );
        assert!(!Price::new(-1.0, 1.0, None, None).valid());
        assert!(!Price::new(1.0, 1.0, Some(f64::NAN), None).valid());
        assert!(Price::new(0.0, super::super::MAX_PRICE, Some(0.0), None).valid());
    }

    fn provider_of(kind: super::super::Kind) -> super::super::Provider {
        super::super::Provider {
            id: "p".into(),
            kind,
            name: "P".into(),
            base_url: String::new(),
            api_key: String::new(),
            models: vec![],
            prices: Prices::new(),
            list_prices: Prices::new(),
            prices_checked_at: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
            owner_id: None,
            sign_in: String::new(),
        }
    }

    #[test]
    fn plans_count_nothing_but_are_estimated_at_api_prices() {
        let mut plan = provider_of(super::super::Kind::Codex);
        assert_eq!(resolve(&plan, "gpt-5").0, Some(FREE));
        assert_eq!(
            estimate(&plan, "gpt-5"),
            Some(Price::new(1.25, 10.0, Some(0.625), Some(1.25)))
        );
        assert_eq!(estimate(&plan, "unknown-model-x"), None);
        // A price the administrator set is both what counts and the estimate.
        plan.prices
            .insert("gpt-5".into(), Price::new(2.0, 4.0, None, None));
        assert_eq!(
            estimate(&plan, "gpt-5"),
            Some(Price::new(2.0, 4.0, None, None))
        );
        let mut keyed = provider_of(super::super::Kind::OpenAi);
        keyed
            .list_prices
            .insert("m".into(), Price::new(1.0, 1.0, None, None));
        assert_eq!(estimate(&keyed, "m"), resolve(&keyed, "m").0);
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn cached_input_costs_its_own_price() {
        let now = Utc::now();
        let price = Price::new(3.0, 15.0, Some(0.3), Some(3.75));
        // 1000 input of which 600 read back and 100 written: 300 plain.
        assert_eq!(
            price.cost_micros(&usage(1000, 200, 600, 100), now),
            900 + 180 + 375 + 3000
        );
        // Without cache prices, cached input costs the input price.
        assert_eq!(
            Price::new(3.0, 15.0, None, None).cost_micros(&usage(1000, 200, 600, 100), now),
            6000
        );
        // A provider reporting more cached than all input is not believed.
        assert_eq!(price.cost_micros(&usage(100, 0, 500, 500), now), 30);
        assert_eq!(FREE.cost_micros(&usage(1000, 1000, 0, 0), now), 0);
    }

    #[test]
    fn peak_hours_and_long_prompts_cost_more() {
        // OpenCode's DeepSeek: twice the price on weekdays 01:00-04:00 and
        // 06:00-10:00 UTC; and a price list from December on.
        let cost: Value = json!({
            "multipliers": [
                {"when": {"utc_days": {"in": ["mon", "tue", "wed", "thu", "fri"]}, "any": [
                    {"all": [{"utc_hours": {"gte": "01:00"}}, {"utc_hours": {"lt": "04:00"}}]},
                    {"all": [{"utc_hours": {"gte": "06:00"}}, {"utc_hours": {"lt": "10:00"}}]}
                ]}, "factor": 2},
                {"when": {"date": {"gte": "2026-12-01"}}, "factor": 3},
                {"when": {"region": "eu"}, "factor": 9}
            ]
        });
        let multipliers: Vec<Multiplier> = cost["multipliers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(opencode_multiplier)
            .collect();
        assert_eq!(multipliers.len(), 2, "an unknown condition is left out");
        assert_eq!(
            multipliers[0].hours,
            vec![
                ["01:00".to_string(), "04:00".to_string()],
                ["06:00".to_string(), "10:00".to_string()]
            ]
        );
        let price = Price {
            multipliers,
            ..Price::new(1.0, 2.0, None, None)
        };
        let one = usage(1000, 1000, 0, 0);
        // Friday 2026-11-27.
        assert_eq!(price.cost_micros(&one, at("2026-11-27T02:30:00Z")), 6000);
        assert_eq!(price.cost_micros(&one, at("2026-11-27T04:00:00Z")), 3000);
        assert_eq!(price.cost_micros(&one, at("2026-11-27T09:59:00Z")), 6000);
        // Saturday: no peak.
        assert_eq!(price.cost_micros(&one, at("2026-11-28T02:30:00Z")), 3000);
        // From December 1st (a Tuesday) both apply.
        assert_eq!(price.cost_micros(&one, at("2026-12-01T02:30:00Z")), 18000);
        assert!(Multiplier {
            hours: vec![["22:00".into(), "02:00".into()]],
            ..multiplier(1.5)
        }
        .applies(at("2026-11-28T01:00:00Z")));
        assert!(!Multiplier {
            hours: vec![["22:00".into(), "02:00".into()]],
            ..multiplier(1.5)
        }
        .applies(at("2026-11-28T03:00:00Z")));
        assert!(!Multiplier {
            days: vec!["funday".into()],
            ..multiplier(2.0)
        }
        .valid());
        assert!(!Multiplier {
            hours: vec![["1:00".into(), "04:00".into()]],
            ..multiplier(2.0)
        }
        .valid());

        // Long prompts: above 200K tokens of input, other prices.
        let text = json!({
            "input": {"price": 2, "cache": {"read": 0.5}, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 4, "cache": {"read": 1}}]},
            "output": {"price": 6, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 12}]}
        });
        let base = Price::new(2.0, 6.0, Some(0.5), None);
        let tiers = opencode_tiers(&text, &base, |v| v.as_f64());
        assert_eq!(
            tiers,
            vec![Tier {
                above: 200_000,
                input: 4.0,
                output: 12.0,
                cached: Some(1.0),
                cache_write: None
            }]
        );
        let price = Price { tiers, ..base };
        let now = at("2026-11-28T12:00:00Z");
        assert_eq!(
            price.cost_micros(&usage(200_000, 100, 0, 0), now),
            400_000 + 600
        );
        assert_eq!(
            price.cost_micros(&usage(200_001, 100, 1, 0), now),
            800_000 + 1 + 1200
        );
        assert!(price.valid());
        let back: Price = serde_json::from_value(serde_json::to_value(&price).unwrap()).unwrap();
        assert_eq!(back, price);
    }

    fn multiplier(factor: f64) -> Multiplier {
        Multiplier {
            factor,
            days: vec![],
            hours: vec![],
            from: None,
            until: None,
        }
    }

    #[test]
    fn the_built_in_list_finds_models_as_providers_name_them() {
        let found = |model: &str| built_in(model).map(|(p, entry)| (p.input, p.output, entry));
        assert_eq!(
            found("claude-opus-5-5"),
            Some((4.0, 20.0, "claude-opus-5-5"))
        );
        assert_eq!(
            found("anthropic/claude-sonnet-4.5"),
            Some((3.0, 15.0, "claude-sonnet-4-5"))
        );
        assert_eq!(
            found("claude-haiku-4-5-20251001"),
            Some((1.0, 5.0, "claude-haiku-4-5"))
        );
        assert_eq!(
            found("Claude-Fable-5-2"),
            Some((8.0, 40.0, "claude-fable-5-2"))
        );
        assert_eq!(found("claude-opus-9"), Some((5.0, 25.0, "claude-opus-*")));
        assert_eq!(
            found("deepseek/deepseek-chat"),
            Some((0.14, 0.28, "deepseek-chat"))
        );
        assert_eq!(found("gpt-4o-2024-08-06"), Some((2.5, 10.0, "gpt-4o")));
        assert_eq!(found("minimax-m2.5"), Some((0.5, 2.0, "MiniMax-M2.5")));
        assert_eq!(
            found("gemini-2.5-pro-high"),
            Some((2.0, 12.0, "gemini-2.5-pro"))
        );
        assert_eq!(
            found("qwen/qwen3-coder-480b:nitro"),
            Some((1.0, 4.0, "qwen3-coder-*"))
        );
        assert_eq!(found("glm-5.3"), Some((1.0, 4.0, "glm-5*")));
        assert_eq!(
            found("meta-llama/llama-3.3-70b-instruct:free"),
            Some((0.0, 0.0, "*:free"))
        );
        assert_eq!(found("big-pickle-free"), Some((0.0, 0.0, "*-free")));
        assert_eq!(found("llama3.2"), None);
        let cached = built_in("claude-sonnet-5-5").unwrap().0;
        assert_eq!((cached.cached, cached.cache_write), (Some(0.1), Some(2.5)));
    }

    #[test]
    fn the_built_in_list_has_each_name_once() {
        let mut seen = std::collections::HashSet::new();
        for row in MODELS {
            assert!(
                seen.insert(row.0.to_ascii_lowercase()),
                "{} listed twice",
                row.0
            );
            assert!(price_of(row).valid(), "{}", row.0);
        }
        for row in PATTERNS {
            assert!(price_of(row).valid(), "{}", row.0);
        }
        assert!(MODELS.len() > 100 && PATTERNS.len() > 50);
    }

    #[test]
    fn globs_match_like_9router() {
        assert!(glob("claude-*", "claude-x"));
        assert!(glob("*-codex", "gpt-5-codex"));
        assert!(glob("qwen*-coder-*", "qwen3-coder-plus"));
        assert!(glob("MiniMax-*", "minimax-m3"));
        assert!(!glob("gpt-4o-*", "gpt-4o"));
        assert!(!glob("a*b*c", "abc-"));
        assert!(glob("a*b*c", "a-b-c"));
        assert!(!glob("ab*ba", "aba"));
    }
}
