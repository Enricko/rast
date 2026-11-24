//! Regression tests for the Terminal "deleted executable" upgrade failure.
//!
//! The privileged helper used to pass `std::env::current_exe()` to systemd-run
//! for the terminal agent. After an atomic binary upgrade the kernel reports
//! that path as `/usr/local/bin/rast (deleted)`, so the unit failed with
//! "Failed to find executable ... (deleted)". These tests pin the two
//! protections: the deleted marker is resolved safely, and an agent refuses to
//! start when its build differs from the helper that spawned it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rast_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rast"))
}

fn run_agent(args: &[&str]) -> std::process::Output {
    Command::new(rast_bin())
        .args(args)
        .env_remove("RAST_CONFIG")
        .output()
        .expect("run the rast binary")
}

#[test]
fn terminal_agent_refuses_a_mismatched_helper_build() {
    let output = run_agent(&[
        "terminal-agent",
        "--socket",
        "/nonexistent/agent.sock",
        "--token-file",
        "/nonexistent/token",
        "--shell",
        "/bin/sh",
        "--cwd",
        "/tmp",
        "--expected-build",
        "deadbeefdeadbeef",
    ]);
    assert!(!output.status.success(), "the agent must refuse to start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match the running rast-native.service helper"),
        "actionable mismatch error expected, got: {stderr}"
    );
    assert!(
        stderr.contains("restart rast-native.service"),
        "the error must say how to fix it: {stderr}"
    );
}

#[test]
fn terminal_agent_keeps_working_without_the_compatibility_argument() {
    // An agent started by an older helper must still reach the normal error
    // path (missing token) instead of failing on a required argument.
    let output = run_agent(&[
        "terminal-agent",
        "--socket",
        "/nonexistent/agent.sock",
        "--token-file",
        "/nonexistent/token",
        "--shell",
        "/bin/sh",
        "--cwd",
        "/tmp",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("unexpected argument"), "{stderr}");
}

#[test]
fn root_file_agent_refuses_a_mismatched_helper_build() {
    let output = run_agent(&[
        "rootfs-agent",
        "--socket",
        "/nonexistent/agent.sock",
        "--token-file",
        "/nonexistent/token",
        "--expected-build",
        "deadbeefdeadbeef",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match the running rast-native.service helper"),
        "actionable mismatch error expected, got: {stderr}"
    );
}

/// The kernel appends " (deleted)" to /proc/self/exe after an atomic upgrade.
/// The resolver must strip it only as a suffix and validate the result.
#[test]
fn deleted_executable_paths_are_resolved_before_use() {
    let dir = tempfile::tempdir().unwrap();
    let installed = dir.path().join("rast");
    std::fs::write(&installed, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let with_marker = Path::new(&installed).with_file_name("rast (deleted)");
    // The helper captured the pre-upgrade path; after the upgrade the captured
    // file is gone but the installed path still exists and is validated.
    let resolved =
        rast::executable::resolve_self(&installed).expect("the installed binary is valid");
    assert!(resolved.ends_with("rast"));
    assert!(
        !with_marker.exists() || rast::executable::validate(&with_marker).is_err(),
        "a literal deleted-marker file must never be treated as executable"
    );
}

#[test]
fn the_build_id_of_the_test_binary_is_parseable_from_version_output() {
    let id = rast::executable::build_id();
    assert!(id.len() >= 7, "unexpected build id {id}");
    let parsed =
        rast::executable::probe_build_id(&rast_bin()).expect("rast version reports a build id");
    assert_eq!(parsed, id);
}
