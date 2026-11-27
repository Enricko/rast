//! Files people add to their AI Agent messages.
//!
//! Images (PNG, JPEG, GIF, WebP) and PDFs go to the model as they are;
//! text files (any UTF-8 text: notes, code, CSV, JSON, logs) go as their
//! text. The bytes live under `<data_dir>/ai-files/<user id>/<id>` and
//! `ai_files` lists them: a file belongs to its account until it is sent,
//! then to the chat it was sent in, and goes with that chat. Files never
//! sent are removed after a day.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::provider::{Attachment, Turn, UserTurn};
use crate::{
    db::now_ms,
    error::{AppError, AppResult},
};

/// The largest file (10 MiB).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// How many files one message takes.
pub const MAX_FILES_PER_MESSAGE: usize = 10;
/// What an account's files may take in all (500 MiB).
pub const MAX_TOTAL_BYTES: i64 = 500 * 1024 * 1024;
/// How much of a text file the model reads (characters).
pub const MAX_TEXT_CHARS: usize = 200_000;
/// Images and PDFs sent with one request, newest first (20 MiB; older ones
/// are named instead of sent).
const MAX_SENT_BYTES: usize = 20 * 1024 * 1024;
/// Files added to the message box but never sent go after a day.
const UNSENT_FOR_MS: i64 = 24 * 3600 * 1000;

pub const UNSUPPORTED: &str =
    "AI Agent reads images (PNG, JPEG, GIF, WebP), PDFs and text files; this file is none of these";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Pdf,
    Text,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Pdf => "pdf",
            Self::Text => "text",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredFile {
    pub id: String,
    pub user_id: i64,
    pub conversation_id: Option<String>,
    pub name: String,
    pub mime: String,
    pub kind: String,
    pub size: i64,
    pub created_at: i64,
}

impl StoredFile {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "mime": self.mime,
            "kind": self.kind,
            "size": self.size,
        })
    }

    /// The file as a block of the message it was sent with.
    pub fn block(&self) -> Value {
        let mut block = self.json();
        block["type"] = json!("file");
        block
    }
}

fn from_row(row: &sqlx::sqlite::SqliteRow) -> StoredFile {
    StoredFile {
        id: row.get("id"),
        user_id: row.get("user_id"),
        conversation_id: row.get("conversation_id"),
        name: row.get("name"),
        mime: row.get("mime"),
        kind: row.get("kind"),
        size: row.get("size_bytes"),
        created_at: row.get("created_at"),
    }
}

/// What a file is, from its bytes (the declared type only names text).
pub fn sniff(bytes: &[u8], declared: &str) -> Option<(FileKind, String)> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let image = if bytes.starts_with(PNG) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    };
    if let Some(mime) = image {
        return Some((FileKind::Image, mime.into()));
    }
    if bytes.starts_with(b"%PDF-") {
        return Some((FileKind::Pdf, "application/pdf".into()));
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return None;
    }
    let base = declared
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let texty = base.starts_with("text/")
        || matches!(
            base.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/sql"
                | "application/x-sh"
        );
    let mime = if texty && base.len() <= 100 {
        base
    } else {
        "text/plain".into()
    };
    Some((FileKind::Text, mime))
}

/// A display name: the last part of a path, no control characters.
pub fn clean_name(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or("");
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned.into()
    }
}

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("ai-files")
}

pub fn path_of(data_dir: &Path, user_id: i64, id: &str) -> PathBuf {
    root(data_dir).join(user_id.to_string()).join(id)
}

fn private_dir(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::internal("invalid file path"))?;
    let temp = parent.join(format!(".tmp-{}", Uuid::new_v4()));
    let result: AppResult<()> = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

/// Keep a file the account added to its message box.
pub async fn store(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    name: Option<&str>,
    declared: &str,
    bytes: &[u8],
) -> AppResult<StoredFile> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(AppError::PayloadTooLarge(
            "files are limited to 10 MiB each",
        ));
    }
    let (kind, mime) = sniff(bytes, declared).ok_or(AppError::UnsupportedMediaType(UNSUPPORTED))?;
    forget_unsent(pool, data_dir, Some(user_id)).await;
    let size = bytes.len() as i64;
    let dir = root(data_dir).join(user_id.to_string());
    private_dir(&root(data_dir))?;
    private_dir(&dir)?;
    let id = Uuid::new_v4().simple().to_string();
    let path = dir.join(&id);
    write_atomic(&path, bytes).await?;
    let file = StoredFile {
        id,
        user_id,
        conversation_id: None,
        name: clean_name(name),
        mime,
        kind: kind.as_str().into(),
        size,
        created_at: now_ms(),
    };
    // Race-safe: the row goes in only while the account has room.
    let inserted = sqlx::query(
        "INSERT INTO ai_files (id, user_id, conversation_id, name, mime, kind, size_bytes, created_at)
         SELECT ?, ?, NULL, ?, ?, ?, ?, ?
         WHERE (SELECT COALESCE(SUM(size_bytes), 0) FROM ai_files WHERE user_id = ?) + ? <= ?",
    )
    .bind(&file.id)
    .bind(user_id)
    .bind(&file.name)
    .bind(&file.mime)
    .bind(&file.kind)
    .bind(size)
    .bind(file.created_at)
    .bind(user_id)
    .bind(size)
    .bind(MAX_TOTAL_BYTES)
    .execute(pool)
    .await;
    match inserted {
        Ok(done) if done.rows_affected() == 1 => Ok(file),
        Ok(_) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(AppError::PayloadTooLarge(
                "AI Agent keeps up to 500 MiB of files per account; deleting chats with files makes room",
            ))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(e.into())
        }
    }
}

pub async fn get(pool: &SqlitePool, id: &str) -> AppResult<Option<StoredFile>> {
    let row = sqlx::query("SELECT * FROM ai_files WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(from_row))
}

/// The account's files `ids` for a message in `conversation_id`: each its
/// own and not sent elsewhere. Ties them to the chat.
pub async fn attach(
    pool: &SqlitePool,
    user_id: i64,
    conversation_id: &str,
    ids: &[String],
) -> AppResult<Vec<StoredFile>> {
    if ids.len() > MAX_FILES_PER_MESSAGE {
        return Err(AppError::bad_request(format!(
            "a message takes up to {MAX_FILES_PER_MESSAGE} files"
        )));
    }
    let mut seen = HashSet::new();
    let mut files = Vec::with_capacity(ids.len());
    for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
        let file = get(pool, id).await?.filter(|f| {
            f.user_id == user_id
                && f.conversation_id
                    .as_deref()
                    .is_none_or(|c| c == conversation_id)
        });
        files.push(file.ok_or_else(|| {
            AppError::bad_request("a file is no longer there; add it to the message again")
        })?);
    }
    for file in &mut files {
        sqlx::query("UPDATE ai_files SET conversation_id = ? WHERE id = ?")
            .bind(conversation_id)
            .bind(&file.id)
            .execute(pool)
            .await?;
        file.conversation_id = Some(conversation_id.into());
    }
    Ok(files)
}

/// Remove a file the account added but has not sent.
pub async fn remove_unsent(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    id: &str,
) -> AppResult<bool> {
    let removed = sqlx::query(
        "DELETE FROM ai_files WHERE id = ? AND user_id = ? AND conversation_id IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if removed {
        let _ = tokio::fs::remove_file(path_of(data_dir, user_id, id)).await;
    }
    Ok(removed)
}

/// Remove a chat's files (before the chat goes).
pub async fn remove_for_conversation(
    pool: &SqlitePool,
    data_dir: &Path,
    conversation_id: &str,
) -> AppResult<()> {
    let rows = sqlx::query("DELETE FROM ai_files WHERE conversation_id = ? RETURNING id, user_id")
        .bind(conversation_id)
        .fetch_all(pool)
        .await?;
    for row in rows {
        let path = path_of(data_dir, row.get("user_id"), &row.get::<String, _>("id"));
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(())
}

/// Files never sent, older than a day (of one account, or everyone's).
async fn forget_unsent(pool: &SqlitePool, data_dir: &Path, user_id: Option<i64>) {
    let rows = sqlx::query(
        "DELETE FROM ai_files WHERE conversation_id IS NULL AND created_at < ?
             AND (? IS NULL OR user_id = ?)
         RETURNING id, user_id",
    )
    .bind(now_ms() - UNSENT_FOR_MS)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            for row in rows {
                let path = path_of(data_dir, row.get("user_id"), &row.get::<String, _>("id"));
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not remove unsent AI files"),
    }
}

/// At start: unsent files past their day, and files whose account or chat
/// is gone (their rows went with it).
pub async fn sweep(pool: &SqlitePool, data_dir: &Path) {
    forget_unsent(pool, data_dir, None).await;
    let known: HashSet<String> = match sqlx::query_scalar::<_, String>("SELECT id FROM ai_files")
        .fetch_all(pool)
        .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list AI files");
            return;
        }
    };
    let Ok(accounts) = std::fs::read_dir(root(data_dir)) else {
        return;
    };
    for account in accounts.flatten() {
        let Ok(entries) = std::fs::read_dir(account.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// What the person wrote in a message, with the files they added (not read
/// yet: see [`load`]).
pub fn user_turn(content: &Value) -> UserTurn {
    let files = content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("file"))
                .filter_map(|b| {
                    Some(Attachment {
                        id: b.get("id")?.as_str()?.into(),
                        name: b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("file")
                            .into(),
                        mime: b.get("mime").and_then(Value::as_str).unwrap_or("").into(),
                        size: b.get("size").and_then(Value::as_u64).unwrap_or(0) as usize,
                        is_text: b.get("kind").and_then(Value::as_str) == Some("text"),
                        data: String::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    UserTurn {
        text: super::text_of(content),
        files,
    }
}

/// Read the files of `turns` (the account `user_id`'s): text files join
/// their message's text, images and PDFs get their bytes. The newest go
/// first; past [`MAX_SENT_BYTES`], or when a file is gone, the message
/// names it instead.
pub async fn load(data_dir: &Path, user_id: i64, turns: &mut [Turn]) {
    let mut budget = MAX_SENT_BYTES;
    for turn in turns.iter_mut().rev() {
        let Turn::User(user) = turn else {
            continue;
        };
        if user.files.is_empty() {
            continue;
        }
        let mut texts = Vec::new();
        let mut notes = Vec::new();
        let mut kept = Vec::new();
        for mut file in std::mem::take(&mut user.files) {
            let bytes = match tokio::fs::read(path_of(data_dir, user_id, &file.id)).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    notes.push(format!(
                        "[{} was attached; it is no longer available]",
                        file.name
                    ));
                    continue;
                }
            };
            if file.is_text {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.trim_start_matches('\u{feff}');
                let mut shown: String = text.chars().take(MAX_TEXT_CHARS).collect();
                if shown.len() < text.len() {
                    shown.push_str(&format!(
                        "\n[… the rest of {} is left out: it is longer than {MAX_TEXT_CHARS} characters]",
                        file.name
                    ));
                }
                texts.push(format!(
                    "<file name=\"{}\">\n{shown}\n</file>",
                    file.name.replace('"', "'")
                ));
            } else if bytes.len() > budget {
                notes.push(format!(
                    "[{} ({}) was attached earlier; it is not sent again with this message]",
                    file.name, file.mime
                ));
            } else {
                budget -= bytes.len();
                file.data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                kept.push(file);
            }
        }
        let mut text = texts.join("\n\n");
        for part in notes.into_iter().chain([std::mem::take(&mut user.text)]) {
            if part.trim().is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&part);
        }
        user.text = text;
        user.files = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_known_by_their_bytes() {
        let png = b"\x89PNG\r\n\x1a\n rest";
        assert_eq!(
            sniff(png, "text/plain"),
            Some((FileKind::Image, "image/png".into()))
        );
        assert_eq!(
            sniff(&[0xFF, 0xD8, 0xFF, 0xE0], ""),
            Some((FileKind::Image, "image/jpeg".into()))
        );
        assert_eq!(
            sniff(b"GIF89a....", ""),
            Some((FileKind::Image, "image/gif".into()))
        );
        assert_eq!(
            sniff(b"RIFF\0\0\0\0WEBPVP8 ", ""),
            Some((FileKind::Image, "image/webp".into()))
        );
        assert_eq!(
            sniff(b"%PDF-1.7\n", "image/png"),
            Some((FileKind::Pdf, "application/pdf".into()))
        );
        assert_eq!(
            sniff(b"a,b\n1,2\n", "text/csv"),
            Some((FileKind::Text, "text/csv".into()))
        );
        assert_eq!(
            sniff(b"{\"a\":1}", "application/json; charset=utf-8"),
            Some((FileKind::Text, "application/json".into()))
        );
        // An HTML page or an unknown type is plain text to the model.
        assert_eq!(
            sniff(b"fn main() {}", "application/octet-stream"),
            Some((FileKind::Text, "text/plain".into()))
        );
        assert_eq!(
            sniff("héllo".as_bytes(), ""),
            Some((FileKind::Text, "text/plain".into()))
        );
        assert_eq!(sniff(b"\x00\x01\x02", ""), None);
        assert_eq!(sniff(&[0xC3, 0x28], "text/plain"), None);
        // SVG is text (it is never served as an image).
        assert_eq!(
            sniff(b"<svg/>", "image/svg+xml"),
            Some((FileKind::Text, "text/plain".into()))
        );
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name(Some("../../etc/passwd")), "passwd");
        assert_eq!(clean_name(Some("C:\\Users\\a\\notes.txt")), "notes.txt");
        assert_eq!(clean_name(Some("a\u{0}b\nc.md")), "abc.md");
        assert_eq!(clean_name(Some("..")), "file");
        assert_eq!(clean_name(None), "file");
    }

    #[test]
    fn a_message_lists_its_files() {
        let content = json!([
            {"type": "file", "id": "f1", "name": "cat.png", "mime": "image/png", "kind": "image", "size": 10},
            {"type": "file", "id": "f2", "name": "notes.txt", "mime": "text/plain", "kind": "text", "size": 5},
            {"type": "text", "text": "What is this?"},
        ]);
        let turn = user_turn(&content);
        assert_eq!(turn.text, "What is this?");
        assert_eq!(turn.files.len(), 2);
        assert!(!turn.files[0].is_text && turn.files[1].is_text);
        assert_eq!(turn.files[0].size, 10);
    }

    #[tokio::test]
    async fn files_are_read_just_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let user_dir = root(dir.path()).join("7");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("img"), b"\x89PNG\r\n\x1a\nxx").unwrap();
        std::fs::write(user_dir.join("txt"), "\u{feff}line one").unwrap();
        let file = |id: &str, name: &str, mime: &str, is_text: bool| Attachment {
            id: id.into(),
            name: name.into(),
            mime: mime.into(),
            size: 10,
            is_text,
            data: String::new(),
        };
        let mut turns = vec![Turn::User(UserTurn {
            text: "Look".into(),
            files: vec![
                file("img", "cat.png", "image/png", false),
                file("txt", "notes.txt", "text/plain", true),
                file("gone", "old.pdf", "application/pdf", false),
            ],
        })];
        load(dir.path(), 7, &mut turns).await;
        let Turn::User(user) = &turns[0] else {
            panic!()
        };
        assert_eq!(
            user.text,
            "<file name=\"notes.txt\">\nline one\n</file>\n\n[old.pdf was attached; it is no longer available]\n\nLook"
        );
        assert_eq!(user.files.len(), 1);
        assert_eq!(
            user.files[0].data,
            base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nxx")
        );
    }
}
