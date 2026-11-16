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
                let reply = json!({"id": request["id"], "ok": true, "data": {"applied": true}});
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
    let native = NativeClient::from_config(
        &Config::from_parts(
            None,
            FileConfig {
                data_dir: Some(dir.path().join("data")),
                storage_dir: Some(dir.path().join("storage")),
                native_enabled: Some(true),
                native_helper_socket: Some(socket),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap(),
    );
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
