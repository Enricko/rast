//! What providers say about an account's limits: the windows they report
//! with each answer (ChatGPT's 5-hour and weekly use, Claude's, the
//! requests and tokens left in OpenAI-style and Anthropic API headers), and
//! whether a refusal means the account reached a limit, with when it resets
//! when they say so.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One limit window as the provider reported it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// "5 hours", "Weekly", "Requests", "Tokens", …
    pub name: String,
    /// Share used, 0–100 (when the provider gives a share).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Counts (when the provider gives counts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// When the window starts again (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// The windows one answer reported, and when.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    #[serde(default)]
    pub windows: Vec<Window>,
    #[serde(default)]
    pub checked_at: i64,
}

impl Quota {
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

/// The account reached a limit: rest it (until `until` when known).
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// When it resets (ms since the epoch), when the provider said.
    pub until: Option<i64>,
    /// A short reason for the account list ("Usage limit reached", …).
    pub reason: String,
}

/// Header names (lowercase) and values of an answer.
pub type Headers = Vec<(String, String)>;

pub fn headers_of(map: &ureq::http::HeaderMap) -> Headers {
    map.iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect()
}

fn header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn number(headers: &Headers, name: &str) -> Option<f64> {
    header(headers, name).and_then(|v| v.parse::<f64>().ok())
}

/// The windows an answer's headers report (none: the provider reports none).
pub fn windows(headers: &Headers, now_ms: i64) -> Vec<Window> {
    let mut found = Vec::new();
    // ChatGPT plans (Codex): primary (5 hours) and secondary (weekly).
    for which in ["primary", "secondary"] {
        let used = number(headers, &format!("x-codex-{which}-used-percent"));
        if used.is_none() {
            continue;
        }
        let minutes = number(headers, &format!("x-codex-{which}-window-minutes"));
        let resets_at = number(headers, &format!("x-codex-{which}-reset-at"))
            .map(|secs| (secs * 1000.0) as i64)
            .or_else(|| {
                number(headers, &format!("x-codex-{which}-reset-after-seconds"))
                    .map(|secs| now_ms + (secs * 1000.0) as i64)
            });
        found.push(Window {
            name: minutes.map_or_else(
                || {
                    if which == "primary" {
                        "5 hours"
                    } else {
                        "Weekly"
                    }
                    .to_string()
                },
                |m| window_name(m as i64),
            ),
            used_percent: used.map(clamp_percent),
            resets_at,
            ..Window::default()
        });
    }
    // Claude plans: unified windows (5h, 7d, …) as a share used.
    let mut unified: Vec<&str> = headers
        .iter()
        .filter_map(|(n, _)| {
            n.strip_prefix("anthropic-ratelimit-unified-")
                .and_then(|rest| rest.strip_suffix("-utilization"))
        })
        .collect();
    unified.sort_unstable();
    for key in unified {
        let share = number(
            headers,
            &format!("anthropic-ratelimit-unified-{key}-utilization"),
        );
        let resets_at = number(headers, &format!("anthropic-ratelimit-unified-{key}-reset"))
            .map(|secs| (secs * 1000.0) as i64);
        found.push(Window {
            name: unified_name(key),
            // A share (0–1); some answers give a percentage.
            used_percent: share.map(|s| clamp_percent(if s <= 1.0 { s * 100.0 } else { s })),
            resets_at,
            ..Window::default()
        });
    }
    // Anthropic API keys: requests and tokens left, reset as a time.
    for what in ["requests", "tokens", "input-tokens", "output-tokens"] {
        let limit = number(headers, &format!("anthropic-ratelimit-{what}-limit"));
        let remaining = number(headers, &format!("anthropic-ratelimit-{what}-remaining"));
        if let (Some(limit), Some(remaining)) = (limit, remaining) {
            let resets_at =
                header(headers, &format!("anthropic-ratelimit-{what}-reset")).and_then(parse_time);
            found.push(counted(what, limit, remaining, resets_at));
        }
    }
    // OpenAI-style: x-ratelimit-{limit,remaining,reset}-<what>.
    let mut kinds: Vec<&str> = headers
        .iter()
        .filter_map(|(n, _)| n.strip_prefix("x-ratelimit-limit-"))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    for what in kinds {
        let limit = number(headers, &format!("x-ratelimit-limit-{what}"));
        let remaining = number(headers, &format!("x-ratelimit-remaining-{what}"));
        if let (Some(limit), Some(remaining)) = (limit, remaining) {
            let resets_at = header(headers, &format!("x-ratelimit-reset-{what}")).and_then(|v| {
                parse_duration(v)
                    .map(|ms| now_ms + ms)
                    .or_else(|| parse_time(v))
            });
            found.push(counted(what, limit, remaining, resets_at));
        }
    }
    found
}

fn counted(what: &str, limit: f64, remaining: f64, resets_at: Option<i64>) -> Window {
    let limit = limit.max(0.0) as i64;
    let used = (limit - remaining.max(0.0) as i64).max(0);
    Window {
        name: count_name(what),
        used_percent: (limit > 0).then(|| clamp_percent(used as f64 * 100.0 / limit as f64)),
        used: Some(used),
        limit: Some(limit),
        resets_at,
    }
}

fn clamp_percent(value: f64) -> f64 {
    (value.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

fn count_name(what: &str) -> String {
    let mut name = what.replace('-', " ");
    if let Some(first) = name.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    name
}

fn window_name(minutes: i64) -> String {
    match minutes {
        10_080 => "Weekly".into(),
        1_440 => "Daily".into(),
        m if m > 0 && m % 1_440 == 0 => format!("{} days", m / 1_440),
        m if m > 0 && m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{m} minutes"),
    }
}

fn unified_name(key: &str) -> String {
    match key {
        "5h" => "5 hours".into(),
        "7d" => "Weekly".into(),
        other => {
            let (period, model) = other.split_once('_').unwrap_or((other, ""));
            let period = match period {
                "5h" => "5 hours".to_string(),
                "7d" => "Weekly".to_string(),
                p => p.to_string(),
            };
            if model.is_empty() {
                period
            } else {
                format!("{period} ({model})")
            }
        }
    }
}

/// Whether a refusal means the account reached a limit, and until when.
pub fn limit(status: u16, headers: &Headers, body: &str, now_ms: i64) -> Option<Limit> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    let lower = body.to_ascii_lowercase();
    let quota_words = [
        "resource_exhausted",
        "insufficient_quota",
        "usage_limit_reached",
        "quota exceeded",
        "exceeded your current quota",
        "rate limit",
        "rate_limit",
    ];
    let limited = match status {
        429 | 402 => true,
        400 | 403 => quota_words.iter().any(|w| lower.contains(w)),
        _ => false,
    };
    if !limited {
        return None;
    }
    let until = reset_from_headers(headers, now_ms)
        .or_else(|| json.as_ref().and_then(|j| reset_from_json(j, now_ms)))
        .or_else(|| reset_from_text(body, now_ms))
        .filter(|&t| t > now_ms);
    let reason = if status == 402 {
        "Out of credits".to_string()
    } else if lower.contains("usage_limit_reached")
        || lower.contains("resource_exhausted")
        || lower.contains("insufficient_quota")
        || lower.contains("quota")
    {
        "Usage limit reached".to_string()
    } else {
        "Rate limited".to_string()
    };
    Some(Limit { until, reason })
}

fn reset_from_headers(headers: &Headers, now_ms: i64) -> Option<i64> {
    if let Some(value) = header(headers, "retry-after") {
        if let Ok(secs) = value.parse::<f64>() {
            return Some(now_ms + (secs * 1000.0) as i64);
        }
        if let Some(at) = parse_http_date(value) {
            return Some(at);
        }
    }
    // The latest reset among the windows that ran out.
    windows(headers, now_ms)
        .into_iter()
        .filter(|w| w.used_percent.is_some_and(|p| p >= 100.0))
        .filter_map(|w| w.resets_at)
        .max()
}

/// Reset times in an error body: `resets_in_seconds` / `resets_at` (ChatGPT),
/// `retryDelay` / `quotaResetDelay` / `quotaResetTimeStamp` (Google).
fn reset_from_json(value: &Value, now_ms: i64) -> Option<i64> {
    let mut best: Option<i64> = None;
    let mut keep = |at: i64| best = Some(best.map_or(at, |b| b.max(at)));
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(map) => {
                for (key, item) in map {
                    match (key.as_str(), item) {
                        ("resets_in_seconds" | "reset_after_seconds", Value::Number(n)) => {
                            if let Some(secs) = n.as_f64() {
                                keep(now_ms + (secs * 1000.0) as i64);
                            }
                        }
                        ("resets_at", Value::Number(n)) => {
                            if let Some(secs) = n.as_f64() {
                                // Seconds (or already milliseconds).
                                keep(if secs > 1e12 {
                                    secs as i64
                                } else {
                                    (secs * 1000.0) as i64
                                });
                            }
                        }
                        ("resets_at" | "quotaResetTimeStamp", Value::String(s)) => {
                            if let Some(at) = parse_time(s) {
                                keep(at);
                            }
                        }
                        ("retryDelay" | "quotaResetDelay", Value::String(s)) => {
                            if let Some(ms) = parse_duration(s) {
                                keep(now_ms + ms);
                            }
                        }
                        _ => stack.push(item),
                    }
                }
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    best
}

/// "… reset after 3h12m4s", "Please retry in 41.2s", "resets in 2h".
fn reset_from_text(text: &str, now_ms: i64) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    for marker in [
        "reset after ",
        "resets after ",
        "retry in ",
        "resets in ",
        "try again in ",
    ] {
        if let Some(at) = lower.find(marker) {
            let rest = &lower[at + marker.len()..];
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
                .collect();
            // The sentence's full stop is not part of the duration.
            if let Some(ms) = parse_duration(token.trim_end_matches('.')) {
                return Some(now_ms + ms);
            }
        }
    }
    None
}

/// Go-style durations: "1s", "6m0s", "20ms", "3h12m4.5s", "1234s".
pub fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut total = 0f64;
    let mut number = String::new();
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        let unit = match c {
            'h' => 3_600_000.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                1.0
            }
            'm' => 60_000.0,
            's' => 1_000.0,
            'd' => 86_400_000.0,
            _ => return None,
        };
        total += value * unit;
        any = true;
    }
    if !number.is_empty() {
        // A bare number: seconds.
        total += number.parse::<f64>().ok()? * 1000.0;
        any = true;
    }
    any.then_some(total as i64)
}

/// An RFC 3339 time, or unix seconds, as ms since the epoch.
fn parse_time(text: &str) -> Option<i64> {
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text.trim()) {
        return Some(at.timestamp_millis());
    }
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|secs| *secs > 1e9)
        .map(|secs| {
            if secs > 1e12 {
                secs as i64
            } else {
                (secs * 1000.0) as i64
            }
        })
}

fn parse_http_date(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(text.trim())
        .ok()
        .map(|at| at.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn durations_read_like_go() {
        assert_eq!(parse_duration("1s"), Some(1_000));
        assert_eq!(parse_duration("6m0s"), Some(360_000));
        assert_eq!(parse_duration("20ms"), Some(20));
        assert_eq!(parse_duration("3h12m4.5s"), Some(11_524_500));
        assert_eq!(parse_duration("41.2s"), Some(41_200));
        assert_eq!(parse_duration("90"), Some(90_000));
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn chatgpt_plans_report_five_hours_and_a_week() {
        let found = windows(
            &h(&[
                ("x-codex-primary-used-percent", "42"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-after-seconds", "3600"),
                ("x-codex-secondary-used-percent", "7.5"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-at", "1800100000"),
            ]),
            NOW,
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "5 hours");
        assert_eq!(found[0].used_percent, Some(42.0));
        assert_eq!(found[0].resets_at, Some(NOW + 3_600_000));
        assert_eq!(found[1].name, "Weekly");
        assert_eq!(found[1].used_percent, Some(7.5));
        assert_eq!(found[1].resets_at, Some(1_800_100_000_000));
    }

    #[test]
    fn claude_plans_and_api_keys_report_their_limits() {
        let found = windows(
            &h(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.25"),
                ("anthropic-ratelimit-unified-5h-reset", "1800003600"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.5"),
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "40"),
                ("anthropic-ratelimit-requests-reset", "2027-01-15T10:00:00Z"),
            ]),
            NOW,
        );
        let names: Vec<&str> = found.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["5 hours", "Weekly", "Requests"]);
        assert_eq!(found[0].used_percent, Some(25.0));
        assert_eq!(found[0].resets_at, Some(1_800_003_600_000));
        assert_eq!(found[2].used, Some(10));
        assert_eq!(found[2].limit, Some(50));
        assert_eq!(found[2].used_percent, Some(20.0));
        assert!(found[2].resets_at.is_some());
    }

    #[test]
    fn openai_style_headers_count_requests_and_tokens() {
        let found = windows(
            &h(&[
                ("x-ratelimit-limit-requests", "500"),
                ("x-ratelimit-remaining-requests", "499"),
                ("x-ratelimit-reset-requests", "120ms"),
                ("x-ratelimit-limit-tokens", "30000"),
                ("x-ratelimit-remaining-tokens", "15000"),
                ("x-ratelimit-reset-tokens", "6m0s"),
            ]),
            NOW,
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Requests");
        assert_eq!(found[0].used, Some(1));
        assert_eq!(found[1].name, "Tokens");
        assert_eq!(found[1].used_percent, Some(50.0));
        assert_eq!(found[1].resets_at, Some(NOW + 360_000));
        assert!(windows(&h(&[("content-type", "text/event-stream")]), NOW).is_empty());
    }

    #[test]
    fn limit_refusals_say_when_the_account_is_back() {
        // ChatGPT plan used up.
        let codex = limit(
            429,
            &h(&[]),
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":7200}}"#,
            NOW,
        )
        .unwrap();
        assert_eq!(codex.until, Some(NOW + 7_200_000));
        assert_eq!(codex.reason, "Usage limit reached");
        // Google: RESOURCE_EXHAUSTED with a retry delay in the details.
        let google = limit(
            429,
            &h(&[]),
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"You have exhausted your capacity. Your quota will reset after 3h12m.","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"11520s"}]}}"#,
            NOW,
        )
        .unwrap();
        assert_eq!(google.until, Some(NOW + 11_520_000));
        // Only the message text.
        let text = limit(429, &h(&[]), "Your quota will reset after 2h30m.", NOW).unwrap();
        assert_eq!(text.until, Some(NOW + 9_000_000));
        // Retry-After in seconds; out of credits.
        let paid = limit(402, &h(&[("retry-after", "30")]), "{}", NOW).unwrap();
        assert_eq!(paid.until, Some(NOW + 30_000));
        assert_eq!(paid.reason, "Out of credits");
        // Unknown reset: the caller picks a short rest.
        assert_eq!(limit(429, &h(&[]), "slow down", NOW).unwrap().until, None);
        // Other refusals are not limits.
        assert!(limit(400, &h(&[]), r#"{"error":"bad model"}"#, NOW).is_none());
        assert!(limit(401, &h(&[]), "unauthorized", NOW).is_none());
        assert!(limit(403, &h(&[]), "Quota exceeded for this project", NOW).is_some());
    }
}
