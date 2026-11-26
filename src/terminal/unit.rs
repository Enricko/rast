//! `rast terminal enable | disable | status`: the server owner turns the
//! Terminal app on (or off) by installing `rast-terminal.service`, the root
//! helper that starts administrators' shells. Needs root, on purpose: the web
//! interface can never give itself a root shell.

use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use super::{helper::passwd_by_uid, socket_path, TerminalClient, TerminalError, DEFAULT_PEER_USER};
use crate::config::Config;

pub const UNIT: &str = "rast-terminal.service";
pub const UNIT_DIR: &str = "/etc/systemd/system";
const MARKER: &str = "# Managed by RAST";

/// The unit file text. `peer_user` is the account `rast.service` runs as.
pub fn unit_text(binary: &Path, peer_user: &str) -> String {
    format!(
        "{MARKER} (`sudo rast terminal enable`); `sudo rast terminal disable` removes it.\n\
         [Unit]\n\
         Description=RAST terminal helper (administrator shells)\n\
         Documentation=https://github.com/Enricko/rast/blob/main/docs/TERMINAL.md\n\
         After=local-fs.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary} terminal helper\n\
         Environment=RAST_TERMINAL_PEER_USER={peer_user}\n\
         RuntimeDirectory=rast-terminal\n\
         RuntimeDirectoryMode=0750\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         # Administrators' shells run in this service as root, unconfined:\n\
         # that is what it is for. Stopping it ends every open shell.\n\
         KillMode=control-group\n\
         TimeoutStopSec=10\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        binary = binary.display(),
    )
}

/// The account that owns the data directory (`rast.service`'s account).
fn service_account(cfg: &Config) -> String {
    std::fs::metadata(&cfg.data_dir)
        .ok()
        .map(|meta| meta.uid())
        .filter(|uid| *uid != 0)
        .and_then(passwd_by_uid)
        .map(|account| account.name)
        .unwrap_or_else(|| DEFAULT_PEER_USER.into())
}

fn binary() -> PathBuf {
    let stable = Path::new(crate::update::BIN_PATH);
    if stable.exists() {
        return stable.to_path_buf();
    }
    std::env::current_exe().unwrap_or_else(|_| stable.to_path_buf())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn require_root(action: &str) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(format!(
            "{action} needs root: run `sudo rast terminal {action}`"
        ));
    }
    Ok(())
}

/// Install (or refresh) and start the helper.
pub async fn enable(cfg: &Config) -> Result<Vec<String>, String> {
    require_root("enable")?;
    let path = Path::new(UNIT_DIR).join(UNIT);
    let text = unit_text(&binary(), &service_account(cfg));
    let existing = std::fs::read_to_string(&path).ok();
    if existing.as_deref().is_some_and(|t| !t.starts_with(MARKER)) {
        return Err(format!(
            "{} exists and was not written by RAST; move it away first",
            path.display()
        ));
    }
    let mut done = Vec::new();
    let changed = existing.as_deref() != Some(text.as_str());
    if changed {
        std::fs::write(&path, &text)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        done.push(format!("wrote {}", path.display()));
        systemctl(&["daemon-reload"])?;
    }
    systemctl(&["enable", UNIT])?;
    // A changed unit restarts the helper (open shells end); otherwise start it.
    systemctl(&[if changed { "restart" } else { "start" }, UNIT])?;
    done.push(format!("started {UNIT}"));
    let client = TerminalClient::default();
    for _ in 0..30 {
        if client.status().await.is_ok() {
            return Ok(done);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "{UNIT} started but its socket ({}) does not answer; see `journalctl -u {UNIT}`",
        socket_path().display()
    ))
}

/// Stop and remove the helper (every open shell ends).
pub fn disable() -> Result<Vec<String>, String> {
    require_root("disable")?;
    let path = Path::new(UNIT_DIR).join(UNIT);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(vec![format!("{UNIT} is not installed")]);
    };
    if !text.starts_with(MARKER) {
        return Err(format!(
            "{} was not written by RAST; leaving it alone",
            path.display()
        ));
    }
    let _ = systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    Ok(vec![format!("stopped and removed {}", path.display())])
}

/// One line for `rast terminal status` and `rast doctor`; `Ok` when on.
pub async fn describe() -> Result<String, String> {
    match TerminalClient::default().status().await {
        Ok(status) => Ok(format!(
            "on ({UNIT}, shells run as {}, {} of {} open)",
            status["user"].as_str().unwrap_or("?"),
            status["sessions"],
            status["max"]
        )),
        Err(TerminalError::Off) => Err("off (turn on with `sudo rast terminal enable`)".into()),
        Err(error) => Err(format!("not answering: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_the_helper_for_the_service_account() {
        let text = unit_text(Path::new("/usr/local/bin/rast"), "rast");
        assert!(text.starts_with(MARKER));
        assert!(text.contains("ExecStart=/usr/local/bin/rast terminal helper\n"));
        assert!(text.contains("Environment=RAST_TERMINAL_PEER_USER=rast\n"));
        assert!(text.contains("RuntimeDirectory=rast-terminal\n"));
        assert!(!text.contains("User="), "the helper runs as root");
    }
}
