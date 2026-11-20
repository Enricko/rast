//! Reaching the RAST server: address parsing, retry policy, the background prober
//! and the status the UI renders.
//!
//! The client is deliberately self-sufficient when the server is down: it keeps
//! running, retries on a backoff, and turns back into the connected state by itself
//! as soon as a TCP connection succeeds. Turning that off is an explicit setting.
//!
//! The browser build (`wasm32`) has no sockets and no threads, so it shares the
//! shared types ([`ServerAddr`], [`Status`], [`Phase`], [`Command`], [`Link`]) but
//! never starts a prober: the UI behaves as if the server is unreachable.

use std::sync::mpsc::{Receiver, Sender};

#[cfg(not(target_arch = "wasm32"))]
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(not(target_arch = "wasm32"))]
use std::thread;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use rune::Waker;

use crate::config::Config;

/// Port assumed when the server field names no port.
pub const DEFAULT_PORT: u16 = 443;
/// How long one TCP probe may take before it counts as unreachable.
#[cfg(not(target_arch = "wasm32"))]
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a connected server is re-checked for going away.
#[cfg(not(target_arch = "wasm32"))]
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long the worker sleeps when it has nothing to do, so a shutdown is quick.
#[cfg(not(target_arch = "wasm32"))]
const IDLE_SLICE: Duration = Duration::from_millis(250);

/// Delays between failed probes: 1 s, 2 s, 4 s, 8 s, 15 s, then 30 s for every
/// attempt after that.
#[cfg(not(target_arch = "wasm32"))]
pub fn retry_delay(attempt: u32) -> Duration {
    const SECONDS: [u64; 6] = [1, 2, 4, 8, 15, 30];
    let index = (attempt as usize).saturating_sub(1).min(SECONDS.len() - 1);
    Duration::from_secs(SECONDS[index])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerAddr {
    pub host: String,
    pub port: u16,
}

impl ServerAddr {
    /// Accept `host`, `host:port`, `https://host/path` and bracketed IPv6.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut text = input.trim();
        if text.is_empty() {
            return Err("the server address is empty".to_owned());
        }
        for scheme in ["https://", "http://", "tcp://"] {
            if let Some(rest) = text.strip_prefix(scheme) {
                text = rest;
                break;
            }
        }
        if let Some(index) = text.find(['/', '?', '#']) {
            text = &text[..index];
        }
        let text = text.trim().trim_end_matches('/');
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, after) = rest
                .split_once(']')
                .ok_or_else(|| "unterminated IPv6 address (missing `]`)".to_owned())?;
            match after {
                "" => (host, None),
                _ => match after.strip_prefix(':') {
                    Some(port) => (host, Some(port)),
                    None => return Err(format!("unexpected text after the IPv6 address: {after}")),
                },
            }
        } else if let Some((host, port)) = text.rsplit_once(':') {
            if host.contains(':') {
                return Err("IPv6 addresses need brackets, for example [::1]:443".to_owned());
            }
            (host, Some(port))
        } else {
            (text, None)
        };
        if host.is_empty() {
            return Err("the server address has no host".to_owned());
        }
        let valid_host = host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
        if !valid_host {
            return Err(format!("`{host}` is not a valid host name"));
        }
        let port = match port {
            None => DEFAULT_PORT,
            Some(raw) => raw
                .parse::<u16>()
                .map_err(|_| format!("`{raw}` is not a valid port"))?,
        };
        if port == 0 {
            return Err("port 0 cannot be a RAST server".to_owned());
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// `host:port`, with IPv6 hosts in brackets.
    pub fn display(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn socket_addrs(&self) -> Result<Vec<SocketAddr>, String> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map(|addrs| addrs.collect())
            .map_err(|error| format!("cannot resolve {}: {error}", self.host))
    }
}

/// One reachability check: a TCP connection, closed immediately. Cheap enough to
/// repeat every few seconds and good enough to tell "server down" from "server up".
#[cfg(not(target_arch = "wasm32"))]
pub fn probe(addr: &ServerAddr) -> Result<(), String> {
    let addrs = addr.socket_addrs()?;
    if addrs.is_empty() {
        return Err(format!("{} resolved to no address", addr.host));
    }
    let mut last_error = String::from("no route to the server");
    for socket in addrs {
        match TcpStream::connect_timeout(&socket, PROBE_TIMEOUT) {
            Ok(_stream) => return Ok(()),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The stored address cannot be used; the settings screen explains why.
    Setup,
    Connecting,
    Online,
    Offline,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub phase: Phase,
    /// Consecutive failed probes since the last success.
    pub attempts: u32,
    /// When the next automatic attempt happens; `None` while connecting, online or
    /// while automatic reconnection is off.
    pub retry_in: Option<Duration>,
    pub error: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self::setup()
    }
}

impl Status {
    pub fn setup() -> Self {
        Self {
            phase: Phase::Setup,
            attempts: 0,
            retry_in: None,
            error: None,
        }
    }

    pub fn setup_with(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::setup()
        }
    }

    pub fn connecting() -> Self {
        Self {
            phase: Phase::Connecting,
            attempts: 0,
            retry_in: None,
            error: None,
        }
    }

    pub fn online() -> Self {
        Self {
            phase: Phase::Online,
            attempts: 0,
            retry_in: None,
            error: None,
        }
    }

    pub fn offline(attempts: u32, retry_in: Option<Duration>, error: impl Into<String>) -> Self {
        Self {
            phase: Phase::Offline,
            attempts,
            retry_in,
            error: Some(error.into()),
        }
    }
}

/// The transition rules, free of threads and sockets so they can be tested.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct Machine {
    pub addr: ServerAddr,
    pub auto_reconnect: bool,
    pub attempts: u32,
}

#[cfg(not(target_arch = "wasm32"))]
impl Machine {
    pub fn new(addr: ServerAddr, auto_reconnect: bool) -> Self {
        Self {
            addr,
            auto_reconnect,
            attempts: 0,
        }
    }

    /// Begin an attempt (also used by "Reconnect now").
    pub fn start(&mut self) -> Status {
        Status::connecting()
    }

    /// Fold one probe result into a new status.
    pub fn probed(&mut self, result: Result<(), String>) -> Status {
        match result {
            Ok(()) => {
                self.attempts = 0;
                Status::online()
            }
            Err(error) => {
                self.attempts = self.attempts.saturating_add(1);
                let retry_in = if self.auto_reconnect {
                    Some(retry_delay(self.attempts))
                } else {
                    None
                };
                Status::offline(self.attempts, retry_in, error)
            }
        }
    }
}

/// Commands the UI sends to the prober thread.
#[derive(Clone, Debug)]
pub enum Command {
    /// Probe now instead of waiting for the backoff.
    RetryNow,
    /// Turn automatic reconnection on or off.
    SetAuto(bool),
    /// Point the client at another server.
    SetServer(ServerAddr),
    /// Stop the thread (window closed, app quitting).
    #[cfg(not(target_arch = "wasm32"))]
    Shutdown,
}

/// Handle to the background prober.
pub struct Link {
    pub commands: Sender<Command>,
    pub statuses: Receiver<Status>,
}

impl Link {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for Link {
    /// The window went away: let the prober thread end instead of leaking it.
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.send(Command::Shutdown);
    }
}

/// Start the background prober for `config`, waking the UI when a status arrives.
///
/// Returns `None` when the app runs headless (snapshots must be deterministic),
/// in the browser (no sockets or threads in the wasm build) or when the stored
/// address cannot be parsed; the UI then shows the setup screen.
pub fn spawn(config: &Config, wake: Waker) -> Option<Link> {
    if rune::is_headless() {
        return None;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (config, wake);
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        spawn_native(config, wake)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_native(config: &Config, wake: Waker) -> Option<Link> {
    let addr = ServerAddr::parse(&config.server).ok()?;
    let auto_reconnect = config.auto_reconnect;
    let (commands, command_rx) = mpsc::channel();
    let (status_sender, statuses) = mpsc::channel();
    thread::Builder::new()
        .name("rast-link".to_owned())
        .spawn(move || {
            worker(
                addr,
                auto_reconnect,
                &command_rx,
                &status_sender,
                wake,
                probe,
            )
        })
        .ok()?;
    Some(Link { commands, statuses })
}

/// The probe is a parameter so tests can drive the worker without a socket.
#[cfg(not(target_arch = "wasm32"))]
type Probe = fn(&ServerAddr) -> Result<(), String>;

#[cfg(not(target_arch = "wasm32"))]
fn worker(
    addr: ServerAddr,
    auto_reconnect: bool,
    commands: &Receiver<Command>,
    statuses: &Sender<Status>,
    wake: Waker,
    probe: Probe,
) {
    let mut machine = Machine::new(addr, auto_reconnect);
    let mut due = Instant::now();
    let mut last_sent: Option<Status> = None;
    loop {
        let now = Instant::now();
        if now >= due {
            publish(statuses, &mut last_sent, machine.start(), &wake);
            let result = probe(&machine.addr);
            let status = machine.probed(result);
            let wait = match status.phase {
                Phase::Online => HEARTBEAT,
                _ => status.retry_in.unwrap_or(IDLE_SLICE),
            };
            due = Instant::now() + wait;
            publish(statuses, &mut last_sent, status, &wake);
        }
        let timeout = due
            .saturating_duration_since(Instant::now())
            .min(IDLE_SLICE);
        match commands.recv_timeout(timeout.max(Duration::from_millis(1))) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Command::RetryNow) => due = Instant::now(),
            Ok(Command::SetAuto(enabled)) => {
                machine.auto_reconnect = enabled;
                if enabled {
                    due = Instant::now();
                }
            }
            Ok(Command::SetServer(addr)) => {
                machine.addr = addr;
                machine.attempts = 0;
                due = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn publish(statuses: &Sender<Status>, last: &mut Option<Status>, status: Status, wake: &Waker) {
    if last.as_ref() != Some(&status) && statuses.send(status.clone()).is_ok() {
        *last = Some(status);
        wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_in_every_shape_people_type() {
        let cases = [
            ("desktop.lufria.tech", "desktop.lufria.tech", 443),
            ("desktop.lufria.tech:8720", "desktop.lufria.tech", 8720),
            ("https://desktop.lufria.tech/", "desktop.lufria.tech", 443),
            (
                "  https://desktop.lufria.tech:8721/login?next=/  ",
                "desktop.lufria.tech",
                8721,
            ),
            ("192.0.2.10:8720", "192.0.2.10", 8720),
            ("[::1]:8720", "::1", 8720),
        ];
        for (input, host, port) in cases {
            let addr = ServerAddr::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!((addr.host.as_str(), addr.port), (host, port), "{input}");
        }
    }

    #[test]
    fn bad_addresses_are_rejected_with_a_reason() {
        for input in [
            "",
            "   ",
            ":443",
            "host:notaport",
            "host:0",
            "host name",
            "2001:db8::1",
        ] {
            assert!(
                ServerAddr::parse(input).is_err(),
                "`{input}` should be rejected"
            );
        }
    }

    #[test]
    fn retry_backoff_grows_then_levels_off() {
        let secs: Vec<u64> = (1..=9).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 15, 30, 30, 30, 30]);
    }

    #[test]
    fn a_failed_probe_schedules_a_retry_and_a_success_clears_it() {
        let addr = ServerAddr::parse("example.test:443").unwrap();
        let mut machine = Machine::new(addr, true);
        let first = machine.probed(Err("connection refused".to_owned()));
        assert_eq!(first.phase, Phase::Offline);
        assert_eq!(first.attempts, 1);
        assert_eq!(first.retry_in, Some(Duration::from_secs(1)));
        let second = machine.probed(Err("connection refused".to_owned()));
        assert_eq!(second.retry_in, Some(Duration::from_secs(2)));
        let online = machine.probed(Ok(()));
        assert_eq!(online, Status::online());
        assert_eq!(machine.attempts, 0);
    }

    #[test]
    fn without_automatic_reconnect_a_failure_waits_for_the_user() {
        let addr = ServerAddr::parse("example.test").unwrap();
        let mut machine = Machine::new(addr, false);
        let status = machine.probed(Err("timed out".to_owned()));
        assert_eq!(status.phase, Phase::Offline);
        assert_eq!(status.retry_in, None);
        assert_eq!(status.attempts, 1);
    }

    #[test]
    fn the_prober_reports_offline_then_online_and_stops_on_command() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        static SERVER_UP: AtomicBool = AtomicBool::new(false);

        fn fake_probe(_addr: &ServerAddr) -> Result<(), String> {
            if SERVER_UP.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("connection refused".to_owned())
            }
        }

        let addr = ServerAddr::parse("example.test:443").unwrap();
        let (commands, command_rx) = mpsc::channel();
        let (statuses, status_rx) = mpsc::channel();
        let wake = Arc::new(|| {}) as Waker;
        let handle = thread::spawn(move || {
            worker(addr, true, &command_rx, &statuses, wake, fake_probe);
        });

        let first = status_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.phase, Phase::Connecting);
        let offline = status_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(offline.phase, Phase::Offline);
        assert_eq!(offline.attempts, 1);

        SERVER_UP.store(true, Ordering::SeqCst);
        commands.send(Command::RetryNow).unwrap();
        let online = loop {
            let status = status_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if status.phase == Phase::Online {
                break status;
            }
        };
        assert_eq!(online, Status::online());

        commands.send(Command::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_real_probe_succeeds_on_a_listening_port_and_fails_on_a_closed_one() {
        // Loopback only: no external network is involved.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local address").port();
        let addr = ServerAddr::parse(&format!("127.0.0.1:{port}")).expect("parse the address");
        assert_eq!(probe(&addr), Ok(()), "a listening port is reachable");
        drop(listener);
        assert!(probe(&addr).is_err(), "a closed port is unreachable");
    }
}
