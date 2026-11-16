use super::*;

use crate::config::{Config, FileConfig};

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
    restarts.revoked(&state, 2);
    let forced = restarts.pending(2).unwrap();
    assert_eq!((forced.reason, forced.forced), ("access_revoked", true));
    assert!(forced.deadline_ms.unwrap() > now_ms());
    restarts.revoked(&state, 2);
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
