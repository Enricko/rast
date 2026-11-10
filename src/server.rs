//! Static asset serving: the frontend is embedded in the binary for release
//! builds (and read from `frontend/dist` in debug builds).

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct Assets;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; media-src 'self' blob:; \
frame-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Fallback handler for every non-API route: serve the asset when it exists,
/// otherwise serve the SPA shell so client-side routes survive a refresh.
pub async fn static_files(
    State(_state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": {"code": "not_found", "message": "endpoint not found"}
            })),
        )
            .into_response();
    }

    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(asset) = Assets::get(candidate) {
        return asset_response(candidate, asset, is_immutable(candidate), &headers);
    }
    let looks_like_file = candidate
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if !looks_like_file {
        if let Some(index) = Assets::get("index.html") {
            return asset_response("index.html", index, false, &headers);
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn is_immutable(path: &str) -> bool {
    path.starts_with("assets/")
        && path.rsplit('.').next().is_some_and(|ext| {
            matches!(
                ext,
                "js" | "css" | "woff2" | "woff" | "png" | "jpg" | "jpeg" | "svg" | "webp" | "map"
            )
        })
}

fn asset_response(
    path: &str,
    asset: rust_embed::EmbeddedFile,
    immutable: bool,
    request_headers: &HeaderMap,
) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let hash = asset.metadata.sha256_hash();
    let etag = format!("\"{}\"", hex::encode(&hash[..8]));

    // Conditional request support (used by the service worker / browser cache).
    if let Some(inm) = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm.split(',').any(|candidate| candidate.trim() == etag) {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            response.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"0\"")),
            );
            return response;
        }
    }

    let cache: &'static str = if immutable {
        "public, max-age=31536000, immutable"
    } else if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=3600"
    };

    let mut response = asset.data.into_owned().into_response();
    *response.status_mut() = StatusCode::OK;
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"0\"")),
    );
    if mime.type_() == mime_guess::mime::TEXT && mime.subtype() == mime_guess::mime::HTML {
        response_headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
    }
    response
}
