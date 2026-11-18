//! Tools the server's administrators installed for their own Linux account:
//! rustup in `~/.cargo/bin`, `claude` or `uv` in `~/.local/bin`, Bun, Deno, a
//! Flutter checkout, … A login of that account finds them; root and the RAST
//! service do not, because they live in a private home directory.
//!
//! Only accounts that may already become root (members of `sudo`, `admin` or
//! `wheel`) are considered, so nothing here gives an account more power than
//! it has: their tool directories are appended to the PATH of administrator
//! root shells, and App Center lists the runtimes found there as *System*
//! installs. This code runs in the privileged helper (the service cannot read
//! `/home`); version commands run as the owning account, never as root.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::system::{self, Probe, SystemInstall, PROBES};

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";
/// Groups whose members can become root.
const ADMIN_GROUPS: &[&str] = &["sudo", "admin", "wheel"];
/// Ordinary login accounts (Debian/Ubuntu/RHEL `UID_MIN`..`UID_MAX`).
const UID_RANGE: std::ops::RangeInclusive<u32> = 1000..=59_999;
const MAX_ACCOUNTS: usize = 8;
const RUN_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_LIMIT: u64 = 16 * 1024;

/// Where per-user installers put commands, relative to the home directory.
const TOOL_DIRS: &[&str] = &[
    ".local/bin",
    "bin",
    ".cargo/bin",
    ".bun/bin",
    ".deno/bin",
    "go/bin",
    ".dotnet",
    ".dotnet/tools",
    ".opencode/bin",
    ".npm-global/bin",
    ".local/share/pnpm",
    ".yarn/bin",
    ".config/composer/vendor/bin",
    ".composer/vendor/bin",
    ".pub-cache/bin",
    "flutter/bin",
    "development/flutter/bin",
    ".sdkman/candidates/java/current/bin",
    ".sdkman/candidates/kotlin/current/bin",
    ".sdkman/candidates/gradle/current/bin",
    ".sdkman/candidates/maven/current/bin",
    ".pyenv/shims",
    ".rbenv/shims",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// Login accounts that may become root, from passwd/group file contents.
pub fn admin_accounts_from(passwd: &str, group: &str) -> Vec<Account> {
    let mut admin_gids: HashSet<u32> = HashSet::new();
    let mut admin_names: HashSet<&str> = HashSet::new();
    for line in group.lines() {
        let mut fields = line.split(':');
        let (Some(name), Some(_), Some(gid)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if !ADMIN_GROUPS.contains(&name) {
            continue;
        }
        if let Ok(gid) = gid.parse() {
            admin_gids.insert(gid);
        }
        admin_names.extend(
            fields
                .next()
                .unwrap_or("")
                .split(',')
                .filter(|m| !m.is_empty()),
        );
    }
    let mut out = Vec::new();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        let (Ok(uid), Ok(gid)) = (fields[2].parse::<u32>(), fields[3].parse::<u32>()) else {
            continue;
        };
        let name = fields[0];
        let home = Path::new(fields[5]);
        let shell = fields[6];
        let valid_name = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            && !name.starts_with('-');
        if !valid_name
            || !UID_RANGE.contains(&uid)
            || !home.is_absolute()
            || home == Path::new("/")
            || shell.ends_with("/nologin")
            || shell.ends_with("/false")
            || !(admin_names.contains(name) || admin_gids.contains(&gid))
        {
            continue;
        }
        out.push(Account {
            name: name.to_string(),
            uid,
            gid,
            home: home.to_path_buf(),
        });
        if out.len() == MAX_ACCOUNTS {
            break;
        }
    }
    out
}

pub fn admin_accounts() -> Vec<Account> {
    let passwd = fs::read_to_string(PASSWD).unwrap_or_default();
    let group = fs::read_to_string(GROUP).unwrap_or_default();
    admin_accounts_from(&passwd, &group)
}

/// The account's tool directories that exist. A directory someone else could
/// write to (group/world writable, or not owned by the account) is skipped.
pub fn tool_dirs(account: &Account) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    TOOL_DIRS
        .iter()
        .map(|dir| account.home.join(dir))
        .filter(|dir| {
            fs::metadata(dir).is_ok_and(|m| {
                m.is_dir()
                    && m.uid() == account.uid
                    && (m.permissions().mode() & 0o002 == 0)
                    && (m.permissions().mode() & 0o020 == 0 || m.gid() == account.gid)
            })
        })
        .collect()
}

/// Every administrator account's tool directories, for the end of a root
/// shell's PATH.
pub fn admin_tool_dirs() -> Vec<String> {
    admin_accounts()
        .iter()
        .flat_map(tool_dirs)
        .map(|dir| dir.display().to_string())
        .collect()
}

/// Official runtimes found in the administrators' tool directories, except
/// those in `skip` (already found system-wide).
pub fn detect(skip: &HashSet<String>) -> HashMap<String, SystemInstall> {
    detect_in(&admin_accounts(), PROBES, skip)
}

pub fn detect_in(
    accounts: &[Account],
    probes: &[Probe],
    skip: &HashSet<String>,
) -> HashMap<String, SystemInstall> {
    let mut out = HashMap::new();
    for account in accounts {
        let dirs = tool_dirs(account);
        if dirs.is_empty() {
            continue;
        }
        for probe in probes {
            if skip.contains(probe.id) || out.contains_key(probe.id) {
                continue;
            }
            let Some(path) = dirs.iter().map(|d| d.join(probe.command)).find(|p| {
                fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }) else {
                continue;
            };
            let real = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let version_command = std::iter::once(probe.command)
                .chain(probe.args().iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            let (version, output) = match system::version_from_files(probe.id, &real)
                .map(Ok)
                .unwrap_or_else(|| run_as(account, &path, probe.args(), &dirs))
            {
                Ok(text) => {
                    let line = text
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect::<String>();
                    match system::parse_version(probe.id, &text) {
                        Some(v) => (Some(v), line),
                        None => (None, format!("no version in the output: {line}")),
                    }
                }
                Err(e) => (None, e),
            };
            out.insert(
                probe.id.to_string(),
                SystemInstall {
                    command: probe.command.to_string(),
                    path: path.display().to_string(),
                    real_path: real.display().to_string(),
                    version,
                    version_command,
                    output,
                    source: "manual".into(),
                    source_package: None,
                    on_path: false,
                    displaced: false,
                    detected_at: crate::db::now_ms(),
                },
            );
        }
    }
    out
}

/// Run `program args` as `account` (its uid, gid and groups via `runuser`),
/// with a short timeout and a small environment. stdout and stderr are read
/// together (Kotlin and old Java print the version on stderr).
fn run_as(
    account: &Account,
    program: &Path,
    args: &[&str],
    dirs: &[PathBuf],
) -> Result<String, String> {
    let path = dirs
        .iter()
        .map(|d| d.display().to_string())
        .chain(system::SYSTEM_PATH.iter().map(|d| d.to_string()))
        .collect::<Vec<_>>()
        .join(":");
    let mut command = if unsafe { libc::geteuid() } == account.uid {
        Command::new(program)
    } else {
        let mut c = Command::new("/usr/sbin/runuser");
        c.arg("-u").arg(&account.name).arg("--").arg(program);
        c
    };
    let mut child = command
        .args(args)
        .current_dir("/")
        .env_clear()
        .env("PATH", path)
        .env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("LANG", "C.UTF-8")
        .env("GOTOOLCHAIN", "local")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let read = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(stream) = stream {
                let _ = stream.take(OUTPUT_LIMIT).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= RUN_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not answer in time", program.display()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("cannot wait for {}: {e}", program.display())),
        }
    };
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    if !status.success() && system::parse_version("", &text).is_none() {
        let line = text.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(format!("exited with {status}: {}", line.unwrap_or("")));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
        ada:x:1000:1000:Ada:/home/ada:/bin/bash\n\
        bob:x:1001:1001:Bob:/home/bob:/bin/bash\n\
        svc:x:1002:27:Service:/home/svc:/usr/sbin/nologin\n\
        eve:x:1003:27:Eve:/home/eve:/bin/zsh\n\
        rast-u7:x:200007:200007::/var/lib/rast/linux-users/7:/usr/sbin/nologin\n";
    const GROUP: &str = "root:x:0:\nsudo:x:27:ada\nusers:x:100:bob\n";

    #[test]
    fn only_login_accounts_that_can_become_root_count() {
        let names: Vec<String> = admin_accounts_from(PASSWD, GROUP)
            .into_iter()
            .map(|a| a.name)
            .collect();
        // ada is a member of sudo, eve has it as primary group; bob is an
        // ordinary user, svc cannot log in, root and system accounts are out.
        assert_eq!(names, vec!["ada", "eve"]);
    }

    fn me(home: &Path) -> Account {
        Account {
            name: "me".into(),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            home: home.to_path_buf(),
        }
    }

    #[test]
    fn runtimes_in_an_accounts_tool_directories_are_found() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".cargo/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("rustc"),
            "#!/bin/sh\necho 'rustc 1.99.1 (abc 2026-10-02)'\n",
        )
        .unwrap();
        fs::set_permissions(bin.join("rustc"), fs::Permissions::from_mode(0o755)).unwrap();
        // A directory others can write to is never trusted.
        let open = home.path().join(".bun/bin");
        fs::create_dir_all(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(open.join("bun"), "#!/bin/sh\necho 1.4.4\n").unwrap();
        fs::set_permissions(open.join("bun"), fs::Permissions::from_mode(0o755)).unwrap();

        let account = me(home.path());
        assert_eq!(tool_dirs(&account), vec![bin.clone()]);
        let found = detect_in(std::slice::from_ref(&account), PROBES, &HashSet::new());
        assert_eq!(found.len(), 1);
        let rust = &found["rust"];
        assert_eq!(rust.version.as_deref(), Some("1.99.1"));
        assert_eq!(rust.path, bin.join("rustc").display().to_string());
        assert!(!rust.on_path && rust.source == "manual");
        // Already found system-wide: not reported again.
        let skip = HashSet::from(["rust".to_string()]);
        assert!(detect_in(&[account], PROBES, &skip).is_empty());
    }

    #[test]
    fn a_flutter_checkout_is_read_from_its_version_file() {
        let home = tempfile::tempdir().unwrap();
        let sdk = home.path().join("flutter");
        fs::create_dir_all(sdk.join("bin")).unwrap();
        fs::write(sdk.join("bin/flutter"), "#!/bin/sh\nsleep 60\n").unwrap();
        fs::set_permissions(sdk.join("bin/flutter"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(sdk.join("version"), "3.47.9\n").unwrap();
        let found = detect_in(&[me(home.path())], PROBES, &HashSet::new());
        assert_eq!(found["flutter"].version.as_deref(), Some("3.47.9"));
    }
}
