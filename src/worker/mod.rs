//! Privileged administrative worker for Docker operations.
//!
//! The worker runs as a separate systemd unit (`rast-worker.service`, in the
//! `docker` group, never exposed to the network) and listens on the Unix socket
//! configured by `RAST_WORKER_SOCKET`. The unprivileged web server (and only
//! it) sends JSON lines with *structured* arguments; this module validates them
//! and executes a small whitelist of Docker/Compose commands.
//!
//! Security properties:
//! * No shell is ever involved: [`tokio::process::Command`] receives argument
//!   vectors and the working directory is always the validated app directory.
//! * Only [`WorkerOp`] operations are accepted; unknown operations and extra
//!   fields fail deserialization.
//! * App slugs must match `[a-z0-9-]`; every path is derived from the slug
//!   under `<data_dir>/apps/<slug>` and canonicalised before use.
//! * Compose files must pass [`crate::docker::compose::check_compose_text`] and
//!   the normalized configuration check before `up`; bind mounts are limited to
//!   `<app>/data`, host namespaces/devices/privileged mode are refused.
//! * Every operation has a timeout; output is size-capped; environment
//!   variables that could redirect Compose are removed.
//! * `down` never removes volumes: `-v` is never passed. Explicit volume
//!   removal is a separate, project-label-scoped operation.
//!
//! Socket permissions are `0660` (root:<group>). Set `RAST_WORKER_SOCKET_GROUP`
//! to a group name or numeric gid (typically `rast`) and
//! `RAST_WORKER_ALLOWED_UID` to additionally enforce the peer uid. When the
//! variables are unset the socket is created `0660` owned by the worker's
//! user/group and any local user that can open it is trusted.

use std::{
    ffi::OsString,
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{oneshot, Semaphore},
};

use crate::{
    config::Config,
    docker::{
        compose,
        containers::{parse_compose_logs, parse_inspect_json, parse_ps_json, parse_stats_json},
        protocol::{
            truncate_message, validate_container_id, validate_request, ValidatedAction, WorkerOp,
            WorkerRequest, WorkerResponse, COMPOSE_OP_TIMEOUT, MAX_LINE_BYTES, MAX_LOG_TAIL,
            PROTOCOL_VERSION, QUICK_OP_TIMEOUT,
        },
    },
};

/// Maximum bytes of Docker stdout kept per command.
const MAX_CMD_OUTPUT: usize = 4 * 1024 * 1024;
/// Maximum bytes of Docker stderr kept for error messages.
const MAX_CMD_STDERR: usize = 16 * 1024;
/// Maximum error message length returned to the client.
const MAX_ERROR_MESSAGE: usize = 800;
/// Maximum number of containers returned by list operations.
const MAX_CONTAINERS: usize = 500;
/// Maximum number of volumes removed in one request.
const MAX_VOLUMES: usize = 200;
/// Maximum bytes read from a Compose file.
const MAX_COMPOSE_READ: u64 = compose::MAX_COMPOSE_BYTES as u64;
/// How long the worker waits for a second client before considering the
/// connection idle-closed. Requests are one-per-connection in practice but the
/// JSON-lines framing supports several.
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything the worker needs to run. Built from [`Config`] in production and
/// directly in tests.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub socket_path: PathBuf,
    pub apps_dir: PathBuf,
    pub docker_bin: PathBuf,
    pub quick_timeout: Duration,
    pub compose_timeout: Duration,
    pub max_concurrent: usize,
    /// Optional numeric uid the connecting peer must have.
    pub allowed_peer_uid: Option<u32>,
}

impl WorkerOptions {
    pub fn from_config(cfg: &Config) -> Self {
        let docker_bin = std::env::var("RAST_DOCKER_BIN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("docker"));
        let allowed_peer_uid = std::env::var("RAST_WORKER_ALLOWED_UID")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok());
        Self {
            socket_path: cfg.worker_socket.clone(),
            apps_dir: cfg.apps_dir(),
            docker_bin,
            quick_timeout: QUICK_OP_TIMEOUT,
            compose_timeout: COMPOSE_OP_TIMEOUT,
            max_concurrent: 2,
            allowed_peer_uid,
        }
    }
}

/// Error returned to the client. `code` is a stable machine-readable string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerError {
    pub code: &'static str,
    pub message: String,
}

impl WorkerError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: truncate_message(&message.into(), MAX_ERROR_MESSAGE),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self::new("unavailable", message)
    }
    fn command_failed(message: impl Into<String>) -> Self {
        Self::new("command_failed", message)
    }
}

/// CLI entry point for `rast worker`.
pub async fn run(cfg: Config) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let options = WorkerOptions::from_config(&cfg);
    tracing::info!(
        socket = %options.socket_path.display(),
        apps_dir = %options.apps_dir.display(),
        "starting privileged docker worker"
    );
    let (tx, rx) = oneshot::channel::<()>();
    let shutdown = async move {
        let _ = rx.await;
    };
    // Bridge OS signals to the oneshot shutdown used by `serve`.
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = tx.send(());
    });
    serve(options, shutdown)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("docker worker failed: {e}").into()
        })?;
    tracing::info!("docker worker stopped");
    Ok(ExitCode::SUCCESS)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Run the worker server until `shutdown` resolves. Exposed separately from
/// [`run`] so tests can drive it on a temporary socket.
pub async fn serve(options: WorkerOptions, shutdown: impl Future<Output = ()>) -> io::Result<()> {
    let socket_path = options.socket_path.clone();
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    apply_socket_permissions(&socket_path)?;
    tracing::info!(socket = %socket_path.display(), "docker worker listening");

    let options = Arc::new(options);
    let semaphore = Arc::new(Semaphore::new(options.max_concurrent.max(1)));
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let options = options.clone();
                        let semaphore = semaphore.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, options, semaphore).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "docker worker accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Create the parent directory and clear a stale socket file. Refuses to delete
/// a non-socket path that happens to sit where the socket belongs.
fn prepare_socket_path(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_socket() {
                std::fs::remove_file(path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a socket; refusing to remove it",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Restrictive socket permissions: 0660, optionally chowned to a configured
/// group so only the web server's group can connect.
fn apply_socket_permissions(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Ok(group) = std::env::var("RAST_WORKER_SOCKET_GROUP") {
        let group = group.trim();
        if !group.is_empty() {
            match resolve_group_id(group) {
                Some(gid) => {
                    std::os::unix::fs::chown(path, None, Some(gid))?;
                }
                None => tracing::warn!(
                    group,
                    "RAST_WORKER_SOCKET_GROUP is not a known group name or gid; socket stays owned by the worker group"
                ),
            }
        }
    }
    Ok(())
}

/// Resolve a group name (via `/etc/group`) or a numeric gid.
fn resolve_group_id(value: &str) -> Option<u32> {
    if let Ok(gid) = value.parse::<u32>() {
        return Some(gid);
    }
    let text = std::fs::read_to_string("/etc/group").ok()?;
    for line in text.lines() {
        let mut parts = line.split(':');
        let name = parts.next()?;
        let _password = parts.next()?;
        let gid = parts.next()?;
        if name == value {
            return gid.parse::<u32>().ok();
        }
    }
    None
}

async fn handle_connection(
    stream: UnixStream,
    options: Arc<WorkerOptions>,
    semaphore: Arc<Semaphore>,
) {
    if let Some(required_uid) = options.allowed_peer_uid {
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == required_uid => {}
            Ok(cred) => {
                tracing::warn!(
                    uid = cred.uid(),
                    "rejecting worker client with unexpected uid"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot read worker peer credentials");
            }
        }
    }

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let line = match tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, next_line(&mut reader)).await
        {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                let response = WorkerResponse::failure(
                    "",
                    "invalid_request",
                    "request exceeds the protocol size limit",
                );
                let _ = write_response(&mut write_half, &response).await;
                break;
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        let response = match serde_json::from_slice::<WorkerRequest>(&line) {
            Ok(request) => process_request(request, &options, &semaphore).await,
            Err(error) => WorkerResponse::failure(
                "",
                "invalid_request",
                format!("malformed JSON request: {error}"),
            ),
        };
        if write_response(&mut write_half, &response).await.is_err() {
            break;
        }
    }
}

async fn write_response(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    response: &WorkerResponse,
) -> io::Result<()> {
    let mut line =
        serde_json::to_vec(response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await
}

/// Read one `\n`-terminated line with a hard cap. Returns `None` on clean EOF.
async fn next_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            out.extend_from_slice(&buffer[..position]);
            reader.consume(position + 1);
            if out.len() > MAX_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request line too large",
                ));
            }
            return Ok(Some(out));
        }
        let consumed = buffer.len();
        out.extend_from_slice(buffer);
        reader.consume(consumed);
        if out.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request line too large",
            ));
        }
    }
}

async fn process_request(
    request: WorkerRequest,
    options: &WorkerOptions,
    semaphore: &Semaphore,
) -> WorkerResponse {
    let id = request.id.clone();
    let action = match validate_request(&request, &options.apps_dir) {
        Ok(action) => action,
        Err(message) => {
            tracing::warn!(
                op = request.op.as_str(),
                "rejected worker request: {message}"
            );
            return WorkerResponse::failure(id, "invalid_request", message);
        }
    };
    let permit = match semaphore.acquire().await {
        Ok(permit) => permit,
        Err(_) => {
            return WorkerResponse::failure(id, "unavailable", "worker is shutting down");
        }
    };
    tracing::debug!(op = action.op().as_str(), "worker executing request");
    let outcome = dispatch(action, options).await;
    drop(permit);
    match outcome {
        Ok(data) => WorkerResponse::success(id, data),
        Err(error) => {
            tracing::warn!(
                op = request.op.as_str(),
                code = error.code,
                "worker request failed: {}",
                error.message
            );
            WorkerResponse::failure(id, error.code, error.message)
        }
    }
}

async fn dispatch(action: ValidatedAction, options: &WorkerOptions) -> Result<Value, WorkerError> {
    match action {
        ValidatedAction::Plain(op) => match op {
            WorkerOp::Version => docker_version(options).await,
            WorkerOp::Info => docker_info(options).await,
            WorkerOp::PsAll => docker_ps_all(options).await,
            WorkerOp::Stats => docker_stats(options).await,
            _ => Err(WorkerError::invalid(format!(
                "operation '{}' requires application arguments",
                op.as_str()
            ))),
        },
        ValidatedAction::Inspect(ids) => docker_inspect(options, &ids).await,
        ValidatedAction::App {
            op,
            slug,
            app_dir,
            compose_path,
            env_path,
            project,
            tail,
        } => {
            let context = AppContext {
                slug,
                app_dir,
                compose_path,
                env_path,
                project,
            };
            match op {
                WorkerOp::ComposeValidate => {
                    let summary = validate_compose(options, &context).await?;
                    Ok(json!({
                        "valid": true,
                        "services": summary.services,
                        "bindMounts": summary.bind_mounts,
                    }))
                }
                WorkerOp::ComposeUp => {
                    validate_compose(options, &context).await?;
                    let output =
                        compose_exec(options, &context, &["up", "-d", "--remove-orphans"]).await?;
                    ensure_success("compose up", &output)?;
                    Ok(json!({"action": "up", "running": true}))
                }
                WorkerOp::ComposeDown => {
                    context.ensure_files(options)?;
                    let output =
                        compose_exec(options, &context, &["down", "--remove-orphans"]).await?;
                    ensure_success("compose down", &output)?;
                    Ok(json!({"action": "down", "running": false}))
                }
                WorkerOp::ComposeStop => {
                    context.ensure_files(options)?;
                    let output = compose_exec(options, &context, &["stop"]).await?;
                    ensure_success("compose stop", &output)?;
                    Ok(json!({"action": "stop", "running": false}))
                }
                WorkerOp::ComposeRestart => {
                    validate_compose(options, &context).await?;
                    let output = compose_exec(options, &context, &["restart"]).await?;
                    ensure_success("compose restart", &output)?;
                    Ok(json!({"action": "restart", "running": true}))
                }
                WorkerOp::RemoveVolumes => remove_project_volumes(options, &context).await,
                WorkerOp::Logs => docker_compose_logs(options, &context, tail.unwrap_or(200)).await,
                WorkerOp::Ps | WorkerOp::AppContainers => {
                    project_containers(options, &context).await
                }
                _ => Err(WorkerError::invalid(format!(
                    "operation '{}' is not supported by the worker",
                    op.as_str()
                ))),
            }
        }
    }
}

/// Validated paths for one managed application.
struct AppContext {
    slug: String,
    app_dir: PathBuf,
    compose_path: PathBuf,
    env_path: PathBuf,
    project: String,
}

impl AppContext {
    /// Verify the app directory and Compose file are safe to use.
    ///
    /// Checks in order: directory exists and is inside `apps_dir` (canonical
    /// paths, symlinks resolved), `compose.yaml` is a regular file (never a
    /// symlink) with a bounded size, the raw text passes the structural check,
    /// and the optional `.env` is a regular file as well.
    fn ensure_files(&self, options: &WorkerOptions) -> Result<(), WorkerError> {
        let apps_root = std::fs::canonicalize(&options.apps_dir).map_err(|error| {
            WorkerError::unavailable(format!(
                "applications directory {} is unavailable: {error}",
                options.apps_dir.display()
            ))
        })?;
        let app_dir = std::fs::canonicalize(&self.app_dir).map_err(|_| {
            WorkerError::not_found(format!(
                "application '{}' has no data directory yet",
                self.slug
            ))
        })?;
        if !app_dir.starts_with(&apps_root) || app_dir == apps_root {
            return Err(WorkerError::forbidden(
                "application directory escapes the applications root",
            ));
        }
        let compose_meta = std::fs::symlink_metadata(&self.compose_path).map_err(|_| {
            WorkerError::not_found(format!("application '{}' has no compose.yaml", self.slug))
        })?;
        if compose_meta.file_type().is_symlink() || !compose_meta.file_type().is_file() {
            return Err(WorkerError::invalid("compose.yaml must be a regular file"));
        }
        if compose_meta.len() > MAX_COMPOSE_READ {
            return Err(WorkerError::invalid(format!(
                "compose.yaml is too large ({} bytes)",
                compose_meta.len()
            )));
        }
        let text = std::fs::read_to_string(&self.compose_path)
            .map_err(|error| WorkerError::invalid(format!("cannot read compose.yaml: {error}")))?;
        compose::check_compose_text(&text, &self.slug).map_err(WorkerError::invalid)?;

        if self.env_path.exists() {
            let env_meta = std::fs::symlink_metadata(&self.env_path)
                .map_err(|error| WorkerError::invalid(format!("cannot stat .env: {error}")))?;
            if env_meta.file_type().is_symlink() || !env_meta.file_type().is_file() {
                return Err(WorkerError::invalid(".env must be a regular file"));
            }
        }
        Ok(())
    }
}

/// Validate the Compose file: raw checks, then `docker compose config --format
/// json`, then the normalized security policy.
async fn validate_compose(
    options: &WorkerOptions,
    context: &AppContext,
) -> Result<compose::ComposeSummary, WorkerError> {
    context.ensure_files(options)?;
    let mut args = compose_base_args(context);
    args.push(OsString::from("config"));
    args.push(OsString::from("--format"));
    args.push(OsString::from("json"));
    let output = run_docker(
        options,
        args,
        Some(&context.app_dir),
        options.quick_timeout,
        MAX_CMD_OUTPUT,
    )
    .await?;
    if !output.success {
        return Err(WorkerError::invalid(format!(
            "compose validation failed: {}",
            stderr_summary(&output)
        )));
    }
    let config: Value = serde_json::from_str(&output.stdout).map_err(|error| {
        WorkerError::invalid(format!(
            "docker compose config did not return JSON (is docker compose v2 installed?): {error}"
        ))
    })?;
    compose::validate_normalized_config(&config, &context.app_dir).map_err(WorkerError::invalid)
}

fn compose_base_args(context: &AppContext) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("compose"),
        OsString::from("-p"),
        OsString::from(&context.project),
        OsString::from("-f"),
        context.compose_path.clone().into_os_string(),
        OsString::from("--project-directory"),
        context.app_dir.clone().into_os_string(),
        OsString::from("--ansi"),
        OsString::from("never"),
    ];
    if context.env_path.exists() {
        args.push(OsString::from("--env-file"));
        args.push(context.env_path.clone().into_os_string());
    }
    args
}

async fn compose_exec(
    options: &WorkerOptions,
    context: &AppContext,
    subcommand: &[&str],
) -> Result<CommandOutput, WorkerError> {
    let mut args = compose_base_args(context);
    args.extend(subcommand.iter().map(OsString::from));
    run_docker(
        options,
        args,
        Some(&context.app_dir),
        options.compose_timeout,
        MAX_CMD_OUTPUT,
    )
    .await
}

async fn docker_version(options: &WorkerOptions) -> Result<Value, WorkerError> {
    let args = os_args(&["version", "--format", "{{.Server.Version}}"]);
    let output = run_docker(options, args, None, options.quick_timeout, 64 * 1024).await?;
    if !output.success {
        return Err(WorkerError::unavailable(format!(
            "docker daemon is not reachable: {}",
            stderr_summary(&output)
        )));
    }
    let version = output.stdout.trim().to_string();
    if version.is_empty() {
        return Err(WorkerError::unavailable(
            "docker daemon returned an empty version",
        ));
    }
    Ok(json!({"version": version, "protocolVersion": PROTOCOL_VERSION}))
}

async fn docker_info(options: &WorkerOptions) -> Result<Value, WorkerError> {
    let args = os_args(&["info", "--format", "{{json .}}"]);
    let output = run_docker(options, args, None, options.quick_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::unavailable(format!(
            "docker daemon is not reachable: {}",
            stderr_summary(&output)
        )));
    }
    let info: Value = serde_json::from_str(&output.stdout).map_err(|error| {
        WorkerError::command_failed(format!("cannot parse docker info output: {error}"))
    })?;
    let pick = |key: &str| info.get(key).cloned().unwrap_or(Value::Null);
    Ok(json!({
        "version": pick("ServerVersion"),
        "containers": pick("Containers"),
        "containersRunning": pick("ContainersRunning"),
        "containersStopped": pick("ContainersStopped"),
        "images": pick("Images"),
        "driver": pick("Driver"),
        "operatingSystem": pick("OperatingSystem"),
        "architecture": pick("Architecture"),
        "cpus": pick("NCPU"),
        "memoryTotalBytes": pick("MemTotal"),
    }))
}

async fn docker_ps_all(options: &WorkerOptions) -> Result<Value, WorkerError> {
    let containers = project_or_all_containers(options, None).await?;
    Ok(json!({"items": containers}))
}

async fn project_containers(
    options: &WorkerOptions,
    context: &AppContext,
) -> Result<Value, WorkerError> {
    let containers = project_or_all_containers(options, Some(&context.project)).await?;
    Ok(json!({
        "project": context.project,
        "items": containers,
    }))
}

async fn project_or_all_containers(
    options: &WorkerOptions,
    project: Option<&str>,
) -> Result<Vec<Value>, WorkerError> {
    let mut args = os_args(&["ps", "-a", "--no-trunc"]);
    if let Some(project) = project {
        args.push(OsString::from("--filter"));
        args.push(OsString::from(format!(
            "label=com.docker.compose.project={project}"
        )));
    }
    args.push(OsString::from("--format"));
    args.push(OsString::from("{{json .}}"));
    let output = run_docker(options, args, None, options.quick_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::unavailable(format!(
            "docker is not reachable: {}",
            stderr_summary(&output)
        )));
    }
    let mut items = parse_ps_json(&output.stdout)
        .map_err(|error| WorkerError::command_failed(format!("cannot parse docker ps: {error}")))?;
    items.truncate(MAX_CONTAINERS);
    Ok(items)
}

async fn docker_stats(options: &WorkerOptions) -> Result<Value, WorkerError> {
    let args = os_args(&[
        "stats",
        "--no-stream",
        "--no-trunc",
        "--format",
        "{{json .}}",
    ]);
    let output = run_docker(options, args, None, options.quick_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::unavailable(format!(
            "docker is not reachable: {}",
            stderr_summary(&output)
        )));
    }
    let mut items = parse_stats_json(&output.stdout).map_err(|error| {
        WorkerError::command_failed(format!("cannot parse docker stats: {error}"))
    })?;
    items.truncate(MAX_CONTAINERS);
    Ok(json!({"items": items}))
}

async fn docker_inspect(options: &WorkerOptions, ids: &[String]) -> Result<Value, WorkerError> {
    for id in ids {
        validate_container_id(id).map_err(WorkerError::invalid)?;
    }
    let mut args = os_args(&["inspect"]);
    args.extend(ids.iter().map(OsString::from));
    let output = run_docker(options, args, None, options.quick_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::not_found(format!(
            "cannot inspect containers: {}",
            stderr_summary(&output)
        )));
    }
    let items = parse_inspect_json(&output.stdout).map_err(|error| {
        WorkerError::command_failed(format!("cannot parse docker inspect: {error}"))
    })?;
    Ok(json!({"items": items}))
}

async fn docker_compose_logs(
    options: &WorkerOptions,
    context: &AppContext,
    tail: u32,
) -> Result<Value, WorkerError> {
    context.ensure_files(options)?;
    let tail = tail.clamp(1, MAX_LOG_TAIL).to_string();
    let output = compose_exec(
        options,
        context,
        &["logs", "--no-color", "--timestamps", "--tail", &tail],
    )
    .await?;
    if !output.success {
        return Err(WorkerError::command_failed(format!(
            "cannot read application logs: {}",
            stderr_summary(&output)
        )));
    }
    let mut lines = parse_compose_logs(&output.stdout);
    truncate_from_start(&mut lines, MAX_LOG_TAIL as usize);
    Ok(json!({"lines": lines, "tail": tail.parse::<u32>().unwrap_or(MAX_LOG_TAIL)}))
}

async fn remove_project_volumes(
    options: &WorkerOptions,
    context: &AppContext,
) -> Result<Value, WorkerError> {
    context.ensure_files(options)?;
    let args = os_args(&[
        "volume",
        "ls",
        "--filter",
        &format!("label=com.docker.compose.project={}", context.project),
        "--format",
        "{{.Name}}",
    ]);
    let output = run_docker(options, args, None, options.quick_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::unavailable(format!(
            "cannot list docker volumes: {}",
            stderr_summary(&output)
        )));
    }
    let mut names: Vec<String> = Vec::new();
    for name in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        if !valid_volume_name(name) {
            tracing::warn!(
                volume = name,
                "refusing to remove a volume with an unexpected name"
            );
            continue;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
        if names.len() >= MAX_VOLUMES {
            break;
        }
    }
    if names.is_empty() {
        return Ok(json!({"removed": []}));
    }
    let mut args = os_args(&["volume", "rm"]);
    args.extend(names.iter().map(OsString::from));
    let output = run_docker(options, args, None, options.compose_timeout, MAX_CMD_OUTPUT).await?;
    if !output.success {
        return Err(WorkerError::command_failed(format!(
            "cannot remove application volumes: {}",
            stderr_summary(&output)
        )));
    }
    Ok(json!({"removed": names}))
}

/// Docker volume names: conservative alphabet, no path separators.
fn valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn os_args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// Run one Docker CLI invocation without a shell.
async fn run_docker(
    options: &WorkerOptions,
    args: Vec<OsString>,
    workdir: Option<&Path>,
    limit: Duration,
    max_stdout: usize,
) -> Result<CommandOutput, WorkerError> {
    let mut command = Command::new(&options.docker_bin);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        // Compose/CLI environment variables that could redirect the project,
        // its files or its environment files are removed; explicit flags win.
        .env_remove("COMPOSE_FILE")
        .env_remove("COMPOSE_PROJECT_NAME")
        .env_remove("COMPOSE_PROFILES")
        .env_remove("COMPOSE_ENV_FILES")
        .env_remove("DOCKER_CONTEXT");
    if let Some(workdir) = workdir {
        command.current_dir(workdir);
    }
    let mut child = command.spawn().map_err(|error| {
        WorkerError::unavailable(format!(
            "cannot execute docker ({}): {error}",
            options.docker_bin.display()
        ))
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_capped(stdout, max_stdout));
    let stderr_task = tokio::spawn(read_capped(stderr, MAX_CMD_STDERR));

    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(WorkerError::command_failed(format!(
                "cannot wait for docker: {error}"
            )));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(WorkerError::new(
                "timeout",
                format!("docker command timed out after {limit:?}"),
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_task.await.unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

async fn read_capped<R>(reader: Option<R>, limit: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < limit {
                    let remaining = limit - out.len();
                    let take = n.min(remaining);
                    out.extend_from_slice(&buffer[..take]);
                }
            }
        }
    }
    out
}

fn ensure_success(what: &str, output: &CommandOutput) -> Result<(), WorkerError> {
    if output.success {
        Ok(())
    } else {
        Err(WorkerError::command_failed(format!(
            "{what} failed: {}",
            stderr_summary(output)
        )))
    }
}

/// Short, control-character-free summary of a failed Docker command.
fn stderr_summary(output: &CommandOutput) -> String {
    let raw = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if raw.is_empty() {
        return "docker exited with a non-zero status".to_string();
    }
    truncate_message(raw, 500)
}

fn truncate_from_start<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        let excess = items.len() - max;
        items.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixStream;

    struct TestEnv {
        _dir: tempfile::TempDir,
        options: WorkerOptions,
        socket: PathBuf,
        args_log: PathBuf,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        server: Option<tokio::task::JoinHandle<io::Result<()>>>,
    }

    impl TestEnv {
        async fn start(script: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let apps_dir = root.join("apps");
            std::fs::create_dir_all(apps_dir.join("shop")).unwrap();
            let script = script.replace("__APPS__", &apps_dir.display().to_string());
            std::fs::write(
                apps_dir.join("shop/compose.yaml"),
                "services:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"8080:80\"\n",
            )
            .unwrap();
            std::fs::write(apps_dir.join("shop/.env"), "TAG=alpine\n").unwrap();

            let args_log = root.join("docker-args.log");
            let script_path = root.join("fake-docker");
            let body = script.replace("__ARGS_LOG__", &args_log.display().to_string());
            std::fs::write(&script_path, body).unwrap();
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

            let socket = root.join("worker.sock");
            let options = WorkerOptions {
                socket_path: socket.clone(),
                apps_dir,
                docker_bin: script_path,
                quick_timeout: Duration::from_secs(5),
                compose_timeout: Duration::from_secs(10),
                max_concurrent: 2,
                allowed_peer_uid: None,
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let server_options = options.clone();
            let server = tokio::spawn(async move {
                serve(server_options, async move {
                    let _ = rx.await;
                })
                .await
            });
            wait_for_socket(&socket).await;
            Self {
                _dir: dir,
                options,
                socket,
                args_log,
                shutdown: Some(tx),
                server: Some(server),
            }
        }

        async fn call(&self, request: WorkerRequest) -> WorkerResponse {
            let line = serde_json::to_string(&request).unwrap();
            self.call_raw(&line).await
        }

        async fn call_raw(&self, line: &str) -> WorkerResponse {
            let mut stream = UnixStream::connect(&self.socket).await.unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            stream.flush().await.unwrap();
            let mut raw = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = stream.read(&mut byte).await.unwrap();
                if n == 0 || byte[0] == b'\n' {
                    break;
                }
                raw.push(byte[0]);
            }
            serde_json::from_slice(&raw).unwrap()
        }

        fn logged_args(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(&self.args_log)
                .unwrap_or_default()
                .lines()
                .map(|line| line.split('\u{1f}').map(str::to_string).collect())
                .collect()
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(server) = self.server.take() {
                server.abort();
            }
        }
    }

    async fn wait_for_socket(socket: &Path) {
        for _ in 0..200 {
            if UnixStream::connect(socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("worker socket {} did not appear", socket.display());
    }

    /// Fake docker CLI: logs its arguments (unit separator separated) to a file
    /// and prints canned output depending on the subcommand.
    const OK_SCRIPT: &str = r#"#!/bin/sh
log="__ARGS_LOG__"
first=""
sep=""
line=""
for arg in "$@"; do
  line="$line$sep$arg"
  sep=$(printf '\037')
  if [ -z "$first" ]; then first="$arg"; fi
done
printf '%s\n' "$line" >> "$log"
case "$1" in
  version) echo "26.1.4" ;;
  info) echo '{"ServerVersion":"26.1.4","Containers":2,"ContainersRunning":1,"ContainersStopped":1,"Images":4,"Driver":"overlay2","OperatingSystem":"Ubuntu","Architecture":"x86_64","NCPU":8,"MemTotal":16000000000}' ;;
  ps) echo '{"ID":"0123456789ab","Image":"nginx:alpine","Names":"shop-web-1","State":"running","Status":"Up 1 minute","Ports":"0.0.0.0:8080->80/tcp","Labels":"com.docker.compose.project=rast-shop,com.docker.compose.service=web"}' ;;
  stats) echo '{"BlockIO":"0B / 0B","CPUPerc":"1.50%","Container":"0123456789ab","ID":"0123456789ab","MemPerc":"0.50%","MemUsage":"10MiB / 1GiB","Name":"shop-web-1","NetIO":"1kB / 2kB","Pids":"3"}' ;;
  inspect) echo '[{"Id":"0123456789ab","Name":"/legacy","Config":{"Image":"nginx:alpine","Env":["A=1"],"Labels":{}},"State":{"Status":"running"},"HostConfig":{"RestartPolicy":{"Name":"always"}},"Mounts":[],"NetworkSettings":{"Ports":{}}}]' ;;
  volume) if [ "$2" = "ls" ]; then echo "rast-shop_data"; else echo "rast-shop_data"; fi ;;
  compose)
    sub=""
    for arg in "$@"; do
      case "$arg" in
        config|up|down|stop|restart|logs) sub="$arg" ;;
      esac
    done
    case "$sub" in
      config) echo '{"services":{"web":{"image":"nginx:alpine","volumes":[{"type":"bind","source":"__APPS__/shop/data/html","target":"/usr/share/nginx/html"}]}}}' ;;
      logs) printf 'shop-web-1  | 2026-11-10T13:24:02.123456789Z hello\nshop-web-1  | world\n' ;;
      *) echo "ok" ;;
    esac
    ;;
esac
exit 0
"#;

    const RISKY_CONFIG_SCRIPT: &str = r#"#!/bin/sh
log="__ARGS_LOG__"
line=""
sep=""
for arg in "$@"; do line="$line$sep$arg"; sep=$(printf '\037'); done
printf '%s\n' "$line" >> "$log"
case "$1" in
  compose) echo '{"services":{"web":{"image":"nginx:alpine","privileged":true,"volumes":[{"type":"bind","source":"/etc","target":"/host-etc"}]}}}' ;;
  *) echo "26.1.4" ;;
esac
exit 0
"#;

    #[tokio::test]
    async fn version_probe_is_answered() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env.call(WorkerRequest::new(WorkerOp::Version)).await;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.data.unwrap()["version"], "26.1.4");
    }

    #[tokio::test]
    async fn unknown_operation_and_extra_fields_are_rejected() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call_raw(r#"{"id":"1","op":"run-shell","slug":"shop"}"#)
            .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");

        let response = env
            .call_raw(r#"{"id":"2","op":"version","slug":"shop"}"#)
            .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
        assert!(env.logged_args().is_empty(), "docker must not be invoked");
    }

    #[tokio::test]
    async fn path_traversal_slugs_are_rejected_without_running_docker() {
        let env = TestEnv::start(OK_SCRIPT).await;
        for slug in ["../escape", "a/b", "/etc", "shop;rm"] {
            let response = env
                .call(WorkerRequest::new(WorkerOp::ComposeUp).slug(slug))
                .await;
            assert!(!response.ok, "{slug} should be rejected");
            assert_eq!(response.error.unwrap().code, "invalid_request");
        }
        assert!(env.logged_args().is_empty(), "docker must not be invoked");
    }

    #[tokio::test]
    async fn compose_up_validates_then_runs_structured_args() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("shop"))
            .await;
        assert!(response.ok, "{response:?}");

        let calls = env.logged_args();
        assert_eq!(calls.len(), 2, "expected config + up, got {calls:?}");
        let config = &calls[0];
        let up = &calls[1];
        for call in [config, up] {
            assert_eq!(call[0], "compose");
            assert!(call.contains(&"-p".to_string()));
            assert!(call.contains(&"rast-shop".to_string()));
            assert!(call.contains(&"-f".to_string()));
            assert!(call
                .iter()
                .any(|arg| arg.ends_with("/apps/shop/compose.yaml")));
            assert!(call.contains(&"--project-directory".to_string()));
            assert!(call.contains(&"--ansi".to_string()));
            assert!(call.contains(&"--env-file".to_string()));
        }
        assert!(config.contains(&"config".to_string()));
        assert!(config.contains(&"--format".to_string()));
        assert!(up.contains(&"up".to_string()));
        assert!(up.contains(&"-d".to_string()));
        assert!(up.contains(&"--remove-orphans".to_string()));
    }

    #[tokio::test]
    async fn compose_down_never_removes_volumes() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::ComposeDown).slug("shop"))
            .await;
        assert!(response.ok, "{response:?}");
        let calls = env.logged_args();
        assert_eq!(calls.len(), 1);
        let down = &calls[0];
        assert!(down.contains(&"down".to_string()));
        assert!(down.contains(&"--remove-orphans".to_string()));
        assert!(!down.contains(&"-v".to_string()));
        assert!(!down.iter().any(|arg| arg == "-v" || arg == "--volumes"));
    }

    #[tokio::test]
    async fn dangerous_normalized_config_is_rejected_before_up() {
        let env = TestEnv::start(RISKY_CONFIG_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("shop"))
            .await;
        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "invalid_request");
        assert!(
            error.message.contains("outside the app data directory")
                || error.message.contains("privileged"),
            "{}",
            error.message
        );
        // Only the config call was made; `up` must never run.
        let calls = env.logged_args();
        assert_eq!(calls.len(), 1, "{calls:?}");
    }

    #[tokio::test]
    async fn missing_application_is_not_found() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("ghost"))
            .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn ps_stats_and_inspect_are_mapped() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let ps = env.call(WorkerRequest::new(WorkerOp::PsAll)).await;
        assert!(ps.ok);
        let items = ps.data.unwrap()["items"].as_array().unwrap().clone();
        assert_eq!(items[0]["name"], "shop-web-1");
        assert_eq!(items[0]["ports"][0]["hostPort"], 8080);

        let stats = env.call(WorkerRequest::new(WorkerOp::Stats)).await;
        assert!(stats.ok, "{stats:?}");
        let data = stats.data.unwrap();
        assert!((data["items"][0]["cpuPercent"].as_f64().unwrap() - 1.5).abs() < 1e-6);

        let inspect = env
            .call(WorkerRequest::new(WorkerOp::Inspect).ids(vec!["0123456789ab".into()]))
            .await;
        assert!(inspect.ok, "{inspect:?}");
        assert_eq!(inspect.data.unwrap()["items"][0]["name"], "legacy");
    }

    #[tokio::test]
    async fn logs_are_tail_bounded() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::Logs).slug("shop").tail(9999))
            .await;
        assert!(response.ok, "{response:?}");
        let data = response.data.unwrap();
        assert_eq!(data["tail"], 2000);
        let lines = data["lines"].as_array().unwrap();
        assert_eq!(lines[0]["ts"], 1_794_317_042_123i64);
        let calls = env.logged_args();
        assert!(calls[0].contains(&"--tail".to_string()));
        assert!(calls[0].contains(&"2000".to_string()));
    }

    #[tokio::test]
    async fn volume_removal_is_project_scoped() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env
            .call(WorkerRequest::new(WorkerOp::RemoveVolumes).slug("shop"))
            .await;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.data.unwrap()["removed"][0], "rast-shop_data");
        let calls = env.logged_args();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains(&"volume".to_string()));
        assert!(calls[0].contains(&"label=com.docker.compose.project=rast-shop".to_string()));
        assert!(calls[1].contains(&"rm".to_string()));
        assert!(calls[1].contains(&"rast-shop_data".to_string()));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_but_connection_protocol_survives() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let response = env.call_raw("this is not json").await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn request_size_limit_is_enforced() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let huge = format!(
            r#"{{"id":"{}","op":"version"}}"#,
            "a".repeat(MAX_LINE_BYTES + 10)
        );
        let response = env.call_raw(&huge).await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn compose_file_symlinks_are_rejected() {
        let env = TestEnv::start(OK_SCRIPT).await;
        let app_dir = env.options.apps_dir.join("shop");
        std::fs::remove_file(app_dir.join("compose.yaml")).unwrap();
        std::fs::write(
            app_dir.join("real.yaml"),
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(app_dir.join("real.yaml"), app_dir.join("compose.yaml"))
            .unwrap();
        let response = env
            .call(WorkerRequest::new(WorkerOp::ComposeUp).slug("shop"))
            .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn socket_helpers_and_volume_names() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nested/worker.sock");
        prepare_socket_path(&socket).unwrap();
        assert!(!socket.exists());
        // A non-socket file in the way is never deleted.
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let blocker = dir.path().join("blocker.sock");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(prepare_socket_path(&blocker).is_err());

        assert!(valid_volume_name("rast-shop_data"));
        assert!(valid_volume_name("a.b-c_1"));
        assert!(!valid_volume_name(""));
        assert!(!valid_volume_name("../escape"));
        assert!(!valid_volume_name("a/b"));
    }

    #[test]
    fn group_resolution_accepts_numeric_and_names() {
        assert_eq!(resolve_group_id("0"), Some(0));
        let _ = resolve_group_id("root");
        assert_eq!(resolve_group_id("definitely-not-a-group-xyz"), None);
    }

    #[test]
    fn os_args_round_trip() {
        let args = os_args(&["a", "b"]);
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], OsString::from("b"));
        let _ = OsStr::new("x");
        let mut items = vec![1, 2, 3, 4];
        truncate_from_start(&mut items, 2);
        assert_eq!(items, vec![3, 4]);
    }
}
