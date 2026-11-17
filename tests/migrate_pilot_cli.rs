//! `rast migrate shares` and `rast migrate home` with the native pilot
//! (ADR 0007): apply and rollback of a `--share` or `--user` outside the
//! pilot are refused by the binary before the server lock, the database or
//! the helper is touched; `--dry-run` is not refused.
//!
//! The binary runs with a temporary configuration file (`RAST_CONFIG`) and
//! an empty environment, and the helper socket does not exist.
//!
//! Run with:
//!     cargo test --test migrate_pilot_cli
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output};

fn rast(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rast"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("RAST_CONFIG", dir.join("rast.toml"))
        .output()
        .expect("run rast")
}

fn configure(dir: &Path) {
    let path = |name: &str| dir.join(name).display().to_string();
    std::fs::write(
        dir.join("rast.toml"),
        format!(
            "data_dir = {:?}\nstorage_dir = {:?}\nnative_enabled = true\n\
             native_helper_socket = {:?}\nnative_pilot_prefixes = [\"t-\"]\n",
            path("data"),
            path("storage"),
            path("absent.sock"),
        ),
    )
    .unwrap();
}

#[test]
fn apply_and_rollback_outside_the_pilot_are_refused_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    configure(dir.path());
    for args in [
        &["migrate", "shares", "--share", "Team"][..],
        &["migrate", "shares", "--share", "Team", "--rollback"],
        &["migrate", "home", "--user", "admin"],
        &["migrate", "home", "--user", "Admin", "--rollback"],
    ] {
        let out = rast(dir.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {stderr}");
        assert!(
            stderr.contains("outside the native pilot"),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains("--dry-run still reports it"),
            "{args:?}: {stderr}"
        );
    }
    // Refused before the database was opened.
    assert!(!dir.path().join("data").exists());
}

#[test]
fn a_dry_run_outside_the_pilot_is_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    configure(dir.path());
    let out = rast(dir.path(), &["migrate", "shares", "--all", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("There are no shared folders."), "{stdout}");
    // `--share` with a dry run gets past the pilot: the name is looked up.
    let out = rast(
        dir.path(),
        &["migrate", "shares", "--share", "Team", "--dry-run"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(!stderr.contains("native pilot"), "{stderr}");
    assert!(
        stderr.contains("no shared folder has that name"),
        "{stderr}"
    );
}
