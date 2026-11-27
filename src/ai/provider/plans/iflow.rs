//! iFlow plan (`iflow`): OpenAI chat completions against `apis.iflow.cn`
//! with an iFlow account's OAuth-derived API key.
//!
//! iFlow's API is not plain bearer auth: the client identifies itself as
//! `iFlow-Cli` and signs every request with an HMAC-SHA256 over
//! `User-Agent:session-id:timestamp` whose key is the account's API key
//! (9router's `open-sse/executors/iflow.js` does the same). The generic
//! headers already carry that API key as the bearer — the sign-in stores it
//! as the access token — and the account id `signed_in_token` passes here is
//! the same key, which is what the signature is computed with.
//!
//! Everything else is the generic chat-completions wire: iFlow has no body
//! quirk (the generic builder already asks for usage in the stream, as
//! iFlow's reference does) and no separate generation URL.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::{agent, read_limited, ChatRequest, Turn, Wire, LIST_TIMEOUT};
use crate::ai::Protocol;

/// The identity iFlow's API expects (9router's `transport.headers`).
const USER_AGENT: &str = "iFlow-Cli";
/// The most of a model list RAST reads.
const MODEL_LIST_LIMIT: u64 = 4 * 1024 * 1024;
/// iFlow's models, from 9router's registry, kept when the live list cannot
/// be read.
const MODELS: &[&str] = &[
    "qwen3-coder-plus",
    "qwen3-max",
    "qwen3-vl-plus",
    "qwen3-max-preview",
    "qwen3-235b",
    "qwen3-235b-a22b-instruct",
    "qwen3-235b-a22b-thinking-2507",
    "qwen3-32b",
    "kimi-k2",
    "deepseek-v3.2",
    "deepseek-v3.1",
    "deepseek-v3",
    "deepseek-r1",
    "glm-4.7",
    "iflow-rome-30ba3b",
];

/// The protocol iFlow speaks (chat completions for every model).
pub(crate) fn protocol_for(_model: &str) -> Protocol {
    Protocol::Chat
}

/// The signature iFlow's API wants beside the bearer, plus the CLI identity.
/// `account` is the account's API key (see the module docs).
pub(crate) fn extra_headers(
    _model: &str,
    session: &str,
    account: &str,
) -> Vec<(&'static str, String)> {
    // iFlow's reference mints a fresh `session-<uuid>` per request; the
    // conversation id is just as unique and keeps a chat's requests together.
    let session_id = if session.is_empty() {
        "session-rast".to_string()
    } else {
        format!("session-{session}")
    };
    let timestamp = now_ms();
    let signature = if account.is_empty() {
        String::new()
    } else {
        let payload = format!("{USER_AGENT}:{session_id}:{timestamp}");
        hex(&hmac_sha256(account.as_bytes(), payload.as_bytes()))
    };
    vec![
        ("User-Agent", USER_AGENT.to_string()),
        ("session-id", session_id),
        ("x-iflow-timestamp", timestamp.to_string()),
        ("x-iflow-signature", signature),
    ]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// HMAC-SHA256 (RFC 2104) with `sha2`, which the crate already has: the
/// `hmac` crate would be a new dependency for ten lines.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner);
    Sha256::digest(&outer).into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The generic chat-completions body is what iFlow takes.
pub(crate) fn override_body(_req: &ChatRequest, _wire: Wire, _turns: &[Turn]) -> Option<Value> {
    None
}

pub(crate) fn tweak_body(_req: &ChatRequest, _wire: Wire, _body: &mut Value) {}

/// Where iFlow's model list is (`{base}/models`).
fn catalog_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// iFlow's live model list (blocking).
pub(crate) fn catalog(base_url: &str, token: &str, _account: &str) -> Result<Vec<String>, String> {
    if base_url.is_empty() {
        return Err("set iFlow's base URL first".into());
    }
    let response = agent(LIST_TIMEOUT)
        .get(catalog_url(base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("cannot reach iFlow's model list: {e}"))?;
    let status = response.status().as_u16();
    let body = read_limited(response.into_body(), MODEL_LIST_LIMIT);
    if !(200..300).contains(&status) {
        return Err(refusal(status, &body)
            .unwrap_or_else(|| format!("iFlow's model list was refused (HTTP {status})")));
    }
    super::super::parse_models(&body)
}

pub(crate) fn fallback_models() -> &'static [&'static str] {
    MODELS
}

/// iFlow takes the generic `{base}/chat/completions`.
pub(crate) fn chat_url(_base_url: &str) -> Option<String> {
    None
}

pub(crate) fn refusal(status: u16, _body: &str) -> Option<String> {
    let what = match status {
        401 | 403 => "iFlow refused the sign-in; sign in again under AI Agent settings → Providers",
        429 => "your iFlow plan's limit is used up for now",
        _ => return None,
    };
    Some(format!("{what} (HTTP {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_hmac_sha256() {
        // RFC 4231, test case 2.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (RFC 4231, case 6).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_wire_headers_sign_the_identity() {
        let headers = extra_headers("any-model", "conv-1", "the-api-key");
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("User-Agent"), Some(USER_AGENT));
        assert_eq!(value("session-id"), Some("session-conv-1"));
        let timestamp = value("x-iflow-timestamp").expect("timestamp").to_string();
        assert!(timestamp.parse::<i64>().is_ok());
        // The signature is the HMAC of "iFlow-Cli:<session-id>:<timestamp>"
        // under the account's key.
        let expected = hex(&hmac_sha256(
            b"the-api-key",
            format!("{USER_AGENT}:session-conv-1:{timestamp}").as_bytes(),
        ));
        assert_eq!(value("x-iflow-signature"), Some(expected.as_str()));
        // Without a key there is nothing to sign (the request will be
        // refused, but it is still shaped as iFlow's).
        let unsigned = extra_headers("", "", "");
        assert!(unsigned
            .iter()
            .any(|(name, value)| *name == "x-iflow-signature" && value.is_empty()));
        assert!(unsigned
            .iter()
            .any(|(name, value)| *name == "session-id" && value == "session-rast"));
    }

    #[test]
    fn the_model_list_is_the_generic_one() {
        assert_eq!(
            catalog_url("https://apis.iflow.cn/v1"),
            "https://apis.iflow.cn/v1/models"
        );
        assert_eq!(
            catalog_url("https://apis.iflow.cn/v1/"),
            "https://apis.iflow.cn/v1/models"
        );
        assert!(catalog("", "token", "").is_err());
        assert!(!fallback_models().is_empty());
    }

    #[test]
    fn the_plan_keeps_the_generic_wire() {
        assert_eq!(protocol_for("any-model"), Protocol::Chat);
        assert!(override_body(
            &ChatRequest {
                kind: crate::ai::Kind::IFlow,
                base_url: String::new(),
                api_key: None,
                model: "m".into(),
                system: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                temperature: None,
                max_output_tokens: None,
                effort: None,
                session: String::new(),
                account: String::new(),
            },
            Wire::of(crate::ai::Kind::IFlow),
            &[],
        )
        .is_none());
        assert!(chat_url("https://apis.iflow.cn/v1").is_none());
        assert!(refusal(401, "").is_some_and(|message| message.contains("sign in again")));
        assert!(refusal(429, "").is_some_and(|message| message.contains("limit")));
        assert!(refusal(400, "").is_none());
    }
}
