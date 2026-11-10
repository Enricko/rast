//! Wire protocol between the unprivileged web server and the privileged
//! Docker worker (`rast worker`).
//!
//! The protocol is JSON-lines over a Unix socket: one request object per line,
//! one response object per line. Every request carries an opaque `id` that the
//! response echoes, which makes the client resilient to stale/reordered data.
//!
//! Security model: the web server can only *ask* the worker to perform one of
//! the whitelisted [`WorkerOp`]s. Arguments are structured (no shell strings)
//! and are re-validated by the worker; slugs and container ids are validated
//! here *and* there so a compromised/buggy client cannot smuggle paths into
//! the worker.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the worker protocol; bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted length of a single JSON line (request or response).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of containers accepted by the `inspect` op.
pub const MAX_CONTAINER_IDS: usize = 64;

/// Maximum accepted log tail (also bounds the response size).
pub const MAX_LOG_TAIL: u32 = 2000;

/// Default log tail when the client does not specify one.
pub const DEFAULT_LOG_TAIL: u32 = 200;

/// Longest accepted application slug.
pub const MAX_SLUG_LEN: usize = 40;

/// Timeout for cheap Docker queries (version/info/ps/stats/inspect).
pub const QUICK_OP_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for Compose operations (up may need to pull images).
pub const COMPOSE_OP_TIMEOUT: Duration = Duration::from_secs(180);

/// Operations the worker is willing to perform. Anything else is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerOp {
    /// `docker version` — used for the availability probe.
    Version,
    /// `docker system info` — daemon summary.
    Info,
    /// `docker ps -a` for every container on the host.
    PsAll,
    /// Containers belonging to one managed Compose project.
    Ps,
    /// Containers belonging to one managed Compose project (alias with
    /// explicit project-label filtering; used for status refreshes).
    AppContainers,
    /// `docker compose config` sanity check without touching the daemon.
    ComposeValidate,
    /// `docker compose up -d --remove-orphans`.
    ComposeUp,
    /// `docker compose down --remove-orphans` (never `-v`).
    ComposeDown,
    /// `docker compose stop`.
    ComposeStop,
    /// `docker compose restart`.
    ComposeRestart,
    /// Remove the project's named volumes explicitly (after a `delete` with
    /// `removeVolumes=true`). This is the only way volumes are ever removed.
    RemoveVolumes,
    /// `docker compose logs --tail N`.
    Logs,
    /// `docker stats --no-stream` for all containers.
    Stats,
    /// `docker inspect` for the given container ids (subset of fields).
    Inspect,
}

impl WorkerOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Info => "info",
            Self::PsAll => "ps-all",
            Self::Ps => "ps",
            Self::AppContainers => "app-containers",
            Self::ComposeValidate => "compose-validate",
            Self::ComposeUp => "compose-up",
            Self::ComposeDown => "compose-down",
            Self::ComposeStop => "compose-stop",
            Self::ComposeRestart => "compose-restart",
            Self::RemoveVolumes => "remove-volumes",
            Self::Logs => "logs",
            Self::Stats => "stats",
            Self::Inspect => "inspect",
        }
    }

    /// Client-side deadline for this operation.
    pub fn timeout(self) -> Duration {
        match self {
            Self::Version
            | Self::Info
            | Self::PsAll
            | Self::Ps
            | Self::AppContainers
            | Self::ComposeValidate
            | Self::Stats
            | Self::Inspect
            | Self::RemoveVolumes => QUICK_OP_TIMEOUT,
            Self::ComposeUp
            | Self::ComposeDown
            | Self::ComposeStop
            | Self::ComposeRestart
            | Self::Logs => COMPOSE_OP_TIMEOUT,
        }
    }

    /// Operations that act on one managed application.
    pub fn is_app_op(self) -> bool {
        matches!(
            self,
            Self::Ps
                | Self::AppContainers
                | Self::ComposeValidate
                | Self::ComposeUp
                | Self::ComposeDown
                | Self::ComposeStop
                | Self::ComposeRestart
                | Self::RemoveVolumes
                | Self::Logs
        )
    }
}

/// One worker request. Unknown fields are rejected so callers cannot stuff
/// additional arguments into the request hoping a future worker honours them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub id: String,
    pub op: WorkerOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
}

impl WorkerRequest {
    pub fn new(op: WorkerOp) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            op,
            slug: None,
            tail: None,
            ids: None,
        }
    }

    pub fn slug(mut self, slug: impl Into<String>) -> Self {
        self.slug = Some(slug.into());
        self
    }

    pub fn tail(mut self, tail: u32) -> Self {
        self.tail = Some(tail);
        self
    }

    pub fn ids(mut self, ids: Vec<String>) -> Self {
        self.ids = Some(ids);
        self
    }
}

/// Worker response for one request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkerErrorBody>,
}

impl WorkerResponse {
    pub fn success(id: impl Into<String>, data: Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(id: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(WorkerErrorBody {
                code: code.into(),
                message: truncate_message(&message.into(), 500),
            }),
        }
    }
}

/// Error payload returned by the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerErrorBody {
    pub code: String,
    pub message: String,
}

/// Validated worker action: all filesystem paths the worker may touch are
/// derived here, never taken from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedAction {
    /// No application context (version/info/ps-all/stats).
    Plain(WorkerOp),
    /// One managed application directory.
    App {
        op: WorkerOp,
        slug: String,
        app_dir: PathBuf,
        compose_path: PathBuf,
        env_path: PathBuf,
        project: String,
        tail: Option<u32>,
    },
    /// Container inspection by id.
    Inspect(Vec<String>),
}

impl ValidatedAction {
    pub fn op(&self) -> WorkerOp {
        match self {
            Self::Plain(op) => *op,
            Self::App { op, .. } => *op,
            Self::Inspect(_) => WorkerOp::Inspect,
        }
    }
}

/// Validate a request coming over the socket. Returns a human-readable reason
/// when the request is not acceptable; the worker turns that into
/// `invalid_request`.
pub fn validate_request(req: &WorkerRequest, apps_dir: &Path) -> Result<ValidatedAction, String> {
    if req.id.is_empty() || req.id.len() > 128 {
        return Err("request id must be 1-128 characters".into());
    }
    match req.op {
        WorkerOp::Version | WorkerOp::Info | WorkerOp::PsAll | WorkerOp::Stats => {
            reject_extra_args(req)?;
            Ok(ValidatedAction::Plain(req.op))
        }
        WorkerOp::Inspect => {
            if req.slug.is_some() || req.tail.is_some() {
                return Err("inspect accepts only `ids`".into());
            }
            let ids = req
                .ids
                .as_ref()
                .ok_or_else(|| "inspect requires `ids`".to_string())?;
            if ids.is_empty() {
                return Err("inspect requires at least one container id".into());
            }
            if ids.len() > MAX_CONTAINER_IDS {
                return Err(format!(
                    "inspect accepts at most {MAX_CONTAINER_IDS} container ids"
                ));
            }
            for id in ids {
                validate_container_id(id)?;
            }
            Ok(ValidatedAction::Inspect(ids.clone()))
        }
        op if op.is_app_op() => {
            if req.ids.is_some() {
                return Err(format!("{} does not accept container ids", op.as_str()));
            }
            let slug = req
                .slug
                .as_deref()
                .ok_or_else(|| format!("{} requires `slug`", op.as_str()))?;
            validate_slug(slug)?;
            let tail = if op == WorkerOp::Logs {
                Some(req.tail.unwrap_or(DEFAULT_LOG_TAIL).clamp(1, MAX_LOG_TAIL))
            } else {
                if req.tail.is_some() {
                    return Err(format!("{} does not accept `tail`", op.as_str()));
                }
                None
            };
            Ok(ValidatedAction::App {
                op,
                slug: slug.to_string(),
                app_dir: apps_dir.join(slug),
                compose_path: apps_dir.join(slug).join("compose.yaml"),
                env_path: apps_dir.join(slug).join(".env"),
                project: project_name(slug),
                tail,
            })
        }
        other => Err(format!("operation '{}' is not supported", other.as_str())),
    }
}

fn reject_extra_args(req: &WorkerRequest) -> Result<(), String> {
    if req.slug.is_some() || req.tail.is_some() || req.ids.is_some() {
        return Err(format!("{} accepts no arguments", req.op.as_str()));
    }
    Ok(())
}

/// Docker Compose project name for a managed app: always `rast-<slug>`.
pub fn project_name(slug: &str) -> String {
    format!("rast-{slug}")
}

/// Application slugs are generated server-side and must stay in a tight
/// alphabet: lowercase alphanumerics with interior dashes only.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() || slug.len() > MAX_SLUG_LEN {
        return Err(format!(
            "invalid app slug: expected 1-{MAX_SLUG_LEN} characters"
        ));
    }
    let bytes = slug.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err("invalid app slug: must start with a lowercase letter or digit".into());
    }
    let last = bytes[bytes.len() - 1];
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err("invalid app slug: must end with a lowercase letter or digit".into());
    }
    for b in bytes {
        if !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-') {
            return Err("invalid app slug: only lowercase letters, digits and '-' are allowed".into());
        }
    }
    Ok(())
}

/// Docker container ids are hex strings (short ids are 12 chars, full ids 64).
pub fn validate_container_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err("invalid container id: expected 1-64 hexadecimal characters".into());
    }
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid container id: expected hexadecimal characters".into());
    }
    Ok(())
}

/// Turn an internal image/env value into a single safe log line: strip control
/// characters and truncate on a UTF-8 boundary.
pub fn truncate_message(value: &str, max: usize) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    if cleaned.len() <= max {
        return cleaned;
    }
    let mut cut = max;
    while cut > 0 && !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &cleaned[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps_dir() -> PathBuf {
        PathBuf::from("/var/lib/rast/apps")
    }

    #[test]
    fn slugs_are_strict() {
        for ok in ["a", "my-app", "app1", "a-b-c", "1app"] {
            assert!(validate_slug(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "-app",
            "app-",
            "App",
            "app_name",
            "app.name",
            "app/../x",
            "app/x",
            "app\\x",
            "app\0x",
            &"a".repeat(MAX_SLUG_LEN + 1),
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn container_ids_are_hex() {
        assert!(validate_container_id("0123456789ab").is_ok());
        assert!(validate_container_id(&"abcdef0123456789".repeat(4)).is_ok());
        for bad in ["", "xyz", "abc\n", "../x", &"a".repeat(65)] {
            assert!(validate_container_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn paths_are_derived_from_slug_never_from_client() {
        let req = WorkerRequest::new(WorkerOp::ComposeUp).slug("shop");
        let action = validate_request(&req, &apps_dir()).unwrap();
        match action {
            ValidatedAction::App {
                app_dir,
                compose_path,
                env_path,
                project,
                tail,
                ..
            } => {
                assert_eq!(app_dir, PathBuf::from("/var/lib/rast/apps/shop"));
                assert_eq!(
                    compose_path,
                    PathBuf::from("/var/lib/rast/apps/shop/compose.yaml")
                );
                assert_eq!(env_path, PathBuf::from("/var/lib/rast/apps/shop/.env"));
                assert_eq!(project, "rast-shop");
                assert!(tail.is_none());
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn traversal_slugs_are_rejected() {
        for slug in ["../escape", "a/../b", "/abs", "..", "a/b", "a\\b"] {
            let req = WorkerRequest::new(WorkerOp::ComposeUp).slug(slug);
            assert!(validate_request(&req, &apps_dir()).is_err(), "{slug}");
        }
    }

    #[test]
    fn unknown_or_extra_arguments_are_rejected() {
        // Nonexistent ops never deserialize.
        let raw = r#"{"id":"1","op":"shell","slug":"x"}"#;
        assert!(serde_json::from_str::<WorkerRequest>(raw).is_err());
        let raw = r#"{"id":"1","op":"please-run","cmd":"id"}"#;
        assert!(serde_json::from_str::<WorkerRequest>(raw).is_err());

        let req = WorkerRequest::new(WorkerOp::Version).slug("x");
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::Stats).tail(10);
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::ComposeUp).slug("x").tail(5);
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::Inspect).slug("x");
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::Inspect).ids(vec![]);
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::Inspect).ids(vec!["nope".into()]);
        assert!(validate_request(&req, &apps_dir()).is_err());
    }

    #[test]
    fn log_tail_is_clamped() {
        let req = WorkerRequest::new(WorkerOp::Logs).slug("x").tail(0);
        match validate_request(&req, &apps_dir()).unwrap() {
            ValidatedAction::App { tail, .. } => assert_eq!(tail, Some(1)),
            other => panic!("{other:?}"),
        }
        let req = WorkerRequest::new(WorkerOp::Logs)
            .slug("x")
            .tail(u32::MAX);
        match validate_request(&req, &apps_dir()).unwrap() {
            ValidatedAction::App { tail, .. } => assert_eq!(tail, Some(MAX_LOG_TAIL)),
            other => panic!("{other:?}"),
        }
        let req = WorkerRequest::new(WorkerOp::Logs).slug("x");
        match validate_request(&req, &apps_dir()).unwrap() {
            ValidatedAction::App { tail, .. } => assert_eq!(tail, Some(DEFAULT_LOG_TAIL)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn request_roundtrips_and_rejects_unknown_fields() {
        let req = WorkerRequest::new(WorkerOp::Logs).slug("app").tail(50);
        let line = serde_json::to_string(&req).unwrap();
        let parsed: WorkerRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.op, WorkerOp::Logs);
        assert_eq!(parsed.slug.as_deref(), Some("app"));
        assert!(serde_json::from_str::<WorkerRequest>(
            r#"{"id":"1","op":"logs","slug":"a","unexpected":true}"#
        )
        .is_err());
        assert_eq!(
            serde_json::to_string(&WorkerOp::ComposeUp).unwrap(),
            "\"compose-up\""
        );
    }

    #[test]
    fn response_roundtrip() {
        let ok = WorkerResponse::success("abc", serde_json::json!({"v": 1}));
        let back: WorkerResponse = serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert!(back.ok);
        assert_eq!(back.data.unwrap()["v"], 1);
        assert!(back.error.is_none());

        let err = WorkerResponse::failure("abc", "command_failed", "boom\nline2");
        let back: WorkerResponse = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_ref().unwrap().code, "command_failed");
        assert!(back.error.unwrap().message.contains("boom"));
    }

    #[test]
    fn message_truncation_is_utf8_safe() {
        assert_eq!(truncate_message("hello", 10), "hello");
        let long = "日本語テキスト";
        let cut = truncate_message(long, 8);
        assert!(cut.len() <= 8 + '…'.len_utf8());
        assert_eq!(truncate_message("a\u{0}b", 10), "ab");
    }
}
