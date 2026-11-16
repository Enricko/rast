//! Deleting, the recycle bin and the usage view of one-HOME accounts
//! (ADR 0005, sections 5 and 6): answered by the account's file agent from
//! the XDG Trash in HOME and in the shared folders, and from a scan of
//! HOME.
//!
//! The server keeps its own rules: write access for every deleted path and
//! for the shared folder of every restored entry, the shared folders the
//! agent may open (in the shares header: those the account can read for
//! listing and removing, only those it can write for restoring), audit and
//! realtime events. Agent replies are untrusted input (ADR 0005, section 2):
//! ids, paths and numbers are checked before they are used or passed on.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::extract::{FromRequestParts, Query, Request};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use super::{agent_headers, check, json_response, Need, JSON_LIMIT};
use crate::api::files::audit_file;
use crate::auth::{AuthUser, ClientIp};
use crate::error::{AppError, AppResult};
use crate::homefs::agent::usage::USAGE_ROUTE;
use crate::homefs::client::AGENT;
use crate::homefs::{ShareRoot, ShareRoots};
use crate::jobs::JobSpec;
use crate::state::AppState;
use crate::storage::ops::{UsageDto, QUOTA_BACKEND_RAST, USAGE_LOCATIONS, USAGE_TYPES};
use crate::storage::trash::TrashEntryDto;
use crate::storage::xdg_trash::TrashId;
use crate::storage::{quota, shares, LogicalPath, MAX_BATCH_PATHS};

/// The routes this module answers for one-HOME accounts.
pub(super) const ROUTES: &[&str] = &[
    "/api/files/delete",
    "/api/files/usage",
    "/api/files/trash",
    "/api/files/trash/restore",
    "/api/files/trash/delete",
];

/// A HOME scan runs at most this often in the background (listings start
/// it); the usage view scans on demand.
pub(super) const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
/// Entries one trash page carries at most.
const MAX_PAGE: usize = 500;
/// Original paths of one removal passed on to realtime events at most.
const MAX_REPORTED_PATHS: usize = 500;
/// Largest byte count believed from an agent (2^53: exact in JSON).
const MAX_BYTES: i64 = 1 << 53;

fn bad_reply() -> AppError {
    AppError::unavailable(format!("unexpected {AGENT} reply"))
}

/// A non-negative count of an agent reply.
fn count(value: &Value, key: &str) -> AppResult<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .filter(|number| (0..=MAX_BYTES).contains(number))
        .ok_or_else(bad_reply)
}

/// How HOME usage changed (never more than it can shrink by).
fn home_delta(value: &Value) -> i64 {
    value
        .get("homeBytes")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(-MAX_BYTES, 0)
}

/// A logical path of an agent reply.
fn agent_path(value: &Value) -> AppResult<String> {
    let text = value.as_str().ok_or_else(bad_reply)?;
    LogicalPath::parse(text)
        .map(|logical| logical.as_string())
        .map_err(|_| bad_reply())
}

/// A trash entry of an agent reply.
fn agent_entry(value: &Value) -> AppResult<TrashEntryDto> {
    let entry: TrashEntryDto = serde_json::from_value(value.clone()).map_err(|_| bad_reply())?;
    let valid = TrashId::parse(&entry.id).is_some()
        && LogicalPath::parse(&entry.original_path).is_ok()
        && matches!(entry.kind.as_str(), "file" | "dir")
        && (0..=MAX_BYTES).contains(&entry.size_bytes);
    if !valid {
        return Err(bad_reply());
    }
    Ok(entry)
}

/// The original paths of a removal (bounded).
fn agent_paths(value: &Value) -> AppResult<Vec<String>> {
    let list = value
        .get("originalPaths")
        .and_then(Value::as_array)
        .ok_or_else(bad_reply)?;
    list.iter()
        .take(MAX_REPORTED_PATHS)
        .map(agent_path)
        .collect()
}

/// The known keys of a breakdown, with non-negative values.
fn known(keys: &[&'static str], value: &Value) -> BTreeMap<&'static str, i64> {
    keys.iter()
        .filter_map(|key| {
            let bytes = value.get(*key)?.as_i64()?;
            (0..=MAX_BYTES).contains(&bytes).then_some((*key, bytes))
        })
        .collect()
}

/// The shared folders the account may open (`Need::Write`: only those it
/// may write), as the agent's shares header names them.
pub(super) async fn share_roots(
    state: &AppState,
    user: &AuthUser,
    need: Need,
) -> AppResult<ShareRoots> {
    let mut roots = ShareRoots::default();
    for record in shares::list_records(&state.db).await? {
        let access = match crate::access::folder_access_by_id(&state.db, user, &record.id).await {
            Ok(access) => access,
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        let Some(level) = access.level else {
            continue;
        };
        if need == Need::Write && !level.allows_write() {
            continue;
        }
        let Ok(container) = shares::validated_container(&record.container_path) else {
            continue;
        };
        roots.insert(
            &record.name,
            ShareRoot {
                id: record.id,
                name: record.name.clone(),
                container,
            },
        )?;
    }
    Ok(roots)
}

/// A `file.quota` event for a one-HOME account. The recycle bin total is
/// the last one the agent reported (left out when unknown, so the browser
/// keeps its own).
pub(super) async fn publish_quota(state: &AppState, user_id: i64) {
    let Ok(info) = quota::info(&state.db, user_id).await else {
        return;
    };
    let mut data = json!({
        "usedBytes": info.used_bytes,
        "quotaBytes": info.quota_bytes,
        "userId": user_id,
    });
    if let Some(bytes) = state.homefs.trash_bytes(user_id) {
        data["trashBytes"] = json!(bytes);
    }
    state
        .events
        .publish_user(crate::realtime::FILE_QUOTA, user_id, data);
}

/// Scan the account's HOME through its agent and record the result: under
/// the RAST fallback the scan is the account's usage.
pub(super) async fn refresh_usage(state: &AppState, user: &AuthUser) -> AppResult<UsageDto> {
    let roots = share_roots(state, user, Need::Read).await?;
    let value = state
        .homefs
        .fetch(user.id, Method::GET, USAGE_ROUTE, agent_headers(&roots)?)
        .await?;
    let allocated = count(&value, "allocatedBytes")?;
    let trash_bytes = count(&value, "trashBytes")?;
    let by_location = known(
        &USAGE_LOCATIONS,
        value.get("byLocation").unwrap_or(&Value::Null),
    );
    let by_type = known(&USAGE_TYPES, value.get("byType").unwrap_or(&Value::Null));
    quota::set_used(&state.db, user.id, allocated).await?;
    state.homefs.note_trash_bytes(user.id, trash_bytes);
    let info = quota::info(&state.db, user.id).await?;
    Ok(UsageDto {
        quota_bytes: info.quota_bytes,
        used_bytes: info.used_bytes,
        trash_bytes,
        by_type,
        by_location,
        quota_backend: QUOTA_BACKEND_RAST,
    })
}

/// Start a background HOME scan when the last one is older than
/// [`SCAN_INTERVAL`].
pub(super) fn refresh_in_background(state: &AppState, user: &AuthUser) {
    if !state.homefs.begin_scan(user.id, SCAN_INTERVAL) {
        return;
    }
    let state = state.clone();
    let user = user.clone();
    tokio::spawn(async move {
        let before = quota::info(&state.db, user.id).await.ok();
        match refresh_usage(&state, &user).await {
            Ok(usage) => {
                if before.is_some_and(|info| info.used_bytes != usage.used_bytes) {
                    publish_quota(&state, user.id).await;
                }
            }
            Err(error) => tracing::debug!(user_id = user.id, error = %error, "HOME scan failed"),
        }
    });
}

async fn read_json_body(body: Body) -> AppResult<Value> {
    let bytes = axum::body::to_bytes(body, JSON_LIMIT)
        .await
        .map_err(|_| AppError::PayloadTooLarge("request body is too large"))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| AppError::bad_request("invalid JSON body"))?;
    if !value.is_object() {
        return Err(AppError::bad_request("invalid JSON body"));
    }
    Ok(value)
}

/// The strings of `body[key]`: at least one, at most [`MAX_BATCH_PATHS`].
fn string_list(body: &Value, key: &str, what: &str) -> AppResult<Vec<String>> {
    let list: Vec<String> = body
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        return Err(AppError::bad_request(format!(
            "at least one {what} is required"
        )));
    }
    if list.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request(format!(
            "too many {what}s in one request"
        )));
    }
    Ok(list)
}

pub(super) async fn handle(
    state: &AppState,
    user: AuthUser,
    route: &str,
    request: Request,
) -> AppResult<Response> {
    let (mut parts, body) = request.into_parts();
    let ip = ClientIp::from_request_parts(&mut parts, state).await?;
    let method = parts.method.clone();
    match (route, method) {
        ("/api/files/usage", Method::GET) => {
            state.homefs.begin_scan(user.id, std::time::Duration::ZERO);
            let usage = refresh_usage(state, &user).await?;
            Ok(json_response(
                StatusCode::OK,
                serde_json::to_value(usage).map_err(|_| AppError::internal("usage"))?,
            ))
        }
        ("/api/files/trash", Method::GET) => list(state, &user, &parts.uri).await,
        ("/api/files/trash", Method::DELETE) => empty(state, &user, &ip).await,
        ("/api/files/trash/restore", Method::POST) => {
            restore(state, &user, &ip, read_json_body(body).await?).await
        }
        ("/api/files/trash/delete", Method::POST) => {
            remove(state, &user, &ip, read_json_body(body).await?).await
        }
        ("/api/files/delete", Method::POST) => {
            delete(state, &user, &ip, read_json_body(body).await?).await
        }
        _ => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
    }
}

/// GET /api/files/trash
async fn list(state: &AppState, user: &AuthUser, uri: &axum::http::Uri) -> AppResult<Response> {
    let query: HashMap<String, String> = Query::try_from_uri(uri)
        .map(|Query(query)| query)
        .unwrap_or_default();
    let number = |key: &str, default: i64| {
        query
            .get(key)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(default)
    };
    let page = number("page", 1).max(1);
    let per_page = number("perPage", crate::storage::DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE as i64);
    // The `trashRetentionDays` setting, honoured lazily as in-process.
    let retention_days = crate::db::get_setting(&state.db, "trashRetentionDays")
        .await
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 3650);
    let roots = share_roots(state, user, Need::Read).await?;
    let path =
        format!("/api/files/trash?page={page}&perPage={per_page}&retentionDays={retention_days}");
    let value = state
        .homefs
        .fetch(user.id, Method::GET, &path, agent_headers(&roots)?)
        .await?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= MAX_PAGE)
        .ok_or_else(bad_reply)?
        .iter()
        .map(agent_entry)
        .collect::<AppResult<Vec<_>>>()?;
    let total = count(&value, "total")?;
    if let Ok(bytes) = count(&value, "bytes") {
        state.homefs.note_trash_bytes(user.id, bytes);
    }
    Ok(json_response(
        StatusCode::OK,
        json!({"items": items, "page": page, "perPage": per_page, "total": total}),
    ))
}

/// POST /api/files/delete: a job that moves the paths into the recycle
/// bins (HOME's, or the shared folder's own).
async fn delete(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    body: Value,
) -> AppResult<Response> {
    let paths = string_list(&body, "paths", "path")?;
    let mut roots = ShareRoots::default();
    let mut affected = Vec::with_capacity(paths.len());
    for path in &paths {
        let logical = check(state, user, path, Need::Write, &mut roots).await?;
        let share_root = logical
            .shared_parts()
            .is_some_and(|(_, rest)| rest.is_empty());
        if logical.is_root() || share_root {
            return Err(AppError::bad_request("storage roots cannot be deleted"));
        }
        affected.push(logical.as_string());
    }
    let headers = agent_headers(&roots)?;
    let count_items = paths.len();
    let mut detail = format!("{count_items} item(s) to the recycle bin: ");
    detail.push_str(&paths.join(", "));
    if detail.len() > 900 {
        let mut end = 900;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push('…');
    }
    audit_file(state, user, ip, "file.delete", &paths[0], detail).await;
    let job_state = state.clone();
    let actor = user.clone();
    let request = json!({ "paths": paths });
    let spec = JobSpec::new("file.delete", move |ctx| async move {
        ctx.progress(
            0,
            count_items as u64,
            &format!("delete: {count_items} item(s)"),
        )
        .await;
        let call = job_state
            .homefs
            .call(actor.id, "/api/files/delete", &request, headers);
        tokio::pin!(call);
        let outcome = loop {
            tokio::select! {
                result = &mut call => break result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    // Dropping the request stops the agent between entries.
                    if let Err(error) = ctx.check_cancelled() {
                        break Err(error);
                    }
                }
            }
        };
        // Whatever moved before an error is gone from its folder.
        super::publish(&job_state, &actor, "delete", &affected, None, false).await;
        let reply = outcome?;
        let deleted = count(&reply, "deleted")?;
        let bytes = count(&reply, "bytes")?;
        if let Some(known) = job_state.homefs.trash_bytes(actor.id) {
            job_state
                .homefs
                .note_trash_bytes(actor.id, known.saturating_add(bytes));
        }
        publish_quota(&job_state, actor.id).await;
        ctx.progress(count_items as u64, count_items as u64, "Done")
            .await;
        Ok(json!({"deleted": deleted, "bytes": bytes, "trashed": true}))
    })
    .actor(user)
    .message(format!("delete {count_items} item(s)"));
    let job = state.jobs.submit(spec).await?;
    Ok(json_response(
        StatusCode::OK,
        json!({"jobId": job, "items": count_items}),
    ))
}

/// The ids of a request; a shared-folder entry needs write access to its
/// folder to be restored (404 when the folder is hidden).
async fn checked_ids(
    state: &AppState,
    user: &AuthUser,
    body: &Value,
    restoring: bool,
) -> AppResult<Vec<String>> {
    let ids = string_list(body, "ids", "trash id")?;
    for raw in &ids {
        let id = TrashId::parse(raw).ok_or(AppError::NotFound("recycle bin entry not found"))?;
        let Some(share_id) = id.share_id() else {
            continue;
        };
        let access = crate::access::folder_access_by_id(&state.db, user, share_id)
            .await
            .map_err(|_| AppError::NotFound("recycle bin entry not found"))?;
        let level = access
            .level
            .ok_or(AppError::NotFound("recycle bin entry not found"))?;
        if restoring && !level.allows_write() {
            return Err(AppError::Forbidden("write permission required"));
        }
    }
    Ok(ids)
}

/// POST /api/files/trash/restore
async fn restore(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    body: Value,
) -> AppResult<Response> {
    let ids = checked_ids(state, user, &body, true).await?;
    // Only folders the account may write: an entry whose origin is a
    // shared folder it may not write goes back into HOME.
    let roots = share_roots(state, user, Need::Write).await?;
    let reply = state
        .homefs
        .call(
            user.id,
            "/api/files/trash/restore",
            &json!({ "ids": ids }),
            agent_headers(&roots)?,
        )
        .await?;
    let restored = reply
        .get("restored")
        .and_then(Value::as_array)
        .filter(|list| list.len() <= MAX_BATCH_PATHS)
        .ok_or_else(bad_reply)?
        .iter()
        .map(|item| {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| TrashId::parse(id).is_some())
                .ok_or_else(bad_reply)?;
            let path = agent_path(item.get("path").unwrap_or(&Value::Null))?;
            Ok((id.to_string(), path))
        })
        .collect::<AppResult<Vec<_>>>()?;
    let delta = home_delta(&reply);
    if delta != 0 {
        quota::adjust_used(&state.db, user.id, delta).await?;
    }
    for (id, path) in &restored {
        audit_file(
            state,
            user,
            ip,
            "file.restore",
            path,
            format!("restored from recycle bin ({id})"),
        )
        .await;
    }
    let paths: Vec<String> = restored.iter().map(|(_, path)| path.clone()).collect();
    super::publish(state, user, "restore", &paths, None, false).await;
    publish_quota(state, user.id).await;
    let restored: Vec<Value> = restored
        .into_iter()
        .map(|(id, path)| json!({"id": id, "path": path}))
        .collect();
    Ok(json_response(
        StatusCode::OK,
        json!({"ok": true, "restored": restored}),
    ))
}

/// Account, audit and announce a removal for good.
async fn removed(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    reply: &Value,
    op: &str,
    object: &str,
) -> AppResult<Response> {
    let deleted = count(reply, "deleted")?;
    let bytes = count(reply, "bytes")?;
    let paths = agent_paths(reply)?;
    let delta = home_delta(reply);
    if delta != 0 {
        quota::adjust_used(&state.db, user.id, delta).await?;
    }
    if let Some(known) = state.homefs.trash_bytes(user.id) {
        state
            .homefs
            .note_trash_bytes(user.id, known.saturating_sub(bytes));
    }
    let detail = if op == "trash_empty" {
        format!("emptied recycle bin: {deleted} entries ({bytes} bytes)")
    } else {
        format!("permanently deleted {deleted} recycle bin entries ({bytes} bytes)")
    };
    audit_file(state, user, ip, "file.trash_delete", object, detail).await;
    crate::realtime::publish_private(&state.events, user, op, &paths);
    publish_quota(state, user.id).await;
    Ok(json_response(
        StatusCode::OK,
        json!({"ok": true, "deleted": deleted, "bytes": bytes}),
    ))
}

/// POST /api/files/trash/delete
async fn remove(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    body: Value,
) -> AppResult<Response> {
    let ids = checked_ids(state, user, &body, false).await?;
    let roots = share_roots(state, user, Need::Read).await?;
    let reply = state
        .homefs
        .call(
            user.id,
            "/api/files/trash/delete",
            &json!({ "ids": ids }),
            agent_headers(&roots)?,
        )
        .await?;
    removed(state, user, ip, &reply, "trash_delete", &ids.join(",")).await
}

/// DELETE /api/files/trash
async fn empty(state: &AppState, user: &AuthUser, ip: &ClientIp) -> AppResult<Response> {
    let roots = share_roots(state, user, Need::Read).await?;
    let reply = state
        .homefs
        .fetch(
            user.id,
            Method::DELETE,
            "/api/files/trash",
            agent_headers(&roots)?,
        )
        .await?;
    removed(state, user, ip, &reply, "trash_empty", "/").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_trash_entries_are_checked() {
        let good = json!({
            "id": TrashId::Home("a".into()).encode(),
            "originalPath": "/docs/a",
            "sizeBytes": 3,
            "kind": "file",
            "deletedAt": 1
        });
        assert_eq!(agent_entry(&good).unwrap().original_path, "/docs/a");
        for (key, value) in [
            ("id", json!("../x")),
            ("originalPath", json!("/docs/../etc")),
            ("originalPath", json!("relative")),
            ("kind", json!("link")),
            ("sizeBytes", json!(-1)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(agent_entry(&bad).is_err(), "{key}");
        }
        let mut extra = good.clone();
        extra["html"] = json!("<b/>");
        assert!(agent_entry(&extra).is_err());
    }

    #[test]
    fn agent_numbers_are_bounded() {
        assert_eq!(home_delta(&json!({"homeBytes": 5})), 0);
        assert_eq!(home_delta(&json!({"homeBytes": -5})), -5);
        assert_eq!(home_delta(&json!({"homeBytes": i64::MIN})), -MAX_BYTES);
        assert_eq!(home_delta(&json!({})), 0);
        assert!(count(&json!({"n": -1}), "n").is_err());
        assert!(count(&json!({"n": "1"}), "n").is_err());
        assert_eq!(count(&json!({"n": 7}), "n").unwrap(), 7);
        let breakdown = known(
            &USAGE_LOCATIONS,
            &json!({"files": 1, "apps": -2, "evil": 3, "trash": "x"}),
        );
        assert_eq!(breakdown, BTreeMap::from([("files", 1)]));
    }
}
