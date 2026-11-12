//! Runtimes the operating system already has: apt packages (`php8.5-cli`),
//! manual installs (`/usr/local/go`, a Node.js tarball linked into
//! `/usr/local/bin`) and snaps. Package Center lists them as *System*
//! installs so a newer verified version can be offered as an update.
//!
//! The version is read by running the command, confined like a package check
//! (see [`super::sandbox`]); the owner comes from dpkg's file lists. RAST never
//! changes these files: an update installs the new version next to them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use super::{sandbox, version};

/// Where a login shell finds commands, in PATH order (Package Center's own
/// `bin` directory is not part of it).
pub const SYSTEM_PATH: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
    "/snap/bin",
];

const DPKG_INFO: &str = "/var/lib/dpkg/info";
const RUN_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_LIMIT: usize = 16 * 1024;

/// How to find and identify one official runtime on the system.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    pub id: &'static str,
    pub command: &'static str,
    args: &'static [&'static str],
    /// Well-known install locations that are not on the system PATH.
    extra: &'static [&'static str],
}

pub const PROBES: &[Probe] = &[
    Probe {
        id: "go",
        command: "go",
        args: &["version"],
        extra: &["/usr/local/go/bin"],
    },
    Probe {
        id: "nodejs",
        command: "node",
        args: &["--version"],
        extra: &[],
    },
    Probe {
        id: "python",
        command: "python3",
        args: &["--version"],
        extra: &[],
    },
    Probe {
        id: "php",
        command: "php",
        // Without php.ini: the version line does not depend on extensions.
        args: &["-n", "--version"],
        extra: &[],
    },
];

/// The command a terminal uses for this package (`python3` for Python).
pub fn primary_command(id: &str) -> Option<&'static str> {
    PROBES.iter().find(|p| p.id == id).map(|p| p.command)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemInstall {
    pub command: String,
    /// Where it was found, e.g. `/usr/bin/php`.
    pub path: String,
    /// The file that path resolves to, e.g. `/usr/bin/php8.5`.
    pub real_path: String,
    /// Read by running `version_command`; `None` when that failed.
    pub version: Option<String>,
    pub version_command: String,
    /// First line of the output, or why the version could not be read.
    pub output: String,
    /// `apt`, `snap` or `manual`.
    pub source: String,
    /// The dpkg package that owns the file (`apt`).
    pub source_package: Option<String>,
    /// False for well-known locations a shell does not search, such as
    /// `/usr/local/go/bin` without a PATH entry.
    pub on_path: bool,
    /// Moved aside (`.rast-displaced.<command>`) while Package Center
    /// provides the command system-wide; it is put back by "Use the system
    /// version" or an uninstall.
    #[serde(default)]
    pub displaced: bool,
    pub detected_at: i64,
}

/// Where to look; the defaults describe the real system, tests point them at
/// a temporary tree.
#[derive(Debug, Clone)]
pub struct Search {
    pub path: Vec<PathBuf>,
    /// Prefix for the probes' extra locations (`/` on the real system).
    pub extra_root: PathBuf,
    pub dpkg_info: PathBuf,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            path: SYSTEM_PATH.iter().map(PathBuf::from).collect(),
            extra_root: PathBuf::from("/"),
            dpkg_info: PathBuf::from(DPKG_INFO),
        }
    }
}

/// Detect every official runtime the system provides. `own_root` is Package
/// Center's directory (its shims are skipped wherever they are linked from);
/// `scratch` is a private directory for the version commands.
pub fn detect_all(own_root: &Path, scratch: &Path) -> HashMap<String, SystemInstall> {
    detect_with(PROBES, &Search::default(), own_root, scratch)
}

pub fn detect_with(
    probes: &[Probe],
    search: &Search,
    own_root: &Path,
    scratch: &Path,
) -> HashMap<String, SystemInstall> {
    let own = fs::canonicalize(own_root).unwrap_or_else(|_| own_root.to_path_buf());
    let found: Vec<(Probe, Found)> = probes
        .iter()
        .filter_map(|p| find(p, search, &own).map(|f| (*p, f)))
        .collect();
    let wanted: Vec<String> = found
        .iter()
        .flat_map(|(_, f)| [f.real.display().to_string(), f.path.display().to_string()])
        .collect();
    let owners = dpkg_owners(&search.dpkg_info, &wanted);
    let now = crate::db::now_ms();
    let mut out = HashMap::new();
    for (probe, f) in found {
        let real = f.real.display().to_string();
        let path = f.path.display().to_string();
        // The owner of the real file names the actual runtime package
        // (python3.14-minimal rather than the python3 symlink's package).
        let source_package = owners.get(&real).or_else(|| owners.get(&path)).cloned();
        let source = if path.starts_with("/snap/") || real.starts_with("/snap/") {
            "snap"
        } else if source_package.is_some() {
            "apt"
        } else {
            "manual"
        };
        let version_command = std::iter::once(probe.command)
            .chain(probe.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        let (version, output) = match read_version(&probe, &f, scratch) {
            Ok((v, line)) => (Some(v), line),
            Err(e) => (None, e),
        };
        out.insert(
            probe.id.to_string(),
            SystemInstall {
                command: probe.command.to_string(),
                path,
                real_path: real,
                version,
                version_command,
                output,
                source: source.into(),
                source_package,
                on_path: f.on_path,
                displaced: f.displaced,
                detected_at: now,
            },
        );
    }
    out
}

struct Found {
    path: PathBuf,
    real: PathBuf,
    on_path: bool,
    displaced: bool,
}

fn find(probe: &Probe, search: &Search, own: &Path) -> Option<Found> {
    let extra = probe
        .extra
        .iter()
        .map(|d| search.extra_root.join(d.trim_start_matches('/')));
    let dirs = search
        .path
        .iter()
        .cloned()
        .map(|d| (d, true))
        .chain(extra.map(|d| (d, false)));
    for (dir, on_path) in dirs {
        // A command moved aside for Package Center's system-wide link
        // belongs where the link is now.
        let candidates = [
            (dir.join(probe.command), false),
            (
                dir.join(format!(
                    "{}{}",
                    super::links::DISPLACED_PREFIX,
                    probe.command
                )),
                true,
            ),
        ];
        for (path, displaced) in candidates {
            let Ok(real) = fs::canonicalize(&path) else {
                continue;
            };
            // Package Center's own shims, wherever they are linked from.
            if real.starts_with(own) {
                continue;
            }
            let executable = fs::metadata(&real)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if executable {
                return Some(Found {
                    path,
                    real,
                    on_path,
                    displaced,
                });
            }
        }
    }
    None
}

/// Run the version command confined: read/execute only in the install's own
/// tree and the system directories, no network, a private scratch home.
fn read_version(probe: &Probe, f: &Found, scratch: &Path) -> Result<(String, String), String> {
    let home = scratch.join(format!("detect-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&home).map_err(|e| format!("cannot create a scratch directory: {e}"))?;
    let result = (|| {
        let confinement = sandbox::Confinement::new(&install_root(&f.real), &home, false)?;
        let output = sandbox::run(
            Command::new(&f.path)
                .args(probe.args)
                .current_dir(&home)
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("HOME", &home)
                .env("TMPDIR", &home)
                .env("LANG", "C.UTF-8")
                // No JIT under the RAST service; no toolchain downloads.
                .env("NODE_OPTIONS", "--jitless")
                .env("GOTOOLCHAIN", "local")
                .env("GOENV", "off"),
            &confinement,
            &home,
            RUN_TIMEOUT,
            OUTPUT_LIMIT,
        )?;
        let line = output
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>();
        let version = parse_version(probe.id, &output)
            .ok_or_else(|| format!("no version in the output of {}: {line}", probe.command))?;
        Ok((version, line))
    })();
    let _ = fs::remove_dir_all(&home);
    result
}

/// The directory a runtime lives in: `/usr/local/go` for
/// `/usr/local/go/bin/go`, the binary's own directory otherwise.
fn install_root(real: &Path) -> PathBuf {
    let parent = real.parent().unwrap_or(Path::new("/"));
    let root = if parent.file_name().is_some_and(|n| n == "bin") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    };
    if root == Path::new("/") {
        parent.to_path_buf()
    } else {
        root.to_path_buf()
    }
}

/// The version in a runtime's `--version` output: `go1.27.4`, `v22.23.4`,
/// `Python 3.14.4`, `PHP 8.5.4 (cli)` or `PHP 8.3.6-1ubuntu1 (cli)`.
pub fn parse_version(id: &str, output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        let token = match id {
            "go" => match token.strip_prefix("go") {
                Some(rest) => rest,
                None => continue,
            },
            "nodejs" => token.strip_prefix('v').unwrap_or(token),
            _ => token,
        };
        let numeric: String = token
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let numeric = numeric.trim_end_matches('.');
        if numeric.contains('.') && version::Version::parse(numeric).is_some() {
            return Some(numeric.to_string());
        }
    }
    None
}

/// Which dpkg package lists each path (merged-/usr aware: `/bin/x` and
/// `/usr/bin/x` are the same file).
fn dpkg_owners(info: &Path, paths: &[String]) -> HashMap<String, String> {
    let mut aliases: HashMap<String, String> = HashMap::new();
    for p in paths {
        aliases.insert(p.clone(), p.clone());
        for (merged, legacy) in [
            ("/usr/bin/", "/bin/"),
            ("/usr/sbin/", "/sbin/"),
            ("/usr/lib/", "/lib/"),
        ] {
            if let Some(rest) = p.strip_prefix(merged) {
                aliases.insert(format!("{legacy}{rest}"), p.clone());
            } else if let Some(rest) = p.strip_prefix(legacy) {
                aliases.insert(format!("{merged}{rest}"), p.clone());
            }
        }
    }
    let mut owners = HashMap::new();
    let Ok(entries) = fs::read_dir(info) else {
        return owners;
    };
    let mut pending: HashSet<&String> = paths.iter().collect();
    for entry in entries.flatten() {
        if pending.is_empty() {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(package) = name.strip_suffix(".list") else {
            continue;
        };
        // `libc6:amd64.list` -> `libc6`.
        let package = package.split(':').next().unwrap_or(package);
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in content.lines() {
            if let Some(original) = aliases.get(line) {
                if pending.remove(original) {
                    owners.insert(original.clone(), package.to_string());
                }
            }
        }
    }
    owners
}

/// The release line of a system version in the catalog's channels: the
/// longest channel id it starts with (`3.14` for `3.14.4`, `22` for
/// `22.23.4`), else the default line of that version.
pub fn channel_of(version: &str, channels: &[String]) -> String {
    channels
        .iter()
        .filter(|c| version == c.as_str() || version.starts_with(&format!("{c}.")))
        .max_by_key(|c| c.len())
        .cloned()
        .unwrap_or_else(|| {
            version::Version::parse(version)
                .map(|v| v.default_channel())
                .unwrap_or_default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn versions_are_read_from_each_runtime_output() {
        let cases = [
            ("go", "go version go1.27.4 linux/amd64", Some("1.27.4")),
            ("nodejs", "v22.23.4\n", Some("22.23.4")),
            ("python", "Python 3.14.4\n", Some("3.14.4")),
            (
                "php",
                "PHP 8.5.4 (cli) (built: Oct 19 2026 14:35:18) (NTS)\nCopyright (c) The PHP Group",
                Some("8.5.4"),
            ),
            ("php", "PHP 8.3.6-1ubuntu1.2 (cli)", Some("8.3.6")),
            ("go", "go: unknown command", None),
            ("python", "", None),
        ];
        for (id, output, want) in cases {
            assert_eq!(
                parse_version(id, output).as_deref(),
                want,
                "{id}: {output:?}"
            );
        }
    }

    #[test]
    fn system_versions_map_to_catalog_release_lines() {
        let channels = vec!["3.15".to_string(), "3.14".to_string(), "22".to_string()];
        assert_eq!(channel_of("3.14.4", &channels), "3.14");
        assert_eq!(channel_of("22.23.4", &channels), "22");
        // Not tracked by the catalog: the version's own default line.
        assert_eq!(channel_of("3.12.3", &channels), "3.12");
    }

    #[test]
    fn detects_system_runtimes_with_owner_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let usr_bin = root.join("usr/bin");
        let local_bin = root.join("usr/local/bin");
        // apt-style: a symlink to the versioned binary, listed by dpkg.
        script(
            &usr_bin.join("php8.5"),
            "echo 'PHP 8.5.4 (cli) (built: Oct 19 2026)'",
        );
        symlink("php8.5", usr_bin.join("php")).unwrap();
        // Manual: a tarball under /usr/local/lib linked into /usr/local/bin.
        script(
            &root.join("usr/local/lib/nodejs/node-v22.23.4/bin/node"),
            "echo v22.23.4",
        );
        fs::create_dir_all(&local_bin).unwrap();
        symlink(
            root.join("usr/local/lib/nodejs/node-v22.23.4/bin/node"),
            local_bin.join("node"),
        )
        .unwrap();
        // Off PATH: /usr/local/go/bin.
        script(
            &root.join("usr/local/go/bin/go"),
            "echo 'go version go1.26.3 linux/amd64'",
        );
        // Package Center's own shim linked into /usr/local/bin is not "system".
        let own = root.join("packages");
        script(
            &own.join("runtimes/python/3.14.10/bin/python3"),
            "echo 'Python 3.14.10'",
        );
        symlink(
            own.join("runtimes/python/3.14.10/bin/python3"),
            local_bin.join("python3"),
        )
        .unwrap();
        // A python3 whose version cannot be read.
        script(&usr_bin.join("python3"), "echo 'no version here'");
        let info = root.join("dpkg/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(
            info.join("php8.5-cli:amd64.list"),
            format!("/.\n/usr\n{}\n", usr_bin.join("php8.5").display()),
        )
        .unwrap();
        fs::write(info.join("unrelated.list"), "/usr/bin/true\n").unwrap();
        let search = Search {
            path: vec![local_bin.clone(), usr_bin.clone()],
            extra_root: root.to_path_buf(),
            dpkg_info: info,
        };
        let scratch = root.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        let found = detect_with(PROBES, &search, &own, &scratch);

        let php = &found["php"];
        assert_eq!(php.version.as_deref(), Some("8.5.4"));
        assert_eq!(php.source, "apt");
        assert_eq!(php.source_package.as_deref(), Some("php8.5-cli"));
        assert_eq!(php.path, usr_bin.join("php").display().to_string());
        assert!(php.real_path.ends_with("/usr/bin/php8.5"));
        assert!(php.on_path);

        let node = &found["nodejs"];
        assert_eq!(node.version.as_deref(), Some("22.23.4"));
        assert_eq!(node.source, "manual");
        assert!(node.source_package.is_none());

        let go = &found["go"];
        assert_eq!(go.version.as_deref(), Some("1.26.3"));
        assert!(!go.on_path, "/usr/local/go/bin is not on the system PATH");

        // The Package Center shim was skipped; the real system python3 is
        // reported, without a version.
        let python = &found["python"];
        assert_eq!(python.path, usr_bin.join("python3").display().to_string());
        assert!(python.version.is_none());
        assert!(python.output.contains("no version"), "{}", python.output);
        assert!(
            fs::read_dir(&scratch).unwrap().next().is_none(),
            "scratch cleaned"
        );
    }

    #[test]
    fn a_command_moved_aside_for_the_system_wide_link_is_still_found() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("usr/local/bin");
        let own = dir.path().join("own");
        fs::create_dir_all(own.join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("usr/local/lib/nodejs/bin")).unwrap();
        fs::create_dir_all(&local).unwrap();
        let manual = dir.path().join("usr/local/lib/nodejs/bin/node");
        fs::write(&manual, "#!/bin/sh\necho v22.23.4\n").unwrap();
        fs::set_permissions(&manual, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(own.join("bin/node"), "#!/bin/sh\necho v22.23.5\n").unwrap();
        fs::set_permissions(own.join("bin/node"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(own.join("bin/node"), local.join("node")).unwrap();
        symlink("../lib/nodejs/bin/node", local.join(".rast-displaced.node")).unwrap();
        let search = Search {
            path: vec![local.clone()],
            extra_root: dir.path().to_path_buf(),
            dpkg_info: dir.path().join("missing"),
        };
        let probe = PROBES.iter().find(|p| p.id == "nodejs").unwrap();
        let found = find(probe, &search, &fs::canonicalize(&own).unwrap()).unwrap();
        assert!(found.displaced && found.on_path);
        assert_eq!(found.path, local.join(".rast-displaced.node"));
        assert_eq!(found.real, fs::canonicalize(&manual).unwrap());
    }

    #[test]
    fn nothing_is_reported_when_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let search = Search {
            path: vec![dir.path().join("bin")],
            extra_root: dir.path().to_path_buf(),
            dpkg_info: dir.path().join("missing"),
        };
        assert!(detect_with(PROBES, &search, &dir.path().join("own"), dir.path()).is_empty());
    }
}
