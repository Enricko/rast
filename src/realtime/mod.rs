//! Realtime File Station events.
//!
//! File operations publish scoped `file.changed` / `file.quota` events after the
//! underlying database/filesystem change committed; the external filesystem
//! watcher ([`watch`]) publishes the same kinds when files change outside the
//! API. Scoping rules:
//!
//! * a change inside a user's home is published to that user only
//!   (`EventBus::publish_user`) — administrators never observe private homes;
//! * a change inside a shared folder is published to the share owner plus its
//!   members (`EventBus::publish_users`), never to every administrator;
//! * paths in event payloads are always **logical** File Station paths, never
//!   host paths.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::{
    auth::AuthUser,
    events::EventBus,
    storage::{quota, shares, trash, EntryDto, LogicalPath},
};

pub mod watch;

pub const FILE_CHANGED: &str = "file.changed";
pub const FILE_QUOTA: &str = "file.quota";
pub const FILES_RESYNC: &str = "files.resync";

/// Who may receive a file event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// A single account (home-directory changes).
    User(i64),
    /// An explicit account list (shared-folder changes: owner + members).
    Users(Vec<i64>),
}

/// Logical parent directories affected by a set of file paths. These are what
/// list views use to decide whether they need to refetch.
pub fn parent_paths(paths: &[String]) -> Vec<String> {
    let mut parents: Vec<String> = Vec::new();
    for raw in paths {
        let Ok(logical) = LogicalPath::parse(raw) else {
            continue;
        };
        let Some(parent) = logical.parent() else {
            continue;
        };
        // `/shared` is a namespace mount point, not a listable directory.
        if parent.segments().first().map(String::as_str) == Some("shared")
            && parent.shared_parts().is_none()
        {
            continue;
        }
        let parent = parent.as_string();
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    parents
}

/// The share name a logical path belongs to: `Some(None)` for home paths,
/// `Some(Some(name))` for shared paths, `None` for paths that are not
/// addressable (e.g. `/shared`).
fn share_name_of(path: &str) -> Option<Option<String>> {
    let logical = LogicalPath::parse(path).ok()?;
    match logical.shared_parts() {
        Some((name, _)) => Some(Some(name.to_string())),
        None => {
            if logical.segments().first().map(String::as_str) == Some("shared") {
                None
            } else {
                Some(None)
            }
        }
    }
}

fn entry_for_scope(entry_path: Option<&String>, share: Option<&str>) -> bool {
    let Some(entry_path) = entry_path else {
        return false;
    };
    match (share_name_of(entry_path), share) {
        (Some(None), None) => true,
        (Some(Some(entry_share)), Some(share)) => entry_share.eq_ignore_ascii_case(share),
        _ => false,
    }
}

/// Publish one scoped `file.changed` event.
///
/// For home audiences `userId` is the owner; for shared audiences `userId` is
/// the acting account (or `null` for external changes) and `userIds` lists every
/// allowed account.
pub fn publish_file_changed(
    events: &EventBus,
    audience: &Audience,
    actor_user_id: Option<i64>,
    op: &str,
    paths: &[String],
    entry: Option<Value>,
    source: &str,
) {
    let Some(primary) = paths.first() else {
        return;
    };
    let mut data = json!({
        "op": op,
        "path": primary,
        "paths": parent_paths(paths),
        "source": source,
    });
    if let Some(entry) = entry {
        data["entry"] = entry;
    }
    match audience {
        Audience::User(user_id) => {
            data["userId"] = json!(user_id);
            events.publish_user(FILE_CHANGED, *user_id, data);
        }
        Audience::Users(user_ids) => {
            data["userId"] = match actor_user_id {
                Some(user_id) => json!(user_id),
                None => Value::Null,
            };
            data["userIds"] = json!(user_ids);
            events.publish_users(FILE_CHANGED, user_ids.clone(), data);
        }
    }
}

/// Publish a committed File Station mutation. Affected paths are grouped by
/// namespace: a copy from a home into a shared folder notifies the home owner
/// and the share members with separate events, so nobody receives paths they
/// are not allowed to see.
pub async fn publish_mutation(
    events: &EventBus,
    pool: &SqlitePool,
    user: &AuthUser,
    op: &str,
    affected_paths: &[String],
    entry: Option<&EntryDto>,
) {
    if affected_paths.is_empty() {
        return;
    }
    let entry_path = entry.map(|entry| entry.path.clone());
    let entry_value = entry.and_then(|entry| serde_json::to_value(entry).ok());

    let mut home_paths: Vec<String> = Vec::new();
    let mut shared_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for raw in affected_paths {
        let Ok(logical) = LogicalPath::parse(raw) else {
            continue;
        };
        match logical.shared_parts() {
            Some((share_name, _)) => shared_paths
                .entry(share_name.to_string())
                .or_default()
                .push(logical.as_string()),
            None => {
                if logical.segments().first().map(String::as_str) == Some("shared") {
                    continue;
                }
                home_paths.push(logical.as_string());
            }
        }
    }

    if !home_paths.is_empty() {
        let home_entry = entry_for_scope(entry_path.as_ref(), None)
            .then(|| entry_value.clone())
            .flatten();
        publish_file_changed(
            events,
            &Audience::User(user.id),
            Some(user.id),
            op,
            &home_paths,
            home_entry,
            "api",
        );
    }

    for (share_name, paths) in shared_paths {
        let member_ids = match shares::find_by_name(pool, &share_name).await {
            Ok(Some(share)) => match shares::member_ids(pool, &share.id).await {
                Ok(ids) => ids,
                Err(error) => {
                    tracing::warn!(error = %error, share = %share_name, "cannot resolve share members for event");
                    continue;
                }
            },
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(error = %error, share = %share_name, "cannot look up share for event");
                continue;
            }
        };
        // The actor always receives their own action, even when they are an
        // administrator who is not a member of the share.
        let mut audience = member_ids;
        if !audience.contains(&user.id) {
            audience.push(user.id);
            audience.sort_unstable();
            audience.dedup();
        }
        if audience.is_empty() {
            continue;
        }
        let share_entry = entry_for_scope(entry_path.as_ref(), Some(&share_name))
            .then(|| entry_value.clone())
            .flatten();
        publish_file_changed(
            events,
            &Audience::Users(audience),
            Some(user.id),
            op,
            &paths,
            share_entry,
            "api",
        );
    }
}

/// Publish a scoped `file.quota` snapshot for the acting account.
pub async fn publish_quota(events: &EventBus, pool: &SqlitePool, user_id: i64) {
    let info = match quota::info(pool, user_id).await {
        Ok(info) => info,
        Err(error) => {
            tracing::debug!(error = %error, user_id, "cannot publish quota event");
            return;
        }
    };
    let trash_bytes = trash::total_bytes(pool, user_id).await.unwrap_or(0);
    events.publish_user(
        FILE_QUOTA,
        user_id,
        json!({
            "usedBytes": info.used_bytes,
            "quotaBytes": info.quota_bytes,
            "trashBytes": trash_bytes,
            "userId": user_id,
        }),
    );
}

/// Publish a committed mutation and, when it changed storage usage, a quota
/// snapshot. Convenience wrapper for API handlers and background jobs.
pub async fn publish_commit(
    events: &EventBus,
    pool: &SqlitePool,
    user: &AuthUser,
    op: &str,
    affected_paths: &[String],
    entry: Option<&EntryDto>,
    quota_changed: bool,
) {
    publish_mutation(events, pool, user, op, affected_paths, entry).await;
    if quota_changed {
        publish_quota(events, pool, user.id).await;
    }
}

/// Broadcast a resync request (watcher overflow / backlog).
pub fn publish_resync(events: &EventBus, reason: &str) {
    events.publish(FILES_RESYNC, json!({ "reason": reason }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Role, db, events::EventScope, storage::ops::EntryDto};

    fn auth_user(id: i64, username: &str, role: Role) -> AuthUser {
        AuthUser {
            id,
            username: username.to_string(),
            display_name: username.to_string(),
            role,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("realtime.db"), 4)
            .await
            .unwrap();
        for (id, username) in [(1_i64, "alice"), (2, "bob")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', 'user', 1, 1)",
            )
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO shares (id, name, description, container_path, created_by, created_at, updated_at)
             VALUES ('s1', 'team', '', 'team', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO share_members (share_id, user_id, perms) VALUES ('s1', 2, 'write')",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    fn entry(path: &str) -> EntryDto {
        EntryDto {
            name: "doc.txt".into(),
            path: path.into(),
            kind: "file".into(),
            size: 3,
            modified_at: 1,
            mime: "text/plain".into(),
            readable: true,
            writable: true,
            trashed: false,
        }
    }

    #[test]
    fn parents_are_logical_directories() {
        assert_eq!(parent_paths(&["/a/b.txt".into()]), vec!["/a".to_string()]);
        assert_eq!(
            parent_paths(&["/a/b.txt".into(), "/a/c.txt".into(), "/a/b.txt".into()]),
            vec!["/a".to_string()]
        );
        assert!(parent_paths(&["/".into()]).is_empty());
        // `/shared` itself is not listable, so it must not appear.
        assert_eq!(
            parent_paths(&["/shared/team/x.txt".into()]),
            vec!["/shared/team".to_string()]
        );
        assert!(parent_paths(&["/shared/team".into()]).is_empty());
        assert!(parent_paths(&["../evil".into()]).is_empty());
    }

    #[tokio::test]
    async fn home_mutation_is_private() {
        let (_dir, pool) = pool().await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let alice = auth_user(1, "alice", Role::User);

        publish_mutation(
            &bus,
            &pool,
            &alice,
            "upload",
            &["/docs/report.txt".to_string()],
            None,
        )
        .await;
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, FILE_CHANGED);
        assert_eq!(event.scope, EventScope::User(1));
        assert_eq!(event.data["op"], "upload");
        assert_eq!(event.data["path"], "/docs/report.txt");
        assert_eq!(event.data["paths"], json!(["/docs"]));
        assert_eq!(event.data["userId"], 1);
        assert_eq!(event.data["source"], "api");
        assert!(event.data.get("userIds").is_none());
        // An unrelated user and administrators may not see private homes.
        assert!(!event.scope.allows(2, false));
        assert!(!event.scope.allows(9, true));
    }

    #[tokio::test]
    async fn shared_mutation_reaches_members_and_actor_only() {
        let (_dir, pool) = pool().await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let admin = auth_user(9, "root", Role::Admin);

        // Admin (not a member) writes into the share: members + actor. The
        // creator (user 1, a regular user here) gets no implicit access (F6);
        // administrators still receive Users-scoped events via scope rules.
        publish_mutation(
            &bus,
            &pool,
            &admin,
            "mkdir",
            &["/shared/team/new".to_string()],
            None,
        )
        .await;
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, FILE_CHANGED);
        match &event.scope {
            EventScope::Users(ids) => {
                assert!(ids.contains(&2), "member must receive the event");
                assert!(ids.contains(&9), "actor must receive the event");
                assert!(
                    !ids.contains(&1),
                    "creator without a grant is not an implicit audience (F6)"
                );
            }
            other => panic!("expected Users scope, got {other:?}"),
        }
        assert_eq!(event.data["path"], "/shared/team/new");
        assert_eq!(event.data["paths"], json!(["/shared/team"]));
        assert_eq!(event.data["userId"], 9);
        assert_eq!(event.data["source"], "api");
        let user_ids = event.data["userIds"].as_array().unwrap();
        assert!(user_ids.iter().any(|id| id == 2));
        // The scope helper lets admins observe shared content only through the
        // explicit Users scope, never through a private home scope.
        assert!(event.scope.allows(2, false));
    }

    #[tokio::test]
    async fn mixed_namespace_copy_publishes_two_scoped_events() {
        let (_dir, pool) = pool().await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let alice = auth_user(1, "alice", Role::User);
        publish_mutation(
            &bus,
            &pool,
            &alice,
            "copy",
            &[
                "/shared/team/copy.txt".to_string(),
                "/homecopy.txt".to_string(),
            ],
            None,
        )
        .await;
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.scope, EventScope::User(1));
        assert_eq!(first.data["path"], "/homecopy.txt");
        match &second.scope {
            EventScope::Users(ids) => assert!(ids.contains(&1) && ids.contains(&2)),
            other => panic!("expected Users scope, got {other:?}"),
        }
        assert_eq!(second.data["path"], "/shared/team/copy.txt");
    }

    #[tokio::test]
    async fn entries_are_attached_to_the_right_namespace_only() {
        let (_dir, pool) = pool().await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let alice = auth_user(1, "alice", Role::User);
        let shared_entry = entry("/shared/team/doc.txt");
        publish_mutation(
            &bus,
            &pool,
            &alice,
            "upload",
            &[
                "/private.txt".to_string(),
                "/shared/team/doc.txt".to_string(),
            ],
            Some(&shared_entry),
        )
        .await;
        let home_event = rx.recv().await.unwrap();
        let shared_event = rx.recv().await.unwrap();
        assert!(home_event.data.get("entry").is_none());
        assert_eq!(shared_event.data["entry"]["path"], "/shared/team/doc.txt");
        assert_eq!(shared_event.data["entry"]["kind"], "file");
    }

    #[tokio::test]
    async fn quota_events_are_scoped_and_shaped() {
        let (_dir, pool) = pool().await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        quota::adjust_used(&pool, 1, 4096).await.unwrap();
        quota::reserve(&pool, 1, 10).await.unwrap();
        publish_quota(&bus, &pool, 1).await;
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, FILE_QUOTA);
        assert_eq!(event.scope, EventScope::User(1));
        assert_eq!(event.data["userId"], 1);
        assert_eq!(event.data["usedBytes"], 4096);
        assert_eq!(event.data["quotaBytes"], 0); // 0 = unlimited default
        assert_eq!(event.data["trashBytes"], 0);
    }

    #[tokio::test]
    async fn external_events_carry_no_actor_and_never_host_paths() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        publish_file_changed(
            &bus,
            &Audience::Users(vec![1, 2]),
            None,
            "changed",
            &["/shared/team/dir/file.txt".into()],
            None,
            "external",
        );
        let event = rx.recv().await.unwrap();
        assert_eq!(event.data["source"], "external");
        assert_eq!(event.data["op"], "changed");
        assert_eq!(event.data["userId"], Value::Null);
        assert_eq!(event.data["path"], "/shared/team/dir/file.txt");
        assert_eq!(event.data["paths"], json!(["/shared/team/dir"]));
        assert!(!event.data.to_string().contains("/srv/rast"));
    }

    #[tokio::test]
    async fn resync_is_broadcast() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        publish_resync(&bus, "overflow");
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, FILES_RESYNC);
        assert_eq!(event.scope, EventScope::Broadcast);
        assert_eq!(event.data["reason"], "overflow");
    }
}
