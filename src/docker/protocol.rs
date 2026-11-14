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

/// Default tail for incremental (`compose-logs-since`) log reads. Also the
/// upper bound used by the live log stream's bounded ring buffer.
pub const DEFAULT_LOGS_SINCE_TAIL: u32 = 500;

/// How far into the future a `sinceSecs` cursor may point (clock tolerance).
pub const MAX_SINCE_SKEW_SECS: i64 = 86_400;

/// Longest accepted application slug.
pub const MAX_SLUG_LEN: usize = 40;

/// Timeout for cheap Docker queries (version/info/ps/stats/inspect).
pub const QUICK_OP_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for Compose operations (up may need to pull images).
pub const COMPOSE_OP_TIMEOUT: Duration = Duration::from_secs(180);
/// Timeout for `docker pull` (large images over slow links).
pub const PULL_OP_TIMEOUT: Duration = Duration::from_secs(1800);

/// Longest accepted image reference (`registry/path:tag@sha256:…`).
pub const MAX_IMAGE_REF_LEN: usize = 255;
/// Longest accepted volume name.
pub const MAX_VOLUME_NAME_LEN: usize = 255;

/// How long a worker operation may take, by kind. The client and the worker
/// map each class onto their own configured deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutClass {
    /// Cheap reads.
    Quick,
    /// Lifecycle changes that may wait for containers to stop.
    Long,
    /// Image downloads.
    Pull,
}

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
    /// Incremental `docker compose logs --since <ts> --tail N` used by the live
    /// log stream. Structured numeric cursor only (no free-form timestamp).
    ComposeLogsSince,
    /// `docker stats --no-stream` for all containers.
    Stats,
    /// `docker inspect` for the given container ids (subset of fields).
    Inspect,
    /// `docker start` for the given container ids.
    ContainerStart,
    /// `docker stop` for the given container ids.
    ContainerStop,
    /// `docker restart` for the given container ids.
    ContainerRestart,
    /// `docker rm -f` for the given container ids (never `-v`: volumes stay).
    ContainerRemove,
    /// `docker logs --timestamps --tail N [--since …]` for one container.
    ContainerLogs,
    /// `docker image ls -a`.
    ImageList,
    /// `docker image inspect <image>`: exposed ports, volumes and defaults
    /// used to prefill "Run image".
    ImageInspect,
    /// `docker pull <image>`.
    ImagePull,
    /// `docker image rm <image>` (never `--force`).
    ImageRemove,
    /// `docker image prune -f`: untagged images no container uses.
    ImagePrune,
    /// `docker volume ls`.
    VolumeList,
    /// `docker volume rm <volume>` (never `--force`; Docker refuses volumes
    /// that a container still uses).
    VolumeRemove,
    /// Interactive shell in one running container. After the success line
    /// the connection carries terminal frames ([`super::exec`]).
    ContainerExec,
    /// Build a Linux app image ([`super::linux`]).
    LinuxBuild,
    /// Start one person's copy of a Linux app.
    LinuxRun,
    /// Stop and remove one person's copy of a Linux app (its data stays).
    LinuxStop,
    /// Remove a Linux app's containers and images (and data on request).
    LinuxRemove,
    /// Remove a Linux app's older images.
    LinuxPrune,
    /// Containers of every Linux app.
    LinuxList,
    /// Ask a `.deb` app's apt sources for a newer version.
    LinuxCheck,
    /// List a legacy Linux app's per-person data volumes and their owners.
    LinuxVolumeList,
    /// Copy one person's legacy Docker data volume into a host directory
    /// (Docker -> native migration; read-only on the volume).
    LinuxVolumeExport,
    /// Remove exactly one person's legacy Docker data volume (post-migration
    /// cleanup only; the server keeps it until the operator asks).
    LinuxVolumeRemove,
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
            Self::ComposeLogsSince => "compose-logs-since",
            Self::Stats => "stats",
            Self::Inspect => "inspect",
            Self::ContainerStart => "container-start",
            Self::ContainerStop => "container-stop",
            Self::ContainerRestart => "container-restart",
            Self::ContainerRemove => "container-remove",
            Self::ContainerLogs => "container-logs",
            Self::ImageList => "image-list",
            Self::ImageInspect => "image-inspect",
            Self::ImagePull => "image-pull",
            Self::ImageRemove => "image-remove",
            Self::ImagePrune => "image-prune",
            Self::VolumeList => "volume-list",
            Self::VolumeRemove => "volume-remove",
            Self::ContainerExec => "container-exec",
            Self::LinuxBuild => "linux-build",
            Self::LinuxRun => "linux-run",
            Self::LinuxStop => "linux-stop",
            Self::LinuxRemove => "linux-remove",
            Self::LinuxPrune => "linux-prune",
            Self::LinuxList => "linux-list",
            Self::LinuxCheck => "linux-check",
            Self::LinuxVolumeList => "linux-volume-list",
            Self::LinuxVolumeExport => "linux-volume-export",
            Self::LinuxVolumeRemove => "linux-volume-remove",
        }
    }

    /// Which deadline applies to this operation.
    pub fn timeout_class(self) -> TimeoutClass {
        match self {
            Self::Version
            | Self::Info
            | Self::PsAll
            | Self::Ps
            | Self::AppContainers
            | Self::ComposeValidate
            | Self::Stats
            | Self::Inspect
            | Self::RemoveVolumes
            | Self::ComposeLogsSince
            | Self::ContainerLogs
            | Self::ImageList
            | Self::ImageInspect
            | Self::VolumeList
            | Self::LinuxList
            | Self::LinuxVolumeList
            | Self::ContainerExec => TimeoutClass::Quick,
            Self::ComposeUp
            | Self::ComposeDown
            | Self::ComposeStop
            | Self::ComposeRestart
            | Self::Logs
            | Self::ContainerStart
            | Self::ContainerStop
            | Self::ContainerRestart
            | Self::ContainerRemove
            | Self::ImageRemove
            | Self::ImagePrune
            | Self::VolumeRemove
            | Self::LinuxRun
            | Self::LinuxStop
            | Self::LinuxRemove
            | Self::LinuxPrune
            | Self::LinuxCheck
            | Self::LinuxVolumeExport
            | Self::LinuxVolumeRemove => TimeoutClass::Long,
            Self::ImagePull | Self::LinuxBuild => TimeoutClass::Pull,
        }
    }

    /// Default deadline for this operation.
    pub fn timeout(self) -> Duration {
        match self.timeout_class() {
            TimeoutClass::Quick => QUICK_OP_TIMEOUT,
            TimeoutClass::Long => COMPOSE_OP_TIMEOUT,
            TimeoutClass::Pull => PULL_OP_TIMEOUT,
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
                | Self::ComposeLogsSince
        )
    }

    /// Operations on Linux desktop apps ([`super::linux`]).
    pub fn is_linux_op(self) -> bool {
        matches!(
            self,
            Self::LinuxBuild
                | Self::LinuxRun
                | Self::LinuxStop
                | Self::LinuxRemove
                | Self::LinuxPrune
                | Self::LinuxList
                | Self::LinuxCheck
                | Self::LinuxVolumeList
                | Self::LinuxVolumeExport
                | Self::LinuxVolumeRemove
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
    /// Incremental log cursor: Unix seconds, validated and converted to an
    /// RFC3339 `--since` argument by the worker. Never a shell string.
    #[serde(default, rename = "sinceSecs", skip_serializing_if = "Option::is_none")]
    pub since_secs: Option<i64>,
    /// Image reference or image id (image ops only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Volume name (volume ops only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// Terminal width in columns (`container-exec` only; clamped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// Terminal height in rows (`container-exec` only; clamped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Arguments of the `linux-*` operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linux: Option<super::linux::LinuxArgs>,
}

impl WorkerRequest {
    pub fn new(op: WorkerOp) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            op,
            slug: None,
            tail: None,
            ids: None,
            since_secs: None,
            image: None,
            volume: None,
            cols: None,
            rows: None,
            linux: None,
        }
    }

    pub fn linux(mut self, args: super::linux::LinuxArgs) -> Self {
        self.linux = Some(args);
        self
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

    pub fn since_secs(mut self, since_secs: i64) -> Self {
        self.since_secs = Some(since_secs);
        self
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn volume(mut self, volume: impl Into<String>) -> Self {
        self.volume = Some(volume.into());
        self
    }

    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = Some(cols);
        self.rows = Some(rows);
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

    pub fn failure(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
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
        /// Only set for `compose-logs-since`: Unix seconds cursor.
        since_secs: Option<i64>,
    },
    /// Container inspection by id.
    Inspect(Vec<String>),
    /// Start/stop/restart/remove of validated container ids.
    Containers { op: WorkerOp, ids: Vec<String> },
    /// Logs of one container.
    ContainerLogs {
        id: String,
        tail: u32,
        since_secs: Option<i64>,
    },
    /// Pull/remove of one validated image reference or id.
    Image { op: WorkerOp, reference: String },
    /// Removal of one validated volume name.
    Volume { op: WorkerOp, name: String },
    /// Terminal into one container, with a clamped initial size.
    Exec { id: String, cols: u16, rows: u16 },
    /// One of the `linux-*` operations.
    Linux(super::linux::LinuxAction),
}

impl ValidatedAction {
    pub fn op(&self) -> WorkerOp {
        match self {
            Self::Plain(op) => *op,
            Self::App { op, .. } => *op,
            Self::Inspect(_) => WorkerOp::Inspect,
            Self::Containers { op, .. } | Self::Image { op, .. } | Self::Volume { op, .. } => *op,
            Self::ContainerLogs { .. } => WorkerOp::ContainerLogs,
            Self::Exec { .. } => WorkerOp::ContainerExec,
            Self::Linux(action) => match action {
                super::linux::LinuxAction::Build { .. } => WorkerOp::LinuxBuild,
                super::linux::LinuxAction::Run(_) => WorkerOp::LinuxRun,
                super::linux::LinuxAction::Stop { .. } => WorkerOp::LinuxStop,
                super::linux::LinuxAction::Remove { .. } => WorkerOp::LinuxRemove,
                super::linux::LinuxAction::Prune { .. } => WorkerOp::LinuxPrune,
                super::linux::LinuxAction::List => WorkerOp::LinuxList,
                super::linux::LinuxAction::Check { .. } => WorkerOp::LinuxCheck,
                super::linux::LinuxAction::VolumeList { .. } => WorkerOp::LinuxVolumeList,
                super::linux::LinuxAction::VolumeExport { .. } => WorkerOp::LinuxVolumeExport,
                super::linux::LinuxAction::VolumeRemove { .. } => WorkerOp::LinuxVolumeRemove,
            },
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
    if req.op.is_linux_op() {
        accept_only(req, &[])?;
        return super::linux::validate(req.op, req.linux.as_ref()).map(ValidatedAction::Linux);
    }
    if req.linux.is_some() {
        return Err(format!("{} does not accept `linux`", req.op.as_str()));
    }
    match req.op {
        WorkerOp::Version
        | WorkerOp::Info
        | WorkerOp::PsAll
        | WorkerOp::Stats
        | WorkerOp::ImageList
        | WorkerOp::ImagePrune
        | WorkerOp::VolumeList => {
            reject_extra_args(req)?;
            Ok(ValidatedAction::Plain(req.op))
        }
        WorkerOp::ContainerStart
        | WorkerOp::ContainerStop
        | WorkerOp::ContainerRestart
        | WorkerOp::ContainerRemove => {
            accept_only(req, &["ids"])?;
            let ids = validated_ids(req)?;
            Ok(ValidatedAction::Containers { op: req.op, ids })
        }
        WorkerOp::ContainerLogs => {
            accept_only(req, &["ids", "tail", "sinceSecs"])?;
            let ids = validated_ids(req)?;
            let [id] = <[String; 1]>::try_from(ids)
                .map_err(|_| "container-logs accepts exactly one container id".to_string())?;
            let tail = req.tail.unwrap_or(DEFAULT_LOG_TAIL).clamp(1, MAX_LOG_TAIL);
            let since_secs = validated_since(req.since_secs)?;
            Ok(ValidatedAction::ContainerLogs {
                id,
                tail,
                since_secs,
            })
        }
        WorkerOp::ContainerExec => {
            accept_only(req, &["ids", "cols", "rows"])?;
            let ids = validated_ids(req)?;
            let [id] = <[String; 1]>::try_from(ids)
                .map_err(|_| "container-exec accepts exactly one container id".to_string())?;
            let (cols, rows) = super::exec::clamp_size(req.cols, req.rows);
            Ok(ValidatedAction::Exec { id, cols, rows })
        }
        WorkerOp::ImagePull | WorkerOp::ImageRemove | WorkerOp::ImageInspect => {
            accept_only(req, &["image"])?;
            let reference = req
                .image
                .as_deref()
                .ok_or_else(|| format!("{} requires `image`", req.op.as_str()))?;
            validate_image_ref(reference)?;
            Ok(ValidatedAction::Image {
                op: req.op,
                reference: reference.to_string(),
            })
        }
        WorkerOp::VolumeRemove => {
            accept_only(req, &["volume"])?;
            let name = req
                .volume
                .as_deref()
                .ok_or_else(|| "volume-remove requires `volume`".to_string())?;
            validate_volume_name(name)?;
            Ok(ValidatedAction::Volume {
                op: req.op,
                name: name.to_string(),
            })
        }
        WorkerOp::Inspect => {
            accept_only(req, &["ids"])?;
            Ok(ValidatedAction::Inspect(validated_ids(req)?))
        }
        op if op.is_app_op() => {
            if req.ids.is_some() {
                return Err(format!("{} does not accept container ids", op.as_str()));
            }
            accept_only(req, &["slug", "tail", "sinceSecs"])?;
            let slug = req
                .slug
                .as_deref()
                .ok_or_else(|| format!("{} requires `slug`", op.as_str()))?;
            validate_slug(slug)?;
            let tail = match op {
                WorkerOp::Logs => Some(req.tail.unwrap_or(DEFAULT_LOG_TAIL).clamp(1, MAX_LOG_TAIL)),
                WorkerOp::ComposeLogsSince => Some(
                    req.tail
                        .unwrap_or(DEFAULT_LOGS_SINCE_TAIL)
                        .clamp(1, MAX_LOG_TAIL),
                ),
                _ => {
                    if req.tail.is_some() {
                        return Err(format!("{} does not accept `tail`", op.as_str()));
                    }
                    None
                }
            };
            let since_secs = if op == WorkerOp::ComposeLogsSince {
                validated_since(req.since_secs)?
            } else {
                if req.since_secs.is_some() {
                    return Err(format!("{} does not accept `sinceSecs`", op.as_str()));
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
                since_secs,
            })
        }
        other => Err(format!("operation '{}' is not supported", other.as_str())),
    }
}

fn reject_extra_args(req: &WorkerRequest) -> Result<(), String> {
    if req.slug.is_some()
        || req.tail.is_some()
        || req.ids.is_some()
        || req.since_secs.is_some()
        || req.image.is_some()
        || req.volume.is_some()
        || req.cols.is_some()
        || req.rows.is_some()
    {
        return Err(format!("{} accepts no arguments", req.op.as_str()));
    }
    Ok(())
}

/// Refuse every argument except the named ones.
fn accept_only(req: &WorkerRequest, allowed: &[&str]) -> Result<(), String> {
    let present = [
        ("slug", req.slug.is_some()),
        ("tail", req.tail.is_some()),
        ("ids", req.ids.is_some()),
        ("sinceSecs", req.since_secs.is_some()),
        ("image", req.image.is_some()),
        ("volume", req.volume.is_some()),
        ("cols", req.cols.is_some()),
        ("rows", req.rows.is_some()),
    ];
    for (name, set) in present {
        if set && !allowed.contains(&name) {
            return Err(format!("{} does not accept `{name}`", req.op.as_str()));
        }
    }
    Ok(())
}

/// 1..=MAX_CONTAINER_IDS hexadecimal container ids, without duplicates.
fn validated_ids(req: &WorkerRequest) -> Result<Vec<String>, String> {
    let op = req.op.as_str();
    let ids = req
        .ids
        .as_ref()
        .ok_or_else(|| format!("{op} requires `ids`"))?;
    if ids.is_empty() {
        return Err(format!("{op} requires at least one container id"));
    }
    if ids.len() > MAX_CONTAINER_IDS {
        return Err(format!(
            "{op} accepts at most {MAX_CONTAINER_IDS} container ids"
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        validate_container_id(id)?;
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    Ok(out)
}

/// A log cursor in Unix seconds: not negative, not far in the future.
fn validated_since(since: Option<i64>) -> Result<Option<i64>, String> {
    if let Some(secs) = since {
        if secs < 0 {
            return Err("sinceSecs must not be negative".into());
        }
        if secs > crate::db::now_secs().saturating_add(MAX_SINCE_SKEW_SECS) {
            return Err("sinceSecs is too far in the future".into());
        }
    }
    Ok(since)
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
            return Err(
                "invalid app slug: only lowercase letters, digits and '-' are allowed".into(),
            );
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

/// Image references (`nginx:1.27`, `ghcr.io/org/app@sha256:…`) and image ids
/// (`sha256:…`). The check is deliberately stricter than Docker's grammar in
/// the places that matter for argument safety: it never starts with `-`,
/// never contains whitespace, control characters or shell metacharacters, and
/// has no empty path segments. Docker validates the rest.
pub fn validate_image_ref(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_IMAGE_REF_LEN {
        return Err(format!(
            "invalid image: expected 1-{MAX_IMAGE_REF_LEN} characters"
        ));
    }
    let first = value.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return Err("invalid image: must start with a letter or digit".into());
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@'))
    {
        return Err("invalid image: only letters, digits and . _ - / : @ are allowed".into());
    }
    if value.contains("//") || value.contains("..") {
        return Err("invalid image: empty or relative path segment".into());
    }
    if value.matches('@').count() > 1 {
        return Err("invalid image: more than one digest".into());
    }
    if value.ends_with(['/', ':', '@', '.', '-']) {
        return Err("invalid image: incomplete reference".into());
    }
    Ok(())
}

/// Docker volume names: `[A-Za-z0-9][A-Za-z0-9_.-]*`, no path separators.
pub fn validate_volume_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_VOLUME_NAME_LEN {
        return Err(format!(
            "invalid volume name: expected 1-{MAX_VOLUME_NAME_LEN} characters"
        ));
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err("invalid volume name: must start with a letter or digit".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err("invalid volume name: only letters, digits and _ . - are allowed".into());
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
        let req = WorkerRequest::new(WorkerOp::Version).since_secs(1);
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
        let req = WorkerRequest::new(WorkerOp::Logs).slug("x").tail(u32::MAX);
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
    fn incremental_logs_op_is_strictly_validated() {
        let now = crate::db::now_secs();
        // Valid: past cursor + bounded tail; defaults when omitted.
        for req in [
            WorkerRequest::new(WorkerOp::ComposeLogsSince)
                .slug("shop")
                .since_secs(now - 30)
                .tail(500),
            WorkerRequest::new(WorkerOp::ComposeLogsSince).slug("shop"),
        ] {
            match validate_request(&req, &apps_dir()).unwrap() {
                ValidatedAction::App {
                    op,
                    tail,
                    since_secs,
                    ..
                } => {
                    assert_eq!(op, WorkerOp::ComposeLogsSince);
                    assert!(tail.is_some());
                    assert!(since_secs.is_none() || since_secs.unwrap() <= now);
                }
                other => panic!("unexpected action: {other:?}"),
            }
        }
        // The op serialises with the documented kebab-case name and structured
        // numeric cursor (never a free-form shell string).
        assert_eq!(
            serde_json::to_string(&WorkerOp::ComposeLogsSince).unwrap(),
            "\"compose-logs-since\""
        );
        let line = serde_json::to_string(
            &WorkerRequest::new(WorkerOp::ComposeLogsSince)
                .slug("shop")
                .since_secs(1_700_000_000)
                .tail(50),
        )
        .unwrap();
        assert!(line.contains("\"sinceSecs\":1700000000"), "{line}");

        // Rejections: negative / far-future cursors, missing slug, cursor on
        // other ops, unknown extra fields.
        for req in [
            WorkerRequest::new(WorkerOp::ComposeLogsSince)
                .slug("shop")
                .since_secs(-5),
            WorkerRequest::new(WorkerOp::ComposeLogsSince)
                .slug("shop")
                .since_secs(now + MAX_SINCE_SKEW_SECS + 10_000),
            WorkerRequest::new(WorkerOp::ComposeLogsSince),
            WorkerRequest::new(WorkerOp::ComposeLogsSince).since_secs(now - 5),
        ] {
            assert!(
                validate_request(&req, &apps_dir()).is_err(),
                "should be rejected: {req:?}"
            );
        }
        let req = WorkerRequest::new(WorkerOp::ComposeUp)
            .slug("shop")
            .since_secs(now - 5);
        assert!(validate_request(&req, &apps_dir()).is_err());
        let req = WorkerRequest::new(WorkerOp::Logs)
            .slug("shop")
            .since_secs(now - 5);
        assert!(validate_request(&req, &apps_dir()).is_err());
        // Invalid slugs are rejected before the cursor is even considered.
        let req = WorkerRequest::new(WorkerOp::ComposeLogsSince)
            .slug("../etc")
            .since_secs(now - 5);
        assert!(validate_request(&req, &apps_dir()).is_err());
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
    fn image_references_are_strict() {
        for ok in [
            "nginx",
            "nginx:1.27-alpine",
            "library/nginx:latest",
            "ghcr.io/org/app:v1.2.3",
            "localhost:5000/team/app",
            "sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a",
            "alpine@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a",
            "be2656061b53",
        ] {
            assert!(validate_image_ref(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "-v",
            "--all",
            ":tag",
            "/abs",
            "nginx latest",
            "nginx;id",
            "nginx$(id)",
            "a//b",
            "a/../b",
            "nginx:",
            "nginx/",
            "a@b@c",
            "nginx\n",
            &"a".repeat(MAX_IMAGE_REF_LEN + 1),
        ] {
            assert!(
                validate_image_ref(bad).is_err(),
                "{bad:?} should be invalid"
            );
        }
    }

    #[test]
    fn volume_names_are_strict() {
        for ok in ["data", "rast-shop_data", "a.b-c_d", "0abc"] {
            assert!(validate_volume_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "a/b",
            "../x",
            "a b",
            "a:b",
            &"a".repeat(256),
        ] {
            assert!(validate_volume_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn container_ops_take_only_validated_ids() {
        let id = "0123456789ab".to_string();
        for op in [
            WorkerOp::ContainerStart,
            WorkerOp::ContainerStop,
            WorkerOp::ContainerRestart,
            WorkerOp::ContainerRemove,
        ] {
            let req = WorkerRequest::new(op).ids(vec![id.clone(), id.clone()]);
            match validate_request(&req, &apps_dir()).unwrap() {
                ValidatedAction::Containers { op: got, ids } => {
                    assert_eq!(got, op);
                    assert_eq!(ids, vec![id.clone()], "duplicates are dropped");
                }
                other => panic!("unexpected action: {other:?}"),
            }
            for bad in [
                WorkerRequest::new(op),
                WorkerRequest::new(op).ids(vec![]),
                WorkerRequest::new(op).ids(vec!["-f".into()]),
                WorkerRequest::new(op).ids(vec!["shop-web-1".into()]),
                WorkerRequest::new(op).ids(vec![id.clone()]).slug("shop"),
                WorkerRequest::new(op).ids(vec![id.clone()]).tail(5),
                WorkerRequest::new(op).ids(vec![id.clone()]).image("nginx"),
                WorkerRequest::new(op).ids(vec![id.clone(); MAX_CONTAINER_IDS + 1]),
            ] {
                assert!(validate_request(&bad, &apps_dir()).is_err(), "{bad:?}");
            }
        }
        assert_eq!(
            serde_json::to_string(&WorkerOp::ContainerRemove).unwrap(),
            "\"container-remove\""
        );
    }

    #[test]
    fn container_logs_take_one_id_and_a_bounded_tail() {
        let now = crate::db::now_secs();
        let req = WorkerRequest::new(WorkerOp::ContainerLogs)
            .ids(vec!["0123456789ab".into()])
            .tail(u32::MAX)
            .since_secs(now - 60);
        match validate_request(&req, &apps_dir()).unwrap() {
            ValidatedAction::ContainerLogs {
                id,
                tail,
                since_secs,
            } => {
                assert_eq!(id, "0123456789ab");
                assert_eq!(tail, MAX_LOG_TAIL);
                assert_eq!(since_secs, Some(now - 60));
            }
            other => panic!("unexpected action: {other:?}"),
        }
        for bad in [
            WorkerRequest::new(WorkerOp::ContainerLogs),
            WorkerRequest::new(WorkerOp::ContainerLogs)
                .ids(vec!["0123456789ab".into(), "0123456789ac".into()]),
            WorkerRequest::new(WorkerOp::ContainerLogs)
                .ids(vec!["0123456789ab".into()])
                .since_secs(-1),
            WorkerRequest::new(WorkerOp::ContainerLogs)
                .ids(vec!["0123456789ab".into()])
                .slug("shop"),
        ] {
            assert!(validate_request(&bad, &apps_dir()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn container_exec_takes_one_id_and_a_clamped_size() {
        let req = WorkerRequest::new(WorkerOp::ContainerExec)
            .ids(vec!["0123456789ab".into()])
            .size(9999, 1);
        assert_eq!(
            validate_request(&req, &apps_dir()).unwrap(),
            ValidatedAction::Exec {
                id: "0123456789ab".into(),
                cols: super::super::exec::MAX_COLS,
                rows: super::super::exec::MIN_ROWS,
            }
        );
        let plain = WorkerRequest::new(WorkerOp::ContainerExec).ids(vec!["0123456789ab".into()]);
        assert!(matches!(
            validate_request(&plain, &apps_dir()).unwrap(),
            ValidatedAction::Exec {
                cols: 80,
                rows: 24,
                ..
            }
        ));
        for bad in [
            WorkerRequest::new(WorkerOp::ContainerExec),
            WorkerRequest::new(WorkerOp::ContainerExec)
                .ids(vec!["0123456789ab".into(), "0123456789ac".into()]),
            WorkerRequest::new(WorkerOp::ContainerExec).ids(vec!["../etc".into()]),
            WorkerRequest::new(WorkerOp::ContainerExec)
                .ids(vec!["0123456789ab".into()])
                .tail(10),
            WorkerRequest::new(WorkerOp::ContainerExec)
                .ids(vec!["0123456789ab".into()])
                .image("alpine"),
        ] {
            assert!(validate_request(&bad, &apps_dir()).is_err(), "{bad:?}");
        }
        // Sizes belong to terminals only.
        let stray = WorkerRequest::new(WorkerOp::ContainerLogs)
            .ids(vec!["0123456789ab".into()])
            .size(80, 24);
        assert!(validate_request(&stray, &apps_dir()).is_err());
        let stray = WorkerRequest::new(WorkerOp::Version).size(80, 24);
        assert!(validate_request(&stray, &apps_dir()).is_err());
    }

    #[test]
    fn image_and_volume_ops_validate_their_argument() {
        let req = WorkerRequest::new(WorkerOp::ImagePull).image("nginx:alpine");
        assert_eq!(
            validate_request(&req, &apps_dir()).unwrap(),
            ValidatedAction::Image {
                op: WorkerOp::ImagePull,
                reference: "nginx:alpine".into()
            }
        );
        let req = WorkerRequest::new(WorkerOp::VolumeRemove).volume("rast-shop_data");
        assert_eq!(
            validate_request(&req, &apps_dir()).unwrap(),
            ValidatedAction::Volume {
                op: WorkerOp::VolumeRemove,
                name: "rast-shop_data".into()
            }
        );
        for bad in [
            WorkerRequest::new(WorkerOp::ImagePull),
            WorkerRequest::new(WorkerOp::ImagePull).image("--help"),
            WorkerRequest::new(WorkerOp::ImageRemove)
                .image("nginx")
                .volume("data"),
            WorkerRequest::new(WorkerOp::ImageRemove)
                .image("nginx")
                .ids(vec!["0123456789ab".into()]),
            WorkerRequest::new(WorkerOp::VolumeRemove),
            WorkerRequest::new(WorkerOp::VolumeRemove).volume("../etc"),
            WorkerRequest::new(WorkerOp::VolumeRemove)
                .volume("data")
                .image("nginx"),
            WorkerRequest::new(WorkerOp::ImageList).image("nginx"),
            WorkerRequest::new(WorkerOp::ImagePrune).volume("data"),
            WorkerRequest::new(WorkerOp::VolumeList).slug("shop"),
            WorkerRequest::new(WorkerOp::ComposeUp)
                .slug("shop")
                .image("nginx"),
            WorkerRequest::new(WorkerOp::Inspect)
                .ids(vec!["0123456789ab".into()])
                .volume("x"),
        ] {
            assert!(validate_request(&bad, &apps_dir()).is_err(), "{bad:?}");
        }
        assert_eq!(WorkerOp::ImagePull.timeout(), PULL_OP_TIMEOUT);
        assert_eq!(WorkerOp::ImageList.timeout(), QUICK_OP_TIMEOUT);
        assert_eq!(WorkerOp::ContainerStop.timeout(), COMPOSE_OP_TIMEOUT);
    }

    #[test]
    fn response_roundtrip() {
        let ok = WorkerResponse::success("abc", serde_json::json!({"v": 1}));
        let back: WorkerResponse =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert!(back.ok);
        assert_eq!(back.data.unwrap()["v"], 1);
        assert!(back.error.is_none());

        let err = WorkerResponse::failure("abc", "command_failed", "boom\nline2");
        let back: WorkerResponse =
            serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
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
