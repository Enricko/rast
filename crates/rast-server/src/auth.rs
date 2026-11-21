//! Passwords, session tokens and the signed-in account of a request.

use axum::http::HeaderMap;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::api::ApiError;
use crate::db::{self, User};
use crate::{AppState, SESSION_SECS};

pub(crate) const COOKIE: &str = "rast_session";
pub(crate) const MIN_PASSWORD: usize = 8;
pub(crate) const MAX_PASSWORD: usize = 1024;

/// URL-safe random text from `bytes` random bytes.
pub(crate) fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).expect("the OS random source works");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

pub(crate) fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={SESSION_SECS}")
}

pub(crate) fn cleared_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
}

/// The session token from the request's cookies.
pub(crate) fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty() && value.len() <= 128)
}

/// The signed-in account and the hash of its session.
pub(crate) struct Signed {
    pub(crate) user: User,
    pub(crate) session: String,
}

pub(crate) async fn signed_in(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Signed>, ApiError> {
    let Some(token) = cookie_token(headers) else {
        return Ok(None);
    };
    let session = token_hash(&token);
    Ok(db::session_user(&state.pool, &session)
        .await?
        .map(|user| Signed { user, session }))
}

pub(crate) async fn require(state: &AppState, headers: &HeaderMap) -> Result<Signed, ApiError> {
    signed_in(state, headers)
        .await?
        .ok_or_else(|| ApiError::unauthorized("Sign in to continue."))
}

pub(crate) async fn hash_password(password: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || {
        use argon2::Argon2;
        use argon2::password_hash::{PasswordHasher, SaltString};
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt).map_err(|error| error.to_string())?;
        let salt = SaltString::encode_b64(&salt).map_err(|error| error.to_string())?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::internal)
}

/// A real hash to check against when the account does not exist, so a wrong
/// username takes as long as a wrong password.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$4R0ThzBOvsKDkcSQQ7TVOBR4Nf1QZDDVF2nQ3MRBlOc";

pub(crate) async fn verify_password(
    password: String,
    phc: Option<String>,
) -> Result<bool, ApiError> {
    tokio::task::spawn_blocking(move || {
        use argon2::Argon2;
        use argon2::password_hash::{PasswordHash, PasswordVerifier};
        let known = phc.is_some();
        let phc = phc.unwrap_or_else(|| DUMMY_HASH.to_owned());
        let Ok(parsed) = PasswordHash::new(&phc) else {
            return false;
        };
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        known && ok
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))
}

/// Lowercase letters, digits, `.`, `_` and `-`, starting with a letter or digit;
/// 1–32 characters, like a Linux or NAS account name.
pub(crate) fn validate_username(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim().to_lowercase();
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() {
        return Err(ApiError::bad_request("Enter a username."));
    }
    if name.len() > 32 || !first_ok || !rest_ok {
        return Err(ApiError::bad_request(
            "Usernames use 1–32 lowercase letters, digits, dots, dashes or underscores, starting with a letter or digit.",
        ));
    }
    Ok(name)
}

pub(crate) fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad_request(format!(
            "Use at least {MIN_PASSWORD} characters for the password."
        )));
    }
    if password.len() > MAX_PASSWORD {
        return Err(ApiError::bad_request("That password is too long."));
    }
    Ok(())
}

pub(crate) fn validate_display_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "Display names are at most 64 characters, without control characters.",
        ));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_lowercase_nas_style_names() {
        assert_eq!(validate_username(" Alice ").unwrap(), "alice");
        assert_eq!(validate_username("ops.team_2-b").unwrap(), "ops.team_2-b");
        for bad in ["", "-dash", "has space", "émile", &"a".repeat(33)] {
            assert!(validate_username(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cookies_are_read_from_any_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append("cookie", "theme=dark; rast_session=abc123".parse().unwrap());
        assert_eq!(cookie_token(&headers).as_deref(), Some("abc123"));
        assert!(session_cookie("t").contains("HttpOnly; Secure; SameSite=Strict"));
    }

    #[tokio::test]
    async fn the_dummy_hash_parses_and_never_verifies() {
        assert!(argon2::password_hash::PasswordHash::new(DUMMY_HASH).is_ok());
        assert!(!verify_password("anything".into(), None).await.unwrap());
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(
            verify_password("correct horse".into(), Some(hash.clone()))
                .await
                .unwrap()
        );
        assert!(
            !verify_password("wrong horse".into(), Some(hash))
                .await
                .unwrap()
        );
    }
}
