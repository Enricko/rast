//! Linux desktop apps: the part of the worker protocol shared by the web
//! server and the worker.
//!
//! An administrator uploads a Linux app (`.deb`, AppImage or a portable
//! `.tar.gz`). The worker turns it into a Docker image on top of a pinned
//! streaming desktop base (Selkies) and runs one container per app per person.
//! The web server proxies the app's page into a RAST window.
//!
//! Everything the worker does is derived from the structured, validated
//! [`LinuxArgs`] here: image, container and volume names, the build context and
//! every `docker run` flag. The server never sends a path, a Dockerfile, a
//! command line or a free-form Docker argument.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::protocol::{validate_slug, WorkerOp};

/// Repository of app images: `rast-linux/<app>:<build>`.
pub const IMAGE_REPOSITORY: &str = "rast-linux";
/// Label on every container, image and volume the worker creates for Linux
/// apps; its value says which kind of object it is.
pub const MANAGED_LABEL: &str = "rast.managed";
/// Label carrying the app id.
pub const APP_LABEL: &str = "rast.linux.app";
/// Label carrying the RAST user id of the person a container belongs to.
pub const USER_LABEL: &str = "rast.linux.user";
/// Port the app's page is served on inside the container.
pub const CONTAINER_PORT: u16 = 3000;
/// URL prefix the proxy serves apps under: `/linux-apps/<key>/`.
pub const URL_PREFIX: &str = "/linux-apps/";

/// Memory limits an administrator may choose, in MiB.
pub const MIN_MEMORY_MB: u32 = 512;
pub const MAX_MEMORY_MB: u32 = 65_536;
/// CPU limits an administrator may choose, in whole CPUs.
pub const MIN_CPUS: u32 = 1;
pub const MAX_CPUS: u32 = 64;
/// Largest package the worker accepts for a build.
pub const MAX_PACKAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// How the app was packaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinuxFormat {
    Deb,
    Appimage,
    Tarball,
}

impl LinuxFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deb => "deb",
            Self::Appimage => "appimage",
            Self::Tarball => "tarball",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deb" => Some(Self::Deb),
            "appimage" => Some(Self::Appimage),
            "tarball" => Some(Self::Tarball),
            _ => None,
        }
    }

    /// Format of an uploaded file, from its name.
    pub fn from_file_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".deb") {
            Some(Self::Deb)
        } else if lower.ends_with(".appimage") {
            Some(Self::Appimage)
        } else if [
            ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tar",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
        {
            Some(Self::Tarball)
        } else {
            None
        }
    }
}

/// Where a build takes the app from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinuxSource {
    /// The file the server staged at `<data_dir>/linux-apps/<app>/package`.
    File,
    /// The newest version of `package` from the apt sources the app's own
    /// package registered (the image `fromBuild` already has them).
    Apt,
}

/// Arguments of the `linux-*` operations. Which fields an operation accepts
/// is checked by [`validate`]; unknown fields fail deserialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LinuxArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<LinuxFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LinuxSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_build: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_data: Option<bool>,
}

/// Everything `linux-run` needs, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub app: String,
    pub build: u32,
    pub user_id: i64,
    /// Lower-cased RAST username; the home folder is `<homes>/<username>`.
    pub username: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub password: String,
    pub key: String,
    pub title: String,
}

/// A validated `linux-*` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxAction {
    Build {
        app: String,
        build: u32,
        format: LinuxFormat,
        source: BuildSource,
    },
    Run(RunSpec),
    Stop {
        app: String,
        user_id: i64,
    },
    /// Remove every container and image of the app, and its people's app
    /// data (volumes) when `remove_data` is set.
    Remove {
        app: String,
        remove_data: bool,
    },
    /// Remove the app's images other than `keep_build` (never forced: an
    /// image a running container uses stays).
    Prune {
        app: String,
        keep_build: u32,
    },
    List,
    Check {
        app: String,
        build: u32,
        package: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildSource {
    File,
    Apt { from_build: u32, package: String },
}

impl LinuxArgs {
    pub fn app(app: &str) -> Self {
        Self {
            app: Some(app.to_string()),
            ..Self::default()
        }
    }

    fn present(&self) -> [(&'static str, bool); 14] {
        [
            ("app", self.app.is_some()),
            ("build", self.build.is_some()),
            ("format", self.format.is_some()),
            ("source", self.source.is_some()),
            ("fromBuild", self.from_build.is_some()),
            ("package", self.package.is_some()),
            ("userId", self.user_id.is_some()),
            ("username", self.username.is_some()),
            ("memoryMb", self.memory_mb.is_some()),
            ("cpus", self.cpus.is_some()),
            ("password", self.password.is_some()),
            ("key", self.key.is_some()),
            ("title", self.title.is_some()),
            ("removeData", self.remove_data.is_some()),
        ]
    }

    fn accept_only(&self, op: WorkerOp, allowed: &[&str]) -> Result<(), String> {
        for (name, set) in self.present() {
            if set && !allowed.contains(&name) {
                return Err(format!("{} does not accept `{name}`", op.as_str()));
            }
        }
        Ok(())
    }

    fn required_app(&self, op: WorkerOp) -> Result<String, String> {
        let app = self
            .app
            .as_deref()
            .ok_or_else(|| format!("{} requires `app`", op.as_str()))?;
        validate_app_id(app)?;
        Ok(app.to_string())
    }

    fn required_build(&self, op: WorkerOp) -> Result<u32, String> {
        let build = self
            .build
            .ok_or_else(|| format!("{} requires `build`", op.as_str()))?;
        validate_build(build)?;
        Ok(build)
    }

    fn required_user(&self, op: WorkerOp) -> Result<i64, String> {
        let user_id = self
            .user_id
            .ok_or_else(|| format!("{} requires `userId`", op.as_str()))?;
        if user_id < 1 {
            return Err("invalid user id".into());
        }
        Ok(user_id)
    }
}

/// Validate the `linux` arguments of a `linux-*` operation.
pub fn validate(op: WorkerOp, args: Option<&LinuxArgs>) -> Result<LinuxAction, String> {
    let empty = LinuxArgs::default();
    let args = args.unwrap_or(&empty);
    match op {
        WorkerOp::LinuxList => {
            args.accept_only(op, &[])?;
            Ok(LinuxAction::List)
        }
        WorkerOp::LinuxBuild => {
            args.accept_only(
                op,
                &["app", "build", "format", "source", "fromBuild", "package"],
            )?;
            let app = args.required_app(op)?;
            let build = args.required_build(op)?;
            let format = args
                .format
                .ok_or_else(|| "linux-build requires `format`".to_string())?;
            let source = match args.source.unwrap_or(LinuxSource::File) {
                LinuxSource::File => {
                    if args.from_build.is_some() || args.package.is_some() {
                        return Err(
                            "a build from the uploaded file takes no `fromBuild`/`package`".into(),
                        );
                    }
                    BuildSource::File
                }
                LinuxSource::Apt => {
                    if format != LinuxFormat::Deb {
                        return Err("apt updates apply to .deb apps only".into());
                    }
                    let from_build = args
                        .from_build
                        .ok_or_else(|| "an apt update requires `fromBuild`".to_string())?;
                    validate_build(from_build)?;
                    if from_build >= build {
                        return Err("`build` must be newer than `fromBuild`".into());
                    }
                    let package = args
                        .package
                        .as_deref()
                        .ok_or_else(|| "an apt update requires `package`".to_string())?;
                    validate_package_name(package)?;
                    BuildSource::Apt {
                        from_build,
                        package: package.to_string(),
                    }
                }
            };
            Ok(LinuxAction::Build {
                app,
                build,
                format,
                source,
            })
        }
        WorkerOp::LinuxRun => {
            args.accept_only(
                op,
                &[
                    "app", "build", "userId", "username", "memoryMb", "cpus", "password", "key",
                    "title",
                ],
            )?;
            let app = args.required_app(op)?;
            let build = args.required_build(op)?;
            let user_id = args.required_user(op)?;
            let username = args
                .username
                .as_deref()
                .ok_or_else(|| "linux-run requires `username`".to_string())?
                .to_ascii_lowercase();
            validate_username(&username)?;
            let memory_mb = args
                .memory_mb
                .ok_or_else(|| "linux-run requires `memoryMb`".to_string())?;
            if !(MIN_MEMORY_MB..=MAX_MEMORY_MB).contains(&memory_mb) {
                return Err(format!(
                    "memory must be {MIN_MEMORY_MB}-{MAX_MEMORY_MB} MiB"
                ));
            }
            let cpus = args
                .cpus
                .ok_or_else(|| "linux-run requires `cpus`".to_string())?;
            if !(MIN_CPUS..=MAX_CPUS).contains(&cpus) {
                return Err(format!("CPUs must be {MIN_CPUS}-{MAX_CPUS}"));
            }
            let password = args
                .password
                .as_deref()
                .ok_or_else(|| "linux-run requires `password`".to_string())?;
            validate_hex_secret("password", password)?;
            let key = args
                .key
                .as_deref()
                .ok_or_else(|| "linux-run requires `key`".to_string())?;
            validate_hex_secret("key", key)?;
            let title = args
                .title
                .as_deref()
                .ok_or_else(|| "linux-run requires `title`".to_string())?;
            validate_title(title)?;
            Ok(LinuxAction::Run(RunSpec {
                app,
                build,
                user_id,
                username,
                memory_mb,
                cpus,
                password: password.to_string(),
                key: key.to_string(),
                title: title.to_string(),
            }))
        }
        WorkerOp::LinuxStop => {
            args.accept_only(op, &["app", "userId"])?;
            Ok(LinuxAction::Stop {
                app: args.required_app(op)?,
                user_id: args.required_user(op)?,
            })
        }
        WorkerOp::LinuxRemove => {
            args.accept_only(op, &["app", "removeData"])?;
            Ok(LinuxAction::Remove {
                app: args.required_app(op)?,
                remove_data: args.remove_data.unwrap_or(false),
            })
        }
        WorkerOp::LinuxPrune => {
            args.accept_only(op, &["app", "build"])?;
            Ok(LinuxAction::Prune {
                app: args.required_app(op)?,
                keep_build: args.required_build(op)?,
            })
        }
        WorkerOp::LinuxCheck => {
            args.accept_only(op, &["app", "build", "package"])?;
            let package = args
                .package
                .as_deref()
                .ok_or_else(|| "linux-check requires `package`".to_string())?;
            validate_package_name(package)?;
            Ok(LinuxAction::Check {
                app: args.required_app(op)?,
                build: args.required_build(op)?,
                package: package.to_string(),
            })
        }
        other => Err(format!("{} is not a Linux app operation", other.as_str())),
    }
}

/// App ids are generated by the server (`[a-z0-9-]`, like app slugs).
pub fn validate_app_id(app: &str) -> Result<(), String> {
    validate_slug(app).map_err(|e| e.replace("app slug", "Linux app id"))
}

fn validate_build(build: u32) -> Result<(), String> {
    if build == 0 || build > 1_000_000 {
        return Err("invalid build number".into());
    }
    Ok(())
}

/// Debian package names: `[a-z0-9][a-z0-9+.-]*` (Debian Policy 5.6.1).
pub fn validate_package_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 100 {
        return Err("invalid package name".into());
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err("invalid package name".into());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'.' | b'-'))
    {
        return Err("invalid package name".into());
    }
    Ok(())
}

/// RAST usernames (see `users::validate_username`), lower-cased.
pub fn validate_username(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 || !bytes[0].is_ascii_alphanumeric() {
        return Err("invalid username".into());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
        || name.ends_with('.')
    {
        return Err("invalid username".into());
    }
    Ok(())
}

/// Session secrets are lowercase hex generated by the server.
fn validate_hex_secret(what: &str, value: &str) -> Result<(), String> {
    if !(32..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("invalid {what}"));
    }
    Ok(())
}

fn validate_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control)
    {
        return Err("invalid app title".into());
    }
    Ok(())
}

/// `rast-linux/<app>:<build>`.
pub fn image_ref(app: &str, build: u32) -> String {
    format!("{IMAGE_REPOSITORY}/{app}:{build}")
}

/// Container (and app-data volume) of one person's copy of an app.
pub fn container_name(app: &str, user_id: i64) -> String {
    format!("rast-linux-{app}-u{user_id}")
}

/// Where the server stages an uploaded package for the worker.
pub fn package_path(linux_apps_dir: &Path, app: &str) -> PathBuf {
    linux_apps_dir.join(app).join("package")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args() -> LinuxArgs {
        LinuxArgs {
            app: Some("chrome".into()),
            build: Some(3),
            user_id: Some(7),
            username: Some("Alice".into()),
            memory_mb: Some(4096),
            cpus: Some(6),
            password: Some("a".repeat(32)),
            key: Some("b".repeat(32)),
            title: Some("Google Chrome".into()),
            ..LinuxArgs::default()
        }
    }

    #[test]
    fn run_arguments_are_validated_and_normalised() {
        let action = validate(WorkerOp::LinuxRun, Some(&run_args())).unwrap();
        let LinuxAction::Run(spec) = action else {
            panic!("expected a run");
        };
        assert_eq!(spec.username, "alice");
        assert_eq!(
            container_name(&spec.app, spec.user_id),
            "rast-linux-chrome-u7"
        );
        assert_eq!(image_ref(&spec.app, spec.build), "rast-linux/chrome:3");

        for (field, args) in [
            (
                "app",
                LinuxArgs {
                    app: Some("../x".into()),
                    ..run_args()
                },
            ),
            (
                "user",
                LinuxArgs {
                    username: Some("../root".into()),
                    ..run_args()
                },
            ),
            (
                "memory",
                LinuxArgs {
                    memory_mb: Some(64),
                    ..run_args()
                },
            ),
            (
                "cpus",
                LinuxArgs {
                    cpus: Some(0),
                    ..run_args()
                },
            ),
            (
                "password",
                LinuxArgs {
                    password: Some("x y".into()),
                    ..run_args()
                },
            ),
            (
                "key",
                LinuxArgs {
                    key: Some("ABCDEF".repeat(6)),
                    ..run_args()
                },
            ),
            (
                "title",
                LinuxArgs {
                    title: Some("a\nb".into()),
                    ..run_args()
                },
            ),
            (
                "extra",
                LinuxArgs {
                    package: Some("bash".into()),
                    ..run_args()
                },
            ),
        ] {
            assert!(
                validate(WorkerOp::LinuxRun, Some(&args)).is_err(),
                "{field} should be rejected"
            );
        }
    }

    #[test]
    fn builds_take_a_file_or_an_apt_upgrade() {
        let file = LinuxArgs {
            build: Some(1),
            format: Some(LinuxFormat::Deb),
            ..LinuxArgs::app("chrome")
        };
        assert!(matches!(
            validate(WorkerOp::LinuxBuild, Some(&file)),
            Ok(LinuxAction::Build {
                source: BuildSource::File,
                ..
            })
        ));
        let apt = LinuxArgs {
            build: Some(2),
            source: Some(LinuxSource::Apt),
            from_build: Some(1),
            package: Some("google-chrome-stable".into()),
            ..file.clone()
        };
        assert!(matches!(
            validate(WorkerOp::LinuxBuild, Some(&apt)),
            Ok(LinuxAction::Build {
                source: BuildSource::Apt { from_build: 1, .. },
                ..
            })
        ));
        for bad in [
            LinuxArgs {
                package: Some("bash; rm".into()),
                ..apt.clone()
            },
            LinuxArgs {
                from_build: Some(2),
                ..apt.clone()
            },
            LinuxArgs {
                format: Some(LinuxFormat::Appimage),
                ..apt.clone()
            },
            LinuxArgs {
                package: Some("bash".into()),
                ..file.clone()
            },
            LinuxArgs {
                format: None,
                ..file.clone()
            },
        ] {
            assert!(
                validate(WorkerOp::LinuxBuild, Some(&bad)).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn other_operations_accept_only_their_fields() {
        assert_eq!(validate(WorkerOp::LinuxList, None), Ok(LinuxAction::List));
        assert!(validate(WorkerOp::LinuxList, Some(&LinuxArgs::app("x"))).is_err());
        assert!(validate(WorkerOp::LinuxStop, Some(&LinuxArgs::app("x"))).is_err());
        let stop = LinuxArgs {
            user_id: Some(1),
            ..LinuxArgs::app("x")
        };
        assert!(validate(WorkerOp::LinuxStop, Some(&stop)).is_ok());
        assert!(validate(WorkerOp::Version, Some(&stop)).is_err());
    }

    #[test]
    fn formats_follow_the_file_name() {
        assert_eq!(
            LinuxFormat::from_file_name("google-chrome-stable_current_amd64.deb"),
            Some(LinuxFormat::Deb)
        );
        assert_eq!(
            LinuxFormat::from_file_name("Obsidian-1.9.AppImage"),
            Some(LinuxFormat::Appimage)
        );
        assert_eq!(
            LinuxFormat::from_file_name("firefox-140.0.tar.xz"),
            Some(LinuxFormat::Tarball)
        );
        assert_eq!(LinuxFormat::from_file_name("setup.exe"), None);
    }
}
