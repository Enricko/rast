//! Resource monitor: host CPU/memory/swap/uptime, disks, network throughput and
//! container stats. A single process-wide sampler runs on the maintenance timer
//! (`AppState`), so the cost does not grow with the number of open browser tabs.
//!
//! Collection is best-effort: sysinfo constructors return `Result` in 0.40, and
//! a missing `/proc`, permission error or failed Docker probe must never panic
//! or take the server down. When the host sampler cannot produce data the
//! snapshot reports `unavailable: true` plus a human-readable `reason`; partial
//! failures (e.g. disks unreadable) are surfaced in `components` while the rest
//! of the snapshot stays usable.
//!
//! A bounded ring buffer ([`HISTORY_CAPACITY`] samples, roughly one hour at the
//! default 5 s interval) serves the history endpoint. Downsampling to `points`
//! happens at read time.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind, System};

use crate::{config::Config, docker::DockerManager, error::AppResult};

/// Number of samples kept in the ring buffer (~1 hour at 5 s).
pub const HISTORY_CAPACITY: usize = 720;
/// Default number of points returned by the history endpoint.
pub const DEFAULT_HISTORY_POINTS: usize = 120;
/// A sample is "fresh" while it is younger than this many intervals.
const FRESH_INTERVAL_MULTIPLIER: u32 = 3;
/// Maximum number of containers reported in one snapshot.
const CONTAINER_STATS_INTERVAL: Duration = Duration::from_secs(15);
const MAX_CONTAINERS: usize = 100;
/// Maximum disks reported in one snapshot.
const MAX_DISKS: usize = 64;

/// One compact history sample (kept small: 720 of these are always resident).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SamplePoint {
    pub ts: i64,
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub rx_per_sec: u64,
    pub tx_per_sec: u64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub load_one: f64,
    pub unavailable: bool,
}

/// Bounded FIFO of samples.
#[derive(Debug)]
pub struct SampleHistory {
    capacity: usize,
    points: VecDeque<SamplePoint>,
}

impl SampleHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            points: VecDeque::with_capacity(capacity.clamp(1, 4096)),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn push(&mut self, point: SamplePoint) {
        while self.points.len() >= self.capacity {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    pub fn last(&self) -> Option<&SamplePoint> {
        self.points.back()
    }

    /// The last `count` points, oldest first.
    pub fn tail(&self, count: usize) -> Vec<&SamplePoint> {
        let count = count.clamp(1, self.capacity);
        let skip = self.points.len().saturating_sub(count);
        self.points.iter().skip(skip).collect()
    }

    pub fn cpu_series(&self, points: usize) -> Vec<Value> {
        self.tail(points)
            .into_iter()
            .map(|p| json!({"ts": p.ts, "value": round2(p.cpu_percent as f64)}))
            .collect()
    }

    pub fn mem_series(&self, points: usize) -> Vec<Value> {
        self.tail(points)
            .into_iter()
            .map(|p| {
                json!({
                    "ts": p.ts,
                    "value": round2(percent(p.mem_used, p.mem_total)),
                    "usedBytes": p.mem_used,
                    "totalBytes": p.mem_total,
                    "swapUsedBytes": p.swap_used,
                    "swapTotalBytes": p.swap_total,
                })
            })
            .collect()
    }

    pub fn net_series(&self, points: usize) -> Vec<Value> {
        self.tail(points)
            .into_iter()
            .map(|p| {
                json!({
                    "ts": p.ts,
                    "value": p.rx_per_sec.saturating_add(p.tx_per_sec),
                    "rxBytesPerSec": p.rx_per_sec,
                    "txBytesPerSec": p.tx_per_sec,
                    "rxTotal": p.rx_total,
                    "txTotal": p.tx_total,
                })
            })
            .collect()
    }
}

/// Mutable sampler state; guarded by a `std::sync::Mutex` because reads
/// (`latest`, `is_fresh`) are synchronous and locks are only held for
/// non-blocking bookkeeping.
struct Inner {
    system: Option<System>,
    networks: Option<Networks>,
    prev_net: Option<(u64, u64)>,
    prev_net_at: Option<Instant>,
    history: SampleHistory,
    latest: Option<Value>,
    last_sample_at: Option<Instant>,
    containers: Vec<Value>,
    containers_available: bool,
    containers_reason: Option<String>,
    last_containers_at: Option<Instant>,
}

/// Process-wide resource sampler.
pub struct Monitor {
    interval_secs: u64,
    docker: Option<Arc<DockerManager>>,
    inner: Mutex<Inner>,
}

impl Monitor {
    pub fn new(cfg: &Config) -> AppResult<Arc<Self>> {
        let docker = if cfg.docker_enabled {
            Some(DockerManager::new(cfg))
        } else {
            None
        };
        Ok(Arc::new(Self {
            interval_secs: cfg.monitor_interval_secs.max(1),
            docker,
            inner: Mutex::new(Inner {
                system: None,
                networks: None,
                prev_net: None,
                prev_net_at: None,
                history: SampleHistory::new(HISTORY_CAPACITY),
                latest: None,
                last_sample_at: None,
                containers: Vec::new(),
                containers_available: false,
                containers_reason: None,
                last_containers_at: None,
            }),
        }))
    }

    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned monitor lock must never take the whole server down.
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// True when the most recent sample is younger than 3 sampling intervals.
    pub fn is_fresh(&self) -> bool {
        let inner = self.lock();
        freshness(inner.last_sample_at, Instant::now(), self.interval_secs)
    }

    /// Latest snapshot, if the sampler has produced one.
    pub fn latest(&self) -> Option<Value> {
        self.lock().latest.clone()
    }

    /// Container samples from the latest snapshot (empty when Docker is
    /// disabled or the worker is unavailable).
    pub fn containers(&self) -> Value {
        let inner = self.lock();
        json!({
            "available": inner.containers_available,
            "reason": inner.containers_reason,
            "containers": inner.containers,
        })
    }

    /// Return the history for one metric as a chart-friendly series.
    ///
    /// `metric` must be one of `cpu`, `mem`, `net`; `points` is clamped to the
    /// ring buffer capacity.
    pub async fn history(&self, metric: &str, points: usize) -> Option<Value> {
        let points = points.clamp(1, HISTORY_CAPACITY);
        let inner = self.lock();
        let (series, retention) = match metric {
            "cpu" => (inner.history.cpu_series(points), inner.history.capacity()),
            "mem" => (inner.history.mem_series(points), inner.history.capacity()),
            "net" => (inner.history.net_series(points), inner.history.capacity()),
            _ => return None,
        };
        let available = !series.is_empty();
        Some(json!({
            "metric": metric,
            "intervalSecs": self.interval_secs,
            "retention": retention,
            "available": available,
            "points": series,
        }))
    }

    /// Sample the host once and update the ring buffer / latest snapshot.
    ///
    /// Never panics: every subsystem failure is recorded in `components` and,
    /// when the host sampler itself is unusable, in `unavailable`/`reason`.
    pub async fn sample(&self) -> Value {
        let ts = now_ms();

        // Phase 1: host metrics (synchronous, cheap). Keep the lock only while
        // mutating sampler state.
        let host = {
            let mut inner = self.lock();
            collect_host(&mut inner)
        };

        // Phase 2: container stats (may await the worker; never while holding
        // the lock). `docker stats` can take seconds while the daemon is busy, so
        // it is collected at most every CONTAINER_STATS_INTERVAL; other samples
        // reuse the last values instead of queueing more worker requests.
        let (containers, docker_ok, docker_reason) = {
            let due = {
                let inner = self.lock();
                inner
                    .last_containers_at
                    .map(|at| at.elapsed() >= CONTAINER_STATS_INTERVAL)
                    .unwrap_or(true)
            };
            if due {
                let result = self.collect_containers().await;
                self.lock().last_containers_at = Some(Instant::now());
                result
            } else {
                let inner = self.lock();
                (
                    inner.containers.clone(),
                    inner.containers_available,
                    inner.containers_reason.clone(),
                )
            }
        };

        // Phase 3: compose the snapshot and store it.
        let mut inner = self.lock();
        inner.containers = containers.clone();
        inner.containers_available = docker_ok;
        inner.containers_reason = docker_reason.clone();
        let snapshot = build_snapshot(
            &host,
            &containers,
            docker_ok,
            docker_reason,
            self.interval_secs,
        );
        inner.latest = Some(snapshot.clone());
        inner.last_sample_at = Some(Instant::now());
        let point = SamplePoint {
            ts,
            cpu_percent: host.cpu_percent,
            mem_used: host.mem_used,
            mem_total: host.mem_total,
            swap_used: host.swap_used,
            swap_total: host.swap_total,
            rx_per_sec: host.rx_per_sec,
            tx_per_sec: host.tx_per_sec,
            rx_total: host.rx_total,
            tx_total: host.tx_total,
            load_one: host.load_one.unwrap_or(0.0),
            unavailable: host.unavailable,
        };
        inner.history.push(point);
        snapshot
    }

    async fn collect_containers(&self) -> (Vec<Value>, bool, Option<String>) {
        let Some(docker) = &self.docker else {
            return (
                Vec::new(),
                false,
                Some("Docker integration is disabled".to_string()),
            );
        };
        match docker.container_stats().await {
            Ok(data) => {
                let items = data
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let containers: Vec<Value> = items
                    .into_iter()
                    .take(MAX_CONTAINERS)
                    .map(|item| {
                        json!({
                            "id": item.get("id").cloned().unwrap_or(Value::Null),
                            "name": item.get("name").cloned().unwrap_or(Value::Null),
                            "cpuPercent": item.get("cpuPercent").cloned().unwrap_or(json!(0.0)),
                            "memoryBytes": item.get("memoryBytes").cloned().unwrap_or(json!(0)),
                            "memoryLimitBytes": item.get("memoryLimitBytes").cloned().unwrap_or(json!(0)),
                            "memoryPercent": item.get("memoryPercent").cloned().unwrap_or(json!(0.0)),
                            "pids": item.get("pids").cloned().unwrap_or(json!(0)),
                        })
                    })
                    .collect();
                (containers, true, None)
            }
            Err(error) => (Vec::new(), false, Some(error.to_string())),
        }
    }
}

/// Raw host metrics collected in one pass.
#[derive(Debug, Clone, Default)]
struct HostSample {
    unavailable: bool,
    reason: Option<String>,
    cpu_percent: f32,
    cpu_cores: usize,
    load_one: Option<f64>,
    load_five: Option<f64>,
    load_fifteen: Option<f64>,
    mem_used: u64,
    mem_total: u64,
    swap_used: u64,
    swap_total: u64,
    uptime_secs: Option<u64>,
    rx_per_sec: u64,
    tx_per_sec: u64,
    rx_total: u64,
    tx_total: u64,
    disks: Vec<Value>,
    failures: Vec<(String, String)>,
}

fn collect_host(inner: &mut Inner) -> HostSample {
    let mut sample = HostSample::default();

    if inner.system.is_none() {
        match System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::everything())
                .with_cpu(CpuRefreshKind::everything()),
        ) {
            Ok(system) => inner.system = Some(system),
            Err(error) => {
                sample.unavailable = true;
                sample.reason = Some(format!("cannot initialise host sampler: {error}"));
                sample.failures.push(("host".into(), error.to_string()));
                return sample;
            }
        }
    }
    if let Some(system) = inner.system.as_mut() {
        // CPU usage is a delta between refreshes; sysinfo requires a minimum
        // interval, which the monitor interval always satisfies.
        system.refresh_cpu_usage();
        system.refresh_memory_specifics(MemoryRefreshKind::everything());
        sample.cpu_percent = sanitize_percent(system.global_cpu_usage());
        sample.cpu_cores = system.cpus().len();
        if sample.cpu_cores == 0 {
            sample.cpu_cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
        }
        sample.mem_total = system.total_memory();
        sample.mem_used = system.used_memory();
        sample.swap_total = system.total_swap();
        sample.swap_used = system.used_swap();
    }

    match System::load_average() {
        Ok(load) => {
            sample.load_one = Some(round2(load.one));
            sample.load_five = Some(round2(load.five));
            sample.load_fifteen = Some(round2(load.fifteen));
        }
        Err(error) => {
            sample
                .failures
                .push(("load".into(), format!("load average unavailable: {error}")));
        }
    }

    match System::uptime() {
        Ok(secs) => sample.uptime_secs = Some(secs),
        Err(error) => sample
            .failures
            .push(("uptime".into(), format!("uptime unavailable: {error}"))),
    }

    match Disks::new_with_refreshed_list() {
        Ok(disks) => {
            for disk in &disks {
                if sample.disks.len() >= MAX_DISKS {
                    break;
                }
                let total = disk.total_space();
                let available = disk.available_space();
                sample.disks.push(json!({
                    "mount": disk.mount_point().display().to_string(),
                    "filesystem": disk.file_system().to_string_lossy(),
                    "name": disk.name().to_string_lossy(),
                    "totalBytes": total,
                    "availableBytes": available,
                    "usedBytes": total.saturating_sub(available),
                    "removable": disk.is_removable(),
                    "readOnly": disk.is_read_only(),
                }));
            }
        }
        Err(error) => sample
            .failures
            .push(("disks".into(), format!("cannot read disks: {error}"))),
    }

    // Network throughput from cumulative counters. The first sample has no
    // previous baseline, so the rate is reported as 0.
    if inner.networks.is_none() {
        match Networks::new_with_refreshed_list() {
            Ok(networks) => inner.networks = Some(networks),
            Err(error) => sample.failures.push((
                "network".into(),
                format!("cannot read network interfaces: {error}"),
            )),
        }
    }
    if let Some(networks) = inner.networks.as_mut() {
        networks.refresh(true);
        let mut rx: u64 = 0;
        let mut tx: u64 = 0;
        for data in networks.values() {
            rx = rx.saturating_add(data.total_received());
            tx = tx.saturating_add(data.total_transmitted());
        }
        sample.rx_total = rx;
        sample.tx_total = tx;
        let now = Instant::now();
        let elapsed = inner
            .prev_net_at
            .map(|previous| now.saturating_duration_since(previous))
            .unwrap_or(Duration::ZERO);
        if let Some((prev_rx, prev_tx)) = inner.prev_net {
            sample.rx_per_sec = rate_per_sec(prev_rx, rx, elapsed);
            sample.tx_per_sec = rate_per_sec(prev_tx, tx, elapsed);
        }
        inner.prev_net = Some((rx, tx));
        inner.prev_net_at = Some(now);
    }

    sample
}

fn build_snapshot(
    host: &HostSample,
    containers: &[Value],
    docker_ok: bool,
    docker_reason: Option<String>,
    interval_secs: u64,
) -> Value {
    let ts = now_ms();
    if host.unavailable {
        return json!({
            "ts": ts,
            "intervalSecs": interval_secs,
            "unavailable": true,
            "reason": host.reason.clone().unwrap_or_else(|| "sampling failed".into()),
            "host": Value::Null,
            "disks": [],
            "network": Value::Null,
            "containers": [],
            "components": component_health(host, false, docker_ok, docker_reason),
        });
    }
    json!({
        "ts": ts,
        "intervalSecs": interval_secs,
        "unavailable": false,
        "reason": Value::Null,
        "host": {
            "cpuPercent": round2(host.cpu_percent as f64),
            "cpuCores": host.cpu_cores,
            "loadAvg": [
                host.load_one.unwrap_or(0.0),
                host.load_five.unwrap_or(0.0),
                host.load_fifteen.unwrap_or(0.0)
            ],
            "memoryUsed": host.mem_used,
            "memoryTotal": host.mem_total,
            "swapUsed": host.swap_used,
            "swapTotal": host.swap_total,
            "uptimeSecs": host.uptime_secs.unwrap_or(0),
        },
        "disks": host.disks,
        "network": {
            "rxBytesPerSec": host.rx_per_sec,
            "txBytesPerSec": host.tx_per_sec,
            "rxTotal": host.rx_total,
            "txTotal": host.tx_total,
        },
        "containers": containers,
        "components": component_health(host, true, docker_ok, docker_reason),
    })
}

fn component_health(
    host: &HostSample,
    host_ok: bool,
    docker_ok: bool,
    docker_reason: Option<String>,
) -> Vec<Value> {
    let failure = |id: &str| {
        host.failures
            .iter()
            .find(|(component, _)| component == id)
            .map(|(_, detail)| detail.clone())
    };
    let mut components = vec![
        json!({
            "id": "host",
            "name": "Host sampler",
            "status": if host_ok { "ok" } else { "error" },
            "detail": if host_ok { "sampling".to_string() } else { host.reason.clone().unwrap_or_else(|| "failed".into()) },
        }),
        json!({
            "id": "cpu",
            "name": "CPU",
            "status": if host_ok { "ok" } else { "error" },
            "detail": format!("{} core(s)", host.cpu_cores),
        }),
        json!({
            "id": "memory",
            "name": "Memory",
            "status": if host_ok && host.mem_total > 0 { "ok" } else if host_ok { "warning" } else { "error" },
            "detail": if host.mem_total > 0 { format!("{} / {} MiB", host.mem_used / 1_048_576, host.mem_total / 1_048_576) } else { "no memory information".into() },
        }),
        json!({
            "id": "load",
            "name": "Load average",
            "status": if failure("load").is_none() { "ok" } else { "warning" },
            "detail": failure("load").unwrap_or_else(|| "available".into()),
        }),
        json!({
            "id": "uptime",
            "name": "Uptime",
            "status": if failure("uptime").is_none() { "ok" } else { "warning" },
            "detail": failure("uptime").unwrap_or_else(|| "available".into()),
        }),
        json!({
            "id": "disks",
            "name": "Disks",
            "status": if failure("disks").is_none() { "ok" } else { "warning" },
            "detail": failure("disks").unwrap_or_else(|| format!("{} filesystem(s)", host.disks.len())),
        }),
        json!({
            "id": "network",
            "name": "Network",
            "status": if failure("network").is_none() { "ok" } else { "warning" },
            "detail": failure("network").unwrap_or_else(|| "available".into()),
        }),
    ];
    let (docker_status, docker_detail) = if docker_ok {
        ("ok".to_string(), "docker worker reachable".to_string())
    } else {
        (
            "disabled".to_string(),
            docker_reason.unwrap_or_else(|| "Docker integration is disabled".into()),
        )
    };
    components.push(json!({
        "id": "docker",
        "name": "Docker",
        "status": docker_status,
        "detail": docker_detail,
    }));
    components
}

/// Clamp a CPU/percent value into `0.0..=100.0` (sysinfo can briefly exceed
/// 100 on virtualised hosts). Non-finite values are saturated: NaN/negative
/// values become 0, positive infinity becomes 100, so the public snapshot can
/// never report an out-of-range percentage.
pub fn sanitize_percent(value: f32) -> f32 {
    if value.is_nan() || value < 0.0 {
        0.0
    } else if value == f32::INFINITY {
        100.0
    } else {
        value.min(100.0)
    }
}

/// Used/total as a 0..=100 percentage; 0 when the total is unknown.
pub fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    round2((used as f64 / total as f64) * 100.0)
}

/// Bytes-per-second rate between two cumulative counters, resilient against
/// counter resets and zero/negative elapsed time.
pub fn rate_per_sec(previous: u64, current: u64, elapsed: Duration) -> u64 {
    let millis = elapsed.as_millis() as u64;
    if millis == 0 || current <= previous {
        return 0;
    }
    let delta = current - previous;
    // bytes * 1000 / millis, saturating.
    delta.saturating_mul(1000) / millis
}

/// A sample is fresh while it is younger than `3 * interval_secs`.
pub fn freshness(last_sample: Option<Instant>, now: Instant, interval_secs: u64) -> bool {
    let Some(last) = last_sample else {
        return false;
    };
    let threshold = Duration::from_secs(
        interval_secs
            .max(1)
            .saturating_mul(FRESH_INTERVAL_MULTIPLIER as u64),
    );
    now.saturating_duration_since(last) < threshold
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;

    fn point(ts: i64) -> SamplePoint {
        SamplePoint {
            ts,
            cpu_percent: ts as f32,
            mem_used: ts as u64 * 10,
            mem_total: 1000,
            swap_used: 0,
            swap_total: 0,
            rx_per_sec: ts as u64,
            tx_per_sec: ts as u64 * 2,
            rx_total: 1,
            tx_total: 2,
            load_one: 0.5,
            unavailable: false,
        }
    }

    fn test_monitor(points: &[SamplePoint]) -> Monitor {
        let mut history = SampleHistory::new(4);
        for p in points {
            history.push(p.clone());
        }
        Monitor {
            interval_secs: 5,
            docker: None,
            inner: Mutex::new(Inner {
                system: None,
                networks: None,
                prev_net: None,
                prev_net_at: None,
                history,
                latest: None,
                last_sample_at: None,
                containers: Vec::new(),
                containers_available: false,
                containers_reason: None,
                last_containers_at: None,
            }),
        }
    }

    #[test]
    fn ring_buffer_is_bounded_and_keeps_the_newest() {
        let mut history = SampleHistory::new(3);
        for ts in 1..=5 {
            history.push(point(ts));
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.capacity(), 3);
        let tail = history.tail(10);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.iter().map(|p| p.ts).collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(history.last().unwrap().ts, 5);
        assert_eq!(history.tail(2).len(), 2);
        // Capacity zero is promoted to one rather than panicking.
        let mut single = SampleHistory::new(0);
        single.push(point(1));
        single.push(point(2));
        assert_eq!(single.len(), 1);
        assert_eq!(single.last().unwrap().ts, 2);
    }

    #[test]
    fn percent_and_rate_math() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(50, 100), 50.0);
        assert_eq!(percent(1, 3), 33.33);
        assert_eq!(rate_per_sec(0, 0, Duration::from_secs(1)), 0);
        assert_eq!(rate_per_sec(100, 1100, Duration::from_secs(1)), 1000);
        assert_eq!(rate_per_sec(100, 1100, Duration::from_millis(500)), 2000);
        assert_eq!(
            rate_per_sec(100, 50, Duration::from_secs(1)),
            0,
            "counter reset"
        );
        assert_eq!(rate_per_sec(0, 100, Duration::ZERO), 0, "no elapsed time");
        // Saturating: huge deltas do not overflow.
        let huge = rate_per_sec(0, u64::MAX, Duration::from_millis(1));
        assert!(huge > 0);
    }

    #[test]
    fn cpu_percent_is_clamped() {
        assert_eq!(sanitize_percent(-1.0), 0.0);
        assert_eq!(sanitize_percent(f32::NAN), 0.0);
        assert_eq!(sanitize_percent(f32::NEG_INFINITY), 0.0);
        assert_eq!(sanitize_percent(f32::INFINITY), 100.0);
        assert_eq!(sanitize_percent(150.0), 100.0);
        assert_eq!(sanitize_percent(100.0), 100.0);
        assert_eq!(sanitize_percent(42.5), 42.5);
    }

    #[test]
    fn freshness_window() {
        let now = Instant::now();
        assert!(!freshness(None, now, 5));
        assert!(freshness(Some(now), now, 5));
        let old = now - Duration::from_secs(16);
        assert!(!freshness(Some(old), now, 5), "3x interval exceeded");
        let recent = now - Duration::from_secs(10);
        assert!(freshness(Some(recent), now, 5));
    }

    #[tokio::test]
    async fn history_series_are_shaped_and_bounded() {
        let monitor = test_monitor(&[point(1), point(2), point(3)]);
        let cpu = monitor.history("cpu", 2).await.unwrap();
        assert_eq!(cpu["metric"], "cpu");
        assert_eq!(cpu["points"].as_array().unwrap().len(), 2);
        assert_eq!(cpu["points"][1]["value"], 3.0);
        assert_eq!(cpu["available"], true);

        let mem = monitor.history("mem", 120).await.unwrap();
        assert_eq!(mem["points"].as_array().unwrap().len(), 3);
        assert_eq!(mem["points"][2]["usedBytes"], 30);

        let net = monitor.history("net", 3).await.unwrap();
        assert_eq!(net["points"][2]["rxBytesPerSec"], 3);
        assert_eq!(net["points"][2]["value"], 9);

        assert!(monitor.history("bogus", 10).await.is_none());
        assert!(monitor.latest().is_none());
        assert!(!monitor.is_fresh());
    }

    fn test_config() -> Config {
        let mut cfg = Config::from_parts(None, FileConfig::default(), |_| None).unwrap();
        cfg.data_dir = std::env::temp_dir().join("rast-monitor-test");
        cfg.storage_dir = cfg.data_dir.join("storage");
        cfg.docker_enabled = false;
        cfg.monitor_interval_secs = 5;
        cfg
    }

    #[tokio::test]
    async fn sample_produces_a_snapshot_and_updates_history() {
        let monitor = Monitor::new(&test_config()).unwrap();
        assert!(monitor.latest().is_none());
        assert!(!monitor.is_fresh());
        let snapshot = monitor.sample().await;
        assert_eq!(snapshot["unavailable"], false);
        assert!(snapshot["host"]["cpuCores"].as_u64().unwrap() >= 1);
        assert!(snapshot["host"]["memoryTotal"].as_u64().unwrap() > 0);
        assert!(snapshot["disks"].is_array());
        assert!(snapshot["network"]["rxTotal"].as_u64().is_some());
        assert!(snapshot["components"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "docker"));
        assert!(snapshot["containers"].as_array().unwrap().is_empty());
        assert!(monitor.is_fresh());
        assert!(monitor.latest().is_some());

        // Second sample fills the network delta path without panicking.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let second = monitor.sample().await;
        assert_eq!(second["unavailable"], false);
        let history = monitor.history("cpu", 10).await.unwrap();
        assert_eq!(history["points"].as_array().unwrap().len(), 2);
        // Containers unavailable without Docker: empty, with a reason.
        let containers = monitor.containers();
        assert_eq!(containers["available"], false);
        assert!(containers["containers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn snapshot_failure_is_reported_without_panicking() {
        let unreachable = HostSample {
            unavailable: true,
            reason: Some("cannot read /proc".into()),
            ..HostSample::default()
        };
        let snapshot = build_snapshot(&unreachable, &[], false, Some("disabled".into()), 5);
        assert_eq!(snapshot["unavailable"], true);
        assert_eq!(snapshot["reason"], "cannot read /proc");
        assert!(snapshot["host"].is_null());
        assert_eq!(snapshot["disks"].as_array().unwrap().len(), 0);
        let components = snapshot["components"].as_array().unwrap();
        assert!(components
            .iter()
            .any(|c| c["id"] == "host" && c["status"] == "error"));
    }
}
