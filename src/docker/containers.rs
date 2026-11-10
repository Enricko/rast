//! Parsing helpers for Docker CLI output and small normalized container types
//! shared by the worker, the Docker client and the resource monitor.
//!
//! The worker always invokes Docker with Go templates that emit JSON
//! (`--format '{{json .}}'`) or with the Compose `--format json` option, so the
//! parsing here is deterministic and unit-testable without a Docker daemon.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Published port of a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortMapping {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    pub container_port: u16,
    pub protocol: String,
}

impl PortMapping {
    fn display(&self) -> String {
        match (&self.host_ip, self.host_port) {
            (Some(ip), Some(port)) => format!("{ip}:{port}:{}", self.container_port),
            (None, Some(port)) => format!("{port}:{}", self.container_port),
            _ => self.container_port.to_string(),
        }
    }
}

impl std::fmt::Display for PortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// Parse the `Ports` column of `docker ps` (`0.0.0.0:8080->80/tcp, :::8080->80/tcp`).
/// Duplicate mappings from IPv4 and IPv6 bindings are collapsed.
pub fn parse_ports(text: &str) -> Vec<PortMapping> {
    let mut out: Vec<PortMapping> = Vec::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Split "host->container/proto"; entries without "->" are exposed-only.
        let (host_part, rest) = match entry.split_once("->") {
            Some((host, rest)) => (Some(host.trim()), rest.trim()),
            None => (None, entry),
        };
        let (container_part, protocol) = match rest.split_once('/') {
            Some((port, proto)) => (port.trim(), proto.trim().to_string()),
            None => (rest, "tcp".to_string()),
        };
        let Ok(container_port) = container_part.parse::<u16>() else {
            continue;
        };
        let mut host_ip = None;
        let mut host_port = None;
        if let Some(host) = host_part {
            if !host.is_empty() {
                // forms: 0.0.0.0:8080  [::]:8080  :::8080  8080
                if let Some((ip, port)) = host.rsplit_once(':') {
                    let ip = ip.trim_matches(['[', ']']);
                    // Wildcard binds (IPv4 `0.0.0.0`, IPv6 `::`) are reported
                    // as "all interfaces" (`None`) and deduplicated.
                    if ip.is_empty() || ip == "::" || ip == "0.0.0.0" {
                        host_ip = None;
                    } else {
                        host_ip = Some(ip.to_string());
                    }
                    host_port = port.parse::<u16>().ok();
                } else {
                    host_port = host.parse::<u16>().ok();
                }
            }
        }
        let mapping = PortMapping {
            host_ip,
            host_port,
            container_port,
            protocol,
        };
        if !out.contains(&mapping) {
            out.push(mapping);
        }
    }
    out
}

/// Parse the comma separated `Labels` column of `docker ps`.
pub fn parse_labels(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                out.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    out
}

fn json_value_from_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(line).ok()
}

/// Parse `docker ps --format '{{json .}}'` output (one JSON object per line)
/// into the normalized container representation used by the API.
pub fn parse_ps_json(output: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Some(value) = json_value_from_line(line) else {
            continue;
        };
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => continue,
        };
        let get = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let labels = parse_labels(&get("Labels"));
        let ports_text = obj
            .get("Ports")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let project = labels
            .get("com.docker.compose.project")
            .cloned()
            .unwrap_or_default();
        let service = labels
            .get("com.docker.compose.service")
            .cloned()
            .unwrap_or_default();
        let name = get("Names");
        if name.is_empty() {
            continue;
        }
        let mounts_text = obj
            .get("Mounts")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let mounts: Vec<Value> = mounts_text
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| Value::String(entry.to_string()))
            .collect();
        out.push(json!({
            "id": get("ID"),
            "name": name,
            "image": get("Image"),
            "command": get("Command"),
            "state": get("State"),
            "status": get("Status"),
            "runningFor": get("RunningFor"),
            "ports": parse_ports(&ports_text),
            "mounts": mounts,
            "labels": labels,
            "project": project,
            "service": service,
        }));
    }
    Ok(out)
}

/// Parse a Docker `--format '{{json .}}'` stream into raw JSON values.
fn parse_json_lines(output: &str) -> Vec<Value> {
    output.lines().filter_map(json_value_from_line).collect()
}

/// Parse `docker stats --no-stream --format '{{json .}}'` output into normalized
/// resource samples (numbers, not human strings).
pub fn parse_stats_json(output: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for value in parse_json_lines(output) {
        let Some(obj) = value.as_object() else {
            continue;
        };
        let get_str = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let id = get_str("ID");
        let container = get_str("Container");
        let id = if id.is_empty() { container } else { id };
        let name = get_str("Name");
        if name.is_empty() {
            continue;
        }
        let (mem_used, mem_limit) = parse_mem_usage(&get_str("MemUsage"));
        let (net_rx, net_tx) = parse_io_pair(&get_str("NetIO"));
        let (block_read, block_write) = parse_io_pair(&get_str("BlockIO"));
        let pids = get_str("Pids").parse::<u64>().unwrap_or(0);
        out.push(json!({
            "id": id,
            "name": name,
            "cpuPercent": parse_percent(&get_str("CPUPerc")).unwrap_or(0.0),
            "memoryBytes": mem_used,
            "memoryLimitBytes": mem_limit,
            "memoryPercent": parse_percent(&get_str("MemPerc")).unwrap_or(0.0),
            "netRxBytes": net_rx,
            "netTxBytes": net_tx,
            "blockReadBytes": block_read,
            "blockWriteBytes": block_write,
            "pids": pids,
        }));
    }
    Ok(out)
}

/// Parse `docker inspect` output into the subset needed for a managed import.
///
/// Only container configuration is returned; environment values are included
/// because the caller encrypts and stores them, and they are never logged.
pub fn parse_inspect_json(output: &str) -> Result<Vec<Value>, String> {
    let parsed: Value = serde_json::from_str(output)
        .map_err(|e| format!("cannot parse docker inspect output: {e}"))?;
    let array = parsed
        .as_array()
        .ok_or_else(|| "docker inspect returned no containers".to_string())?;
    let mut out = Vec::new();
    for item in array {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let get_str = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let name = get_str("Name").trim_start_matches('/').to_string();
        if name.is_empty() {
            continue;
        }
        let labels: BTreeMap<String, String> = obj
            .get("Config")
            .and_then(|c| c.get("Labels"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let env: Vec<String> = obj
            .get("Config")
            .and_then(|c| c.get("Env"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut ports = Vec::new();
        if let Some(map) = obj
            .get("NetworkSettings")
            .and_then(|n| n.get("Ports"))
            .and_then(Value::as_object)
        {
            for (container_port, bindings) in map {
                let (port_str, protocol) = match container_port.split_once('/') {
                    Some((p, proto)) => (p, proto),
                    None => (container_port.as_str(), "tcp"),
                };
                let Ok(container_port) = port_str.parse::<u16>() else {
                    continue;
                };
                match bindings.as_array() {
                    Some(bindings) if !bindings.is_empty() => {
                        for binding in bindings {
                            let host_ip = binding
                                .get("HostIp")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string);
                            let host_port = binding
                                .get("HostPort")
                                .and_then(Value::as_str)
                                .and_then(|s| s.parse::<u16>().ok());
                            ports.push(PortMapping {
                                host_ip,
                                host_port,
                                container_port,
                                protocol: protocol.to_string(),
                            });
                        }
                    }
                    _ => ports.push(PortMapping {
                        host_ip: None,
                        host_port: None,
                        container_port,
                        protocol: protocol.to_string(),
                    }),
                }
            }
        }
        let mounts: Vec<Value> = obj
            .get("Mounts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        let obj = m.as_object()?;
                        Some(json!({
                            "type": obj.get("Type").and_then(Value::as_str).unwrap_or(""),
                            "source": obj.get("Source").and_then(Value::as_str).unwrap_or(""),
                            "target": obj.get("Destination").and_then(Value::as_str).unwrap_or(""),
                            "name": obj.get("Name").and_then(Value::as_str).unwrap_or(""),
                            "readOnly": obj.get("RW").and_then(Value::as_bool).map(|rw| !rw).unwrap_or(false),
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let restart_policy = obj
            .get("HostConfig")
            .and_then(|h| h.get("RestartPolicy"))
            .and_then(|r| r.get("Name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let state = obj
            .get("State")
            .and_then(|s| s.get("Status"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(json!({
            "id": get_str("Id"),
            "name": name,
            "image": obj.get("Config").and_then(|c| c.get("Image")).and_then(Value::as_str).unwrap_or(""),
            "state": state,
            "ports": ports,
            "mounts": mounts,
            "env": env,
            "labels": labels,
            "restartPolicy": restart_policy,
            "composeProject": labels.get("com.docker.compose.project").cloned().unwrap_or_default(),
            "composeService": labels.get("com.docker.compose.service").cloned().unwrap_or_default(),
        }));
    }
    Ok(out)
}

/// Parse the output of `docker compose logs --no-color --timestamps --tail N`.
///
/// Compose prefixes each line with `<service>  | `; with `--timestamps` the
/// first token after the separator is RFC3339. The stream (stdout/stderr) is
/// merged by Compose, so it is reported as `stdout`.
pub fn parse_compose_logs(output: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (service, rest) = match line.split_once('|') {
            Some((left, right)) => (left.trim().to_string(), right.trim_start()),
            None => (String::new(), line),
        };
        let mut rest = rest;
        let mut ts = 0i64;
        if let Some((first, remainder)) = rest.split_once(char::is_whitespace) {
            if let Some(parsed) = parse_rfc3339_ms(first) {
                ts = parsed;
                rest = remainder;
            }
        }
        out.push(json!({
            "ts": ts,
            "stream": "stdout",
            "service": service,
            "message": sanitize_log_line(rest),
        }));
    }
    out
}

fn sanitize_log_line(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    crate::docker::protocol::truncate_message(&cleaned, 4096)
}

/// Convert `12.34%` to `12.34`.
pub fn parse_percent(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('%').trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Convert a Docker human-readable size (`1.5MiB`, `12kB`, `0B`) to bytes.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let split_at = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(split_at);
    let Ok(value) = number.trim().parse::<f64>() else {
        return None;
    };
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let multiplier: f64 = match suffix.trim() {
        "" | "B" => 1.0,
        // Docker uses decimal units for network and block I/O.
        "kB" | "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        // ... and binary units for memory.
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = value * multiplier;
    if bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes.round() as u64)
}

/// Parse `"1.5MiB / 7.6GiB"` into `(used, limit)`.
pub fn parse_mem_usage(text: &str) -> (u64, u64) {
    let mut parts = text.split('/').map(str::trim);
    let used = parts.next().and_then(parse_size).unwrap_or(0);
    let limit = parts.next().and_then(parse_size).unwrap_or(0);
    (used, limit)
}

/// Parse `"1.2kB / 3.4kB"` into `(rx, tx)`.
pub fn parse_io_pair(text: &str) -> (u64, u64) {
    let mut parts = text.split('/').map(str::trim);
    let rx = parts.next().and_then(parse_size).unwrap_or(0);
    let tx = parts.next().and_then(parse_size).unwrap_or(0);
    (rx, tx)
}

/// Parse an RFC3339 timestamp into Unix epoch milliseconds.
///
/// Docker emits nanosecond precision; only millisecond precision is kept. A
/// missing timezone is treated as UTC. Returns `None` for anything unexpected.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Split the timezone suffix off: Z, +hh:mm or -hh:mm.
    let (time_part, offset_secs) = if let Some(stripped) = time.strip_suffix('Z') {
        (stripped, 0i64)
    } else if let Some(idx) = time[1..].rfind(['+', '-']).map(|i| i + 1) {
        let (t, off) = time.split_at(idx);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut off_parts = off[1..].split(':');
        let oh: i64 = off_parts.next()?.parse().ok()?;
        let om: i64 = off_parts.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (time, 0)
    };
    let mut time_parts = time_part.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let sec_part = time_parts.next()?;
    if time_parts.next().is_some() {
        return None;
    }
    let (second_str, frac_str) = match sec_part.split_once('.') {
        Some((s, f)) => (s, f),
        None => (sec_part, ""),
    };
    let second: i64 = second_str.parse().ok()?;
    let mut millis = 0i64;
    if !frac_str.is_empty() {
        let digits: String = frac_str.chars().take(3).collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let padded = format!("{digits:0<3}");
        millis = padded.parse().ok()?;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_parsing_handles_ipv4_ipv6_and_exposed_only() {
        let ports = parse_ports(
            "0.0.0.0:8080->80/tcp, :::8080->80/tcp, 127.0.0.1:5432->5432/tcp, 9000/tcp",
        );
        assert_eq!(
            ports.len(),
            3,
            "IPv4/IPv6 wildcard binds collapse: {ports:?}"
        );
        assert_eq!(ports[0].host_port, Some(8080));
        assert_eq!(ports[0].container_port, 80);
        assert_eq!(ports[0].host_ip, None, "wildcard binds are unspecified");
        assert_eq!(ports[1].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(ports[1].protocol, "tcp");
        assert_eq!(ports[2].container_port, 9000);
        assert_eq!(ports[2].host_port, None);

        let exposed = parse_ports("9000/tcp");
        assert_eq!(exposed.len(), 1);
        assert_eq!(exposed[0].host_port, None);
        assert!(parse_ports("").is_empty());
    }

    #[test]
    fn label_parsing() {
        let labels = parse_labels(
            "com.docker.compose.project=rast-shop,com.docker.compose.service=web,maintainer=me",
        );
        assert_eq!(
            labels.get("com.docker.compose.project").map(String::as_str),
            Some("rast-shop")
        );
        assert_eq!(
            labels.get("com.docker.compose.service").map(String::as_str),
            Some("web")
        );
        assert!(parse_labels("").is_empty());
    }

    #[test]
    fn ps_json_lines_are_normalized() {
        let output = r#"{"Command":"\"/docker-entrypoint.sh\"","CreatedAt":"2026-11-10 12:00:00 +0000 UTC","ID":"0123456789ab","Image":"nginx:alpine","Labels":"com.docker.compose.project=rast-shop,com.docker.compose.service=web","Mounts":"","Names":"shop-web-1","Networks":"rast-shop_default","Ports":"0.0.0.0:8080->80/tcp","RunningFor":"2 hours ago","Size":"0B","State":"running","Status":"Up 2 hours"}"#;
        let containers = parse_ps_json(output).unwrap();
        assert_eq!(containers.len(), 1);
        let c = &containers[0];
        assert_eq!(c["name"], "shop-web-1");
        assert_eq!(c["state"], "running");
        assert_eq!(c["project"], "rast-shop");
        assert_eq!(c["service"], "web");
        assert_eq!(c["ports"][0]["hostPort"], 8080);
        assert!(parse_ps_json("not json\n").unwrap().is_empty());
    }

    #[test]
    fn stats_json_lines_are_normalized() {
        let output = r#"{"BlockIO":"1.5MB / 2MB","CPUPerc":"12.34%","Container":"0123456789ab","ID":"0123456789ab","MemPerc":"1.20%","MemUsage":"120MiB / 7.5GiB","Name":"shop-web-1","NetIO":"3.5kB / 1.2kB","Pids":"7"}"#;
        let stats = parse_stats_json(output).unwrap();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s["name"], "shop-web-1");
        assert!((s["cpuPercent"].as_f64().unwrap() - 12.34).abs() < 1e-6);
        assert_eq!(s["memoryBytes"], 120 * 1024 * 1024);
        assert_eq!(s["pids"], 7);
    }

    #[test]
    fn size_and_percent_parsing() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("1.5MiB"), Some(1_572_864));
        assert_eq!(parse_size("12kB"), Some(12_000));
        assert_eq!(parse_size("2GB"), Some(2_000_000_000));
        assert_eq!(parse_size("garbage"), None);
        assert_eq!(parse_size("-1MiB"), None);
        assert_eq!(parse_percent("12.34%"), Some(12.34));
        assert_eq!(parse_percent("0.00%"), Some(0.0));
        assert_eq!(parse_percent(""), None);
        assert_eq!(
            parse_mem_usage("120MiB / 7.5GiB"),
            (125_829_120, 8_053_063_680)
        );
        assert_eq!(parse_io_pair("3.5kB / 1.2kB"), (3500, 1200));
    }

    #[test]
    fn rfc3339_parsing() {
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T13:24:02Z"),
            Some(1_794_317_042_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T13:24:02.123456789Z"),
            Some(1_794_317_042_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T14:24:02+01:00"),
            Some(1_794_317_042_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T13:24:02"),
            Some(1_794_317_042_000)
        );
        assert_eq!(parse_rfc3339_ms("not-a-time"), None);
    }

    #[test]
    fn compose_logs_are_parsed_and_bounded() {
        let output = "shop-web-1  | 2026-11-10T13:24:02.123456789Z request served\n\
                      shop-web-1  | plain message without timestamp\n\
                      \n";
        let lines = parse_compose_logs(output);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ts"], 1_794_317_042_123i64);
        assert_eq!(lines[0]["message"], "request served");
        assert_eq!(lines[1]["ts"], 0);
        assert_eq!(lines[1]["message"], "plain message without timestamp");
    }

    #[test]
    fn inspect_parsing_extracts_import_fields() {
        let output = r#"[{"Id":"abc123","Name":"/legacy-web","Config":{"Image":"nginx:alpine","Env":["FOO=bar","PATH=/usr/bin"],"Labels":{"com.docker.compose.project":"other"}},"State":{"Status":"running"},"HostConfig":{"RestartPolicy":{"Name":"unless-stopped"}},"Mounts":[{"Type":"volume","Source":"/var/lib/docker/volumes/legacy/_data","Destination":"/data","Name":"legacy_data","RW":true}],"NetworkSettings":{"Ports":{"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"8080"}]}}}]"#;
        let inspected = parse_inspect_json(output).unwrap();
        assert_eq!(inspected.len(), 1);
        let c = &inspected[0];
        assert_eq!(c["name"], "legacy-web");
        assert_eq!(c["image"], "nginx:alpine");
        assert_eq!(c["ports"][0]["hostPort"], 8080);
        assert_eq!(c["mounts"][0]["type"], "volume");
        assert_eq!(c["mounts"][0]["name"], "legacy_data");
        assert_eq!(c["env"][0], "FOO=bar");
        assert_eq!(c["restartPolicy"], "unless-stopped");
        assert!(parse_inspect_json("{}").is_err());
        assert!(parse_inspect_json("nonsense").is_err());
    }
}
