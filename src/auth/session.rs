//! Server-side sessions: opaque tokens whose SHA-256 hash is stored in SQLite.

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::{db, error::AppResult};

pub const SESSION_COOKIE: &str = "rast_session";
pub const SESSION_ID_COOKIE: &str = "rast_session_id";
pub const TOKEN_BYTES: usize = 32;
/// Sessions are extended on activity but never beyond this absolute lifetime.
pub const ABSOLUTE_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Minimum interval between last_seen updates to avoid a write per request.
pub const TOUCH_INTERVAL_MS: i64 = 60_000;

pub struct NewSession {
    pub token: String,
    pub session_id: String,
    pub expires_at: i64,
    /// Token only exists in the `Set-Cookie` header, never in the database.
    _private: (),
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Hash a session token for storage. Tokens are high-entropy random values so a
/// fast hash is appropriate here (unlike passwords).
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Create a session row for `user_id`.
pub async fn create(
    pool: &SqlitePool,
    user_id: i64,
    ttl_hours: i64,
    ip: &str,
    user_agent: &str,
) -> AppResult<NewSession> {
    let token = random_token();
    let session_id = new_session_id();
    let now = db::now_ms();
    let expires_at = now + ttl_hours.max(1) * 3_600_000;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, created_at, expires_at, last_seen_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(user_id)
    .bind(hash_token(&token))
    .bind(now)
    .bind(expires_at)
    .bind(now)
    .bind(truncate(ip, 64))
    .bind(truncate(user_agent, 400))
    .execute(pool)
    .await?;
    Ok(NewSession {
        token,
        session_id,
        expires_at,
        _private: (),
    })
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub user_id: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub last_seen_at: i64,
    pub ip: String,
    pub user_agent: String,
}

/// Validate a token and return its session, or `None` when expired/revoked.
pub async fn validate(
    pool: &SqlitePool,
    token: &str,
    ttl_hours: i64,
) -> AppResult<Option<SessionRecord>> {
    let now = db::now_ms();
    let row = sqlx::query(
        "SELECT id, user_id, created_at, expires_at, last_seen_at, ip, user_agent
         FROM sessions
         WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(hash_token(token))
    .bind(now)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let mut record = SessionRecord {
        session_id: row.get("id"),
        user_id: row.get("user_id"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        last_seen_at: row.get("last_seen_at"),
        ip: row.get("ip"),
        user_agent: row.get("user_agent"),
    };

    // Sliding expiry: extend on activity, capped by the absolute lifetime.
    if now - record.last_seen_at > TOUCH_INTERVAL_MS {
        let max_expiry = record.created_at + ABSOLUTE_MAX_AGE_MS.min(ttl_hours.max(1) * 3_600_000 * 4);
        let new_expiry = (now + ttl_hours.max(1) * 3_600_000).min(max_expiry);
        sqlx::query("UPDATE sessions SET last_seen_at = ?, expires_at = ? WHERE id = ?")
            .bind(now)
            .bind(new_expiry)
            .bind(&record.session_id)
            .execute(pool)
            .await?;
        record.last_seen_at = now;
        record.expires_at = new_expiry;
    }
    Ok(Some(record))
}

/// Revoke one session by id.
pub async fn revoke(pool: &SqlitePool, session_id: &str) -> AppResult<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Revoke every session of a user, optionally keeping the current one.
pub async fn revoke_all_for_user(
    pool: &SqlitePool,
    user_id: i64,
    keep_session_id: Option<&str>,
) -> AppResult<u64> {
    let now = db::now_ms();
    let res = match keep_session_id {
        Some(keep) => {
            sqlx::query(
                "UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND id != ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(user_id)
            .bind(keep)
            .execute(pool)
            .await?
        }
        None => {
            sqlx::query(
                "UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(user_id)
            .execute(pool)
            .await?
        }
    };
    Ok(res.rows_affected())
}

/// Delete expired/revoked sessions; called by the maintenance task.
pub async fn cleanup(pool: &SqlitePool) -> AppResult<u64> {
    let now = db::now_ms();
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at < ? OR revoked_at IS NOT NULL")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Remove all sessions for a user (account deletion path).
pub async fn delete_for_user(pool: &SqlitePool, user_id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Duration helper used by cookie headers.
pub fn ttl_duration_hours(hours: i64) -> Duration {
    Duration::from_secs((hours.max(1) * 3600) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;

    async fn test_db() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_parts(
            None,
            crate::config::FileConfig::default(),
            |_| None,
        )
        .unwrap();
        let _ = cfg;
        let pool = db::connect(&dir.path().join("t.db"), 4).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (1, 'alice', 'x', 'admin', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn create_validate_revoke() {
        let (_dir, pool) = test_db().await;
        let s = create(&pool, 1, 24, "127.0.0.1", "test-agent").await.unwrap();
        let rec = validate(&pool, &s.token, 24).await.unwrap().expect("valid");
        assert_eq!(rec.session_id, s.session_id);
        assert_eq!(rec.user_id, 1);
        // Stored value is a hash, not the token.
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT token_hash FROM sessions WHERE id = ?")
                .bind(&s.session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, hash_token(&s.token));
        assert_ne!(stored, s.token.as_bytes());

        revoke(&pool, &s.session_id).await.unwrap();
        assert!(validate(&pool, &s.token, 24).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unknown_token_rejected() {
        let (_dir, pool) = test_db().await;
        assert!(validate(&pool, "not-a-token", 24).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoke_all_keeps_current() {
        let (_dir, pool) = test_db().await;
        let a = create(&pool, 1, 24, "", "").await.unwrap();
        let b = create(&pool, 1, 24, "", "").await.unwrap();
        let n = revoke_all_for_user(&pool, 1, Some(&a.session_id)).await.unwrap();
        assert_eq!(n, 1);
        assert!(validate(&pool, &a.token, 24).await.unwrap().is_some());
        assert!(validate(&pool, &b.token, 24).await.unwrap().is_none());
    }
}
