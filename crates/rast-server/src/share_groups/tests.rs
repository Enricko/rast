use super::*;

use crate::config::{Config, FileConfig};
use std::sync::{Arc, Mutex};

const TEAM: &str = "0123abcd-4567-4ef0-8123-456789abcdef";
const SALES: &str = "11111111-2222-4333-8444-555555555555";

fn test_config(dir: &std::path::Path) -> Config {
    Config::from_parts(
        None,
        FileConfig {
            data_dir: Some(dir.join("data")),
            storage_dir: Some(dir.join("storage")),
            monitor_interval_secs: Some(300),
            ..Default::default()
        },
        |_| None,
    )
    .expect("test config")
}

async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

/// Accounts: 1 admin, 2 legacy reader, 3 writer through a rule, 4 legacy
/// writer with an explicit deny, 5 deactivated writer, 6 writer without a
/// Linux identity, 7 no access at all.
async fn seed(pool: &SqlitePool) {
    for (id, name, role, active) in [
        (1, "alice", "admin", 1),
        (2, "bob", "user", 1),
        (3, "carol", "user", 1),
        (4, "dave", "user", 1),
        (5, "erin", "user", 0),
        (6, "frank", "user", 1),
        (7, "grace", "user", 1),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, active, created_at, updated_at)
             VALUES (?, ?, 'x', ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(role)
        .bind(active)
        .execute(pool)
        .await
        .unwrap();
        if id != 6 {
            sqlx::query(
                "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
                 VALUES (?, ?, ?, ?, '/nonexistent', 1)",
            )
            .bind(id)
            .bind(format!("rast-u{id}"))
            .bind(200_000 + id)
            .bind(200_000 + id)
            .execute(pool)
            .await
            .unwrap();
        }
    }
    for (id, name, container) in [(TEAM, "Team", "team"), (SALES, "Sales", "sales")] {
        sqlx::query(
            "INSERT INTO shares (id, name, container_path, created_at, updated_at)
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(container)
        .execute(pool)
        .await
        .unwrap();
    }
    exec(
        pool,
        &format!(
            "INSERT INTO share_members (share_id, user_id, perms) VALUES
             ('{TEAM}', 2, 'read'), ('{TEAM}', 4, 'write'), ('{TEAM}', 5, 'write'),
             ('{TEAM}', 6, 'write')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO user_folder_rules (user_id, share_id, rule, updated_at) VALUES
             (3, '{TEAM}', 'write', 1), (4, '{TEAM}', 'none', 1)"
        ),
    )
    .await;
}

#[tokio::test]
async fn membership_follows_effective_access() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(test_config(dir.path())).await.unwrap();
    seed(&state.db).await;
    crate::access::bump_generation();

    let plan = plan(&state.db).await.unwrap();
    assert_eq!(
        plan.accounts.iter().map(|user| user.id).collect::<Vec<_>>(),
        [1, 2, 3, 4, 7],
        "active accounts with a Linux identity"
    );
    // Administrators are in every read-write group (D6); a read grant
    // gives the read-only group; a deny wins over a legacy write
    // membership; deactivated accounts and accounts without an identity
    // are in no group.
    assert_eq!(
        plan.members[TEAM],
        Membership {
            write: vec![1, 3],
            read: vec![2],
        }
    );
    assert_eq!(
        plan.members[SALES],
        Membership {
            write: vec![1],
            read: vec![],
        }
    );
    // `~/Shared` links: the shares each account may open.
    let names = |id: i64| -> Vec<String> {
        plan.roots
            .get(&id)
            .map(|roots| roots.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    };
    assert_eq!(names(1), ["Sales", "Team"]);
    assert_eq!(names(2), ["Team"]);
    assert_eq!(names(3), ["Team"]);
    assert!(names(4).is_empty());
    assert!(names(7).is_empty());
    assert_eq!(plan.roots[&2].get("Team").unwrap().container, "team");

    // Revocation: carol loses write and keeps read; bob loses everything.
    exec(
        &state.db,
        &format!(
            "UPDATE user_folder_rules SET rule = 'read' WHERE user_id = 3 AND share_id = '{TEAM}'"
        ),
    )
    .await;
    exec(
        &state.db,
        &format!("DELETE FROM share_members WHERE user_id = 2 AND share_id = '{TEAM}'"),
    )
    .await;
    crate::access::bump_generation();
    let plan = super::plan(&state.db).await.unwrap();
    assert_eq!(
        plan.members[TEAM],
        Membership {
            write: vec![1],
            read: vec![3],
        }
    );
    assert!(names_of(&plan, 2).is_empty());
}

fn names_of(plan: &Plan, id: i64) -> Vec<String> {
    plan.roots
        .get(&id)
        .map(|roots| roots.iter().map(|(name, _)| name.clone()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_share_with_a_bad_container_gets_no_groups() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(test_config(dir.path())).await.unwrap();
    seed(&state.db).await;
    exec(
        &state.db,
        &format!("UPDATE shares SET container_path = '../etc' WHERE id = '{SALES}'"),
    )
    .await;
    let plan = plan(&state.db).await.unwrap();
    assert_eq!(
        plan.shares
            .iter()
            .map(|share| share.id.as_str())
            .collect::<Vec<_>>(),
        [TEAM]
    );
}

#[test]
fn a_move_to_the_read_only_group_is_a_revocation() {
    let mut changes = Changes::default();
    changes.record(&json!({
        "write": {"added": [], "removed": [3, 9], "removedOther": 0},
        "read": {"added": [3, 4], "removed": [], "removedOther": 1},
        "skipped": [],
    }));
    changes.record(&json!({
        "write": {"added": [5], "removed": [], "removedOther": 0},
        "read": {"added": [], "removed": [], "removedOther": 0},
    }));
    assert_eq!(changes.revoked, HashSet::from([3, 9]));
    assert_eq!(changes.granted, HashSet::from([3, 4, 5]));
    assert_eq!(changes.changed(), HashSet::from([3, 4, 5, 9]));
    // A malformed reply changes nothing.
    let mut none = Changes::default();
    none.record(&json!({"write": "x"}));
    assert_eq!(none, Changes::default());
}

#[test]
fn a_move_to_the_read_write_group_is_no_revocation_and_lost_shares_are_named() {
    let mut changes = Changes::default();
    changes.record(&json!({
        "shareId": TEAM,
        // 6: read → write; 3: write → read; 9: write → nothing; 8: read →
        // nothing.
        "write": {"added": [6], "removed": [3, 9], "removedOther": 0},
        "read": {"added": [3], "removed": [6, 8], "removedOther": 0},
    }));
    assert_eq!(changes.revoked, HashSet::from([3, 8, 9]));
    assert_eq!(changes.granted, HashSet::from([3, 6]));
    assert_eq!(
        changes.lost,
        HashMap::from([
            (8, BTreeSet::from([TEAM.to_string()])),
            (9, BTreeSet::from([TEAM.to_string()])),
        ])
    );
    assert_eq!(changes.gained[&6], BTreeSet::from([TEAM.to_string()]));
}

#[tokio::test]
async fn sessions_that_carry_a_lost_group_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(test_config(dir.path())).await.unwrap();
    seed(&state.db).await;
    crate::access::bump_generation();
    let plan = plan(&state.db).await.unwrap();
    assert_eq!(plan.identities, [1, 2, 3, 4, 5, 7], "deactivated included");
    let rw = |share: &str| shares::rw_group_name(share).unwrap();
    let ro = |share: &str| shares::ro_group_name(share).unwrap();
    // Team: write [1, 3], read [2]; Sales: write [1].
    let reply = json!({
        "users": [
            // Admin: rw everywhere, as it should be.
            {"userId": 1, "processes": 3, "gids": [10, 11]},
            // Reader of Team still holding Team's rw group: a downgrade.
            {"userId": 2, "processes": 1, "gids": [12, 13]},
            // Writer of Team holding Team's ro group: allowed (a subset);
            // and Sales' ro group, which it lost.
            {"userId": 3, "processes": 1, "gids": [13, 14]},
            // A gid without a share group, and a group of a share that
            // no longer exists: nothing.
            {"userId": 4, "processes": 1, "gids": [99, 15]},
            // Deactivated, still holding Team's rw group.
            {"userId": 5, "processes": 2, "gids": [12]},
        ],
        "groups": {
            "10": rw(TEAM), "11": rw(SALES), "12": rw(TEAM), "13": ro(TEAM),
            "14": ro(SALES), "15": "rast-sffffffffffffffff-rw",
        },
    });
    let index = plan.group_index();
    let stale = stale_in_reply(&reply, &plan, &index);
    assert_eq!(
        stale,
        [
            (2, BTreeMap::new()),
            (
                3,
                BTreeMap::from([(SALES.to_string(), "sales".to_string())])
            ),
            (5, BTreeMap::from([(TEAM.to_string(), "team".to_string())])),
        ]
    );
}

/// A helper that answers every operation and records `(op, shareId,
/// userIds)`.
fn recording_helper(socket: &std::path::Path) -> Arc<Mutex<Vec<(String, String, Value)>>> {
    recording_helper_with(socket, |_| Some(json!({"applied": true})))
}

type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

/// [`recording_helper`] with the data of each reply from `answer` (`None`:
/// an error).
fn recording_helper_with(socket: &std::path::Path, answer: fn(&Value) -> Option<Value>) -> Calls {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let log = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push((
                    request["op"].as_str().unwrap().to_string(),
                    request["shareId"].as_str().unwrap_or_default().to_string(),
                    request["userIds"].clone(),
                ));
                let reply = match answer(&request) {
                    Some(data) => json!({"id": request["id"], "ok": true, "data": data}),
                    None => json!({"id": request["id"], "ok": false, "error": {
                        "code": "not_found", "message": "planted"}}),
                };
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            });
        }
    });
    calls
}

#[tokio::test]
async fn containers_deny_users_until_their_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(test_config(dir.path())).await.unwrap();
    state.session_restarts = SessionRestarts::with_warning(Duration::from_secs(600));
    let restarts = state.session_restarts.clone();
    let socket = dir.path().join("h.sock");
    let calls = recording_helper(&socket);
    let native = native_for(dir.path(), socket);
    let share = |id: &str, container: &str| ShareInfo {
        id: id.to_string(),
        name: String::new(),
        container: container.to_string(),
    };
    const OTHER: &str = "22222222-3333-4444-8555-666666666666";
    let shares = [
        share(TEAM, "team"),
        share(SALES, "sales"),
        share(OTHER, "other"),
    ];
    let take = || -> Vec<(String, Value)> {
        calls
            .lock()
            .unwrap()
            .drain(..)
            .filter(|(op, _, _)| op == "share_deny_set")
            .map(|(_, share, users)| (share, users))
            .collect()
    };

    // User 2 lost Team, then Sales; user 3 only moved to read (no entry).
    let lost =
        |id: &str, container: &str| BTreeMap::from([(id.to_string(), container.to_string())]);
    restarts.revoked(&state, 2, lost(TEAM, "team"));
    restarts.revoked(&state, 3, BTreeMap::new());
    restarts.revoked(&state, 2, lost(SALES, "sales"));
    assert_eq!(restarts.wanted_denials(TEAM), BTreeSet::from([2]));
    assert_eq!(restarts.denied(2).len(), 2);
    // The first sync sets every share, so leftovers of an earlier run go.
    restarts.sync_denials(&native, &shares).await;
    let mut first = take();
    first.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (TEAM.to_string(), json!([2])),
        (SALES.to_string(), json!([2])),
        (OTHER.to_string(), json!([])),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(first, expected);
    restarts.sync_denials(&native, &shares).await;
    assert!(take().is_empty(), "nothing changed");

    // Sales is granted again: its entry goes, the restart still comes.
    restarts.regained(2, &BTreeSet::from([SALES.to_string()]));
    restarts.sync_denials(&native, &shares).await;
    assert_eq!(take(), [(SALES.to_string(), json!([]))]);
    assert!(restarts.pending_forced(2).is_some());

    // After the restart nothing denies the user any more.
    restarts.restart_now(&state, 2).await;
    assert!(restarts.recently_restarted(2));
    assert!(!restarts.recently_restarted(3));
    restarts.sync_denials(&native, &shares).await;
    assert_eq!(take(), [(TEAM.to_string(), json!([]))]);
}

#[tokio::test]
async fn a_revocation_warns_then_restarts_and_a_grant_only_offers() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(test_config(dir.path())).await.unwrap();
    let warning = Duration::from_millis(300);
    state.session_restarts = SessionRestarts::with_warning(warning);
    let mut events = state.events.subscribe();
    let restarts = state.session_restarts.clone();
    assert_eq!(SessionRestarts::default().warning, RESTART_WARNING);

    // A grant is only offered.
    restarts.granted(&state, 2);
    let offered = restarts.pending(2).unwrap();
    assert_eq!((offered.reason, offered.forced), ("access_granted", false));
    assert_eq!(offered.deadline_ms, None);

    // A revocation replaces the offer with a forced restart, and keeps its
    // deadline when another revocation follows.
    restarts.revoked(&state, 2, BTreeMap::new());
    let forced = restarts.pending(2).unwrap();
    assert_eq!((forced.reason, forced.forced), ("access_revoked", true));
    assert!(forced.deadline_ms.unwrap() > now_ms());
    restarts.revoked(&state, 2, BTreeMap::new());
    restarts.granted(&state, 2);
    assert_eq!(restarts.pending(2).unwrap(), forced);

    // After the warning the session restarts.
    let mut kinds = Vec::new();
    let collect = async {
        while let Ok(event) = events.recv().await {
            if event.kind.starts_with("session.") {
                kinds.push((event.kind.clone(), event.data.clone()));
                if event.kind == "session.restarted" {
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("the session restarts after the warning");
    assert_eq!(kinds.len(), 3, "{kinds:?}");
    assert_eq!(kinds[0].1["forced"], false);
    assert_eq!(kinds[1].1["forced"], true);
    assert_eq!(kinds[1].1["deadlineMs"], json!(forced.deadline_ms));
    assert_eq!(kinds[2].1["reason"], "access_revoked");
    assert!(restarts.pending(2).is_none());

    // "Restart session now" clears an offer.
    restarts.granted(&state, 3);
    restarts.restart_now(&state, 3).await;
    assert!(restarts.pending(3).is_none());
}

fn native_for(dir: &std::path::Path, socket: std::path::PathBuf) -> NativeClient {
    NativeClient::from_config(
        &Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.join("data")),
                storage_dir: Some(dir.join("storage")),
                native_enabled: Some(true),
                native_helper_socket: Some(socket),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap(),
    )
}

#[test]
fn the_repair_pass_runs_at_start_and_then_daily() {
    let now = std::time::Instant::now();
    assert!(repair_due(None, now));
    assert!(!repair_due(Some(now), now + Duration::from_secs(3600)));
    assert!(repair_due(Some(now), now + REPAIR_INTERVAL));
}

#[tokio::test]
async fn the_repair_pass_visits_every_shared_folder() {
    const BROKEN: &str = "33333333-4444-4555-8666-777777777777";
    const ODD: &str = "44444444-5555-4666-8777-888888888888";
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(test_config(dir.path())).await.unwrap();
    seed(&state.db).await;
    for (id, name, container) in [(BROKEN, "Broken", "broken"), (ODD, "Odd", "../etc")] {
        sqlx::query(
            "INSERT INTO shares (id, name, container_path, created_at, updated_at)
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(container)
        .execute(&state.db)
        .await
        .unwrap();
    }
    let socket = dir.path().join("h.sock");
    let calls = recording_helper_with(&socket, |request| match request["shareId"].as_str() {
        Some(TEAM) => Some(json!({"repaired": true, "report": {
            "changes": {"count": 3, "paths": []},
            "foreignAclEntries": {"count": 1, "paths": ["docs: u:1001:rwx"]},
        }})),
        Some(SALES) => Some(json!({"repaired": false, "reason": "not_migrated"})),
        _ => None,
    });
    let native = native_for(dir.path(), socket);
    let summary = repair_all(&state, &native).await.unwrap();
    assert_eq!(
        summary,
        RepairSummary {
            repaired: 1,
            not_migrated: 1,
            failed: 1,
            changed: 3,
        }
    );
    let mut visited: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(op, share, _)| {
            assert_eq!(op, "share_dir_repair");
            share.clone()
        })
        .collect();
    visited.sort();
    let mut expected = vec![TEAM.to_string(), SALES.to_string(), BROKEN.to_string()];
    expected.sort();
    assert_eq!(visited, expected, "an invalid container is never sent");
}

#[tokio::test]
async fn a_shared_folder_the_helper_refuses_does_not_hold_up_the_others() {
    // "Broken" sorts first. Its groups cannot be set (for example an
    // account with an unexpected uid); Team still loses user 3, and that
    // revocation must still be acted on.
    const BROKEN: &str = "33333333-4444-4555-8666-777777777777";
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(test_config(dir.path())).await.unwrap();
    state.session_restarts = SessionRestarts::with_warning(Duration::from_secs(600));
    seed(&state.db).await;
    sqlx::query(
        "INSERT INTO shares (id, name, container_path, created_at, updated_at)
         VALUES (?, 'Broken', 'broken', 1, 1)",
    )
    .bind(BROKEN)
    .execute(&state.db)
    .await
    .unwrap();
    let socket = dir.path().join("h.sock");
    let calls = recording_helper_with(&socket, |request| {
        let share = request["shareId"].as_str().unwrap_or_default();
        let unchanged = json!({"added": [], "removed": []});
        match request["op"].as_str() {
            Some("share_members_set") if share == BROKEN => None,
            Some("share_members_set") if share == TEAM => Some(json!({
                "shareId": share,
                "write": {"added": [], "removed": [3]},
                "read": unchanged,
            })),
            Some("share_members_set") => {
                Some(json!({"shareId": share, "write": unchanged, "read": unchanged}))
            }
            Some("share_session_groups") => Some(json!({"users": [], "groups": {}})),
            _ => Some(json!({"applied": true})),
        }
    });
    let native = native_for(dir.path(), socket);
    let mut known = Known::default();
    // Done, but not complete: the loop retries soon.
    assert!(!sync_once(&state, &native, &mut known).await.unwrap());

    let calls = calls.lock().unwrap().clone();
    let set: BTreeSet<&str> = calls
        .iter()
        .filter(|(op, _, _)| op == "share_members_set")
        .map(|(_, share, _)| share.as_str())
        .collect();
    assert_eq!(set, BTreeSet::from([BROKEN, SALES, TEAM]));
    assert!(calls.iter().any(|(op, _, _)| op == "share_session_groups"));
    assert!(calls
        .iter()
        .any(|(op, share, users)| op == "share_deny_set" && share == TEAM && *users == json!([3])));
    assert!(state.session_restarts.pending_forced(3).is_some());
    assert!(known.share_errors.contains_key(BROKEN));
    assert!(!known.share_errors.contains_key(TEAM));

    // Without the failing folder the pass is complete again.
    sqlx::query("DELETE FROM shares WHERE id = ?")
        .bind(BROKEN)
        .execute(&state.db)
        .await
        .unwrap();
    assert!(sync_once(&state, &native, &mut known).await.unwrap());
    assert!(known.share_errors.is_empty());

    // A helper that cannot be reached ends the pass with an error.
    let gone = native_for(dir.path(), dir.path().join("missing.sock"));
    let error = sync_once(&state, &gone, &mut Known::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("share_group_ensure"), "{error}");
}

#[tokio::test]
async fn identity_conflicts_are_remembered_until_they_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(test_config(dir.path())).await.unwrap();
    seed(&state.db).await;
    let socket = dir.path().join("h.sock");
    use std::sync::atomic::{AtomicBool, Ordering};
    static CONFLICT: AtomicBool = AtomicBool::new(true);
    let _calls = recording_helper_with(&socket, |request| {
        let share = request["shareId"].as_str().unwrap_or_default();
        let unchanged = json!({"added": [], "removed": []});
        match request["op"].as_str() {
            Some("share_members_set") => {
                let conflicts = if share == SALES && CONFLICT.load(Ordering::SeqCst) {
                    json!([{"userId": 3, "uid": 999, "expectedUid": 200_003}])
                } else {
                    json!([])
                };
                Some(json!({
                    "shareId": share,
                    "write": unchanged,
                    "read": unchanged,
                    "conflicts": conflicts,
                }))
            }
            Some("share_session_groups") => Some(json!({"users": [], "groups": {}})),
            _ => Some(json!({"applied": true})),
        }
    });
    let native = native_for(dir.path(), socket);
    let mut known = Known::default();
    // A conflict is no failure: the pass is complete.
    assert!(sync_once(&state, &native, &mut known).await.unwrap());
    assert_eq!(
        known.conflicts.get(SALES),
        Some(&json!([{"userId": 3, "uid": 999, "expectedUid": 200_003}]))
    );
    assert!(!known.conflicts.contains_key(TEAM));
    CONFLICT.store(false, Ordering::SeqCst);
    assert!(sync_once(&state, &native, &mut known).await.unwrap());
    assert!(known.conflicts.is_empty());
}

#[test]
fn doctor_lists_recycle_bin_folders_made_by_someone_else() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(dir.path());
    let team = cfg.shared_dir().join("team");
    std::fs::create_dir_all(&team).unwrap();
    let base = cfg.linux_uid_base;
    // Made by this process (not by the identities they are named for).
    std::fs::create_dir_all(team.join(format!(".Trash-{}", base + 5))).unwrap();
    std::fs::write(team.join(format!(".Trash-{}", base + 6)), b"").unwrap();
    // Not names of RAST identities' recycle bins.
    std::fs::create_dir_all(team.join(".Trash-1000")).unwrap();
    std::fs::create_dir_all(team.join("docs")).unwrap();
    let shares = vec![
        ("Team".to_string(), "team".to_string()),
        ("Gone".to_string(), "missing".to_string()),
        ("Bad".to_string(), "../etc".to_string()),
    ];
    let (mut lines, warnings) = doctor_share_trash(&cfg, &shares);
    lines.sort();
    assert_eq!(warnings, 2, "{lines:?}");
    assert!(
        lines[0].contains(&format!(".Trash-{}", base + 5)),
        "{lines:?}"
    );
    assert!(lines[0].contains("a folder"), "{lines:?}");
    assert!(lines[1].contains("not a folder"), "{lines:?}");
    assert!(lines.iter().all(|line| line.contains("shared folder Team")));
}

// ---------------------------------------------------------------------------
// The native pilot (ADR 0007)
// ---------------------------------------------------------------------------

/// `t-share`: in the pilot (name and container).
const PILOT: &str = "55555555-6666-4777-8888-999999999999";
/// Renamed out of the pilot: name `Moved`, container `t-moved`.
const RENAMED_OUT: &str = "66666666-7777-4888-8999-aaaaaaaaaaaa";
/// Renamed into the pilot: name `t-real`, container `real`.
const RENAMED_IN: &str = "77777777-8888-4999-8aaa-bbbbbbbbbbbb";
/// The accounts in the pilot (see [`seed_pilot`]).
const PILOT_USERS: [i64; 3] = [8, 9, 10];

fn pilot_config(dir: &std::path::Path, native: bool) -> Config {
    Config::from_parts(
        None,
        FileConfig {
            data_dir: Some(dir.join("data")),
            storage_dir: Some(dir.join("storage")),
            monitor_interval_secs: Some(300),
            native_enabled: Some(native),
            native_helper_socket: Some(dir.join("h.sock")),
            native_pilot_prefixes: Some(vec!["t-".into()]),
            ..Default::default()
        },
        |_| None,
    )
    .expect("test config")
}

/// [`seed`], plus the pilot accounts 8 `t-alice`, 9 `t-bob` and 10
/// `t-admin` (an administrator), and three shared folders: `t-share` (bob,
/// outside the pilot, was given write too), and two renamed across the
/// boundary.
async fn seed_pilot(pool: &SqlitePool) {
    seed(pool).await;
    for (id, name, role) in [
        (8, "t-alice", "user"),
        (9, "t-bob", "user"),
        (10, "t-admin", "admin"),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, active, created_at, updated_at)
             VALUES (?, ?, 'x', ?, 1, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO linux_identities (user_id, username, uid, gid, home, created_at)
             VALUES (?, ?, ?, ?, '/nonexistent', 1)",
        )
        .bind(id)
        .bind(format!("rast-u{id}"))
        .bind(200_000 + id)
        .bind(200_000 + id)
        .execute(pool)
        .await
        .unwrap();
    }
    for (id, name, container) in [
        (PILOT, "t-share", "t-share"),
        (RENAMED_OUT, "Moved", "t-moved"),
        (RENAMED_IN, "t-real", "real"),
    ] {
        sqlx::query(
            "INSERT INTO shares (id, name, container_path, created_at, updated_at)
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(container)
        .execute(pool)
        .await
        .unwrap();
    }
    exec(
        pool,
        &format!(
            "INSERT INTO share_members (share_id, user_id, perms) VALUES
             ('{PILOT}', 8, 'write'), ('{PILOT}', 9, 'read'), ('{PILOT}', 2, 'write'),
             ('{RENAMED_OUT}', 8, 'write'), ('{RENAMED_IN}', 8, 'write')"
        ),
    )
    .await;
}

/// A helper that answers through `answer` and records every request.
fn full_recording_helper(
    socket: &std::path::Path,
    answer: fn(&Value) -> Value,
) -> Arc<Mutex<Vec<Value>>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let log = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push(request.clone());
                let reply = json!({"id": request["id"], "ok": true, "data": answer(&request)});
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            });
        }
    });
    calls
}

/// Every share and account a helper request names must be in the pilot.
fn assert_only_pilot(calls: &[Value]) {
    for request in calls {
        if let Some(share) = request["shareId"].as_str() {
            assert_eq!(share, PILOT, "{request}");
        }
        let mut users: Vec<i64> = Vec::new();
        users.extend(request["userId"].as_i64());
        for key in ["userIds", "write", "read"] {
            users.extend(
                request[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_i64),
            );
        }
        for user in users {
            assert!(PILOT_USERS.contains(&user), "{request}");
        }
    }
}

#[tokio::test]
async fn the_plan_covers_only_the_native_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(pilot_config(dir.path(), false))
        .await
        .unwrap();
    seed_pilot(&state.db).await;
    let piloted = plan_for(&state.db, &state.config.native_pilot)
        .await
        .unwrap();
    assert_eq!(
        piloted
            .shares
            .iter()
            .map(|share| share.id.as_str())
            .collect::<Vec<_>>(),
        [PILOT],
        "renamed across the boundary in either direction: outside"
    );
    assert_eq!(
        piloted
            .accounts
            .iter()
            .map(|user| user.id)
            .collect::<Vec<_>>(),
        PILOT_USERS
    );
    assert_eq!(piloted.identities, PILOT_USERS);
    // bob (outside the pilot) and the administrators outside it are in no
    // group; the pilot administrator is in the rw group (D6).
    assert_eq!(
        piloted.members[PILOT],
        Membership {
            write: vec![8, 10],
            read: vec![9],
        }
    );
    assert_eq!(names_of(&piloted, 8), ["t-share"]);
    assert!(!piloted.roots.contains_key(&1));
    assert!(!piloted.roots.contains_key(&2));
    // Without the pilot: everything, as before.
    let all = plan(&state.db).await.unwrap();
    assert_eq!(all.shares.len(), 5);
    assert_eq!(all.identities, [1, 2, 3, 4, 5, 7, 8, 9, 10]);
}

#[tokio::test]
async fn the_syncer_never_names_a_share_or_an_account_outside_the_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(pilot_config(dir.path(), false))
        .await
        .unwrap();
    state.session_restarts = SessionRestarts::with_warning(Duration::from_secs(600));
    seed_pilot(&state.db).await;
    let socket = dir.path().join("fake.sock");
    let calls = full_recording_helper(&socket, |request| match request["op"].as_str() {
        // bob (2) was in the rw group before the pilot was set: the helper
        // takes him out and says so.
        Some("share_members_set") => json!({
            "shareId": request["shareId"],
            "write": {"added": [8, 10], "removed": [2]},
            "read": {"added": [9], "removed": []},
        }),
        Some("share_session_groups") => json!({"users": [], "groups": {}}),
        _ => json!({"applied": true}),
    });
    let native = native_for(dir.path(), socket);
    let mut known = Known::default();
    assert!(sync_once(&state, &native, &mut known).await.unwrap());
    let calls = calls.lock().unwrap().clone();
    assert_only_pilot(&calls);
    let ops: Vec<&str> = calls.iter().filter_map(|c| c["op"].as_str()).collect();
    assert!(ops.contains(&"share_group_ensure"), "{ops:?}");
    assert!(ops.contains(&"share_members_set"), "{ops:?}");
    let asked: Vec<&Value> = calls
        .iter()
        .filter(|c| c["op"] == "share_session_groups")
        .map(|c| &c["userIds"])
        .collect();
    assert_eq!(asked, [&json!(PILOT_USERS)]);
    // The pilot accounts are offered a restart; bob's sessions are left
    // alone.
    for user in PILOT_USERS {
        assert!(state.session_restarts.pending(user).is_some(), "{user}");
    }
    assert!(state.session_restarts.pending(2).is_none());
}

#[tokio::test]
async fn the_repair_pass_skips_shared_folders_outside_the_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(pilot_config(dir.path(), false))
        .await
        .unwrap();
    seed_pilot(&state.db).await;
    let socket = dir.path().join("fake.sock");
    let calls = full_recording_helper(
        &socket,
        |_| json!({"repaired": true, "report": {"changes": {"count": 0, "paths": []}}}),
    );
    let native = native_for(dir.path(), socket);
    let summary = repair_all(&state, &native).await.unwrap();
    assert_eq!(summary.repaired, 1);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_only_pilot(&calls);
}

#[tokio::test]
async fn the_create_and_delete_hooks_act_only_on_pilot_shared_folders() {
    let dir = tempfile::tempdir().unwrap();
    // The hooks use the configured helper: it must be listening first.
    let calls = full_recording_helper(&dir.path().join("h.sock"), |request| {
        match request["op"].as_str() {
            Some("share_session_groups") => json!({"users": [], "groups": {}}),
            _ => json!({"applied": true}),
        }
    });
    let state = AppState::new(pilot_config(dir.path(), true)).await.unwrap();
    for (id, name, container) in [
        (TEAM, "Team", "team"),
        (RENAMED_OUT, "Moved", "t-moved"),
        (RENAMED_IN, "t-real", "real"),
    ] {
        share_created(&state, id, name, container).await;
        share_deleted(&state, id, name, container).await;
    }
    let named = |share: &str| -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["shareId"] == share)
            .map(|c| c["op"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    for share in [TEAM, RENAMED_OUT, RENAMED_IN] {
        assert!(named(share).is_empty(), "{share}: {:?}", named(share));
    }
    share_created(&state, PILOT, "t-share", "t-share").await;
    share_deleted(&state, PILOT, "t-share", "t-share").await;
    assert_eq!(
        named(PILOT),
        [
            "share_group_ensure",
            "share_dir_prepare",
            "share_dir_release",
            "share_group_remove"
        ]
    );
    assert_only_pilot(&calls.lock().unwrap());
}
