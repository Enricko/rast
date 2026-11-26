//! Filesystem operations for managed runtimes (blocking; run them from
//! `spawn_blocking`).
//!
//! Layout under `<data_dir>/packages/`:
//!
//! ```text
//! runtimes/<id>/<version>/        one directory per installed version
//! runtimes/<id>/current -> <ver>  the RAST default version (symlink)
//! bin/<exe> -> ../runtimes/<id>/current/<path>   default shims (RAST-managed,
//!                                                 never added to the global PATH)
//! catalog/                        last verified catalog + signature
//! .staging/<job>/                 downloads and extraction in progress
//! .trash/                         directories being replaced or removed
//! ```
//!
//! Everything happens inside the RAST service account's data directory: the
//! operating system's runtimes, package-manager state and global `PATH` are
//! never touched. A version is moved into `runtimes/` only after its download
//! matched the pinned SHA-256, the archive passed path/link validation and
//! every declared version check succeeded; the default only changes after
//! that, so a failed install or update leaves the current version usable.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use serde::Serialize;
use walkdir::WalkDir;

use super::{
    archive,
    manifest::{self, Artifact, Manifest},
    net::{self, OriginPolicy},
    sandbox,
};

pub const INSTALL_MARKER: &str = ".rast-install.json";
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const CHECK_OUTPUT_LIMIT: usize = 16 * 1024;
/// Free space a download or extraction must leave on the data filesystem.
const DISK_RESERVE: u64 = 512 * 1024 * 1024;

/// Bytes available to unprivileged users on the filesystem holding `path`.
fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: valid NUL-terminated path and a zeroed out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn runtimes(&self) -> PathBuf {
        self.root.join("runtimes")
    }
    pub fn package_dir(&self, id: &str) -> PathBuf {
        self.runtimes().join(id)
    }
    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.package_dir(id).join(version)
    }
    pub fn current_link(&self, id: &str) -> PathBuf {
        self.package_dir(id).join("current")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn catalog_dir(&self) -> PathBuf {
        self.root.join("catalog")
    }
    pub fn staging(&self) -> PathBuf {
        self.root.join(".staging")
    }
    pub fn trash(&self) -> PathBuf {
        self.root.join(".trash")
    }

    pub fn ensure(&self) -> io::Result<()> {
        for dir in [
            self.runtimes(),
            self.bin_dir(),
            self.catalog_dir(),
            self.staging(),
            self.trash(),
        ] {
            fs::create_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Remove leftovers of interrupted jobs (staging and trash are always
    /// RAST's own temporary directories). Trees set aside by an unfinished
    /// replacement are kept for [`Layout::displaced`].
    pub fn clean_temporary(&self) {
        for dir in [self.staging(), self.trash()] {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(DISPLACED_PREFIX)
                    {
                        continue;
                    }
                    let result = if path.is_dir() && !path.is_symlink() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                    if let Err(e) = result {
                        tracing::warn!(path = %path.display(), error = %e, "cannot clean package staging");
                    }
                }
            }
        }
    }
}

impl Layout {
    /// Version trees set aside by a replacement that RAST did not settle
    /// (it stopped between moving the old tree aside and recording the new
    /// one). See [`settle_replacement`].
    pub fn displaced(&self) -> Vec<Displaced> {
        let Ok(entries) = fs::read_dir(self.trash()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(DISPLACED_PREFIX)
                || path.is_symlink()
            {
                continue;
            }
            match read_marker(&path) {
                Some(marker) if path.is_dir() => out.push(Displaced { path, marker }),
                _ => {
                    let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
                }
            }
        }
        out
    }
}

/// Prefix of trees set aside during a replacement (`.trash/displaced.<uuid>`).
const DISPLACED_PREFIX: &str = "displaced.";

/// Identity of an installed tree, from its install marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub id: String,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Displaced {
    pub path: PathBuf,
    pub marker: Marker,
}

/// Read and sanity-check the install marker of a version tree.
pub fn read_marker(dir: &Path) -> Option<Marker> {
    let bytes = fs::read(dir.join(INSTALL_MARKER)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let field = |k: &str| value.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let marker = Marker {
        id: field("id")?,
        version: field("version")?,
        sha256: field("sha256")?,
    };
    let safe = |v: &str| {
        !v.is_empty()
            && v.len() <= 64
            && v != "."
            && v != ".."
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
    };
    (safe(&marker.id) && safe(&marker.version)).then_some(marker)
}

/// Finish a replacement: keep the new tree at `target` and delete the old one
/// at `aside`, or (when the new one was not recorded) put the old one back.
pub fn settle_replacement(target: &Path, aside: &Path, keep_new: bool) -> Result<(), String> {
    if keep_new {
        return fs::remove_dir_all(aside).map_err(|e| format!("cannot remove old files: {e}"));
    }
    let parent = aside.parent().unwrap_or(aside);
    let unrecorded = parent.join(format!("unrecorded-{}", uuid::Uuid::new_v4()));
    if target.exists() || target.is_symlink() {
        fs::rename(target, &unrecorded).map_err(|e| format!("cannot move new files aside: {e}"))?;
    }
    fs::rename(aside, target).map_err(|e| format!("cannot restore the previous files: {e}"))?;
    let _ = fs::remove_dir_all(&unrecorded);
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub label: String,
    pub command: String,
    pub output: String,
}

/// Outcome of a successful install.
#[derive(Debug, Clone)]
pub struct Installed {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub checks: Vec<CheckResult>,
    /// The tree this install replaced, kept until the database records the
    /// new one; settle it with [`settle_replacement`].
    pub displaced: Option<PathBuf>,
}

/// Progress reporting from the blocking installer to the job.
pub trait Reporter {
    fn phase(&mut self, done: u64, total: u64, message: &str);
    /// Shared cancellation flag of the job.
    fn cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool>;
    fn cancelled(&self) -> bool {
        self.cancel_flag()
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MB)
}

/// Download, verify, extract, check and move one version into place.
/// `untrusted` marks an imported (third-party) package: its checks must run
/// under Landlock.
pub fn install_version(
    layout: &Layout,
    manifest: &Manifest,
    artifact: &Artifact,
    policy: &OriginPolicy,
    job_id: &str,
    untrusted: bool,
    reporter: &mut dyn Reporter,
) -> Result<Installed, String> {
    layout
        .ensure()
        .map_err(|e| format!("cannot prepare package directory: {e}"))?;
    let stage = layout.staging().join(job_id);
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(|e| format!("cannot reset staging: {e}"))?;
    }
    fs::create_dir_all(&stage).map_err(|e| format!("cannot create staging: {e}"))?;
    let result = install_staged(
        layout, manifest, artifact, policy, &stage, untrusted, reporter,
    );
    // Staging is ours; remove it whatever happened.
    let _ = fs::remove_dir_all(&stage);
    result
}

/// The progress message from which an install can no longer be cancelled.
pub const CANCEL_ENDS_AT: &str = "Moving into place";

fn install_staged(
    layout: &Layout,
    manifest: &Manifest,
    artifact: &Artifact,
    policy: &OriginPolicy,
    stage: &Path,
    untrusted: bool,
    reporter: &mut dyn Reporter,
) -> Result<Installed, String> {
    // Never fill the disk: every archive must fit with room to spare.
    let total_size = artifact.total_size();
    let free = available_bytes(stage);
    if let Some(free) = free {
        if free < total_size.saturating_add(DISK_RESERVE) {
            return Err(format!(
                "not enough free disk space: the download needs {} and {} must stay free ({} available)",
                human(total_size),
                human(DISK_RESERVE),
                human(free)
            ));
        }
    }
    // Cancel works until the files are moved into place (step 5); after
    // that the install finishes. `CANCEL_ENDS_AT` names that phase.
    if reporter.cancelled() {
        return Err("cancelled".into());
    }
    // 1./2. Download each archive with origin checks (size and SHA-256 must
    // match the pin), then extract it into the shared staging root with path
    // and link validation. Multi-component runtimes (Rust) merge several
    // archives into one standalone prefix.
    let root = stage.join("root");
    fs::create_dir(&root).map_err(|e| format!("cannot create staging root: {e}"))?;
    let flag = reporter.cancel_flag();
    let cancelled = || flag.load(std::sync::atomic::Ordering::Relaxed);
    let parts = artifact.archives();
    let count = parts.len();
    let mut stats = archive::ExtractStats::default();
    let mut downloaded = 0u64;
    for (index, part) in parts.iter().enumerate() {
        if reporter.cancelled() {
            return Err("cancelled".into());
        }
        let label = if count == 1 {
            "archive".to_string()
        } else {
            format!("component {}/{count}", index + 1)
        };
        let download = stage.join(format!("artifact-{index}.{}", part.format));
        reporter.phase(
            downloaded,
            total_size,
            &format!(
                "Downloading {label} ({} of {})",
                human(downloaded),
                human(total_size)
            ),
        );
        let base = downloaded;
        {
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut progress = |done: u64, total: u64| {
                if last.elapsed() >= Duration::from_millis(300) || done == total {
                    last = Instant::now();
                    let overall = base + done;
                    reporter.phase(
                        overall,
                        total_size,
                        &format!(
                            "Downloading {label} ({} of {})",
                            human(overall),
                            human(total_size)
                        ),
                    );
                }
            };
            net::download_verified(
                part.url,
                policy,
                &download,
                part.size,
                part.sha256,
                &mut progress,
                &cancelled,
            )?;
        }
        downloaded += part.size;
        reporter.phase(downloaded, total_size, "Verified SHA-256 and size");
        if reporter.cancelled() {
            return Err("cancelled".into());
        }
        let room = available_bytes(stage)
            .map(|free| free.saturating_sub(DISK_RESERVE))
            .unwrap_or(u64::MAX);
        // A phar artifact is one verified single file (Composer): write it
        // into the prefix, never execute it. Everything else is extracted.
        if part.format == "phar" {
            let phar = artifact.phar.as_ref().ok_or_else(|| {
                "an artifact with format \"phar\" needs a phar destination path".to_string()
            })?;
            install_phar(&root, &download, &phar.path)?;
            stats.files += 1;
        } else {
            let files_before = stats.files;
            let part_stats = archive::extract_archive(
                &download,
                &root,
                part.format,
                part.strip_components,
                room,
                &mut |s| {
                    reporter.phase(
                        0,
                        0,
                        &format!("Extracting {label} ({} files)", files_before + s.files),
                    )
                },
                &cancelled,
            )?;
            stats.files += part_stats.files;
            stats.dirs += part_stats.dirs;
            stats.links += part_stats.links;
            stats.bytes += part_stats.bytes;
            let _ = fs::remove_file(&download);
        }
    }

    // 3. A phar artifact's executables are generated `bin/` wrappers that run
    // the verified phar with the PHP interpreter found on PATH. They are
    // written from fixed text (nothing from the manifest is shell-expanded).
    if let Some(phar) = artifact.phar.as_ref() {
        write_phar_wrappers(&root, &manifest.runtime.executables, &phar.path)?;
    }

    // 4. Executables must exist inside the package and be executable.
    reporter.phase(0, 0, "Checking executables");
    for exe in &manifest.runtime.executables {
        archive::executable_inside(&root, &exe.path)
            .map_err(|e| format!("executable {}: {e}", exe.name))?;
    }
    // 5. Version checks in the staging location.
    if reporter.cancelled() {
        return Err("cancelled".into());
    }
    let home = stage.join("home");
    fs::create_dir_all(&home).ok();
    run_checks(manifest, &root, &home, untrusted, Some(layout))?;
    if reporter.cancelled() {
        return Err("cancelled".into());
    }

    // 6. Record what was installed, then move into place. `sha256` stays the
    // primary archive digest the database records; component digests are
    // kept for diagnostics.
    let archives: Vec<serde_json::Value> = artifact
        .archives()
        .iter()
        .map(|part| {
            serde_json::json!({
                "name": part.name,
                "url": part.url,
                "sha256": part.sha256,
                "size": part.size,
                "format": part.format,
            })
        })
        .collect();
    let marker = serde_json::json!({
        "id": manifest.id,
        "version": manifest.version,
        "revision": manifest.revision,
        "target": artifact.target,
        "sha256": artifact.sha256,
        "url": artifact.url,
        "archives": archives,
        "installedAt": crate::db::now_ms(),
        "files": stats.files,
    });
    fs::write(
        root.join(INSTALL_MARKER),
        serde_json::to_vec_pretty(&marker).unwrap_or_default(),
    )
    .map_err(|e| format!("cannot write install marker: {e}"))?;
    reporter.phase(0, 0, CANCEL_ENDS_AT);
    let target = layout.version_dir(&manifest.id, &manifest.version);
    fs::create_dir_all(layout.package_dir(&manifest.id))
        .map_err(|e| format!("cannot create package directory: {e}"))?;
    let displaced = if target.exists() || target.is_symlink() {
        // An older revision (or a leftover): move it aside so a failure, or
        // a stop before the new tree is recorded, can put it back.
        let aside = layout
            .trash()
            .join(format!("{DISPLACED_PREFIX}{}", uuid::Uuid::new_v4()));
        fs::rename(&target, &aside)
            .map_err(|e| format!("cannot move previous files aside: {e}"))?;
        Some(aside)
    } else {
        None
    };
    if let Err(e) = fs::rename(&root, &target) {
        if let Some(aside) = &displaced {
            let _ = fs::rename(aside, &target);
        }
        return Err(format!("cannot move the version into place: {e}"));
    }
    // 7. Check again from the final location (relocation must not break it).
    let checks = match run_checks(manifest, &target, &home, untrusted, Some(layout)) {
        Ok(checks) => checks,
        Err(e) => {
            let failed = layout
                .trash()
                .join(format!("failed-{}", uuid::Uuid::new_v4()));
            let _ = fs::rename(&target, &failed);
            if let Some(aside) = &displaced {
                let _ = fs::rename(aside, &target);
            }
            let _ = fs::remove_dir_all(&failed);
            return Err(format!("check after installation failed: {e}"));
        }
    };
    Ok(Installed {
        size_bytes: dir_size(&target),
        path: target,
        checks,
        displaced,
    })
}

/// Write a verified single-file phar into a fresh staging root with mode
/// 0644. The installer never executes it; the version checks only run the
/// generated `bin/` wrapper (with the PHP on `PATH`).
pub fn install_phar(root: &Path, download: &Path, path: &str) -> Result<(), String> {
    if !manifest::is_phar_path(path) {
        return Err(format!(
            "phar.path {path:?} must be a relative path of [A-Za-z0-9._+-] components"
        ));
    }
    let parts: Vec<&str> = path.split('/').collect();
    let mut parent = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("phar.path {path:?} is below a file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).ok();
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", parent.display())),
        }
    }
    let dest = root.join(path);
    if dest.exists() || dest.is_symlink() {
        return Err(format!("phar.path {path:?} already exists in the package"));
    }
    fs::rename(download, &dest).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("cannot set permissions on {}: {e}", dest.display()))?;
    Ok(())
}

/// Generate the `bin/` wrappers of a phar package. The script is fixed text
/// plus the validated phar path, resolved relative to the wrapper's own
/// location so it keeps working after the staging tree is moved into place:
///
/// ```text
/// #!/bin/sh
/// root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
/// exec php "$root/<phar path>" "$@"
/// ```
pub fn write_phar_wrappers(
    root: &Path,
    executables: &[manifest::Executable],
    phar_path: &str,
) -> Result<(), String> {
    if !manifest::is_phar_path(phar_path) {
        return Err(format!(
            "phar.path {phar_path:?} must be a relative path of [A-Za-z0-9._+-] components"
        ));
    }
    let bin = root.join("bin");
    fs::create_dir_all(&bin).map_err(|e| format!("cannot create {}: {e}", bin.display()))?;
    for exe in executables {
        // The wrapper derives the prefix from `bin/..`; anything else could
        // point outside the package.
        if !manifest::is_wrapper_path(&exe.path) {
            return Err(format!(
                "executable {} of a phar package must be a wrapper in bin/",
                exe.name
            ));
        }
        if exe.path == phar_path {
            return Err(format!(
                "executable {} would overwrite the phar file",
                exe.name
            ));
        }
        let body = format!(
            "#!/bin/sh\n\
             # Generated by RAST: runs the verified phar with the PHP on PATH.\n\
             root=$(CDPATH= cd -- \"$(dirname -- \"$0\")/..\" && pwd) || exit 1\n\
             exec php \"$root/{phar_path}\" \"$@\"\n"
        );
        let wrapper = root.join(&exe.path);
        fs::write(&wrapper, body)
            .map_err(|e| format!("cannot write {}: {e}", wrapper.display()))?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot set permissions on {}: {e}", wrapper.display()))?;
    }
    Ok(())
}

/// Run every declared check: no shell, cleared environment, timeout, and the
/// confinement described in [`sandbox`] (process group, seccomp, Landlock).
/// `untrusted` (imported packages) makes Landlock mandatory.
///
/// With `layout`, a package from the official catalog also finds the runtimes
/// already installed by App Center, ahead of the system's: a tool that runs
/// on another runtime (Composer on PHP) is checked with the interpreter it
/// will really use. Imported packages never see them.
pub fn run_checks(
    manifest: &Manifest,
    root: &Path,
    home: &Path,
    untrusted: bool,
    layout: Option<&Layout>,
) -> Result<Vec<CheckResult>, String> {
    let mut results = Vec::new();
    let bin_path = root.join("bin");
    let layout = layout.filter(|_| !untrusted);
    let peers: Vec<PathBuf> = layout
        .map(|layout| vec![layout.bin_dir(), layout.runtimes()])
        .unwrap_or_default();
    let path = match layout {
        Some(layout) => format!(
            "{}:{}:/usr/local/bin:/usr/bin:/bin",
            bin_path.display(),
            layout.bin_dir().display()
        ),
        None => format!("{}:/usr/local/bin:/usr/bin:/bin", bin_path.display()),
    };
    let confinement = sandbox::Confinement::with_peers(root, home, untrusted, &peers)?;
    // Runtime env hints (JAVA_HOME, DOTNET_ROOT, …) apply to the checks too;
    // {root} is the staging or final prefix. They are set before the fixed
    // entries, so a hint can never replace PATH/HOME/TMPDIR.
    let hinted: Vec<(String, String)> = manifest
        .runtime
        .env
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value.replace("{root}", &root.display().to_string()),
            )
        })
        .collect();
    for check in &manifest.runtime.checks {
        let exe = manifest
            .executable(&check.executable)
            .ok_or_else(|| format!("check uses unknown executable {}", check.executable))?;
        let program = archive::executable_inside(root, &exe.path)?;
        let label = check
            .label
            .clone()
            .unwrap_or_else(|| check.executable.clone());
        let command = std::iter::once(check.executable.clone())
            .chain(check.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let output = sandbox::run(
            Command::new(&program)
                .args(&check.args)
                .current_dir(root)
                .env_clear()
                .envs(hinted.iter().map(|(name, value)| (name, value)))
                .env("PATH", &path)
                .env("HOME", home)
                .env("LANG", "C.UTF-8")
                .env("TMPDIR", home)
                // The RAST service forbids writable+executable memory; V8-based
                // runtimes run without JIT for their checks.
                .env("NODE_OPTIONS", "--jitless")
                .env("npm_config_update_notifier", "false")
                .env("npm_config_cache", home.join("npm-cache")),
            &confinement,
            home,
            CHECK_TIMEOUT,
            CHECK_OUTPUT_LIMIT,
        )
        .map_err(|e| format!("{label} check ({command}) failed: {e}"))?;
        if !output.contains(&check.expect) {
            return Err(format!(
                "{label} check ({command}) printed {:?}, expected it to contain {:?}",
                first_line(&output),
                check.expect
            ));
        }
        results.push(CheckResult {
            label,
            command,
            output: first_line(&output),
        });
    }
    Ok(results)
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

pub fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn replace_symlink(target: &str, link: &Path) -> io::Result<()> {
    let tmp = link.with_file_name(format!(
        ".{}.tmp-{}",
        link.file_name().and_then(|n| n.to_str()).unwrap_or("link"),
        uuid::Uuid::new_v4()
    ));
    std::os::unix::fs::symlink(target, &tmp)?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activation {
    /// Shims created or updated in `packages/bin`.
    pub shims: Vec<String>,
    /// Executable names already provided by another package.
    pub conflicts: Vec<String>,
}

/// Point `current` and the shims at `version` (atomic renames). On failure
/// the previous `current` link is restored and shims created by this call
/// are removed, so the files keep matching the recorded default.
pub fn activate(
    layout: &Layout,
    manifest: &Manifest,
    previous: Option<&Manifest>,
) -> Result<Activation, String> {
    let dir = layout.version_dir(&manifest.id, &manifest.version);
    if !dir.is_dir() {
        return Err(format!(
            "{} {} is not installed",
            manifest.name, manifest.version
        ));
    }
    fs::create_dir_all(layout.bin_dir())
        .map_err(|e| format!("cannot create bin directory: {e}"))?;
    let current = layout.current_link(&manifest.id);
    let before = fs::read_link(&current).ok();
    replace_symlink(&manifest.version, &current)
        .map_err(|e| format!("cannot switch the default version: {e}"))?;
    let mut out = Activation::default();
    let mut created = Vec::new();
    let own_prefix = format!("../runtimes/{}/", manifest.id);
    let result = (|| {
        for exe in &manifest.runtime.executables {
            let shim = layout.bin_dir().join(&exe.name);
            let existed = match fs::read_link(&shim) {
                Ok(existing) => {
                    if !existing.to_string_lossy().starts_with(&own_prefix) {
                        out.conflicts.push(exe.name.clone());
                        continue;
                    }
                    true
                }
                Err(_) if shim.exists() => {
                    out.conflicts.push(exe.name.clone());
                    continue;
                }
                Err(_) => false,
            };
            let target = format!("../runtimes/{}/current/{}", manifest.id, exe.path);
            replace_symlink(&target, &shim)
                .map_err(|e| format!("cannot create shim {}: {e}", exe.name))?;
            if !existed {
                created.push(shim);
            }
            out.shims.push(exe.name.clone());
        }
        Ok::<(), String>(())
    })();
    if let Err(e) = result {
        for shim in created {
            let _ = fs::remove_file(shim);
        }
        let _ = match &before {
            Some(target) => replace_symlink(&target.to_string_lossy(), &current),
            None => fs::remove_file(&current),
        };
        return Err(e);
    }
    // Shims of the previous default that the new version no longer provides.
    if let Some(prev) = previous {
        for exe in &prev.runtime.executables {
            if manifest.executable(&exe.name).is_none() {
                remove_own_shim(layout, &manifest.id, &exe.name);
            }
        }
    }
    Ok(out)
}

fn remove_own_shim(layout: &Layout, id: &str, name: &str) {
    let shim = layout.bin_dir().join(name);
    if let Ok(target) = fs::read_link(&shim) {
        if target
            .to_string_lossy()
            .starts_with(&format!("../runtimes/{id}/"))
        {
            let _ = fs::remove_file(&shim);
        }
    }
}

/// Remove the default link and shims of a package (no default left).
pub fn deactivate(layout: &Layout, manifest: &Manifest) {
    let _ = fs::remove_file(layout.current_link(&manifest.id));
    for exe in &manifest.runtime.executables {
        remove_own_shim(layout, &manifest.id, &exe.name);
    }
}

/// Remove one installed version directory (moved aside first, then deleted).
pub fn remove_version(layout: &Layout, id: &str, version: &str) -> Result<(), String> {
    let dir = layout.version_dir(id, version);
    if !dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(layout.trash()).ok();
    let aside = layout
        .trash()
        .join(format!("{id}-{version}-{}", uuid::Uuid::new_v4()));
    fs::rename(&dir, &aside).map_err(|e| format!("cannot remove {id} {version}: {e}"))?;
    fs::remove_dir_all(&aside).map_err(|e| format!("cannot delete {id} {version}: {e}"))?;
    // Drop the package directory once nothing is left in it.
    let pkg = layout.package_dir(id);
    if let Ok(mut entries) = fs::read_dir(&pkg) {
        if entries.next().is_none() {
            let _ = fs::remove_dir(&pkg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::archive::tests::{build, Item};

    fn manifest_for(version: &str, expect: &str) -> Manifest {
        let mut v = crate::packages::manifest::tests::sample();
        v["id"] = "demo".into();
        v["version"] = version.into();
        v["runtime"] = serde_json::json!({
            "executables": [{"name": "demo", "path": "bin/demo"}],
            "checks": [{"executable": "demo", "args": ["--version"], "expect": expect}]
        });
        serde_json::from_value(v).unwrap()
    }

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"demo {version}\"\n").into_bytes()
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn checks_run_without_shell_and_validate_output() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("a.tar.gz");
        let body = script("1.0.0");
        build(
            &archive_path,
            &[
                Item::Dir("demo/"),
                Item::Dir("demo/bin/"),
                Item::File("demo/bin/demo", &body, 0o755),
            ],
        );
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        archive::extract_tar_gz(&archive_path, &root, 1, u64::MAX, &mut |_| {}, &|| false).unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let ok = run_checks(
            &manifest_for("1.0.0", "demo 1.0.0"),
            &root,
            &home,
            false,
            None,
        )
        .unwrap();
        assert_eq!(ok[0].output, "demo 1.0.0");
        let err = run_checks(
            &manifest_for("1.0.0", "demo 2.0.0"),
            &root,
            &home,
            false,
            None,
        )
        .unwrap_err();
        assert!(err.contains("expected it to contain"), "{err}");
    }

    struct Quiet(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Reporter for Quiet {
        fn phase(&mut self, _: u64, _: u64, _: &str) {}
        fn cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
            self.0.clone()
        }
    }

    /// A `file://` artifact for `version` whose script prints `prints`.
    fn local_artifact(dir: &Path, version: &str, prints: &str) -> Artifact {
        artifact_with(dir, version, &script(prints))
    }

    /// A `file://` artifact whose `bin/demo` is `body`.
    fn artifact_with(dir: &Path, version: &str, body: &[u8]) -> Artifact {
        use sha2::{Digest, Sha256};
        let path = dir.join(format!("demo-{version}-{}.tar.gz", uuid::Uuid::new_v4()));
        build(
            &path,
            &[
                Item::Dir("demo/"),
                Item::Dir("demo/bin/"),
                Item::File("demo/bin/demo", body, 0o755),
            ],
        );
        let bytes = fs::read(&path).unwrap();
        let mut a = crate::packages::manifest::tests::sample_artifact();
        a.url = format!("file://{}", path.display());
        a.sha256 = hex::encode(Sha256::digest(&bytes));
        a.size = bytes.len() as u64;
        a
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn staged_install_verifies_and_failures_keep_the_working_version() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("packages"));
        let policy = OriginPolicy {
            hosts: vec![],
            allow_file: true,
        };
        let mut quiet = Quiet(Default::default());
        let m = manifest_for("1.0.0", "demo 1.0.0");

        let good = local_artifact(dir.path(), "1.0.0", "1.0.0");
        // As an imported package when the kernel can confine it.
        let untrusted = sandbox::landlock_abi().is_some();
        let installed =
            install_version(&layout, &m, &good, &policy, "job-1", untrusted, &mut quiet).unwrap();
        assert_eq!(installed.path, layout.version_dir("demo", "1.0.0"));
        assert_eq!(installed.checks[0].output, "demo 1.0.0");
        assert!(installed.path.join(INSTALL_MARKER).is_file());
        assert!(!layout.staging().join("job-1").exists());

        // Integrity: a wrong digest is refused before anything is extracted.
        let mut tampered = local_artifact(dir.path(), "2.0.0", "2.0.0");
        tampered.sha256 = "0".repeat(64);
        let two = manifest_for("2.0.0", "demo 2.0.0");
        let err = install_version(
            &layout, &two, &tampered, &policy, "job-2", false, &mut quiet,
        )
        .unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!layout.version_dir("demo", "2.0.0").exists());

        // Origin: file:// is refused unless the policy allows it.
        let strict = OriginPolicy::new(["github.com"]);
        let err =
            install_version(&layout, &m, &good, &strict, "job-3", false, &mut quiet).unwrap_err();
        assert!(err.contains("not an allowed"), "{err}");

        // A reinstall whose check fails leaves the installed files untouched.
        let broken = local_artifact(dir.path(), "1.0.0", "9.9.9");
        let err =
            install_version(&layout, &m, &broken, &policy, "job-4", false, &mut quiet).unwrap_err();
        assert!(err.contains("expected it to contain"), "{err}");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let still =
            run_checks(&m, &layout.version_dir("demo", "1.0.0"), &home, false, None).unwrap();
        assert_eq!(still[0].output, "demo 1.0.0");

        // Cancellation stops the job without leaving a version behind.
        let cancelled = Quiet(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        )));
        let mut cancelled = cancelled;
        let three = manifest_for("3.0.0", "demo 3.0.0");
        let art = local_artifact(dir.path(), "3.0.0", "3.0.0");
        assert!(install_version(
            &layout,
            &three,
            &art,
            &policy,
            "job-5",
            false,
            &mut cancelled
        )
        .is_err());
        assert!(!layout.version_dir("demo", "3.0.0").exists());
    }

    #[test]
    fn activation_switches_default_and_shims() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure().unwrap();
        for version in ["1.0.0", "2.0.0"] {
            let vdir = layout.version_dir("demo", version).join("bin");
            fs::create_dir_all(&vdir).unwrap();
            fs::write(vdir.join("demo"), script(version)).unwrap();
        }
        let one = manifest_for("1.0.0", "demo 1.0.0");
        let two = manifest_for("2.0.0", "demo 2.0.0");
        activate(&layout, &one, None).unwrap();
        assert_eq!(
            fs::read_link(layout.current_link("demo")).unwrap(),
            PathBuf::from("1.0.0")
        );
        let shim = layout.bin_dir().join("demo");
        assert_eq!(
            fs::read_link(&shim).unwrap(),
            PathBuf::from("../runtimes/demo/current/bin/demo")
        );
        activate(&layout, &two, Some(&one)).unwrap();
        assert_eq!(
            fs::read_link(layout.current_link("demo")).unwrap(),
            PathBuf::from("2.0.0")
        );
        assert!(fs::read_to_string(&shim).unwrap().contains("2.0.0"));

        // Another package cannot take over an existing shim.
        let mut other = manifest_for("1.0.0", "x");
        other.id = "other".into();
        fs::create_dir_all(layout.version_dir("other", "1.0.0")).unwrap();
        let result = activate(&layout, &other, None).unwrap();
        assert_eq!(result.conflicts, vec!["demo".to_string()]);

        remove_version(&layout, "demo", "1.0.0").unwrap();
        assert!(!layout.version_dir("demo", "1.0.0").exists());
        assert!(layout.version_dir("demo", "2.0.0").exists());
        deactivate(&layout, &two);
        assert!(!shim.exists() && !shim.is_symlink());
    }

    #[test]
    fn imported_checks_cannot_reach_the_rast_data_or_network() {
        if sandbox::landlock_abi().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // The RAST data directory: database next to the package tree.
        let data = dir.path().join("data");
        let layout = Layout::new(data.join("packages"));
        let db = data.join("rast.db");
        fs::create_dir_all(&data).unwrap();
        fs::write(&db, "sessions and password hashes").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if !Path::new("/bin/bash").is_file() {
            return;
        }
        let body = format!(
            "#!/bin/bash\n\
             r=$(cat {db} 2>/dev/null && echo LEAKED-DB)\n\
             w=$(echo x 2>/dev/null > {data}/planted && echo PLANTED)\n\
             n=$( (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo CONNECTED)\n\
             echo \"demo 1.0.0 $r $w $n\"\n",
            db = db.display(),
            data = data.display(),
        );
        // Control: unconfined, the same script reaches all three.
        let control = dir.path().join("control.sh");
        fs::write(&control, &body).unwrap();
        let out = Command::new("/bin/bash").arg(&control).output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.contains("LEAKED-DB") && out.contains("PLANTED") && out.contains("CONNECTED"),
            "control: {out}"
        );
        let _ = listener.accept().unwrap();
        fs::remove_file(data.join("planted")).unwrap();

        let art = artifact_with(dir.path(), "1.0.0", body.as_bytes());
        let m = manifest_for("1.0.0", "demo 1.0.0");
        let policy = OriginPolicy {
            hosts: vec![],
            allow_file: true,
        };
        let mut quiet = Quiet(Default::default());
        let installed =
            install_version(&layout, &m, &art, &policy, "hostile", true, &mut quiet).unwrap();
        let out = &installed.checks[0].output;
        assert!(out.starts_with("demo 1.0.0"), "{out}");
        for leak in ["LEAKED-DB", "PLANTED", "CONNECTED"] {
            assert!(!out.contains(leak), "{leak}: {out}");
        }
        assert!(!data.join("planted").exists());
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "the check reached the listener");
    }

    fn fake_version(layout: &Layout, version: &str, sha: &str) -> PathBuf {
        let dir = layout.version_dir("demo", version);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/demo"), "x").unwrap();
        fs::write(
            dir.join(INSTALL_MARKER),
            serde_json::json!({"id": "demo", "version": version, "sha256": sha}).to_string(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_failed_activation_restores_the_previous_default() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: plain getter.
        if unsafe { libc::geteuid() } == 0 {
            return; // permissions do not stop root
        }
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("packages"));
        layout.ensure().unwrap();
        fake_version(&layout, "1.0.0", "one");
        fake_version(&layout, "2.0.0", "two");
        let one = manifest_for("1.0.0", "demo 1.0.0");
        let two = manifest_for("2.0.0", "demo 2.0.0");
        activate(&layout, &one, None).unwrap();
        // Shims cannot be written: the switch must be undone.
        fs::set_permissions(layout.bin_dir(), fs::Permissions::from_mode(0o555)).unwrap();
        let err = activate(&layout, &two, Some(&one));
        fs::set_permissions(layout.bin_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.unwrap_err().contains("cannot create shim"));
        assert_eq!(
            fs::read_link(layout.current_link("demo")).unwrap(),
            Path::new("1.0.0")
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn runtime_env_hints_apply_to_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("bin")).unwrap();
        let demo = root.join("bin/demo");
        fs::write(&demo, "#!/bin/sh\necho \"demo 1.0.0 $DEMO_HOME\"\n").unwrap();
        fs::set_permissions(&demo, fs::Permissions::from_mode(0o755)).unwrap();
        let mut v = crate::packages::manifest::tests::sample();
        v["id"] = "demo".into();
        v["version"] = "1.0.0".into();
        v["runtime"] = serde_json::json!({
            "executables": [{"name": "demo", "path": "bin/demo"}],
            "checks": [{"executable": "demo", "args": [], "expect": "demo 1.0.0"}],
            "env": {"DEMO_HOME": "{root}"},
            "pathDirs": ["bin"]
        });
        let m = crate::packages::manifest::parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let checks = run_checks(&m, &root, &home, false, None).unwrap();
        assert_eq!(checks[0].output, format!("demo 1.0.0 {}", root.display()));
    }

    /// A tool that runs on another runtime (Composer on PHP) is checked with
    /// the runtime App Center installed, which lives outside its own tree.
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn checks_find_the_runtimes_app_center_installed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("packages"));
        layout.ensure().unwrap();
        // An installed interpreter, linked into the shared bin directory. The
        // name exists nowhere on the system PATH.
        let interpreter = layout.version_dir("interp", "1.0.0").join("bin");
        fs::create_dir_all(&interpreter).unwrap();
        let program = interpreter.join("rast-test-interp");
        fs::write(&program, "#!/bin/sh\necho \"demo 1.0.0\"\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(layout.bin_dir()).unwrap();
        std::os::unix::fs::symlink(&program, layout.bin_dir().join("rast-test-interp")).unwrap();
        // The tool: a wrapper that runs on that interpreter.
        let root = dir.path().join("tool");
        fs::create_dir_all(root.join("bin")).unwrap();
        let wrapper = root.join("bin/demo");
        fs::write(&wrapper, "#!/bin/sh\nexec rast-test-interp \"$@\"\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let manifest = manifest_for("1.0.0", "demo 1.0.0");

        let ok = run_checks(&manifest, &root, &home, false, Some(&layout)).unwrap();
        assert_eq!(ok[0].output, "demo 1.0.0");
        // Without the installed runtimes the interpreter is not found, and an
        // imported (untrusted) package never gets them.
        assert!(run_checks(&manifest, &root, &home, false, None).is_err());
        if sandbox::landlock_abi().is_some() {
            assert!(run_checks(&manifest, &root, &home, true, Some(&layout)).is_err());
        }
    }

    #[test]
    fn replaced_trees_survive_a_restart_until_settled() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("packages"));
        layout.ensure().unwrap();
        // The recorded tree was set aside, the new one is in place.
        let target = fake_version(&layout, "1.0.0", "new");
        let old = layout.trash().join("old");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join(INSTALL_MARKER),
            serde_json::json!({"id": "demo", "version": "1.0.0", "sha256": "old"}).to_string(),
        )
        .unwrap();
        let aside = layout
            .trash()
            .join(format!("{DISPLACED_PREFIX}{}", uuid::Uuid::new_v4()));
        fs::rename(&old, &aside).unwrap();
        // A marker that tries to point elsewhere is discarded.
        let evil = layout.trash().join(format!("{DISPLACED_PREFIX}evil"));
        fs::create_dir_all(&evil).unwrap();
        fs::write(
            evil.join(INSTALL_MARKER),
            r#"{"id": "..", "version": "../../x", "sha256": "z"}"#,
        )
        .unwrap();
        fs::create_dir_all(layout.staging().join("job")).unwrap();

        layout.clean_temporary();
        assert!(!layout.staging().join("job").exists());
        let found = layout.displaced();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].marker.sha256, "old");
        assert!(!evil.exists());

        // Not recorded: the old tree goes back.
        settle_replacement(&target, &found[0].path, false).unwrap();
        assert_eq!(read_marker(&target).unwrap().sha256, "old");
        assert!(layout.displaced().is_empty());
        assert_eq!(fs::read_dir(layout.trash()).unwrap().count(), 0);

        // Recorded: the old tree is deleted.
        let again = layout.trash().join(format!("{DISPLACED_PREFIX}again"));
        fs::create_dir_all(&again).unwrap();
        settle_replacement(&target, &again, true).unwrap();
        assert!(!again.exists());
        assert_eq!(read_marker(&target).unwrap().sha256, "old");
    }

    fn file_bytes(path: &Path) -> (String, u64) {
        use sha2::{Digest, Sha256};
        let bytes = fs::read(path).unwrap();
        (hex::encode(Sha256::digest(&bytes)), bytes.len() as u64)
    }

    /// A zip artifact like Bun/Deno: bin/demo inside `demo/`, strip 1.
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn zip_artifacts_install_like_tar_gz() {
        use crate::packages::archive::tests::{build_zip, ZipItem};
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("demo.zip");
        build_zip(
            &archive,
            &[
                ZipItem::Dir("demo/"),
                ZipItem::Dir("demo/bin/"),
                ZipItem::File("demo/bin/demo", &script("1.0.0"), 0o755),
            ],
        );
        let (sha256, size) = file_bytes(&archive);
        let mut artifact = crate::packages::manifest::tests::sample_artifact();
        artifact.url = format!("file://{}", archive.display());
        artifact.sha256 = sha256;
        artifact.size = size;
        artifact.format = "zip".into();
        artifact.strip_components = 1;
        let layout = Layout::new(dir.path().join("packages"));
        let policy = OriginPolicy {
            hosts: vec![],
            allow_file: true,
            ..Default::default()
        };
        let mut quiet = Quiet(Default::default());
        let m = manifest_for("1.0.0", "demo 1.0.0");
        let installed =
            install_version(&layout, &m, &artifact, &policy, "zip", false, &mut quiet).unwrap();
        assert_eq!(installed.checks[0].output, "demo 1.0.0");
        assert!(installed.path.join("bin/demo").is_file());
    }

    /// Rust: several archives merge into one prefix before the executables are
    /// checked (the primary archive alone would not contain `lib/`).
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn multi_archive_install_merges_components_into_one_prefix() {
        use crate::packages::manifest::ArchivePart;
        let dir = tempfile::tempdir().unwrap();
        let primary_path = dir.path().join("rustc.tar.gz");
        build(
            &primary_path,
            &[
                Item::Dir("rustc-1.100.0/"),
                Item::Dir("rustc-1.100.0/rustc/"),
                Item::Dir("rustc-1.100.0/rustc/bin/"),
                Item::File("rustc-1.100.0/rustc/bin/demo", &script("1.0.0"), 0o755),
            ],
        );
        let secondary_path = dir.path().join("std.tar.gz");
        build(
            &secondary_path,
            &[
                Item::Dir("rust-std-1.100.0/"),
                Item::Dir("rust-std-1.100.0/rust-std-x86_64-unknown-linux-gnu/"),
                Item::Dir("rust-std-1.100.0/rust-std-x86_64-unknown-linux-gnu/lib/"),
                Item::File(
                    "rust-std-1.100.0/rust-std-x86_64-unknown-linux-gnu/lib/std.rlib",
                    b"rlib",
                    0o644,
                ),
            ],
        );
        let (primary_sha, primary_size) = file_bytes(&primary_path);
        let (secondary_sha, secondary_size) = file_bytes(&secondary_path);
        let mut artifact = crate::packages::manifest::tests::sample_artifact();
        artifact.url = format!("file://{}", primary_path.display());
        artifact.sha256 = primary_sha;
        artifact.size = primary_size;
        artifact.strip_components = 2;
        artifact.archives = vec![ArchivePart {
            name: Some("rust-std".into()),
            url: format!("file://{}", secondary_path.display()),
            sha256: secondary_sha,
            size: secondary_size,
            format: "tar.gz".into(),
            strip_components: 2,
        }];
        assert_eq!(artifact.archives().len(), 2);
        let layout = Layout::new(dir.path().join("packages"));
        let policy = OriginPolicy {
            hosts: vec![],
            allow_file: true,
            ..Default::default()
        };
        let mut quiet = Quiet(Default::default());
        let m = manifest_for("1.0.0", "demo 1.0.0");
        let installed =
            install_version(&layout, &m, &artifact, &policy, "multi", false, &mut quiet).unwrap();
        assert!(installed.path.join("bin/demo").is_file());
        assert!(installed.path.join("lib/std.rlib").is_file());
        assert_eq!(installed.checks[0].output, "demo 1.0.0");
        let marker = read_marker(&installed.path).unwrap();
        assert_eq!(marker.sha256, artifact.sha256);
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(installed.path.join(INSTALL_MARKER)).unwrap())
                .unwrap();
        assert_eq!(raw["archives"].as_array().unwrap().len(), 2);
        assert_eq!(raw["archives"][1]["name"], "rust-std");

        // A component digest mismatch fails before anything is installed.
        let mut tampered = artifact.clone();
        tampered.archives[0].sha256 = "0".repeat(64);
        let err =
            install_version(&layout, &m, &tampered, &policy, "bad", false, &mut quiet).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A phar manifest (Composer shape); tests override the artifact URL.
    fn phar_manifest(sha256: &str, size: u64) -> Manifest {
        let mut v = crate::packages::manifest::tests::sample();
        v["id"] = "composer".into();
        v["version"] = "2.11.2".into();
        v["runtime"] = serde_json::json!({
            "executables": [{"name": "composer", "path": "bin/composer"}],
            "checks": [{
                "executable": "composer",
                "args": ["--version"],
                "expect": "Composer version 2.11.2"
            }]
        });
        v["artifacts"][0]["format"] = "phar".into();
        v["artifacts"][0]["stripComponents"] = 0.into();
        v["artifacts"][0]["url"] =
            "https://github.com/composer/composer/releases/download/2.11.2/composer.phar".into();
        v["artifacts"][0]["sha256"] = sha256.into();
        v["artifacts"][0]["size"] = size.into();
        v["artifacts"][0]["phar"] = serde_json::json!({"path": "composer.phar"});
        crate::packages::manifest::parse(&serde_json::to_vec(&v).unwrap()).unwrap()
    }

    /// The phar itself is written read-only (never executable) and the
    /// generated wrapper runs `php "<prefix>/composer.phar" "$@"` — proven
    /// with a fake `php` on PATH, so the test needs no PHP.
    #[test]
    fn phar_files_are_read_only_and_wrappers_exec_the_php_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let download = dir.path().join("composer.phar");
        fs::write(&download, b"<?php echo 'hi';\n").unwrap();
        install_phar(&root, &download, "composer.phar").unwrap();
        let phar = root.join("composer.phar");
        assert!(
            !download.exists(),
            "the verified download is moved into place"
        );
        assert_eq!(mode(&phar), 0o644, "the phar must never be executable");

        let exe = manifest::Executable {
            name: "composer".into(),
            path: "bin/composer".into(),
        };
        write_phar_wrappers(&root, std::slice::from_ref(&exe), "composer.phar").unwrap();
        let wrapper = root.join("bin/composer");
        assert_eq!(mode(&wrapper), 0o755);
        let text = fs::read_to_string(&wrapper).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"), "{text}");
        assert!(
            text.contains("exec php \"$root/composer.phar\" \"$@\""),
            "{text}"
        );

        let fake = dir.path().join("fakebin");
        fs::create_dir(&fake).unwrap();
        let php = fake.join("php");
        fs::write(&php, "#!/bin/sh\nprintf 'fake-php %s\\n' \"$*\"\n").unwrap();
        fs::set_permissions(&php, fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("/bin/sh")
            .arg(&wrapper)
            .arg("--version")
            .env("PATH", format!("{}:/usr/bin:/bin", fake.display()))
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("fake-php"), "{stdout}");
        assert!(
            stdout.contains(&format!("{}/composer.phar --version", root.display())),
            "{stdout}"
        );

        // Unsafe destinations and wrappers that could leave the prefix are
        // refused by the installer even if a manifest skipped validation.
        for bad in [
            "../composer.phar",
            "/etc/composer.phar",
            "composer$(id).phar",
            "composer`id`.phar",
            "a\\b.phar",
        ] {
            let copy = dir.path().join("copy.phar");
            fs::write(&copy, b"x").unwrap();
            assert!(install_phar(&root, &copy, bad).is_err(), "{bad} accepted");
        }
        for bad in ["composer", "bin/x/y", "bin/composer.phar/.."] {
            let bad_exe = manifest::Executable {
                name: "composer".into(),
                path: bad.into(),
            };
            assert!(
                write_phar_wrappers(&root, std::slice::from_ref(&bad_exe), "composer.phar")
                    .is_err(),
                "{bad} accepted"
            );
        }
    }

    /// End to end: a verified phar is installed and its generated wrapper is
    /// run by the version check with the PHP found on PATH.
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "package checks run only on Linux")]
    fn phar_artifacts_install_and_their_check_runs() {
        if Command::new("php").arg("--version").output().is_err() {
            eprintln!("skipping: no PHP on PATH");
            return;
        }
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let body = b"<?php echo \"Composer version 2.11.2\\n\";\n";
        let source = dir.path().join("composer.phar");
        fs::write(&source, body).unwrap();
        let sha256 = hex::encode(Sha256::digest(body));
        let manifest = phar_manifest(&sha256, body.len() as u64);
        let mut artifact = manifest.artifacts[0].clone();
        artifact.url = format!("file://{}", source.display());
        let layout = Layout::new(dir.path().join("packages"));
        let policy = OriginPolicy {
            hosts: vec![],
            allow_file: true,
            ..Default::default()
        };
        let mut quiet = Quiet(Default::default());
        let installed = install_version(
            &layout, &manifest, &artifact, &policy, "phar", false, &mut quiet,
        )
        .unwrap();
        assert_eq!(installed.checks[0].output, "Composer version 2.11.2");
        assert_eq!(mode(&installed.path.join("composer.phar")), 0o644);
        let wrapper = installed.path.join("bin/composer");
        assert_eq!(mode(&wrapper), 0o755);
        assert!(fs::read_to_string(&wrapper)
            .unwrap()
            .contains("exec php \"$root/composer.phar\" \"$@\""));
    }
}
