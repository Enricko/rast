//! Server secrets: persisted key material used for CSRF tokens, session token
//! hashing and encryption of application secrets at rest.
//!
//! The master key lives in `<data_dir>/secret.key` (mode 0600). It is generated
//! on first start and never leaves the server. Losing it invalidates sessions
//! and encrypted app secrets, so it is included in backups.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("cannot access secret key {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("secret key file {0} has invalid contents (expected 64 hex characters)")]
    InvalidKey(String),
    #[error("encryption failure")]
    Encrypt,
    #[error("decryption failure")]
    Decrypt,
}

/// Holds the master key and derives purpose-specific subkeys.
#[derive(Clone)]
pub struct Secrets {
    master: [u8; 32],
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(<redacted>)")
    }
}

impl Secrets {
    /// Load the key at `path`, creating it with 0600 permissions when absent.
    pub fn load_or_create(path: &Path) -> Result<Self, SecretError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SecretError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        match Self::read_key(path) {
            Ok(key) => return Ok(Self { master: key }),
            Err(SecretError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Create atomically; if another process won the race, read its key.
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let hex_key = hex::encode(key);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(Self {
                    master: Self::read_key(path)?,
                })
            }
            Err(source) => {
                return Err(SecretError::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        file.write_all(hex_key.as_bytes())
            .map_err(|source| SecretError::Io {
                path: path.display().to_string(),
                source,
            })?;
        file.sync_all().ok();
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(Self { master: key })
    }

    fn read_key(path: &Path) -> Result<[u8; 32], SecretError> {
        let mut file = File::open(path).map_err(|source| SecretError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|source| SecretError::Io {
                path: path.display().to_string(),
                source,
            })?;
        let trimmed = text.trim();
        let bytes = hex::decode(trimmed)
            .map_err(|_| SecretError::InvalidKey(path.display().to_string()))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| SecretError::InvalidKey(path.display().to_string()))?;
        Ok(arr)
    }

    /// Derive a 32-byte subkey for a purpose label.
    fn derive(&self, purpose: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"rast-subkey-v1");
        h.update(self.master);
        h.update(purpose.as_bytes());
        let out = h.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&out);
        key
    }

    /// HMAC-SHA256 over `msg` with a purpose-scoped key, hex encoded.
    pub fn hmac_hex(&self, purpose: &str, msg: &[u8]) -> String {
        let key = self.derive(purpose);
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&key).expect("hmac accepts 32-byte keys");
        mac.update(msg);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Constant-time verification of a hex HMAC.
    pub fn verify_hmac_hex(&self, purpose: &str, msg: &[u8], provided_hex: &str) -> bool {
        let expected = self.hmac_hex(purpose, msg);
        let a = expected.as_bytes();
        let b = provided_hex.trim().as_bytes();
        if a.len() != b.len() {
            return false;
        }
        a.ct_eq(b).into()
    }

    /// Encrypt bytes; output is base64(nonce || ciphertext).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String, SecretError> {
        let key = self.derive("secretbox");
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| SecretError::Encrypt)?;
        let mut nonce_bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| SecretError::Encrypt)?;
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(B64.encode(out))
    }

    /// Decrypt a value produced by [`Secrets::encrypt`].
    pub fn decrypt(&self, encoded: &str) -> Result<Vec<u8>, SecretError> {
        let raw = B64.decode(encoded).map_err(|_| SecretError::Decrypt)?;
        if raw.len() < 13 {
            return Err(SecretError::Decrypt);
        }
        let (nonce_bytes, ct) = raw.split_at(12);
        let key = self.derive("secretbox");
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| SecretError::Decrypt)?;
        let nonce = Nonce::from_slice(nonce_bytes);
        cipher.decrypt(nonce, ct).map_err(|_| SecretError::Decrypt)
    }

    pub fn encrypt_str(&self, plaintext: &str) -> Result<String, SecretError> {
        self.encrypt(plaintext.as_bytes())
    }

    pub fn decrypt_string(&self, encoded: &str) -> Result<String, SecretError> {
        let bytes = self.decrypt(encoded)?;
        String::from_utf8(bytes).map_err(|_| SecretError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_persisted_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let a = Secrets::load_or_create(&path).unwrap();
        let b = Secrets::load_or_create(&path).unwrap();
        assert_eq!(a.hmac_hex("csrf", b"x"), b.hmac_hex("csrf", b"x"));
        assert_ne!(a.hmac_hex("csrf", b"x"), a.hmac_hex("session", b"x"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn encrypt_roundtrip_and_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::load_or_create(&dir.path().join("secret.key")).unwrap();
        let ct = s.encrypt_str("super-secret-value").unwrap();
        assert!(!ct.contains("super-secret-value"));
        assert_eq!(s.decrypt_string(&ct).unwrap(), "super-secret-value");
        // flip one ciphertext byte -> must fail
        let mut raw = B64.decode(&ct).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert!(s.decrypt(&B64.encode(raw)).is_err());
    }

    #[test]
    fn hmac_verify_rejects_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::load_or_create(&dir.path().join("secret.key")).unwrap();
        let good = s.hmac_hex("csrf", b"payload");
        assert!(s.verify_hmac_hex("csrf", b"payload", &good));
        assert!(!s.verify_hmac_hex("csrf", b"payload2", &good));
        assert!(!s.verify_hmac_hex("csrf", b"payload", "00"));
    }
}
