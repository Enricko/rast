//! Tests for the release helper scripts used by `.github/workflows/release.yml`:
//! `scripts/release-check.sh` (version, pre-release flag, "final release is on
//! main") and the pre-release fallback of `scripts/changelog-section.sh`.
//!
//! They need `bash` and `git` on `PATH` and work in temporary git
//! repositories only (no network, no root).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Release Test",
            "-c",
            "user.email=release@example.test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with `main` (one commit) and `dev` (one commit on top of main).
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    git(p, &["commit", "-q", "--allow-empty", "-m", "main commit"]);
    git(p, &["checkout", "-q", "-b", "dev"]);
    git(p, &["commit", "-q", "--allow-empty", "-m", "dev commit"]);
    dir
}

fn release_check(dir: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script("release-check.sh"))
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run release-check.sh")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn suffixed_version_is_always_a_prerelease_even_off_main() {
    let dir = repo();
    for input in ["false", "true"] {
        let out = release_check(
            dir.path(),
            &[
                "--prerelease",
                input,
                "--commit",
                "dev",
                "--main-ref",
                "main",
                "v1.0.1-alpha.2",
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(
            stdout(&out),
            "version=1.0.1-alpha.2\ntag=v1.0.1-alpha.2\nprerelease=true\n"
        );
    }
}

#[test]
fn suffixed_version_from_a_tag_push_needs_no_main_ref() {
    // A pushed tag passes --prerelease false; the suffix still wins, and the
    // main-branch check is skipped (the ref need not even exist).
    let dir = repo();
    let out = release_check(dir.path(), &["--main-ref", "does-not-exist", "1.0.1-rc.1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("prerelease=true\n"));
}

#[test]
fn final_version_on_main_is_a_normal_release() {
    let dir = repo();
    let out = release_check(
        dir.path(),
        &["--commit", "main", "--main-ref", "main", "1.0.1"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "version=1.0.1\ntag=v1.0.1\nprerelease=false\n"
    );
}

#[test]
fn final_version_keeps_the_ticked_prerelease_box_on_main() {
    let dir = repo();
    let out = release_check(
        dir.path(),
        &[
            "--prerelease",
            "true",
            "--commit",
            "main",
            "--main-ref",
            "main",
            "1.0.1",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("prerelease=true\n"));
}

#[test]
fn final_version_off_main_is_refused_even_as_prerelease() {
    let dir = repo();
    for input in ["false", "true"] {
        let out = release_check(
            dir.path(),
            &[
                "--prerelease",
                input,
                "--commit",
                "dev",
                "--main-ref",
                "main",
                "1.0.1",
            ],
        );
        assert_eq!(out.status.code(), Some(1), "input {input}");
        assert_eq!(stdout(&out), "", "no output may reach GITHUB_OUTPUT");
        assert!(stderr(&out).contains("is not on main"), "{}", stderr(&out));
    }
}

#[test]
fn build_metadata_alone_is_not_a_prerelease() {
    let dir = repo();
    let out = release_check(
        dir.path(),
        &["--commit", "dev", "--main-ref", "main", "1.0.1+build.5"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("such as 1.0.1-alpha.1"));
}

#[test]
fn final_version_with_a_missing_main_ref_is_refused() {
    let dir = repo();
    let out = release_check(dir.path(), &["--main-ref", "origin/main", "1.0.1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cannot resolve the main branch"));
}

#[test]
fn invalid_versions_and_usage_are_rejected() {
    let dir = repo();
    for bad in ["1.0", "1.0.1-", "1.0.1 alpha", "x1.0.1", "1.0.1-alpha_1"] {
        let out = release_check(dir.path(), &[bad]);
        assert_eq!(out.status.code(), Some(1), "{bad:?} must be rejected");
        assert_eq!(stdout(&out), "");
    }
    assert_eq!(release_check(dir.path(), &[]).status.code(), Some(2));
    assert_eq!(
        release_check(dir.path(), &["--prerelease", "yes", "1.0.1-alpha.1"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        release_check(dir.path(), &["--bogus", "1.0.1-alpha.1"])
            .status
            .code(),
        Some(2)
    );
    let help = release_check(dir.path(), &["--help"]);
    assert!(help.status.success());
    assert!(stdout(&help).starts_with("Resolve and check a release version"));
}

const CHANGELOG: &str = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
## [1.0.2-beta.1] - 2026-12-01\n\
\n\
Own beta notes.\n\
\n\
## [1.0.2] - Unreleased\n\
\n\
Notes for 1.0.2.\n\
\n\
## [1.0.1] - Unreleased\n\
\n\
### Added\n\
\n\
- Notes for 1.0.1.\n\
\n\
## [0.18.3] - 2026-11-16\n\
\n\
- Old notes.\n";

fn changelog_section(version: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("CHANGELOG.md");
    std::fs::write(&file, CHANGELOG).unwrap();
    Command::new("bash")
        .arg(script("changelog-section.sh"))
        .arg(version)
        .arg(&file)
        .output()
        .expect("run changelog-section.sh")
}

#[test]
fn changelog_prerelease_uses_the_base_section() {
    let out = changelog_section("1.0.1-alpha.3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "### Added\n\n- Notes for 1.0.1.\n");
    let out = changelog_section("v1.0.1-alpha.3");
    assert_eq!(stdout(&out), "### Added\n\n- Notes for 1.0.1.\n");
}

#[test]
fn changelog_prerelease_prefers_its_own_section() {
    let out = changelog_section("1.0.2-beta.1");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "Own beta notes.\n");
    let out = changelog_section("1.0.2-beta.2");
    assert_eq!(stdout(&out), "Notes for 1.0.2.\n");
}

#[test]
fn changelog_exact_and_missing_versions_are_unchanged() {
    assert_eq!(stdout(&changelog_section("0.18.3")), "- Old notes.\n");
    assert_eq!(
        stdout(&changelog_section("1.0.1")),
        "### Added\n\n- Notes for 1.0.1.\n"
    );
    // No fallback for a final version, nor for a pre-release whose base
    // section does not exist.
    assert_eq!(changelog_section("1.0.3").status.code(), Some(1));
    assert_eq!(changelog_section("1.0.3-alpha.1").status.code(), Some(1));
    // Build metadata is not a pre-release suffix.
    assert_eq!(changelog_section("1.0.1+build.1").status.code(), Some(1));
}
