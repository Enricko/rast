//! Appearance API: per-user theme + wallpaper preferences and private
//! wallpaper uploads.
//!
//! All routes are mounted in the protected router (authentication + CSRF
//! middleware). Wallpaper files are only ever served to the user who uploaded
//! them: even administrators get a 404 for someone else's upload.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::io::ReaderStream;

use crate::{
    appearance,
    audit::{self, AuditLog},
    auth::{AuthUser, ClientIp},
    error::{AppError, AppResult},
    state::AppState,
};

/// Immutable per-user wallpaper cache header (content-addressed by UUID).
const WALLPAPER_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// Notify the user's other sessions (and their own open tabs) that appearance
/// changed. The originating session id lets a preview dialog distinguish its own
/// save from an external change.
fn publish_appearance_changed(
    state: &AppState,
    user: &AuthUser,
    reason: &str,
    appearance: Option<&appearance::AppearanceDto>,
) {
    let data = serde_json::json!({
        "reason": reason,
        "originSessionId": user.session_id,
        "appearance": appearance,
    });
    state
        .events
        .publish_user("appearance.changed", user.id, data);
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/appearance",
            get(get_appearance).put(update_appearance),
        )
        .route(
            "/api/appearance/wallpapers",
            get(list_wallpapers).post(upload_wallpaper),
        )
        .route(
            "/api/appearance/wallpapers/{id}",
            get(serve_wallpaper).delete(delete_wallpaper),
        )
        .route("/api/appearance/reset", post(reset_appearance))
}

#[derive(Debug, Default, Deserialize)]
pub struct UploadWallpaperQuery {
    /// Optional display name; never used as a path. Sanitised before storage.
    #[serde(default)]
    pub name: Option<String>,
}

/// GET /api/appearance
pub async fn get_appearance(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<appearance::AppearanceDto>> {
    Ok(Json(appearance::get(&state.db, user.id).await?))
}

/// PUT /api/appearance
pub async fn update_appearance(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Json(body): Json<appearance::UpdateAppearanceRequest>,
) -> AppResult<Json<appearance::AppearanceDto>> {
    let updated = appearance::update(&state.db, &user, body).await?;
    publish_appearance_changed(&state, &user, "update", Some(&updated));
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("appearance.update")
            .actor(&user)
            .object("appearance", user.id.to_string())
            .success()
            .detail(format!(
                "theme={} wallpaper={}",
                updated.theme, updated.wallpaper.kind
            ))
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(updated))
}

/// GET /api/appearance/wallpapers
pub async fn list_wallpapers(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Value>> {
    let items = appearance::list_uploads(&state.db, user.id).await?;
    Ok(Json(json!({ "items": items })))
}

/// POST /api/appearance/wallpapers — raw image body.
pub async fn upload_wallpaper(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Query(query): Query<UploadWallpaperQuery>,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let base_mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !matches!(
        base_mime.as_str(),
        appearance::MIME_JPEG | appearance::MIME_PNG | appearance::MIME_WEBP
    ) {
        return Err(AppError::UnsupportedMediaType(
            "wallpaper must be a JPEG, PNG or WebP image",
        ));
    }
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AppError::bad_request("Content-Length is required"))?;
    if content_length > appearance::MAX_UPLOAD_BYTES {
        return Err(AppError::PayloadTooLarge(
            "wallpaper is larger than the 10 MiB limit",
        ));
    }
    // Bounded read: at most MAX_UPLOAD_BYTES + 1 byte is ever buffered.
    let mut data: Vec<u8> = Vec::with_capacity(content_length as usize);
    let mut stream = body.into_data_stream();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(bytes) => {
                if data.len() as u64 + bytes.len() as u64 > appearance::MAX_UPLOAD_BYTES {
                    return Err(AppError::PayloadTooLarge(
                        "wallpaper is larger than the 10 MiB limit",
                    ));
                }
                data.extend_from_slice(&bytes);
            }
            Err(error) => {
                return Err(AppError::bad_request(format!(
                    "upload stream failed: {error}"
                )))
            }
        }
    }
    if data.len() as u64 != content_length {
        return Err(AppError::bad_request(
            "upload ended before the complete body was received",
        ));
    }
    let upload = appearance::store_upload(
        &state.db,
        &state.config.data_dir,
        &user,
        &base_mime,
        query.name.as_deref(),
        &data,
    )
    .await?;
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("appearance.wallpaper_upload")
            .actor(&user)
            .object("wallpaper", upload.id.clone())
            .success()
            .detail(format!(
                "{} {}x{} {} bytes",
                upload.mime, upload.width, upload.height, upload.size_bytes
            ))
            .ip(ip.as_string()),
    )
    .await;
    Ok((StatusCode::CREATED, Json(upload)).into_response())
}

fn etag_for(id: &str) -> String {
    format!("\"{id}\"")
}

fn insert_etag_headers(headers: &mut HeaderMap, etag: &str) {
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(WALLPAPER_CACHE_CONTROL),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        if candidate == "*" {
            return true;
        }
        candidate.trim_start_matches("W/").trim() == etag
    })
}

/// GET /api/appearance/wallpapers/{id} — owner only.
pub async fn serve_wallpaper(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let upload =
        appearance::upload_for_serving(&state.db, &state.config.data_dir, &user, &id).await?;
    let etag = etag_for(&upload.id);
    if if_none_match_matches(&headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        insert_etag_headers(response.headers_mut(), &etag);
        return Ok(response);
    }
    let file = crate::storage::safe_path::open_read_no_follow_async(&upload.path).await?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return Err(AppError::NotFound("wallpaper not found"));
    }
    let length = meta.len();
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    *response.status_mut() = StatusCode::OK;
    let response_headers = response.headers_mut();
    match HeaderValue::from_str(&upload.mime) {
        Ok(value) => {
            response_headers.insert(header::CONTENT_TYPE, value);
        }
        Err(_) => {
            response_headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
    }
    match HeaderValue::from_str(&length.to_string()) {
        Ok(value) => {
            response_headers.insert(header::CONTENT_LENGTH, value);
        }
        Err(_) => {
            response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        }
    }
    response_headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    insert_etag_headers(response_headers, &etag);
    Ok(response)
}

/// DELETE /api/appearance/wallpapers/{id} — owner only.
pub async fn delete_wallpaper(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let updated = appearance::delete_upload(&state.db, &state.config.data_dir, &user, &id).await?;
    let current = appearance::get(&state.db, user.id).await.ok();
    publish_appearance_changed(&state, &user, "delete", current.as_ref());
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("appearance.wallpaper_delete")
            .actor(&user)
            .object("wallpaper", id)
            .success()
            .detail("uploaded wallpaper deleted")
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(json!({"ok": true, "appearance": updated})))
}

/// POST /api/appearance/reset — reset the wallpaper, keep the theme.
pub async fn reset_appearance(
    State(state): State<AppState>,
    user: AuthUser,
    ip: ClientIp,
) -> AppResult<Json<appearance::AppearanceDto>> {
    let updated = appearance::reset(&state.db, &user).await?;
    publish_appearance_changed(&state, &user, "reset", Some(&updated));
    audit::record_with_events(
        &state.db,
        &state.events,
        AuditLog::new("appearance.reset")
            .actor(&user)
            .object("appearance", user.id.to_string())
            .success()
            .detail("wallpaper reset to default")
            .ip(ip.as_string()),
    )
    .await;
    Ok(Json(updated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        appearance::tests::{png_fixture, webp_lossy_fixture},
        auth::Role,
        config::{Config, FileConfig},
        state::AppState,
    };
    use axum::{
        body::Body,
        http::{Method, Request},
        Extension,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

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

    async fn state_with_users(role: Role) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(dir.path())).await.unwrap();
        for (id, username, user_role) in [
            (1_i64, "alice", role.as_str()),
            (2, "bob", "user"),
            (3, "admin2", "admin"),
        ] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', ?, 1, 1)",
            )
            .bind(id)
            .bind(username)
            .bind(user_role)
            .execute(&state.db)
            .await
            .unwrap();
        }
        (dir, state)
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers
    }

    async fn request(
        app: &Router,
        method: Method,
        uri: &str,
        headers: HeaderMap,
        body: Body,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let response = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    async fn json_call(
        app: &Router,
        method: Method,
        uri: &str,
        value: Value,
    ) -> (StatusCode, Value) {
        let (status, _, bytes) = request(
            app,
            method,
            uri,
            json_headers(),
            Body::from(value.to_string()),
        )
        .await;
        let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, parsed)
    }

    async fn app_for(state: &AppState, user: AuthUser) -> Router {
        routes().layer(Extension(user)).with_state(state.clone())
    }

    async fn upload_png(app: &Router, image: &[u8]) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&image.len().to_string()).unwrap(),
        );
        let (status, _, bytes) = request(
            app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(image.to_vec()),
        )
        .await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn defaults_roundtrip_and_validation() {
        let (_dir, state) = state_with_users(Role::User).await;
        let alice = auth_user(1, "alice", Role::User);
        let app = app_for(&state, alice.clone()).await;

        let (status, body) = json_call(&app, Method::GET, "/api/appearance", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["theme"], "light");
        assert_eq!(body["wallpaper"]["kind"], "builtin");
        assert_eq!(body["wallpaper"]["builtinId"], "alpine-dawn");
        assert_eq!(body["wallpaper"]["uploadId"], Value::Null);
        assert_eq!(body["wallpaper"]["color"], "#1B2430");
        assert_eq!(body["wallpaper"]["fit"], "cover");
        assert_eq!(body["wallpaper"]["position"], "center");
        assert_eq!(body["wallpaper"]["overlay"], 20);
        assert_eq!(body["uploads"], json!([]));
        assert_eq!(body["limits"]["maxBytes"], appearance::MAX_UPLOAD_BYTES);
        assert_eq!(
            body["limits"]["maxUploads"],
            appearance::MAX_UPLOADS_PER_USER
        );

        let (status, body) = json_call(
            &app,
            Method::PUT,
            "/api/appearance",
            json!({
                "theme": "dark",
                "wallpaper": {
                    "kind": "builtin",
                    "builtinId": "forest-mist",
                    "uploadId": null,
                    "color": "#0a0b0c",
                    "fit": "contain",
                    "position": "bottom-right",
                    "overlay": 35
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["theme"], "dark");
        assert_eq!(body["wallpaper"]["builtinId"], "forest-mist");
        assert_eq!(body["wallpaper"]["color"], "#0A0B0C");
        assert_eq!(body["wallpaper"]["fit"], "contain");
        assert_eq!(body["wallpaper"]["position"], "bottom-right");
        assert_eq!(body["wallpaper"]["overlay"], 35);

        // Persisted across requests.
        let (status, body) = json_call(&app, Method::GET, "/api/appearance", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["theme"], "dark");
        assert_eq!(body["wallpaper"]["builtinId"], "forest-mist");

        // Invalid payloads are rejected with our JSON error shape.
        for payload in [
            json!({"theme": "neon"}),
            json!({"wallpaper": {"kind": "sticker"}}),
            json!({"wallpaper": {"builtinId": "unknown-wallpaper"}}),
            json!({"wallpaper": {"fit": "stretch"}}),
            json!({"wallpaper": {"position": "middle"}}),
            json!({"wallpaper": {"color": "blue"}}),
            json!({"wallpaper": {"overlay": 61}}),
            json!({"wallpaper": {"overlay": -1}}),
            json!({"wallpaper": {"kind": "upload"}}),
        ] {
            let (status, body) = json_call(&app, Method::PUT, "/api/appearance", payload).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "payload should be rejected: {body}"
            );
            assert!(body["error"]["message"].is_string());
        }

        // Reset keeps the theme but restores wallpaper defaults.
        let (status, _body) = json_call(
            &app,
            Method::PUT,
            "/api/appearance",
            json!({"theme": "system"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, bytes) = request(
            &app,
            Method::POST,
            "/api/appearance/reset",
            json_headers(),
            Body::from("{}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["theme"], "system");
        assert_eq!(body["wallpaper"]["kind"], "builtin");
        assert_eq!(body["wallpaper"]["builtinId"], "alpine-dawn");

        // Mutations were audited.
        let actions: Vec<String> =
            sqlx::query_scalar("SELECT action FROM audit_logs WHERE user_id = 1 ORDER BY id")
                .fetch_all(&state.db)
                .await
                .unwrap();
        assert!(actions.iter().any(|action| action == "appearance.update"));
        assert!(actions.iter().any(|action| action == "appearance.reset"));
    }

    #[tokio::test]
    async fn wallpaper_upload_serve_privacy_and_delete() {
        let (_dir, state) = state_with_users(Role::User).await;
        let image = png_fixture(1280, 720);
        let alice = auth_user(1, "alice", Role::User);
        let bob = auth_user(2, "bob", Role::User);
        let admin = auth_user(3, "admin2", Role::Admin);
        let alice_app = app_for(&state, alice.clone()).await;
        let bob_app = app_for(&state, bob.clone()).await;
        let admin_app = app_for(&state, admin.clone()).await;

        let (status, upload) = upload_png(&alice_app, &image).await;
        assert_eq!(status, StatusCode::CREATED, "{upload}");
        let id = upload["id"].as_str().unwrap().to_string();
        assert_eq!(upload["width"], 1280);
        assert_eq!(upload["height"], 720);
        assert_eq!(upload["sizeBytes"], image.len());
        assert_eq!(upload["mime"], "image/png");
        assert_eq!(upload["url"], format!("/api/appearance/wallpapers/{id}"));
        assert!(upload["createdAt"].as_i64().unwrap() > 0);

        // The owner can stream the exact bytes back.
        let (status, headers, bytes) = request(
            &alice_app,
            Method::GET,
            &format!("/api/appearance/wallpapers/{id}"),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, image);
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::CONTENT_DISPOSITION], "inline");
        let cache = headers[header::CACHE_CONTROL].to_str().unwrap();
        assert!(cache.contains("private"));
        assert!(cache.contains("max-age=31536000"));
        assert!(cache.contains("immutable"));
        let etag = headers[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(etag, format!("\"{id}\""));
        assert_eq!(
            headers[header::CONTENT_LENGTH].to_str().unwrap(),
            image.len().to_string()
        );

        // Conditional request -> 304 with the same ETag.
        let mut conditional = HeaderMap::new();
        conditional.insert(header::IF_NONE_MATCH, HeaderValue::from_str(&etag).unwrap());
        let (status, headers, bytes) = request(
            &alice_app,
            Method::GET,
            &format!("/api/appearance/wallpapers/{id}"),
            conditional,
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(bytes.is_empty());
        assert_eq!(headers[header::ETAG], etag);

        // Other users (including admins) must not be able to fetch it.
        for other in [&bob_app, &admin_app] {
            let (status, _, _) = request(
                other,
                Method::GET,
                &format!("/api/appearance/wallpapers/{id}"),
                HeaderMap::new(),
                Body::empty(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        // Traversal-looking ids are just 404s.
        let (status, _, _) = request(
            &alice_app,
            Method::GET,
            "/api/appearance/wallpapers/..%2F..%2Fetc",
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Listing is owner-scoped.
        let (status, body) = json_call(
            &alice_app,
            Method::GET,
            "/api/appearance/wallpapers",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], id);
        let (status, body) = json_call(
            &bob_app,
            Method::GET,
            "/api/appearance/wallpapers",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);

        // Activate it, then delete it: the appearance falls back to builtin.
        let (status, body) = json_call(
            &alice_app,
            Method::PUT,
            "/api/appearance",
            json!({
                "theme": "dark",
                "wallpaper": {"kind": "upload", "uploadId": id}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["wallpaper"]["kind"], "upload");
        assert_eq!(body["wallpaper"]["uploadId"], id);

        // Bob cannot delete Alice's upload.
        let (status, _, _) = request(
            &bob_app,
            Method::DELETE,
            &format!("/api/appearance/wallpapers/{id}"),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, headers, bytes) = request(
            &alice_app,
            Method::DELETE,
            &format!("/api/appearance/wallpapers/{id}"),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["appearance"]["theme"], "dark");
        assert_eq!(body["appearance"]["wallpaper"]["kind"], "builtin");
        assert_eq!(body["appearance"]["wallpaper"]["uploadId"], Value::Null);
        assert!(body["appearance"]["uploads"].as_array().unwrap().is_empty());
        let _ = headers;
        // The file is gone for good.
        let (status, _, _) = request(
            &alice_app,
            Method::GET,
            &format!("/api/appearance/wallpapers/{id}"),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Audit trail.
        let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_logs ORDER BY id")
            .fetch_all(&state.db)
            .await
            .unwrap();
        assert!(actions
            .iter()
            .any(|action| action == "appearance.wallpaper_upload"));
        assert!(actions
            .iter()
            .any(|action| action == "appearance.wallpaper_delete"));
    }

    #[tokio::test]
    async fn upload_validation_and_limits() {
        let (_dir, state) = state_with_users(Role::User).await;
        let alice = auth_user(1, "alice", Role::User);
        let app = app_for(&state, alice).await;
        let png = png_fixture(800, 600);

        // Unsupported declared type.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/gif"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("10"));
        let (status, _, _) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(vec![0u8; 10]),
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Missing Content-Length.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        let (status, _, _) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(png.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Declared length over the 10 MiB limit is rejected before reading.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&(appearance::MAX_UPLOAD_BYTES + 1).to_string()).unwrap(),
        );
        let (status, _, _) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(vec![0u8; 8]),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

        // Declared MIME and actual bytes must agree.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&png.len().to_string()).unwrap(),
        );
        let (status, _, body) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(png.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Content-Type"));

        // Corrupt image bytes.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("6"));
        let (status, _, _) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(b"nope!!".to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Truncated body relative to Content-Length.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&(png.len() + 10).to_string()).unwrap(),
        );
        let (status, _, _) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(png.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // A valid WebP upload works too.
        let webp = webp_lossy_fixture(1920, 1080);
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&webp.len().to_string()).unwrap(),
        );
        let (status, _, body) = request(
            &app,
            Method::POST,
            "/api/appearance/wallpapers",
            headers,
            Body::from(webp),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{:?}",
            String::from_utf8_lossy(&body)
        );
    }

    #[tokio::test]
    async fn upload_cannot_reference_another_users_wallpaper() {
        let (_dir, state) = state_with_users(Role::User).await;
        let alice = auth_user(1, "alice", Role::User);
        let bob = auth_user(2, "bob", Role::User);
        let alice_app = app_for(&state, alice).await;
        let bob_app = app_for(&state, bob).await;
        let (status, upload) = upload_png(&alice_app, &png_fixture(800, 600)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = upload["id"].as_str().unwrap().to_string();
        let (status, body) = json_call(
            &bob_app,
            Method::PUT,
            "/api/appearance",
            json!({"wallpaper": {"kind": "upload", "uploadId": id}}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        // No appearance row was written for Bob's failed attempt.
        let (status, body) = json_call(&bob_app, Method::GET, "/api/appearance", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["wallpaper"]["kind"], "builtin");
    }
}
