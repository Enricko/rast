//! Unprivileged client of the privileged native helper.
//!
//! The RAST web server (running as `rast`) uses [`NativeClient`] to talk to
//! `rast-native.service` (running as root) over the Unix socket configured by
//! `native_helper_socket`. The client opens one short-lived connection per
//! call, so it can be shared freely across tasks without any connection state.
//!
//! Every call is bounded by the operation's deadline (quick 15 s, `apt` preview
//! 120 s, install 900 s, remove 300 s, `apt_update` 190 s). Responses larger
//! than 1 MiB or with a mismatched request id are rejected as protocol errors;
//! errors from the helper keep their stable `code` and actionable `message`.
//!
//! No secret is ever logged or embedded in an error message by this module.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{Map, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::native::protocol::{
    NativeError, NativeRequest, NativeResponse, MAX_RESPONSE_BYTES, QUICK_OP_TIMEOUT,
};

/// Timeouts used for each operation class. Built from the protocol constants;
/// tests may lower them (the fields are private to this module).
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    quick: Duration,
    identity: Duration,
    preview: Duration,
    install: Duration,
    remove: Duration,
    download: Duration,
    update: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            quick: crate::native::protocol::QUICK_OP_TIMEOUT,
            identity: crate::native::protocol::IDENTITY_OP_TIMEOUT,
            preview: crate::native::protocol::DEB_PREVIEW_TIMEOUT,
            install: crate::native::protocol::DEB_INSTALL_TIMEOUT,
            remove: crate::native::protocol::DEB_REMOVE_TIMEOUT,
            download: crate::native::protocol::DEB_DOWNLOAD_TIMEOUT,
            update: crate::native::protocol::APT_UPDATE_TIMEOUT,
        }
    }
}

/// Cloneable, concurrent-safe client of the native helper socket.
#[derive(Debug, Clone)]
pub struct NativeClient {
    socket: PathBuf,
    next_id: Arc<AtomicU64>,
    timeouts: Timeouts,
}

impl NativeClient {
    /// Create a client for the helper listening on `socket`.
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            next_id: Arc::new(AtomicU64::new(1)),
            timeouts: Timeouts::default(),
        }
    }

    /// Build a client from the resolved configuration.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self::new(cfg.native_helper_socket.clone())
    }

    /// Path of the helper socket this client talks to.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    /// Allocate a request id. Ids are only required to be unique per client;
    /// the helper echoes them back verbatim.
    fn allocate_id(&self) -> u64 {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Cheap liveness/feature probe. Returns the helper's `data` object:
    /// `{"version":1,"root":true,"xpra":true,"dockerFallback":false}`.
    pub async fn ping(&self) -> Result<Value, NativeError> {
        self.call_json("ping", Value::Object(Map::new())).await
    }

    /// Send one typed request and return its `data` object.
    pub async fn call(&self, req: NativeRequest) -> Result<Value, NativeError> {
        let line = req.to_line()?;
        self.exchange(&line, req.id, req.timeout(), req.name())
            .await
    }

    /// Send one generic JSON request (escape hatch for callers that prefer raw
    /// JSON). `params` must be a JSON object; it may not contain `id` or `op`.
    pub async fn call_json(&self, op: &str, params: Value) -> Result<Value, NativeError> {
        let op = validate_op_name(op)?;
        let Value::Object(mut fields) = params else {
            return Err(NativeError::invalid("request params must be a JSON object"));
        };
        for reserved in ["id", "op"] {
            if fields.contains_key(reserved) {
                return Err(NativeError::invalid(format!(
                    "request params must not contain `{reserved}`"
                )));
            }
        }
        let id = self.allocate_id();
        fields.insert("id".to_string(), Value::from(id));
        fields.insert("op".to_string(), Value::String(op.to_string()));
        let mut line = serde_json::to_vec(&Value::Object(fields))
            .map_err(|error| NativeError::protocol(format!("cannot encode request: {error}")))?;
        line.push(b'\n');
        self.exchange(&line, id, self.timeout_for(op), op).await
    }

    fn timeout_for(&self, op: &str) -> Duration {
        match op {
            "deb_preview" => self.timeouts.preview,
            "deb_install" => self.timeouts.install,
            "deb_remove" => self.timeouts.remove,
            "deb_download" => self.timeouts.download,
            "apt_update" => self.timeouts.update,
            "identity_ensure" | "identity_remove" => self.timeouts.identity,
            _ => self.timeouts.quick,
        }
    }

    /// One request/response exchange over a fresh connection.
    async fn exchange(
        &self,
        line: &[u8],
        expected_id: u64,
        deadline: Duration,
        op: &str,
    ) -> Result<Value, NativeError> {
        let socket = self.socket.clone();
        let exchange = async move {
            let stream = UnixStream::connect(&socket).await.map_err(|error| {
                NativeError::unavailable(format!(
                    "cannot connect to the native helper at {}: {error}",
                    socket.display()
                ))
            })?;
            let (read_half, mut write_half) = stream.into_split();
            write_half.write_all(line).await.map_err(|error| {
                NativeError::unavailable(format!(
                    "cannot send the request to the native helper: {error}"
                ))
            })?;
            write_half.flush().await.ok();
            let mut reader = BufReader::new(read_half);
            let read =
                tokio::time::timeout(deadline, read_line_capped(&mut reader, MAX_RESPONSE_BYTES))
                    .await;
            let line = match read {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => {
                    return Err(NativeError::protocol(format!(
                        "the native helper closed the connection without answering `{op}`"
                    )))
                }
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(NativeError::protocol(format!(
                        "the native helper response for `{op}` exceeds {MAX_RESPONSE_BYTES} bytes"
                    )))
                }
                Ok(Err(error)) => {
                    return Err(NativeError::protocol(format!(
                        "cannot read the native helper response for `{op}`: {error}"
                    )))
                }
                Err(_) => {
                    return Err(NativeError::timeout(format!(
                        "the native helper did not answer `{op}` within {deadline:?}"
                    )))
                }
            };
            if line.len() > MAX_RESPONSE_BYTES {
                return Err(NativeError::protocol(
                    "the native helper response exceeds the protocol size limit",
                ));
            }
            let response: NativeResponse = serde_json::from_slice(&line).map_err(|error| {
                NativeError::protocol(format!(
                    "cannot parse the native helper response for `{op}`: {error}"
                ))
            })?;
            if response.id != Some(expected_id) {
                return Err(NativeError::protocol(format!(
                    "the native helper answered request id {:?} instead of {expected_id}",
                    response.id
                )));
            }
            if response.ok {
                Ok(response.data.unwrap_or(Value::Null))
            } else {
                let error = response
                    .error
                    .unwrap_or(crate::native::protocol::NativeErrorBody {
                        code: "unknown".to_string(),
                        message: "the native helper returned an error without details".to_string(),
                    });
                Err(NativeError::new(error.code, error.message))
            }
        };
        // The body already applies the deadline around the response read; this
        // outer guard also bounds connect/write stalls.
        match tokio::time::timeout(deadline, exchange).await {
            Ok(result) => result,
            Err(_) => Err(NativeError::timeout(format!(
                "the native helper did not answer `{op}` within {deadline:?}"
            ))),
        }
    }
}

/// Operation names are generated server-side: lowercase snake_case.
fn validate_op_name(op: &str) -> Result<&str, NativeError> {
    if op.is_empty() || op.len() > 64 {
        return Err(NativeError::invalid("invalid operation name"));
    }
    if !op
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(NativeError::invalid(format!(
            "invalid operation name {op:?}"
        )));
    }
    Ok(op)
}

/// Read one newline-terminated line without ever buffering more than `max`
/// bytes past the limit (same framing as the Docker worker).
async fn read_line_capped<R>(reader: &mut R, max: usize) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            out.extend_from_slice(&buffer[..position]);
            reader.consume(position + 1);
            if out.len() > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response exceeds the protocol size limit",
                ));
            }
            return Ok(Some(out));
        }
        let consumed = buffer.len();
        out.extend_from_slice(buffer);
        reader.consume(consumed);
        if out.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response exceeds the protocol size limit",
            ));
        }
    }
}

/// Convenience wrapper: the default quick deadline.
pub const DEFAULT_CALL_TIMEOUT: Duration = QUICK_OP_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::protocol::{
        timeout_for_op, NativeOp, NativeResponse, SessionStopOp, DEB_INSTALL_TIMEOUT,
        DEB_PREVIEW_TIMEOUT, DEB_REMOVE_TIMEOUT,
    };
    use serde_json::json;
    use std::path::Path;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    struct FakeHelper {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeHelper {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn wait_for_socket(socket: &Path) {
        for _ in 0..200 {
            if UnixStream::connect(socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("fake helper socket did not appear");
    }

    /// Start a fake helper that answers every line with `respond(request)`.
    async fn fake_helper(
        respond: impl Fn(Value) -> Option<Value> + Send + Sync + 'static,
    ) -> FakeHelper {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("native.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let respond = Arc::new(respond);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let respond = respond.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);
                    let mut line = Vec::new();
                    if reader.read_until(b'\n', &mut line).await.is_err() {
                        return;
                    }
                    line.pop();
                    let request: Value = match serde_json::from_slice(&line) {
                        Ok(request) => request,
                        Err(_) => return,
                    };
                    if let Some(response) = respond(request) {
                        let mut bytes = serde_json::to_vec(&response).unwrap();
                        bytes.push(b'\n');
                        let _ = write_half.write_all(&bytes).await;
                    } else {
                        // Simulate a hung helper: keep the connection open
                        // until the client gives up and disconnects.
                        let mut sink = [0u8; 64];
                        loop {
                            match reader.read(&mut sink).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                });
            }
        });
        wait_for_socket(&socket).await;
        FakeHelper {
            _dir: dir,
            socket,
            server,
        }
    }

    fn data_response(id: u64, data: Value) -> Value {
        serde_json::to_value(NativeResponse::success(Some(id), data)).unwrap()
    }

    #[tokio::test]
    async fn call_roundtrips_a_typed_request() {
        let helper = fake_helper(|request| {
            assert_eq!(request["op"], "session_stop");
            assert_eq!(request["userId"], 7);
            Some(data_response(
                request["id"].as_u64().unwrap(),
                json!({"stopped": true}),
            ))
        })
        .await;
        let client = NativeClient::new(helper.socket.clone());
        let value = client
            .call(NativeRequest::new(
                1,
                NativeOp::SessionStop(SessionStopOp { user_id: 7 }),
            ))
            .await
            .unwrap();
        assert_eq!(value["stopped"], true);
    }

    #[tokio::test]
    async fn call_json_and_ping_use_generic_framing() {
        let helper = fake_helper(|request| {
            let id = request["id"].as_u64().unwrap();
            match request["op"].as_str().unwrap() {
                "ping" => Some(data_response(
                    id,
                    json!({"version": 1, "root": true, "xpra": true, "dockerFallback": false}),
                )),
                "session_stop" => {
                    assert_eq!(request["userId"], 3);
                    Some(data_response(id, json!({"stopped": true})))
                }
                _ => None,
            }
        })
        .await;
        let client = NativeClient::new(helper.socket.clone());
        let ping = client.ping().await.unwrap();
        assert_eq!(ping["version"], 1);
        let value = client
            .call_json("session_stop", json!({"userId": 3}))
            .await
            .unwrap();
        assert_eq!(value["stopped"], true);
    }

    #[tokio::test]
    async fn call_json_rejects_reserved_fields_and_bad_names() {
        let helper = fake_helper(|_| None).await;
        let client = NativeClient::new(helper.socket.clone());
        for error in [
            client.call_json("", json!({})).await.unwrap_err(),
            client.call_json("Bad-Name", json!({})).await.unwrap_err(),
            client.call_json("ping", json!([1])).await.unwrap_err(),
            client
                .call_json("ping", json!({"id": 5}))
                .await
                .unwrap_err(),
            client
                .call_json("ping", json!({"op": "x"}))
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.code, "invalid_request", "{error}");
        }
    }

    #[tokio::test]
    async fn remote_errors_keep_code_and_message() {
        let helper = fake_helper(|request| {
            let id = request["id"].as_u64().unwrap();
            Some(
                serde_json::to_value(NativeResponse::failure(
                    Some(id),
                    &NativeError::identity_conflict("stolen identity"),
                ))
                .unwrap(),
            )
        })
        .await;
        let client = NativeClient::new(helper.socket.clone());
        let error = client.ping().await.unwrap_err();
        assert_eq!(error.code, "identity_conflict");
        assert!(error.message.contains("stolen identity"));
    }

    #[tokio::test]
    async fn malformed_and_mismatched_responses_are_protocol_errors() {
        let helper = fake_helper(|_| Some(json!({"garbage": true}))).await;
        let client = NativeClient::new(helper.socket.clone());
        let error = client.ping().await.unwrap_err();
        assert_eq!(error.code, "protocol", "{error}");

        let helper = fake_helper(|request| {
            let id = request["id"].as_u64().unwrap() + 100;
            Some(data_response(id, json!({})))
        })
        .await;
        let client = NativeClient::new(helper.socket.clone());
        let error = client.ping().await.unwrap_err();
        assert_eq!(error.code, "protocol", "{error}");
        assert!(error.message.contains("request id"), "{error}");
    }

    #[tokio::test]
    async fn timeout_is_reported_and_caller_is_not_left_hanging() {
        let helper = fake_helper(|_| None).await;
        let mut client = NativeClient::new(helper.socket.clone());
        client.timeouts.quick = Duration::from_millis(100);
        let started = std::time::Instant::now();
        let error = client.ping().await.unwrap_err();
        assert_eq!(error.code, "timeout", "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn missing_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let client = NativeClient::new(dir.path().join("missing.sock"));
        let error = client.ping().await.unwrap_err();
        assert_eq!(error.code, "unavailable");
        assert!(error.message.contains("cannot connect"), "{error}");
    }

    #[tokio::test]
    async fn concurrent_calls_do_not_interleave() {
        let helper = fake_helper(|request| {
            let id = request["id"].as_u64().unwrap();
            Some(data_response(id, json!({"echo": request["userId"]})))
        })
        .await;
        let client = NativeClient::new(helper.socket.clone());
        let mut handles = Vec::new();
        for user_id in 1..=16i64 {
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                client
                    .call_json("identity_ensure", json!({"userId": user_id}))
                    .await
                    .unwrap()
            }));
        }
        for (index, handle) in handles.into_iter().enumerate() {
            let value = handle.await.unwrap();
            assert_eq!(value["echo"].as_i64().unwrap(), index as i64 + 1);
        }
    }

    #[test]
    fn per_op_deadlines_match_the_contract() {
        let client = NativeClient::new(PathBuf::from("/nonexistent"));
        assert_eq!(client.timeout_for("ping"), QUICK_OP_TIMEOUT);
        assert_eq!(
            client.timeout_for("identity_ensure"),
            crate::native::protocol::IDENTITY_OP_TIMEOUT
        );
        assert_eq!(client.timeout_for("deb_preview"), DEB_PREVIEW_TIMEOUT);
        assert_eq!(client.timeout_for("deb_install"), DEB_INSTALL_TIMEOUT);
        assert_eq!(client.timeout_for("deb_remove"), DEB_REMOVE_TIMEOUT);
        assert_eq!(
            client.timeout_for("deb_download"),
            crate::native::protocol::DEB_DOWNLOAD_TIMEOUT
        );
        assert_eq!(
            client.timeout_for("apt_update"),
            crate::native::protocol::APT_UPDATE_TIMEOUT
        );
        assert_eq!(
            timeout_for_op("session_start"),
            crate::native::protocol::QUICK_OP_TIMEOUT
        );
    }

    #[test]
    fn client_is_clone_and_allocates_rising_ids() {
        let client = NativeClient::new(PathBuf::from("/tmp/x.sock"));
        let clone = client.clone();
        let first = client.allocate_id();
        let second = clone.allocate_id();
        assert!(second > first);
        assert_eq!(client.socket_path(), Path::new("/tmp/x.sock"));
    }
}
