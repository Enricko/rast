//! Resolve the RAST executable used for self-exec subcommands safely across
//! upgrades.
//!
//! The privileged helper starts short-lived helper processes (`terminal-agent`,
//! `rootfs-agent`) using its **own** executable. `std::env::current_exe()` reads
//! `/proc/self/exe`, so after the on-disk binary has been replaced the kernel
//! reports the old path with a `" (deleted)"` suffix. Passing that to
//! `systemd-run` fails with `Failed to find executable /usr/local/bin/rast
//! (deleted)` — the terminal regression fixed here.
//!
//! Two defences, in order:
//! 1. the helper captures its resolved path **once at start-up** (before any
//!    upgrade can replace the file); production installs versioned binaries
//!    behind a stable symlink, so the captured file stays on disk;
//! 2. the child receives the helper's build id and refuses to run when they do
//!    not match, instead of silently running an incompatible agent.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

/// Suffix the kernel appends to `/proc/<pid>/exe` for an unlinked executable.
pub const DELETED_SUFFIX: &str = " (deleted)";

/// Build id embedded at compile time (the git revision the binary reports).
pub fn build_id() -> &'static str {
    env!("RAST_GIT_SHA")
}

/// Drop the kernel's `" (deleted)"` marker from a `/proc/self/exe` path.
pub fn strip_deleted(raw: &Path) -> PathBuf {
    let text = raw.to_string_lossy();
    match text.strip_suffix(DELETED_SUFFIX) {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => raw.to_path_buf(),
    }
}

/// Validate a candidate executable: absolute, regular file, executable bit.
pub fn validate(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    let meta = std::fs::metadata(path)
        .map_err(|error| format!("{} cannot be read ({error})", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

/// Resolve a `/proc/self/exe` path to the current executable, tolerating the
/// deleted-inode marker, and return the canonical (symlink-resolved) path.
pub fn resolve_self(raw: &Path) -> Result<PathBuf, String> {
    let candidate = strip_deleted(raw);
    validate(&candidate)?;
    candidate
        .canonicalize()
        .map_err(|error| format!("cannot resolve {} ({error})", candidate.display()))
}

/// Capture the running executable. Call once, as early as possible at process
/// start (before an upgrade can replace the on-disk file).
pub fn capture_self_exe() -> Result<PathBuf, String> {
    let raw = std::env::current_exe()
        .map_err(|error| format!("cannot locate the running RAST executable ({error})"))?;
    resolve_self(&raw)
}

/// The path captured at start-up, shared by all self-exec subcommands.
pub fn capture() {
    let _ = self_exe();
}

/// The path captured at start-up (or captured on first use for processes that
/// did not call [`capture`] explicitly, such as unit tests).
pub fn self_exe() -> Result<PathBuf, String> {
    static CAPTURED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    CAPTURED.get_or_init(capture_self_exe).clone()
}

/// Run `<path> version` and extract the build id from `RAST 0.18.3 (<sha>)`.
/// Used only when the captured path is gone (rare upgrade window) or for the
/// installed fallback, so the extra process spawn is acceptable.
pub fn probe_build_id(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or_default();
    let start = first.find('(')? + 1;
    let rest = &first[start..];
    let end = rest.find(')')?;
    let id = rest[..end].trim();
    let looks_like_build = (7..=40).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && id.bytes().any(|byte| byte.is_ascii_digit());
    looks_like_build.then(|| id.to_string())
}

/// Resolve the executable to spawn for a self-exec subcommand.
///
/// `captured` is the start-up path; `install_path` is used only when the
/// captured file is gone. A candidate is accepted only when it reports the
/// same build id as this process, so an upgraded helper can never start an
/// agent from a different revision by accident. The error message tells the
/// operator exactly what to do.
pub fn resolve_agent_executable(captured: &Path, install_path: &Path) -> Result<PathBuf, String> {
    let mut problems = Vec::new();
    for candidate in [captured, install_path] {
        match validate(candidate) {
            Ok(()) => match probe_build_id(candidate) {
                Some(id) if id == build_id() => {
                    return match candidate.canonicalize() {
                        Ok(path) => Ok(path),
                        Err(error) => {
                            Err(format!("cannot resolve {} ({error})", candidate.display()))
                        }
                    }
                }
                Some(id) => problems.push(format!("{} is build {id}", candidate.display())),
                None => problems.push(format!(
                    "{} does not report a RAST build id",
                    candidate.display()
                )),
            },
            Err(error) => problems.push(error),
        }
    }
    Err(format!(
        "the RAST helper cannot find a compatible executable for its helper agents ({}). \
         Restart rast-native.service so it captures the installed binary again.",
        problems.join("; ")
    ))
}

/// Convenience for the privileged helper: the captured executable, with the
/// binary's install location (the current `/proc/self/exe` path without the
/// deleted marker) as the fallback for the upgrade window.
pub fn agent_executable() -> Result<PathBuf, String> {
    let captured = self_exe()?;
    let install_raw = std::env::current_exe()
        .map_err(|error| format!("cannot locate the installed RAST binary ({error})"))?;
    let install = strip_deleted(&install_raw);
    resolve_agent_executable(&captured, &install)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, build: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho 'RAST 0.18.3 ({build})'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn deleted_marker_is_stripped_only_as_suffix() {
        assert_eq!(
            strip_deleted(Path::new("/usr/local/bin/rast (deleted)")),
            PathBuf::from("/usr/local/bin/rast")
        );
        // A real path that merely contains the word is left untouched.
        assert_eq!(
            strip_deleted(Path::new("/opt/rast (deleted)/bin/rast")),
            PathBuf::from("/opt/rast (deleted)/bin/rast")
        );
    }

    #[test]
    fn validate_rejects_missing_relative_and_non_executable_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate(Path::new("relative/rast")).is_err());
        assert!(validate(&dir.path().join("missing")).is_err());
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "not executable").unwrap();
        assert!(validate(&plain).is_err());
        let ok = script(dir.path(), "rast", build_id());
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn build_id_is_parsed_from_the_version_line() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "rast", "abcdef123456");
        assert_eq!(probe_build_id(&exe).as_deref(), Some("abcdef123456"));
        let broken = dir.path().join("broken");
        std::fs::write(&broken, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(probe_build_id(&broken), None);
    }

    #[test]
    fn a_captured_binary_from_another_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let captured = script(dir.path(), "captured", "deadbeef00");
        let install = script(dir.path(), "install", "deadbeef00");
        let error = resolve_agent_executable(&captured, &install).unwrap_err();
        assert!(
            error.contains("cannot find a compatible executable"),
            "{error}"
        );
        assert!(
            error.to_lowercase().contains("restart rast-native.service"),
            "{error}"
        );
    }

    #[test]
    fn the_matching_installed_binary_is_used_when_the_captured_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let install = script(dir.path(), "install", build_id());
        let captured = dir.path().join("gone");
        let resolved = resolve_agent_executable(&captured, &install).unwrap();
        assert_eq!(resolved, install.canonicalize().unwrap());
    }

    #[test]
    fn a_matching_captured_binary_wins() {
        let dir = tempfile::tempdir().unwrap();
        let captured = script(dir.path(), "captured", build_id());
        let install = script(dir.path(), "install", "deadbeef00");
        let resolved = resolve_agent_executable(&captured, &install).unwrap();
        assert_eq!(resolved, captured.canonicalize().unwrap());
    }
}
