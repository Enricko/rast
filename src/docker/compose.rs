//! Docker Compose validation and container-sandbox policy.
//!
//! Two layers are used:
//!
//! 1. [`check_compose_text`] is a cheap structural/syntactic pre-check on the
//!    raw text. It rejects documents that are not a YAML mapping with a
//!    top-level `services` key and refuses features that would make Compose
//!    read files or expand host paths before we can inspect the result
//!    (`env_file`, `include`, `extends`, ...). It also stops the obvious
//!    `docker.sock` / `privileged` payloads early.
//! 2. [`validate_normalized_config`] inspects the JSON produced by
//!    `docker compose config --format json`. That document is what Compose
//!    would actually deploy, so bind-mount ownership, volume drivers, devices,
//!    host networking, capabilities and config/secret files can be checked
//!    exactly rather than by guessing at YAML.
//!
//! The policy: a managed app may only bind-mount host paths inside its own
//! `data/` directory. Everything else (host namespaces, Docker socket, device
//! access, privileged mode, external/host-path volumes) is rejected with a
//! human-readable reason.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use serde_json::Value;

/// Maximum accepted Compose file size. Keeps parsing and validation bounded.
pub const MAX_COMPOSE_BYTES: usize = 512 * 1024;

/// Maximum number of services in one application.
pub const MAX_SERVICES: usize = 100;

/// Service/volume/config names: conservative Compose-compatible alphabet.
fn valid_object_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or(' ');
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Keys that would make Compose read arbitrary files or expand templates
/// before the normalized config can be validated.
const FORBIDDEN_RAW_KEYS: &[&str] = &[
    "env_file",
    "include",
    "extends",
    "volumes_from",
    "privileged",
];

/// Capabilities that grant effective host-level control and are refused.
const DANGEROUS_CAPS: &[&str] = &[
    "ALL",
    "SYS_ADMIN",
    "SYS_PTRACE",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_BOOT",
    "SYS_TIME",
    "DAC_READ_SEARCH",
    "DAC_OVERRIDE",
];

/// Cheap structural check before the text is written to disk / handed to
/// Compose. `slug` is only used in error messages.
pub fn check_compose_text(text: &str, slug: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("compose file is empty".into());
    }
    if text.len() > MAX_COMPOSE_BYTES {
        return Err(format!(
            "compose file is too large ({} bytes, max {MAX_COMPOSE_BYTES})",
            text.len()
        ));
    }
    if text.contains('\0') {
        return Err("compose file contains NUL bytes".into());
    }

    let mut has_top_level_services = false;
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let unindented = !raw_line.starts_with(' ') && !raw_line.starts_with('\t');
        if unindented && (trimmed == "services:" || trimmed.starts_with("services: ")) {
            has_top_level_services = true;
        }
        // Key checks: `key:` or `- key:` style mappings.
        let key_text = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        if let Some((key, _)) = key_text.split_once(':') {
            let key = key.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
            if FORBIDDEN_RAW_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "compose key '{key}' is not allowed for managed applications"
                ));
            }
        }
        // Catch flow-style / quoted occurrences of file-reading keys.
        let compact: String = trimmed
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .collect::<String>()
            .to_ascii_lowercase();
        for token in ["env_file", "include:", "extends:", "volumes_from"] {
            if compact.contains(token) {
                return Err(format!(
                    "compose feature '{token}' is not allowed for managed applications"
                ));
            }
        }
        if compact.contains("docker.sock") || compact.contains("secret.key") {
            return Err("compose files may not reference host sockets or secrets".into());
        }
    }

    if !has_top_level_services {
        return Err(format!(
            "compose file for '{slug}' must be a YAML mapping with a top-level 'services' key"
        ));
    }
    Ok(())
}

/// Result of validating a normalized Compose configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeSummary {
    pub services: Vec<String>,
    pub bind_mounts: usize,
}

impl ComposeSummary {
    pub fn container_count(&self) -> usize {
        self.services.len()
    }
}

/// Validate the normalized config (`docker compose config --format json`) for
/// one managed application.
///
/// `app_dir` is `<data_dir>/apps/<slug>`; only `<app_dir>/data` may be used as
/// the source of bind mounts, and only `<app_dir>` may contain config/secret
/// files.
pub fn validate_normalized_config(
    config: &Value,
    app_dir: &Path,
) -> Result<ComposeSummary, String> {
    let app_dir = normalize_lexical(app_dir)
        .ok_or_else(|| "internal error: invalid application directory".to_string())?;
    let data_root = app_dir.join("data");

    let root = config
        .as_object()
        .ok_or_else(|| "compose file must be a YAML mapping".to_string())?;

    let services = root
        .get("services")
        .and_then(Value::as_object)
        .ok_or_else(|| "compose file must define a 'services' mapping".to_string())?;
    if services.is_empty() {
        return Err("compose file defines no services".into());
    }
    if services.len() > MAX_SERVICES {
        return Err(format!(
            "compose file defines too many services ({} > {MAX_SERVICES})",
            services.len()
        ));
    }

    // Top-level named volumes: no external volumes and no driver options that
    // could remap a volume onto a host path.
    let mut volume_names = BTreeSet::new();
    if let Some(volumes) = root.get("volumes") {
        let volumes = volumes
            .as_object()
            .ok_or_else(|| "'volumes' must be a mapping".to_string())?;
        for (name, spec) in volumes {
            if !valid_object_name(name) {
                return Err(format!("invalid volume name '{name}'"));
            }
            let spec = spec
                .as_object()
                .ok_or_else(|| format!("volume '{name}' must be a mapping"))?;
            if spec
                .get("external")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(format!(
                    "external volume '{name}' is not allowed; managed apps must own their volumes"
                ));
            }
            if let Some(driver_opts) = spec.get("driver_opts").and_then(Value::as_object) {
                if !driver_opts.is_empty() {
                    return Err(format!(
                        "volume '{name}' uses driver_opts, which can bind host paths; not allowed"
                    ));
                }
            }
            if let Some(volume_name) = spec.get("name").and_then(Value::as_str) {
                if volume_name.contains('/') || volume_name.contains('$') {
                    return Err(format!("volume '{name}' has an invalid explicit name"));
                }
            }
            volume_names.insert(name.clone());
        }
    }

    // Top-level configs/secrets may only reference files inside the app dir.
    for kind in ["configs", "secrets"] {
        let Some(entries) = root.get(kind) else {
            continue;
        };
        let entries = entries
            .as_object()
            .ok_or_else(|| format!("'{kind}' must be a mapping"))?;
        for (name, spec) in entries {
            let spec = spec
                .as_object()
                .ok_or_else(|| format!("{kind} '{name}' must be a mapping"))?;
            if spec
                .get("external")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(format!("external {kind} '{name}' is not allowed"));
            }
            if spec.get("environment").is_some() {
                return Err(format!(
                    "{kind} '{name}' sources a worker environment variable; not allowed"
                ));
            }
            if let Some(file) = spec.get("file").and_then(Value::as_str) {
                if !path_is_within(&app_dir, file) {
                    return Err(format!(
                        "{kind} '{name}' references a file outside the application directory"
                    ));
                }
            }
        }
    }

    // Networks must be project-local: external networks would attach managed
    // containers to host infrastructure.
    if let Some(networks) = root.get("networks") {
        let networks = networks
            .as_object()
            .ok_or_else(|| "'networks' must be a mapping".to_string())?;
        for (name, spec) in networks {
            if let Some(spec) = spec.as_object() {
                if spec
                    .get("external")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return Err(format!("external network '{name}' is not allowed"));
                }
            }
        }
    }

    let mut summary = ComposeSummary {
        services: Vec::new(),
        bind_mounts: 0,
    };

    for (service_name, service) in services {
        if !valid_object_name(service_name) {
            return Err(format!("invalid service name '{service_name}'"));
        }
        let service = service
            .as_object()
            .ok_or_else(|| format!("service '{service_name}' must be a mapping"))?;

        // Every service must name a prebuilt image; local builds are not
        // supported for managed apps (no source upload / build context).
        match service.get("image").and_then(Value::as_str) {
            Some(image) if !image.trim().is_empty() => {
                if image.chars().any(char::is_whitespace) {
                    return Err(format!(
                        "service '{service_name}' has an invalid image reference"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "service '{service_name}' must specify an 'image' (builds are not supported)"
                ))
            }
        }
        if service.get("build").is_some() {
            return Err(format!(
                "service '{service_name}' uses 'build', which is not supported for managed apps"
            ));
        }
        if service
            .get("privileged")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(format!("service '{service_name}' requests privileged mode"));
        }
        if service.get("pid").is_some() {
            return Err(format!(
                "service '{service_name}' sets a host PID namespace"
            ));
        }
        if service.get("userns_mode").is_some() {
            return Err(format!("service '{service_name}' sets userns_mode"));
        }
        if service.get("cgroup").is_some() {
            return Err(format!(
                "service '{service_name}' sets a host cgroup namespace"
            ));
        }
        if let Some(ipc) = service.get("ipc").and_then(Value::as_str) {
            if ipc != "private" && ipc != "none" && ipc != "shareable" {
                return Err(format!(
                    "service '{service_name}' uses an unsupported IPC namespace '{ipc}'"
                ));
            }
        }
        if let Some(network_mode) = service.get("network_mode").and_then(Value::as_str) {
            if !matches!(network_mode, "bridge" | "none") {
                return Err(format!(
                    "service '{service_name}' uses network_mode '{network_mode}'; host/container networking is not allowed"
                ));
            }
        }
        if service.get("volumes_from").is_some() {
            return Err(format!(
                "service '{service_name}' uses volumes_from, which is not allowed"
            ));
        }
        if let Some(devices) = service.get("devices").and_then(Value::as_array) {
            if !devices.is_empty() {
                return Err(format!(
                    "service '{service_name}' requests device access, which is not allowed"
                ));
            }
        }
        if let Some(caps) = service.get("cap_add").and_then(Value::as_array) {
            for cap in caps.iter().filter_map(Value::as_str) {
                let upper = cap.trim().to_ascii_uppercase();
                let normalized = upper.strip_prefix("CAP_").unwrap_or(&upper);
                if DANGEROUS_CAPS.contains(&normalized) {
                    return Err(format!(
                        "service '{service_name}' requests the dangerous capability '{cap}'"
                    ));
                }
            }
        }
        if let Some(opts) = service.get("security_opt").and_then(Value::as_array) {
            for opt in opts.iter().filter_map(Value::as_str) {
                if opt.to_ascii_lowercase().contains("unconfined") {
                    return Err(format!(
                        "service '{service_name}' disables a security profile ({opt})"
                    ));
                }
            }
        }

        // Bind mounts and named volumes.
        if let Some(volumes) = service.get("volumes") {
            let volumes = volumes
                .as_array()
                .ok_or_else(|| format!("service '{service_name}' volumes must be a list"))?;
            for volume in volumes {
                let spec = volume
                    .as_object()
                    .ok_or_else(|| format!("service '{service_name}' has an invalid volume"))?;
                let source = spec.get("source").and_then(Value::as_str).unwrap_or("");
                let target = spec.get("target").and_then(Value::as_str).unwrap_or("");
                if source.contains('$') || target.contains('$') {
                    return Err(format!(
                        "service '{service_name}' uses variable interpolation in a mount path"
                    ));
                }
                if !target.starts_with('/') || target.contains("..") {
                    return Err(format!(
                        "service '{service_name}' has an invalid mount target '{target}'"
                    ));
                }
                match spec.get("type").and_then(Value::as_str).unwrap_or("") {
                    "bind" => {
                        if !path_is_within(&data_root, source) {
                            return Err(format!(
                                "service '{service_name}' bind-mounts '{source}', which is outside the app data directory"
                            ));
                        }
                        summary.bind_mounts += 1;
                    }
                    "volume" => {
                        if source.is_empty() || source.contains('/') || source.contains("..") {
                            return Err(format!(
                                "service '{service_name}' mounts an invalid named volume '{source}'"
                            ));
                        }
                        if !volume_names.contains(source) {
                            return Err(format!(
                                "service '{service_name}' references undeclared volume '{source}'"
                            ));
                        }
                    }
                    "tmpfs" => {}
                    other => {
                        return Err(format!(
                            "service '{service_name}' uses unsupported mount type '{other}'"
                        ))
                    }
                }
            }
        }

        summary.services.push(service_name.clone());
    }

    Ok(summary)
}

/// Lexically normalize a path (resolve `.` and `..` without touching the
/// filesystem). Returns `None` when the path escapes its root via `..`.
pub fn normalize_lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// True when `candidate` (absolute or relative) resolves lexically inside
/// `root`. Relative candidates are interpreted against the filesystem root,
/// which means they never match an absolute root — callers reject them, which
/// is exactly what we want for bind mounts and config files.
fn path_is_within(root: &Path, candidate: &str) -> bool {
    let candidate_path = Path::new(candidate);
    if !candidate_path.is_absolute() {
        return false;
    }
    match normalize_lexical(candidate_path) {
        Some(candidate) => candidate.starts_with(root),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app_dir() -> PathBuf {
        PathBuf::from("/var/lib/rast/apps/shop")
    }

    fn sample_config() -> Value {
        json!({
            "name": "rast-shop",
            "services": {
                "web": {
                    "image": "nginx:alpine",
                    "ports": [{"mode": "ingress", "target": 80, "published": "8080", "protocol": "tcp"}],
                    "volumes": [
                        {"type": "bind", "source": "/var/lib/rast/apps/shop/data/html", "target": "/usr/share/nginx/html", "read_only": true},
                        {"type": "volume", "source": "cache", "target": "/var/cache/nginx", "volume": {}}
                    ]
                }
            },
            "volumes": {"cache": {}}
        })
    }

    #[test]
    fn accepts_a_sane_normalized_config() {
        let summary = validate_normalized_config(&sample_config(), &app_dir()).unwrap();
        assert_eq!(summary.services, vec!["web".to_string()]);
        assert_eq!(summary.bind_mounts, 1);
        assert_eq!(summary.container_count(), 1);
    }

    #[test]
    fn rejects_host_bind_mounts_outside_app_data() {
        let mut cfg = sample_config();
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "bind", "source": "/etc", "target": "/host-etc"}
        ]);
        let err = validate_normalized_config(&cfg, &app_dir()).unwrap_err();
        assert!(err.contains("outside the app data directory"), "{err}");
    }

    #[test]
    fn rejects_docker_socket_mount() {
        let mut cfg = sample_config();
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "bind", "source": "/var/run/docker.sock", "target": "/var/run/docker.sock"}
        ]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
    }

    #[test]
    fn rejects_priviliged_host_namespaces_and_devices() {
        for (key, value) in [
            ("privileged", json!(true)),
            ("pid", json!("host")),
            ("network_mode", json!("host")),
            ("network_mode", json!("container:other")),
            ("ipc", json!("host")),
            ("userns_mode", json!("host")),
            ("cgroup", json!("host")),
            ("volumes_from", json!(["other"])),
            (
                "devices",
                json!([{"source": "/dev/sda", "target": "/dev/sda"}]),
            ),
        ] {
            let mut cfg = sample_config();
            cfg["services"]["web"][key] = value;
            assert!(
                validate_normalized_config(&cfg, &app_dir()).is_err(),
                "{key} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_dangerous_capabilities_and_unconfined_profiles() {
        let mut cfg = sample_config();
        cfg["services"]["web"]["cap_add"] = json!(["SYS_ADMIN"]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let mut cfg = sample_config();
        cfg["services"]["web"]["cap_add"] = json!(["CAP_DAC_OVERRIDE"]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let mut cfg = sample_config();
        cfg["services"]["web"]["security_opt"] = json!(["seccomp=unconfined"]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        // Benign capabilities stay allowed.
        let mut cfg = sample_config();
        cfg["services"]["web"]["cap_add"] = json!(["NET_BIND_SERVICE"]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_ok());
    }

    #[test]
    fn rejects_external_or_driver_opt_volumes() {
        let mut cfg = sample_config();
        cfg["volumes"]["cache"] = json!({"external": true});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());

        let mut cfg = sample_config();
        cfg["volumes"]["cache"] =
            json!({"driver_opts": {"type": "none", "o": "bind", "device": "/etc"}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
    }

    #[test]
    fn rejects_undeclared_named_volume_and_invalid_mount() {
        let mut cfg = sample_config();
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "volume", "source": "missing", "target": "/data"}
        ]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());

        let mut cfg = sample_config();
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "volume", "source": "cache", "target": "/data", "source2": "${X}"}
        ]);
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "bind", "source": "${DATA}", "target": "/data"}
        ]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());

        let mut cfg = sample_config();
        cfg["services"]["web"]["volumes"] = json!([
            {"type": "bind", "source": "/var/lib/rast/apps/shop/data/../../other", "target": "/data"}
        ]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
    }

    #[test]
    fn rejects_external_configs_secrets_and_networks() {
        let mut cfg = sample_config();
        cfg["configs"] = json!({"conf": {"file": "/etc/passwd"}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let mut cfg = sample_config();
        cfg["secrets"] = json!({"tok": {"file": "/var/lib/rast/secret.key"}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let mut cfg = sample_config();
        cfg["configs"] = json!({"conf": {"file": "/var/lib/rast/apps/shop/conf/x.conf"}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_ok());
        let mut cfg = sample_config();
        cfg["networks"] = json!({"default": {"external": true}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
    }

    #[test]
    fn rejects_build_and_missing_image() {
        let mut cfg = sample_config();
        cfg["services"]["web"] = json!({"build": {"context": "/etc"}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let mut cfg = sample_config();
        cfg["services"]["web"] = json!({"command": ["true"]});
        let err = validate_normalized_config(&cfg, &app_dir()).unwrap_err();
        assert!(err.contains("image"), "{err}");
    }

    #[test]
    fn rejects_bad_service_names_and_empty_services() {
        let mut cfg = sample_config();
        cfg["services"]["bad service"] = json!({"image": "alpine"});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let cfg = json!({"services": {}});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let cfg = json!({"version": "3"});
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
        let cfg = json!(["services"]);
        assert!(validate_normalized_config(&cfg, &app_dir()).is_err());
    }

    #[test]
    fn raw_text_precheck() {
        assert!(check_compose_text("services:\n  web:\n    image: nginx\n", "shop").is_ok());
        assert!(
            check_compose_text("# comment\nservices:\n  web:\n    image: nginx\n", "shop").is_ok()
        );

        for bad in [
            "",
            "version: \"3\"\n",
            "- just\n- a list\n",
            "services:\n  web:\n    image: nginx\n    env_file: /etc/passwd\n",
            "services:\n  web:\n    image: nginx\n    privileged: true\n",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - /var/run/docker.sock:/s\n",
            "services:\n  web: {image: nginx, env_file: /etc/passwd}\n",
            "include:\n  - other.yaml\nservices:\n  web:\n    image: nginx\n",
        ] {
            assert!(
                check_compose_text(bad, "shop").is_err(),
                "{bad:?} should fail"
            );
        }

        let huge = format!(
            "services:\n  web:\n    image: {}\n",
            "a".repeat(MAX_COMPOSE_BYTES)
        );
        assert!(check_compose_text(&huge, "shop").is_err());
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c/./d")).unwrap(),
            PathBuf::from("/a/c/d")
        );
        assert!(path_is_within(Path::new("/a/b"), "/a/b/c"));
        assert!(path_is_within(Path::new("/a/b"), "/a/b/c/../d"));
        assert!(!path_is_within(Path::new("/a/b"), "/a/bc"));
        assert!(!path_is_within(Path::new("/a/b"), "/a"));
        assert!(!path_is_within(Path::new("/a/b"), "relative/path"));
        assert!(!path_is_within(Path::new("/a/b"), "/a/b/../../etc"));
    }
}
