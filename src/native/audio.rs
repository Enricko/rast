//! Playback-only audio for one native identity, never the host audio server.
//!
//! Xpra owns the foreground PulseAudio child inside its existing session cgroup.
//! `-n` skips default.pa (and therefore hardware discovery), and module loading
//! is disabled after the two explicit startup modules. No TCP listener, ALSA
//! device, microphone source, X11 publishing or system-wide daemon is involved.

use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use super::protocol::NativeError;

const CONFIG_DIR: &str = ".xpra-audio";
const CONFIG: &str = "# RAST playback-only audio; do not run distro Pulse configure commands.\n\
                       pulseaudio-configure-commands = none\n";

pub(crate) fn config_dir(runtime_root: &Path) -> PathBuf {
    runtime_root.join(CONFIG_DIR)
}

/// Xpra 6.5 appends list-valued CLI flags to distro defaults: specifying
/// `--pulseaudio-configure-commands=none` when /etc/xpra already sets `none`
/// produces two shell commands named `none`. A final config layer replaces the
/// list instead. This root-owned directory is outside the identity-writable
/// trees, overrides only this setting and is never read as a Pulse script.
pub(crate) fn prepare_config(runtime_root: &Path) -> Result<(), NativeError> {
    let dir = config_dir(runtime_root);
    let failure = |error| {
        NativeError::unavailable(format!(
            "cannot prepare Xpra audio config {}: {error}",
            dir.display()
        ))
    };
    // The managed runtime root is already checked/root-owned by helper startup.
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failure(error)),
    }
    let metadata = std::fs::symlink_metadata(&dir).map_err(failure)?;
    // In production euid is root; tests exercise the same checks as their uid.
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o022 != 0 {
        return Err(NativeError::forbidden("Xpra audio config directory must be a real, helper-owned directory, not writable by other users"));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).map_err(failure)?;
    // Do not truncate until the opened inode has been checked; never follow a
    // planted symlink or write a hard-linked file, device, fifo or user file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("xpra.conf"))
        .map_err(failure)?;
    let metadata = file.metadata().map_err(failure)?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(NativeError::forbidden(
            "Xpra audio config must be a helper-owned regular file with no hard links",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o644))
        .map_err(failure)?;
    file.set_len(0).map_err(failure)?;
    file.write_all(CONFIG.as_bytes()).map_err(failure)?;
    Ok(())
}

/// Xpra's server-side speaker is a recording source: the monitor of this sink.
pub(crate) const SPEAKER_SINK: &str = "Xpra-Speaker";
pub(crate) const SPEAKER_MONITOR: &str = "Xpra-Speaker.monitor";

/// Pin every Pulse endpoint to the identity's 0700 runtime tree. Apps have
/// distinct profile HOMEs, so neither their Pulse defaults nor the helper's
/// environment may select a different server. An explicit PULSE_SERVER also
/// prevents libpulse from autospawning a fallback daemon when this one is down.
pub(crate) fn environment(runtime: &Path) -> BTreeMap<String, String> {
    let pulse = runtime.join("pulse");
    BTreeMap::from([
        (
            "PULSE_SERVER".into(),
            format!("unix:{}", pulse.join("native").display()),
        ),
        ("PULSE_RUNTIME_PATH".into(), pulse.display().to_string()),
        ("PULSE_STATE_PATH".into(), pulse.display().to_string()),
        ("PULSE_CONFIG_PATH".into(), pulse.display().to_string()),
        (
            "PULSE_COOKIE".into(),
            pulse.join("cookie").display().to_string(),
        ),
        ("PULSE_SINK".into(), SPEAKER_SINK.into()),
        ("PULSE_SOURCE".into(), SPEAKER_MONITOR.into()),
    ])
}

/// A command string is required by Xpra's API, but Xpra 6.5 shlex-splits it
/// before Popen (no shell). It expands the two pinned Pulse path variables.
/// Do not use Xpra's `auto` command: it also creates microphone devices and can
/// read an administrator's xpra.pa. No --start: never adopt another daemon.
pub(crate) fn pulseaudio_command() -> &'static str {
    "/usr/bin/pulseaudio -n --daemonize=false --system=false --fail=true \
     --use-pid-file=false --exit-idle-time=-1 --disable-shm=yes \
     --realtime=no --high-priority=no --disallow-module-loading=yes \
     --log-target=stderr \
     --load='module-null-sink sink_name=Xpra-Speaker channels=2 rate=48000' \
     --load='module-native-protocol-unix socket=$PULSE_RUNTIME_PATH/native auth-anonymous=0 auth-cookie-enabled=1 auth-cookie=$PULSE_COOKIE'"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_one_final_list_override_and_rejects_link_attacks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        prepare_config(root.path()).unwrap();
        let file = config_dir(root.path()).join("xpra.conf");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), CONFIG);
        prepare_config(root.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), CONFIG);
        let other = root.path().join("untouched");
        std::fs::write(&other, "untouched").unwrap();
        std::fs::remove_file(&file).unwrap();
        symlink(&other, &file).unwrap();
        assert!(prepare_config(root.path()).is_err());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "untouched");
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&other, &file).unwrap();
        assert!(prepare_config(root.path()).is_err());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "untouched");
        let another_root = tempfile::tempdir().unwrap();
        symlink(root.path(), config_dir(another_root.path())).unwrap();
        assert!(prepare_config(another_root.path()).is_err());
    }

    #[test]
    fn pulse_endpoints_are_private_and_independent_of_app_home() {
        let first = environment(Path::new("/run/rast-linux/200001"));
        let second = environment(Path::new("/run/rast-linux/200002"));
        assert_eq!(
            first["PULSE_SERVER"],
            "unix:/run/rast-linux/200001/pulse/native"
        );
        assert_eq!(first["PULSE_COOKIE"], "/run/rast-linux/200001/pulse/cookie");
        assert_ne!(first["PULSE_SERVER"], second["PULSE_SERVER"]);
        assert_ne!(first["PULSE_COOKIE"], second["PULSE_COOKIE"]);
        assert_eq!(first["PULSE_SOURCE"], SPEAKER_MONITOR);
        assert_eq!(first["PULSE_SINK"], SPEAKER_SINK);
        assert!(!first.contains_key("HOME"));
    }

    #[test]
    fn daemon_has_only_a_null_sink_and_authenticated_local_socket() {
        let command = pulseaudio_command();
        assert!(command.starts_with("/usr/bin/pulseaudio -n "));
        assert!(command.contains("--daemonize=false"));
        assert!(command.contains("--system=false"));
        assert!(command.contains("--disallow-module-loading=yes"));
        assert!(command.contains("--realtime=no --high-priority=no"));
        assert_eq!(command.matches("--load=").count(), 2);
        assert!(command.contains("auth-anonymous=0 auth-cookie-enabled=1"));
        for forbidden in [
            "--start",
            "--file",
            "alsa",
            "udev",
            "tcp",
            "remap",
            "Microphone",
        ] {
            assert!(!command.contains(forbidden), "{forbidden}");
        }
    }
}
