//! The root file system in File Station (super administrators only).
//!
//! Logical paths under [`PREFIX`] name the server's own file system:
//! `/@root` is `/`, `/@root/etc/hosts` is `/etc/hosts`. The hardened service
//! cannot read those files, so they are served by the **root file agent**
//! ([`agent`]): a root process in its own transient unit, started on demand
//! by the privileged helper (`rootfs_start`) and speaking the File Station
//! API over a Unix socket that only the RAST service can reach. The API
//! layer ([`crate::api::rootfs`]) checks the super administrator role,
//! audits, and forwards.
//!
//! There is no quota and no recycle bin here: a delete is permanent.

pub mod agent;

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, Method, Request, Response, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::native::client::NativeClient;

/// Logical prefix of the root file system.
pub const PREFIX: &str = "/@root";
/// The reserved first path segment (no home or shared folder may use it).
pub use crate::storage::safe_path::RESERVED_NAME;
/// Prefix of chunked upload ids that belong to the agent.
pub const UPLOAD_PREFIX: &str = "sys-";
/// Header carrying the agent token.
pub const TOKEN_HEADER: &str = "x-rast-rootfs-token";

/// Whether a logical path names the root file system.
pub fn is_root_path(path: &str) -> bool {
    path == PREFIX
        || path
            .strip_prefix(PREFIX)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The real path behind a root file system logical path.
pub fn physical(logical: &str) -> AppResult<PathBuf> {
    if !is_root_path(logical) || logical.contains('\0') {
        return Err(AppError::bad_request("invalid path"));
    }
    let mut out = PathBuf::from("/");
    for segment in logical[PREFIX.len()..].split('/') {
        match segment {
            "" => {}
            "." | ".." => return Err(AppError::bad_request("path traversal is not allowed")),
            other => out.push(other),
        }
    }
    Ok(out)
}

/// The logical path of a real path.
pub fn logical(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text == "/" {
        PREFIX.to_string()
    } else {
        format!("{PREFIX}{text}")
    }
}

/// Connection to the root file agent, started through the helper on first
/// use and again whenever it has exited (it stops itself when idle).
pub struct RootFs {
    enabled: bool,
    native: NativeClient,
    agent: Mutex<Option<(PathBuf, String)>>,
}

impl RootFs {
    pub fn new(enabled: bool, native: NativeClient) -> Self {
        Self {
            enabled,
            native,
            agent: Mutex::new(None),
        }
    }

    /// A client for an agent that is already listening (tests).
    #[cfg(test)]
    fn with_agent(native: NativeClient, socket: PathBuf, token: &str) -> Self {
        Self {
            enabled: true,
            native,
            agent: Mutex::new(Some((socket, token.to_string()))),
        }
    }

    /// `files_root_enabled` and a platform that has the helper.
    pub fn enabled(&self) -> bool {
        self.enabled && cfg!(target_os = "linux")
    }

    async fn start(&self) -> AppResult<(PathBuf, String)> {
        let reply = self
            .native
            .call_json("rootfs_start", Value::Object(Default::default()))
            .await
            .map_err(|error| {
                AppError::unavailable(format!("the root file system is unavailable: {error}"))
            })?;
        let field = |name: &str| reply.get(name).and_then(Value::as_str).map(str::to_string);
        match (field("socket"), field("token")) {
            (Some(socket), Some(token)) => Ok((PathBuf::from(socket), token)),
            _ => Err(AppError::unavailable(
                "the root file system is unavailable: unexpected helper reply",
            )),
        }
    }

    async fn connect(&self) -> AppResult<(tokio::net::UnixStream, String)> {
        let mut agent = self.agent.lock().await;
        if let Some((socket, token)) = agent.as_ref() {
            if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
                return Ok((stream, token.clone()));
            }
        }
        *agent = None;
        let (socket, token) = self.start().await?;
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|error| {
                AppError::unavailable(format!("cannot reach the root file agent: {error}"))
            })?;
        *agent = Some((socket, token.clone()));
        Ok((stream, token))
    }

    /// Send one request to the agent and stream its response back.
    pub async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> AppResult<Response<Body>> {
        if !self.enabled() {
            return Err(AppError::Forbidden(
                "the root file system is turned off on this server",
            ));
        }
        let (stream, token) = self.connect().await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(agent_error)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request = Request::builder()
            .method(method)
            .uri(path_and_query)
            .header(header::HOST, "rootfs")
            .header(TOKEN_HEADER, token);
        for name in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::RANGE] {
            if let Some(value) = headers.get(&name) {
                request = request.header(name, value);
            }
        }
        let request = request
            .body(body)
            .map_err(|error| AppError::internal(format!("cannot build agent request: {error}")))?;
        let response = sender.send_request(request).await.map_err(agent_error)?;
        let (mut parts, incoming) = response.into_parts();
        for name in [header::CONNECTION, header::TRANSFER_ENCODING] {
            parts.headers.remove(name);
        }
        parts.headers.remove(HeaderName::from_static("keep-alive"));
        Ok(Response::from_parts(parts, Body::new(incoming)))
    }

    /// A JSON request whose JSON reply is read completely (small bodies).
    pub async fn call(&self, path: &str, body: &Value) -> AppResult<Value> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        let response = self
            .send(Method::POST, path, &headers, Body::from(body.to_string()))
            .await?;
        let status = response.status();
        let value = read_json(response).await?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(error_from(status, &value))
        }
    }
}

fn agent_error(error: hyper::Error) -> AppError {
    AppError::unavailable(format!("the root file agent did not answer: {error}"))
}

/// Read a (bounded) JSON body from an agent response.
pub async fn read_json(response: Response<Body>) -> AppResult<Value> {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|error| AppError::unavailable(format!("root file agent reply: {error}")))?;
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Rebuild the agent's error for the caller of a background job.
pub fn error_from(status: StatusCode, body: &Value) -> AppError {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("the root file agent failed")
        .to_string();
    match status {
        StatusCode::CONFLICT => AppError::Conflict(message),
        StatusCode::SERVICE_UNAVAILABLE => AppError::Unavailable(message),
        status if status.is_server_error() => AppError::Internal(message),
        _ => AppError::BadRequest(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_paths_map_to_the_real_file_system() {
        assert!(is_root_path("/@root"));
        assert!(is_root_path("/@root/etc"));
        assert!(!is_root_path("/@rootx"));
        assert!(!is_root_path("/"));
        assert!(!is_root_path("/shared/@root"));
        assert_eq!(physical("/@root").unwrap(), PathBuf::from("/"));
        assert_eq!(physical("/@root/").unwrap(), PathBuf::from("/"));
        assert_eq!(
            physical("/@root//etc/hosts").unwrap(),
            PathBuf::from("/etc/hosts")
        );
        assert!(physical("/@root/etc/../shadow").is_err());
        assert!(physical("/etc").is_err());
        assert_eq!(logical(Path::new("/")), "/@root");
        assert_eq!(logical(Path::new("/etc/hosts")), "/@root/etc/hosts");
    }

    /// The whole path the API layer uses: client, socket, token, agent.
    #[tokio::test]
    async fn requests_reach_the_agent_and_stream_back() {
        let dir = crate::test_support::socket_tempdir();
        let socket = dir.path().join("agent.sock");
        let token = "t".repeat(64);
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, &token).unwrap();
        tokio::spawn(agent::run(
            socket.clone(),
            token_file,
            std::time::Duration::from_secs(600),
        ));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let native = NativeClient::new(dir.path().join("no-helper.sock"));
        let client = RootFs::with_agent(native, socket.clone(), &token);
        let work = logical(dir.path());

        // A JSON call, then an upload and a ranged download, both streamed.
        let made = client
            .call(
                "/api/files/mkdir",
                &serde_json::json!({"path": format!("{work}/docs")}),
            )
            .await
            .unwrap();
        assert_eq!(made["kind"], "dir");
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, "11".parse().unwrap());
        let uploaded = client
            .send(
                Method::POST,
                &format!("/api/files/upload?dir={work}/docs&name=a.txt"),
                &headers,
                Body::from("hello world"),
            )
            .await
            .unwrap();
        assert_eq!(uploaded.status(), StatusCode::OK);
        assert_eq!(read_json(uploaded).await.unwrap()["size"], 11);
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=6-".parse().unwrap());
        let part = client
            .send(
                Method::GET,
                &format!("/api/files/download?path={work}/docs/a.txt"),
                &headers,
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        let bytes = axum::body::to_bytes(part.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"world");

        // The agent's errors come back as errors, with its message.
        let error = client
            .call(
                "/api/files/delete",
                &serde_json::json!({"paths": [format!("{work}/missing")]}),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)), "{error}");

        // A wrong token is refused; a dead agent with no helper is reported.
        let stranger = RootFs::with_agent(
            NativeClient::new(dir.path().join("no-helper.sock")),
            socket,
            &"x".repeat(64),
        );
        let refused = stranger
            .send(
                Method::GET,
                "/api/files?path=/@root",
                &HeaderMap::new(),
                Body::empty(),
            )
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let gone = RootFs::with_agent(
            NativeClient::new(dir.path().join("no-helper.sock")),
            dir.path().join("nothing.sock"),
            &token,
        );
        assert!(matches!(
            gone.send(
                Method::GET,
                "/api/files?path=/@root",
                &HeaderMap::new(),
                Body::empty()
            )
            .await,
            Err(AppError::Unavailable(_))
        ));
    }
}
