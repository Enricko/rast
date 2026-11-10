//! Audit logging: who did what, to which object, with what result.
//!
//! Passwords, session tokens, CSRF tokens and app secrets must never be passed
//! here. Detail strings are sanitised and truncated before storage.

use sqlx::SqlitePool;

use crate::{auth::AuthUser, db};

#[derive(Debug, Clone)]
pub struct AuditLog {
    pub user_id: Option<i64>,
    pub username: String,
    pub action: String,
    pub object_type: String,
    pub object_id: String,
    pub object_path: String,
    pub result: String,
    pub detail: String,
    pub ip: String,
}

impl AuditLog {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            user_id: None,
            username: String::new(),
            action: action.into(),
            object_type: String::new(),
            object_id: String::new(),
            object_path: String::new(),
            result: "success".into(),
            detail: String::new(),
            ip: String::new(),
        }
    }

    pub fn actor(mut self, user: &AuthUser) -> Self {
        self.user_id = Some(user.id);
        self.username = user.username.clone();
        self
    }

    pub fn anonymous(mut self, username: impl Into<String>) -> Self {
        self.username = username.into();
        self
    }

    pub fn object(mut self, object_type: impl Into<String>, id: impl Into<String>) -> Self {
        self.object_type = object_type.into();
        self.object_id = id.into();
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.object_path = path.into();
        self
    }

    pub fn success(mut self) -> Self {
        self.result = "success".into();
        self
    }

    pub fn denied(mut self) -> Self {
        self.result = "denied".into();
        self
    }

    pub fn failure(mut self) -> Self {
        self.result = "failure".into();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn ip(mut self, ip: impl Into<String>) -> Self {
        self.ip = ip.into();
        self
    }
}

/// Persist an audit entry. Failures are logged but never fail the request.
pub async fn record(pool: &SqlitePool, log: AuditLog) {
    let result = sqlx::query(
        "INSERT INTO audit_logs
         (created_at, user_id, username, action, object_type, object_id, object_path, result, detail, ip)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(db::now_ms())
    .bind(log.user_id)
    .bind(sanitize(&log.username, 64))
    .bind(sanitize(&log.action, 64))
    .bind(sanitize(&log.object_type, 32))
    .bind(sanitize(&log.object_id, 128))
    .bind(sanitize(&log.object_path, 512))
    .bind(sanitize(&log.result, 16))
    .bind(sanitize(&log.detail, 2000))
    .bind(sanitize(&log.ip, 64))
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to write audit log");
    }
}

/// Strip control characters (which could poison log views) and truncate to a
/// UTF-8 boundary.
pub fn sanitize(input: &str, max: usize) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    if cleaned.len() <= max {
        return cleaned;
    }
    let mut cut = max;
    while cut > 0 && !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &cleaned[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn records_entry() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("a.db"), 2).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
             VALUES (1,'admin','x','admin',1,1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let user = AuthUser {
            id: 1,
            username: "admin".into(),
            display_name: "Admin".into(),
            role: crate::auth::Role::Admin,
            session_id: "s".into(),
            quota_bytes: 0,
            must_change_password: false,
        };
        record(
            &pool,
            AuditLog::new("file.upload")
                .actor(&user)
                .object("file", "42")
                .path("homes/admin/report.pdf")
                .success()
                .detail("12 MiB")
                .ip("127.0.0.1"),
        )
        .await;
        let row: (String, String, String, String) =
            sqlx::query_as("SELECT action, username, result, object_path FROM audit_logs LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "file.upload");
        assert_eq!(row.1, "admin");
        assert_eq!(row.2, "success");
        assert_eq!(row.3, "homes/admin/report.pdf");
    }

    #[test]
    fn sanitize_strips_control_chars_and_truncates() {
        let s = sanitize("hello\u{0}\u{7}world", 100);
        assert_eq!(s, "helloworld");
        let long = "a".repeat(50);
        assert!(sanitize(&long, 10).ends_with('…'));
        let multibyte = "日本語テキスト";
        let cut = sanitize(multibyte, 8);
        assert!(cut.len() <= 8 + '…'.len_utf8());
    }
}
