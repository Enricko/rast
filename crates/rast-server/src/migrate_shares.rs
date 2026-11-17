//! `rast migrate shares` (ADR 0005, section 8): move the shared folders of an
//! existing install to the group layout, before `rast migrate home`.
//!
//! The CLI runs as the `rast` service user, reads the shares and Effective
//! Access from the database, and asks the helper for every root step:
//!
//! - `--dry-run`: `shares_migrate_scan` per share (read-only report);
//! - apply: `share_group_ensure` (reusing gids that a restored container
//!   still names), `share_members_set` from Effective Access, then
//!   `shares_migrate_apply`, which journals owners, modes and ACLs before it
//!   changes anything and resumes a run that stopped half way;
//! - `--rollback`: `shares_migrate_rollback` from the journal. The groups
//!   stay (they give nothing without the container layout).
//!
//! Each share is done on its own: a failure is reported and the others go
//! on. Apply and rollback need the server stopped (the instance lock, D9).

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::native::client::NativeClient;
use crate::share_groups::{self, ShareInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Apply,
    Rollback,
}

impl Mode {
    fn op(self) -> &'static str {
        match self {
            Self::DryRun => "shares_migrate_scan",
            Self::Apply => "shares_migrate_apply",
            Self::Rollback => "shares_migrate_rollback",
        }
    }
}

/// The result for one shared folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareOutcome {
    pub share_id: String,
    pub name: String,
    pub container: String,
    pub ok: bool,
    /// The helper's report (`null` on failure).
    pub report: Value,
    /// What the server set as members before the apply.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub members: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Run one mode over every shared folder, or only the one named `only`.
pub async fn run(
    pool: &SqlitePool,
    native: &NativeClient,
    mode: Mode,
    only: Option<&str>,
) -> AppResult<Vec<ShareOutcome>> {
    let plan = share_groups::plan(pool).await?;
    let shares: Vec<&ShareInfo> = plan
        .shares
        .iter()
        .filter(|share| only.is_none_or(|name| share.name.eq_ignore_ascii_case(name)))
        .collect();
    if only.is_some() && shares.is_empty() {
        return Err(AppError::NotFound("no shared folder has that name"));
    }
    let mut out = Vec::with_capacity(shares.len());
    for share in shares {
        let members = plan.members.get(&share.id).cloned().unwrap_or_default();
        let mut outcome = ShareOutcome {
            share_id: share.id.clone(),
            name: share.name.clone(),
            container: share.container.clone(),
            ok: false,
            report: Value::Null,
            members: Value::Null,
            error: None,
        };
        let result = async {
            if mode == Mode::Apply {
                call(
                    native,
                    "share_group_ensure",
                    json!({"shareId": share.id, "container": share.container}),
                )
                .await?;
                outcome.members = call(
                    native,
                    "share_members_set",
                    json!({"shareId": share.id, "write": members.write, "read": members.read}),
                )
                .await?;
            }
            call(
                native,
                mode.op(),
                json!({"shareId": share.id, "container": share.container}),
            )
            .await
        }
        .await;
        match result {
            Ok(report) => {
                outcome.ok = true;
                outcome.report = report;
            }
            Err(error) => outcome.error = Some(error),
        }
        out.push(outcome);
    }
    Ok(out)
}

async fn call(native: &NativeClient, op: &str, params: Value) -> Result<Value, String> {
    native
        .call_json(op, params)
        .await
        .map_err(|error| format!("{op}: {error}"))
}

/// Human-readable lines for one outcome.
pub fn describe(mode: Mode, outcome: &ShareOutcome) -> Vec<String> {
    let mut lines = vec![format!(
        "{} ({}): {}",
        outcome.name,
        outcome.container,
        match (&outcome.error, mode) {
            (Some(error), _) => format!("FAILED: {error}"),
            (None, Mode::DryRun) => "would change".to_string(),
            (None, Mode::Apply) if outcome.report["resumed"] == true => {
                "applied (resumed with the existing journal)".to_string()
            }
            (None, Mode::Apply) => "applied".to_string(),
            (None, Mode::Rollback) => "rolled back".to_string(),
        }
    )];
    if outcome.error.is_some() {
        return lines;
    }
    let report = &outcome.report["report"];
    let count = |key: &str| report[key]["count"].as_u64().unwrap_or(0);
    match mode {
        Mode::DryRun | Mode::Apply => {
            lines.push(format!(
                "  {} folders, {} files, {} bytes; {} entries {}",
                report["dirs"].as_u64().unwrap_or(0),
                report["files"].as_u64().unwrap_or(0),
                report["bytes"].as_u64().unwrap_or(0),
                count("changes"),
                if mode == Mode::DryRun {
                    "to change"
                } else {
                    "changed"
                }
            ));
            for (key, label) in [
                ("symlinks", "symlinks (left unchanged)"),
                ("hardLinks", "files with several hard links (skipped)"),
                ("rootOwned", "entries owned by root (left unchanged)"),
                (
                    "otherOwner",
                    "entries owned by other accounts (left unchanged)",
                ),
                (
                    "otherDevice",
                    "mount points of other file systems and bind mounts (skipped)",
                ),
                ("special", "special files (skipped)"),
                ("nonUtf8", "names that are not UTF-8 (skipped)"),
                (
                    "identityAclEntries",
                    "entries with old per-user ACL entries (removed)",
                ),
                ("changedDuringWalk", "entries that changed during the walk"),
            ] {
                let n = count(key);
                if n == 0 {
                    continue;
                }
                let sample: Vec<&str> = report[key]["paths"]
                    .as_array()
                    .map(|paths| paths.iter().filter_map(Value::as_str).take(5).collect())
                    .unwrap_or_default();
                lines.push(format!("  {n} {label}: {}", sample.join(", ")));
            }
            // ACL entries that the new ACL removes: every listed one, so
            // the administrator knows what will be lost.
            let foreign = count("foreignAclEntries");
            if foreign > 0 {
                lines.push(format!(
                    "  {foreign} entries with ACL entries that RAST does not manage ({}):",
                    if mode == Mode::DryRun {
                        "they will be removed"
                    } else {
                        "removed"
                    }
                ));
                let listed: Vec<&str> = report["foreignAclEntries"]["paths"]
                    .as_array()
                    .map(|paths| paths.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for entry in &listed {
                    lines.push(format!("    {entry}"));
                }
                if (listed.len() as u64) < foreign {
                    lines.push(format!(
                        "    and {} more (save them first with getfacl -R)",
                        foreign - listed.len() as u64
                    ));
                }
            }
            if let Some(journal) = outcome.report["journal"].as_str() {
                lines.push(format!("  journal: {journal}"));
            }
        }
        Mode::Rollback => {
            lines.push(format!(
                "  {} entries restored; {} changed since (left as they are), {} created \
                 after the migration, {} missing",
                report["restored"].as_u64().unwrap_or(0),
                count("changedSince"),
                count("notRecorded"),
                count("missing"),
            ));
            // Every entry that lost set-uid (or set-gid on a file).
            let dropped = report["droppedBitsCount"].as_u64().unwrap_or(0);
            if dropped > 0 {
                lines.push(format!(
                    "  {dropped} entries restored without set-uid, or without set-gid on a file:"
                ));
                for entry in report["droppedBits"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "    {} (recorded {}, now {})",
                        entry["path"].as_str().unwrap_or("?"),
                        entry["recorded"].as_str().unwrap_or("?"),
                        entry["restored"].as_str().unwrap_or("?"),
                    ));
                }
                let listed = report["droppedBits"].as_array().map_or(0, Vec::len) as u64;
                if let Some(file) = report["droppedBitsFile"].as_str() {
                    lines.push(format!(
                        "    {}full list: <data_dir>/migrations/shares/{file}",
                        if listed < dropped {
                            format!("{} more; ", dropped - listed)
                        } else {
                            String::new()
                        }
                    ));
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A helper that answers every operation, except `shares_migrate_apply`
    /// for the share `fail`, and records `(op, shareId)`.
    fn fake_helper(
        socket: &std::path::Path,
        fail: &'static str,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
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
                    let op = request["op"].as_str().unwrap().to_string();
                    let share = request["shareId"].as_str().unwrap_or_default().to_string();
                    log.lock().unwrap().push((op.clone(), share.clone()));
                    let reply = if op == "shares_migrate_apply" && share == fail {
                        json!({"id": request["id"], "ok": false, "error": {
                            "code": "forbidden", "message": "planted"}})
                    } else {
                        json!({"id": request["id"], "ok": true, "data": {"report": {}}})
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
    async fn each_share_gets_its_groups_members_and_migration_on_its_own() {
        const TEAM: &str = "0123abcd-4567-4ef0-8123-456789abcdef";
        const SALES: &str = "11111111-2222-4333-8444-555555555555";
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::from_parts(
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
        for (id, name) in [(TEAM, "Team"), (SALES, "Sales")] {
            sqlx::query(
                "INSERT INTO shares (id, name, container_path, created_at, updated_at)
                 VALUES (?, ?, ?, 1, 1)",
            )
            .bind(id)
            .bind(name)
            .bind(name.to_lowercase())
            .execute(&pool)
            .await
            .unwrap();
        }
        let calls = fake_helper(&dir.path().join("h.sock"), SALES);
        let native = NativeClient::from_config(&cfg);

        let outcomes = run(&pool, &native, Mode::Apply, None).await.unwrap();
        let ops = |share: &str| -> Vec<String> {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, id)| id == share)
                .map(|(op, _)| op.clone())
                .collect()
        };
        assert_eq!(
            ops(TEAM),
            [
                "share_group_ensure",
                "share_members_set",
                "shares_migrate_apply"
            ]
        );
        // Sales failed and Team was still done.
        let by_name = |name: &str| outcomes.iter().find(|o| o.name == name).unwrap().clone();
        assert!(by_name("Team").ok);
        assert!(!by_name("Sales").ok);
        assert!(by_name("Sales")
            .error
            .as_deref()
            .unwrap()
            .contains("planted"));

        calls.lock().unwrap().clear();
        run(&pool, &native, Mode::DryRun, Some("team"))
            .await
            .unwrap();
        run(&pool, &native, Mode::Rollback, Some("Team"))
            .await
            .unwrap();
        assert_eq!(
            ops(TEAM),
            ["shares_migrate_scan", "shares_migrate_rollback"]
        );
        assert!(ops(SALES).is_empty());
        assert!(matches!(
            run(&pool, &native, Mode::DryRun, Some("nope")).await,
            Err(AppError::NotFound(_))
        ));
        for mode in [Mode::Apply, Mode::Rollback] {
            assert!(matches!(
                run(&pool, &native, mode, Some("nope")).await,
                Err(AppError::NotFound(_))
            ));
        }

        // `--share` applies to that shared folder only.
        calls.lock().unwrap().clear();
        let outcomes = run(&pool, &native, Mode::Apply, Some("TEAM"))
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].ok);
        assert_eq!(
            ops(TEAM),
            [
                "share_group_ensure",
                "share_members_set",
                "shares_migrate_apply"
            ]
        );
        assert!(ops(SALES).is_empty());
        // And so do dry-run and rollback: one share, one helper call.
        calls.lock().unwrap().clear();
        let outcomes = run(&pool, &native, Mode::DryRun, Some("sales"))
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        let outcomes = run(&pool, &native, Mode::Rollback, Some("sales"))
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            ops(SALES),
            ["shares_migrate_scan", "shares_migrate_rollback"]
        );
        assert!(ops(TEAM).is_empty());
    }

    #[test]
    fn reports_name_what_was_skipped_and_where_the_journal_is() {
        let outcome = ShareOutcome {
            share_id: "id".into(),
            name: "Team".into(),
            container: "team".into(),
            ok: true,
            report: json!({
                "journal": "1763251200",
                "resumed": false,
                "report": {
                    "dirs": 2, "files": 3, "bytes": 10,
                    "changes": {"count": 5, "paths": []},
                    "symlinks": {"count": 1, "paths": ["docs/link"]},
                    "hardLinks": {"count": 0, "paths": []},
                    "otherOwner": {"count": 1, "paths": ["docs/linked-in.txt"]},
                    "foreignAclEntries": {"count": 3, "paths": [
                        "docs: u:1001:rwx, default g:2000:r-x",
                        "docs/plan.txt: u:1001:rw-",
                    ]},
                }
            }),
            members: Value::Null,
            error: None,
        };
        let lines = describe(Mode::Apply, &outcome);
        assert_eq!(lines[0], "Team (team): applied");
        assert_eq!(
            lines[1],
            "  2 folders, 3 files, 10 bytes; 5 entries changed"
        );
        assert_eq!(lines[2], "  1 symlinks (left unchanged): docs/link");
        assert_eq!(
            lines[3],
            "  1 entries owned by other accounts (left unchanged): docs/linked-in.txt"
        );
        assert_eq!(
            lines[4..8],
            [
                "  3 entries with ACL entries that RAST does not manage (removed):",
                "    docs: u:1001:rwx, default g:2000:r-x",
                "    docs/plan.txt: u:1001:rw-",
                "    and 1 more (save them first with getfacl -R)",
            ]
        );
        assert_eq!(lines[8], "  journal: 1763251200");
        assert_eq!(
            describe(Mode::DryRun, &outcome)[4],
            "  3 entries with ACL entries that RAST does not manage (they will be removed):"
        );
        let failed = ShareOutcome {
            ok: false,
            error: Some("shares_migrate_apply: forbidden".into()),
            report: Value::Null,
            ..outcome
        };
        assert_eq!(
            describe(Mode::Apply, &failed),
            ["Team (team): FAILED: shares_migrate_apply: forbidden"]
        );
    }

    #[test]
    fn a_rollback_lists_every_entry_that_lost_a_bit() {
        let outcome = ShareOutcome {
            share_id: "id".into(),
            name: "Team".into(),
            container: "team".into(),
            ok: true,
            report: json!({
                "journal": "001763251200",
                "report": {
                    "restored": 4,
                    "changedSince": {"count": 0, "paths": []},
                    "notRecorded": {"count": 1, "paths": ["new.txt"]},
                    "missing": {"count": 0, "paths": []},
                    "droppedBitsCount": 3,
                    "droppedBits": [
                        {"path": "bin/tool", "recorded": "4755", "restored": "0755"},
                        {"path": "bin/group", "recorded": "2750", "restored": "0750"},
                    ],
                    "droppedBitsFile": "id/001763251200.rolled-back/dropped-bits.txt",
                }
            }),
            members: Value::Null,
            error: None,
        };
        assert_eq!(
            describe(Mode::Rollback, &outcome),
            [
                "Team (team): rolled back",
                "  4 entries restored; 0 changed since (left as they are), 1 created after the \
                 migration, 0 missing",
                "  3 entries restored without set-uid, or without set-gid on a file:",
                "    bin/tool (recorded 4755, now 0755)",
                "    bin/group (recorded 2750, now 0750)",
                "    1 more; full list: <data_dir>/migrations/shares/id/001763251200.rolled-back/\
                 dropped-bits.txt",
            ]
        );
    }
}
