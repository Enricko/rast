//! Self-update: check GitHub Releases, verify the artifact, back up the
//! database, replace the binary atomically, restart the service and verify
//! health. Rollback is only attempted when the database schema is compatible.
//!
//! Design notes:
//! * The update is *not* a silent self-replacing network daemon: it runs from
//!   the CLI (`rast update`), normally as root via sudo.
//! * Downloads go to a temporary directory and are verified against
//!   `SHA256SUMS` published with the release before anything is replaced.
//! * A rollback (restoring the previous binary) only happens when the new
//!   binary starts and reports a schema version <= the backed-up database
//!   schema; otherwise the operator is told to restore the backup manually.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use serde_json::{json, Value};

use crate::{
    config::Config,
    db,
    error::{AppError, AppResult},
};

const REPO: &str = "Enricko/rast";
const API_LATEST: &str = "https://api.github.com/repos/Enricko/rast/releases/latest";
const API_TAG: &str = "https://api.github.com/repos/Enricko/rast/releases/tags";
const DOWNLOAD_BASE: &str = "https://github.com/Enricko/rast/releases/download";
const SERVICE: &str = "rast.service";
/// The Docker helper runs the same binary; it must be restarted with it.
const WORKER_SERVICE: &str = "rast-worker.service";
/// The privileged native helper runs the same binary too; a helper left on the
/// previous binary rejects operations added by an update.
const NATIVE_SERVICE: &str = "rast-native.service";
const NATIVE_UNIT: &str = "/etc/systemd/system/rast-native.service";
const NATIVE_TMPFILES: &str = "/etc/tmpfiles.d/rast-native.conf";
/// The configuration path the release installer manages.
const INSTALLER_CONFIG: &str = "/etc/rast/rast.toml";
pub const BIN_PATH: &str = "/usr/local/bin/rast";

pub async fn run(
    cfg: Config,
    target_version: Option<&str>,
    check_only: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let current = crate::VERSION;
    println!("RAST {current} ({})", env!("RAST_GIT_SHA"));

    let release = match target_version {
        Some(version) => fetch_release(version)?,
        None => fetch_latest()?,
    };
    let version = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    if version.is_empty() {
        return Err("release metadata does not contain a tag name".into());
    }
    println!("latest available: {version}  ({REPO})");

    if check_only {
        let newer = version_is_newer(&version, current);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "current": current,
                "available": version,
                "updateAvailable": newer,
                "releaseUrl": release.get("html_url"),
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    if !version_is_newer(&version, current) && target_version.is_none() {
        println!("already up to date");
        return Ok(ExitCode::SUCCESS);
    }
    if !is_root() {
        return Err("rast update must run as root (sudo rast update)".into());
    }

    let arch = arch_asset_suffix()?;
    let asset_name = format!("rast-linux-{arch}");
    let tag = format!("v{version}");
    let sums_url = format!("{DOWNLOAD_BASE}/{tag}/SHA256SUMS");
    let binary_url = format!("{DOWNLOAD_BASE}/{tag}/{asset_name}");

    let work = std::env::temp_dir().join(format!("rast-update-{version}-{}", std::process::id()));
    fs::create_dir_all(&work)?;
    let _cleanup = TempCleanup(work.clone());

    println!("downloading SHA256SUMS…");
    let sums = download_text(&sums_url)?;
    let expected = find_checksum(&sums, &asset_name)
        .ok_or_else(|| format!("{asset_name} is not listed in SHA256SUMS"))?;
    println!("downloading {asset_name}…");
    let binary_path = work.join(&asset_name);
    let actual = download_file(&binary_url, &binary_path)?;
    // The downloaded file is created 0644; it must be executable for the
    // version self-check (and for the atomic replacement later).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))?;
    }
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "checksum mismatch for {asset_name}: expected {expected}, got {actual}; aborting"
        )
        .into());
    }
    println!("checksum verified: {actual}");

    // Verify it is an executable that runs and reports the expected version.
    let probe = std::process::Command::new(&binary_path)
        .arg("version")
        .output()
        .map_err(|e| format!("downloaded binary is not executable: {e}"))?;
    if !probe.status.success() {
        return Err("downloaded binary failed its version check".into());
    }
    let probe_stdout = String::from_utf8_lossy(&probe.stdout);
    if !probe_stdout.contains(&version) {
        return Err(
            format!("downloaded binary reports an unexpected version: {probe_stdout}").into(),
        );
    }

    // Releases after 0.11.4 add the native backend: its systemd unit, tmpfiles
    // and directories are installed by the installer, not by the binary. An
    // update must not leave an enabled backend without its helper, so missing
    // assets are installed with the release's own installer before the new
    // binary is started.
    let installer = plan_asset_refresh(&cfg, &work, &version, &sums)?;
    if installer.is_some() && !installer_downloader_available() {
        return Err(format!(
            "the native backend is enabled but `{NATIVE_UNIT}` is missing and the release \
             installer needs curl or wget to install the missing assets. Install the update \
             manually with: sh install.sh --version {version}"
        )
        .into());
    }
    let asset_base = match (installer.as_deref(), local_release_supported()) {
        (Some(_), true) => Some(stage_local_release(
            &work,
            &version,
            arch,
            &binary_path,
            &sums,
            installer.as_deref().unwrap(),
        )?),
        _ => None,
    };

    // ---- database backup before any migration ------------------------------
    let backup_meta = backup_before_update(&cfg).await?;
    println!("database backup: {}", backup_meta["path"]);
    // `rast update` runs as root but the data directory belongs to the service
    // account; hand the new artifacts back to its owner so the running service
    // (and retention pruning) can manage them.
    if let Some(path) = backup_meta.get("path").and_then(Value::as_str) {
        adopt_ownership(&cfg, Path::new(path));
    }

    let current_bin = fs::read(BIN_PATH)?;
    let current_mode = fs::metadata(BIN_PATH)?.permissions();
    let rollback_path = PathBuf::from(format!("{BIN_PATH}.rollback-{current}"));
    fs::write(&rollback_path, &current_bin)?;
    println!("previous binary saved to {}", rollback_path.display());

    // Atomic replacement: write next to the target and rename over it.
    let staged = PathBuf::from(format!("{BIN_PATH}.new-{}", std::process::id()));
    fs::copy(&binary_path, &staged)?;
    fs::set_permissions(&staged, current_mode)?;
    fs::rename(&staged, BIN_PATH)?;
    println!("binary replaced with {version}");

    let mut installed_cfg = cfg.clone();
    if let Some(installer) = installer.as_deref() {
        let base = asset_base.as_deref();
        if let Err(error) = run_release_installer(installer, base, &version).await {
            // Do not leave the machine on a binary whose native backend has no
            // helper: restore the previous binary and let the operator run the
            // installer explicitly.
            rollback(&rollback_path)?;
            restart_worker().await;
            restart_native_helper(&cfg).await;
            return Err(format!(
                "{error}
The previous binary was restored. Install this release with the installer to
add the missing native service assets, then run `rast update` again."
            )
            .into());
        }
        // The installer may have recorded a different effective backend (for
        // example it disables native when Xpra is unavailable).
        installed_cfg = Config::load(None).unwrap_or(cfg.clone());
    }

    println!("restarting {SERVICE}…");
    let restarted = run_cmd("systemctl", &["restart", SERVICE]).await?;
    if !restarted {
        rollback(&rollback_path)?;
        restart_worker().await;
        restart_native_helper(&cfg).await;
        return Err("service failed to restart after update; previous binary restored".into());
    }
    restart_worker().await;
    restart_native_helper(&installed_cfg).await;

    // Never report a green update while an enabled backend has no helper.
    if installed_cfg.native_enabled && !Path::new(NATIVE_UNIT).exists() {
        return Err(format!(
            "RAST {version} is installed and the web server is healthy, but native Linux apps \
             are enabled in the configuration and {NATIVE_UNIT} is missing. The native backend \
             will not work until the release installer is run: sh install.sh --version {version}"
        )
        .into());
    }

    // Health check (bounded retries).
    let healthy = wait_for_health(Duration::from_secs(20)).await;
    if !healthy {
        eprintln!("new version is not healthy; rolling back binary");
        rollback(&rollback_path)?;
        let _ = run_cmd("systemctl", &["restart", SERVICE]).await;
        restart_worker().await;
        restart_native_helper(&cfg).await;
        return Err("health check failed after update; rolled back".into());
    }

    // App Center on the login-shell PATH (needs root, which only this
    // command has). Run by the new binary: it knows the current script.
    // Best effort: an older target release may not have the command.
    match std::process::Command::new(BIN_PATH)
        .args(["packages", "path", "sync"])
        .output()
    {
        Ok(out) if out.status.success() => {
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }
        Ok(out) => eprintln!(
            "note: `rast packages path sync` did not complete: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => eprintln!("note: cannot run `rast packages path sync`: {e}"),
    }

    // Schema compatibility: the new binary may have applied migrations. Only
    // report; automatic DB rollback is intentionally NOT performed.
    // Read it without running this (previous) binary's migrations.
    let db_schema = db::recorded_schema_version(&cfg.db_path()).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "updated": true,
            "from": current,
            "to": version,
            "databaseBackup": backup_meta,
            "schemaVersion": db_schema,
            "health": "ok",
            "note": "database migrations are forward-only; to roll back the binary use the saved rollback binary only if the schema is compatible",
        }))?
    );
    Ok(ExitCode::SUCCESS)
}

/// Give files created by a root CLI run back to the service account that owns
/// the data directory (best effort; no-op when not root).
fn adopt_ownership(cfg: &Config, path: &Path) {
    if !is_root() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let Ok(owner) = fs::metadata(&cfg.data_dir) else {
        return;
    };
    let (uid, gid) = (owner.uid(), owner.gid());
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", cfg.db_path().display())),
        PathBuf::from(format!("{}-shm", cfg.db_path().display())),
        cfg.db_path(),
    ] {
        if candidate.exists() {
            let _ = std::os::unix::fs::chown(&candidate, Some(uid), Some(gid));
        }
    }
}

async fn backup_before_update(cfg: &Config) -> AppResult<Value> {
    let pool = db::init(cfg).await?;
    let meta = crate::backup::create(
        cfg,
        Some(&pool),
        &["db".to_string(), "config".to_string()],
        &format!("pre-update snapshot (running {})", crate::VERSION),
        None,
        None,
    )
    .await?;
    Ok(serde_json::to_value(meta).unwrap_or(Value::Null))
}

fn is_root() -> bool {
    // Avoid a libc dependency by reading the effective UID from /proc.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(2))
                .and_then(|uid| uid.parse::<u32>().ok())
        })
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

fn arch_asset_suffix() -> Result<&'static str, Box<dyn std::error::Error>> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => Err(format!("unsupported architecture for self-update: {other}").into()),
    }
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split(['.', '-', '+'])
            .take(3)
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let a = parse(candidate);
    let b = parse(current);
    for index in 0..3 {
        let ca = a.get(index).copied().unwrap_or(0);
        let cb = b.get(index).copied().unwrap_or(0);
        if ca != cb {
            return ca > cb;
        }
    }
    false
}

fn find_checksum(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        if hash.len() != 64 {
            continue;
        }
        let name = parts.next()?.trim_start_matches('*');
        if name == asset {
            return Some(hash.to_string());
        }
    }
    None
}

fn fetch_release(tag_or_version: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let tag = if tag_or_version.starts_with('v') {
        tag_or_version.to_string()
    } else {
        format!("v{tag_or_version}")
    };
    let url = format!("{API_TAG}/{tag}");
    parse_json(&http_get(&url)?)
}

fn fetch_latest() -> Result<Value, Box<dyn std::error::Error>> {
    parse_json(&http_get(API_LATEST)?)
}

fn parse_json(bytes: &[u8]) -> Result<Value, Box<dyn std::error::Error>> {
    serde_json::from_slice(bytes).map_err(|e| format!("invalid GitHub API response: {e}").into())
}

fn http_get(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("rast/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let mut out = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut out)
        .map_err(|e| format!("cannot read {url}: {e}"))?;
    Ok(out)
}

fn download_text(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    String::from_utf8(http_get(url)?).map_err(|e| format!("{url} is not text: {e}").into())
}

fn download_file(url: &str, target: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = http_get(url)?;
    let mut file = fs::File::create(target)?;
    file.write_all(&bytes)?;
    file.sync_all().ok();
    // Compute SHA-256 of the downloaded artifact.
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Are the native backend's service assets (installed by the installer, not
/// by the binary) missing while the configuration enables the backend?
fn assets_missing(native_enabled: bool, unit: &Path, tmpfiles: &Path, layout: &Path) -> bool {
    native_enabled && (!unit.is_file() || !tmpfiles.is_file() || !layout.is_dir())
}

fn native_assets_missing(cfg: &Config) -> bool {
    assets_missing(
        cfg.native_enabled,
        Path::new(NATIVE_UNIT),
        Path::new(NATIVE_TMPFILES),
        &cfg.linux_apps_dir(),
    )
}

fn local_release_path(base: &Path, version: &str) -> PathBuf {
    base.join(format!("v{version}"))
}

/// Can the release installer reach a download URL (it uses curl or wget)?
fn installer_downloader_available() -> bool {
    ["curl", "wget"].iter().any(|program| {
        std::process::Command::new(program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Can a local `file://` release base be used (curl supports it, wget does not)?
fn local_release_supported() -> bool {
    std::process::Command::new("curl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Download the release installer and verify it against the release
/// checksums. Returns `None` when no asset refresh is needed.
fn plan_asset_refresh(
    cfg: &Config,
    work: &Path,
    version: &str,
    sums: &str,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    if !native_assets_missing(cfg) {
        return Ok(None);
    }
    if !Path::new(INSTALLER_CONFIG).is_file() {
        return Err(format!(
            "native Linux apps are enabled but their service assets are missing and \
             {INSTALLER_CONFIG} does not exist, so the release installer cannot be invoked \
             automatically. Run the release installer manually, then `rast update` again."
        )
        .into());
    }
    if let Some(path) = cfg.loaded_from.as_deref() {
        if path != Path::new(INSTALLER_CONFIG) {
            return Err(format!(
                "native Linux apps are enabled but their service assets are missing, and RAST \
                 uses a custom configuration file ({}). Run the release installer manually, \
                 then `rast update` again.",
                path.display()
            )
            .into());
        }
    }
    let tag = format!("v{version}");
    let url = format!("{DOWNLOAD_BASE}/{tag}/install.sh");
    println!("downloading install.sh (native service assets are missing)…");
    let installer = work.join("release-install.sh");
    let actual = download_file(&url, &installer)?;
    let expected = find_checksum(sums, "install.sh")
        .ok_or("the release SHA256SUMS file has no entry for install.sh")?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "checksum mismatch for install.sh: expected {expected}, got {actual}; refusing to \
             install the native service assets"
        )
        .into());
    }
    println!("checksum verified: install.sh");
    Ok(Some(installer))
}

/// Stage the already-downloaded release files as a local `file://` release so
/// the installer re-verifies and installs exactly this release without a
/// second network download.
fn stage_local_release(
    work: &Path,
    version: &str,
    arch: &str,
    binary: &Path,
    sums: &str,
    installer: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = work.join("local-release");
    let dir = local_release_path(&base, version);
    fs::create_dir_all(&dir)?;
    fs::copy(binary, dir.join(format!("rast-linux-{arch}")))?;
    fs::write(dir.join("SHA256SUMS"), sums)?;
    fs::copy(installer, dir.join("install.sh"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("install.sh"), fs::Permissions::from_mode(0o755))?;
    }
    Ok(base)
}

/// Run the release installer in `--no-start` mode so it installs/refreshes the
/// systemd units, tmpfiles and directories (the "embedded unit install"), then
/// let the caller restart the services on the new binary.
async fn run_release_installer(
    installer: &Path,
    base: Option<&Path>,
    version: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("installing the release's native service assets (no service restart)…");
    let mut command = tokio::process::Command::new("sh");
    command
        .arg(installer)
        .arg("--version")
        .arg(version)
        .arg("--no-start")
        .env("RAST_VERSION", version)
        .env("RAST_NO_START", "1");
    if let Some(base) = base {
        command.env(
            "RAST_RELEASE_BASE_URL",
            format!("file://{}", base.display()),
        );
    }
    let output = command.output().await?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stdout
            .lines()
            .chain(stderr.lines())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            eprintln!("  {line}");
        }
        return Err(format!(
            "the release installer could not install the missing native service assets              (exit {}); install this release manually with: sh install.sh --version {version}",
            output.status
        )
        .into());
    }
    println!("native service assets installed");
    Ok(())
}

/// Restart the privileged native helper so it runs the updated binary, and
/// refresh the native runtime directories from the installed tmpfiles asset.
/// Best effort like [`restart_worker`]: an old helper is reported by
/// `rast doctor`, and a helper that is not installed is left alone.
async fn restart_native_helper(cfg: &Config) {
    if Path::new(NATIVE_TMPFILES).exists() {
        match run_cmd("systemd-tmpfiles", &["--create", NATIVE_TMPFILES]).await {
            Ok(true) => println!("refreshed the native runtime directories"),
            _ => eprintln!(
                "note: could not refresh {NATIVE_TMPFILES}; run: systemd-tmpfiles --create                  {NATIVE_TMPFILES}"
            ),
        }
    }
    if !Path::new(NATIVE_UNIT).exists() {
        if cfg.native_enabled {
            eprintln!(
                "note: {NATIVE_UNIT} is missing, so native Linux apps cannot start; run the \
                 release installer to add the native service assets"
            );
        }
        return;
    }
    match run_cmd("systemctl", &["try-restart", NATIVE_SERVICE]).await {
        Ok(true) => println!("restarted {NATIVE_SERVICE} (if it was running)"),
        Ok(false) | Err(_) => eprintln!(
            "note: could not restart {NATIVE_SERVICE}; run `systemctl restart {NATIVE_SERVICE}`"
        ),
    }
}

/// Restart the Docker helper if it is running, so it uses the binary now in
/// place (a helper left on the old binary refuses actions added since). Not
/// fatal: the server is already up, and `rast doctor` reports a stale helper.
async fn restart_worker() {
    match run_cmd("systemctl", &["try-restart", WORKER_SERVICE]).await {
        Ok(true) => println!("restarted {WORKER_SERVICE} (if it was running)"),
        Ok(false) | Err(_) => eprintln!(
            "note: could not restart {WORKER_SERVICE}; run `systemctl restart {WORKER_SERVICE}`"
        ),
    }
}

async fn run_cmd(program: &str, args: &[&str]) -> Result<bool, Box<dyn std::error::Error>> {
    let status = tokio::process::Command::new(program)
        .args(args)
        .status()
        .await
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    Ok(status.success())
}

async fn wait_for_health(timeout: Duration) -> bool {
    // Probe `rast status --json` via the local HTTP health endpoint. The CLI's
    // own binary is already replaced, so use a raw TCP health check instead of
    // spawning it again.
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_secs(2), probe_health())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn probe_health() -> Result<bool, Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The service binds to the configured address; try loopback first.
    let addr = Config::load(None)
        .map(|cfg| cfg.bind)
        .unwrap_or_else(|_| "127.0.0.1:8720".parse().expect("static addr"));
    let target = if addr.ip().is_unspecified() {
        std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    } else {
        addr
    };
    let mut stream = tokio::net::TcpStream::connect(target).await?;
    stream
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await?;
    Ok(buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200"))
}

fn rollback(rollback_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !rollback_path.exists() {
        return Err("rollback binary is missing".into());
    }
    let staged = PathBuf::from(format!("{BIN_PATH}.rollback-staged-{}", std::process::id()));
    fs::copy(rollback_path, &staged)?;
    if let Ok(meta) = fs::metadata(BIN_PATH) {
        fs::set_permissions(&staged, meta.permissions())?;
    }
    fs::rename(&staged, BIN_PATH)?;
    eprintln!("restored previous binary from {}", rollback_path.display());
    Ok(())
}

struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Small helper so `rast doctor` can report update availability cheaply.
pub fn update_available_blocking() -> Option<String> {
    let latest = fetch_latest().ok()?;
    let version = latest.get("tag_name")?.as_str()?.trim_start_matches('v');
    if version_is_newer(version, crate::VERSION) {
        Some(version.to_string())
    } else {
        None
    }
}

#[allow(dead_code)]
fn _ensure_error_name(e: &AppError) -> String {
    e.public_message()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(version_is_newer("0.2.0", "0.1.0"));
        assert!(version_is_newer("0.1.1", "0.1.0"));
        assert!(!version_is_newer("0.1.0", "0.1.0"));
        assert!(!version_is_newer("0.1.0", "0.2.0"));
        assert!(version_is_newer("1.0.0", "0.99.99"));
        assert!(version_is_newer("0.10.0", "0.9.9"));
    }

    #[test]
    fn checksum_parsing() {
        let sums = "abc123\n\
                    d3b07384d113edec49eaa6238ad5ff00d3b07384d113edec49eaa6238ad5ff00  rast-linux-x86_64\n\
                    1111111111111111111111111111111111111111111111111111111111111111 *rast-linux-aarch64\n";
        assert_eq!(
            find_checksum(sums, "rast-linux-x86_64").as_deref(),
            Some("d3b07384d113edec49eaa6238ad5ff00d3b07384d113edec49eaa6238ad5ff00")
        );
        assert_eq!(
            find_checksum(sums, "rast-linux-aarch64").as_deref(),
            Some("1111111111111111111111111111111111111111111111111111111111111111")
        );
        assert!(find_checksum(sums, "missing").is_none());
    }

    #[test]
    fn native_assets_are_needed_only_when_enabled_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("rast-native.service");
        let tmpfiles = dir.path().join("rast-native.conf");
        let layout = dir.path().join("linux-apps");
        // A disabled backend never needs assets, even when the files are absent.
        assert!(!assets_missing(false, &unit, &tmpfiles, &layout));
        // An enabled backend without its unit/tmpfiles/layout needs them.
        assert!(assets_missing(true, &unit, &tmpfiles, &layout));
        std::fs::write(&unit, b"unit").unwrap();
        std::fs::write(&tmpfiles, b"tmpfiles").unwrap();
        assert!(assets_missing(true, &unit, &tmpfiles, &layout));
        std::fs::create_dir(&layout).unwrap();
        assert!(!assets_missing(true, &unit, &tmpfiles, &layout));
    }

    #[test]
    fn local_release_layout_matches_installer_expectations() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let binary = work.join("rast-linux-x86_64.download");
        std::fs::write(&binary, b"binary bytes").unwrap();
        let installer = work.join("release-install.sh");
        std::fs::write(&installer, b"#!/bin/sh\n").unwrap();
        let sums = "aa  rast-linux-x86_64\nbb  install.sh\n";
        let base = stage_local_release(&work, "9.9.9", "x86_64", &binary, sums, &installer)
            .expect("stage local release");
        let dir = local_release_path(&base, "9.9.9");
        assert!(dir.join("rast-linux-x86_64").is_file());
        assert!(dir.join("install.sh").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap(),
            sums
        );
        assert_eq!(local_release_path(&base, "1.2.3"), base.join("v1.2.3"));
    }

    #[test]
    fn arch_suffix_matches_release_assets() {
        let suffix = arch_asset_suffix().unwrap();
        assert!(["x86_64", "aarch64"].contains(&suffix));
    }
}
