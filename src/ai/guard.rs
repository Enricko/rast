//! What agents may see of the server for accounts that are not
//! administrators (administrators are root on the server anyway).
//!
//! * Reading server files: only what any unprivileged account could read
//!   (the file world-readable, every folder above it world-searchable), never
//!   RAST's own data, the people's files (those go through the file tools and
//!   their permissions), account and network secrets, other accounts' home
//!   folders or processes, and no file whose name or content says secret.
//! * Commands: [`guest_script`] runs them as `nobody` in a transient systemd
//!   service where the whole file system is read-only (a private `/tmp` is
//!   thrown away afterwards), the same places are hidden, there is no network
//!   and no way to gain privileges or see other processes.
//! * What either returns passes through [`redact`]: private keys, password
//!   hashes, well-known token formats and the values of `password=`-style
//!   settings are replaced with `[hidden]`.

use std::path::{Component, Path, PathBuf};

/// Places hidden on every server, besides RAST's own folders.
const HIDDEN: &[&str] = &[
    "/etc/rast",
    "/etc/cloudflared",
    "/etc/netplan",
    "/etc/NetworkManager/system-connections",
    "/etc/ssh",
    "/etc/ssl/private",
    "/etc/wireguard",
    "/etc/letsencrypt",
    "/etc/sudoers",
    "/etc/sudoers.d",
    "/etc/shadow",
    "/etc/shadow-",
    "/etc/gshadow",
    "/etc/gshadow-",
    "/etc/security/opasswd",
    "/root",
    "/home",
    "/run/user",
    "/run/rast",
    "/run/rast-terminal",
    "/run/docker.sock",
    "/var/run/docker.sock",
    "/run/containerd",
    "/var/lib/docker",
    "/var/lib/containerd",
    "/var/lib/rast",
    "/var/backups",
    "/var/spool/cron",
    "/sys/firmware",
];

/// The only things read from `/proc` (the rest describes processes, RAST's
/// own included).
const PROC_ALLOWED: &[&str] = &[
    "/proc",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/loadavg",
    "/proc/uptime",
    "/proc/version",
    "/proc/stat",
    "/proc/diskstats",
    "/proc/partitions",
    "/proc/swaps",
    "/proc/filesystems",
];

/// File and folder names that hold secrets wherever they are.
const SECRET_NAMES: &[&str] = &[
    "shadow",
    "gshadow",
    ".netrc",
    ".pgpass",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    "htpasswd",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "rast.toml",
];
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "private",
    "apikey",
    "api_key",
    "api-key",
];
const SECRET_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "gpg", "asc", "ovpn", "db", "sqlite",
    "sqlite3", "db-wal", "db-shm",
];

/// Everything hidden on this server: the fixed places and RAST's folders.
/// `private`: RAST's own places (`Host::private_paths`).
pub fn hidden_paths(private: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = HIDDEN.iter().map(PathBuf::from).collect();
    out.extend(private);
    out.sort();
    out.dedup();
    out
}

/// Whether a name (one path component) says it holds a secret.
pub fn secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if SECRET_NAMES.contains(&lower.as_str()) {
        return true;
    }
    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        return true;
    }
    if SECRET_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| SECRET_EXTENSIONS.contains(&ext))
}

/// Why `path` (absolute, symlinks resolved) may not be read, if so.
pub fn refusal(path: &Path, hidden: &[PathBuf]) -> Option<&'static str> {
    if hidden.iter().any(|h| path.starts_with(h)) {
        return Some("that place on the server is private");
    }
    if path.starts_with("/proc") && !PROC_ALLOWED.iter().any(|p| path == Path::new(p)) {
        return Some("processes' details are private");
    }
    let secret = path.components().any(|c| match c {
        Component::Normal(name) => secret_name(&name.to_string_lossy()),
        _ => false,
    });
    if secret {
        return Some("it looks like it holds passwords, keys or other secrets");
    }
    None
}

/// The real path of `raw` when it may be read (or listed): resolved, not
/// hidden, not secret, and readable by any account on the server.
pub fn allowed_path(raw: &str, hidden: &[PathBuf]) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let given = Path::new(raw.trim());
    if !given.is_absolute() {
        return Err("give an absolute path on the server, such as /etc/os-release".into());
    }
    let real = std::fs::canonicalize(given).map_err(|_| format!("{raw} does not exist"))?;
    if let Some(why) = refusal(&real, hidden) {
        return Err(format!("{} cannot be read: {why}", real.display()));
    }
    let meta = std::fs::metadata(&real).map_err(|e| format!("cannot read {raw}: {e}"))?;
    // Every folder above it must let anyone through (o+x)…
    for dir in real.ancestors().skip(1) {
        let searchable = std::fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o001 != 0);
        if !searchable {
            return Err(format!(
                "{} cannot be read: only some accounts on the server may open {}",
                real.display(),
                dir.display()
            ));
        }
    }
    // …and it must be readable by anyone (folders also searchable).
    let mode = meta.permissions().mode();
    let open = mode & 0o004 != 0 && (!meta.is_dir() || mode & 0o001 != 0);
    if !open {
        return Err(format!(
            "{} cannot be read: only some accounts on the server may read it",
            real.display()
        ));
    }
    Ok(real)
}

/// Text with secrets replaced by `[hidden]`, and how many were.
pub fn redact(text: &str) -> (String, usize) {
    let mut count = 0;
    let mut out = String::with_capacity(text.len());
    let mut in_key = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let end = &line[body.len()..];
        if in_key {
            if body.contains("-----END") {
                in_key = false;
            }
            continue;
        }
        if body.contains("-----BEGIN") && body.contains("PRIVATE KEY") {
            count += 1;
            out.push_str("[private key hidden]");
            out.push_str(end);
            in_key = !body.contains("-----END");
            continue;
        }
        let (line, n) = redact_line(body);
        count += n;
        out.push_str(&line);
        out.push_str(end);
    }
    (out, count)
}

fn redact_line(line: &str) -> (String, usize) {
    // `password = x`, `DB_PASSWORD=x`, `"api_key": "x"`, `token: x`, also
    // after a file name (`/etc/app.conf:password=x` from grep).
    let key_char = |c: char| c.is_ascii_alphanumeric() || "_-.\"'".contains(c);
    for (at, _) in line.match_indices(['=', ':']) {
        let before = line[..at].trim_end();
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| !key_char(*c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let key = before[start..]
            .trim_matches(['"', '\''])
            .to_ascii_lowercase();
        if key.is_empty() || key.len() > 60 {
            continue;
        }
        let last = key.rsplit(['_', '-', '.']).next().unwrap_or("");
        let worded = SECRET_WORDS.iter().any(|w| key.contains(w))
            || ["auth", "pass", "pwd", "pin"].contains(&last)
            || [
                "authorization",
                "proxy-authorization",
                "cookie",
                "set-cookie",
            ]
            .contains(&key.as_str());
        let value = line[at + 1..].trim();
        if worded && !value.is_empty() && value != "\"\"" && value != "''" {
            return (format!("{} [hidden]", &line[..=at]), 1);
        }
    }
    redact_words(line)
}

/// Password hashes and tokens with well-known prefixes, anywhere in a line.
fn redact_words(line: &str) -> (String, usize) {
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-", 20),
        ("sk_live_", 16),
        ("rk_live_", 16),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghs_", 20),
        ("ghu_", 20),
        ("github_pat_", 20),
        ("glpat-", 16),
        ("xoxb-", 16),
        ("xoxp-", 16),
        ("xapp-", 16),
        ("AKIA", 16),
        ("ASIA", 16),
        ("AIza", 30),
        ("$1$", 8),
        ("$2a$", 20),
        ("$2b$", 20),
        ("$2y$", 20),
        ("$5$", 8),
        ("$6$", 8),
        ("$y$", 8),
        ("$7$", 8),
    ];
    let token_char = |c: char| c.is_ascii_alphanumeric() || "_-./$".contains(c);
    let mut out = String::with_capacity(line.len());
    let mut count = 0;
    let mut rest = line;
    'scan: while !rest.is_empty() {
        for (prefix, min) in PREFIXES {
            if let Some(tail) = rest.strip_prefix(prefix) {
                let len = tail.find(|c: char| !token_char(c)).unwrap_or(tail.len());
                // A word on its own (not the middle of another one).
                let starts_word = out.chars().last().is_none_or(|c| !token_char(c));
                if len >= *min && starts_word {
                    out.push_str("[hidden]");
                    count += 1;
                    rest = &tail[len..];
                    continue 'scan;
                }
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    (out, count)
}

/// A string as one word for a POSIX shell.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Whether a guest command's exit code is `timeout`'s for "stopped at the
/// limit" (124, or 137 when it had to be killed).
pub fn timed_out(exit_code: Option<i32>) -> bool {
    matches!(exit_code, Some(124 | 137))
}

/// The root script that runs `command` read-only as `nobody` (see the
/// module docs), stopping it after `secs` (see [`timed_out`]).
pub fn guest_script(command: &str, secs: u32, hidden: &[PathBuf]) -> Result<String, String> {
    let mut args: Vec<String> = [
        "--quiet",
        "--pipe",
        "--wait",
        "--collect",
        "--service-type=exec",
        // The command is the shell's, not systemd's: `$x` and `%i` stay.
        "--expand-environment=no",
        "--description=RAST agent command (read-only)",
        "--property=User=nobody",
        "--property=Group=nogroup",
        "--property=ProtectSystem=strict",
        "--property=ProtectHome=yes",
        "--property=PrivateTmp=yes",
        "--property=PrivateDevices=yes",
        "--property=PrivatePIDs=yes",
        "--property=ProtectProc=invisible",
        "--property=ProtectKernelTunables=yes",
        "--property=ProtectKernelModules=yes",
        "--property=ProtectKernelLogs=yes",
        "--property=ProtectControlGroups=yes",
        "--property=ProtectClock=yes",
        "--property=ProtectHostname=yes",
        "--property=NoNewPrivileges=yes",
        "--property=RestrictSUIDSGID=yes",
        "--property=CapabilityBoundingSet=",
        "--property=LockPersonality=yes",
        "--property=RestrictRealtime=yes",
        "--property=RestrictNamespaces=yes",
        "--property=IPAddressDeny=any",
        "--property=RestrictAddressFamilies=AF_UNIX AF_NETLINK",
        "--property=MemoryMax=1G",
        "--property=TasksMax=128",
        "--property=CPUQuota=200%",
        "--property=WorkingDirectory=/tmp",
        "--property=Environment=HOME=/tmp TERM=dumb PAGER=cat SYSTEMD_PAGER=cat NO_COLOR=1 LANG=C.UTF-8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // `timeout` stops the command; systemd stops whatever is left soon
    // after (and kills what ignores that).
    args.push(format!("--property=RuntimeMaxSec={}", secs + 10));
    args.push("--property=TimeoutStopSec=3".into());
    for path in hidden {
        let text = path.to_string_lossy();
        if text
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
        {
            return Err("a server folder RAST hides has an unusual name; commands are off".into());
        }
        // `-`: fine when it does not exist; quoted: spaces stay in the path.
        args.push(format!("--property=InaccessiblePaths=\"-{text}\""));
    }
    args.push("--".into());
    // The first process in its own process namespace ignores signals it has
    // no handler for, so bash there would outlive the limit: `timeout` is
    // first, and stops bash and all it started (TERM, then KILL 3 s later).
    args.push("/usr/bin/timeout".into());
    args.push("--kill-after=3".into());
    args.push(secs.to_string());
    args.push("/bin/bash".into());
    args.push("--noprofile".into());
    args.push("--norc".into());
    args.push("-c".into());
    args.push(command.to_string());
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    Ok(format!("exec systemd-run {}", quoted.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_hidden() {
        let text = "\
user=alice
DB_PASSWORD=hunter2
  \"api_key\": \"sk-abcdefghijklmnopqrstuvwx\",
token: abc
password=
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAA
-----END OPENSSH PRIVATE KEY-----
root:$6$saltsalt$0123456789abcdefABCDEF:19000:0:99999:7:::
export GH=ghp_0123456789abcdefghijABCDEF and more
the ask-me endpoint is fine
/etc/app.conf:password=abc
Tokens used: 12
Authorization: Bearer abc.def
";
        let (out, count) = redact(text);
        assert_eq!(
            out,
            "\
user=alice
DB_PASSWORD= [hidden]
  \"api_key\": [hidden]
token: [hidden]
password=
[private key hidden]
root:[hidden]:19000:0:99999:7:::
export GH=[hidden] and more
the ask-me endpoint is fine
/etc/app.conf:password= [hidden]
Tokens used: 12
Authorization: [hidden]
"
        );
        assert_eq!(count, 8);
        // Ordinary text is left alone.
        let plain = "PRETTY_NAME=\"Ubuntu 26.04\"\nload: 0.5 1.0\nsk-short\n";
        assert_eq!(redact(plain), (plain.to_string(), 0));
    }

    #[test]
    fn secret_names_are_recognised() {
        for name in [
            "shadow",
            ".env",
            "prod.env",
            ".env.local",
            "id_ed25519",
            "server.key",
            "cert.PEM",
            "db_password.txt",
            "client_secret.json",
            "rast.toml",
            "data.sqlite",
            ".ssh",
        ] {
            assert!(secret_name(name), "{name}");
        }
        for name in [
            "os-release",
            "hostname",
            "nginx.conf",
            "README.md",
            "keyboard",
        ] {
            assert!(!secret_name(name), "{name}");
        }
    }

    #[test]
    // Linux paths (/etc/hostname; macOS resolves /var/tmp under /private).
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the server guard is for Linux servers"
    )]
    fn private_places_are_refused() {
        let hidden = vec![PathBuf::from("/srv/rast"), PathBuf::from("/etc/rast")];
        let refused = |p: &str| refusal(Path::new(p), &hidden).is_some();
        assert!(refused("/srv/rast/homes/alice/notes.txt"));
        assert!(refused("/etc/rast/rast.toml"));
        assert!(refused("/proc/self/environ"));
        assert!(refused("/proc/1/cmdline"));
        assert!(refused("/opt/app/.env"));
        assert!(refused("/opt/app/.ssh/config"));
        assert!(!refused("/proc/meminfo"));
        assert!(!refused("/etc/os-release"));
        assert!(!refused("/srv/rastafari/readme"));
        // On disk: world-readable only, symlinks resolved.
        assert!(allowed_path("/etc/hostname", &hidden).is_ok());
        assert!(allowed_path("relative/path", &hidden).is_err());
        // Somewhere every account may pass through (not a private TMPDIR).
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let open = dir.path().join("open.txt");
        std::fs::write(&open, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("/srv/rast/homes", &link).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = allowed_path(open.to_str().unwrap(), &hidden).unwrap_err();
        assert!(err.contains("only some accounts"), "{err}");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(allowed_path(open.to_str().unwrap(), &hidden).is_err());
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(allowed_path(open.to_str().unwrap(), &hidden).is_ok());
    }

    #[test]
    fn the_guest_script_quotes_the_command_whole() {
        let script = guest_script(
            "echo 'it''s' \"$HOME\"; rm -rf /",
            30,
            &[PathBuf::from("/srv/my rast")],
        )
        .unwrap();
        assert!(script.starts_with("exec systemd-run '--quiet'"));
        assert!(script.contains("'--property=RuntimeMaxSec=40'"));
        assert!(script.contains("'/usr/bin/timeout' '--kill-after=3' '30' '/bin/bash'"));
        assert!(script.contains("'--property=InaccessiblePaths=\"-/srv/my rast\"'"));
        assert!(script.ends_with(r#"'-c' 'echo '\''it'\'''\''s'\'' "$HOME"; rm -rf /'"#));
        // What bash makes of it: the command arrives as one argument.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -- {}; printf '%s' \"${{@: -1}}\"",
                script.trim_start_matches("exec systemd-run ")
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "echo 'it''s' \"$HOME\"; rm -rf /"
        );
        assert!(guest_script("ls", 5, &[PathBuf::from("/odd\"name")]).is_err());
    }
}
