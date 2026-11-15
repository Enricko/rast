//! File Station's root file system: `/api/files/*` requests whose paths are
//! under `/@root` are answered by the root file agent, not by storage.
//!
//! [`dispatch`] sits in front of the File Station routes. A request that
//! names a root file system path must come from a super administrator; it is
//! audited and forwarded to the agent (see [`crate::rootfs`]) with its body
//! and the response streamed through. Copy, move and delete become
//! background jobs, like their storage counterparts. Everything else goes on
//! to the ordinary handlers untouched.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{FromRequestParts, Query, Request, State};
use axum::http::{header, request::Parts, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::files::audit_file;
use crate::auth::{AuthUser, ClientIp};
use crate::error::{AppError, AppResult};
use crate::jobs::JobSpec;
use crate::rootfs::{self, is_root_path, UPLOAD_PREFIX};
use crate::state::AppState;

/// JSON bodies of File Station requests are small (paths and names).
const JSON_LIMIT: usize = 2 * 1024 * 1024;

/// Routes whose paths travel in a JSON body.
const JSON_ROUTES: &[&str] = &[
    "/api/files/mkdir",
    "/api/files/rename",
    "/api/files/copy",
    "/api/files/move",
    "/api/files/delete",
    "/api/files/upload/init",
];

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

/// Every logical path a JSON body names.
fn body_paths(body: &Value) -> Vec<&str> {
    let mut out: Vec<&str> = ["path", "parent", "dir", "destDir", "dest_dir"]
        .iter()
        .filter_map(|key| body.get(key).and_then(Value::as_str))
        .collect();
    if let Some(paths) = body.get("paths").and_then(Value::as_array) {
        out.extend(paths.iter().filter_map(Value::as_str));
    }
    out
}

/// Whether the paths are on the root file system. Mixing it with storage in
/// one request is refused: the two are served by different processes.
fn all_root(paths: &[&str]) -> AppResult<bool> {
    let root = paths.iter().filter(|path| is_root_path(path)).count();
    if root == 0 {
        Ok(false)
    } else if root == paths.len() {
        Ok(true)
    } else {
        Err(AppError::bad_request(
            "copying or moving between the root file system and your storage is not \
             supported; download the file and upload it on the other side",
        ))
    }
}

pub async fn dispatch(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let route = request.uri().path().trim_end_matches('/').to_string();
    if QUERY_ROUTES.contains(&route.as_str()) {
        let query: HashMap<String, String> = Query::try_from_uri(request.uri())
            .map(|Query(query)| query)
            .unwrap_or_default();
        let named = ["path", "dir"]
            .iter()
            .any(|key| query.get(*key).is_some_and(|path| is_root_path(path)));
        let upload = ["uploadId", "upload_id"].iter().any(|key| {
            query
                .get(*key)
                .is_some_and(|id| id.starts_with(UPLOAD_PREFIX))
        });
        if named || upload {
            return forward(state, route, request)
                .await
                .unwrap_or_else(IntoResponse::into_response);
        }
        return next.run(request).await;
    }
    if !JSON_ROUTES.contains(&route.as_str()) || request.method() != Method::POST {
        return next.run(request).await;
    }
    // Read the (small) body to see which side it names, then hand it on.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, JSON_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return AppError::PayloadTooLarge("request body is too large").into_response(),
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match all_root(&body_paths(&value)) {
        Ok(false) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Ok(true) => json_request(state, route, parts, value)
            .await
            .unwrap_or_else(IntoResponse::into_response),
        Err(error) => error.into_response(),
    }
}

/// The caller, once it is known to be a super administrator.
async fn authorize(state: &AppState, parts: &mut Parts) -> AppResult<(AuthUser, ClientIp)> {
    let user = parts
        .extensions
        .get::<AuthUser>()
        .cloned()
        .ok_or(AppError::Unauthorized("authentication required"))?;
    if !state.rootfs.enabled() {
        return Err(AppError::Forbidden(
            "the root file system is turned off on this server",
        ));
    }
    crate::auth::require_super_admin(&state.db, &user).await?;
    let ip = ClientIp::from_request_parts(parts, state).await?;
    Ok((user, ip))
}

fn path_and_query(parts: &Parts) -> String {
    parts
        .uri
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string())
}

/// Forward a request whose path is in the query string; the body (an upload)
/// and the response (a download) are streamed.
async fn forward(state: AppState, route: String, request: Request) -> AppResult<Response> {
    let (mut parts, body) = request.into_parts();
    let (user, ip) = authorize(&state, &mut parts).await?;
    let response = state
        .rootfs
        .send(
            parts.method.clone(),
            &path_and_query(&parts),
            &parts.headers,
            body,
        )
        .await?;
    let query: HashMap<String, String> = Query::try_from_uri(&parts.uri)
        .map(|Query(query)| query)
        .unwrap_or_default();
    match (route.as_str(), parts.method.as_str()) {
        ("/api/files/download", "GET") if response.status().is_success() => {
            // A ranged request is one download seen many times: log the start.
            let first = parts
                .headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|range| range.trim().starts_with("bytes=0-"));
            if first {
                let path = query.get("path").cloned().unwrap_or_default();
                audit_file(&state, &user, &ip, "file.download", &path, String::new()).await;
            }
            Ok(response)
        }
        ("/api/files/upload", "POST") | ("/api/files/upload/complete", "POST") => {
            audited_entry(&state, &user, &ip, "file.upload", response, |entry| {
                format!(
                    "{} bytes",
                    entry.get("size").and_then(Value::as_i64).unwrap_or(0)
                )
            })
            .await
        }
        _ => Ok(response),
    }
}

/// Pass an entry reply on, recording it in the activity log when it
/// succeeded.
async fn audited_entry(
    state: &AppState,
    user: &AuthUser,
    ip: &ClientIp,
    action: &str,
    response: Response<Body>,
    detail: impl FnOnce(&Value) -> String,
) -> AppResult<Response> {
    let status = response.status();
    let entry = rootfs::read_json(response).await?;
    if status.is_success() {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        audit_file(state, user, ip, action, path, detail(&entry)).await;
    }
    Ok((status, Json(entry)).into_response())
}

/// A request whose paths are in its JSON body.
async fn json_request(
    state: AppState,
    route: String,
    mut parts: Parts,
    body: Value,
) -> AppResult<Response> {
    let (user, ip) = authorize(&state, &mut parts).await?;
    let (kind, verb) = match route.as_str() {
        "/api/files/copy" => ("file.copy", "copy"),
        "/api/files/move" => ("file.move", "move"),
        "/api/files/delete" => ("file.delete", "delete"),
        _ => {
            let response = state
                .rootfs
                .send(
                    Method::POST,
                    &path_and_query(&parts),
                    &parts.headers,
                    Body::from(body.to_string()),
                )
                .await?;
            return match route.as_str() {
                "/api/files/mkdir" => {
                    audited_entry(&state, &user, &ip, "file.mkdir", response, |_| {
                        "created folder".to_string()
                    })
                    .await
                }
                "/api/files/rename" => {
                    let from = body
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    audited_entry(&state, &user, &ip, "file.rename", response, |_| {
                        format!("renamed from {from}")
                    })
                    .await
                }
                _ => Ok(response),
            };
        }
    };
    // Copy, move and delete run as background jobs. The agent does the work
    // in one request; cancelling the job drops it, which stops the agent.
    let paths: Vec<String> = body_paths(&json!({"paths": body.get("paths")}))
        .into_iter()
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        return Err(AppError::bad_request("at least one path is required"));
    }
    let count = paths.len();
    let dest = body
        .get("destDir")
        .or_else(|| body.get("dest_dir"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut detail = match &dest {
        Some(dest) => format!("{count} item(s) to {dest}: "),
        None => format!("{count} item(s), permanently: "),
    };
    detail.push_str(&paths.join(", "));
    if detail.len() > 900 {
        let mut end = 900;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push('…');
    }
    audit_file(&state, &user, &ip, kind, &paths[0], detail).await;

    let rootfs = state.rootfs.clone();
    let agent_route = route.clone();
    let spec = JobSpec::new(kind, move |ctx| async move {
        ctx.progress(0, count as u64, &format!("{verb}: {count} item(s)"))
            .await;
        let request = rootfs.call(&agent_route, &body);
        tokio::pin!(request);
        let result = loop {
            tokio::select! {
                result = &mut request => break result?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    ctx.check_cancelled()?;
                }
            }
        };
        ctx.progress(count as u64, count as u64, "Done").await;
        Ok(result)
    })
    .actor(&user)
    .message(format!("{verb} {count} item(s)"));
    let job = state.jobs.submit(spec).await?;
    Ok((StatusCode::OK, Json(json!({"jobId": job, "items": count}))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_sorted_by_the_side_they_name() {
        let storage = json!({"paths": ["/a", "/shared/team/b"], "destDir": "/c"});
        assert!(!all_root(&body_paths(&storage)).unwrap());
        let root = json!({"paths": ["/@root/etc/a"], "destDir": "/@root/tmp"});
        assert!(all_root(&body_paths(&root)).unwrap());
        let mixed = json!({"paths": ["/@root/etc/a"], "destDir": "/docs"});
        assert!(all_root(&body_paths(&mixed)).is_err());
        // A folder that merely looks similar stays in storage.
        let lookalike = json!({"path": "/@rootx/a"});
        assert!(!all_root(&body_paths(&lookalike)).unwrap());
        assert!(!all_root(&[]).unwrap());
    }
}
