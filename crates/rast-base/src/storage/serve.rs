//! Streaming a regular file over HTTP: `Content-Disposition`, `Range` support
//! and the sandboxing CSP for inline previews. Shared by the File Station API
//! and the root file agent.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::{AppError, AppResult};
use crate::storage::{safe_path, ResolvedPath};

/// Sandboxing CSP for inline previews. `sandbox` without `allow-scripts` means
/// even an SVG/HTML payload that somehow got a markup content type cannot run
/// script in the application origin or read the session cookie.
pub const PREVIEW_CSP: &str = "sandbox; default-src 'none'; img-src 'self' data: blob:; \
style-src 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'none'; \
frame-ancestors 'self'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Attachment,
    Inline,
}

fn content_disposition(kind: Disposition, file_name: &str) -> HeaderValue {
    let kind = match kind {
        Disposition::Attachment => "attachment",
        Disposition::Inline => "inline",
    };
    // Build a header-safe ASCII fallback and an RFC 5987 encoded name. Control
    // characters, quotes and path separators can never reach the header.
    let ascii: String = file_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || " ._()-[]{}".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded =
        percent_encoding::utf8_percent_encode(file_name, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""))
}

enum RangeRequest {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn parse_range(value: Option<&str>, len: u64) -> RangeRequest {
    let Some(value) = value else {
        return RangeRequest::Full;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        // Multiple ranges are legal to ignore; serve the full representation.
        return RangeRequest::Full;
    }
    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    if len == 0 {
        return RangeRequest::Unsatisfiable;
    }
    if start_raw.trim().is_empty() {
        let Ok(suffix) = end_raw.trim().parse::<u64>() else {
            return RangeRequest::Full;
        };
        if suffix == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }
    let Ok(start) = start_raw.trim().parse::<u64>() else {
        return RangeRequest::Full;
    };
    if start >= len {
        return RangeRequest::Unsatisfiable;
    }
    let end = if end_raw.trim().is_empty() {
        len - 1
    } else {
        match end_raw.trim().parse::<u64>() {
            Ok(end) => end.min(len - 1),
            Err(_) => return RangeRequest::Full,
        }
    };
    if start > end {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Partial { start, end }
}

fn numeric_header(value: u64) -> HeaderValue {
    HeaderValue::from_str(&value.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

/// Stream a resolved file with optional `Range` support.
pub async fn serve_file(
    resolved: &ResolvedPath,
    request_headers: &HeaderMap,
    content_type: &str,
    disposition: Disposition,
    csp: Option<&'static str>,
) -> AppResult<Response> {
    serve_path(
        &resolved.physical,
        resolved.name(),
        request_headers,
        content_type,
        disposition,
        csp,
    )
    .await
}

/// Stream the regular file at `physical` (never through a symbolic link)
/// under the download name `file_name`, with optional `Range` support.
pub async fn serve_path(
    physical: &std::path::Path,
    file_name: &str,
    request_headers: &HeaderMap,
    content_type: &str,
    disposition: Disposition,
    csp: Option<&'static str>,
) -> AppResult<Response> {
    let meta = tokio::fs::symlink_metadata(physical).await?;
    if !meta.is_file() {
        return Err(AppError::bad_request("path is not a regular file"));
    }
    let Some(part) = requested_part(request_headers, meta.len()) else {
        return Ok(unsatisfiable(meta.len()));
    };
    // Reopen-safe: O_NOFOLLOW (Linux) + inode comparison against the earlier
    // lstat, so a symlink swapped in after resolution cannot leak data.
    let file = crate::storage::safe_path::open_read_no_follow_async(physical).await?;
    let opened_meta = file.metadata().await?;
    safe_path::verify_same_inode(&opened_meta, &meta)?;
    respond(file, part, file_name, content_type, disposition, csp).await
}

/// Stream a regular file that the caller has already opened (and checked)
/// and whose size is `len`, with optional `Range` support. The per-user file
/// agent opens files relative to a directory descriptor.
pub async fn serve_open_file(
    file: tokio::fs::File,
    len: u64,
    file_name: &str,
    request_headers: &HeaderMap,
    content_type: &str,
    disposition: Disposition,
    csp: Option<&'static str>,
) -> AppResult<Response> {
    let Some(part) = requested_part(request_headers, len) else {
        return Ok(unsatisfiable(len));
    };
    respond(file, part, file_name, content_type, disposition, csp).await
}

/// The part of a file of `len` bytes that a request asks for.
struct Part {
    status: StatusCode,
    start: u64,
    length: u64,
    content_range: Option<String>,
}

/// The part a request's `Range` asks for (`None`: not satisfiable).
fn requested_part(request_headers: &HeaderMap, len: u64) -> Option<Part> {
    match parse_range(
        request_headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        len,
    ) {
        RangeRequest::Full => Some(Part {
            status: StatusCode::OK,
            start: 0,
            length: len,
            content_range: None,
        }),
        RangeRequest::Partial { start, end } => Some(Part {
            status: StatusCode::PARTIAL_CONTENT,
            start,
            length: end - start + 1,
            content_range: Some(format!("bytes {start}-{end}/{len}")),
        }),
        RangeRequest::Unsatisfiable => None,
    }
}

/// The `416` reply for a file of `len` bytes.
fn unsatisfiable(len: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn respond(
    mut file: tokio::fs::File,
    part: Part,
    file_name: &str,
    content_type: &str,
    disposition: Disposition,
    csp: Option<&'static str>,
) -> AppResult<Response> {
    let Part {
        status,
        start,
        length,
        content_range,
    } = part;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let stream = ReaderStream::new(file.take(length));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    match HeaderValue::from_str(content_type) {
        Ok(value) => {
            headers.insert(header::CONTENT_TYPE, value);
        }
        Err(_) => {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
    }
    headers.insert(header::CONTENT_LENGTH, numeric_header(length));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(disposition, file_name),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(range) = content_range {
        if let Ok(value) = HeaderValue::from_str(&range) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    if let Some(csp) = csp {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parser_covers_edge_cases() {
        assert!(matches!(parse_range(None, 10), RangeRequest::Full));
        match parse_range(Some("bytes=0-4"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (0, 4)),
            _ => panic!("expected partial"),
        }
        match parse_range(Some("bytes=5-"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (5, 9)),
            _ => panic!("expected partial"),
        }
        match parse_range(Some("bytes=-3"), 10) {
            RangeRequest::Partial { start, end } => assert_eq!((start, end), (7, 9)),
            _ => panic!("expected partial"),
        }
        assert!(matches!(
            parse_range(Some("bytes=20-30"), 10),
            RangeRequest::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some("bytes=-0"), 10),
            RangeRequest::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some("bytes=0-4"), 0),
            RangeRequest::Unsatisfiable
        ));
        // Multi-range requests are ignored (full representation).
        assert!(matches!(
            parse_range(Some("bytes=0-1,3-4"), 10),
            RangeRequest::Full
        ));
        assert!(matches!(
            parse_range(Some("chunks=1-2"), 10),
            RangeRequest::Full
        ));
    }

    #[test]
    fn content_disposition_cannot_inject_headers() {
        let value = content_disposition(Disposition::Attachment, "evil\r\nSet-Cookie: x=1\";.txt");
        let text = value.to_str().unwrap();
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert!(text.starts_with("attachment; filename=\""));
        assert!(text.contains("filename*=UTF-8''"));
        // Unicode names survive through the encoded form.
        let unicode = content_disposition(Disposition::Inline, "日本語.txt");
        assert!(unicode.to_str().unwrap().contains("filename*=UTF-8''"));
    }
}
