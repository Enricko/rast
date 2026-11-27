//! PKCE ("proof key for code exchange"): RAST invents a long random
//! verifier, sends only its SHA-256 challenge to the browser, and presents
//! the verifier when trading the code for tokens. Someone who sees the
//! address (or steals the code) cannot use it elsewhere, and the code RAST
//! gets is useless without the verifier that never left this process.
//!
//! Also the random names in a sign-in's address: the `state` ties an answer
//! back to the sign-in that started it (and stops another site from feeding
//! RAST a code of its choosing).

use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// `bytes` random bytes, base64url without padding (safe in an address).
pub(crate) fn random_url_safe(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// A new PKCE verifier (64 random bytes, as the references use).
pub(crate) fn verifier() -> String {
    random_url_safe(64)
}

/// The S256 challenge for a verifier: base64url of its SHA-256.
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A new `state` (24 random bytes).
pub(crate) fn state() -> String {
    random_url_safe(24)
}

/// Decode a base64url value with or without padding (JWTs carry none).
pub(crate) fn decode_url_safe(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(value))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_are_the_verifiers_sha256() {
        let verifier = verifier();
        assert_eq!(verifier.len(), 86, "64 random bytes, base64url");
        assert_eq!(
            challenge(&verifier),
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        assert_ne!(state(), state());
        assert_eq!(state().len(), 32, "24 random bytes, base64url");
    }
}
