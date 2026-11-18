//! Static asset serving: the frontend is embedded in the binary for release
//! builds (and read from `frontend/dist` in debug builds). The Leptos/WASM UI
//! (`crates/rast-ui/dist`, built by Trunk) is embedded the same way and served
//! under `/ui/`.

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "../../frontend/dist/"]
struct Assets;

/// Output of `scripts/build-ui.sh` (Trunk). Served under [`UI_PREFIX`]. Trunk's
/// own `index.html` is only a build entry (it carries an inline loader that
/// the CSP forbids), so it is not embedded; the frontend imports the glue.
#[derive(RustEmbed)]
#[folder = "../../crates/rast-ui/dist/"]
#[exclude = "index.html"]
#[exclude = ".trunk-lock"]
struct UiAssets;

/// URL prefix of the embedded Leptos/WASM UI; matches `public_url` in
/// `crates/rast-ui/Trunk.toml`.
const UI_PREFIX: &str = "ui/";

const CSP: &str =
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
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

    if let Some(rest) = path.strip_prefix(UI_PREFIX) {
        return ui_response(rest, &headers);
    }

    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(asset) = Assets::get(candidate) {
        return asset_response(candidate, asset, cache_control(candidate), &headers);
    }
    let looks_like_file = candidate
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if !looks_like_file {
        if let Some(index) = Assets::get("index.html") {
            return asset_response("index.html", index, CACHE_NO_CACHE, &headers);
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
const CACHE_DEFAULT: &str = "public, max-age=3600";
const CACHE_NO_CACHE: &str = "no-cache";

/// Serve a file of the embedded WASM UI. The names are stable (no content
/// hash), so every response is revalidated with its ETag. An unknown path is a
/// plain 404, never the SPA shell, so a failed module import reports an error
/// instead of parsing HTML as JavaScript.
fn ui_response(path: &str, headers: &HeaderMap) -> Response {
    match UiAssets::get(path) {
        Some(asset) => asset_response(path, asset, CACHE_NO_CACHE, headers),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Cache policy of a frontend asset: hashed bundles are immutable, the shell
/// page is always revalidated, everything else is cached for an hour.
fn cache_control(path: &str) -> &'static str {
    if is_immutable(path) {
        CACHE_IMMUTABLE
    } else if path == "index.html" {
        CACHE_NO_CACHE
    } else {
        CACHE_DEFAULT
    }
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
    cache: &'static str,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header(response: &Response, name: header::HeaderName) -> &str {
        response.headers()[name].to_str().unwrap()
    }

    #[test]
    fn wasm_is_served_as_application_wasm() {
        // The build script always leaves an (empty) module here, real or not.
        let response = ui_response("rast-ui_bg.wasm", &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, header::CONTENT_TYPE), "application/wasm");
        assert_eq!(header(&response, header::CACHE_CONTROL), "no-cache");
        assert!(response.headers().contains_key(header::ETAG));
    }

    #[test]
    fn ui_assets_revalidate_with_the_etag() {
        let first = ui_response("rast-ui_bg.wasm", &HeaderMap::new());
        let etag = first.headers()[header::ETAG].clone();
        let mut request = HeaderMap::new();
        request.insert(header::IF_NONE_MATCH, etag);
        assert_eq!(
            ui_response("rast-ui_bg.wasm", &request).status(),
            StatusCode::NOT_MODIFIED
        );
    }

    #[test]
    fn unknown_ui_paths_are_404_not_the_spa_shell() {
        for path in ["missing.js", "", "nested/missing.wasm", "index.html"] {
            let response = ui_response(path, &HeaderMap::new());
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path {path:?}");
            assert_ne!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .map(|v| v.to_str().unwrap()),
                Some("text/html"),
                "path {path:?}"
            );
        }
    }

    #[test]
    fn the_csp_allows_wasm_compilation_but_not_more() {
        assert!(CSP.contains("default-src 'self'"));
        assert!(CSP.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(!CSP.contains("'unsafe-eval'"));
        let script_src = CSP
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src"))
            .unwrap();
        assert!(!script_src.contains("'unsafe-inline'"));
        assert!(!script_src.contains("http"));
    }

    #[test]
    fn frontend_cache_policy_is_unchanged() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(
            cache_control("assets/index-abc123.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("wallpapers/one.jpg"), "public, max-age=3600");
        assert_eq!(cache_control("boot-wallpaper.js"), "public, max-age=3600");
    }

    #[test]
    fn the_shell_page_carries_the_csp_header() {
        let index = Assets::get("index.html").expect("frontend index.html is embedded");
        let response = asset_response("index.html", index, CACHE_NO_CACHE, &HeaderMap::new());
        assert_eq!(header(&response, header::CONTENT_SECURITY_POLICY), CSP);
    }
}
