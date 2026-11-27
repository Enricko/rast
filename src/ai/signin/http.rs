//! The small blocking HTTP client every sign-in shares: `ureq`, no redirects
//! (an OAuth endpoint must answer where it was asked; following a redirect
//! would hide the provider's refusal behind a page), a short timeout so a
//! stuck provider does not tie up the sign-in, and JSON both ways.
//!
//! Nothing here is async: the API layer runs these on
//! `tokio::task::spawn_blocking`. The provider modules word their own
//! refusals; this module only says when the provider could not be reached.

use std::{io::Read, time::Duration};

use serde_json::Value;

/// How long any one request may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The most of a response RAST reads (a token answer is tiny; a wrong
/// address may answer with a whole page).
const MAX_BODY: u64 = 256 * 1024;

/// A reply RAST could read: the HTTP status and the JSON body (`null` when
/// the body was not JSON).
pub(crate) struct Reply {
    pub status: u16,
    pub value: Value,
}

impl Reply {
    /// The provider's own explanation, for the caller's error copy: OAuth's
    /// `error_description`, Anthropic's `error.message`, or a bare `error`
    /// ("" when it said nothing useful).
    pub(crate) fn said(&self) -> &str {
        self.value
            .get("error_description")
            .or_else(|| self.value.pointer("/error/message"))
            .or_else(|| self.value.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// The agent every request uses. Plan modules with an unusual request can
/// build it themselves from this agent.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("RAST/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn reply(response: ureq::http::Response<ureq::Body>) -> Reply {
    let status = response.status().as_u16();
    let mut text = String::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(MAX_BODY)
        .read_to_string(&mut text);
    Reply {
        status,
        value: serde_json::from_str(&text).unwrap_or(Value::Null),
    }
}

/// `application/x-www-form-urlencoded` (also a query string).
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    fn encode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A URL-decoded query value (also the pasted-address parser's).
pub(crate) fn decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            ) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// POST a form (token exchanges, device-code requests) and read the JSON
/// answer. `site` names who could not be reached in the transport error
/// ("OpenAI's sign-in").
pub(crate) fn post_form(
    url: &str,
    site: &str,
    headers: &[(&str, &str)],
    pairs: &[(&str, &str)],
) -> Result<Reply, String> {
    let mut request = agent()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send(form(pairs).as_bytes())
        .map_err(|e| format!("cannot reach {site}: {e}"))?;
    Ok(reply(response))
}

/// POST a JSON body (Claude's token endpoint takes JSON) and read the JSON
/// answer.
pub(crate) fn post_json(
    url: &str,
    site: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> Result<Reply, String> {
    let mut request = agent()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body =
        serde_json::to_vec(body).map_err(|_| format!("cannot build the request to {site}"))?;
    let response = request
        .send(body)
        .map_err(|e| format!("cannot reach {site}: {e}"))?;
    Ok(reply(response))
}

/// GET a JSON document with extra headers (profiles, model catalogs).
pub(crate) fn get_json(url: &str, site: &str, headers: &[(&str, &str)]) -> Result<Reply, String> {
    let mut request = agent().get(url).header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .call()
        .map_err(|e| format!("cannot reach {site}: {e}"))?;
    Ok(reply(response))
}
