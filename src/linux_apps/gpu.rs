//! GPU / display capability detection for native Linux sessions.
//!
//! Everything here is a **read-only probe**: no X/Wayland server is started, no
//! package or configuration is changed, and nothing runs on behalf of a user.
//! The report is exposed through `GET /api/linux-apps`, printed by
//! `rast doctor` and used to choose the Xvfb/custom display command that the
//! per-user Xpra session starts with.
//!
//! Four layers are deliberately kept apart (see `docs/GPU-ACCELERATION.md`):
//! GPU rendering inside the application, GPU compositing inside the display
//! server, GPU encoding on the server, and GPU decoding in the browser. This
//! module only *detects* the host pieces; it never claims acceleration that was
//! not verified by a real test.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::Serialize;

use crate::config::{Config, LinuxDisplayBackend};

/// Where the default software X server lives when `PATH` does not resolve it.
const FALLBACK_BIN_DIRS: &[&str] = &["/usr/bin", "/usr/local/bin", "/bin"];

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    FALLBACK_BIN_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| is_executable(candidate))
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn library_present(prefix: &str) -> bool {
    const LIB_DIRS: &[&str] = &[
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
    ];
    LIB_DIRS.iter().any(|dir| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix))
            })
        })
    })
}

/// `xpra-codecs-nvidia` installs an `xpra/codecs/nvidia` python package.
fn xpra_nvidia_codecs_present() -> bool {
    let Ok(entries) = std::fs::read_dir("/usr/lib") else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("python3"))
        })
        .any(|entry| {
            entry
                .path()
                .join("dist-packages/xpra/codecs/nvidia")
                .is_dir()
        })
}

fn nvidia_version() -> Option<String> {
    let text = read_text(Path::new("/proc/driver/nvidia/version"))?;
    let first = text.lines().next()?;
    first
        .split_whitespace()
        .find(|token| {
            token.len() >= 3
                && token.contains('.')
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
        .map(str::to_string)
}

fn nvidia_model() -> Option<String> {
    let entries = std::fs::read_dir("/proc/driver/nvidia/gpus").ok()?;
    for entry in entries.filter_map(Result::ok) {
        let text = read_text(&entry.path().join("information"))?;
        for line in text.lines() {
            if let Some(model) = line.strip_prefix("Model:") {
                return Some(model.trim().to_string());
            }
        }
    }
    None
}

fn render_nodes() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| name.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayCapability {
    /// Xvfb is installed and usable (the software fallback).
    pub xvfb: bool,
    pub xvfb_path: Option<String>,
    /// `linux_xvfb_command` was configured by the operator.
    pub custom_configured: bool,
    /// That command exists and is executable, so sessions can actually use it.
    pub custom_usable: bool,
    /// Backend sessions use right now: `xvfb` or `custom`.
    pub active: String,
    /// The custom command path, when configured.
    pub custom_command: Option<String>,
    /// Whether the NVIDIA Xorg driver module is installed (needed by a
    /// GPU-backed custom Xorg wrapper).
    pub nvidia_xorg_driver: bool,
    /// Whether a plain Xorg binary exists (alternative display backend work).
    pub xorg: bool,
    /// GPU-backed display is *configured*; it is not a verified acceleration
    /// claim (see `gpu_acceleration_verified`).
    pub gpu_display_configured: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCapability {
    /// `nvidia`, `intel`, `amd` or `none` (first detected).
    pub vendor: String,
    pub model: Option<String>,
    pub driver_version: Option<String>,
    pub nvidia_device_nodes: bool,
    pub nvidia_uvm: bool,
    pub nvidia_smi: bool,
    /// `libnvidia-encode.so*` is installed (NVENC runtime dependency).
    pub nvenc_library: bool,
    /// `libnvcuvid.so*` is installed (NVDEC runtime dependency).
    pub nvdec_library: bool,
    /// Xpra's `xpra-codecs-nvidia` module is installed (enables nvenc/nvdec).
    pub xpra_nvidia_codecs: bool,
    /// Mesa EGL/GLVND is present (software or Intel iGPU EGL paths).
    pub mesa_egl: bool,
    pub render_nodes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeReport {
    pub profile: String,
    pub quality: Option<u8>,
    pub min_quality: Option<u8>,
    pub speed: Option<u8>,
    pub min_speed: Option<u8>,
    pub video_scaling: Option<u8>,
    pub av_sync: Option<bool>,
    pub encoding: Option<String>,
    pub display: DisplayCapability,
    pub gpu: GpuCapability,
    /// `true` only when a real application was verified to render on the GPU.
    /// RAST cannot prove that from filesystem probes, so this is `false` until
    /// an operator validates it (see `docs/GPU-ACCELERATION.md`).
    pub gpu_acceleration_verified: bool,
    pub notes: Vec<String>,
}

/// The custom headless-X wrapper, if configured **and** executable.
pub fn custom_command(config: &Config) -> Option<&Path> {
    config
        .linux_xvfb_command
        .as_deref()
        .filter(|path| is_executable(path))
}

/// `--xvfb=` argument for `xpra start`: the configured GPU/custom wrapper when
/// usable, otherwise the bundled Xvfb. Never fails: a missing custom command
/// falls back to software rendering so a session still starts.
pub fn display_backend_arg(config: &Config) -> String {
    match config.linux_display_backend {
        LinuxDisplayBackend::Software => "--xvfb=Xvfb".to_string(),
        LinuxDisplayBackend::Auto | LinuxDisplayBackend::Custom => match custom_command(config) {
            Some(path) => format!("--xvfb={}", path.display()),
            None => "--xvfb=Xvfb".to_string(),
        },
    }
}

/// Backend that sessions actually use with the current configuration.
pub fn resolved_display_backend(config: &Config) -> &'static str {
    if config.linux_display_backend == LinuxDisplayBackend::Software {
        return "xvfb";
    }
    if custom_command(config).is_some() {
        "custom"
    } else {
        "xvfb"
    }
}

fn detect_display(config: &Config) -> DisplayCapability {
    let xvfb_path = find_in_path("Xvfb");
    let custom_command_path = config.linux_xvfb_command.clone();
    let custom_usable = config
        .linux_xvfb_command
        .as_deref()
        .is_some_and(is_executable);
    let active = resolved_display_backend(config);
    let nvidia_xorg_driver = Path::new("/usr/lib/xorg/modules/drivers/nvidia_drv.so").exists();
    let xorg = find_in_path("Xorg").is_some();
    let gpu_display_configured = active == "custom"
        && (nvidia_xorg_driver
            || custom_command_path.as_ref().is_some_and(|path| {
                let name = path.to_string_lossy().to_ascii_lowercase();
                name.contains("nvidia") || name.contains("xorg") || name.contains("gpu")
            }));
    DisplayCapability {
        xvfb: xvfb_path.is_some(),
        xvfb_path: xvfb_path.map(|path| path.display().to_string()),
        custom_configured: config.linux_xvfb_command.is_some(),
        custom_usable,
        active: active.to_string(),
        custom_command: custom_command_path.map(|path| path.display().to_string()),
        nvidia_xorg_driver,
        xorg,
        gpu_display_configured,
    }
}

fn detect_gpu() -> GpuCapability {
    let nvidia_version = nvidia_version();
    let nvidia_model = nvidia_model();
    let nvidia_present = nvidia_version.is_some() || Path::new("/dev/nvidia0").exists();
    let render_nodes = render_nodes();
    let vendor = if nvidia_present {
        "nvidia"
    } else if Path::new("/dev/dri/renderD128").exists() {
        // A render node without NVIDIA is normally Intel or AMD; report it as
        // `generic` rather than guessing the vendor.
        "generic"
    } else {
        "none"
    };
    GpuCapability {
        vendor: vendor.to_string(),
        model: nvidia_model,
        driver_version: nvidia_version,
        nvidia_device_nodes: Path::new("/dev/nvidia0").exists(),
        nvidia_uvm: Path::new("/dev/nvidia-uvm").exists(),
        nvidia_smi: find_in_path("nvidia-smi").is_some(),
        nvenc_library: library_present("libnvidia-encode.so"),
        nvdec_library: library_present("libnvcuvid.so"),
        xpra_nvidia_codecs: xpra_nvidia_codecs_present(),
        mesa_egl: Path::new("/usr/lib/x86_64-linux-gnu/libEGL_mesa.so.0").exists()
            || Path::new("/usr/share/glvnd/egl_vendor.d/50_mesa.json").exists(),
        render_nodes,
    }
}

/// Full report used by the API and `rast doctor`. Cached per process after the
/// first call: package installation while the server runs needs a restart,
/// which matches how session arguments and the native helper already behave.
pub fn report(config: &Config) -> RuntimeReport {
    static CACHED: OnceLock<RuntimeReport> = OnceLock::new();
    CACHED.get_or_init(|| build_report(config)).clone()
}

fn build_report(config: &Config) -> RuntimeReport {
    let display = detect_display(config);
    let gpu = detect_gpu();
    let mut notes = Vec::new();

    if gpu.vendor == "nvidia" && !gpu.nvidia_device_nodes {
        notes.push(
            "An NVIDIA driver is loaded but /dev/nvidia0 is missing; applications cannot use the GPU."
                .to_string(),
        );
    }
    if gpu.vendor == "nvidia" && !gpu.xpra_nvidia_codecs {
        notes.push(
            "NVIDIA GPU present, but Xpra's NVIDIA codec module is not installed: sessions encode \
             with software codecs (x264/webp/jpeg). Install the xpra-codecs-nvidia package that \
             matches the installed Xpra version to enable NVENC/NVDEC."
                .to_string(),
        );
    }
    if display.active == "xvfb" {
        notes.push(
            "Display backend is Xvfb: applications render with software OpenGL (llvmpipe) unless \
             a validated custom GPU-backed X server is configured (linux_display_backend / \
             linux_xvfb_command)."
                .to_string(),
        );
    }
    if display.custom_configured && !display.custom_usable {
        notes.push(
            "linux_xvfb_command is configured but is not an executable file; sessions fall back \
             to Xvfb."
                .to_string(),
        );
    }
    if !display.custom_configured && display.nvidia_xorg_driver {
        notes.push(
            "The NVIDIA Xorg driver is installed but no custom GPU display backend is configured; \
             see docs/GPU-ACCELERATION.md to validate one before enabling it."
                .to_string(),
        );
    }
    if display.gpu_display_configured {
        notes.push(
            "A GPU-backed custom display backend is configured. Acceleration inside applications \
             is not assumed: verify it with a real application before trusting it."
                .to_string(),
        );
    }

    RuntimeReport {
        profile: config.linux_profile.as_str().to_string(),
        quality: config.linux_quality,
        min_quality: config.linux_min_quality,
        speed: config.linux_speed,
        min_speed: config.linux_min_speed,
        video_scaling: config.linux_video_scaling,
        av_sync: config.linux_av_sync,
        encoding: config.linux_encoding.clone(),
        display,
        gpu,
        gpu_acceleration_verified: false,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn config(backend: Option<&str>, command: Option<PathBuf>) -> Config {
        Config::from_parts(
            None,
            FileConfig {
                linux_display_backend: backend.map(str::to_string),
                linux_xvfb_command: command,
                ..Default::default()
            },
            no_env,
        )
        .unwrap()
    }

    fn executable_script(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("xorg-nvidia.sh");
        std::fs::write(&path, "#!/bin/sh\nexec Xorg \"$@\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn default_backend_is_the_bundled_xvfb() {
        let cfg = config(None, None);
        assert_eq!(display_backend_arg(&cfg), "--xvfb=Xvfb");
        assert_eq!(resolved_display_backend(&cfg), "xvfb");
    }

    #[test]
    fn a_usable_custom_command_replaces_xvfb() {
        let dir = tempfile::tempdir().unwrap();
        let script = executable_script(dir.path());
        let cfg = config(None, Some(script.clone()));
        assert_eq!(
            display_backend_arg(&cfg),
            format!("--xvfb={}", script.display())
        );
        assert_eq!(resolved_display_backend(&cfg), "custom");
    }

    #[test]
    fn a_missing_or_non_executable_custom_command_falls_back_to_xvfb() {
        let dir = tempfile::tempdir().unwrap();
        let not_executable = dir.path().join("not-executable.sh");
        std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
        let cfg = config(Some("custom"), Some(not_executable));
        assert_eq!(display_backend_arg(&cfg), "--xvfb=Xvfb");
        assert_eq!(resolved_display_backend(&cfg), "xvfb");
        let cfg = config(Some("custom"), Some(dir.path().join("missing.sh")));
        assert_eq!(display_backend_arg(&cfg), "--xvfb=Xvfb");
        assert_eq!(resolved_display_backend(&cfg), "xvfb");
    }

    #[test]
    fn software_backend_ignores_a_configured_custom_command() {
        let dir = tempfile::tempdir().unwrap();
        let script = executable_script(dir.path());
        let cfg = config(Some("software"), Some(script));
        assert_eq!(display_backend_arg(&cfg), "--xvfb=Xvfb");
        assert_eq!(resolved_display_backend(&cfg), "xvfb");
    }

    #[test]
    fn report_is_serializable_and_never_claims_verified_acceleration() {
        let cfg = config(None, None);
        let report = build_report(&cfg);
        assert_eq!(report.profile, "balanced");
        assert!(!report.gpu_acceleration_verified);
        let json = serde_json::to_value(&report).unwrap();
        assert!(json["display"]["active"].is_string());
        assert!(json["gpu"]["vendor"].is_string());
    }
}
