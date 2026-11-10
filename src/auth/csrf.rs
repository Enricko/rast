//! CSRF protection: double-submit token bound to the session.
//!
//! The client receives a token from `/api/auth/me` (and after login) and must
//! echo it in the `X-CSRF-Token` header on every state-changing request.

use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

use crate::secrets::Secrets;

pub const CSRF_HEADER: &str = "x-csrf-token";

/// Deterministic per-session token derived with a server-side secret.
pub fn token(secrets: &Secrets, session_id: &str) -> String {
    secrets.hmac_hex("csrf-v1", session_id.as_bytes())
}

/// Constant-time verification of the header against the expected token.
pub fn verify(headers: &HeaderMap, secrets: &Secrets, session_id: &str) -> bool {
    let Some(provided) = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let expected = token(secrets, session_id);
    let a = expected.as_bytes();
    let b = provided.trim().as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn secrets() -> (tempfile::TempDir, Secrets) {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::load_or_create(&dir.path().join("secret.key")).unwrap();
        (dir, s)
    }

    #[test]
    fn tokens_are_stable_per_session_and_secret() {
        let (_d, s) = secrets();
        assert_eq!(token(&s, "abc"), token(&s, "abc"));
        assert_ne!(token(&s, "abc"), token(&s, "abd"));
        assert_ne!(token(&s, "abc"), token(&s, "abc").to_uppercase());
    }

    #[test]
    fn verify_accepts_correct_header_only() {
        let (_d, s) = secrets();
        let mut headers = HeaderMap::new();
        headers.insert(
            CSRF_HEADER,
            HeaderValue::from_str(&token(&s, "sess-1")).unwrap(),
        );
        assert!(verify(&headers, &s, "sess-1"));
        assert!(!verify(&headers, &s, "sess-2"));
        headers.insert(CSRF_HEADER, HeaderValue::from_static("nope"));
        assert!(!verify(&headers, &s, "sess-1"));
    }

    #[test]
    fn verify_rejects_missing_header() {
        let (_d, s) = secrets();
        assert!(!verify(&HeaderMap::new(), &s, "sess-1"));
    }
}
