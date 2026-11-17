use super::*;

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const TEAM: &str = "0123abcd-4567-4ef0-8123-456789abcdef";
const GONE: &str = "11111111-2222-4333-8444-555555555555";
const ID1: &str = "00000000-0000-4000-8000-000000000001";
const ID2: &str = "00000000-0000-4000-8000-000000000002";
const ID3: &str = "00000000-0000-4000-8000-000000000003";
const ID4: &str = "00000000-0000-4000-8000-000000000004";

type Handler = Arc<dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync>;
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A helper that answers through `handler` and records every request.
fn fake_helper(socket: &Path, handler: Handler) -> Calls {
    let calls: Calls = Arc::default();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let log = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let op = request["op"].as_str().unwrap().to_string();
                log.lock().unwrap().push((op.clone(), request.clone()));
                let reply = match handler(&op, &request) {
                    Ok(data) => json!({"id": request["id"], "ok": true, "data": data}),
                    Err(message) => json!({"id": request["id"], "ok": false, "error": {
                        "code": "forbidden", "message": message}}),
                };
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            });
        }
    });
    calls
}

/// The answers of a helper that migrates everything.
fn answers(op: &str, request: &Value) -> Result<Value, String> {
    let user = request["userId"].as_i64().unwrap_or_default();
    Ok(match op {
        "identity_ensure" => json!({
            "username": format!("rast-u{user}"), "uid": 200_000 + user, "gid": 200_000 + user,
            "home": format!("/var/lib/rast/linux-users/{user}"),
        }),
        "home_migrate_scan" => json!({
            "mode": "legacy", "journal": null, "applied": false, "processes": 0,
            "strayHome": user == 2, "shared": ["Team", "notes"], "units": ["rast-xpra-u2"],
            "passwdHome": "/var/lib/rast/linux-users/2",
            "tree": {
                "dirs": 2, "files": 3, "bytes": 30, "allocatedBytes": 12288,
                "changeOwner": {"count": 5, "paths": [".", "docs"]},
                "identityAclEntries": {"count": 2, "paths": ["docs", "docs/a.txt"]},
                "foreignAclEntries": {"count": 1, "paths": ["b.txt: u:1001:rw-"]},
                "hardLinks": {"count": 0, "paths": []}, "nonUtf8": 0,
            },
            "merge": {
                "moves": 3, "foldersCreated": 1,
                "sources": [
                    {"source": "identity", "path": "/var/lib/rast/linux-users/2",
                     "files": 2, "bytes": 1000, "moves": 2, "conflicts": 0, "skipped": 0},
                    {"source": "chrome", "path": "/var/lib/rast/linux-apps/profiles/chrome/2",
                     "files": 4, "bytes": 2000, "moves": 1, "conflicts": 1, "skipped": 0,
                     "otherFileSystem": false},
                ],
                "conflicts": {"count": 1, "paths": ["chrome: config/pulse/cookie"]},
                "skipped": {"count": 0, "paths": []},
            },
        }),
        "home_migrate_apply" => json!({
            "alreadyOneHome": false, "journal": "000000000001", "resumed": false,
            "stopped": ["rast-xpra-u2"], "tree": {"dirs": 1, "files": 1, "bytes": 1,
            "allocatedBytes": 4096, "changeOwner": {"count": 2, "paths": []}},
            "merge": {"moves": 0, "foldersCreated": 0, "sources": []},
            "steps": {"moved": 0, "already": 0, "created": 0,
                      "missing": {"count": 0, "paths": []}, "conflicts": {"count": 0, "paths": []}},
        }),
        "home_migrate_trash" => {
            let results: Vec<Value> = request["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let id = entry["id"].as_str().unwrap();
                    let status = match id {
                        ID3 => "missing",
                        ID4 => "failed",
                        _ if id.ends_with("0005") => "already",
                        _ => "moved",
                    };
                    json!({"id": id, "status": status, "message": "planted"})
                })
                .collect();
            json!({"results": results})
        }
        "home_ensure" => json!({"created": false}),
        "home_migrate_rollback" => json!({
            "journal": "000000000001",
            "report": {"restored": 4, "trashRestored": [ID1, ID2, ID1, "00000000-0000-4000-8000-0000000fffff"],
                       "returned": {"count": 1, "paths": ["new.txt"]},
                       "steps": {"moved": 2, "created": 1}},
        }),
        other => return Err(format!("unexpected {other}")),
    })
}

struct Fixture {
    dir: tempfile::TempDir,
    cfg: Config,
    pool: SqlitePool,
    native: NativeClient,
    calls: Calls,
}

impl Fixture {
    fn env(&self) -> Env<'_> {
        Env {
            cfg: &self.cfg,
            pool: &self.pool,
            native: &self.native,
        }
    }

    fn ops(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _)| op.clone())
            .collect()
    }

    fn requests(&self, op: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == op)
            .map(|(_, request)| request.clone())
            .collect()
    }

    async fn trash_ids(&self, user_id: i64) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM trash WHERE user_id = ? ORDER BY id")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    async fn add_trash(&self, id: &str, user_id: i64, original: &str, trashed: &str) {
        sqlx::query(
            "INSERT INTO trash (id, user_id, original_path, trashed_path, size_bytes, kind, deleted_at)
             VALUES (?, ?, ?, ?, 100, 'file', 1794823200000)",
        )
        .bind(id)
        .bind(user_id)
        .bind(original)
        .bind(trashed)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

async fn fixture_with(handler: Handler) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_parts(
        None,
        crate::config::FileConfig {
            data_dir: Some(dir.path().join("data")),
            storage_dir: Some(dir.path().join("storage")),
            native_enabled: Some(true),
            native_helper_socket: Some(dir.path().join("h.sock")),
            ..Default::default()
        },
        |_| None,
    )
    .unwrap();
    let pool = crate::db::init(&cfg).await.unwrap();
    // Two accounts (an administrator and a user) and a test share.
    for (id, name, role, quota) in [(1, "admin", "admin", 0), (2, "T-Alice", "user", 20_000)] {
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, role, quota_bytes,
                                created_at, updated_at)
             VALUES (?, ?, '', 'x', ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(role)
        .bind(quota)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO shares (id, name, container_path, created_at, updated_at)
         VALUES (?, 'Team', 'team', 1, 1)",
    )
    .bind(TEAM)
    .execute(&pool)
    .await
    .unwrap();
    let _ = GONE;
    let calls = fake_helper(&dir.path().join("h.sock"), handler);
    let native = NativeClient::from_config(&cfg);
    Fixture {
        dir,
        cfg,
        pool,
        native,
        calls,
    }
}

async fn fixture() -> Fixture {
    fixture_with(Arc::new(answers)).await
}

fn user(name: &str) -> Selection {
    Selection::User(name.into())
}

fn migrated(_: &Path) -> bool {
    false
}

#[tokio::test]
async fn the_dry_run_adds_the_recycle_bin_quota_and_shared_folder_checks() {
    let fx = fixture().await;
    fx.add_trash(ID1, 2, "/docs/a.txt", &format!("2/{ID1}"))
        .await;
    fx.add_trash(ID2, 2, "/shared/team/plans/q4.txt", &format!("2/{ID2}"))
        .await;
    fx.add_trash(ID3, 2, "/shared/old/x.txt", &format!("2/{ID3}"))
        .await;
    fx.add_trash(ID4, 2, "/docs/b.txt", "1/elsewhere").await;
    // The administrator can open Team, and Team is still legacy.
    let legacy = |path: &Path| path.ends_with("team");
    let outcomes = run_with(&fx.env(), Mode::DryRun, &Selection::All, &legacy)
        .await
        .unwrap();
    assert_eq!(fx.ops(), ["home_migrate_scan", "home_migrate_scan"]);
    let scans = fx.requests("home_migrate_scan");
    assert_eq!(
        scans[1]["username"], "t-alice",
        "the HOME name is lowercase"
    );
    let alice = &outcomes[1];
    assert!(alice.ok, "{alice:?}");
    assert_eq!(alice.trash.rows, 4);
    assert_eq!(alice.trash.bytes, 400);
    assert_eq!(
        alice.trash.left,
        [
            format!("{ID3} (/shared/old/x.txt): its shared folder no longer exists"),
            format!("{ID4} (/docs/b.txt): its data is not in the account's recycle bin"),
        ]
    );
    let quota = alice.quota.as_ref().unwrap();
    // 12288 on disk + 3000 of app data + 400 in the recycle bin.
    assert_eq!(quota.projected_bytes, 15_688);
    assert!(!quota.over);
    assert!(alice
        .warnings
        .iter()
        .any(|w| w.contains("rast-u2") && w.contains("D13")));
    assert!(alice.warnings.iter().any(|w| w.contains("rast-xpra-u2")));
    // Alice cannot open Team; the administrator can, and it is legacy.
    assert!(alice.legacy_shares.is_empty());
    assert!(alice.shared_collisions.is_empty());
    let admin = &outcomes[0];
    assert_eq!(admin.legacy_shares, ["Team"]);
    assert_eq!(admin.shared_collisions, ["Team"]);
    assert!(admin.warnings[0].contains("rast migrate shares --share"));

    let lines = describe(Mode::DryRun, alice);
    assert_eq!(lines[0], "T-Alice (user 2): legacy HOME, would be migrated");
    assert_eq!(
        lines[1],
        "  HOME: 2 folders, 3 files, 30 bytes (12288 on disk); 5 entries to give to the identity"
    );
    assert!(lines[2].starts_with("  2 entries with ACL entries of RAST identities"));
    assert!(
        lines.contains(&"  1 entries with other ACL entries (kept): b.txt: u:1001:rw-".to_string())
    );
    assert!(lines.contains(
        &"  1 conflicts (they stay in the old profile): chrome: config/pulse/cookie".to_string()
    ));
    assert!(lines.contains(&"  projected usage: 15688 of 20000 bytes".to_string()));
    assert!(lines
        .iter()
        .any(|line| line == "  recycle bin: 4 entries, 400 bytes, to move into XDG Trash"));
}

#[tokio::test]
async fn apply_moves_the_recycle_bin_in_batches_and_keeps_rows_it_could_not_move() {
    let fx = fixture().await;
    fx.add_trash(ID1, 2, "/docs/a.txt", &format!("2/{ID1}"))
        .await;
    fx.add_trash(ID2, 2, "/shared/Team/plans/q4.txt", &format!("2/{ID2}"))
        .await;
    fx.add_trash(ID3, 2, "/gone.txt", &format!("2/{ID3}")).await;
    fx.add_trash(ID4, 2, "/broken.txt", &format!("2/{ID4}"))
        .await;
    // 300 more: two batches.
    for n in 5..305u32 {
        let id = format!("00000000-0000-4000-8000-{n:012x}");
        fx.add_trash(&id, 2, &format!("/many/{n}.txt"), &format!("2/{id}"))
            .await;
    }
    let outcomes = run_with(&fx.env(), Mode::Apply, &user("t-alice"), &migrated)
        .await
        .unwrap();
    let alice = &outcomes[0];
    assert!(alice.ok, "{alice:?}");
    assert_eq!(
        fx.ops(),
        [
            "identity_ensure",
            "home_migrate_apply",
            "home_migrate_trash",
            "home_migrate_trash",
            "home_ensure"
        ]
    );
    let batches = fx.requests("home_migrate_trash");
    assert_eq!(
        batches[0]["entries"].as_array().unwrap().len(),
        MAX_TRASH_BATCH
    );
    assert_eq!(
        batches[1]["entries"].as_array().unwrap().len(),
        304 - MAX_TRASH_BATCH
    );
    assert_eq!(batches[0]["username"], "t-alice");
    let first = &batches[0]["entries"];
    assert_eq!(
        first[0],
        json!({"id": ID1, "deletedAt": 1_794_823_200_000i64, "path": "docs/a.txt"})
    );
    assert_eq!(
        first[1],
        json!({"id": ID2, "deletedAt": 1_794_823_200_000i64, "path": "plans/q4.txt",
               "container": "team"})
    );
    // Moved and already-moved rows are gone; missing and failed stay.
    assert_eq!(fx.trash_ids(2).await, [ID3, ID4]);
    assert_eq!(alice.trash.moved, 301);
    assert_eq!(alice.trash.already, 1);
    assert_eq!(alice.trash.left.len(), 2);
    // The rows were saved first, for a rollback.
    let dir = fx.cfg.data_dir.join("migrate-home/2");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&files[0]), 0o600);
    }
    let (saved, _) = saved_rows(&fx.cfg, 2).unwrap();
    assert_eq!(saved.len(), 304);
    // The identity's home in the database is the HOME now.
    let home: String = sqlx::query_scalar("SELECT home FROM linux_identities WHERE user_id = 2")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(PathBuf::from(home), fx.cfg.home_for("t-alice"));
    let lines = describe(Mode::Apply, alice);
    assert_eq!(lines[0], "T-Alice (user 2): migrated to one-HOME mode");
    assert!(lines.contains(&"  stopped: rast-xpra-u2".to_string()));
    assert!(lines.contains(&"  journal: <data_dir>/migrations/home/2/000000000001".to_string()));
    assert!(lines
        .iter()
        .any(|line| line.contains("2 recycle bin entries stay")));

    // The rollback puts back the rows of the entries the helper moved back.
    fx.calls.lock().unwrap().clear();
    let outcomes = run_with(&fx.env(), Mode::Rollback, &user("T-ALICE"), &migrated)
        .await
        .unwrap();
    let alice = &outcomes[0];
    assert!(alice.ok, "{alice:?}");
    assert_eq!(fx.ops(), ["home_migrate_rollback"]);
    assert_eq!(fx.trash_ids(2).await, [ID1, ID2, ID3, ID4]);
    assert_eq!(alice.trash.restored, 2);
    assert!(
        alice.warnings[0].contains("0000000fffff"),
        "{:?}",
        alice.warnings
    );
    let home: String = sqlx::query_scalar("SELECT home FROM linux_identities WHERE user_id = 2")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(PathBuf::from(home), fx.cfg.linux_users_dir().join("2"));
    // The saved rows are used once.
    let (saved, files) = saved_rows(&fx.cfg, 2).unwrap();
    assert!(saved.is_empty() && files.is_empty());
    let lines = describe(Mode::Rollback, alice);
    assert_eq!(
        lines[0],
        "T-Alice (user 2): rolled back to the legacy layout"
    );
    assert_eq!(
        lines[1],
        "  4 entries restored; 1 made since given back to the service, 0 changed since \
         (left as they are), 0 missing"
    );
    let _ = &fx.dir;
}

#[tokio::test]
async fn apply_refuses_while_a_shared_folder_of_the_account_is_legacy() {
    let fx = fixture().await;
    let legacy = |_: &Path| true;
    let outcomes = run_with(&fx.env(), Mode::Apply, &user("admin"), &legacy)
        .await
        .unwrap();
    assert!(!outcomes[0].ok);
    assert!(outcomes[0]
        .error
        .as_deref()
        .unwrap()
        .contains("rast migrate shares --share <name>"));
    assert_eq!(outcomes[0].legacy_shares, ["Team"]);
    assert!(fx.ops().is_empty(), "nothing was asked of the helper");
    // Alice cannot open Team: her migration goes on.
    let outcomes = run_with(&fx.env(), Mode::Apply, &user("t-alice"), &legacy)
        .await
        .unwrap();
    assert!(outcomes[0].ok, "{:?}", outcomes[0]);
}

#[tokio::test]
async fn one_failing_account_does_not_stop_the_others_and_names_are_checked() {
    let fx = fixture_with(Arc::new(|op: &str, request: &Value| {
        if op == "home_migrate_apply" && request["userId"] == 1 {
            return Err("planted".into());
        }
        answers(op, request)
    }))
    .await;
    let outcomes = run_with(&fx.env(), Mode::Apply, &Selection::All, &migrated)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(!outcomes[0].ok);
    assert!(outcomes[0].error.as_deref().unwrap().contains("planted"));
    assert!(outcomes[1].ok);
    assert_eq!(
        describe(Mode::Apply, &outcomes[0])[0],
        "admin (user 1): FAILED: home_migrate_apply: forbidden: planted"
    );
    assert!(matches!(
        run_with(&fx.env(), Mode::DryRun, &user("nobody"), &migrated).await,
        Err(AppError::NotFound(_))
    ));
}

#[tokio::test]
async fn an_account_made_in_one_home_mode_has_no_recycle_bin_to_move() {
    let fx = fixture_with(Arc::new(|op: &str, request: &Value| {
        if op == "home_migrate_apply" {
            return Ok(json!({"alreadyOneHome": true, "journal": null, "stopped": []}));
        }
        answers(op, request)
    }))
    .await;
    let outcomes = run_with(&fx.env(), Mode::Apply, &user("t-alice"), &migrated)
        .await
        .unwrap();
    assert!(outcomes[0].ok);
    assert_eq!(
        fx.ops(),
        ["identity_ensure", "home_migrate_apply", "home_ensure"]
    );
    assert_eq!(
        describe(Mode::Apply, &outcomes[0])[0],
        "T-Alice (user 2): already in one-HOME mode"
    );
}

#[test]
fn rows_map_to_the_home_or_the_container_of_their_shared_folder() {
    let shares = vec![ShareInfo {
        id: TEAM.into(),
        name: "Team".into(),
        container: "team".into(),
    }];
    let row = |id: &str, original: &str, trashed: String| TrashRow {
        id: id.into(),
        user_id: 2,
        original_path: original.into(),
        trashed_path: trashed,
        size_bytes: 1,
        kind: "file".into(),
        deleted_at: 5,
    };
    assert_eq!(
        map_row(&row(ID1, "/a/b c.txt", format!("2/{ID1}")), 2, &shares).unwrap(),
        json!({"id": ID1, "deletedAt": 5, "path": "a/b c.txt"})
    );
    assert_eq!(
        map_row(&row(ID1, "/shared/TEAM/x", format!("2/{ID1}")), 2, &shares).unwrap()["container"],
        "team"
    );
    for (original, trashed) in [
        ("/shared/Team", format!("2/{ID1}")),
        ("/", format!("2/{ID1}")),
        ("relative", format!("2/{ID1}")),
        ("/a/../b", format!("2/{ID1}")),
        ("/a", format!("3/{ID1}")),
        ("/a", format!("2/{ID1}/x")),
        ("/a", "../2/x".to_string()),
    ] {
        assert!(
            map_row(&row(ID1, original, trashed.clone()), 2, &shares).is_err(),
            "{original} {trashed}"
        );
    }
    assert!(map_row(&row("ABC", "/a", "2/ABC".into()), 2, &shares).is_err());
}

// ---------------------------------------------------------------------------
// The native pilot (ADR 0007)
// ---------------------------------------------------------------------------

/// [`fixture`] with `native_pilot_prefixes = ["t-"]`: `T-Alice` is in the
/// pilot, `admin` and the shared folder `Team` are not.
async fn pilot_fixture() -> Fixture {
    let mut fx = fixture().await;
    fx.cfg.native_pilot = NativePilot::with_prefixes(["t-"]).unwrap();
    fx
}

#[tokio::test]
async fn apply_and_rollback_refuse_an_account_outside_the_native_pilot() {
    let fx = pilot_fixture().await;
    let pilot = &fx.cfg.native_pilot;
    for mode in [Mode::Apply, Mode::Rollback] {
        assert!(matches!(
            run_with(&fx.env(), mode, &user("admin"), &migrated).await,
            Err(AppError::Forbidden(OUTSIDE_PILOT))
        ));
        assert_eq!(
            refuse_outside_pilot(pilot, mode, "admin"),
            Some(OUTSIDE_PILOT)
        );
        assert_eq!(refuse_outside_pilot(pilot, mode, "T-Alice"), None);
    }
    assert!(fx.ops().is_empty(), "nothing was asked of the helper");
    // The dry run still reports it, marked.
    assert_eq!(refuse_outside_pilot(pilot, Mode::DryRun, "admin"), None);
    let outcomes = run_with(&fx.env(), Mode::DryRun, &user("admin"), &migrated)
        .await
        .unwrap();
    assert!(
        outcomes[0].ok && outcomes[0].outside_pilot,
        "{:?}",
        outcomes[0]
    );
    assert_eq!(fx.ops(), ["home_migrate_scan"]);
    let lines = describe(Mode::DryRun, &outcomes[0]);
    assert_eq!(
        lines[1],
        "  outside the native pilot: apply and rollback refuse this account"
    );
    // Without the pilot nothing is refused.
    assert_eq!(
        refuse_outside_pilot(&NativePilot::off(), Mode::Apply, "admin"),
        None
    );
}

#[tokio::test]
async fn all_selects_only_accounts_in_the_native_pilot() {
    let fx = pilot_fixture().await;
    // One recycle bin entry of the shared folder outside the pilot (Alice
    // cannot open it any more) and one of her HOME.
    fx.add_trash(ID1, 2, "/shared/Team/a.txt", &format!("2/{ID1}"))
        .await;
    fx.add_trash(ID2, 2, "/docs/b.txt", &format!("2/{ID2}"))
        .await;
    for mode in [Mode::Apply, Mode::Rollback] {
        fx.calls.lock().unwrap().clear();
        let outcomes = run_with(&fx.env(), mode, &Selection::All, &migrated)
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        let (admin, alice) = (&outcomes[0], &outcomes[1]);
        assert!(admin.ok && admin.outside_pilot && admin.report.is_null());
        assert_eq!(
            describe(mode, admin),
            ["admin (user 1): skipped (outside the native pilot)"]
        );
        assert!(alice.ok && !alice.outside_pilot, "{alice:?}");
        for (op, request) in fx.calls.lock().unwrap().iter() {
            assert_eq!(request["userId"], 2, "{op}: {request}");
        }
        if mode == Mode::Apply {
            // The entry of the shared folder outside the pilot stays.
            let batches = fx.requests("home_migrate_trash");
            assert_eq!(batches.len(), 1);
            assert_eq!(
                batches[0]["entries"],
                json!([{"id": ID2, "deletedAt": 1_794_823_200_000i64, "path": "docs/b.txt"}])
            );
            assert_eq!(alice.trash.left.len(), 1);
            assert!(alice.trash.left[0].contains("outside the native pilot"));
            assert_eq!(fx.trash_ids(2).await, [ID1]);
        }
    }
}

#[tokio::test]
async fn an_account_that_can_open_a_shared_folder_outside_the_pilot_stays_legacy() {
    let fx = pilot_fixture().await;
    sqlx::query("INSERT INTO share_members (share_id, user_id, perms) VALUES (?, 2, 'read')")
        .bind(TEAM)
        .execute(&fx.pool)
        .await
        .unwrap();
    let outcomes = run_with(&fx.env(), Mode::Apply, &user("t-alice"), &migrated)
        .await
        .unwrap();
    let alice = &outcomes[0];
    assert!(!alice.ok);
    assert_eq!(alice.outside_pilot_shares, ["Team"]);
    assert!(alice
        .error
        .as_deref()
        .unwrap()
        .contains("outside the native pilot"));
    assert!(fx.ops().is_empty(), "nothing was asked of the helper");
    // Even when its container is still legacy, the reason is the pilot
    // (`rast migrate shares` would refuse Team).
    let legacy = |_: &Path| true;
    let outcomes = run_with(&fx.env(), Mode::DryRun, &user("t-alice"), &legacy)
        .await
        .unwrap();
    let alice = &outcomes[0];
    assert!(alice.legacy_shares.is_empty(), "{alice:?}");
    assert!(alice
        .warnings
        .iter()
        .any(|warning| warning.contains("outside the native pilot") && warning.contains("Team")));
}
