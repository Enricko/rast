//! Docker administration. The web server **never** opens the Docker socket
//! directly: every container operation is delegated to a privileged worker
//! process (`rast worker`) listening on a Unix socket. This module is the
//! unprivileged client side.
//!
//! The client opens a short-lived connection per request (which also acts as
//! the reconnect strategy if the worker restarted), frames one JSON line in and
//! one JSON line out, enforces per-operation deadlines, and validates that the
//! response belongs to the request that was sent.

pub mod compose;
pub mod containers;
pub mod protocol;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Mutex,
};

use crate::{config::Config, error::AppError};

use self::protocol::{
    validate_request, ValidatedAction, WorkerOp, WorkerRequest, WorkerResponse, MAX_LINE_BYTES,
};

/// How long a cached `status()` result is considered fresh.
const STATUS_TTL: Duration = Duration::from_secs(2);
/// Connect timeout for the worker socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Errors produced by the Docker client. The API layer maps these onto HTTP
/// errors (503 for "worker unavailable", 400 for rejected requests).
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("docker integration is disabled: {0}")]
    Disabled(String),
    #[error("docker worker unavailable: {0}")]
    Unavailable(String),
    #[error("docker worker rejected the request: {0}")]
    BadRequest(String),
    #[error("docker operation failed: {0}")]
    Failed(String),
    #[error("docker worker timed out after {0:?}")]
    Timeout(Duration),
    #[error("docker worker protocol error: {0}")]
    Protocol(String),
}

impl DockerError {
    /// True when retrying later (as opposed to fixing the request) may help.
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Disabled(_) | Self::Unavailable(_) | Self::Timeout(_)
        )
    }
}

impl From<DockerError> for AppError {
    fn from(err: DockerError) -> Self {
        match err {
            DockerError::BadRequest(message) => AppError::bad_request(message),
            DockerError::Failed(message) => AppError::unavailable(message),
            DockerError::Timeout(after) => {
                AppError::unavailable(format!("docker worker did not answer within {after:?}"))
            }
            DockerError::Disabled(reason) | DockerError::Unavailable(reason) => {
                AppError::unavailable(format!("docker worker unavailable: {reason}"))
            }
            DockerError::Protocol(message) => {
                AppError::unavailable(format!("docker worker protocol error: {message}"))
            }
        }
    }
}

/// Handle to the privileged Docker worker.
pub struct DockerManager {
    /// Whether Docker integration is enabled in the configuration.
    pub enabled: bool,
    /// Path of the worker's Unix socket.
    pub socket_path: PathBuf,
    /// `<data_dir>/apps`, used for client-side request validation.
    apps_dir: PathBuf,
    status_cache: Mutex<Option<(Instant, Value)>>,
    quick_timeout: Duration,
    compose_timeout: Duration,
}

impl DockerManager {
    pub fn new(cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            enabled: cfg.docker_enabled,
            socket_path: cfg.worker_socket.clone(),
            apps_dir: cfg.apps_dir(),
            status_cache: Mutex::new(None),
            quick_timeout: protocol::QUICK_OP_TIMEOUT,
            compose_timeout: protocol::COMPOSE_OP_TIMEOUT,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// `{enabled, available, version, reason}` for the UI. Cached briefly so
    /// dashboards/health endpoints do not hammer the worker.
    pub async fn status(&self) -> Value {
        if let Some((at, cached)) = self.status_cache.lock().await.as_ref() {
            if at.elapsed() < STATUS_TTL {
                return cached.clone();
            }
        }
        let status = self.probe_status().await;
        *self.status_cache.lock().await = Some((Instant::now(), status.clone()));
        status
    }

    /// Force a fresh `{enabled, available, version, reason}` probe.
    pub async fn refresh_status(&self) -> Value {
        let status = self.probe_status().await;
        *self.status_cache.lock().await = Some((Instant::now(), status.clone()));
        status
    }

    async fn probe_status(&self) -> Value {
        if !self.enabled {
            return json!({
                "enabled": false,
                "available": false,
                "version": Value::Null,
                "reason": "Docker integration is disabled (RAST_DOCKER_ENABLED=false)",
            });
        }
        if !self.socket_path.exists() {
            return json!({
                "enabled": true,
                "available": false,
                "version": Value::Null,
                "reason": format!(
                    "docker worker socket {} is missing (is rast-worker.service running?)",
                    self.socket_path.display()
                ),
            });
        }
        match self.send(&WorkerRequest::new(WorkerOp::Version)).await {
            Ok(data) => json!({
                "enabled": true,
                "available": true,
                "version": data.get("version").cloned().unwrap_or(Value::Null),
                "reason": "docker worker is reachable",
            }),
            Err(err) => json!({
                "enabled": true,
                "available": false,
                "version": Value::Null,
                "reason": err.to_string(),
            }),
        }
    }

    /// Fail with 503 when the Docker subsystem cannot be used, including a
    /// human-readable reason that the UI can display.
    pub async fn ensure_available(&self) -> Result<(), AppError> {
        if !self.enabled {
            return Err(AppError::unavailable(
                "Docker integration is disabled (set RAST_DOCKER_ENABLED=true)",
            ));
        }
        if !self.socket_path.exists() {
            return Err(AppError::unavailable(format!(
                "docker worker unavailable: socket {} is missing (is rast-worker.service running?)",
                self.socket_path.display()
            )));
        }
        match self.send(&WorkerRequest::new(WorkerOp::Version)).await {
            Ok(_) => Ok(()),
            Err(err) => Err(AppError::unavailable(format!(
                "docker worker unavailable: {err}"
            ))),
        }
    }

    /// Perform one whitelisted operation on the worker.
    pub async fn call(&self, request: WorkerRequest) -> Result<Value, DockerError> {
        self.send(&request).await
    }

    /// Convenience: containers of one managed app.
    pub async fn app_containers(&self, slug: &str) -> Result<Value, DockerError> {
        self.call(WorkerRequest::new(WorkerOp::AppContainers).slug(slug))
            .await
    }

    /// Convenience: all containers on the host.
    pub async fn all_containers(&self) -> Result<Value, DockerError> {
        self.call(WorkerRequest::new(WorkerOp::PsAll)).await
    }

    /// Convenience: current `docker stats` samples.
    pub async fn container_stats(&self) -> Result<Value, DockerError> {
        self.call(WorkerRequest::new(WorkerOp::Stats)).await
    }

    /// Send a request over the worker socket and parse the response.
    async fn send(&self, request: &WorkerRequest) -> Result<Value, DockerError> {
        if !self.enabled {
            return Err(DockerError::Disabled("RAST_DOCKER_ENABLED is false".into()));
        }
        // Validate once more on the client side: this catches path handling
        // bugs before they reach the privileged process. The worker re-checks.
        validate_request(request, &self.apps_dir).map_err(DockerError::BadRequest)?;
        let timeout = match request.op {
            op if op.is_app_op() => self.compose_timeout,
            _ => self.quick_timeout,
        };
        match tokio::time::timeout(timeout, self.roundtrip(request)).await {
            Ok(result) => result,
            Err(_) => Err(DockerError::Timeout(timeout)),
        }
    }

    async fn roundtrip(&self, request: &WorkerRequest) -> Result<Value, DockerError> {
        let mut stream =
            tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket_path))
                .await
                .map_err(|_| {
                    DockerError::Unavailable("connection to the worker socket timed out".into())
                })?
                .map_err(|e| {
                    DockerError::Unavailable(format!(
                        "cannot connect to {}: {e}",
                        self.socket_path.display()
                    ))
                })?;

        let mut line = serde_json::to_vec(request)
            .map_err(|e| DockerError::Protocol(format!("cannot encode request: {e}")))?;
        line.push(b'\n');
        stream.write_all(&line).await.map_err(|e| {
            DockerError::Unavailable(format!("cannot write to the worker socket: {e}"))
        })?;
        stream.flush().await.map_err(|e| {
            DockerError::Unavailable(format!("cannot flush the worker socket: {e}"))
        })?;

        let raw = read_line(&mut stream).await.map_err(|e| {
            DockerError::Unavailable(format!("cannot read from the worker socket: {e}"))
        })?;
        if raw.is_empty() {
            return Err(DockerError::Protocol(
                "worker closed the connection without a response".into(),
            ));
        }
        let response: WorkerResponse = serde_json::from_slice(&raw)
            .map_err(|e| DockerError::Protocol(format!("invalid worker response: {e}")))?;
        if response.id != request.id {
            return Err(DockerError::Protocol(
                "worker response id does not match the request".into(),
            ));
        }
        if response.ok {
            return Ok(response.data.unwrap_or(Value::Null));
        }
        let error = response.error.unwrap_or(protocol::WorkerErrorBody {
            code: "unknown".into(),
            message: "worker reported an error without details".into(),
        });
        match error.code.as_str() {
            "invalid_request" | "not_found" | "forbidden" => {
                Err(DockerError::BadRequest(error.message))
            }
            "timeout" => Err(DockerError::Unavailable(format!(
                "docker operation timed out: {}",
                error.message
            ))),
            "unavailable" => Err(DockerError::Unavailable(error.message)),
            _ => Err(DockerError::Failed(error.message)),
        }
    }
}

/// Read one `\n` terminated line with a hard size cap.
async fn read_line(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if let Some(pos) = chunk[..n].iter().position(|b| *b == b'\n') {
            out.extend_from_slice(&chunk[..pos]);
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "worker response exceeds the protocol size limit",
            ));
        }
    }
    if out.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "worker response exceeds the protocol size limit",
        ));
    }
    Ok(out)
}

/// Validate a request against an applications root without talking to the
/// worker. Useful for the web-side pre-checks and unit tests.
pub fn validate_worker_request(
    request: &WorkerRequest,
    apps_dir: &Path,
) -> Result<ValidatedAction, String> {
    validate_request(request, apps_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;
    use tokio::task::JoinHandle;

    fn test_config(socket: PathBuf, enabled: bool) -> Config {
        let mut cfg =
            Config::from_parts(None, crate::config::FileConfig::default(), |_| None).unwrap();
        cfg.worker_socket = socket;
        cfg.docker_enabled = enabled;
        cfg
    }

    /// Start a fake worker on a temp Unix socket. The handler receives the
    /// parsed request and returns the response body (success or failure).
    async fn fake_worker(
        socket: PathBuf,
        handler: impl Fn(WorkerRequest) -> Result<Value, protocol::WorkerErrorBody>
            + Send
            + Sync
            + 'static,
    ) -> JoinHandle<()> {
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let handler = std::sync::Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) => return,
                            Ok(_) => {
                                if byte[0] == b'\n' {
                                    break;
                                }
                                raw.push(byte[0]);
                            }
                            Err(_) => return,
                        }
                    }
                    let Ok(request) = serde_json::from_slice::<WorkerRequest>(&raw) else {
                        return;
                    };
                    let response = match handler(request) {
                        Ok(data) => WorkerResponse::success("unused", data),
                        Err(error) => WorkerResponse::failure("unused", error.code, error.message),
                    };
                    // Echo the request id like the real worker.
                    let request_id = serde_json::from_slice::<Value>(&raw)
                        .ok()
                        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_default();
                    let response = WorkerResponse {
                        id: request_id,
                        ..response
                    };
                    let mut line = serde_json::to_vec(&response).unwrap();
                    line.push(b'\n');
                    let _ = stream.write_all(&line).await;
                    let _ = stream.flush().await;
                });
            }
        })
    }

    fn manager_with_timeouts(socket: PathBuf, timeout: StdDuration) -> Arc<DockerManager> {
        Arc::new(DockerManager {
            enabled: true,
            socket_path: socket,
            apps_dir: PathBuf::from("/tmp/rast-test/apps"),
            status_cache: Mutex::new(None),
            quick_timeout: timeout,
            compose_timeout: timeout,
        })
    }

    #[tokio::test]
    async fn status_reports_available_when_worker_answers() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let worker = fake_worker(socket.clone(), |req| {
            assert_eq!(req.op, WorkerOp::Version);
            Ok(json!({"version": "26.1.4", "apiVersion": "1.45"}))
        })
        .await;
        let manager = manager_with_timeouts(socket, StdDuration::from_secs(5));
        let status = manager.status().await;
        assert_eq!(status["enabled"], true);
        assert_eq!(status["available"], true);
        assert_eq!(status["version"], "26.1.4");
        worker.abort();
    }

    #[tokio::test]
    async fn status_reports_missing_socket_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path().join("missing.sock"), true);
        let manager = DockerManager::new(&cfg);
        let status = manager.status().await;
        assert_eq!(status["enabled"], true);
        assert_eq!(status["available"], false);
        let reason = status["reason"].as_str().unwrap();
        assert!(reason.contains("missing"), "{reason}");
    }

    #[tokio::test]
    async fn status_reports_disabled_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path().join("worker.sock"), false);
        let manager = DockerManager::new(&cfg);
        let status = manager.status().await;
        assert_eq!(status["enabled"], false);
        assert_eq!(status["available"], false);
        assert!(manager.ensure_available().await.is_err());
        // 503 mapping.
        let err = manager.ensure_available().await.unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn calls_are_forwarded_and_errors_are_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let worker = fake_worker(socket.clone(), |req| match req.op {
            WorkerOp::Info => Ok(json!({"containers": 3})),
            WorkerOp::AppContainers => Err(protocol::WorkerErrorBody {
                code: "invalid_request".into(),
                message: "bad slug".into(),
            }),
            _ => Err(protocol::WorkerErrorBody {
                code: "command_failed".into(),
                message: "docker said no".into(),
            }),
        })
        .await;
        let manager = manager_with_timeouts(socket, StdDuration::from_secs(5));
        let info = manager
            .call(WorkerRequest::new(WorkerOp::Info))
            .await
            .unwrap();
        assert_eq!(info["containers"], 3);
        let err = manager
            .call(WorkerRequest::new(WorkerOp::AppContainers).slug("shop"))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::BadRequest(_)));
        let err = manager
            .call(WorkerRequest::new(WorkerOp::PsAll))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::Failed(_)));
        worker.abort();
    }

    #[tokio::test]
    async fn unresponsive_worker_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let bad_worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(StdDuration::from_secs(30)).await;
            drop(stream);
        });
        let manager = manager_with_timeouts(socket, StdDuration::from_millis(100));
        let err = manager
            .call(WorkerRequest::new(WorkerOp::Version))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::Timeout(_)), "got {err:?}");
        bad_worker.abort();
    }

    #[tokio::test]
    async fn malformed_or_mismatched_responses_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let bad_worker = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut byte = [0u8; 1];
                    while stream.read(&mut byte).await.unwrap_or(0) == 1 && byte[0] != b'\n' {
                        raw.push(byte[0]);
                    }
                    let _ = stream.write_all(b"not json\n").await;
                });
            }
        });
        let manager = manager_with_timeouts(socket.clone(), StdDuration::from_secs(5));
        let err = manager
            .call(WorkerRequest::new(WorkerOp::Version))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::Protocol(_)), "got {err:?}");
        bad_worker.abort();
    }

    #[tokio::test]
    async fn client_validates_requests_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        // No worker is running; a request that fails client-side validation is
        // rejected as BadRequest before any connection attempt.
        let manager = manager_with_timeouts(socket, StdDuration::from_millis(200));
        let err = manager
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("../escape"))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn from_error_maps_to_app_errors() {
        let err: AppError = DockerError::Unavailable("down".into()).into();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let err: AppError = DockerError::BadRequest("bad".into()).into();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        let err: AppError = DockerError::Failed("boom".into()).into();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}

/// Cross-module integration test: the unprivileged client speaks the real
/// worker protocol (framing, ids, validation, timeouts) against the real worker
/// server, with a fake `docker` binary so no daemon is required.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration as StdDuration;

    const FAKE_DOCKER: &str = r#"#!/bin/sh
case "$1" in
  version) echo "26.1.4" ;;
  compose)
    echo '{"services":{"web":{"image":"nginx:alpine","ports":[{"mode":"ingress","target":80,"published":"8080","protocol":"tcp"}]}}}'
    ;;
  ps)
    echo '{"ID":"0123456789ab","Image":"nginx:alpine","Names":"shop-web-1","State":"running","Status":"Up 1 min","Ports":"0.0.0.0:8080->80/tcp","Mounts":"","Labels":"com.docker.compose.project=rast-shop,com.docker.compose.service=web"}'
    ;;
esac
exit 0
"#;

    fn config(data_dir: &Path, socket: &Path) -> Config {
        let mut cfg =
            Config::from_parts(None, crate::config::FileConfig::default(), |_| None).unwrap();
        cfg.data_dir = data_dir.to_path_buf();
        cfg.storage_dir = data_dir.join("storage");
        cfg.worker_socket = socket.to_path_buf();
        cfg.docker_enabled = true;
        cfg
    }

    #[tokio::test]
    async fn client_and_worker_interoperate_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let apps_dir = data_dir.join("apps");
        std::fs::create_dir_all(apps_dir.join("shop")).unwrap();
        std::fs::write(
            apps_dir.join("shop/compose.yaml"),
            "services:\n  web:\n    image: nginx:alpine\n",
        )
        .unwrap();
        std::fs::write(apps_dir.join("shop/.env"), "").unwrap();

        let docker_bin = dir.path().join("fake-docker");
        std::fs::write(&docker_bin, FAKE_DOCKER).unwrap();
        std::fs::set_permissions(&docker_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let socket = dir.path().join("worker.sock");
        let options = crate::worker::WorkerOptions {
            socket_path: socket.clone(),
            apps_dir,
            docker_bin,
            quick_timeout: StdDuration::from_secs(5),
            compose_timeout: StdDuration::from_secs(5),
            max_concurrent: 2,
            allowed_peer_uid: None,
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            crate::worker::serve(options, async move {
                let _ = rx.await;
            })
            .await
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }

        let manager = DockerManager::new(&config(&data_dir, &socket));
        let status = manager.status().await;
        assert_eq!(status["available"], true, "{status}");
        assert_eq!(status["version"], "26.1.4");

        let validated = manager
            .call(WorkerRequest::new(WorkerOp::ComposeValidate).slug("shop"))
            .await
            .unwrap();
        assert_eq!(validated["valid"], true);

        let containers = manager.app_containers("shop").await.unwrap();
        assert_eq!(containers["items"][0]["name"], "shop-web-1");

        // Client-side validation rejects traversal before it reaches the wire.
        let err = manager
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("../escape"))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::BadRequest(_)), "got {err:?}");

        // Worker-side validation rejects a missing app with a not_found code.
        let err = manager
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::BadRequest(_)), "got {err:?}");

        let _ = tx.send(());
        let _ = server.await;
        assert!(!socket.exists(), "worker socket should be cleaned up");
    }
}
