//! `/linux-apps/<key>/…`: a running Linux app's page, served from its
//! container through RAST.
//!
//! The window shows this page in a sandboxed frame **without** the
//! same-origin permission, and every response also carries a CSP `sandbox`
//! directive, so the page runs with an opaque origin even when opened on its
//! own: it cannot read RAST's cookies, storage or pages. The unguessable key
//! (one per start of a copy) is the page's only credential; the container's
//! own password never leaves the server.
//!
//! Requests are forwarded as HTTP/1.0 (`Connection: close`) with a fixed
//! list of request headers, so cookies and RAST's credentials never reach the
//! app. HTML pages get a small script ([`SHIM`]) that gives the opaque page
//! the in-memory storage and same-window messaging the streaming client
//! expects. The WebSocket carrying the picture and the input is relayed
//! message by message, which also tells RAST when someone last typed or
//! moved the mouse.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{
        ws::{Message as ClientMessage, WebSocket, WebSocketUpgrade},
        FromRequestParts, Request, State,
    },
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as AppMessage};

use super::{LinuxApps, Session};
use crate::{docker::linux::URL_PREFIX, state::AppState};

/// Injected at the start of every HTML page of an app.
pub const SHIM: &str = include_str!("shim.js");
/// What an app's page may do.
pub const CONTENT_SECURITY_POLICY: &str = "sandbox allow-scripts allow-forms allow-pointer-lock \
allow-downloads allow-modals; frame-ancestors 'self'";

const MAX_REQUEST_BODY: usize = 1024 * 1024;
const MAX_RESPONSE_HEAD: usize = 64 * 1024;
const MAX_HTML: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Request headers passed on to the app (everything else, notably
/// `Cookie` and `Authorization`, stays with RAST).
const REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "cache-control",
    "content-type",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "range",
    "user-agent",
];

/// Response headers passed back from the app.
const RESPONSE_HEADERS: &[&str] = &[
    "accept-ranges",
    "cache-control",
    "content-encoding",
    "content-language",
    "content-length",
    "content-range",
    "content-type",
    "etag",
    "expires",
    "last-modified",
    "vary",
];

/// Messages of the streaming client that are a person's input (keyboard,
/// mouse, scrolling, touch, gamepad, clipboard, composition, resize).
const INPUT_MESSAGES: &[&str] = &[
    "m", "m2", "kd", "ku", "kr", "kh", "js", "co", "sf", "sfe", "cw", "p", "vp", "r",
];

/// Is this streaming-client message somebody using the app?
pub fn is_input(message: &str) -> bool {
    let kind = message.split([',', ' ']).next().unwrap_or_default();
    INPUT_MESSAGES.contains(&kind)
}

fn valid_key(key: &str) -> bool {
    key.len() == 32
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Handler for every method under `/linux-apps/`.
pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path();
    let Some(rest) = path.strip_prefix(URL_PREFIX) else {
        return closed_page(StatusCode::NOT_FOUND);
    };
    let (key, has_slash) = match rest.split_once('/') {
        Some((key, _)) => (key, true),
        None => (rest, false),
    };
    if !valid_key(key) {
        return closed_page(StatusCode::NOT_FOUND);
    }
    if !has_slash {
        let location = format!("{URL_PREFIX}{key}/");
        return with_policy(
            (
                StatusCode::PERMANENT_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
        );
    }
    let Some(session) = state.linux_apps.find_by_key(key) else {
        return closed_page(StatusCode::NOT_FOUND);
    };
    let Some((port, authorization)) = session.upstream() else {
        return closed_page(StatusCode::NOT_FOUND);
    };
    let authorization = authorization.to_string();
    if is_websocket(request.headers()) {
        websocket(
            request,
            session,
            port,
            authorization,
            state.linux_apps.clone(),
        )
        .await
    } else {
        forward(request, port, &authorization).await
    }
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Security headers every response from an app carries.
fn with_policy(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // The sandboxed page has an opaque origin; its module scripts and fetches
    // are cross-origin requests without credentials.
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

fn closed_page(status: StatusCode) -> Response {
    let body = "<!doctype html><meta charset=utf-8><title>App closed</title>\
<body style=\"font:14px system-ui;color:#666;display:grid;place-items:center;height:90vh\">\
This app is closed. Open it again from RAST.</body>";
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    with_policy(response)
}

fn bad_gateway(message: &'static str) -> Response {
    with_policy((StatusCode::BAD_GATEWAY, message).into_response())
}

async fn forward(request: Request, port: u16, authorization: &str) -> Response {
    let (parts, body) = request.into_parts();
    if !matches!(
        parts.method,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
    ) {
        return with_policy(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(body) => body,
        Err(_) => return with_policy(StatusCode::PAYLOAD_TOO_LARGE.into_response()),
    };
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut head = format!(
        "{} {target} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {authorization}\r\nConnection: close\r\n",
        parts.method
    );
    for name in REQUEST_HEADERS {
        for value in parts.headers.get_all(*name) {
            if let Ok(value) = value.to_str() {
                if !value.contains(['\r', '\n']) {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
            }
        }
    }
    if !body.is_empty() || matches!(parts.method, Method::POST | Method::PUT | Method::PATCH) {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    let mut stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return bad_gateway("the app is not answering"),
    };
    if stream.write_all(head.as_bytes()).await.is_err() || stream.write_all(&body).await.is_err() {
        return bad_gateway("the app is not answering");
    }
    let (status, headers, leftover) =
        match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
            Ok(Some(parsed)) => parsed,
            _ => return bad_gateway("the app sent an unreadable answer"),
        };

    let mut response_headers = HeaderMap::new();
    let mut content_type = String::new();
    for (name, value) in &headers {
        let lower = name.to_ascii_lowercase();
        if !RESPONSE_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if lower == "content-type" {
            content_type = value.to_ascii_lowercase();
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(lower.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            response_headers.append(name, value);
        }
    }
    let html = content_type.starts_with("text/html") && parts.method != Method::HEAD;
    let body = if html {
        let mut page = leftover;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    if page.len() + n > MAX_HTML {
                        return bad_gateway("the app's page is too large");
                    }
                    page.extend_from_slice(&chunk[..n]);
                }
                Err(_) => return bad_gateway("the app's page was cut off"),
            }
        }
        let page = inject_shim(&String::from_utf8_lossy(&page));
        response_headers.remove(header::CONTENT_LENGTH);
        response_headers.remove(header::CONTENT_ENCODING);
        response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Body::from(page)
    } else {
        let first = futures::stream::once(async move {
            Ok::<_, std::io::Error>(axum::body::Bytes::from(leftover))
        });
        Body::from_stream(first.chain(tokio_util::io::ReaderStream::new(stream)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    with_policy(response)
}

/// Read and parse an HTTP response head; returns the bytes after it.
async fn read_head(stream: &mut TcpStream) -> Option<(StatusCode, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&buffer) {
            Ok(httparse::Status::Complete(length)) => {
                let status = StatusCode::from_u16(response.code?).ok()?;
                let headers = response
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect();
                return Some((status, headers, buffer[length..].to_vec()));
            }
            Ok(httparse::Status::Partial) if buffer.len() < MAX_RESPONSE_HEAD => continue,
            _ => return None,
        }
    }
}

/// Put [`SHIM`] at the start of the page's `<head>`.
pub fn inject_shim(page: &str) -> String {
    let script = format!("<script>{SHIM}</script>");
    let lower = page.to_ascii_lowercase();
    let at = lower
        .find("<head")
        .and_then(|start| lower[start..].find('>').map(|end| start + end + 1));
    match at {
        Some(at) => format!("{}{script}{}", &page[..at], &page[at..]),
        None => format!("{script}{page}"),
    }
}

async fn websocket(
    request: Request,
    session: Arc<Session>,
    port: u16,
    authorization: String,
    apps: Arc<LinuxApps>,
) -> Response {
    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return with_policy(rejection.into_response()),
    };
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let Ok(mut upstream_request) = format!("ws://127.0.0.1:{port}{target}").into_client_request()
    else {
        return bad_gateway("invalid app address");
    };
    let Ok(value) = HeaderValue::from_str(&authorization) else {
        return bad_gateway("invalid app credentials");
    };
    upstream_request
        .headers_mut()
        .insert(header::AUTHORIZATION, value);
    if let Some(protocols) = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        upstream_request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, protocols.clone());
    }
    let connected = async {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        tokio_tungstenite::client_async(upstream_request, stream)
            .await
            .ok()
    };
    let Ok(Some((upstream, answer))) =
        tokio::time::timeout(Duration::from_secs(10), connected).await
    else {
        return bad_gateway("the app is not answering");
    };
    let upgrade = match answer
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        Some(protocol) => upgrade.protocols([protocol.to_string()]),
        None => upgrade,
    };
    upgrade
        .max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| relay(socket, upstream, session, apps))
}

async fn relay(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<TcpStream>,
    session: Arc<Session>,
    apps: Arc<LinuxApps>,
) {
    session.viewer_joined();
    let (mut client_tx, mut client_rx) = client.split();
    let (mut app_tx, mut app_rx) = upstream.split();
    let watcher = session.clone();
    let to_app = async move {
        while let Some(Ok(message)) = client_rx.next().await {
            let message = match message {
                ClientMessage::Text(text) => {
                    if is_input(text.as_str()) {
                        watcher.input();
                    }
                    AppMessage::text(text.as_str().to_owned())
                }
                ClientMessage::Binary(bytes) => {
                    watcher.input();
                    AppMessage::Binary(bytes)
                }
                ClientMessage::Ping(bytes) => AppMessage::Ping(bytes),
                ClientMessage::Pong(bytes) => AppMessage::Pong(bytes),
                ClientMessage::Close(_) => break,
            };
            if app_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = app_tx.close().await;
    };
    let to_client = async move {
        while let Some(Ok(message)) = app_rx.next().await {
            let message = match message {
                AppMessage::Text(text) => ClientMessage::Text(text.as_str().to_owned().into()),
                AppMessage::Binary(bytes) => ClientMessage::Binary(bytes),
                AppMessage::Ping(bytes) => ClientMessage::Ping(bytes),
                AppMessage::Pong(bytes) => ClientMessage::Pong(bytes),
                AppMessage::Close(_) => break,
                AppMessage::Frame(_) => continue,
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };
    tokio::select! {
        _ = to_app => {}
        _ = to_client => {}
    }
    session.viewer_left();
    // When the app quits from inside (its own close button) the picture just
    // disappears; ask the worker shortly after whether the app is still there
    // so the RAST window can close instead of staying blank.
    apps.watch_exit(session);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_messages_are_recognised() {
        for input in [
            "m,10,20,0,0,0",
            "kd,65",
            "ku,65",
            "sf,1.00,2.00",
            "co,end,abc",
            "js,c,0,1",
        ] {
            assert!(is_input(input), "{input}");
        }
        for other in [
            "CLIENT_FRAME_ACK 12 34",
            "_f,60",
            "_stats,{}",
            "SETTINGS,{}",
            "pong",
            "START_VIDEO",
        ] {
            assert!(!is_input(other), "{other}");
        }
    }

    #[test]
    fn the_shim_goes_into_the_head() {
        let page =
            inject_shim("<!doctype html><HTML><Head lang=\"en\"><title>x</title></head></html>");
        let at = page.find("<script>").unwrap();
        assert!(page[..at].ends_with("<Head lang=\"en\">"));
        assert!(page.contains("localStorage"));
        assert!(inject_shim("plain").starts_with("<script>"));
    }

    #[test]
    fn keys_are_hex_of_fixed_length() {
        assert!(valid_key(&"a1".repeat(16)));
        assert!(!valid_key("short"));
        assert!(!valid_key(&"A1".repeat(16)));
        assert!(!valid_key(&format!("{}/", "a".repeat(31))));
    }

    #[test]
    fn every_response_is_sandboxed() {
        let response = closed_page(StatusCode::NOT_FOUND);
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.starts_with("sandbox "));
        assert!(!csp.contains("allow-same-origin"));
        assert!(csp.contains("frame-ancestors 'self'"));
    }
}
