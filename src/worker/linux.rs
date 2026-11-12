//! Worker side of Linux desktop apps (see [`crate::docker::linux`]).
//!
//! * **Build**: a shared base image (`rast-linux-base:<hash>`, the pinned
//!   streaming desktop plus RAST's session scripts, all embedded in this
//!   binary) and one image per app version (`rast-linux/<app>:<build>`). The
//!   build context is a tar stream the worker writes to `docker build -`: a
//!   Dockerfile generated here and, for uploads, the staged package. Nothing in
//!   the context comes from the server except the package bytes.
//! * **Run**: one container per app per person, published on 127.0.0.1 only,
//!   with memory/CPU/process limits, `no-new-privileges`, a named volume for
//!   the app's own settings and the person's RAST home folder at
//!   `/config/RAST`.
//! * Containers, images and volumes carry `rast.managed` labels; stop/remove
//!   refuse objects without them.

use std::{
    ffi::OsString,
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{docker_command, read_capped, run_docker, CommandOutput, WorkerError, WorkerOptions};
use crate::docker::{
    linux::{
        container_name, image_ref, package_path, validate_package_name, BuildSource, LinuxAction,
        LinuxFormat, RunSpec, APP_LABEL, CONTAINER_PORT, IMAGE_REPOSITORY, MANAGED_LABEL,
        MAX_PACKAGE_BYTES, URL_PREFIX, USER_LABEL,
    },
    protocol::{truncate_message, validate_container_id},
};

const BASE_DOCKERFILE: &str = include_str!("linux_assets/Dockerfile.base");
const INSTALL_SCRIPT: &str = include_str!("linux_assets/rast-install.py");
const AUTOSTART: &str = include_str!("linux_assets/autostart");
/// Shows a window bar while the app has several windows or a minimized one.
const WINDOWS_SCRIPT: &str = include_str!("linux_assets/windows");
const WINDOW_BAR_CONFIG: &str = include_str!("linux_assets/tint2rc");
const INIT_SCRIPT: &str = include_str!("linux_assets/init-rast");
const EXIT_SERVICE: &str = include_str!("linux_assets/svc-rast-exit.run");

/// Repository of the shared base image; the tag is a hash of its contents.
const BASE_REPOSITORY: &str = "rast-linux-base";
/// Build output kept to explain a failed build.
const MAX_BUILD_LOG: usize = 512 * 1024;
/// Largest `/rast/meta.json` accepted from an image.
const MAX_META_BYTES: usize = 2 * 1024 * 1024;
/// Largest icon (base64 characters) passed on to the server.
const MAX_ICON_B64: usize = 700 * 1024;
/// Processes one app may run.
const PIDS_LIMIT: &str = "4096";
/// How long a closing app gets before Docker stops it forcibly.
const STOP_GRACE_SECS: &str = "10";
/// Capabilities the desktop does not need.
const DROPPED_CAPABILITIES: &[&str] = &["NET_RAW", "MKNOD", "AUDIT_WRITE", "SETFCAP", "SYS_CHROOT"];

/// One file of a build context.
struct TarEntry {
    path: String,
    mode: u32,
    data: TarData,
}

enum TarData {
    Dir,
    Bytes(Vec<u8>),
    /// A file opened (without following symlinks) when the build starts.
    File {
        file: std::fs::File,
        size: u64,
    },
}

impl TarEntry {
    fn dir(path: &str) -> Self {
        Self {
            path: path.to_string(),
            mode: 0o755,
            data: TarData::Dir,
        }
    }

    fn bytes(path: &str, mode: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.to_string(),
            mode,
            data: TarData::Bytes(data.into()),
        }
    }
}

fn base_entries() -> Vec<TarEntry> {
    vec![
        TarEntry::bytes("Dockerfile", 0o644, BASE_DOCKERFILE),
        TarEntry::dir("rast"),
        TarEntry::dir("rast/bin"),
        TarEntry::bytes("rast/bin/rast-install", 0o755, INSTALL_SCRIPT),
        TarEntry::bytes("rast/autostart", 0o644, AUTOSTART),
        TarEntry::bytes("rast/windows", 0o755, WINDOWS_SCRIPT),
        TarEntry::bytes("rast/tint2rc", 0o644, WINDOW_BAR_CONFIG),
        TarEntry::dir("custom-cont-init.d"),
        TarEntry::bytes("custom-cont-init.d/50-rast", 0o755, INIT_SCRIPT),
        TarEntry::dir("s6"),
        TarEntry::dir("s6/svc-rast-exit"),
        TarEntry::bytes("s6/svc-rast-exit/type", 0o644, "longrun\n"),
        TarEntry::bytes("s6/svc-rast-exit/run", 0o755, EXIT_SERVICE),
        TarEntry::dir("s6/svc-rast-exit/dependencies.d"),
        TarEntry::bytes("s6/svc-rast-exit/dependencies.d/init-services", 0o644, ""),
        TarEntry::dir("s6/user"),
        TarEntry::dir("s6/user/contents.d"),
        TarEntry::bytes("s6/user/contents.d/svc-rast-exit", 0o644, ""),
    ]
}

/// Content hash of the base image's build context: a RAST release that
/// changes the desktop layer builds a new base, older app images keep theirs.
pub fn base_tag() -> String {
    let mut hasher = Sha256::new();
    for entry in base_entries() {
        hasher.update(entry.path.as_bytes());
        hasher.update(entry.mode.to_be_bytes());
        if let TarData::Bytes(bytes) = &entry.data {
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        }
    }
    hex::encode(hasher.finalize())[..12].to_string()
}

pub fn base_image() -> String {
    format!("{BASE_REPOSITORY}:{}", base_tag())
}

pub(super) async fn dispatch(
    options: &WorkerOptions,
    action: LinuxAction,
) -> Result<Value, WorkerError> {
    match action {
        LinuxAction::Build {
            app,
            build,
            format,
            source,
        } => build_app(options, &app, build, format, source).await,
        LinuxAction::Run(spec) => run_app(options, &spec).await,
        LinuxAction::Stop { app, user_id } => {
            let name = container_name(&app, user_id);
            let stopped = stop_container(options, &name).await?;
            Ok(json!({"container": name, "stopped": stopped}))
        }
        LinuxAction::Remove { app, remove_data } => remove_app(options, &app, remove_data).await,
        LinuxAction::Prune { app, keep_build } => prune_images(options, &app, keep_build).await,
        LinuxAction::List => list_containers(options).await,
        LinuxAction::Check {
            app,
            build,
            package,
        } => check_apt(options, &app, build, &package).await,
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

fn mount_name(format: LinuxFormat) -> &'static str {
    match format {
        LinuxFormat::Deb => "app.deb",
        LinuxFormat::Appimage => "app.AppImage",
        LinuxFormat::Tarball => "app.tar",
    }
}

fn install_step(format: LinuxFormat, mount: &str) -> String {
    let target = format!("/tmp/rast-src/{}", mount_name(format));
    format!(
        "RUN --mount=type=bind,{mount},target={target} \
         /lsiopy/bin/python3 /rast/bin/rast-install {} {target}\n",
        format.as_str()
    )
}

fn file_dockerfile(base: &str, format: LinuxFormat) -> String {
    format!("FROM {base}\n{}", install_step(format, "source=package"))
}

fn apt_dockerfile(base: &str, from_image: &str, package: &str) -> String {
    format!(
        "FROM {from_image} AS fetch\n\
         RUN apt-get update -qq && mkdir -p /rast-fetch && cd /rast-fetch \
         && apt-get download {package} && mv /rast-fetch/*.deb /rast-fetch/app.deb\n\
         FROM {base}\n{}",
        install_step(LinuxFormat::Deb, "from=fetch,source=/rast-fetch/app.deb")
    )
}

/// The staged upload, opened without following symlinks inside
/// `<data_dir>/linux-apps/<app>/`.
fn open_package(options: &WorkerOptions, app: &str) -> Result<(std::fs::File, u64), WorkerError> {
    let root = std::fs::canonicalize(&options.linux_apps_dir)
        .map_err(|_| WorkerError::not_found("no Linux app has been uploaded"))?;
    let dir = root.join(app);
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|_| WorkerError::not_found("the uploaded app file is missing"))?;
    if !meta.is_dir() {
        return Err(WorkerError::forbidden(
            "the app's upload folder is not a folder",
        ));
    }
    let path = package_path(&root, app);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| WorkerError::not_found("the uploaded app file is missing"))?;
    let meta = file
        .metadata()
        .map_err(|e| WorkerError::command_failed(format!("cannot read the app file: {e}")))?;
    if !meta.is_file() {
        return Err(WorkerError::forbidden(
            "the uploaded app is not a regular file",
        ));
    }
    if meta.len() == 0 || meta.len() > MAX_PACKAGE_BYTES {
        return Err(WorkerError::invalid(
            "the uploaded app file is empty or too large",
        ));
    }
    Ok((file, meta.len()))
}

async fn build_app(
    options: &WorkerOptions,
    app: &str,
    build: u32,
    format: LinuxFormat,
    source: BuildSource,
) -> Result<Value, WorkerError> {
    let base = ensure_base(options).await?;
    let image = image_ref(app, build);
    let mut entries = Vec::new();
    match &source {
        BuildSource::File => {
            let (file, size) = open_package(options, app)?;
            entries.push(TarEntry::bytes(
                "Dockerfile",
                0o644,
                file_dockerfile(&base, format),
            ));
            entries.push(TarEntry {
                path: "package".into(),
                mode: 0o644,
                data: TarData::File { file, size },
            });
        }
        BuildSource::Apt {
            from_build,
            package,
        } => {
            validate_package_name(package).map_err(WorkerError::invalid)?;
            let from_image = image_ref(app, *from_build);
            if !image_exists(options, &from_image).await? {
                return Err(WorkerError::not_found(format!(
                    "the installed version ({from_image}) is missing"
                )));
            }
            entries.push(TarEntry::bytes(
                "Dockerfile",
                0o644,
                apt_dockerfile(&base, &from_image, package),
            ));
        }
    }
    let args = build_args(
        &image,
        &[
            (MANAGED_LABEL, "linux-app".to_string()),
            (APP_LABEL, app.to_string()),
            ("rast.linux.build", build.to_string()),
        ],
    );
    let output = run_docker_with_context(options, args, entries, options.pull_timeout).await?;
    if !output.success {
        return Err(build_failure(&output));
    }
    let meta = read_meta(options, &image).await?;
    let size = image_size(options, &image).await;
    Ok(json!({"image": image, "build": build, "meta": meta, "sizeBytes": size}))
}

fn build_args(image: &str, labels: &[(&str, String)]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["build".into(), "--progress=plain".into()];
    for (key, value) in labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
    args.push("--tag".into());
    args.push(image.into());
    args.push("-".into());
    args
}

/// The shared base image, built when this RAST version has not built it yet.
async fn ensure_base(options: &WorkerOptions) -> Result<String, WorkerError> {
    let image = base_image();
    if image_exists(options, &image).await? {
        return Ok(image);
    }
    tracing::info!(image = %image, "building the Linux apps base image");
    let args = build_args(&image, &[(MANAGED_LABEL, "linux-base".to_string())]);
    let output =
        run_docker_with_context(options, args, base_entries(), options.pull_timeout).await?;
    if !output.success {
        let mut error = build_failure(&output);
        error.message = format!(
            "could not prepare the Linux desktop layer: {}",
            error.message
        );
        return Err(error);
    }
    Ok(image)
}

/// Why a build failed, in a form the person can act on: the installer's own
/// message when it gave one, else the end of the build log.
fn build_failure(output: &CommandOutput) -> WorkerError {
    let log = format!("{}\n{}", output.stdout, output.stderr);
    if let Some(message) = log
        .lines()
        .filter_map(|line| line.split_once("RAST-ERROR: ").map(|(_, m)| m.trim()))
        .next_back()
    {
        return WorkerError::new("build_failed", truncate_message(message, 400));
    }
    let tail: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let start = tail.len().saturating_sub(6);
    WorkerError::new(
        "build_failed",
        truncate_message(&tail[start..].join("\n"), 700),
    )
}

async fn image_exists(options: &WorkerOptions, image: &str) -> Result<bool, WorkerError> {
    let args = vec![
        OsString::from("image"),
        "inspect".into(),
        "--format".into(),
        "{{.Id}}".into(),
        "--".into(),
        image.into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 4096).await?;
    Ok(output.success)
}

async fn image_size(options: &WorkerOptions, image: &str) -> Option<u64> {
    let args = vec![
        OsString::from("image"),
        "inspect".into(),
        "--format".into(),
        "{{.Size}}".into(),
        "--".into(),
        image.into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 4096)
        .await
        .ok()?;
    output.stdout.trim().parse().ok()
}

/// `/rast/meta.json` of a freshly built image, checked field by field: the
/// file was written by code that came with the uploaded app.
async fn read_meta(options: &WorkerOptions, image: &str) -> Result<Value, WorkerError> {
    let args = vec![
        OsString::from("run"),
        "--rm".into(),
        "--pull".into(),
        "never".into(),
        "--network".into(),
        "none".into(),
        "--label".into(),
        format!("{MANAGED_LABEL}=linux-probe").into(),
        "--entrypoint".into(),
        "/bin/cat".into(),
        "--".into(),
        image.into(),
        "/rast/meta.json".into(),
    ];
    let output = run_docker(options, args, None, options.compose_timeout, MAX_META_BYTES).await?;
    if !output.success {
        return Err(WorkerError::command_failed(format!(
            "the app was installed but could not be described: {}",
            super::stderr_summary(&output)
        )));
    }
    let raw: Value = serde_json::from_str(output.stdout.trim()).map_err(|e| {
        WorkerError::command_failed(format!("the app's description is unreadable: {e}"))
    })?;
    Ok(sanitize_meta(&raw))
}

fn clean_text(value: Option<&Value>, max_chars: usize) -> Option<String> {
    let text = value?.as_str()?;
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn safe_token(value: Option<&Value>, max: usize) -> Option<String> {
    let text = value?.as_str()?;
    (!text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'*' | b'+')))
    .then(|| text.to_string())
}

/// Keep only well-formed fields of an image's self-description.
pub(crate) fn sanitize_meta(raw: &Value) -> Value {
    let mut meta = Map::new();
    let put = |meta: &mut Map<String, Value>, key: &str, value: Option<String>| {
        meta.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
    };
    put(&mut meta, "name", clean_text(raw.get("name"), 80));
    put(&mut meta, "version", clean_text(raw.get("version"), 64));
    put(
        &mut meta,
        "description",
        clean_text(raw.get("description"), 300),
    );
    put(
        &mut meta,
        "package",
        raw.get("package")
            .and_then(Value::as_str)
            .filter(|name| validate_package_name(name).is_ok())
            .map(str::to_string),
    );
    put(
        &mut meta,
        "format",
        raw.get("format")
            .and_then(Value::as_str)
            .and_then(LinuxFormat::parse)
            .map(|f| f.as_str().to_string()),
    );
    let exec: Vec<Value> = raw
        .get("exec")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(32)
                .filter_map(|item| clean_text(Some(item), 300).map(Value::String))
                .collect()
        })
        .unwrap_or_default();
    meta.insert("exec".into(), Value::Array(exec));
    for flag in ["chromium", "launchGuess"] {
        meta.insert(
            flag.into(),
            Value::Bool(raw.get(flag).and_then(Value::as_bool).unwrap_or(false)),
        );
    }
    let icon = raw.get("icon").and_then(|icon| {
        let kind = icon.get("type")?.as_str()?;
        let data = icon.get("data")?.as_str()?;
        let valid = matches!(kind, "png" | "svg")
            && !data.is_empty()
            && data.len() <= MAX_ICON_B64
            && data
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
        valid.then(|| json!({"type": kind, "data": data}))
    });
    meta.insert("icon".into(), icon.unwrap_or(Value::Null));
    let update = raw.get("update").and_then(|update| {
        match update.get("kind")?.as_str()? {
            "apt" => Some(json!({"kind": "apt"})),
            "github" => {
                let owner = safe_token(update.get("owner"), 100)?;
                let repo = safe_token(update.get("repo"), 100)?;
                let tag = safe_token(update.get("tag"), 100)?;
                let file = safe_token(update.get("file"), 200)?;
                Some(json!({"kind": "github", "owner": owner, "repo": repo, "tag": tag, "file": file}))
            }
            "zsync" => {
                let url = clean_text(update.get("url"), 500)?;
                (url.starts_with("https://") && !url.contains(char::is_whitespace))
                    .then(|| json!({"kind": "zsync", "url": url}))
            }
            _ => None,
        }
    });
    meta.insert("update".into(), update.unwrap_or(Value::Null));
    Value::Object(meta)
}

/// Run `docker <args>` with a tar build context written to its stdin.
async fn run_docker_with_context(
    options: &WorkerOptions,
    args: Vec<OsString>,
    entries: Vec<TarEntry>,
    limit: Duration,
) -> Result<CommandOutput, WorkerError> {
    let mut command = docker_command(options);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        WorkerError::unavailable(format!(
            "cannot execute docker ({}): {error}",
            options.docker_bin.display()
        ))
    })?;
    let stdin = child.stdin.take();
    let stdout_task = tokio::spawn(read_capped(child.stdout.take(), MAX_BUILD_LOG));
    let stderr_task = tokio::spawn(read_capped(child.stderr.take(), MAX_BUILD_LOG));
    let writer = tokio::spawn(async move {
        match stdin {
            Some(mut stdin) => write_tar(&mut stdin, entries).await,
            None => Err(io::Error::other("no stdin")),
        }
    });

    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            writer.abort();
            return Err(WorkerError::command_failed(format!(
                "cannot wait for docker: {error}"
            )));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            writer.abort();
            return Err(WorkerError::new(
                "timeout",
                format!(
                    "the build did not finish within {} minutes",
                    limit.as_secs() / 60
                ),
            ));
        }
    };
    let written = writer.await;
    let stdout = String::from_utf8_lossy(&stdout_task.await.unwrap_or_default()).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
    if let Ok(Err(error)) = written {
        if status.success() {
            return Err(WorkerError::command_failed(format!(
                "could not send the app to Docker: {error}"
            )));
        }
        stderr.push_str(&format!("\n(sending the build context failed: {error})"));
    }
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

async fn write_tar<W>(out: &mut W, entries: Vec<TarEntry>) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    for entry in entries {
        let mut header = tar::Header::new_ustar();
        header.set_path(&entry.path)?;
        header.set_mode(entry.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        let size = match &entry.data {
            TarData::Dir => 0,
            TarData::Bytes(bytes) => bytes.len() as u64,
            TarData::File { size, .. } => *size,
        };
        header.set_entry_type(match entry.data {
            TarData::Dir => tar::EntryType::Directory,
            _ => tar::EntryType::Regular,
        });
        header.set_size(size);
        header.set_cksum();
        out.write_all(header.as_bytes()).await?;
        match entry.data {
            TarData::Dir => {}
            TarData::Bytes(bytes) => out.write_all(&bytes).await?,
            TarData::File { file, size } => {
                let mut reader = tokio::fs::File::from_std(file).take(size);
                let copied = tokio::io::copy(&mut reader, out).await?;
                if copied != size {
                    return Err(io::Error::other("the app file changed while it was sent"));
                }
            }
        }
        let padding = (512 - (size % 512) as usize) % 512;
        out.write_all(&vec![0u8; padding]).await?;
    }
    out.write_all(&[0u8; 1024]).await?;
    out.shutdown().await
}

// ---------------------------------------------------------------------------
// Run / stop
// ---------------------------------------------------------------------------

/// The person's home folder, which must be a real folder (not a symlink)
/// directly inside the homes root and not owned by root. Its owner becomes
/// the app's user, so files the app saves belong to RAST.
fn person_home(
    options: &WorkerOptions,
    username: &str,
) -> Result<(PathBuf, u32, u32), WorkerError> {
    let root = std::fs::canonicalize(&options.homes_dir)
        .map_err(|_| WorkerError::unavailable("the home folders are not available"))?;
    let path = root.join(username);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| WorkerError::not_found("this account has no home folder yet"))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(WorkerError::forbidden(
            "the home folder is not a regular folder",
        ));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|_| WorkerError::not_found("this account has no home folder yet"))?;
    if canonical != path {
        return Err(WorkerError::forbidden(
            "the home folder is outside the homes root",
        ));
    }
    if canonical
        .to_str()
        .is_none_or(|text| text.contains([',', '"', '\n', '\r']))
    {
        return Err(WorkerError::forbidden(
            "the home folder path cannot be shared with apps",
        ));
    }
    if meta.uid() == 0 {
        return Err(WorkerError::forbidden(
            "home folders owned by root cannot be shared with apps",
        ));
    }
    Ok((canonical, meta.uid(), meta.gid()))
}

/// The server's time zone name, when it has a well-formed one.
fn host_timezone() -> Option<String> {
    let from_file = std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|text| text.trim().to_string());
    let from_link = || {
        let target = std::fs::read_link("/etc/localtime").ok()?;
        let text = target.to_str()?;
        let (_, zone) = text.split_once("zoneinfo/")?;
        Some(zone.to_string())
    };
    let zone = from_file.filter(|z| !z.is_empty()).or_else(from_link)?;
    (zone.len() <= 64
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-')))
    .then_some(zone)
}

fn push_pair(args: &mut Vec<OsString>, flag: &str, value: impl Into<OsString>) {
    args.push(flag.into());
    args.push(value.into());
}

pub(crate) fn run_args(
    spec: &RunSpec,
    home: &Path,
    uid: u32,
    gid: u32,
    tz: Option<&str>,
) -> Vec<OsString> {
    let name = container_name(&spec.app, spec.user_id);
    let mut args: Vec<OsString> = vec!["run".into(), "--detach".into()];
    push_pair(&mut args, "--name", &name);
    // A fixed machine name: apps that lock their profile by host name
    // (Chrome, Electron) see the next copy as the same computer.
    push_pair(&mut args, "--hostname", "rast");
    push_pair(&mut args, "--pull", "never");
    push_pair(&mut args, "--label", format!("{MANAGED_LABEL}=linux-app"));
    push_pair(&mut args, "--label", format!("{APP_LABEL}={}", spec.app));
    push_pair(
        &mut args,
        "--label",
        format!("{USER_LABEL}={}", spec.user_id),
    );
    push_pair(&mut args, "--restart", "no");
    push_pair(&mut args, "--shm-size", "1g");
    push_pair(&mut args, "--memory", format!("{}m", spec.memory_mb));
    push_pair(&mut args, "--memory-swap", format!("{}m", spec.memory_mb));
    push_pair(&mut args, "--cpus", spec.cpus.to_string());
    push_pair(&mut args, "--pids-limit", PIDS_LIMIT);
    push_pair(&mut args, "--security-opt", "no-new-privileges");
    for cap in DROPPED_CAPABILITIES {
        push_pair(&mut args, "--cap-drop", *cap);
    }
    push_pair(
        &mut args,
        "--publish",
        format!("127.0.0.1::{CONTAINER_PORT}"),
    );
    let mut env = vec![
        format!("PUID={uid}"),
        format!("PGID={gid}"),
        "CUSTOM_USER=rast".to_string(),
        format!("PASSWORD={}", spec.password),
        format!("SUBFOLDER={URL_PREFIX}{}/", spec.key),
        format!("TITLE={}", spec.title),
        "HARDEN_DESKTOP=true".to_string(),
        "DISABLE_MOUSE_BUTTONS=true".to_string(),
        "NO_DECOR=true".to_string(),
        "NO_GAMEPAD=true".to_string(),
        "NO_WEBCAM=true".to_string(),
        "SELKIES_UI_SHOW_SIDEBAR=false|locked".to_string(),
    ];
    if let Some(tz) = tz {
        env.push(format!("TZ={tz}"));
    }
    for item in env {
        push_pair(&mut args, "--env", item);
    }
    push_pair(&mut args, "--volume", format!("{name}:/config"));
    let mut mount = OsString::from("type=bind,source=");
    mount.push(home.as_os_str());
    mount.push(",target=/config/RAST");
    push_pair(&mut args, "--mount", mount);
    args.push("--".into());
    args.push(image_ref(&spec.app, spec.build).into());
    args
}

async fn run_app(options: &WorkerOptions, spec: &RunSpec) -> Result<Value, WorkerError> {
    let image = image_ref(&spec.app, spec.build);
    let name = container_name(&spec.app, spec.user_id);
    let (home, uid, gid) = person_home(options, &spec.username)?;
    if !image_exists(options, &image).await? {
        return Err(WorkerError::not_found(
            "this app's image is missing; reinstall the app",
        ));
    }
    // A copy left over from an earlier start (for example after a restart of
    // RAST) is replaced: its address and password are no longer known.
    stop_container(options, &name).await?;
    ensure_volume(options, spec).await?;
    let args = run_args(spec, &home, uid, gid, host_timezone().as_deref());
    let output = run_docker(options, args, None, options.compose_timeout, 64 * 1024).await?;
    if !output.success {
        return Err(WorkerError::command_failed(format!(
            "the app could not start: {}",
            super::stderr_summary(&output)
        )));
    }
    let id = output
        .stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    validate_container_id(&id).map_err(WorkerError::command_failed)?;
    let port = published_port(options, &id).await?;
    Ok(json!({"container": name, "containerId": id, "port": port, "image": image}))
}

async fn ensure_volume(options: &WorkerOptions, spec: &RunSpec) -> Result<(), WorkerError> {
    let name = container_name(&spec.app, spec.user_id);
    let args = vec![
        OsString::from("volume"),
        "create".into(),
        "--label".into(),
        format!("{MANAGED_LABEL}=linux-app").into(),
        "--label".into(),
        format!("{APP_LABEL}={}", spec.app).into(),
        "--label".into(),
        format!("{USER_LABEL}={}", spec.user_id).into(),
        "--".into(),
        name.into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 4096).await?;
    super::ensure_success("volume create", &output)
}

async fn published_port(options: &WorkerOptions, id: &str) -> Result<u16, WorkerError> {
    let args = vec![
        OsString::from("port"),
        id.into(),
        format!("{CONTAINER_PORT}/tcp").into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 4096).await?;
    super::ensure_success("docker port", &output)?;
    parse_published_port(&output.stdout)
        .ok_or_else(|| WorkerError::command_failed("the app has no local address"))
}

/// First `127.0.0.1:<port>` line of `docker port`.
pub(crate) fn parse_published_port(stdout: &str) -> Option<u16> {
    stdout.lines().find_map(|line| {
        let (host, port) = line.trim().rsplit_once(':')?;
        (host == "127.0.0.1").then(|| port.parse().ok()).flatten()
    })
}

/// The `rast.managed` label of a container, or `None` when it does not exist.
async fn managed_label(options: &WorkerOptions, name: &str) -> Result<Option<String>, WorkerError> {
    let args = vec![
        OsString::from("container"),
        "inspect".into(),
        "--format".into(),
        format!("{{{{index .Config.Labels \"{MANAGED_LABEL}\"}}}}").into(),
        "--".into(),
        name.into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 4096).await?;
    if !output.success {
        if output.stderr.to_ascii_lowercase().contains("no such") {
            return Ok(None);
        }
        return Err(WorkerError::command_failed(format!(
            "cannot inspect {name}: {}",
            super::stderr_summary(&output)
        )));
    }
    Ok(Some(output.stdout.trim().to_string()))
}

/// Stop and remove one app container (its volume stays). Containers RAST did
/// not create are never touched.
async fn stop_container(options: &WorkerOptions, name: &str) -> Result<bool, WorkerError> {
    match managed_label(options, name).await? {
        None => return Ok(false),
        Some(label) if label == "linux-app" => {}
        Some(_) => {
            return Err(WorkerError::forbidden(format!(
                "{name} exists but was not created by RAST; rename or remove it first"
            )))
        }
    }
    let stop = vec![
        OsString::from("stop"),
        "--time".into(),
        STOP_GRACE_SECS.into(),
        "--".into(),
        name.into(),
    ];
    let _ = run_docker(options, stop, None, options.compose_timeout, 4096).await?;
    let remove = vec![
        OsString::from("rm"),
        "--force".into(),
        "--".into(),
        name.into(),
    ];
    let output = run_docker(options, remove, None, options.compose_timeout, 4096).await?;
    if !output.success && !output.stderr.to_ascii_lowercase().contains("no such") {
        return Err(WorkerError::command_failed(format!(
            "cannot remove {name}: {}",
            super::stderr_summary(&output)
        )));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// List / check / remove
// ---------------------------------------------------------------------------

async fn labelled_containers(
    options: &WorkerOptions,
    filter: &str,
) -> Result<Vec<Value>, WorkerError> {
    let args = vec![
        OsString::from("ps"),
        "--all".into(),
        "--no-trunc".into(),
        "--filter".into(),
        format!("label={MANAGED_LABEL}=linux-app").into(),
        "--filter".into(),
        filter.into(),
        "--format".into(),
        "{{json .}}".into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 1024 * 1024).await?;
    super::ensure_success("docker ps", &output)?;
    Ok(parse_container_lines(&output.stdout))
}

/// `docker ps --format '{{json .}}'` lines → `{id, name, app, userId, state}`.
pub(crate) fn parse_container_lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|row| {
            let labels = row.get("Labels")?.as_str()?;
            let label = |key: &str| {
                labels.split(',').find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    (k == key).then(|| v.to_string())
                })
            };
            let app = label(APP_LABEL)?;
            let user_id: i64 = label(USER_LABEL)?.parse().ok()?;
            let id = row.get("ID")?.as_str()?.to_string();
            validate_container_id(&id).ok()?;
            let state = row
                .get("State")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Some(json!({
                "id": id,
                "name": row.get("Names").and_then(Value::as_str).unwrap_or_default(),
                "app": app,
                "userId": user_id,
                "state": state,
                "running": state == "running",
            }))
        })
        .collect()
}

async fn list_containers(options: &WorkerOptions) -> Result<Value, WorkerError> {
    let containers = labelled_containers(options, &format!("label={APP_LABEL}")).await?;
    Ok(json!({"containers": containers}))
}

/// Ask the app's apt sources for the newest version, in a throwaway copy of
/// the app's image (the sources are inside it).
async fn check_apt(
    options: &WorkerOptions,
    app: &str,
    build: u32,
    package: &str,
) -> Result<Value, WorkerError> {
    let image = image_ref(app, build);
    let args = vec![
        OsString::from("run"),
        "--rm".into(),
        "--pull".into(),
        "never".into(),
        "--label".into(),
        format!("{MANAGED_LABEL}=linux-probe").into(),
        "--entrypoint".into(),
        "/bin/sh".into(),
        "--".into(),
        image.into(),
        "-c".into(),
        "apt-get update -qq >/dev/null 2>&1 || exit 3; LC_ALL=C apt-cache policy \"$1\"".into(),
        "rast-check".into(),
        package.into(),
    ];
    let output = run_docker(options, args, None, options.compose_timeout, 64 * 1024).await?;
    if !output.success {
        return Err(WorkerError::command_failed(
            "the app's update source could not be reached",
        ));
    }
    let (installed, candidate) = parse_apt_policy(&output.stdout);
    Ok(json!({"installed": installed, "candidate": candidate}))
}

/// `Installed:` and `Candidate:` of `apt-cache policy`.
pub(crate) fn parse_apt_policy(stdout: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| {
        stdout.lines().find_map(|line| {
            let value = line.trim().strip_prefix(name)?.trim();
            (!value.is_empty() && value != "(none)" && value.len() <= 64).then(|| value.to_string())
        })
    };
    (field("Installed:"), field("Candidate:"))
}

async fn app_images(options: &WorkerOptions, app: &str) -> Result<Vec<String>, WorkerError> {
    let args = vec![
        OsString::from("image"),
        "ls".into(),
        "--filter".into(),
        format!("reference={IMAGE_REPOSITORY}/{app}").into(),
        "--format".into(),
        "{{.Repository}}:{{.Tag}}".into(),
    ];
    let output = run_docker(options, args, None, options.quick_timeout, 256 * 1024).await?;
    super::ensure_success("docker image ls", &output)?;
    let prefix = format!("{IMAGE_REPOSITORY}/{app}:");
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.strip_prefix(&prefix)
                .is_some_and(|tag| !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect())
}

async fn remove_image(options: &WorkerOptions, image: &str) -> bool {
    let args = vec![
        OsString::from("image"),
        "rm".into(),
        "--".into(),
        image.into(),
    ];
    matches!(
        run_docker(options, args, None, options.compose_timeout, 64 * 1024).await,
        Ok(output) if output.success
    )
}

async fn prune_images(
    options: &WorkerOptions,
    app: &str,
    keep_build: u32,
) -> Result<Value, WorkerError> {
    let keep = image_ref(app, keep_build);
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for image in app_images(options, app).await? {
        if image == keep {
            continue;
        }
        if remove_image(options, &image).await {
            removed.push(image);
        } else {
            kept.push(image);
        }
    }
    Ok(json!({"removed": removed, "inUse": kept}))
}

async fn remove_app(
    options: &WorkerOptions,
    app: &str,
    remove_data: bool,
) -> Result<Value, WorkerError> {
    let containers = labelled_containers(options, &format!("label={APP_LABEL}={app}")).await?;
    let mut stopped = 0;
    for container in &containers {
        if let Some(name) = container.get("name").and_then(Value::as_str) {
            if stop_container(options, name).await? {
                stopped += 1;
            }
        }
    }
    let mut removed_images = 0;
    for image in app_images(options, app).await? {
        if remove_image(options, &image).await {
            removed_images += 1;
        }
    }
    let mut removed_volumes = 0;
    if remove_data {
        let args = vec![
            OsString::from("volume"),
            "ls".into(),
            "--quiet".into(),
            "--filter".into(),
            format!("label={MANAGED_LABEL}=linux-app").into(),
            "--filter".into(),
            format!("label={APP_LABEL}={app}").into(),
        ];
        let output = run_docker(options, args, None, options.quick_timeout, 256 * 1024).await?;
        super::ensure_success("docker volume ls", &output)?;
        for volume in output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if crate::docker::protocol::validate_volume_name(volume).is_err() {
                continue;
            }
            let args = vec![
                OsString::from("volume"),
                "rm".into(),
                "--".into(),
                volume.into(),
            ];
            if matches!(
                run_docker(options, args, None, options.compose_timeout, 4096).await,
                Ok(output) if output.success
            ) {
                removed_volumes += 1;
            }
        }
    }
    Ok(json!({
        "stoppedContainers": stopped,
        "removedImages": removed_images,
        "removedVolumes": removed_volumes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_tag_is_stable_and_short() {
        assert_eq!(base_tag(), base_tag());
        assert_eq!(base_tag().len(), 12);
        assert!(base_image().starts_with("rast-linux-base:"));
    }

    #[test]
    fn dockerfiles_are_generated_from_validated_values() {
        let file = file_dockerfile("rast-linux-base:abc", LinuxFormat::Deb);
        assert!(file.starts_with("FROM rast-linux-base:abc\n"));
        assert!(file.contains("source=package,target=/tmp/rast-src/app.deb"));
        assert!(file.contains("rast-install deb /tmp/rast-src/app.deb"));
        let apt = apt_dockerfile(
            "rast-linux-base:abc",
            "rast-linux/chrome:1",
            "google-chrome-stable",
        );
        assert!(apt.starts_with("FROM rast-linux/chrome:1 AS fetch\n"));
        assert!(apt.contains("apt-get download google-chrome-stable"));
        assert!(apt.contains("from=fetch,source=/rast-fetch/app.deb"));
    }

    #[test]
    fn run_flags_limit_and_isolate_the_app() {
        let spec = RunSpec {
            app: "chrome".into(),
            build: 2,
            user_id: 5,
            username: "alice".into(),
            memory_mb: 4096,
            cpus: 6,
            password: "a".repeat(32),
            key: "b".repeat(32),
            title: "Google Chrome".into(),
        };
        let args: Vec<String> = run_args(
            &spec,
            Path::new("/srv/rast/homes/alice"),
            993,
            980,
            Some("Asia/Jakarta"),
        )
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
        let joined = args.join(" ");
        for expected in [
            "--name rast-linux-chrome-u5",
            "--hostname rast",
            "--pull never",
            "--memory 4096m",
            "--memory-swap 4096m",
            "--cpus 6",
            "--security-opt no-new-privileges",
            "--publish 127.0.0.1::3000",
            "--env PUID=993",
            "--env PGID=980",
            "--env TZ=Asia/Jakarta",
            &format!("--env SUBFOLDER=/linux-apps/{}/", "b".repeat(32)),
            "--volume rast-linux-chrome-u5:/config",
            "--mount type=bind,source=/srv/rast/homes/alice,target=/config/RAST",
            "-- rast-linux/chrome:2",
        ] {
            assert!(
                joined.contains(expected),
                "missing `{expected}` in {joined}"
            );
        }
        assert!(!joined.contains("--privileged"));
        assert_eq!(args.last().unwrap(), "rast-linux/chrome:2");
    }

    #[test]
    fn docker_output_parsers() {
        assert_eq!(parse_published_port("127.0.0.1:32771\n"), Some(32771));
        assert_eq!(parse_published_port("0.0.0.0:32771\n"), None);
        assert_eq!(
            parse_apt_policy(
                "google-chrome-stable:\n  Installed: 142.0.1-1\n  Candidate: 143.0.2-1\n  Version table:\n"
            ),
            (Some("142.0.1-1".into()), Some("143.0.2-1".into()))
        );
        assert_eq!(parse_apt_policy("x:\n  Installed: (none)\n"), (None, None));
        let rows = parse_container_lines(concat!(
            r#"{"ID":"abc123","Names":"rast-linux-chrome-u5","State":"running","Labels":"rast.managed=linux-app,rast.linux.app=chrome,rast.linux.user=5"}"#,
            "\n",
            r#"{"ID":"zzz","Names":"x","State":"running","Labels":"rast.linux.app=x,rast.linux.user=1"}"#,
        ));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["app"], "chrome");
        assert_eq!(rows[0]["userId"], 5);
        assert_eq!(rows[0]["running"], true);
    }

    #[test]
    fn image_descriptions_are_sanitized() {
        let meta = sanitize_meta(&json!({
            "name": "Google\u{7}Chrome",
            "version": "142.0",
            "package": "bad name; rm",
            "format": "deb",
            "exec": ["/usr/bin/google-chrome-stable", "--no-sandbox"],
            "chromium": true,
            "icon": {"type": "png", "data": "iVBORw0KGgo="},
            "update": {"kind": "github", "owner": "a/b", "repo": "c", "tag": "latest", "file": "x"},
        }));
        assert_eq!(meta["name"], "GoogleChrome");
        assert_eq!(meta["package"], Value::Null);
        assert_eq!(meta["exec"][1], "--no-sandbox");
        assert_eq!(meta["icon"]["type"], "png");
        assert_eq!(meta["update"], Value::Null, "owner with a slash is refused");
        let bad_icon = sanitize_meta(&json!({"icon": {"type": "html", "data": "PHNjcmlwdD4="}}));
        assert_eq!(bad_icon["icon"], Value::Null);
    }

    #[tokio::test]
    async fn tar_context_is_well_formed() {
        let mut out = Vec::new();
        write_tar(&mut out, base_entries()).await.unwrap();
        assert_eq!(out.len() % 512, 0);
        let mut archive = tar::Archive::new(out.as_slice());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(names.contains(&"Dockerfile".to_string()));
        assert!(names.contains(&"rast/bin/rast-install".to_string()));
        assert!(names.contains(&"s6/user/contents.d/svc-rast-exit".to_string()));
    }
}
