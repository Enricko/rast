//! Package-authoring commands used by the `rast-package-template` and
//! `rast-packages` repositories.
//!
//! These replace the Python helpers (`scripts/package.py`, the local copy of
//! `rastpkg.py`) with the exact same validation code RAST itself runs, so a
//! template release cannot drift from what the Package Center accepts.
//!
//! * `manifest` — write `rast-package.json` for a release tag from built
//!   archives in `dist/`.
//! * `validate` — parse and validate a manifest (optionally against local
//!   archives).
//! * `smoke` — extract an archive the way RAST does and run the manifest's
//!   runtime checks.
//! * `check-release` — cross-check a published GitHub release exactly like
//!   Package Center → Install from GitHub.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    archive, github,
    manifest::{self, Artifact, Manifest, Provenance},
};

/// Environment/behaviour limit for one smoke check.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Archives for authoring are small; the installer's own limits apply on top.
const SMOKE_ROOM: u64 = 16 * 1024 * 1024 * 1024;

/// The repository's `package.json` (template format, not a manifest).
#[derive(Debug, Clone, Deserialize)]
pub struct TemplatePackage {
    /// Everything except `version`, `revision` and `artifacts`. The template
    /// may contain the literal `{version}` in strings (for example in a check),
    /// replaced before validation.
    pub manifest: Value,
    pub source: TemplateSource,
    /// Host targets: a list (`["linux-x86_64"]`) or a mapping
    /// (`{"linux-x86_64": "ubuntu-24.04"}`), as the template uses.
    pub targets: Value,
}

impl TemplatePackage {
    pub fn target_names(&self) -> Result<Vec<String>, String> {
        let mut names = match &self.targets {
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "targets entries must be strings".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?,
            Value::Object(map) => map.keys().cloned().collect(),
            _ => return Err("targets must be an array or an object".into()),
        };
        names.sort();
        if names.is_empty() {
            return Err("package.json declares no targets".into());
        }
        Ok(names)
    }
}

/// Read the repository's `package.json`.
pub fn load_template(path: &Path) -> Result<TemplatePackage, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} is not a valid package.json: {e}", path.display()))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateSource {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// `v<version>-<revision>`, e.g. `v5.4.9-1`.
pub fn parse_tag(tag: &str) -> Result<(String, u32), String> {
    let rest = tag
        .strip_prefix('v')
        .ok_or_else(|| format!("release tag {tag:?} must look like v<version>-<revision>, e.g. v5.4.9-1"))?;
    let (version, revision) = rest.rsplit_once('-').ok_or_else(|| {
        format!("release tag {tag:?} must look like v<version>-<revision>, e.g. v5.4.9-1")
    })?;
    let valid_version = !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    if !valid_version {
        return Err(format!("release tag {tag:?} has an invalid version part"));
    }
    let revision: u32 = revision
        .parse()
        .map_err(|_| format!("release tag {tag:?} has an invalid revision (expected a positive number)"))?;
    if revision == 0 {
        return Err(format!("release tag {tag:?} revision must be at least 1"));
    }
    Ok((version.to_string(), revision))
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Replace every `{version}` token in the template manifest strings.
fn render_placeholders(value: &mut Value, version: &str) {
    match value {
        Value::String(text) => *text = text.replace("{version}", version),
        Value::Array(items) => {
            for item in items {
                render_placeholders(item, version);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                render_placeholders(item, version);
            }
        }
        _ => {}
    }
}

/// Build the manifest for one release from built archives.
#[allow(clippy::too_many_arguments)]
pub fn build_manifest(
    package: &TemplatePackage,
    tag: &str,
    repo: &str,
    dist: &Path,
    attestations: &BTreeMap<String, String>,
    allow_missing: bool,
    run_url: Option<&str>,
) -> Result<Manifest, String> {
    let (version, revision) = parse_tag(tag)?;
    if version != package.source.version {
        return Err(format!(
            "tag version {version} does not match package.json source.version {}",
            package.source.version
        ));
    }
    let mut body = package.manifest.clone();
    render_placeholders(&mut body, &version);
    let object = body
        .as_object_mut()
        .ok_or("package.json manifest must be a JSON object")?;
    object.insert("version".into(), Value::String(version.clone()));
    object.insert("revision".into(), Value::from(revision));

    let mut artifacts = Vec::new();
    for target in package.target_names()? {
        let name = format!(
            "{}-{version}-{target}.tar.gz",
            object
                .get("id")
                .and_then(Value::as_str)
                .ok_or("package.json manifest.id is required")?
        );
        let path = dist.join(&name);
        if !path.is_file() {
            if allow_missing {
                continue;
            }
            return Err(format!("missing archive {}", path.display()));
        }
        let provenance = Provenance {
            kind: "built".into(),
            source: package.source.url.clone(),
            source_sha256: Some(package.source.sha256.clone()),
            verification: Some(
                "Source SHA-256 checked before building; binaries built and executed on a native runner in GitHub Actions".into(),
            ),
            build_url: run_url.map(str::to_string),
            attestation_url: attestations.get(&target).cloned(),
        };
        artifacts.push(Artifact {
            target: target.clone(),
            url: format!("https://github.com/{repo}/releases/download/{tag}/{name}"),
            sha256: sha256_file(&path)?,
            size: fs::metadata(&path)
                .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
                .len(),
            format: "tar.gz".into(),
            strip_components: 1,
            requires: Default::default(),
            provenance,
        });
    }
    object.insert(
        "artifacts".into(),
        serde_json::to_value(artifacts).map_err(|e| e.to_string())?,
    );
    let manifest: Manifest = serde_json::from_value(body)
        .map_err(|e| format!("generated manifest does not match the schema: {e}"))?;
    manifest::validate(&manifest)?;
    Ok(manifest)
}

/// Load and validate a manifest file, optionally verifying local archives.
pub fn validate_file(manifest_path: &Path, dist: Option<&Path>) -> Result<Manifest, String> {
    let bytes = fs::read(manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest = manifest::parse(&bytes)?;
    if let Some(dist) = dist {
        for artifact in &manifest.artifacts {
            let name = artifact
                .url
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("artifact {} has no file name", artifact.target))?;
            let path = dist.join(name);
            let size = fs::metadata(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?
                .len();
            if size != artifact.size {
                return Err(format!(
                    "{} is {size} bytes but the manifest declares {}",
                    path.display(),
                    artifact.size
                ));
            }
            let sha = sha256_file(&path)?;
            if sha != artifact.sha256 {
                return Err(format!(
                    "{} does not match the manifest SHA-256",
                    path.display()
                ));
            }
        }
    }
    Ok(manifest)
}

/// Extract an archive like the installer does and run every runtime check.
pub fn smoke(archive_path: &Path, package: &TemplatePackage) -> Result<Vec<String>, String> {
    let version = package.source.version.clone();
    let mut body = package.manifest.clone();
    render_placeholders(&mut body, &version);
    // `smoke` runs before a completed manifest exists, so only the runtime
    // section is needed here (it is validated when the manifest is written).
    #[derive(Deserialize)]
    struct TemplateRuntime {
        runtime: manifest::RuntimeSpec,
    }
    let template: TemplateRuntime = serde_json::from_value(body)
        .map_err(|e| format!("package.json runtime section is invalid: {e}"))?;
    let runtime = template.runtime;

    let work = tempdir()?;
    let root = work.path().join("root");
    let home = work.path().join("home");
    fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    let mut progress = |_stats: &archive::ExtractStats| {};
    archive::extract_tar_gz(
        archive_path,
        &root,
        1,
        SMOKE_ROOM,
        &mut progress,
        &|| false,
    )?;

    let mut executables = BTreeMap::new();
    for executable in &runtime.executables {
        let path = root.join(&executable.path);
        let metadata = fs::metadata(&path)
            .map_err(|_| format!("executable {} ({}) is missing", executable.name, executable.path))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(format!(
                    "executable {} ({}) is not executable",
                    executable.name, executable.path
                ));
            }
        }
        executables.insert(executable.name.clone(), path);
    }
    let mut lines = Vec::new();
    for check in &runtime.checks {
        let path = executables
            .get(&check.executable)
            .ok_or_else(|| format!("check names unknown executable {:?}", check.executable))?;
        let output = run_with_timeout(
            path,
            &check.args,
            &home,
            &format!(
                "{}:{}:/usr/bin:/bin",
                root.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )?;
        let text = format!("{}{}", output.0, output.1);
        let text = text.trim();
        if output.2 {
            return Err(format!(
                "check {} timed out after {}s",
                check.executable,
                CHECK_TIMEOUT.as_secs()
            ));
        }
        if output.3 != 0 || !text.contains(&check.expect) {
            return Err(format!(
                "check {} failed: exit {}, output {:?}",
                check.executable,
                output.3,
                text.chars().take(200).collect::<String>()
            ));
        }
        lines.push(format!(
            "ok: {} {} -> {}",
            check.executable,
            check.args.join(" "),
            text.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(lines)
}

/// `Ok((stdout, stderr, timed_out, exit_code))`.
fn run_with_timeout(
    program: &Path,
    args: &[String],
    cwd: &Path,
    path_env: &str,
) -> Result<(String, String, bool, i32), String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", path_env)
        .env("HOME", cwd)
        .env("TMPDIR", cwd)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let mut stdout = child.stdout.take().ok_or("cannot capture stdout")?;
    let mut stderr = child.stderr.take().ok_or("cannot capture stderr")?;
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = out_tx.send(buffer);
    });
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        let _ = err_tx.send(buffer);
    });
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if started.elapsed() >= CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let status = child.wait().map_err(|e| e.to_string())?.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out_rx.recv().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_rx.recv().unwrap_or_default()).into_owned();
    Ok((stdout, stderr, timed_out, status))
}

/// Cross-check a published GitHub release exactly like Package Center does.
pub fn check_release(repo: &str, tag: &str, token: Option<&str>) -> Result<Vec<String>, String> {
    let parsed = github::parse_link(&format!("{repo}/releases/tag/{tag}"))
        .map_err(|e| format!("invalid repository {repo:?}: {e}"))?;
    if parsed.tag.as_deref() != Some(tag) {
        return Err("the repository and tag could not be combined into a release link".into());
    }
    let inspected = github::inspect(&parsed, token)?;
    let mut lines = Vec::new();
    for check in &inspected.checks {
        lines.push(format!("{}: {}", check.label, check.detail));
        if check.status == "failed" {
            return Err(format!("{}: {}", check.label, check.detail));
        }
    }
    lines.push(format!(
        "{}@{}: {} {} is importable",
        inspected.repo, inspected.release.tag, inspected.manifest.id, inspected.manifest.version
    ));
    Ok(lines)
}

/// Minimal private temporary directory (removed on drop).
struct TempDir(PathBuf);

impl TempDir {
    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> Result<TempDir, String> {
    let base = std::env::temp_dir();
    let name = format!(
        "rast-package-smoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = base.join(name);
    fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    Ok(TempDir(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(manifest: Value, version: &str) -> TemplatePackage {
        TemplatePackage {
            manifest,
            source: TemplateSource {
                version: version.into(),
                url: "https://example.com/source.tar.gz".into(),
                sha256: "a".repeat(64),
            },
            targets: serde_json::json!({"linux-x86_64": "ubuntu-24.04", "linux-aarch64": "ubuntu-24.04-arm"}),
        }
    }

    fn sample_manifest() -> Value {
        serde_json::json!({
            "schemaVersion": 1,
            "kind": "rast-package",
            "id": "demo",
            "type": "runtime",
            "name": "Demo {version}",
            "description": "Demo runtime",
            "publisher": {"id": "demo-publisher", "name": "Demo", "url": "https://example.com"},
            "minRastVersion": "0.11.0",
            "runtime": {
                "executables": [{"name": "demo", "path": "bin/demo"}],
                "checks": [{"executable": "demo", "args": ["--version"], "expect": "{version}"}]
            },
            "artifacts": []
        })
    }

    #[test]
    fn release_tags_are_parsed_strictly() {
        assert_eq!(parse_tag("v5.4.9-1").unwrap(), ("5.4.9".into(), 1));
        assert_eq!(parse_tag("v1.0-rc1-2").unwrap(), ("1.0-rc1".into(), 2));
        for bad in ["5.4.9-1", "v5.4.9", "v5.4.9-0", "v-1", "v5.4.9-x"] {
            assert!(parse_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn manifest_is_built_from_archives_and_placeholders() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("demo-5.4.9-linux-x86_64.tar.gz"), b"x").unwrap();
        let manifest = build_manifest(
            &package(sample_manifest(), "5.4.9"),
            "v5.4.9-1",
            "owner/repo",
            &dir.path(),
            &BTreeMap::new(),
            true,
            Some("https://github.com/owner/repo/actions/runs/1"),
        )
        .unwrap();
        assert_eq!(manifest.name, "Demo 5.4.9");
        assert_eq!(manifest.revision, 1);
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].target, "linux-x86_64");
        assert_eq!(
            manifest.runtime.checks[0].expect, "5.4.9",
            "placeholders must be replaced in checks"
        );
        assert_eq!(manifest.artifacts[0].size, 1);
        assert_eq!(
            manifest.artifacts[0].url,
            "https://github.com/owner/repo/releases/download/v5.4.9-1/demo-5.4.9-linux-x86_64.tar.gz"
        );
        // The rendered manifest must survive RAST's own parser.
        let bytes = serde_json::to_vec(&manifest).unwrap();
        manifest::parse(&bytes).unwrap();
    }

    #[test]
    fn a_tag_must_match_the_template_source_version() {
        let dir = tempdir().unwrap();
        let error = build_manifest(
            &package(sample_manifest(), "5.4.9"),
            "v5.4.10-1",
            "owner/repo",
            &dir.path(),
            &BTreeMap::new(),
            true,
            None,
        )
        .unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn missing_archives_are_an_error_unless_allowed() {
        let dir = tempdir().unwrap();
        let error = build_manifest(
            &package(sample_manifest(), "5.4.9"),
            "v5.4.9-1",
            "owner/repo",
            &dir.path(),
            &BTreeMap::new(),
            false,
            None,
        )
        .unwrap_err();
        assert!(error.contains("missing archive"), "{error}");
    }

    #[test]
    fn validation_checks_local_archives() {
        let dir = tempdir().unwrap();
        let archive = dir.path().join("demo-5.4.9-linux-x86_64.tar.gz");
        fs::write(&archive, b"payload").unwrap();
        let manifest = build_manifest(
            &package(sample_manifest(), "5.4.9"),
            "v5.4.9-1",
            "owner/repo",
            &dir.path(),
            &BTreeMap::new(),
            true,
            None,
        )
        .unwrap();
        let manifest_path = dir.path().join("rast-package.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        validate_file(&manifest_path, Some(&dir.path())).unwrap();
        fs::write(&archive, b"tampered").unwrap();
        assert!(validate_file(&manifest_path, Some(&dir.path())).is_err());
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("one");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
