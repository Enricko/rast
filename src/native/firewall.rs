//! Kernel-level restriction of the loopback Xpra display ports.
//!
//! Xpra's HTML5 transport listens on `127.0.0.1:<ephemeral>` and is reached by
//! the unprivileged RAST server, which proxies every browser request after
//! authenticating it. Loopback is **not** a security boundary between local
//! uids: another unprivileged account on the host could connect to the port
//! and drive that person's graphical session. To close that hole without
//! teaching Xpra's HTML5 client a shared secret (which would have to appear in
//! a process command line readable by every local user), the helper installs
//! one nftables rule at startup:
//!
//! ```text
//! table inet rast_native {
//!   set xpra_ports { type inet_service; }
//!   chain output {
//!     type filter hook output priority 0; policy accept;
//!     ip daddr 127.0.0.1 tcp dport @xpra_ports \
//!        meta skuid != <rast uid> reject with tcp reset
//!   }
//! }
//! ```
//!
//! Every session port is added to the set before the session is usable and
//! removed when the session stops. The set is reconciled against the live
//! `rast-xpra-*` units on helper start, so sessions that survived a helper
//! restart stay protected and ports of dead sessions never leak.
//!
//! The rule is defence in depth: the display port itself still has no
//! application-level authentication, but it is unreachable for every local
//! account except the authenticated gateway (the `rast` service account).

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Duration,
};

use super::{is_root, run_command, NativeError, NativeOptions};

/// nftables table owned by RAST (never flushes anything else).
const TABLE: &str = "rast_native";
const CHAIN: &str = "output";
const SET: &str = "xpra_ports";
const NFT_CANDIDATES: &[&str] = &[
    "/usr/sbin/nft",
    "/sbin/nft",
    "/usr/bin/nft",
    "/usr/local/sbin/nft",
];
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: usize = 64 * 1024;

static NFT_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
/// True only in a production helper that successfully installed the rule.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Absolute path of the nftables binary, resolved once.
pub(crate) fn nft_binary() -> Option<PathBuf> {
    NFT_BIN
        .get_or_init(|| {
            NFT_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|candidate| candidate.is_file())
        })
        .clone()
}

/// The Xpra display-port restriction is actually installed (production only).
pub(crate) fn enforced() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

async fn nft(args: &[&str]) -> Result<super::CommandOutput, NativeError> {
    let Some(binary) = nft_binary() else {
        return Err(NativeError::unavailable(
            "nftables is not installed; the Xpra display ports cannot be restricted",
        ));
    };
    run_command(
        &binary,
        args.iter().map(std::ffi::OsString::from).collect(),
        &[],
        COMMAND_TIMEOUT,
        MAX_OUTPUT,
        MAX_OUTPUT,
    )
    .await
}

async fn nft_ok(args: &[&str]) -> bool {
    matches!(nft(args).await, Ok(output) if output.success)
}

/// Install the owned table/chain/set/rule when it does not exist yet.
async fn ensure_ruleset(allowed_uid: u32) -> Result<(), NativeError> {
    if nft_ok(&["list", "table", "inet", TABLE]).await {
        return Ok(());
    }
    let rules = format!(
        "add table inet {TABLE}\n\
         add chain inet {TABLE} {CHAIN} {{ type filter hook output priority 0; policy accept; }}\n\
         add set inet {TABLE} {SET} {{ type inet_service; }}\n\
         add rule inet {TABLE} {CHAIN} ip daddr 127.0.0.1 tcp dport @{SET} \
         meta skuid != {allowed_uid} reject with tcp reset\n"
    );
    let path =
        std::env::temp_dir().join(format!("rast-display-firewall-{}.nft", std::process::id()));
    write_private(&path, rules.as_bytes())?;
    let result = nft(&["-f", &path.to_string_lossy()]).await;
    let _ = std::fs::remove_file(&path);
    let result = result?;
    if result.success {
        tracing::info!(
            allowed_uid,
            "restricted Xpra display ports to the RAST service account (nftables)"
        );
        Ok(())
    } else {
        Err(NativeError::command_failed(format!(
            "could not install the Xpra display-port firewall rule: {}",
            super::summarize_command(&result)
        )))
    }
}

fn write_private(path: &Path, contents: &[u8]) -> Result<(), NativeError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            NativeError::unavailable(format!(
                "cannot write the display firewall ruleset {}: {error}",
                path.display()
            ))
        })?;
    file.write_all(contents).map_err(|error| {
        NativeError::unavailable(format!(
            "cannot write the display firewall ruleset {}: {error}",
            path.display()
        ))
    })
}

/// The port of a running `rast-xpra-*` unit, read from its Xpra command line.
pub(crate) async fn session_port(unit: &str) -> Option<u16> {
    let show = nft_systemctl(&["show", unit, "--property=MainPID", "--value"]).await?;
    let pid: u32 = show.trim().parse().ok()?;
    if pid == 0 {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    port_from_cmdline(&cmdline)
}

async fn nft_systemctl(args: &[&str]) -> Option<String> {
    let output = run_command(
        Path::new(super::SYSTEMCTL_BIN),
        args.iter().map(std::ffi::OsString::from).collect(),
        &[],
        COMMAND_TIMEOUT,
        MAX_OUTPUT,
        MAX_OUTPUT,
    )
    .await
    .ok()?;
    output.success.then_some(output.stdout)
}

/// Parse `--bind-tcp=127.0.0.1:<port>` out of an Xpra command line.
fn port_from_cmdline(cmdline: &[u8]) -> Option<u16> {
    for arg in cmdline.split(|byte| *byte == 0) {
        let Ok(text) = std::str::from_utf8(arg) else {
            continue;
        };
        let Some(value) = text.strip_prefix("--bind-tcp=") else {
            continue;
        };
        // Xpra accepts host:port and host:port:options; the port is the last
        // numeric field before the options when no options are set.
        let host_port = value.split(':').collect::<Vec<_>>();
        if host_port.len() < 2 {
            continue;
        }
        if let Some(port) = host_port
            .get(1)
            .and_then(|part| part.split(',').next())
            .and_then(|part| part.parse::<u16>().ok())
        {
            return Some(port);
        }
    }
    None
}

/// Add one session port to the restricted set.
pub(crate) async fn allow_port(port: u16) -> Result<(), NativeError> {
    if !enforced() {
        return Ok(());
    }
    let port = port.to_string();
    let result = nft(&["add", "element", "inet", TABLE, SET, "{", &port, "}"]).await?;
    if result.success {
        Ok(())
    } else {
        Err(NativeError::command_failed(format!(
            "could not restrict display port {port}: {}",
            super::summarize_command(&result)
        )))
    }
}

/// Remove one session port (best effort: the session is already gone).
pub(crate) async fn release_port(port: u16) {
    if !enforced() {
        return;
    }
    let port = port.to_string();
    match nft(&["delete", "element", "inet", TABLE, SET, "{", &port, "}"]).await {
        Ok(output) if output.success => {}
        Ok(output) => tracing::debug!(
            port,
            "display port was not in the restricted set: {}",
            super::summarize_command(&output)
        ),
        Err(error) => tracing::debug!(port, "could not release display port: {error}"),
    }
}

/// Install the rule and reconcile the port set with the live sessions.
pub(crate) async fn init(options: &NativeOptions) -> Result<(), NativeError> {
    let production = options.require_root && is_root();
    if nft_binary().is_none() {
        if production {
            tracing::warn!(
                "nftables (nft) is not installed: Xpra display ports stay reachable by other \
                 local accounts. Install the nftables package (see docs/INSTALL.md, section \
                 'Native Linux apps (Xpra)')."
            );
        }
        return Ok(());
    }
    if !production {
        // Tests and unprivileged runs cannot (and must not) install rules.
        return Ok(());
    }
    let Some(allowed_uid) = options.allowed_peer_uid.or(options.rast_uid) else {
        tracing::warn!(
            "no RAST service uid is configured: cannot restrict the Xpra display ports to it"
        );
        return Ok(());
    };
    ensure_ruleset(allowed_uid).await?;
    reconcile().await?;
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Make the set match the live `rast-xpra-*` units exactly.
async fn reconcile() -> Result<(), NativeError> {
    if !nft_ok(&["list", "set", "inet", TABLE, SET]).await {
        return Ok(());
    }
    let units = unit_list().await;
    // Flushing is safe only for ports we can re-add: sessions that are still
    // running are discovered from their unit below.
    let _ = nft(&["flush", "set", "inet", TABLE, SET]).await;
    for unit in units {
        if let Some(port) = session_port(&unit).await {
            if let Err(error) = allow_port(port).await {
                tracing::warn!(
                    unit,
                    port,
                    "could not restore display-port protection: {error}"
                );
            }
        }
    }
    Ok(())
}

async fn unit_list() -> Vec<String> {
    let Some(output) = nft_systemctl(&[
        "list-units",
        "--type=service",
        "--state=running",
        "--no-legend",
        "--plain",
        "rast-xpra-*.service",
    ])
    .await
    else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("rast-xpra-"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_display_port_from_an_xpra_command_line() {
        let cmdline = b"/usr/bin/python3\0/usr/bin/xpra\0start\0:100\0--bind-tcp=127.0.0.1:41207\0--html=on\0";
        assert_eq!(port_from_cmdline(cmdline), Some(41207));

        let with_options = b"/usr/bin/xpra\0--bind-tcp=127.0.0.1:18457,debug\0--html=on\0";
        assert_eq!(port_from_cmdline(with_options), Some(18457));

        assert_eq!(port_from_cmdline(b"/usr/bin/xpra\0--html=on\0"), None);
        assert_eq!(
            port_from_cmdline(b"--bind-tcp=127.0.0.1:not-a-port\0"),
            None
        );
        assert_eq!(port_from_cmdline(b"--bind-tcp=]::1]:14500\0"), None);
    }
}
