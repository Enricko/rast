//! File Station for one-HOME accounts (ADR 0005, section 2): `/api/files/*`
//! requests of an account whose HOME belongs to its Linux identity are
//! answered by that account's own file agent (see [`crate::homefs`]).
//!
//! [`dispatch`] sits behind the root file system dispatch, so `/@root` still
//! goes to the root agent, and legacy accounts go on to the in-process
//! handlers untouched. For a one-HOME account the server keeps every check
//! it does today (session and CSRF before this layer, path parsing, the
//! effective access of `/shared/<name>` with 404 for hidden and 403 for
//! read-only folders, the RAST quota, audit, background jobs for copy and
//! move, realtime events), then forwards path, query and body unchanged and
//! streams the response back. The containers of the shared folders a
//! request names travel in a header that only the server sets
//! ([`SHARES_HEADER`]): the agent never follows `~/Shared/<name>`. When the
//! helper or the agent is down the account gets 503: the server never falls
//! back to in-process access.
//!
//! Deleting, the recycle bin and the usage view are answered from the
//! account's XDG Trash and a scan of its HOME (task 1.4, see [`trash`]).

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{FromRequestParts, Query, Request, State};
use axum::http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::files::audit_file;
use crate::auth::{AuthUser, ClientIp};
use crate::error::{AppError, AppResult};
use crate::fileagent::{client, QUOTA_HEADER, REPLACED_HEADER, SHARES_HEADER};
use crate::homefs::client::AGENT;
use crate::homefs::{ShareRoot, ShareRoots};
use crate::jobs::JobSpec;
use crate::state::AppState;
use crate::storage::ops::{EntryDto, QuotaDto};
use crate::storage::serve::PREVIEW_CSP;
use crate::storage::{quota, shares, LogicalPath, MAX_BATCH_PATHS};

/// JSON bodies of File Station requests are small (paths and names).
const JSON_LIMIT: usize = 2 * 1024 * 1024;

/// Routes whose paths travel in the query string.
const QUERY_ROUTES: &[&str] = &[
    "/api/files",
    "/api/files/stat",
    "/api/files/download",
    "/api/files/preview",
    "/api/files/text",
    "/api/files/search",
    "/api/files/upload",
    "/api/files/upload/chunk",
    "/api/files/upload/complete",
];

/// Routes whose paths travel in a JSON body.
const JSON_ROUTES: &[&str] = &[
    "/api/files/mkdir",
    "/api/files/rename",
    "/api/files/copy",
    "/api/files/move",
    "/api/files/upload/init",
];

mod trash;

pub async fn dispatch(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let route = request.uri().path().trim_end_matches('/').to_string();
    let route = route.as_str();
    let known = QUERY_ROUTES.contains(&route)
        || JSON_ROUTES.contains(&route)
        || trash::ROUTES.contains(&route);
    if !known {
        return next.run(request).await;
    }
    let Some(user) = request.extensions().get::<AuthUser>().cloned() else {
        return next.run(request).await;
    };
    match state.homefs.is_one_home(&state.db, &user).await {
        Ok(true) => {}
        Ok(false) => return next.run(request).await,
        Err(error) => return error.into_response(),
    }
    release_expired(&state).await;
    let result = if trash::ROUTES.contains(&route) {
        trash::handle(&state, user, route, request).await
    } else if QUERY_ROUTES.contains(&route) {
        forward(&state, user, route, request).await
    } else if request.method() == Method::POST {
        json_request(&state, user, route, request).await
    } else {
        return next.run(request).await;
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

/// Give back the quota of chunked uploads the agents have dropped.
async fn release_expired(state: &AppState) {
    for reservation in state.homefs.expired() {
        let _ = quota::rollback(&state.db, reservation.user_id, reservation.bytes).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Read,
    Write,
}

/// RAST's own rules for one logical path; the agent's uid then applies the
/// kernel's. A hidden shared folder is 404, a read-only one refuses writes.
/// The container of a shared folder that passes is recorded in `roots`,
/// for the agent.
async fn check(
    state: &AppState,
    user: &AuthUser,
    raw: &str,
    need: Need,
    roots: &mut ShareRoots,
) -> AppResult<LogicalPath> {
    let logical = LogicalPath::parse(raw)?;
    if let Some((name, _)) = logical.shared_parts() {
        let share = shares::find_by_name(&state.db, name)
            .await?
            .ok_or(AppError::NotFound("path not found"))?;
        let access = crate::access::folder_access_by_id(&state.db, user, &share.id).await?;
        let perm = access.level.ok_or(AppError::NotFound("path not found"))?;
        if need == Need::Write && !perm.allows_write() {
            return Err(AppError::Forbidden("write permission required"));
        }
        let container = shares::validated_container(&share.container_path)?;
        roots.insert(
            name,
            ShareRoot {
                id: share.id,
                name: share.name,
                container,
            },
        )?;
    }
    Ok(logical)
}

/// The headers of an agent request: the shares the checks recorded.
fn agent_headers(roots: &ShareRoots) -> AppResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    roots.apply(&mut headers)?;
    Ok(headers)
}

/// The id of the shared folder a checked path is in.
fn share_id_of(roots: &ShareRoots, logical: &LogicalPath) -> Option<String> {
    let (name, _) = logical.shared_parts()?;
    roots.get(name).map(|root| root.id.clone())
}

/// Whether a logical path is in HOME (where the quota applies).
fn in_home(logical: &LogicalPath) -> bool {
    logical.segments().first().map(String::as_str) != Some("shared")
}

fn path_and_query(parts: &Parts) -> String {
    parts
        .uri
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string())
}

/// An entry the agent sent back (untrusted: its path must parse).
fn agent_entry(value: &Value) -> AppResult<EntryDto> {
    let entry: EntryDto = serde_json::from_value(value.clone())
        .map_err(|_| AppError::unavailable(format!("unexpected {AGENT} reply")))?;
    LogicalPath::parse(&entry.path)
        .map_err(|_| AppError::unavailable(format!("unexpected {AGENT} reply")))?;
    Ok(entry)
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

/// Headers of a streamed agent reply that reach the browser: those of a
/// file or JSON reply (see `storage::serve::serve_file`).
const PASSED_HEADERS: [header::HeaderName; 6] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::CONTENT_DISPOSITION,
    header::CACHE_CONTROL,
];

/// A streamed agent reply as it goes to the browser. The agent runs as the
/// user, whose other processes share its uid, so its replies are untrusted
/// (ADR 0005): only [`PASSED_HEADERS`] pass (no cookie, no redirect, no
/// security header of its own), and the server sets `nosniff` and the
/// sandboxing preview CSP itself, so no reply can run script on the RAST
/// origin. A status the browser would act on (401) or a redirect becomes
/// 502.
fn untrusted(response: Response<Body>) -> Response {
    let (mut parts, body) = response.into_parts();
    let mut headers = HeaderMap::new();
    for name in PASSED_HEADERS {
        for value in parts.headers.get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PREVIEW_CSP),
    );
    parts.headers = headers;
    parts.status = client::passed_status(parts.status);
    Response::from_parts(parts, body)
}

async fn publish(
    state: &AppState,
    user: &AuthUser,
    op: &str,
    paths: &[String],
    entry: Option<&EntryDto>,
    quota_changed: bool,
) {
    crate::realtime::publish_commit(
        &state.events,
        &state.db,
        user,
        op,
        paths,
        entry,
        quota_changed,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Query-string routes (streamed)
// ---------------------------------------------------------------------------

async fn forward(
    state: &AppState,
    user: AuthUser,
    route: &str,
    request: Request,
) -> AppResult<Response> {
    let (mut parts, body) = request.into_parts();
    let ip = ClientIp::from_request_parts(&mut parts, state).await?;
    // Only the server states the free quota and the shares' containers.
    parts.headers.remove(QUOTA_HEADER);
    parts.headers.remove(SHARES_HEADER);
    let mut roots = ShareRoots::default();
    let query: HashMap<String, String> = Query::try_from_uri(&parts.uri)
        .map(|Query(query)| query)
        .unwrap_or_default();
    let method = parts.method.clone();
    let uploading = route == "/api/files/upload" && method == Method::POST;
    let need = Need::Read;
    for key in ["path", "dir"] {
        if let Some(path) = query.get(key).filter(|_| !uploading) {
            check(state, &user, path, need, &mut roots).await?;
        }
    }

    // A single-request upload reserves its size first, like in-process.
    let mut reserved = 0i64;
    let mut share_reserved = None;
    if uploading {
        let dir = query.get("dir").map(String::as_str).unwrap_or("/");
        let logical = check(state, &user, dir, Need::Write, &mut roots).await?;
        let length = header_number(&parts.headers, header::CONTENT_LENGTH.as_str())
            .ok_or_else(|| AppError::bad_request("Content-Length is required for uploads"))?;
        if length > state.storage.max_upload_bytes {
            return Err(AppError::PayloadTooLarge(
                "file exceeds the maximum upload size",
            ));
        }
        if in_home(&logical) {
            let bytes = i64::try_from(length).unwrap_or(i64::MAX);
            if !quota::reserve(&state.db, user.id, bytes).await? {
                return Err(AppError::PayloadTooLarge("storage quota exceeded"));
            }
            reserved = bytes;
        } else if let Some(share_id) = share_id_of(&roots, &logical) {
            share_reserved =
                crate::storage::share_quota::reserve(&state.db, &state.storage, &share_id, length)
                    .await?;
        }
    }

    roots.apply(&mut parts.headers)?;
    let sent = state
        .homefs
        .send(
            user.id,
            method.clone(),
            &path_and_query(&parts),
            &parts.headers,
            body,
        )
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            if reserved > 0 {
                let _ = quota::rollback(&state.db, user.id, reserved).await;
            }
            return Err(error);
        }
    };
    let upload_id = query
        .get("uploadId")
        .or_else(|| query.get("upload_id"))
        .cloned()
        .unwrap_or_default();
    match (route, method.as_str()) {
        ("/api/files", "GET") => {
            let status = response.status();
            let mut listing = client::read_json(AGENT, response).await?;
            if status.is_success() {
                // Apps write into HOME without RAST: the fallback quota
                // follows a HOME scan, at most every ten minutes.
                trash::refresh_in_background(state, &user);
                let info = quota::info(&state.db, user.id).await?;
                listing["quota"] = serde_json::to_value(QuotaDto {
                    quota_bytes: info.quota_bytes,
                    used_bytes: info.used_bytes,
                    free_bytes: info.free_bytes(),
                })
                .unwrap_or(Value::Null);
            }
            Ok(json_response(status, listing))
        }
        ("/api/files/download", "GET") if response.status().is_success() => {
            // A ranged request is one download seen many times: log the start.
            let first = parts
                .headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|range| range.trim().starts_with("bytes=0-"));
            if first {
                let path = query.get("path").cloned().unwrap_or_default();
                audit_file(state, &user, &ip, "file.download", &path, String::new()).await;
            }
            Ok(untrusted(response))
        }
        ("/api/files/upload", "POST") => {
            let share = share_reserved.map(std::sync::Arc::new);
            finish_upload(state, &user, &ip, response, reserved, share).await
        }
        ("/api/files/upload/complete", "POST") => match state.homefs.take(&upload_id, user.id) {
            Some(reservation) => {
                finish_upload(
                    state,
                    &user,
                    &ip,
                    response,
                    reservation.bytes,
                    reservation.share,
                )
                .await
            }
            None => finish_upload(state, &user, &ip, response, 0, None).await,
        },
        ("/api/files/upload", "DELETE") => {
            if let Some(reservation) = state.homefs.take(&upload_id, user.id) {
                let _ = quota::rollback(&state.db, user.id, reservation.bytes).await;
            }
            Ok(untrusted(response))
        }
        _ => Ok(untrusted(response)),
    }
}

/// Account, audit and publish a finished upload; release its reservations
/// when it failed (a shared folder's goes when `share` is dropped without a
/// commit).
async fn finish_upload(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    response: Response<Body>,
    reserved: i64,
    share: Option<std::sync::Arc<crate::storage::share_quota::ShareReservation>>,
) -> AppResult<Response> {
    let status = response.status();
    let replaced = header_number(response.headers(), REPLACED_HEADER).unwrap_or(0);
    let release = || async {
        if reserved > 0 {
            let _ = quota::rollback(&state.db, user.id, reserved).await;
        }
    };
    let value = match client::read_json(AGENT, response).await {
        Ok(value) => value,
        Err(error) => {
            release().await;
            return Err(error);
        }
    };
    if !status.is_success() {
        release().await;
        return Ok(json_response(status, value));
    }
    let entry = match agent_entry(&value) {
        Ok(entry) => entry,
        Err(error) => {
            release().await;
            return Err(error);
        }
    };
    let home = LogicalPath::parse(&entry.path).is_ok_and(|logical| in_home(&logical));
    if let Some(share) = &share {
        let replaced = i64::try_from(replaced).unwrap_or(0);
        let growth = entry.size.max(0).saturating_sub(replaced).max(0) as u64;
        // What the agent says it wrote, never more than was reserved.
        share.commit(growth.min(share.bytes()));
    }
    if home {
        let replaced = i64::try_from(replaced).unwrap_or(0);
        quota::commit(
            &state.db,
            user.id,
            reserved,
            entry.size.max(0).saturating_sub(replaced),
        )
        .await?;
    } else if reserved > 0 {
        quota::rollback(&state.db, user.id, reserved).await?;
    }
    audit_file(
        state,
        user,
        ip,
        "file.upload",
        &entry.path,
        format!("{} bytes", entry.size),
    )
    .await;
    publish(
        state,
        user,
        "upload",
        std::slice::from_ref(&entry.path),
        Some(&entry),
        home,
    )
    .await;
    Ok(json_response(status, value))
}

// ---------------------------------------------------------------------------
// JSON routes
// ---------------------------------------------------------------------------

fn string_field<'a>(body: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str))
}

async fn json_request(
    state: &AppState,
    user: AuthUser,
    route: &str,
    request: Request,
) -> AppResult<Response> {
    let (mut parts, body) = request.into_parts();
    let ip = ClientIp::from_request_parts(&mut parts, state).await?;
    let bytes = axum::body::to_bytes(body, JSON_LIMIT)
        .await
        .map_err(|_| AppError::PayloadTooLarge("request body is too large"))?;
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| AppError::bad_request("invalid JSON body"))?;
    if !body.is_object() {
        return Err(AppError::bad_request("invalid JSON body"));
    }
    match route {
        "/api/files/mkdir" => {
            let target = match (
                string_field(&body, &["path"]),
                string_field(&body, &["parent"]),
                string_field(&body, &["name"]),
            ) {
                (Some(path), _, _) => path.to_string(),
                (None, Some(parent), Some(name)) => {
                    LogicalPath::parse(parent)?.child(name)?.as_string()
                }
                _ => return Err(AppError::bad_request("path (or parent + name) is required")),
            };
            let mut roots = ShareRoots::default();
            check(state, &user, &target, Need::Write, &mut roots).await?;
            let value = state
                .homefs
                .call(user.id, "/api/files/mkdir", &body, agent_headers(&roots)?)
                .await?;
            let entry = agent_entry(&value)?;
            audit_file(
                state,
                &user,
                &ip,
                "file.mkdir",
                &entry.path,
                "created folder".to_string(),
            )
            .await;
            publish(
                state,
                &user,
                "mkdir",
                std::slice::from_ref(&entry.path),
                Some(&entry),
                false,
            )
            .await;
            Ok(json_response(StatusCode::OK, value))
        }
        "/api/files/rename" => {
            let from = string_field(&body, &["path"])
                .ok_or_else(|| AppError::bad_request("path is required"))?;
            let mut roots = ShareRoots::default();
            let old = check(state, &user, from, Need::Write, &mut roots)
                .await?
                .as_string();
            let value = state
                .homefs
                .call(user.id, "/api/files/rename", &body, agent_headers(&roots)?)
                .await?;
            let entry = agent_entry(&value)?;
            audit_file(
                state,
                &user,
                &ip,
                "file.rename",
                &entry.path,
                format!("renamed from {from}"),
            )
            .await;
            publish(
                state,
                &user,
                "rename",
                &[old, entry.path.clone()],
                Some(&entry),
                false,
            )
            .await;
            Ok(json_response(StatusCode::OK, value))
        }
        "/api/files/upload/init" => upload_init(state, &user, &body).await,
        "/api/files/copy" => transfer(state, &user, &ip, body, false).await,
        "/api/files/move" => transfer(state, &user, &ip, body, true).await,
        _ => Err(AppError::NotFound("not found")),
    }
}

/// Reserve the size of a chunked upload into HOME, then let the agent
/// start it.
async fn upload_init(state: &AppState, user: &AuthUser, body: &Value) -> AppResult<Response> {
    let dir = string_field(body, &["dir"]).unwrap_or("/");
    let mut roots = ShareRoots::default();
    let logical = check(state, user, dir, Need::Write, &mut roots).await?;
    let size = body
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::bad_request("size is required"))?;
    if size > state.storage.max_upload_bytes {
        return Err(AppError::PayloadTooLarge(
            "file exceeds the maximum upload size",
        ));
    }
    // The limits of in-process uploads, before any quota is reserved.
    let slot = state.homefs.begin_upload(user.id)?;
    let mut reserved = 0i64;
    if in_home(&logical) {
        let bytes = i64::try_from(size).unwrap_or(i64::MAX);
        if !quota::reserve(&state.db, user.id, bytes).await? {
            return Err(AppError::PayloadTooLarge("storage quota exceeded"));
        }
        reserved = bytes;
    }
    let share = match share_id_of(&roots, &logical) {
        Some(share_id) if !in_home(&logical) => {
            crate::storage::share_quota::reserve(&state.db, &state.storage, &share_id, size).await?
        }
        _ => None,
    };
    let reply = state
        .homefs
        .call(
            user.id,
            "/api/files/upload/init",
            body,
            agent_headers(&roots)?,
        )
        .await;
    let value = match reply {
        Ok(value) => value,
        Err(error) => {
            if reserved > 0 {
                let _ = quota::rollback(&state.db, user.id, reserved).await;
            }
            return Err(error);
        }
    };
    let upload_id = value
        .get("uploadId")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with(crate::homefs::UPLOAD_PREFIX));
    match upload_id {
        Some(id) => {
            slot.register(id, reserved, share);
            Ok(json_response(StatusCode::OK, value))
        }
        None => {
            if reserved > 0 {
                let _ = quota::rollback(&state.db, user.id, reserved).await;
            }
            Err(AppError::unavailable(format!("unexpected {AGENT} reply")))
        }
    }
}

/// Copy and move run as background jobs. The agent does the work in one
/// request; cancelling the job drops it, which stops the agent.
async fn transfer(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    body: Value,
    moving: bool,
) -> AppResult<Response> {
    let (kind, verb, route) = if moving {
        ("file.move", "move", "/api/files/move")
    } else {
        ("file.copy", "copy", "/api/files/copy")
    };
    let paths: Vec<String> = body
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    if paths.len() > MAX_BATCH_PATHS {
        return Err(AppError::bad_request("too many paths in one request"));
    }
    let dest_raw = string_field(&body, &["destDir", "dest_dir"])
        .ok_or_else(|| AppError::bad_request("destDir is required"))?
        .to_string();
    let mut roots = ShareRoots::default();
    let dest = check(state, user, &dest_raw, Need::Write, &mut roots).await?;
    let need = if moving { Need::Write } else { Need::Read };
    let mut sources = Vec::with_capacity(paths.len());
    for path in &paths {
        sources.push(check(state, user, path, need, &mut roots).await?);
    }

    // What changes, for realtime: the sources of a move and the new names.
    let mut affected: Vec<String> = Vec::new();
    for source in &sources {
        if moving {
            affected.push(source.as_string());
        }
        if let Some(name) = source.name() {
            if let Ok(child) = dest.child(name) {
                affected.push(child.as_string());
            }
        }
    }
    let mut headers = agent_headers(&roots)?;
    if let Some(share_id) = share_id_of(&roots, &dest) {
        // A shared folder with a size limit (D3): what is still free.
        if let Some(free) =
            crate::storage::share_quota::free(&state.db, &state.storage, &share_id).await?
        {
            headers.insert(QUOTA_HEADER, HeaderValue::from(free));
        }
    } else if in_home(&dest) {
        let info = quota::info(&state.db, user.id).await?;
        // The rule of `quota::reserve`: administrators are unlimited.
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?;
        if !info.is_unlimited() && role.as_deref() != Some("admin") {
            headers.insert(QUOTA_HEADER, HeaderValue::from(info.free_bytes().max(0)));
        }
    }

    let count = paths.len();
    let mut detail = format!("{count} item(s) to {}: ", dest.as_string());
    detail.push_str(&paths.join(", "));
    if detail.len() > 900 {
        let mut end = 900;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push('…');
    }
    audit_file(state, user, ip, kind, &paths[0], detail).await;

    let job_state = state.clone();
    let actor = user.clone();
    let spec = JobSpec::new(kind, move |ctx| async move {
        ctx.progress(0, count as u64, &format!("{verb}: {count} item(s)"))
            .await;
        let request = job_state.homefs.call(actor.id, route, &body, headers);
        tokio::pin!(request);
        let mut result = loop {
            tokio::select! {
                result = &mut request => break result?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    ctx.check_cancelled()?;
                }
            }
        };
        let home_bytes = result
            .as_object_mut()
            .and_then(|object| object.remove("homeBytes"))
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        if home_bytes != 0 {
            quota::adjust_used(&job_state.db, actor.id, home_bytes).await?;
        }
        publish(&job_state, &actor, verb, &affected, None, home_bytes != 0).await;
        ctx.progress(count as u64, count as u64, "Done").await;
        Ok(result)
    })
    .actor(user)
    .message(format!("{verb} {count} item(s)"));
    let job = state.jobs.submit(spec).await?;
    Ok(json_response(
        StatusCode::OK,
        json!({"jobId": job, "items": count}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_paths_are_outside_home() {
        assert!(in_home(&LogicalPath::parse("/docs/a").unwrap()));
        assert!(in_home(&LogicalPath::parse("/").unwrap()));
        assert!(!in_home(&LogicalPath::parse("/shared/team/a").unwrap()));
        // A folder that merely looks similar is in HOME.
        assert!(in_home(&LogicalPath::parse("/sharedx").unwrap()));
    }

    #[test]
    fn streamed_agent_replies_keep_only_file_headers() {
        // What an agent taken over by another process of the user might send.
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html")
            .header(header::CONTENT_LENGTH, "4")
            .header(header::CONTENT_DISPOSITION, "inline")
            .header(header::SET_COOKIE, "rast_session=planted; Path=/")
            .header(header::LOCATION, "https://example.test/")
            .header(
                header::CONTENT_SECURITY_POLICY,
                "script-src 'unsafe-inline'",
            )
            .header(header::X_CONTENT_TYPE_OPTIONS, "sniff")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(REPLACED_HEADER, "9")
            .body(Body::from("<b/>"))
            .unwrap();
        let response = untrusted(response);
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "text/html");
        assert_eq!(headers[header::CONTENT_LENGTH], "4");
        assert_eq!(headers[header::CONTENT_DISPOSITION], "inline");
        // The server's own policy: sandboxed, no script, no sniffing.
        assert!(PREVIEW_CSP.starts_with("sandbox"));
        let csp: Vec<_> = headers
            .get_all(header::CONTENT_SECURITY_POLICY)
            .iter()
            .collect();
        assert_eq!(csp, [PREVIEW_CSP]);
        let nosniff: Vec<_> = headers
            .get_all(header::X_CONTENT_TYPE_OPTIONS)
            .iter()
            .collect();
        assert_eq!(nosniff, ["nosniff"]);
        for name in [
            header::SET_COOKIE,
            header::LOCATION,
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
        ] {
            assert!(!headers.contains_key(&name), "{name}");
        }
        assert!(!headers.contains_key(REPLACED_HEADER));
        assert_eq!(headers.len(), 5);
        // An agent cannot sign the browser out or redirect it.
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FOUND] {
            let response = Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap();
            assert_eq!(untrusted(response).status(), StatusCode::BAD_GATEWAY);
        }
    }

    #[test]
    fn agent_entries_are_checked() {
        let entry = json!({
            "name": "a.txt", "path": "/docs/a.txt", "kind": "file", "size": 3,
            "modifiedAt": 0, "mime": "text/plain", "readable": true,
            "writable": true, "trashed": false
        });
        assert_eq!(agent_entry(&entry).unwrap().size, 3);
        let mut bad = entry.clone();
        bad["path"] = json!("/docs/../etc");
        assert!(agent_entry(&bad).is_err());
        assert!(agent_entry(&json!({"path": "/x"})).is_err());
    }
}
