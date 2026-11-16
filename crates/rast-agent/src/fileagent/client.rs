//! The server side of a file agent: one HTTP/1.1 request per connection to
//! an agent socket, with the request body and the response streamed through.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, Method, Request, Response, StatusCode};
use serde_json::Value;

use crate::error::{AppError, AppResult};

use super::{QUOTA_HEADER, TOKEN_HEADER};

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

/// Rebuild an agent's error for the caller of a background job.
pub fn error_from(agent: &str, status: StatusCode, body: &Value) -> AppError {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("the {agent} failed"));
    match status {
        StatusCode::CONFLICT => AppError::Conflict(message),
        StatusCode::SERVICE_UNAVAILABLE => AppError::Unavailable(message),
        status if status.is_server_error() => AppError::Internal(message),
        _ => AppError::BadRequest(message),
    }
}
