//! Kernel-level restriction of the loopback Xpra display ports.
//!
//! Xpra's HTML5 transport listens on `127.0.0.1:<ephemeral>` and is reached by
//! the unprivileged RAST server, which proxies every browser request after
//! authenticating it. Loopback is **not** a security boundary between local
//! uids: another unprivileged account on the host could connect to the port
//! and drive that person's graphical session. To close that hole without
//! teaching Xpra's HTML5 client a shared secret (which would have to appear in
//! a process command line readable by every local user), the helper owns one
//! nftables table:
//!
//! ```text
//! table inet rast_native {
//!   set xpra_ports { type inet_service; }
//!   chain output {
//!     type filter hook output priority 0; policy accept;
//!     ip daddr 127.0.0.1 tcp dport @xpra_ports \
//!        meta skuid != <rast uid> reject with tcp reset
//!   }
//! }
//! ```
//!
//! Every session port is added to the set before the session is usable and
//! removed when the session stops. The set is reconciled against the live
//! `rast-xpra-*` units on helper start, so sessions that survived a helper
//! restart stay protected and ports of dead sessions never leak.
//!
//! The helper never trusts a table it finds. At startup and before every
//! session port is added it reads the table back (`nft -j list table inet
//! rast_native`) and accepts it only when it is exactly the definition above
//! for the uid this helper exempts. Anything else (another uid, an extra rule
//! or object, a missing reject, another hook, priority or policy, table flags
//! such as `dormant`, a listing it cannot read) is replaced in **one** `nft -f`
//! transaction that also carries the ports of the live sessions, so there is
//! no moment without the reject rule or with a live port missing from the
//! set. The case this guards: a table left by an earlier helper run with
//! another `RAST_NATIVE_ALLOWED_UID` resets every connection of the RAST
//! server and lets that other uid reach every display port.
//!
//! The rule is defence in depth: the display port itself still has no
//! application-level authentication, but it is unreachable for every local
//! account except the authenticated gateway (the `rast` service account).

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        OnceLock,
    },
    time::Duration,
};

use serde_json::{json, Map, Value};

use super::{is_root, protocol, run_command, CommandOutput, NativeError, NativeOptions};

/// nftables table owned by RAST (never flushes anything else).
const TABLE: &str = "rast_native";
const CHAIN: &str = "output";
const SET: &str = "xpra_ports";
const NFT_CANDIDATES: &[&str] = &[
    "/usr/sbin/nft",
    "/sbin/nft",
    "/usr/bin/nft",
    "/usr/local/sbin/nft",
];
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: usize = 64 * 1024;
/// Bytes of `nft -j list table` output read. A listing that reaches the cap
/// is never accepted (it may be cut short): the table is replaced.
const MAX_LISTING: usize = 256 * 1024;
/// Characters of a mismatch reason kept in logs and diagnostics.
const MAX_REASON: usize = 400;
/// nft prints an equality or a set lookup as `==` (some releases print an
/// implicit one as `in`); both mean the same for the two positive matches.
const POSITIVE_OPS: &[&str] = &["==", "in"];

static NFT_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
/// True only in a production helper that successfully installed the rule.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The production helper's nftables access and the uid its rule exempts, set
/// once by [`init`].
static ENFORCEMENT: OnceLock<Enforcement> = OnceLock::new();
/// Serialises every read-and-change of the table. Without it a session start
/// could add its port to a table that a concurrent replacement then drops.
static TABLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Makes the names of concurrent ruleset files unique within the process.
static SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

struct Enforcement {
    host: HostNft,
    allowed_uid: u32,
}

/// Absolute path of the nftables binary, resolved once.
pub(crate) fn nft_binary() -> Option<PathBuf> {
    NFT_BIN
        .get_or_init(|| {
            NFT_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|candidate| candidate.is_file())
        })
        .clone()
}

/// The Xpra display-port restriction is actually installed (production only).
pub(crate) fn enforced() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// What the firewall code needs from the host: `nft` and the display ports of
/// the live sessions. Production uses [`HostNft`]; the unit tests use a fake,
/// so no test runs `nft` or reads systemd.
pub(crate) trait NftHost {
    /// Run `nft` with an argument vector (no shell).
    async fn run(&self, args: &[&str]) -> Result<CommandOutput, NativeError>;
    /// Run one ruleset with `nft -f`: nftables applies a file as one
    /// transaction (all of it or nothing).
    async fn apply(&self, script: &str) -> Result<CommandOutput, NativeError>;
    /// Display ports of the running `rast-xpra-*` units.
    async fn live_ports(&self) -> Vec<u16>;
}

/// The real `nft` binary. Ruleset files are written to `script_dir`, which
/// only root can write (`/run/rast-linux`, mode 0755 root:root).
pub(crate) struct HostNft {
    binary: PathBuf,
    script_dir: PathBuf,
}

impl HostNft {
    pub(crate) fn new(binary: PathBuf, script_dir: PathBuf) -> Self {
        Self { binary, script_dir }
    }
}

impl NftHost for HostNft {
    async fn run(&self, args: &[&str]) -> Result<CommandOutput, NativeError> {
        // The C locale keeps nft's "No such file or directory" readable.
        run_command(
            &self.binary,
            args.iter().map(std::ffi::OsString::from).collect(),
            &[("LC_ALL", "C")],
            COMMAND_TIMEOUT,
            MAX_LISTING,
            MAX_OUTPUT,
        )
        .await
    }

    async fn apply(&self, script: &str) -> Result<CommandOutput, NativeError> {
        let path = self.script_dir.join(format!(
            ".rast-display-firewall-{}-{}.nft",
            std::process::id(),
            SCRIPT_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        write_private(&path, script.as_bytes())?;
        let result = self.run(&["-f", &path.to_string_lossy()]).await;
        let _ = std::fs::remove_file(&path);
        result
    }

    async fn live_ports(&self) -> Vec<u16> {
        let mut ports = Vec::new();
        for unit in unit_list().await {
            if let Some(port) = session_port(&unit).await {
                ports.push(port);
            }
        }
        ports
    }
}

/// The text of the one rule of the chain.
fn rule_text(allowed_uid: u32) -> String {
    format!("ip daddr 127.0.0.1 tcp dport @{SET} meta skuid != {allowed_uid} reject with tcp reset")
}

/// `{ 18457, 41207 }` element list, sorted and without duplicates.
fn element_list(ports: &[u16]) -> Option<String> {
    let mut ports = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        return None;
    }
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{{ {list} }}"))
}

/// One `nft -f` transaction that drops whatever table `inet rast_native`
/// holds and defines it again, with the given ports already in the set. The
/// first `add table` makes the `delete` valid when the table does not exist
/// yet. Nothing outside the owned table is named.
fn replacement_script(allowed_uid: u32, ports: &[u16]) -> String {
    let mut script = format!(
        "add table inet {TABLE}\n\
         delete table inet {TABLE}\n\
         add table inet {TABLE}\n\
         add chain inet {TABLE} {CHAIN} {{ type filter hook output priority 0; policy accept; }}\n\
         add set inet {TABLE} {SET} {{ type inet_service; }}\n\
         add rule inet {TABLE} {CHAIN} {}\n",
        rule_text(allowed_uid)
    );
    if let Some(elements) = element_list(ports) {
        script.push_str(&format!("add element inet {TABLE} {SET} {elements}\n"));
    }
    script
}

/// One `nft -f` transaction that makes the set hold exactly `ports`.
fn reconcile_script(ports: &[u16]) -> String {
    let mut script = format!("flush set inet {TABLE} {SET}\n");
    if let Some(elements) = element_list(ports) {
        script.push_str(&format!("add element inet {TABLE} {SET} {elements}\n"));
    }
    script
}

/// What `nft -j list table inet rast_native` showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableState {
    /// Exactly the expected table, chain, set and rule for the allowed uid.
    Expected,
    /// The table does not exist.
    Missing,
    /// The table exists but differs. `installed_uid` is the uid its reject
    /// rule exempts, when it has such a rule.
    Mismatch {
        reason: String,
        installed_uid: Option<u32>,
    },
    /// `nft` failed, or printed something that is not a listing.
    Unreadable(String),
}

impl TableState {
    /// The `ping` field `displayFirewall` for this state.
    fn to_json(&self, allowed_uid: u32) -> Value {
        match self {
            Self::Expected => json!({"state": "ok", "allowedUid": allowed_uid}),
            Self::Missing => json!({"state": "missing", "allowedUid": allowed_uid}),
            Self::Mismatch {
                reason,
                installed_uid,
            } => json!({
                "state": "mismatch",
                "allowedUid": allowed_uid,
                "installedUid": installed_uid,
                "detail": reason,
            }),
            Self::Unreadable(reason) => json!({
                "state": "unreadable",
                "allowedUid": allowed_uid,
                "detail": reason,
            }),
        }
    }

    /// The `rast doctor` / `rast native-helper --check` warning, if any.
    pub(crate) fn warning(&self, allowed_uid: u32) -> Option<String> {
        match self {
            Self::Expected => None,
            Self::Missing => Some(format!(
                "the Xpra display-port firewall (nftables table inet {TABLE}) is not installed: \
                 rast-native.service installs it when it starts and restores it before a session \
                 starts; if the service is running, restart it"
            )),
            Self::Mismatch {
                reason,
                installed_uid,
            } => Some(format!(
                "the nftables table inet {TABLE} does not match the Xpra display-port rule \
                 `{}` (its rule exempts uid {}, expected uid {allowed_uid}): {reason}. The RAST \
                 server may be refused by every display and another account may reach them; \
                 restart rast-native.service to replace the table (the helper also replaces it \
                 before the next session starts)",
                rule_text(allowed_uid),
                installed_uid.map_or_else(|| "none".to_string(), |uid| uid.to_string()),
            )),
            Self::Unreadable(reason) => Some(format!(
                "cannot verify the Xpra display-port firewall (nftables table inet {TABLE}): \
                 {reason}"
            )),
        }
    }
}

/// Read the owned table back and compare it with the expected definition.
async fn table_state<H: NftHost>(host: &H, allowed_uid: u32) -> TableState {
    match host.run(&["-j", "list", "table", "inet", TABLE]).await {
        Err(error) => {
            TableState::Unreadable(protocol::truncate_message(&error.message, MAX_REASON))
        }
        Ok(output) if output.success => check_listing(&output.stdout, allowed_uid),
        // nft reports a missing table as ENOENT.
        Ok(output) if output.stderr.contains("No such file or directory") => TableState::Missing,
        Ok(output) => TableState::Unreadable(protocol::truncate_message(
            &super::summarize_command(&output),
            MAX_REASON,
        )),
    }
}

/// Strict check of an `nft -j list table inet rast_native` listing: one
/// table, one chain, one set and one rule, each with only the fields nft
/// prints for the expected definition. `metainfo` and object handles are
/// ignored; set elements are not checked (the set is reconciled).
fn check_listing(text: &str, allowed_uid: u32) -> TableState {
    if text.len() >= MAX_LISTING {
        return TableState::Mismatch {
            reason: format!("the listing reaches the {MAX_LISTING}-byte read limit"),
            installed_uid: None,
        };
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return TableState::Unreadable(format!("nft printed invalid JSON: {error}"));
        }
    };
    let Some(objects) = only_key(&value, "nftables").and_then(Value::as_array) else {
        return TableState::Unreadable("nft printed JSON without an `nftables` list".to_string());
    };
    let mut problems = Vec::new();
    let (mut tables, mut chains, mut sets, mut rules) = (0usize, 0usize, 0usize, 0usize);
    let mut installed_uid = None;
    for object in objects {
        let Some((kind, body)) = single_entry(object) else {
            problems.push("an entry is not an object with one key".to_string());
            continue;
        };
        match kind {
            "metainfo" => {}
            "table" => {
                tables += 1;
                check_table(body, &mut problems);
            }
            "chain" => {
                chains += 1;
                check_chain(body, &mut problems);
            }
            "set" => {
                sets += 1;
                check_set(body, &mut problems);
            }
            "rule" => {
                rules += 1;
                let uid = check_rule(body, allowed_uid, &mut problems);
                installed_uid = installed_uid.or(uid);
            }
            other => problems.push(format!("unexpected `{}` object", short(other))),
        }
    }
    for (count, what) in [
        (tables, "table"),
        (chains, "chain"),
        (sets, "set"),
        (rules, "rule"),
    ] {
        if count != 1 {
            problems.push(format!("{count} {what} objects, expected 1"));
        }
    }
    if problems.is_empty() {
        TableState::Expected
    } else {
        TableState::Mismatch {
            reason: protocol::truncate_message(&problems.join("; "), MAX_REASON),
            installed_uid,
        }
    }
}

fn short(text: &str) -> String {
    protocol::truncate_message(text, 40)
}

/// `(key, value)` of an object with exactly one key.
fn single_entry(value: &Value) -> Option<(&str, &Value)> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.iter().next().map(|(key, value)| (key.as_str(), value))
}

/// The value of `{"<key>": value}`, when that is the object's only key.
fn only_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    single_entry(value)
        .filter(|(found, _)| *found == key)
        .map(|(_, value)| value)
}

/// The fields of a listing entry; a field outside `allowed` is a problem.
fn fields<'a>(
    body: &'a Value,
    allowed: &[&str],
    what: &str,
    problems: &mut Vec<String>,
) -> Option<&'a Map<String, Value>> {
    let Some(map) = body.as_object() else {
        problems.push(format!("the {what} entry is not an object"));
        return None;
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            problems.push(format!(
                "the {what} has an unexpected `{}` field",
                short(key)
            ));
        }
    }
    Some(map)
}

fn expect_str(
    map: &Map<String, Value>,
    key: &str,
    expected: &str,
    what: &str,
    problems: &mut Vec<String>,
) {
    if map.get(key).and_then(Value::as_str) != Some(expected) {
        problems.push(format!("the {what} {key} is not `{expected}`"));
    }
}

fn check_table(body: &Value, problems: &mut Vec<String>) {
    let Some(table) = fields(body, &["family", "name", "handle"], "table", problems) else {
        return;
    };
    expect_str(table, "family", "inet", "table", problems);
    expect_str(table, "name", TABLE, "table", problems);
}

fn check_chain(body: &Value, problems: &mut Vec<String>) {
    let allowed = [
        "family", "table", "name", "handle", "type", "hook", "prio", "policy",
    ];
    let Some(chain) = fields(body, &allowed, "chain", problems) else {
        return;
    };
    expect_str(chain, "family", "inet", "chain", problems);
    expect_str(chain, "table", TABLE, "chain", problems);
    expect_str(chain, "name", CHAIN, "chain", problems);
    expect_str(chain, "type", "filter", "chain", problems);
    expect_str(chain, "hook", "output", "chain", problems);
    expect_str(chain, "policy", "accept", "chain", problems);
    if chain.get("prio").and_then(Value::as_i64) != Some(0) {
        problems.push("the chain prio is not 0 (filter)".to_string());
    }
}

fn check_set(body: &Value, problems: &mut Vec<String>) {
    let allowed = ["family", "table", "name", "handle", "type", "elem"];
    let Some(set) = fields(body, &allowed, "set", problems) else {
        return;
    };
    expect_str(set, "family", "inet", "set", problems);
    expect_str(set, "table", TABLE, "set", problems);
    expect_str(set, "name", SET, "set", problems);
    expect_str(set, "type", "inet_service", "set", problems);
    if set.get("elem").is_some_and(|elem| !elem.is_array()) {
        problems.push("the set elements are not a list".to_string());
    }
}

/// Check the one rule; returns the uid it exempts, when it has a
/// `meta skuid` match.
fn check_rule(body: &Value, allowed_uid: u32, problems: &mut Vec<String>) -> Option<u32> {
    let allowed = ["family", "table", "chain", "handle", "expr"];
    let rule = fields(body, &allowed, "rule", problems)?;
    expect_str(rule, "family", "inet", "rule", problems);
    expect_str(rule, "table", TABLE, "rule", problems);
    expect_str(rule, "chain", CHAIN, "rule", problems);
    let expr = rule.get("expr");
    let installed = expr.and_then(exempt_uid);
    if !expr.is_some_and(|expr| rule_matches(expr, allowed_uid)) {
        match installed {
            Some(uid) if uid != allowed_uid => {
                problems.push(format!("the rule exempts uid {uid}, not {allowed_uid}"));
            }
            _ => problems.push(format!("the rule is not `{}`", rule_text(allowed_uid))),
        }
    }
    installed
}

fn skuid_left() -> Value {
    json!({"meta": {"key": "skuid"}})
}

/// The statements of `ip daddr 127.0.0.1 tcp dport @xpra_ports meta skuid !=
/// <uid> reject with tcp reset`, and nothing else.
fn rule_matches(expr: &Value, allowed_uid: u32) -> bool {
    let Some(items) = expr.as_array() else {
        return false;
    };
    let [daddr, dport, skuid, reject] = items.as_slice() else {
        return false;
    };
    is_match(
        daddr,
        POSITIVE_OPS,
        &json!({"payload": {"protocol": "ip", "field": "daddr"}}),
        &json!("127.0.0.1"),
    ) && is_match(
        dport,
        POSITIVE_OPS,
        &json!({"payload": {"protocol": "tcp", "field": "dport"}}),
        &json!(format!("@{SET}")),
    ) && is_match(skuid, &["!="], &skuid_left(), &json!(allowed_uid))
        && *reject == json!({"reject": {"type": "tcp reset"}})
}

fn is_match(item: &Value, ops: &[&str], left: &Value, right: &Value) -> bool {
    let Some(found) = only_key(item, "match").and_then(Value::as_object) else {
        return false;
    };
    found.len() == 3
        && found
            .get("op")
            .and_then(Value::as_str)
            .is_some_and(|op| ops.contains(&op))
        && found.get("left") == Some(left)
        && found.get("right") == Some(right)
}

/// The uid of the first `meta skuid` match of a rule.
fn exempt_uid(expr: &Value) -> Option<u32> {
    let skuid = skuid_left();
    expr.as_array()?.iter().find_map(|item| {
        let found = only_key(item, "match")?.as_object()?;
        if found.get("left")? != &skuid {
            return None;
        }
        u32::try_from(found.get("right")?.as_u64()?).ok()
    })
}

/// How [`ensure_ruleset`] left the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ensured {
    /// The table was already exactly the expected one; nothing changed.
    Verified,
    /// There was no table; it was created with the live ports.
    Installed,
    /// The table differed (or could not be read) and was replaced, with the
    /// live ports, in one transaction.
    Replaced,
}

/// Verify the owned table and replace it when it is not exactly the expected
/// definition for `allowed_uid`. A replacement carries the ports of the live
/// sessions plus `extra_ports` (a session that is starting). Callers hold
/// [`TABLE_LOCK`].
async fn ensure_ruleset<H: NftHost>(
    host: &H,
    allowed_uid: u32,
    extra_ports: &[u16],
) -> Result<Ensured, NativeError> {
    let state = table_state(host, allowed_uid).await;
    let outcome = match state {
        TableState::Expected => return Ok(Ensured::Verified),
        TableState::Missing => Ensured::Installed,
        TableState::Mismatch { .. } | TableState::Unreadable(_) => Ensured::Replaced,
    };
    let mut ports = host.live_ports().await;
    ports.extend_from_slice(extra_ports);
    let result = host.apply(&replacement_script(allowed_uid, &ports)).await?;
    if !result.success {
        return Err(NativeError::command_failed(format!(
            "could not install the Xpra display-port firewall rule: {}",
            super::summarize_command(&result)
        )));
    }
    match state {
        TableState::Missing | TableState::Expected => tracing::info!(
            allowed_uid,
            "restricted Xpra display ports to the RAST service account (nftables)"
        ),
        TableState::Mismatch {
            reason,
            installed_uid,
        } => {
            let old_uid = installed_uid.map_or_else(|| "none".to_string(), |uid| uid.to_string());
            tracing::warn!(
                old_uid = %old_uid,
                new_uid = allowed_uid,
                reason = %reason,
                "replaced the stale nftables table inet {TABLE}: its Xpra display-port rule \
                 exempted uid {old_uid}, now only uid {allowed_uid} (the RAST service account) \
                 may reach the display ports"
            );
        }
        TableState::Unreadable(reason) => tracing::warn!(
            new_uid = allowed_uid,
            reason = %reason,
            "replaced the nftables table inet {TABLE}, which could not be verified; now only uid \
             {allowed_uid} (the RAST service account) may reach the display ports"
        ),
    }
    Ok(outcome)
}

fn write_private(path: &Path, contents: &[u8]) -> Result<(), NativeError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            NativeError::unavailable(format!(
                "cannot write the display firewall ruleset {}: {error}",
                path.display()
            ))
        })?;
    file.write_all(contents).map_err(|error| {
        NativeError::unavailable(format!(
            "cannot write the display firewall ruleset {}: {error}",
            path.display()
        ))
    })
}

/// The port of a running `rast-xpra-*` unit, read from its Xpra command line.
pub(crate) async fn session_port(unit: &str) -> Option<u16> {
    let show = nft_systemctl(&["show", unit, "--property=MainPID", "--value"]).await?;
    let pid: u32 = show.trim().parse().ok()?;
    if pid == 0 {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    port_from_cmdline(&cmdline)
}

async fn nft_systemctl(args: &[&str]) -> Option<String> {
    let output = run_command(
        Path::new(super::SYSTEMCTL_BIN),
        args.iter().map(std::ffi::OsString::from).collect(),
        &[],
        COMMAND_TIMEOUT,
        MAX_OUTPUT,
        MAX_OUTPUT,
    )
    .await
    .ok()?;
    output.success.then_some(output.stdout)
}

/// Parse `--bind-tcp=127.0.0.1:<port>` out of an Xpra command line.
fn port_from_cmdline(cmdline: &[u8]) -> Option<u16> {
    for arg in cmdline.split(|byte| *byte == 0) {
        let Ok(text) = std::str::from_utf8(arg) else {
            continue;
        };
        let Some(value) = text.strip_prefix("--bind-tcp=") else {
            continue;
        };
        // Xpra accepts host:port and host:port:options; the port is the last
        // numeric field before the options when no options are set.
        let host_port = value.split(':').collect::<Vec<_>>();
        if host_port.len() < 2 {
            continue;
        }
        if let Some(port) = host_port
            .get(1)
            .and_then(|part| part.split(',').next())
            .and_then(|part| part.parse::<u16>().ok())
        {
            return Some(port);
        }
    }
    None
}

/// Whether a port must be added to the set.
///
/// Normal adds are gated on the rule being marked active (tests and
/// unprivileged runs must not touch nftables). The startup reconciliation is
/// the exception: it runs while `init` is still finishing, before `ACTIVE` is
/// set, and it must restore the ports of sessions that survived a helper
/// restart. Keeping the decision in one place keeps that ordering testable.
fn should_add_port(restoring: bool) -> bool {
    restoring || enforced()
}

/// Add one session port to the restricted set, unconditionally.
async fn add_port<H: NftHost>(host: &H, port: u16) -> Result<(), NativeError> {
    let port = port.to_string();
    let result = host
        .run(&["add", "element", "inet", TABLE, SET, "{", &port, "}"])
        .await?;
    if result.success {
        Ok(())
    } else {
        Err(NativeError::command_failed(format!(
            "could not restrict display port {port}: {}",
            super::summarize_command(&result)
        )))
    }
}

fn enforcement() -> Result<&'static Enforcement, NativeError> {
    ENFORCEMENT.get().ok_or_else(|| {
        NativeError::unavailable("the Xpra display-port firewall has not been initialised")
    })
}

/// Add one session port to the restricted set when the rule is enforced,
/// after checking (and if needed replacing) the table.
pub(crate) async fn allow_port(port: u16) -> Result<(), NativeError> {
    if !should_add_port(false) {
        return Ok(());
    }
    let enforcement = enforcement()?;
    let _guard = TABLE_LOCK.lock().await;
    allow_port_with(&enforcement.host, enforcement.allowed_uid, port).await
}

async fn allow_port_with<H: NftHost>(
    host: &H,
    allowed_uid: u32,
    port: u16,
) -> Result<(), NativeError> {
    ensure_ruleset(host, allowed_uid, &[port]).await?;
    // `add element` of a port already in the set is not an error.
    add_port(host, port).await
}

/// Remove one session port (best effort: the session is already gone).
pub(crate) async fn release_port(port: u16) {
    if !enforced() {
        return;
    }
    let Some(enforcement) = ENFORCEMENT.get() else {
        return;
    };
    let _guard = TABLE_LOCK.lock().await;
    let port_text = port.to_string();
    match enforcement
        .host
        .run(&[
            "delete", "element", "inet", TABLE, SET, "{", &port_text, "}",
        ])
        .await
    {
        Ok(output) if output.success => {}
        Ok(output) => tracing::debug!(
            port,
            "display port was not in the restricted set: {}",
            super::summarize_command(&output)
        ),
        Err(error) => tracing::debug!(port, "could not release display port: {error}"),
    }
}

/// Install (or verify and repair) the rule and reconcile the port set with
/// the live sessions.
pub(crate) async fn init(options: &NativeOptions) -> Result<(), NativeError> {
    let production = options.require_root && is_root();
    let Some(binary) = nft_binary() else {
        if production {
            // Fail closed: an Xpra display port that other local accounts can
            // reach is never started. The operator gets an actionable error
            // instead of a silently unprotected native session.
            return Err(NativeError::unavailable(
                "nftables (nft) is required to restrict Xpra display ports to the RAST service \
                 account, but it is not installed. Install the 'nftables' package and restart \
                 rast-native.service (see docs/INSTALL.md, section 'Native Linux apps (Xpra)'), \
                 or disable native apps with native_enabled = false.",
            ));
        }
        return Ok(());
    };
    if !production {
        // Tests and unprivileged runs cannot (and must not) install rules.
        return Ok(());
    }
    let Some(allowed_uid) = options.allowed_peer_uid.or(options.rast_uid) else {
        // Fail closed for the same reason: without the gateway uid there is no
        // way to restrict the ports, so no session may start.
        return Err(NativeError::unavailable(
            "the RAST service account uid could not be resolved, so Xpra display ports cannot \
             be restricted to it. Check that the 'rast' user and group exist and restart \
             rast-native.service, or disable native apps with native_enabled = false.",
        ));
    };
    let host = HostNft::new(binary, options.run_dir.clone());
    {
        let _guard = TABLE_LOCK.lock().await;
        startup(&host, allowed_uid).await?;
    }
    // `init` runs once per helper process.
    let _ = ENFORCEMENT.set(Enforcement { host, allowed_uid });
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Startup: verify the table (replacing a stale one), then make the set match
/// the live sessions. A replacement already carries the live ports.
async fn startup<H: NftHost>(host: &H, allowed_uid: u32) -> Result<Ensured, NativeError> {
    let outcome = ensure_ruleset(host, allowed_uid, &[]).await?;
    if outcome == Ensured::Verified {
        reconcile(host).await;
    }
    Ok(outcome)
}

/// Make the set match the live `rast-xpra-*` units exactly, in one
/// transaction (flush and re-add), so the port of a session that survived a
/// helper restart is never missing from the set. A failure keeps the old set
/// and is logged.
async fn reconcile<H: NftHost>(host: &H) {
    // Runs before `ACTIVE` is set: restoring is never gated on it.
    let ports = if should_add_port(true) {
        host.live_ports().await
    } else {
        Vec::new()
    };
    match host.apply(&reconcile_script(&ports)).await {
        Ok(output) if output.success => {}
        Ok(output) => tracing::warn!(
            ports = ?ports,
            "could not restore display-port protection: {}",
            super::summarize_command(&output)
        ),
        Err(error) => tracing::warn!(
            ports = ?ports,
            "could not restore display-port protection: {error}"
        ),
    }
}

async fn unit_list() -> Vec<String> {
    let Some(output) = nft_systemctl(&[
        "list-units",
        "--type=service",
        "--state=running",
        "--no-legend",
        "--plain",
        "rast-xpra-*.service",
    ])
    .await
    else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("rast-xpra-"))
        .map(str::to_string)
        .collect()
}

/// The `displayFirewall` field of the helper's `ping`: the owned table as
/// the running helper sees it. Read-only: a mismatch is repaired at the next
/// session start or helper start.
pub(crate) async fn ping_status() -> Value {
    if !enforced() {
        return json!({"state": "inactive"});
    }
    let Some(enforcement) = ENFORCEMENT.get() else {
        return json!({"state": "inactive"});
    };
    let _guard = TABLE_LOCK.lock().await;
    table_state(&enforcement.host, enforcement.allowed_uid)
        .await
        .to_json(enforcement.allowed_uid)
}

/// `rast native-helper --check` (as root): a warning when the installed table
/// is missing or is not the expected rule. Read-only.
pub(crate) async fn check_installed(options: &NativeOptions) -> Option<String> {
    if !is_root() {
        // Listing nftables needs CAP_NET_ADMIN; the check already reports
        // that it must run as root.
        return None;
    }
    let binary = nft_binary()?;
    let allowed_uid = options.allowed_peer_uid.or(options.rast_uid)?;
    let host = HostNft::new(binary, options.run_dir.clone());
    table_state(&host, allowed_uid).await.warning(allowed_uid)
}

/// `rast doctor`: read the `displayFirewall` field of the helper's `ping`.
/// `Ok` is an information line, `Err` a warning. `None` when the helper does
/// not report the field (an older helper).
pub(crate) fn doctor_status(value: Option<&Value>) -> Option<Result<String, String>> {
    let value = value?;
    let Some(state) = value.get("state").and_then(Value::as_str) else {
        return Some(Err(
            "the native helper reported its display firewall without a state".to_string(),
        ));
    };
    let uid = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|uid| u32::try_from(uid).ok())
    };
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .map(|detail| protocol::truncate_message(detail, MAX_REASON))
        .unwrap_or_default();
    let table = match state {
        "ok" => TableState::Expected,
        "missing" => TableState::Missing,
        "mismatch" => TableState::Mismatch {
            reason: detail,
            installed_uid: uid("installedUid"),
        },
        "unreadable" => TableState::Unreadable(detail),
        "inactive" => {
            return Some(Err(
                "the native helper does not enforce the Xpra display-port firewall".to_string(),
            ));
        }
        other => {
            return Some(Err(format!(
                "the native helper reported an unknown display firewall state `{}`",
                short(other)
            )));
        }
    };
    let Some(allowed_uid) = uid("allowedUid") else {
        return Some(Err(
            "the native helper did not report the uid its display firewall exempts".to_string(),
        ));
    };
    Some(match table.warning(allowed_uid) {
        None => Ok(format!(
            "        display firewall rule: verified by the helper (only uid {allowed_uid} may \
             connect to the display ports)"
        )),
        Some(warning) => Err(warning),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Uid of the RAST service account in these tests, and the uid a stale
    /// table from an earlier helper run exempted (the production case).
    const RAST_UID: u32 = 993;
    const OTHER_UID: u32 = 1000;

    #[test]
    fn parses_the_display_port_from_an_xpra_command_line() {
        let cmdline = b"/usr/bin/python3\0/usr/bin/xpra\0start\0:100\0--bind-tcp=127.0.0.1:41207\0--html=on\0";
        assert_eq!(port_from_cmdline(cmdline), Some(41207));

        let with_options = b"/usr/bin/xpra\0--bind-tcp=127.0.0.1:18457,debug\0--html=on\0";
        assert_eq!(port_from_cmdline(with_options), Some(18457));

        assert_eq!(port_from_cmdline(b"/usr/bin/xpra\0--html=on\0"), None);
        assert_eq!(
            port_from_cmdline(b"--bind-tcp=127.0.0.1:not-a-port\0"),
            None
        );
        assert_eq!(port_from_cmdline(b"--bind-tcp=]::1]:14500\0"), None);
    }

    /// Regression: `init()` reconciles the set while `ACTIVE` is still false.
    /// Reusing the gated `allow_port()` there silently emptied the set on every
    /// helper restart, leaving the Xpra ports of sessions that survived the
    /// restart reachable by other local accounts.
    #[test]
    fn startup_reconciliation_restores_ports_before_the_rule_is_active() {
        ACTIVE.store(false, Ordering::Relaxed);
        assert!(!enforced());
        assert!(
            !should_add_port(false),
            "normal adds stay gated until init has validated the rule"
        );
        assert!(
            should_add_port(true),
            "startup reconciliation must restore discovered ports"
        );
    }

    /// The listing `nft -j list table inet rast_native` prints for the
    /// expected table (nftables 1.x JSON).
    fn listing(uid: u32) -> Value {
        json!({"nftables": [
            {"metainfo": {"version": "1.1.3", "release_name": "example", "json_schema_version": 1}},
            {"table": {"family": "inet", "name": "rast_native", "handle": 7}},
            {"set": {"family": "inet", "name": "xpra_ports", "table": "rast_native",
                     "type": "inet_service", "handle": 1, "elem": [41207, 18457]}},
            {"chain": {"family": "inet", "table": "rast_native", "name": "output", "handle": 2,
                       "type": "filter", "hook": "output", "prio": 0, "policy": "accept"}},
            {"rule": {"family": "inet", "table": "rast_native", "chain": "output", "handle": 3,
                      "expr": [
                {"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "daddr"}},
                           "right": "127.0.0.1"}},
                {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}},
                           "right": "@xpra_ports"}},
                {"match": {"op": "!=", "left": {"meta": {"key": "skuid"}}, "right": uid}},
                {"reject": {"type": "tcp reset"}}
            ]}}
        ]})
    }

    /// The body of the first `kind` object of a listing.
    fn entry<'a>(listing: &'a mut Value, kind: &str) -> &'a mut Value {
        listing["nftables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find_map(|object| object.get_mut(kind))
            .unwrap()
    }

    fn check(listing: &Value) -> TableState {
        check_listing(&serde_json::to_string(listing).unwrap(), RAST_UID)
    }

    fn assert_mismatch(state: &TableState, needle: &str) {
        match state {
            TableState::Mismatch { reason, .. } => assert!(
                reason.contains(needle),
                "reason {reason:?} does not mention {needle:?}"
            ),
            other => panic!("expected a mismatch about {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn accepts_exactly_the_expected_table() {
        assert_eq!(check(&listing(RAST_UID)), TableState::Expected);

        // `in` for the positive matches, no metainfo, an empty set.
        let mut other_print = listing(RAST_UID);
        let expr = entry(&mut other_print, "rule")["expr"]
            .as_array_mut()
            .unwrap();
        expr[0]["match"]["op"] = json!("in");
        expr[1]["match"]["op"] = json!("in");
        entry(&mut other_print, "set")
            .as_object_mut()
            .unwrap()
            .remove("elem");
        other_print["nftables"].as_array_mut().unwrap().remove(0);
        assert_eq!(check(&other_print), TableState::Expected);
    }

    /// The production case: a table left by a helper run that exempted uid
    /// 1000, while this helper exempts the `rast` uid.
    #[test]
    fn a_table_that_exempts_another_uid_is_a_mismatch() {
        let state = check(&listing(OTHER_UID));
        assert_eq!(
            state,
            TableState::Mismatch {
                reason: format!("the rule exempts uid {OTHER_UID}, not {RAST_UID}"),
                installed_uid: Some(OTHER_UID),
            }
        );
        let warning = state.warning(RAST_UID).unwrap();
        assert!(
            warning.contains("exempts uid 1000, expected uid 993"),
            "{warning}"
        );
    }

    #[test]
    fn an_extra_rule_or_object_is_a_mismatch() {
        let mut extra_rule = listing(RAST_UID);
        let rule = entry(&mut extra_rule, "rule").clone();
        let mut accept = rule.clone();
        accept["expr"] = json!([{"accept": null}]);
        extra_rule["nftables"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "rule": accept }));
        assert_mismatch(&check(&extra_rule), "2 rule objects, expected 1");

        let mut twice = listing(RAST_UID);
        twice["nftables"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "rule": rule }));
        assert_mismatch(&check(&twice), "2 rule objects, expected 1");

        let mut extra_chain = listing(RAST_UID);
        let mut input = entry(&mut extra_chain, "chain").clone();
        input["name"] = json!("input");
        input["hook"] = json!("input");
        extra_chain["nftables"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "chain": input }));
        assert_mismatch(&check(&extra_chain), "2 chain objects, expected 1");

        let mut map = listing(RAST_UID);
        map["nftables"]
            .as_array_mut()
            .unwrap()
            .push(json!({"map": {"family": "inet", "name": "m", "table": "rast_native"}}));
        assert_mismatch(&check(&map), "unexpected `map` object");

        let mut counter = listing(RAST_UID);
        entry(&mut counter, "rule")["expr"]
            .as_array_mut()
            .unwrap()
            .insert(3, json!({"counter": {"packets": 0, "bytes": 0}}));
        assert_mismatch(&check(&counter), "the rule is not `ip daddr 127.0.0.1");
    }

    #[test]
    fn a_missing_reject_is_a_mismatch() {
        let mut no_reject = listing(RAST_UID);
        entry(&mut no_reject, "rule")["expr"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_mismatch(&check(&no_reject), "the rule is not");

        let mut accept = listing(RAST_UID);
        entry(&mut accept, "rule")["expr"][3] = json!({"accept": null});
        assert_mismatch(&check(&accept), "the rule is not");

        let mut no_rule = listing(RAST_UID);
        no_rule["nftables"]
            .as_array_mut()
            .unwrap()
            .retain(|object| object.get("rule").is_none());
        assert_mismatch(&check(&no_rule), "0 rule objects, expected 1");

        let mut no_uid_match = listing(RAST_UID);
        entry(&mut no_uid_match, "rule")["expr"]
            .as_array_mut()
            .unwrap()
            .remove(2);
        assert_eq!(
            check(&no_uid_match),
            TableState::Mismatch {
                reason: format!("the rule is not `{}`", rule_text(RAST_UID)),
                installed_uid: None,
            }
        );
    }

    #[test]
    fn a_wrong_chain_policy_hook_or_priority_is_a_mismatch() {
        for (key, value, needle) in [
            ("policy", json!("drop"), "the chain policy is not `accept`"),
            ("hook", json!("input"), "the chain hook is not `output`"),
            ("prio", json!(10), "the chain prio is not 0"),
            ("type", json!("nat"), "the chain type is not `filter`"),
        ] {
            let mut changed = listing(RAST_UID);
            entry(&mut changed, "chain")[key] = value;
            assert_mismatch(&check(&changed), needle);
        }
    }

    #[test]
    fn table_flags_set_changes_and_other_names_are_a_mismatch() {
        let mut dormant = listing(RAST_UID);
        entry(&mut dormant, "table")["flags"] = json!(["dormant"]);
        assert_mismatch(
            &check(&dormant),
            "the table has an unexpected `flags` field",
        );

        let mut timeout = listing(RAST_UID);
        entry(&mut timeout, "set")["flags"] = json!(["timeout"]);
        assert_mismatch(&check(&timeout), "the set has an unexpected `flags` field");

        let mut set_type = listing(RAST_UID);
        entry(&mut set_type, "set")["type"] = json!("ipv4_addr");
        assert_mismatch(&check(&set_type), "the set type is not `inet_service`");

        let mut comment = listing(RAST_UID);
        entry(&mut comment, "rule")["comment"] = json!("allow");
        assert_mismatch(
            &check(&comment),
            "the rule has an unexpected `comment` field",
        );

        let mut other_address = listing(RAST_UID);
        entry(&mut other_address, "rule")["expr"][0]["match"]["right"] = json!("127.0.0.2");
        assert_mismatch(&check(&other_address), "the rule is not");

        let mut other_set = listing(RAST_UID);
        entry(&mut other_set, "rule")["expr"][1]["match"]["right"] = json!("@other");
        assert_mismatch(&check(&other_set), "the rule is not");
    }

    #[test]
    fn unreadable_or_oversized_listings_are_never_accepted() {
        assert!(matches!(
            check_listing("not json", RAST_UID),
            TableState::Unreadable(_)
        ));
        assert!(matches!(
            check_listing(r#"{"tables": []}"#, RAST_UID),
            TableState::Unreadable(_)
        ));
        let mut padded = serde_json::to_string(&listing(RAST_UID)).unwrap();
        padded.push_str(&" ".repeat(MAX_LISTING));
        assert_mismatch(&check_listing(&padded, RAST_UID), "read limit");
    }

    #[test]
    fn the_replacement_is_one_transaction_with_the_full_table() {
        assert_eq!(
            replacement_script(RAST_UID, &[41207, 18457, 41207]),
            "add table inet rast_native\n\
             delete table inet rast_native\n\
             add table inet rast_native\n\
             add chain inet rast_native output { type filter hook output priority 0; policy accept; }\n\
             add set inet rast_native xpra_ports { type inet_service; }\n\
             add rule inet rast_native output ip daddr 127.0.0.1 tcp dport @xpra_ports meta skuid != 993 reject with tcp reset\n\
             add element inet rast_native xpra_ports { 18457, 41207 }\n"
        );
        let empty = replacement_script(RAST_UID, &[]);
        assert!(!empty.contains("add element"));
        assert_eq!(empty.matches("delete table").count(), 1);
        assert!(!empty.contains("flush ruleset"));

        assert_eq!(
            reconcile_script(&[41207]),
            "flush set inet rast_native xpra_ports\n\
             add element inet rast_native xpra_ports { 41207 }\n"
        );
        assert_eq!(
            reconcile_script(&[]),
            "flush set inet rast_native xpra_ports\n"
        );
    }

    /// Fake host: answers `-j list table` with a canned result, records
    /// every call and every ruleset it is asked to apply.
    struct FakeNft {
        listing: CommandOutput,
        apply_success: bool,
        ports: Vec<u16>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeNft {
        fn with_listing(listing: &Value) -> Self {
            Self::new(CommandOutput {
                success: true,
                stdout: serde_json::to_string(listing).unwrap(),
                stderr: String::new(),
            })
        }

        fn missing() -> Self {
            Self::new(CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "Error: No such file or directory\nlist table inet rast_native\n"
                    .to_string(),
            })
        }

        fn new(listing: CommandOutput) -> Self {
            Self {
                listing,
                apply_success: true,
                ports: vec![41207],
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl NftHost for FakeNft {
        async fn run(&self, args: &[&str]) -> Result<CommandOutput, NativeError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("run {}", args.join(" ")));
            if args.first() == Some(&"-j") {
                return Ok(self.listing.clone());
            }
            Ok(CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn apply(&self, script: &str) -> Result<CommandOutput, NativeError> {
            self.calls.lock().unwrap().push(format!("apply {script}"));
            Ok(CommandOutput {
                success: self.apply_success,
                stdout: String::new(),
                stderr: if self.apply_success {
                    String::new()
                } else {
                    "Error: Operation not permitted".to_string()
                },
            })
        }

        async fn live_ports(&self) -> Vec<u16> {
            self.calls.lock().unwrap().push("live_ports".to_string());
            self.ports.clone()
        }
    }

    const LIST: &str = "run -j list table inet rast_native";

    #[tokio::test]
    async fn an_expected_table_is_left_alone() {
        let host = FakeNft::with_listing(&listing(RAST_UID));
        assert_eq!(
            ensure_ruleset(&host, RAST_UID, &[]).await.unwrap(),
            Ensured::Verified
        );
        assert_eq!(host.calls(), vec![LIST.to_string()]);
    }

    #[tokio::test]
    async fn a_stale_table_is_replaced_in_one_transaction_with_the_live_ports() {
        let host = FakeNft::with_listing(&listing(OTHER_UID));
        assert_eq!(
            ensure_ruleset(&host, RAST_UID, &[18457]).await.unwrap(),
            Ensured::Replaced
        );
        assert_eq!(
            host.calls(),
            vec![
                LIST.to_string(),
                "live_ports".to_string(),
                format!("apply {}", replacement_script(RAST_UID, &[41207, 18457])),
            ]
        );
    }

    #[tokio::test]
    async fn a_missing_or_unreadable_table_is_installed() {
        let host = FakeNft::missing();
        assert_eq!(
            ensure_ruleset(&host, RAST_UID, &[]).await.unwrap(),
            Ensured::Installed
        );
        assert_eq!(
            host.calls().last().unwrap(),
            &format!("apply {}", replacement_script(RAST_UID, &[41207]))
        );

        let host = FakeNft::new(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "Error: JSON support not compiled-in".to_string(),
        });
        assert_eq!(
            ensure_ruleset(&host, RAST_UID, &[]).await.unwrap(),
            Ensured::Replaced
        );
        assert_eq!(host.calls().len(), 3);
    }

    #[tokio::test]
    async fn a_failed_replacement_fails_closed() {
        let mut host = FakeNft::with_listing(&listing(OTHER_UID));
        host.apply_success = false;
        let error = ensure_ruleset(&host, RAST_UID, &[]).await.unwrap_err();
        assert!(error.message.contains("Operation not permitted"), "{error}");
        let error = allow_port_with(&host, RAST_UID, 18457).await.unwrap_err();
        assert!(error.message.contains("display-port firewall"), "{error}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|call| call.starts_with("run add element")),
            "no port is added to a table that could not be repaired"
        );
    }

    #[tokio::test]
    async fn a_session_start_repairs_a_stale_table_before_adding_its_port() {
        let host = FakeNft::with_listing(&listing(OTHER_UID));
        allow_port_with(&host, RAST_UID, 18457).await.unwrap();
        assert_eq!(
            host.calls(),
            vec![
                LIST.to_string(),
                "live_ports".to_string(),
                format!("apply {}", replacement_script(RAST_UID, &[18457, 41207])),
                "run add element inet rast_native xpra_ports { 18457 }".to_string(),
            ]
        );

        let host = FakeNft::with_listing(&listing(RAST_UID));
        allow_port_with(&host, RAST_UID, 18457).await.unwrap();
        assert_eq!(
            host.calls(),
            vec![
                LIST.to_string(),
                "run add element inet rast_native xpra_ports { 18457 }".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn startup_reconciles_a_verified_table_and_repairs_a_stale_one() {
        ACTIVE.store(false, Ordering::Relaxed);
        let host = FakeNft::with_listing(&listing(RAST_UID));
        assert_eq!(startup(&host, RAST_UID).await.unwrap(), Ensured::Verified);
        assert_eq!(
            host.calls(),
            vec![
                LIST.to_string(),
                "live_ports".to_string(),
                format!("apply {}", reconcile_script(&[41207])),
            ]
        );

        let host = FakeNft::with_listing(&listing(OTHER_UID));
        assert_eq!(startup(&host, RAST_UID).await.unwrap(), Ensured::Replaced);
        let applies = host
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("apply "))
            .collect::<Vec<_>>();
        assert_eq!(
            applies,
            vec![format!("apply {}", replacement_script(RAST_UID, &[41207]))],
            "the replacement already restored the live ports"
        );
    }

    /// The real host side: one `nft -f <file>` call, the file is 0600 in the
    /// script directory and removed afterwards.
    #[tokio::test]
    async fn the_host_applies_a_ruleset_with_one_nft_call() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("nft.log");
        let copy = dir.path().join("ruleset.copy");
        let nft = dir.path().join("nft");
        std::fs::write(
            &nft,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 if [ \"$1\" = -f ]; then cat \"$2\" > '{copy}'; stat -c %a \"$2\" >> '{log}'; fi\n",
                log = log.display(),
                copy = copy.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&nft, std::fs::Permissions::from_mode(0o755)).unwrap();
        let scripts = dir.path().join("run");
        std::fs::create_dir(&scripts).unwrap();
        let host = HostNft::new(nft, scripts.clone());
        let ruleset = replacement_script(RAST_UID, &[41207]);

        let output = host.apply(&ruleset).await.unwrap();
        assert!(output.success);
        let calls = std::fs::read_to_string(&log).unwrap();
        let calls = calls.lines().collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{calls:?}");
        let file = calls[0].strip_prefix("-f ").unwrap();
        assert!(Path::new(file).starts_with(&scripts), "{file}");
        assert_eq!(calls[1], "600");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), ruleset);
        assert_eq!(std::fs::read_dir(&scripts).unwrap().count(), 0);
    }

    #[test]
    fn doctor_reports_the_helper_view_of_the_table() {
        let ok = TableState::Expected.to_json(RAST_UID);
        let line = doctor_status(Some(&ok)).unwrap().unwrap();
        assert!(line.contains("only uid 993"), "{line}");

        let stale = TableState::Mismatch {
            reason: format!("the rule exempts uid {OTHER_UID}, not {RAST_UID}"),
            installed_uid: Some(OTHER_UID),
        }
        .to_json(RAST_UID);
        let warning = doctor_status(Some(&stale)).unwrap().unwrap_err();
        assert!(
            warning.contains("exempts uid 1000, expected uid 993"),
            "{warning}"
        );
        assert!(warning.contains("restart rast-native.service"), "{warning}");

        let missing = TableState::Missing.to_json(RAST_UID);
        assert!(doctor_status(Some(&missing)).unwrap().is_err());
        let unreadable = TableState::Unreadable("nft failed".into()).to_json(RAST_UID);
        assert!(doctor_status(Some(&unreadable))
            .unwrap()
            .unwrap_err()
            .contains("nft failed"));
        assert!(doctor_status(Some(&json!({"state": "inactive"})))
            .unwrap()
            .is_err());
        assert!(doctor_status(Some(&json!({"state": "ok"})))
            .unwrap()
            .is_err());
        assert!(doctor_status(Some(&json!({"state": "strange"})))
            .unwrap()
            .is_err());
        assert_eq!(doctor_status(None), None);
    }

    /// `sudo nft -j list table inet rast_native` as nftables 1.1.6 printed it
    /// on production (2026-11-17), byte for byte: the stale table of an
    /// earlier helper run that exempted uid 1000. The chain comes before the
    /// set and the empty set has no `elem`.
    const REAL_LISTING_1_1_6: &str = r#"{"nftables": [{"metainfo": {"version": "1.1.6", "release_name": "Commodore Bullmoose #7", "json_schema_version": 1}}, {"table": {"family": "inet", "name": "rast_native", "handle": 21}}, {"chain": {"family": "inet", "table": "rast_native", "name": "output", "handle": 1, "type": "filter", "hook": "output", "prio": 0, "policy": "accept"}}, {"set": {"family": "inet", "name": "xpra_ports", "table": "rast_native", "type": "inet_service", "handle": 2}}, {"rule": {"family": "inet", "table": "rast_native", "chain": "output", "handle": 3, "expr": [{"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "daddr"}}, "right": "127.0.0.1"}}, {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": "@xpra_ports"}}, {"match": {"op": "!=", "left": {"meta": {"key": "skuid"}}, "right": 1000}}, {"reject": {"type": "tcp reset"}}]}}]}"#;

    #[tokio::test]
    async fn the_real_nftables_1_1_6_listing_is_parsed_exactly() {
        assert_eq!(
            check_listing(REAL_LISTING_1_1_6, OTHER_UID),
            TableState::Expected,
            "the same table is correct for a helper that exempts uid 1000"
        );
        assert_eq!(
            check_listing(REAL_LISTING_1_1_6, RAST_UID),
            TableState::Mismatch {
                reason: format!("the rule exempts uid {OTHER_UID}, not {RAST_UID}"),
                installed_uid: Some(OTHER_UID),
            }
        );

        let host = FakeNft::new(CommandOutput {
            success: true,
            stdout: format!("{REAL_LISTING_1_1_6}\n"),
            stderr: String::new(),
        });
        assert_eq!(
            ensure_ruleset(&host, RAST_UID, &[]).await.unwrap(),
            Ensured::Replaced
        );
        assert_eq!(
            host.calls().last().unwrap(),
            &format!("apply {}", replacement_script(RAST_UID, &[41207]))
        );

        let host = FakeNft::new(CommandOutput {
            success: true,
            stdout: format!("{REAL_LISTING_1_1_6}\n"),
            stderr: String::new(),
        });
        assert_eq!(
            ensure_ruleset(&host, OTHER_UID, &[]).await.unwrap(),
            Ensured::Verified
        );
        assert_eq!(host.calls(), vec![LIST.to_string()]);
    }
}
