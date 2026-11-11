//! Per-user appearance: theme + wallpaper preferences and uploaded wallpapers.
//!
//! The appearance settings are stored server-side (`user_appearance`) so the
//! desktop looks identical on every browser a user signs in from. Wallpapers
//! can be one of the bundled ids, a solid colour, or a user-uploaded image that
//! lives in `<data_dir>/wallpapers/<user_id>/<uuid>.<ext>`.
//!
//! Security properties:
//!
//! * uploads are validated by parsing the actual image headers (JPEG/PNG/WebP)
//!   and the declared `Content-Type` must match the real format;
//! * the server generates the file name (UUID), never trusting the client, so
//!   path traversal is impossible;
//! * every read/delete is scoped by `user_id` in SQL — other users (admins
//!   included) get a 404 even with a guessed UUID;
//! * the per-user directory is created `0700`, files are written to a temporary
//!   file first and atomically renamed after `fsync`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    db,
    error::{AppError, AppResult},
};

/// Maximum accepted wallpaper request body / stored file size.
pub const MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum number of uploaded wallpapers per user.
pub const MAX_UPLOADS_PER_USER: i64 = 20;
/// Maximum total bytes of uploaded wallpapers per user.
pub const MAX_TOTAL_BYTES_PER_USER: i64 = 100 * 1024 * 1024;
/// Minimum accepted wallpaper dimensions.
pub const MIN_WIDTH: u32 = 320;
pub const MIN_HEIGHT: u32 = 180;
/// Maximum accepted dimension of either side.
pub const MAX_SIDE: u32 = 10_000;
/// Maximum accepted pixel count (60 megapixels).
pub const MAX_PIXELS: u64 = 60_000_000;

pub const DEFAULT_THEME: &str = "light";
/// Accent presets. Ids are stable API values; the exact colours live in the
/// frontend design tokens so both themes can pair a readable foreground.
pub const ACCENTS: &[&str] = &[
    "ocean",
    "forest",
    "teal",
    "sage",
    "amber",
    "terracotta",
    "rose",
    "plum",
    "indigo",
    "graphite",
];
pub const DEFAULT_ACCENT: &str = "ocean";
pub const DEFAULT_BUILTIN_ID: &str = "alpine-dawn";
pub const DEFAULT_COLOR: &str = "#1B2430";
pub const DEFAULT_FIT: &str = "cover";
pub const DEFAULT_POSITION: &str = "center";
pub const DEFAULT_OVERLAY: i64 = 20;

/// Bundled wallpapers the frontend ships (`/wallpapers/<id>.jpg`).
pub const BUILTIN_IDS: &[&str] = &["alpine-dawn", "forest-mist", "coastal-calm", "desert-dusk"];
pub const THEMES: &[&str] = &["light", "dark", "system"];
pub const WALLPAPER_KINDS: &[&str] = &["builtin", "upload", "color"];
pub const FITS: &[&str] = &["cover", "contain", "center"];
pub const POSITIONS: &[&str] = &[
    "center",
    "top",
    "bottom",
    "left",
    "right",
    "top-left",
    "top-right",
    "bottom-left",
    "bottom-right",
];

pub const MIME_JPEG: &str = "image/jpeg";
pub const MIME_PNG: &str = "image/png";
pub const MIME_WEBP: &str = "image/webp";

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const JPEG_SOI: [u8; 3] = [0xFF, 0xD8, 0xFF];
const JPEG_EOI: [u8; 2] = [0xFF, 0xD9];

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WallpaperSettingsDto {
    pub kind: String,
    pub builtin_id: String,
    pub upload_id: Option<String>,
    pub color: String,
    pub fit: String,
    pub position: String,
    pub overlay: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceUploadDto {
    pub id: String,
    pub width: i64,
    pub height: i64,
    pub size_bytes: i64,
    pub mime: String,
    pub created_at: i64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsDto {
    pub max_bytes: u64,
    pub max_uploads: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceDto {
    pub theme: String,
    pub accent: String,
    pub wallpaper: WallpaperSettingsDto,
    pub uploads: Vec<AppearanceUploadDto>,
    pub limits: LimitsDto,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WallpaperPatch {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub builtin_id: Option<String>,
    #[serde(default)]
    pub upload_id: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub fit: Option<String>,
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub overlay: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAppearanceRequest {
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub wallpaper: Option<WallpaperPatch>,
}

impl Default for WallpaperSettingsDto {
    fn default() -> Self {
        Self {
            kind: "builtin".to_string(),
            builtin_id: DEFAULT_BUILTIN_ID.to_string(),
            upload_id: None,
            color: DEFAULT_COLOR.to_string(),
            fit: DEFAULT_FIT.to_string(),
            position: DEFAULT_POSITION.to_string(),
            overlay: DEFAULT_OVERLAY,
        }
    }
}

impl Default for AppearanceDto {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_string(),
            accent: DEFAULT_ACCENT.to_string(),
            wallpaper: WallpaperSettingsDto::default(),
            uploads: Vec::new(),
            limits: LimitsDto {
                max_bytes: MAX_UPLOAD_BYTES,
                max_uploads: MAX_UPLOADS_PER_USER,
            },
        }
    }
}

/// URL the frontend uses as CSS background for an uploaded wallpaper.
///
/// Server-generated ids are URL-safe; anything else is percent-encoded as a
/// defence-in-depth measure (ids are never taken from the client).
pub fn upload_url(id: &str) -> String {
    if is_safe_upload_id(id) {
        format!("/api/appearance/wallpapers/{id}")
    } else {
        format!(
            "/api/appearance/wallpapers/{}",
            percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC)
        )
    }
}

/// A wallpaper file ready to be streamed to its owner.
#[derive(Debug, Clone)]
pub struct ServeUpload {
    pub id: String,
    pub mime: String,
    pub size_bytes: i64,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn bad_request(message: impl Into<String>) -> AppError {
    AppError::bad_request(message)
}

fn normalize_mime(raw: &str) -> AppResult<&'static str> {
    let mime = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        MIME_JPEG => Ok(MIME_JPEG),
        MIME_PNG => Ok(MIME_PNG),
        MIME_WEBP => Ok(MIME_WEBP),
        _ => Err(AppError::UnsupportedMediaType(
            "wallpaper must be a JPEG, PNG or WebP image",
        )),
    }
}

fn normalize_accent(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_ascii_lowercase();
    if ACCENTS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_request(
            "invalid accent; expected one of ocean, forest, teal, sage, amber, terracotta, rose, plum, indigo, graphite",
        ))
    }
}

fn normalize_theme(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_ascii_lowercase();
    if THEMES.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_request("invalid theme; expected light, dark or system"))
    }
}

fn normalize_kind(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_ascii_lowercase();
    if WALLPAPER_KINDS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_request(
            "invalid wallpaper kind; expected builtin, upload or color",
        ))
    }
}

fn normalize_builtin(raw: &str) -> AppResult<String> {
    let value = raw.trim();
    if BUILTIN_IDS.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(bad_request(
            "unknown built-in wallpaper id; expected one of alpine-dawn, forest-mist, coastal-calm, desert-dusk",
        ))
    }
}

fn normalize_fit(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_ascii_lowercase();
    if FITS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_request(
            "invalid fit; expected cover, contain or center",
        ))
    }
}

fn normalize_position(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_ascii_lowercase();
    if POSITIONS.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_request("invalid wallpaper position"))
    }
}

fn normalize_color(raw: &str) -> AppResult<String> {
    let value = raw.trim();
    let hex = value.strip_prefix('#');
    match hex {
        Some(hex) if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
            Ok(format!("#{}", hex.to_ascii_uppercase()))
        }
        _ => Err(bad_request("color must be in #RRGGBB format")),
    }
}

fn validate_overlay(value: i64) -> AppResult<i64> {
    if (0..=60).contains(&value) {
        Ok(value)
    } else {
        Err(bad_request("overlay must be between 0 and 60"))
    }
}

/// File extension used on disk for a supported MIME type.
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        MIME_JPEG => Some("jpg"),
        MIME_PNG => Some("png"),
        MIME_WEBP => Some("webp"),
        _ => None,
    }
}

/// Strict identifier check for server-generated upload ids (defence in depth).
pub fn is_safe_upload_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Ownership predicate: uploads are private to the user who created them.
pub fn owns(record_owner_id: i64, caller: &AuthUser) -> bool {
    record_owner_id == caller.id
}

/// Sanitise a client-supplied file name for *display only*. The value is never
/// used as a filesystem path.
pub fn sanitize_file_name(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "wallpaper".to_string();
    };
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(80).collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "wallpaper".to_string()
    } else {
        cleaned.to_string()
    }
}

// ---------------------------------------------------------------------------
// Image parsing (manual, dependency-free)
// ---------------------------------------------------------------------------

/// Parsed image header information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub mime: &'static str,
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Detect the real image format from the magic bytes.
pub fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= PNG_SIGNATURE.len() && bytes[..PNG_SIGNATURE.len()] == PNG_SIGNATURE {
        return Some(MIME_PNG);
    }
    if bytes.len() >= JPEG_SOI.len() && bytes[..JPEG_SOI.len()] == JPEG_SOI {
        return Some(MIME_JPEG);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(MIME_WEBP);
    }
    None
}

/// Validate raw wallpaper bytes against the declared MIME type and the
/// dimension/format limits. Pure function (no I/O) so it is unit testable.
pub fn validate_wallpaper(bytes: &[u8], declared_content_type: &str) -> AppResult<ImageInfo> {
    let declared = normalize_mime(declared_content_type)?;
    let actual = sniff_format(bytes).ok_or_else(|| {
        bad_request("unsupported image format; expected a JPEG, PNG or WebP image")
    })?;
    if actual != declared {
        return Err(bad_request(format!(
            "image content is {actual} but Content-Type was {declared}"
        )));
    }
    let (width, height) = match actual {
        MIME_JPEG => jpeg_dimensions(bytes)?,
        MIME_PNG => png_dimensions(bytes)?,
        _ => webp_dimensions(bytes)?,
    };
    validate_dimensions(width, height)?;
    Ok(ImageInfo {
        mime: actual,
        ext: ext_for_mime(actual).unwrap_or("bin"),
        width,
        height,
    })
}

fn validate_dimensions(width: u32, height: u32) -> AppResult<()> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return Err(bad_request(format!(
            "wallpaper must be at least {MIN_WIDTH}x{MIN_HEIGHT} pixels (got {width}x{height})"
        )));
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err(bad_request(format!(
            "wallpaper dimensions must not exceed {MAX_SIDE} pixels per side"
        )));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(bad_request(
            "wallpaper is too large; at most 60 megapixels are allowed",
        ));
    }
    Ok(())
}

fn malformed(format: &str) -> AppError {
    bad_request(format!("malformed {format} image"))
}

fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn le_u24(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(3)?)?;
    Some(u32::from(slice[0]) | (u32::from(slice[1]) << 8) | (u32::from(slice[2]) << 16))
}

/// Parse JPEG dimensions from a `SOF0`/`SOF1`/`SOF2` frame header.
fn jpeg_dimensions(bytes: &[u8]) -> AppResult<(u32, u32)> {
    if bytes.len() < 4 || bytes[..3] != JPEG_SOI {
        return Err(malformed("JPEG: missing SOI marker"));
    }
    if !bytes.ends_with(&JPEG_EOI) {
        return Err(malformed("JPEG: missing EOI marker"));
    }
    let mut offset = 2usize;
    let mut dimensions: Option<(u32, u32)> = None;
    // Marker segments are small; the loop is bounded by the input length.
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xFF {
            return Err(malformed("JPEG: expected marker"));
        }
        let marker = bytes[offset + 1];
        match marker {
            0xFF => {
                // Fill byte before the actual marker.
                offset += 1;
                continue;
            }
            0x00 => {
                // Stuffed zero outside entropy-coded data; skip defensively.
                offset += 2;
                continue;
            }
            0xD8 => {
                offset += 2;
                continue;
            }
            0xD9 => break,
            0x01 | 0xD0..=0xD7 => {
                // Parameterless markers.
                offset += 2;
                continue;
            }
            _ => {}
        }
        let Some(length) = be_u16(bytes, offset + 2).map(usize::from) else {
            return Err(malformed("JPEG: truncated segment length"));
        };
        if length < 2 || offset + 2 + length > bytes.len() {
            return Err(malformed("JPEG: truncated segment"));
        }
        match marker {
            // Baseline, extended sequential and progressive frame headers.
            0xC0..=0xC2 => {
                if length < 8 {
                    return Err(malformed("JPEG: truncated SOF segment"));
                }
                let height = be_u16(bytes, offset + 5).map(u32::from).unwrap_or(0);
                let width = be_u16(bytes, offset + 7).map(u32::from).unwrap_or(0);
                dimensions = Some((width, height));
            }
            // Arithmetic/hierarchical/lossless variants are outside the
            // supported baseline/progressive set.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(bad_request("unsupported JPEG encoding"));
            }
            0xDA => break,
            _ => {}
        }
        offset += 2 + length;
    }
    dimensions.ok_or_else(|| malformed("JPEG: no frame header"))
}

/// Parse PNG dimensions from the mandatory `IHDR` chunk and verify the chunk
/// structure terminates with a well-formed `IEND`.
fn png_dimensions(bytes: &[u8]) -> AppResult<(u32, u32)> {
    if bytes.len() < PNG_SIGNATURE.len() || bytes[..8] != PNG_SIGNATURE {
        return Err(malformed("PNG: bad signature"));
    }
    let mut offset = 8usize;
    let mut dimensions: Option<(u32, u32)> = None;
    let mut seen_iend = false;
    let mut first_chunk = true;
    while offset + 12 <= bytes.len() {
        let Some(length) = be_u32(bytes, offset).map(|l| l as usize) else {
            return Err(malformed("PNG: truncated chunk header"));
        };
        let Some(end) = offset.checked_add(12).and_then(|v| v.checked_add(length)) else {
            return Err(malformed("PNG: chunk length overflow"));
        };
        if end > bytes.len() {
            return Err(malformed("PNG: truncated chunk"));
        }
        let chunk_type = &bytes[offset + 4..offset + 8];
        if first_chunk {
            if chunk_type != b"IHDR" || length != 13 {
                return Err(malformed("PNG: missing IHDR chunk"));
            }
            dimensions = Some((
                be_u32(bytes, offset + 8).unwrap_or(0),
                be_u32(bytes, offset + 12).unwrap_or(0),
            ));
            first_chunk = false;
        }
        offset = end;
        if chunk_type == b"IEND" {
            if length != 0 {
                return Err(malformed("PNG: invalid IEND chunk"));
            }
            seen_iend = true;
            break;
        }
    }
    if !seen_iend {
        return Err(malformed("PNG: missing IEND chunk"));
    }
    if offset != bytes.len() {
        return Err(malformed("PNG: trailing bytes after IEND"));
    }
    dimensions.ok_or_else(|| malformed("PNG: no IHDR chunk"))
}

/// Parse WebP dimensions for lossy (`VP8 `), lossless (`VP8L`) and extended
/// (`VP8X`) bitstreams.
fn webp_dimensions(bytes: &[u8]) -> AppResult<(u32, u32)> {
    if bytes.len() < 20 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(malformed("WebP: bad RIFF header"));
    }
    let Some(riff_size) = le_u32(bytes, 4).map(|v| v as usize) else {
        return Err(malformed("WebP: truncated RIFF size"));
    };
    if riff_size.checked_add(8) != Some(bytes.len()) {
        return Err(malformed("WebP: inconsistent RIFF size"));
    }
    let Some(chunk_size) = le_u32(bytes, 16).map(|v| v as usize) else {
        return Err(malformed("WebP: truncated chunk header"));
    };
    let Some(chunk_end) = 20usize.checked_add(chunk_size) else {
        return Err(malformed("WebP: chunk length overflow"));
    };
    if chunk_end > bytes.len() {
        return Err(malformed("WebP: truncated chunk"));
    }
    let data = &bytes[20..chunk_end];
    match &bytes[12..16] {
        b"VP8 " => {
            if chunk_size < 10 {
                return Err(malformed("WebP: truncated VP8 frame"));
            }
            if data[3..6] != [0x9D, 0x01, 0x2A] {
                return Err(malformed("WebP: bad VP8 start code"));
            }
            let width = u32::from(le_u16(data, 6).unwrap_or(0) & 0x3FFF);
            let height = u32::from(le_u16(data, 8).unwrap_or(0) & 0x3FFF);
            Ok((width, height))
        }
        b"VP8L" => {
            if chunk_size < 5 || data[0] != 0x2F {
                return Err(malformed("WebP: bad VP8L header"));
            }
            let b1 = u32::from(data[1]);
            let b2 = u32::from(data[2]);
            let b3 = u32::from(data[3]);
            let b4 = u32::from(data[4]);
            let width = 1 + (((b2 & 0x3F) << 8) | b1);
            let height = 1 + (((b4 & 0x0F) << 10) | (b3 << 2) | ((b2 & 0xC0) >> 6));
            Ok((width, height))
        }
        b"VP8X" => {
            if chunk_size < 10 {
                return Err(malformed("WebP: truncated VP8X header"));
            }
            let width = 1 + le_u24(data, 4).unwrap_or(0);
            let height = 1 + le_u24(data, 7).unwrap_or(0);
            Ok((width, height))
        }
        _ => Err(bad_request("unsupported WebP encoding")),
    }
}

// ---------------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------------

/// Root directory for uploaded wallpapers (`<data_dir>/wallpapers`).
pub fn wallpapers_root(data_dir: &Path) -> PathBuf {
    data_dir.join("wallpapers")
}

fn ensure_dir(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Ensure (and return) the private per-user wallpaper directory.
pub fn ensure_user_dir(data_dir: &Path, user_id: i64) -> AppResult<PathBuf> {
    let root = wallpapers_root(data_dir);
    ensure_dir(&root)?;
    let user_dir = root.join(user_id.to_string());
    ensure_dir(&user_dir)?;
    Ok(user_dir)
}

/// Write bytes to a temporary file then atomically rename it to `path`.
async fn write_file_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::internal("invalid wallpaper path"))?;
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

// ---------------------------------------------------------------------------
// Database access
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredWallpaper {
    theme: String,
    accent: String,
    kind: String,
    builtin_id: String,
    upload_id: Option<String>,
    color: String,
    fit: String,
    position: String,
    overlay: i64,
}

impl StoredWallpaper {
    fn defaults() -> Self {
        Self {
            theme: DEFAULT_THEME.to_string(),
            accent: DEFAULT_ACCENT.to_string(),
            kind: "builtin".to_string(),
            builtin_id: DEFAULT_BUILTIN_ID.to_string(),
            upload_id: None,
            color: DEFAULT_COLOR.to_string(),
            fit: DEFAULT_FIT.to_string(),
            position: DEFAULT_POSITION.to_string(),
            overlay: DEFAULT_OVERLAY,
        }
    }

    /// Defensive normalisation of values read back from the database.
    fn sanitized(mut self) -> Self {
        if !THEMES.contains(&self.theme.as_str()) {
            self.theme = DEFAULT_THEME.to_string();
        }
        if !ACCENTS.contains(&self.accent.as_str()) {
            self.accent = DEFAULT_ACCENT.to_string();
        }
        if !WALLPAPER_KINDS.contains(&self.kind.as_str()) {
            self.kind = "builtin".to_string();
        }
        if !BUILTIN_IDS.contains(&self.builtin_id.as_str()) {
            self.builtin_id = DEFAULT_BUILTIN_ID.to_string();
        }
        if !FITS.contains(&self.fit.as_str()) {
            self.fit = DEFAULT_FIT.to_string();
        }
        if !POSITIONS.contains(&self.position.as_str()) {
            self.position = DEFAULT_POSITION.to_string();
        }
        self.overlay = self.overlay.clamp(0, 60);
        if !self.color.starts_with('#') || self.color.len() != 7 {
            self.color = DEFAULT_COLOR.to_string();
        }
        if self
            .upload_id
            .as_deref()
            .is_some_and(|id| !is_safe_upload_id(id))
        {
            self.upload_id = None;
        }
        self
    }
}

async fn load_stored(pool: &SqlitePool, user_id: i64) -> AppResult<StoredWallpaper> {
    let row = sqlx::query(
        "SELECT theme, accent, wallpaper_kind, builtin_id, upload_id, color, fit, position, overlay
         FROM user_appearance WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some(row) => StoredWallpaper {
            theme: row.get("theme"),
            accent: row.get("accent"),
            kind: row.get("wallpaper_kind"),
            builtin_id: row.get("builtin_id"),
            upload_id: row.get("upload_id"),
            color: row.get("color"),
            fit: row.get("fit"),
            position: row.get("position"),
            overlay: row.get("overlay"),
        }
        .sanitized(),
        None => StoredWallpaper::defaults(),
    })
}

fn dto_upload(row: &sqlx::sqlite::SqliteRow) -> AppearanceUploadDto {
    let id: String = row.get("id");
    AppearanceUploadDto {
        url: upload_url(&id),
        id,
        width: row.get("width"),
        height: row.get("height"),
        size_bytes: row.get("size_bytes"),
        mime: row.get("mime"),
        created_at: row.get("created_at"),
    }
}

/// List a user's uploaded wallpapers, newest first.
pub async fn list_uploads(pool: &SqlitePool, user_id: i64) -> AppResult<Vec<AppearanceUploadDto>> {
    let rows = sqlx::query(
        "SELECT id, width, height, size_bytes, mime, created_at
         FROM appearance_uploads WHERE user_id = ?
         ORDER BY created_at DESC, id DESC LIMIT 100",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(dto_upload).collect())
}

async fn upload_exists(pool: &SqlitePool, user_id: i64, upload_id: &str) -> AppResult<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM appearance_uploads WHERE id = ? AND user_id = ?")
            .bind(upload_id)
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

fn build_appearance(stored: StoredWallpaper, uploads: Vec<AppearanceUploadDto>) -> AppearanceDto {
    AppearanceDto {
        theme: stored.theme,
        accent: stored.accent,
        wallpaper: WallpaperSettingsDto {
            kind: stored.kind,
            builtin_id: stored.builtin_id,
            upload_id: stored.upload_id,
            color: stored.color,
            fit: stored.fit,
            position: stored.position,
            overlay: stored.overlay,
        },
        uploads,
        limits: LimitsDto {
            max_bytes: MAX_UPLOAD_BYTES,
            max_uploads: MAX_UPLOADS_PER_USER,
        },
    }
}

/// Load a user's appearance (defaults when the user never customised it).
pub async fn get(pool: &SqlitePool, user_id: i64) -> AppResult<AppearanceDto> {
    let mut stored = load_stored(pool, user_id).await?;
    let uploads = list_uploads(pool, user_id).await?;
    // Repair a dangling upload reference (e.g. row deleted out-of-band).
    if stored.kind == "upload" {
        let valid = match stored.upload_id.as_deref() {
            Some(id) => uploads.iter().any(|upload| upload.id == id),
            None => false,
        };
        if !valid {
            stored.kind = "builtin".to_string();
            stored.builtin_id = DEFAULT_BUILTIN_ID.to_string();
            stored.upload_id = None;
        }
    }
    Ok(build_appearance(stored, uploads))
}

/// Get just the wallpaper preferences (used by the reset/delete flows).
async fn store(pool: &SqlitePool, user_id: i64, stored: &StoredWallpaper) -> AppResult<()> {
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO user_appearance
            (user_id, theme, accent, wallpaper_kind, builtin_id, upload_id, color, fit, position, overlay, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET
            theme = excluded.theme,
            accent = excluded.accent,
            wallpaper_kind = excluded.wallpaper_kind,
            builtin_id = excluded.builtin_id,
            upload_id = excluded.upload_id,
            color = excluded.color,
            fit = excluded.fit,
            position = excluded.position,
            overlay = excluded.overlay,
            updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(&stored.theme)
    .bind(&stored.accent)
    .bind(&stored.kind)
    .bind(&stored.builtin_id)
    .bind(&stored.upload_id)
    .bind(&stored.color)
    .bind(&stored.fit)
    .bind(&stored.position)
    .bind(stored.overlay)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Serialises appearance read-modify-write cycles. Updates are partial (the
/// tray theme toggle sends only `theme`, an editor only what changed), so two
/// concurrent requests from different tabs must not both start from the same
/// stored row and silently drop each other's field.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Apply a partial update to a user's appearance settings: fields that are
/// absent from `patch` keep their stored value.
pub async fn update(
    pool: &SqlitePool,
    user: &AuthUser,
    patch: UpdateAppearanceRequest,
) -> AppResult<AppearanceDto> {
    let _write = WRITE_LOCK.lock().await;
    let mut stored = load_stored(pool, user.id).await?;
    if let Some(theme) = patch.theme.as_deref() {
        stored.theme = normalize_theme(theme)?;
    }
    if let Some(accent) = patch.accent.as_deref() {
        stored.accent = normalize_accent(accent)?;
    }
    if let Some(wallpaper) = patch.wallpaper {
        if let Some(kind) = wallpaper.kind.as_deref() {
            stored.kind = normalize_kind(kind)?;
        }
        if let Some(builtin_id) = wallpaper.builtin_id.as_deref() {
            stored.builtin_id = normalize_builtin(builtin_id)?;
        }
        if let Some(upload_id) = wallpaper.upload_id.as_deref() {
            stored.upload_id = Some(upload_id.to_string());
        }
        if let Some(color) = wallpaper.color.as_deref() {
            stored.color = normalize_color(color)?;
        }
        if let Some(fit) = wallpaper.fit.as_deref() {
            stored.fit = normalize_fit(fit)?;
        }
        if let Some(position) = wallpaper.position.as_deref() {
            stored.position = normalize_position(position)?;
        }
        if let Some(overlay) = wallpaper.overlay {
            stored.overlay = validate_overlay(overlay)?;
        }
    }
    match stored.kind.as_str() {
        "upload" => {
            let upload_id = stored
                .upload_id
                .as_deref()
                .filter(|id| is_safe_upload_id(id))
                .ok_or_else(|| {
                    bad_request("uploadId is required when the wallpaper kind is upload")
                })?;
            if !upload_exists(pool, user.id, upload_id).await? {
                return Err(AppError::NotFound("wallpaper upload not found"));
            }
        }
        // Any other kind does not reference an uploaded wallpaper.
        _ => stored.upload_id = None,
    }
    if !BUILTIN_IDS.contains(&stored.builtin_id.as_str()) {
        stored.builtin_id = DEFAULT_BUILTIN_ID.to_string();
    }
    store(pool, user.id, &stored).await?;
    get(pool, user.id).await
}

/// Reset the wallpaper to defaults while keeping the chosen theme.
pub async fn reset(pool: &SqlitePool, user: &AuthUser) -> AppResult<AppearanceDto> {
    let _write = WRITE_LOCK.lock().await;
    let mut stored = load_stored(pool, user.id).await?;
    let defaults = StoredWallpaper::defaults();
    stored = StoredWallpaper {
        theme: stored.theme,
        accent: stored.accent,
        ..defaults
    };
    store(pool, user.id, &stored).await?;
    get(pool, user.id).await
}

/// Persist a validated wallpaper upload. Returns the created upload object.
pub async fn store_upload(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &AuthUser,
    declared_content_type: &str,
    file_name: Option<&str>,
    bytes: &[u8],
) -> AppResult<AppearanceUploadDto> {
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        return Err(AppError::PayloadTooLarge(
            "wallpaper is larger than the 10 MiB limit",
        ));
    }
    // Fast pre-check for a clear error; the conditional INSERT below is the
    // race-safe enforcement of the same limits.
    let row = sqlx::query(
        "SELECT COUNT(*) AS count, COALESCE(SUM(size_bytes), 0) AS total
         FROM appearance_uploads WHERE user_id = ?",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await?;
    let count: i64 = row.get("count");
    let total: i64 = row.get("total");
    if count >= MAX_UPLOADS_PER_USER {
        return Err(AppError::conflict(format!(
            "maximum of {MAX_UPLOADS_PER_USER} uploaded wallpapers reached; delete one first"
        )));
    }
    let size_bytes = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    if total.saturating_add(size_bytes) > MAX_TOTAL_BYTES_PER_USER {
        return Err(AppError::PayloadTooLarge(
            "wallpaper storage limit exceeded (100 MiB per user)",
        ));
    }
    let info = validate_wallpaper(bytes, declared_content_type)?;
    let user_dir = ensure_user_dir(data_dir, user.id)?;
    let id = Uuid::new_v4().to_string();
    let path = user_dir.join(format!("{id}.{}", info.ext));
    write_file_atomic(&path, bytes).await?;
    let display_name = sanitize_file_name(file_name);
    let created_at = db::now_ms();
    let inserted = sqlx::query(
        "INSERT INTO appearance_uploads
            (id, user_id, mime, width, height, size_bytes, file_name, created_at)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?
         WHERE (SELECT COUNT(*) FROM appearance_uploads WHERE user_id = ?) < ?
           AND (SELECT COALESCE(SUM(size_bytes), 0) FROM appearance_uploads WHERE user_id = ?)
               + ? <= ?",
    )
    .bind(&id)
    .bind(user.id)
    .bind(info.mime)
    .bind(i64::from(info.width))
    .bind(i64::from(info.height))
    .bind(size_bytes)
    .bind(&display_name)
    .bind(created_at)
    .bind(user.id)
    .bind(MAX_UPLOADS_PER_USER)
    .bind(user.id)
    .bind(size_bytes)
    .bind(MAX_TOTAL_BYTES_PER_USER)
    .execute(pool)
    .await;
    match inserted {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            let _ = tokio::fs::remove_file(&path).await;
            // Re-check to report which limit was hit.
            let row = sqlx::query(
                "SELECT COUNT(*) AS count, COALESCE(SUM(size_bytes), 0) AS total
                 FROM appearance_uploads WHERE user_id = ?",
            )
            .bind(user.id)
            .fetch_one(pool)
            .await?;
            let count: i64 = row.get("count");
            let total: i64 = row.get("total");
            if count >= MAX_UPLOADS_PER_USER {
                return Err(AppError::conflict(format!(
                    "maximum of {MAX_UPLOADS_PER_USER} uploaded wallpapers reached; delete one first"
                )));
            }
            let _ = total;
            return Err(AppError::PayloadTooLarge(
                "wallpaper storage limit exceeded (100 MiB per user)",
            ));
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(error.into());
        }
    }
    Ok(AppearanceUploadDto {
        id: id.clone(),
        width: i64::from(info.width),
        height: i64::from(info.height),
        size_bytes,
        mime: info.mime.to_string(),
        created_at,
        url: upload_url(&id),
    })
}

/// Delete an uploaded wallpaper (owner only). If it was the active wallpaper,
/// the appearance falls back to the default built-in wallpaper (theme kept).
pub async fn delete_upload(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &AuthUser,
    upload_id: &str,
) -> AppResult<AppearanceDto> {
    if !is_safe_upload_id(upload_id) {
        return Err(AppError::NotFound("wallpaper not found"));
    }
    let row = sqlx::query("SELECT mime FROM appearance_uploads WHERE id = ? AND user_id = ?")
        .bind(upload_id)
        .bind(user.id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound("wallpaper not found"))?;
    let mime: String = row.get("mime");
    let write = WRITE_LOCK.lock().await;
    sqlx::query("DELETE FROM appearance_uploads WHERE id = ? AND user_id = ?")
        .bind(upload_id)
        .bind(user.id)
        .execute(pool)
        .await?;
    // Fall back to the built-in default when the deleted file was referenced.
    sqlx::query(
        "UPDATE user_appearance
         SET wallpaper_kind = 'builtin', builtin_id = ?, upload_id = NULL,
             color = ?, fit = ?, position = ?, overlay = ?, updated_at = ?
         WHERE user_id = ? AND upload_id = ?",
    )
    .bind(DEFAULT_BUILTIN_ID)
    .bind(DEFAULT_COLOR)
    .bind(DEFAULT_FIT)
    .bind(DEFAULT_POSITION)
    .bind(DEFAULT_OVERLAY)
    .bind(db::now_ms())
    .bind(user.id)
    .bind(upload_id)
    .execute(pool)
    .await?;
    drop(write);
    if let Some(ext) = ext_for_mime(&mime) {
        let path = wallpapers_root(data_dir)
            .join(user.id.to_string())
            .join(format!("{upload_id}.{ext}"));
        if let Err(error) = tokio::fs::remove_file(&path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %error, upload = %upload_id, "cannot remove wallpaper file");
            }
        }
    }
    get(pool, user.id).await
}

/// Look up an upload for serving. The `user_id` predicate is what enforces
/// privacy: another user (even an admin) gets a 404.
pub async fn upload_for_serving(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &AuthUser,
    upload_id: &str,
) -> AppResult<ServeUpload> {
    if !is_safe_upload_id(upload_id) {
        return Err(AppError::NotFound("wallpaper not found"));
    }
    let row = sqlx::query(
        "SELECT id, mime, size_bytes FROM appearance_uploads WHERE id = ? AND user_id = ?",
    )
    .bind(upload_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound("wallpaper not found"))?;
    let id: String = row.get("id");
    if !owns(user.id, user) {
        return Err(AppError::NotFound("wallpaper not found"));
    }
    let mime: String = row.get("mime");
    let size_bytes: i64 = row.get("size_bytes");
    let ext = ext_for_mime(&mime).ok_or_else(|| AppError::internal("invalid wallpaper MIME"))?;
    if !is_safe_upload_id(&id) {
        return Err(AppError::internal("invalid wallpaper identifier"));
    }
    let path = wallpapers_root(data_dir)
        .join(user.id.to_string())
        .join(format!("{id}.{ext}"));
    match tokio::fs::symlink_metadata(&path).await {
        Ok(meta) if meta.is_file() => {}
        _ => return Err(AppError::NotFound("wallpaper not found")),
    }
    Ok(ServeUpload {
        id,
        mime,
        size_bytes,
        path,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{auth::Role, db};

    // ---------------------------------------------------------------- fixtures

    pub(crate) fn jpeg_fixture(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8];
        // Minimal APP0 segment (empty payload).
        data.extend([0xFF, 0xE0, 0x00, 0x02]);
        // SOF0: precision 8, height, width, 3 components.
        data.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
        data.extend(height.to_be_bytes());
        data.extend(width.to_be_bytes());
        data.push(3);
        for component in 1..=3u8 {
            data.extend([component, 0x11, 0x00]);
        }
        // Minimal SOS for the 3 components.
        data.extend([
            0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x3F, 0x00,
        ]);
        data.extend([0xFF, 0xD9]);
        data
    }

    pub(crate) fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut data = PNG_SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8u8, 6, 0, 0, 0]);
        data.extend((ihdr.len() as u32).to_be_bytes());
        data.extend(b"IHDR");
        data.extend(ihdr);
        data.extend([0, 0, 0, 0]); // CRC (not validated)
        data.extend(0u32.to_be_bytes());
        data.extend(b"IEND");
        data.extend([0, 0, 0, 0]);
        data
    }

    fn webp_container(fourcc: [u8; 4], chunk: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(b"RIFF");
        data.extend(((4 + 8 + chunk.len()) as u32).to_le_bytes());
        data.extend(b"WEBP");
        data.extend(fourcc);
        data.extend((chunk.len() as u32).to_le_bytes());
        data.extend(chunk);
        data
    }

    pub(crate) fn webp_lossy_fixture(width: u16, height: u16) -> Vec<u8> {
        let mut chunk = vec![0u8; 10];
        chunk[3..6].copy_from_slice(&[0x9D, 0x01, 0x2A]);
        chunk[6..8].copy_from_slice(&(width & 0x3FFF).to_le_bytes());
        chunk[8..10].copy_from_slice(&(height & 0x3FFF).to_le_bytes());
        webp_container(*b"VP8 ", &chunk)
    }

    pub(crate) fn webp_lossless_fixture(width: u32, height: u32) -> Vec<u8> {
        let width = width - 1;
        let height = height - 1;
        let chunk = [
            0x2F,
            (width & 0xFF) as u8,
            (((width >> 8) & 0x3F) | ((height & 0x03) << 6)) as u8,
            ((height >> 2) & 0xFF) as u8,
            ((height >> 10) & 0x0F) as u8,
        ];
        webp_container(*b"VP8L", &chunk)
    }

    pub(crate) fn webp_extended_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut chunk = vec![0u8; 10];
        let w = width - 1;
        let h = height - 1;
        chunk[4..7].copy_from_slice(&[w as u8, (w >> 8) as u8, (w >> 16) as u8]);
        chunk[7..10].copy_from_slice(&[h as u8, (h >> 8) as u8, (h >> 16) as u8]);
        webp_container(*b"VP8X", &chunk)
    }

    fn auth_user(id: i64, username: &str) -> AuthUser {
        AuthUser {
            id,
            username: username.to_string(),
            display_name: username.to_string(),
            role: Role::User,
            session_id: "test".into(),
            quota_bytes: 0,
            must_change_password: false,
        }
    }

    // ------------------------------------------------------------- image tests

    #[test]
    fn parses_valid_jpeg_dimensions() {
        let data = jpeg_fixture(1920, 1080);
        assert_eq!(sniff_format(&data), Some(MIME_JPEG));
        let info = validate_wallpaper(&data, "image/jpeg").unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.ext, "jpg");
        // Content-Type parameters are ignored.
        assert!(validate_wallpaper(&data, "image/jpeg; charset=binary").is_ok());
    }

    #[test]
    fn rejects_corrupt_jpeg() {
        let valid = jpeg_fixture(1920, 1080);
        // Missing EOI.
        let mut missing_eoi = valid.clone();
        missing_eoi.truncate(valid.len() - 2);
        assert!(validate_wallpaper(&missing_eoi, "image/jpeg").is_err());
        // Missing SOI.
        let mut missing_soi = valid.clone();
        missing_soi[0] = 0x00;
        assert!(validate_wallpaper(&missing_soi, "image/jpeg").is_err());
        // Truncated SOF segment.
        let truncated = valid[..6].to_vec();
        assert!(validate_wallpaper(&truncated, "image/jpeg").is_err());
        // Unsupported SOF variant (SOF3 = 0xC3).
        let mut unsupported = valid.clone();
        let sof = unsupported
            .windows(2)
            .position(|pair| pair == [0xFF, 0xC0])
            .unwrap();
        unsupported[sof + 1] = 0xC3;
        assert!(validate_wallpaper(&unsupported, "image/jpeg").is_err());
    }

    #[test]
    fn parses_valid_png_dimensions() {
        let data = png_fixture(2560, 1440);
        assert_eq!(sniff_format(&data), Some(MIME_PNG));
        let info = validate_wallpaper(&data, "image/png").unwrap();
        assert_eq!((info.width, info.height), (2560, 1440));
        assert_eq!(info.ext, "png");
    }

    #[test]
    fn rejects_corrupt_png() {
        let valid = png_fixture(2560, 1440);
        // Bad signature.
        let mut bad_sig = valid.clone();
        bad_sig[1] = b'X';
        assert!(validate_wallpaper(&bad_sig, "image/png").is_err());
        // Missing IEND.
        let truncated = valid[..valid.len() - 12].to_vec();
        assert!(validate_wallpaper(&truncated, "image/png").is_err());
        // Truncated chunk.
        let truncated_chunk = valid[..20].to_vec();
        assert!(validate_wallpaper(&truncated_chunk, "image/png").is_err());
        // Trailing garbage after IEND.
        let mut trailing = valid.clone();
        trailing.push(0x00);
        assert!(validate_wallpaper(&trailing, "image/png").is_err());
        // IHDR missing (first chunk is IEND).
        let mut no_ihdr = PNG_SIGNATURE.to_vec();
        no_ihdr.extend(0u32.to_be_bytes());
        no_ihdr.extend(b"IEND");
        no_ihdr.extend([0, 0, 0, 0]);
        assert!(validate_wallpaper(&no_ihdr, "image/png").is_err());
    }

    #[test]
    fn parses_valid_webp_dimensions() {
        for (data, format) in [
            (webp_lossy_fixture(1920, 1080), "VP8 "),
            (webp_lossless_fixture(1920, 1080), "VP8L"),
            (webp_extended_fixture(1920, 1080), "VP8X"),
        ] {
            assert_eq!(sniff_format(&data), Some(MIME_WEBP));
            let info = validate_wallpaper(&data, "image/webp")
                .unwrap_or_else(|error| panic!("{format} should parse: {error}"));
            assert_eq!((info.width, info.height), (1920, 1080), "{format}");
            assert_eq!(info.ext, "webp");
        }
    }

    #[test]
    fn rejects_corrupt_webp() {
        let valid = webp_lossy_fixture(1920, 1080);
        // Inconsistent RIFF size.
        let mut bad_size = valid.clone();
        bad_size[4] = bad_size[4].wrapping_add(1);
        assert!(validate_wallpaper(&bad_size, "image/webp").is_err());
        // Truncated chunk.
        let truncated = valid[..24].to_vec();
        assert!(validate_wallpaper(&truncated, "image/webp").is_err());
        // Bad VP8 start code.
        let mut bad_code = valid.clone();
        bad_code[23] = 0x00;
        assert!(validate_wallpaper(&bad_code, "image/webp").is_err());
        // Unknown first chunk.
        let mut unknown = valid.clone();
        unknown[12..16].copy_from_slice(b"ANIM");
        assert!(validate_wallpaper(&unknown, "image/webp").is_err());
    }

    #[test]
    fn enforces_dimension_limits() {
        // Too small (each side).
        assert!(validate_wallpaper(&png_fixture(300, 400), "image/png").is_err());
        assert!(validate_wallpaper(&png_fixture(800, 100), "image/png").is_err());
        // Exactly at the minimum.
        assert!(validate_wallpaper(&png_fixture(320, 180), "image/png").is_ok());
        // Too many pixels: 8000x8000 = 64 MP.
        assert!(validate_wallpaper(&png_fixture(8000, 8000), "image/png").is_err());
        // 7700x7700 ≈ 59.3 MP is accepted.
        assert!(validate_wallpaper(&png_fixture(7700, 7700), "image/png").is_ok());
        // Side limit.
        assert!(validate_wallpaper(&png_fixture(10001, 400), "image/png").is_err());
        // 10000x600 = 6 MP, accepted.
        assert!(validate_wallpaper(&png_fixture(10000, 600), "image/png").is_ok());
    }

    #[test]
    fn mime_must_match_actual_format() {
        let png = png_fixture(800, 600);
        let error = validate_wallpaper(&png, "image/jpeg").unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        let error = validate_wallpaper(&png, "image/gif").unwrap_err();
        assert_eq!(
            error.status(),
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert!(validate_wallpaper(b"not an image", "image/png").is_err());
        assert!(validate_wallpaper(&[], "image/png").is_err());
    }

    #[test]
    fn helpers_are_safe() {
        assert_eq!(sanitize_file_name(None), "wallpaper");
        assert_eq!(sanitize_file_name(Some("../../etc/passwd")), "passwd");
        assert_eq!(sanitize_file_name(Some("C:\\x\\beach.jpg")), "beach.jpg");
        assert_eq!(sanitize_file_name(Some("...")), "wallpaper");
        assert_eq!(sanitize_file_name(Some("\u{0}\u{7}")), "wallpaper");
        assert_eq!(sanitize_file_name(Some(&"a".repeat(200))).len(), 80);
        assert!(is_safe_upload_id("5d0c5ad0-1f2e-4a21-9d0a-123456789abc"));
        assert!(!is_safe_upload_id("../etc/passwd"));
        assert!(!is_safe_upload_id(""));
        assert!(!is_safe_upload_id("a/b"));
        assert!(!is_safe_upload_id(&"a".repeat(65)));
        assert_eq!(ext_for_mime(MIME_JPEG), Some("jpg"));
        assert_eq!(ext_for_mime(MIME_PNG), Some("png"));
        assert_eq!(ext_for_mime(MIME_WEBP), Some("webp"));
        assert_eq!(ext_for_mime("image/gif"), None);
        let alice = auth_user(1, "alice");
        assert!(owns(1, &alice));
        assert!(!owns(2, &alice));
    }

    #[test]
    fn color_validation() {
        assert_eq!(normalize_color("#1b2430").unwrap(), "#1B2430");
        for bad in ["1B2430", "#12345", "#1234567", "#GGGGGG", "", "#12 456"] {
            assert!(normalize_color(bad).is_err(), "{bad:?} accepted");
        }
    }

    // --------------------------------------------------------------- db tests

    async fn pool_with_users(dir: &std::path::Path) -> SqlitePool {
        let pool = db::connect(&dir.join("appearance.db"), 4).await.unwrap();
        for (id, username) in [(1_i64, "alice"), (2, "bob")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
                 VALUES (?, ?, 'x', 'user', 1, 1)",
            )
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn defaults_update_and_reset_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_with_users(dir.path()).await;
        let alice = auth_user(1, "alice");

        let defaults = get(&pool, 1).await.unwrap();
        assert_eq!(defaults.theme, "light");
        assert_eq!(defaults.wallpaper.kind, "builtin");
        assert_eq!(defaults.wallpaper.builtin_id, DEFAULT_BUILTIN_ID);
        assert_eq!(defaults.wallpaper.overlay, 20);
        assert!(defaults.uploads.is_empty());
        assert_eq!(defaults.limits.max_bytes, MAX_UPLOAD_BYTES);
        assert_eq!(defaults.limits.max_uploads, MAX_UPLOADS_PER_USER);

        let updated = update(
            &pool,
            &alice,
            UpdateAppearanceRequest {
                accent: None,
                theme: Some("dark".into()),
                wallpaper: Some(WallpaperPatch {
                    kind: Some("builtin".into()),
                    builtin_id: Some("forest-mist".into()),
                    upload_id: None,
                    color: Some("#0a0b0c".into()),
                    fit: Some("contain".into()),
                    position: Some("top-left".into()),
                    overlay: Some(45),
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.theme, "dark");
        assert_eq!(updated.wallpaper.builtin_id, "forest-mist");
        assert_eq!(updated.wallpaper.color, "#0A0B0C");
        assert_eq!(updated.wallpaper.fit, "contain");
        assert_eq!(updated.wallpaper.position, "top-left");
        assert_eq!(updated.wallpaper.overlay, 45);
        // Persisted.
        assert_eq!(get(&pool, 1).await.unwrap().theme, "dark");

        // Validation failures.
        for patch in [
            WallpaperPatch {
                kind: Some("nope".into()),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                builtin_id: Some("unknown".into()),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                fit: Some("stretch".into()),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                position: Some("middle".into()),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                color: Some("red".into()),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                overlay: Some(61),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                overlay: Some(-1),
                ..WallpaperPatch::default()
            },
            WallpaperPatch {
                kind: Some("upload".into()),
                ..WallpaperPatch::default()
            },
        ] {
            assert!(update(
                &pool,
                &alice,
                UpdateAppearanceRequest {
                    accent: None,
                    theme: None,
                    wallpaper: Some(patch)
                }
            )
            .await
            .is_err());
        }
        assert!(update(
            &pool,
            &alice,
            UpdateAppearanceRequest {
                accent: None,
                theme: Some("sepia".into()),
                wallpaper: None
            }
        )
        .await
        .is_err());

        // Reset keeps the theme but restores wallpaper defaults.
        let reset = reset(&pool, &alice).await.unwrap();
        assert_eq!(reset.theme, "dark");
        assert_eq!(reset.wallpaper.kind, "builtin");
        assert_eq!(reset.wallpaper.builtin_id, DEFAULT_BUILTIN_ID);
        assert_eq!(reset.wallpaper.fit, DEFAULT_FIT);
        assert_eq!(reset.wallpaper.overlay, DEFAULT_OVERLAY);
    }

    #[tokio::test]
    async fn upload_store_serve_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_with_users(dir.path()).await;
        let alice = auth_user(1, "alice");
        let data_dir = dir.path().join("data");
        let image = png_fixture(1280, 720);

        let upload = store_upload(
            &pool,
            &data_dir,
            &alice,
            "image/png",
            Some("beach.png"),
            &image,
        )
        .await
        .unwrap();
        assert_eq!(upload.width, 1280);
        assert_eq!(upload.height, 720);
        assert_eq!(upload.size_bytes, image.len() as i64);
        assert_eq!(upload.mime, "image/png");
        assert!(upload.url.starts_with("/api/appearance/wallpapers/"));
        // File is inside the private per-user directory.
        let path = wallpapers_root(&data_dir)
            .join("1")
            .join(format!("{}.png", upload.id));
        assert_eq!(std::fs::read(&path).unwrap(), image);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(wallpapers_root(&data_dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // Listed for the owner only.
        assert_eq!(list_uploads(&pool, 1).await.unwrap().len(), 1);
        assert!(list_uploads(&pool, 2).await.unwrap().is_empty());

        // The owner can fetch it, other users cannot (SQL-scoped).
        let served = upload_for_serving(&pool, &data_dir, &alice, &upload.id)
            .await
            .unwrap();
        assert_eq!(served.mime, "image/png");
        assert_eq!(served.size_bytes, image.len() as i64);
        let bob = auth_user(2, "bob");
        assert!(upload_for_serving(&pool, &data_dir, &bob, &upload.id)
            .await
            .is_err());
        // Malformed ids do not reach the filesystem.
        assert!(upload_for_serving(&pool, &data_dir, &alice, "../etc")
            .await
            .is_err());

        // Activate the upload, then delete it: appearance falls back to builtin.
        let active = update(
            &pool,
            &alice,
            UpdateAppearanceRequest {
                accent: None,
                theme: None,
                wallpaper: Some(WallpaperPatch {
                    kind: Some("upload".into()),
                    upload_id: Some(upload.id.clone()),
                    ..WallpaperPatch::default()
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(active.wallpaper.kind, "upload");
        assert_eq!(
            active.wallpaper.upload_id.as_deref(),
            Some(upload.id.as_str())
        );

        let after = delete_upload(&pool, &data_dir, &alice, &upload.id)
            .await
            .unwrap();
        assert_eq!(after.wallpaper.kind, "builtin");
        assert_eq!(after.wallpaper.upload_id, None);
        assert!(after.uploads.is_empty());
        assert!(!path.exists());
        assert!(delete_upload(&pool, &data_dir, &alice, &upload.id)
            .await
            .is_err());
        // Bob cannot delete Alice's upload even if he knew the id.
        assert!(delete_upload(&pool, &data_dir, &bob, "some-other-id")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn upload_limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_with_users(dir.path()).await;
        let alice = auth_user(1, "alice");
        let data_dir = dir.path().join("data");

        // Too many uploads (20) — simulate rows without real files.
        for index in 0..MAX_UPLOADS_PER_USER {
            sqlx::query(
                "INSERT INTO appearance_uploads
                    (id, user_id, mime, width, height, size_bytes, file_name, created_at)
                 VALUES (?, 1, 'image/png', 320, 180, 1, 'x.png', ?)",
            )
            .bind(format!("upload-{index}"))
            .bind(index)
            .execute(&pool)
            .await
            .unwrap();
        }
        let error = store_upload(
            &pool,
            &data_dir,
            &alice,
            "image/png",
            Some("new.png"),
            &png_fixture(800, 600),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);

        // Total byte budget: 5 rows of 20 MiB already exactly fills 100 MiB.
        sqlx::query("DELETE FROM appearance_uploads")
            .execute(&pool)
            .await
            .unwrap();
        for index in 0..5 {
            sqlx::query(
                "INSERT INTO appearance_uploads
                    (id, user_id, mime, width, height, size_bytes, file_name, created_at)
                 VALUES (?, 1, 'image/png', 320, 180, ?, 'x.png', ?)",
            )
            .bind(format!("big-{index}"))
            .bind(20 * 1024 * 1024_i64)
            .bind(index)
            .execute(&pool)
            .await
            .unwrap();
        }
        let error = store_upload(
            &pool,
            &data_dir,
            &alice,
            "image/png",
            Some("new.png"),
            &png_fixture(800, 600),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn other_users_uploads_cannot_be_selected() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_with_users(dir.path()).await;
        let alice = auth_user(1, "alice");
        let bob = auth_user(2, "bob");
        let data_dir = dir.path().join("data");
        let upload = store_upload(
            &pool,
            &data_dir,
            &alice,
            "image/png",
            None,
            &png_fixture(800, 600),
        )
        .await
        .unwrap();
        // Bob cannot reference (or even guess) Alice's upload in PUT.
        let error = update(
            &pool,
            &bob,
            UpdateAppearanceRequest {
                accent: None,
                theme: None,
                wallpaper: Some(WallpaperPatch {
                    kind: Some("upload".into()),
                    upload_id: Some(upload.id),
                    ..WallpaperPatch::default()
                }),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
