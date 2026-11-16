//! The server side of a file agent: one HTTP/1.1 request per connection to
//! an agent socket, with the request body and the response streamed through.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, Method, Request, Response, StatusCode};
use serde_json::Value;

use crate::error::{AppError, AppResult};

use super::{QUOTA_HEADER, SHARES_HEADER, TOKEN_HEADER, TOKEN_REFUSED};

/// Largest JSON reply read completely from an agent.
pub const MAX_JSON_REPLY: usize = 4 * 1024 * 1024;

/// Send one request over a connected agent socket and stream the response
/// back. Only the headers an agent needs are passed on. `agent` names the
/// agent in error messages ("root file agent", "file agent").
pub async fn send(
    agent: &str,
    stream: tokio::net::UnixStream,
    token: &str,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Body,
) -> AppResult<Response<Body>> {
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|error| agent_error(agent, error))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path_and_query)
        .header(header::HOST, "rootfs")
        .header(TOKEN_HEADER, token);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::RANGE,
        HeaderName::from_static(QUOTA_HEADER),
        HeaderName::from_static(SHARES_HEADER),
    ] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value);
        }
    }
    let request = request
        .body(body)
        .map_err(|error| AppError::internal(format!("cannot build agent request: {error}")))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| agent_error(agent, error))?;
    let (mut parts, incoming) = response.into_parts();
    for name in [header::CONNECTION, header::TRANSFER_ENCODING] {
        parts.headers.remove(name);
    }
    parts.headers.remove(HeaderName::from_static("keep-alive"));
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

fn agent_error(agent: &str, error: hyper::Error) -> AppError {
    AppError::unavailable(format!("the {agent} did not answer: {error}"))
}

/// Read a (bounded) JSON body from an agent response.
pub async fn read_json(agent: &str, response: Response<Body>) -> AppResult<Value> {
    let bytes = axum::body::to_bytes(response.into_body(), MAX_JSON_REPLY)
        .await
        .map_err(|error| AppError::unavailable(format!("{agent} reply: {error}")))?;
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Longest message kept from an agent's error.
pub const MAX_ERROR_MESSAGE: usize = 300;

/// The messages agents send with the statuses whose errors carry a static
/// message. Any other text gets the status's generic message: an agent runs
/// as its user, so its text is untrusted (ADR 0005).
const FORBIDDEN: &[&str] = &[
    "symbolic links are not supported",
    "path escapes the permitted storage area",
    "file changed while it was being opened",
    "this shared folder is not set up correctly",
    "write permission required",
    "permission denied",
    "invalid agent token",
];
const NOT_FOUND: &[&str] = &[
    "path not found",
    "file not found",
    "upload not found",
    "shared folder not found",
];
const TOO_LARGE: &[&str] = &[
    "storage quota exceeded",
    "file exceeds the maximum upload size",
    "chunk is too large",
    "chunk exceeds the declared upload size",
    "upload is larger than its declared size",
    "request body is too large",
];
const UNSUPPORTED: &[&str] = &["preview is not supported for this file type"];
const TOO_MANY: &[&str] = &["too many concurrent uploads, finish or cancel one first"];

fn known(message: &str, known: &[&'static str], fallback: &'static str) -> &'static str {
    known
        .iter()
        .copied()
        .find(|candidate| *candidate == message)
        .unwrap_or(fallback)
}

/// An agent's message, without control characters and at most
/// [`MAX_ERROR_MESSAGE`] characters long.
fn bounded(message: &str) -> String {
    let mut out: String = message
        .chars()
        .take(MAX_ERROR_MESSAGE)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if message.chars().nth(MAX_ERROR_MESSAGE).is_some() {
        out.push('…');
    }
    out
}

/// The message of an agent's JSON error reply, bounded.
pub fn error_message(body: &Value) -> Option<String> {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .map(bounded)
}

/// Whether an agent's JSON reply says that it refused the token (its
/// socket unit was restarted with a new one).
pub fn token_refused(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::FORBIDDEN
        && body.pointer("/error/message").and_then(Value::as_str) == Some(TOKEN_REFUSED)
}

/// Rebuild an agent's error for the caller of a background job, with the
/// agent's status. A status the server would act on differently (such as
/// 401, which signs the browser out) is never passed on.
pub fn error_from(agent: &str, status: StatusCode, body: &Value) -> AppError {
    let message = error_message(body).unwrap_or_else(|| format!("the {agent} failed"));
    match status {
        StatusCode::FORBIDDEN => AppError::Forbidden(known(&message, FORBIDDEN, "access denied")),
        StatusCode::NOT_FOUND => AppError::NotFound(known(&message, NOT_FOUND, "path not found")),
        StatusCode::PAYLOAD_TOO_LARGE => {
            AppError::PayloadTooLarge(known(&message, TOO_LARGE, "the request is too large"))
        }
        StatusCode::UNSUPPORTED_MEDIA_TYPE => AppError::UnsupportedMediaType(known(
            &message,
            UNSUPPORTED,
            "this file type is not supported",
        )),
        StatusCode::TOO_MANY_REQUESTS => AppError::TooManyRequests {
            retry_after_secs: 60,
            message: known(&message, TOO_MANY, "too many requests, try again later"),
        },
        StatusCode::CONFLICT => AppError::Conflict(message),
        StatusCode::SERVICE_UNAVAILABLE => AppError::Unavailable(message),
        status if status.is_server_error() => AppError::Internal(message),
        _ => AppError::BadRequest(message),
    }
}

/// The status of a streamed agent reply as it goes to the browser: success,
/// client and server errors pass, except those the browser would act on
/// (401 and 407 sign out or ask for credentials); anything else, such as a
/// redirect or an informational status, becomes 502.
pub fn passed_status(status: StatusCode) -> StatusCode {
    let acted_on = matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );
    if (status.is_success() || status.is_client_error() || status.is_server_error()) && !acted_on {
        status
    } else {
        StatusCode::BAD_GATEWAY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error(message: &str) -> Value {
        json!({"error": {"code": "x", "message": message}})
    }

    #[test]
    fn agent_errors_keep_their_status_with_known_messages_only() {
        let cases = [
            (StatusCode::FORBIDDEN, "symbolic links are not supported"),
            (StatusCode::NOT_FOUND, "path not found"),
            (StatusCode::PAYLOAD_TOO_LARGE, "storage quota exceeded"),
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "preview is not supported for this file type",
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "too many concurrent uploads, finish or cancel one first",
            ),
        ];
        for (status, message) in cases {
            let rebuilt = error_from("file agent", status, &error(message));
            assert_eq!(rebuilt.status(), status, "{message}");
            assert_eq!(rebuilt.public_message(), message);
            // Other text from the agent is not shown with such a status.
            let planted = error_from("file agent", status, &error("<script>"));
            assert_eq!(planted.status(), status);
            assert!(!planted.public_message().contains("script"), "{planted:?}");
        }
        let conflict = error_from(
            "file agent",
            StatusCode::CONFLICT,
            &error("\"a.txt\" already exists"),
        );
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(conflict.public_message(), "\"a.txt\" already exists");
        // 401 would sign the browser out: never passed on.
        let unauthorized = error_from("file agent", StatusCode::UNAUTHORIZED, &error("x"));
        assert_eq!(unauthorized.status(), StatusCode::BAD_REQUEST);
        let unknown = error_from("file agent", StatusCode::BAD_GATEWAY, &json!(null));
        assert_eq!(unknown.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(unknown.public_message(), "the file agent failed");
    }

    #[test]
    fn agent_messages_are_bounded() {
        let long = "x".repeat(10_000);
        let rebuilt = error_from("file agent", StatusCode::CONFLICT, &error(&long));
        assert_eq!(
            rebuilt.public_message().chars().count(),
            MAX_ERROR_MESSAGE + 1
        );
        let message = error_message(&error("a\nb\u{1b}[31mc")).unwrap();
        assert_eq!(message, "a b [31mc");
        assert_eq!(
            error_message(&error(&"é".repeat(MAX_ERROR_MESSAGE))).unwrap(),
            "é".repeat(MAX_ERROR_MESSAGE)
        );
    }

    #[test]
    fn streamed_replies_never_pass_statuses_the_browser_acts_on() {
        for status in [200, 206, 400, 403, 404, 409, 413, 416, 429, 500, 503] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(passed_status(status), status);
        }
        for status in [101, 301, 302, 304, 401, 407] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(passed_status(status), StatusCode::BAD_GATEWAY, "{status}");
        }
    }
}
