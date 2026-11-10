//! Password hashing (Argon2id) and password policy.

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
};

use crate::error::{AppError, AppResult};

/// OWASP-recommended Argon2id parameters: 19 MiB memory, 2 iterations, 1 lane.
pub const M_COST_KIB: u32 = 19 * 1024;
pub const T_COST: u32 = 2;
pub const P_COST: u32 = 1;
pub const OUTPUT_LEN: usize = 32;

pub const MIN_PASSWORD_LEN: usize = 10;
pub const MAX_PASSWORD_LEN: usize = 256;

fn argon2() -> Argon2<'static> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(OUTPUT_LEN))
        .expect("static Argon2 parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password into a PHC string (`$argon2id$v=19$...`).
///
/// This is CPU and memory intensive by design; callers must run it on a
/// blocking thread (see [`hash_password_async`]).
pub fn hash_password(password: &str) -> AppResult<String> {
    validate_password(password)?;
    let hash = argon2()
        .hash_password(password.as_bytes())
        .map_err(|e| AppError::internal(format!("password hashing failed: {e}")))?;
    Ok(hash.to_string())
}

/// Hash on the blocking thread pool so the async runtime is never stalled.
pub async fn hash_password_async(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AppError::internal(format!("password hashing task failed: {e}")))?
}

/// Verify a password against a PHC string on the blocking thread pool.
///
/// Returns `Ok(false)` for a mismatch and `Err` only for malformed hashes.
pub async fn verify_password_async(password: String, phc: String) -> AppResult<bool> {
    tokio::task::spawn_blocking(move || verify_password(&password, &phc))
        .await
        .map_err(|e| AppError::internal(format!("password verification task failed: {e}")))?
}

/// Synchronous verification (used by CLI paths).
pub fn verify_password(password: &str, phc: &str) -> AppResult<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| AppError::internal(format!("stored password hash is invalid: {e}")))?;
    Ok(argon2().verify_password(password.as_bytes(), &parsed).is_ok())
}

/// True when the stored hash uses parameters weaker than the current policy,
/// so it should be transparently upgraded after a successful login.
pub fn needs_rehash(phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => {
            let params = parsed.params;
            let m = params.get("m").and_then(|v| v.decimal().ok()).unwrap_or(0);
            let t = params.get("t").and_then(|v| v.decimal().ok()).unwrap_or(0);
            let p = params.get("p").and_then(|v| v.decimal().ok()).unwrap_or(0);
            parsed.algorithm.as_str() != "argon2id"
                || m < M_COST_KIB
                || t < T_COST
                || p < P_COST
        }
        Err(_) => true,
    }
}

/// Password policy shared by the API, the CLI and the setup flow.
pub fn validate_password(password: &str) -> AppResult<()> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(AppError::bad_request(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(AppError::bad_request(format!(
            "password must be at most {MAX_PASSWORD_LEN} bytes"
        )));
    }
    if password.trim() != password {
        return Err(AppError::bad_request(
            "password must not start or end with whitespace",
        ));
    }
    let classes = [
        password.chars().any(|c| c.is_lowercase()),
        password.chars().any(|c| c.is_uppercase()),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    if classes < 3 {
        return Err(AppError::bad_request(
            "password must mix at least three of: lowercase letters, uppercase letters, digits, symbols",
        ));
    }
    if password.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad_request("password must not be all digits"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let phc = hash_password("Correct-Horse-9!").unwrap();
        assert!(phc.starts_with("$argon2id$v=19$"));
        assert!(verify_password("Correct-Horse-9!", &phc).unwrap());
        assert!(!verify_password("wrong-password-1!", &phc).unwrap());
        assert!(!needs_rehash(&phc));
    }

    #[test]
    fn hashes_are_salted() {
        let a = hash_password("Correct-Horse-9!").unwrap();
        let b = hash_password("Correct-Horse-9!").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn policy_rejects_weak_passwords() {
        for bad in [
            "short",
            "alllowercaseonly",
            "1234567890123",
            "nouppernum",
        ] {
            assert!(validate_password(bad).is_err(), "{bad:?} should fail");
        }
        assert!(validate_password("Sup3r-Secret-Passphrase!").is_ok());
    }

    #[test]
    fn rehash_detects_weak_params() {
        // Build a hash with lower cost than policy.
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8 * 1024, 1, 1, Some(32)).unwrap(),
        )
        .hash_password(b"pw")
        .unwrap()
        .to_string();
        assert!(needs_rehash(&weak));
        assert!(needs_rehash("not-a-hash"));
    }

    #[tokio::test]
    async fn async_hash_offloads() {
        let phc = hash_password_async("Another-Good-Pass1!".to_string())
            .await
            .unwrap();
        assert!(verify_password_async("Another-Good-Pass1!".into(), phc)
            .await
            .unwrap());
    }
}
