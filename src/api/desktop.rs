//! Desktop session API: the windows an account has open on its desktop.
//!
//! The browser saves the session as windows open, move and close, and loads
//! it when the desktop starts, so a reload, another tab or another device
//! shows the same windows: which apps, where, how big, minimized, maximized
//! or snapped, their stacking order and where each app was (`params`, which
//! the app reads when it opens). Each account only ever reads and writes its
//! own session.
//!
//! A window may also keep what is inside it (`view`): where its scrollable
//! areas are scrolled to and what was typed into its text fields and not
//! saved yet (never password fields; the browser leaves them out), and the
//! dialogs open in it (`dialogs`: the app's name for each, what it is about,
//! and its own `view`), which the app opens again.
//!
//! Every save is published to the account's open pages as a
//! `desktop.session` event (`{session, updatedAt, origin}`), so the other
//! tabs and devices show the change live. `updatedAt` only ever grows for an
//! account: pages apply a session only when it is newer than the one they
//! have, and ignore their own saves by `origin` (the `X-RAST-Desktop` header
//! the page sent, a random id per page).

use axum::{extract::State, http::HeaderMap, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::AuthUser,
    db,
    error::{AppError, AppResult},
    state::AppState,
};

/// More windows than the desktop opens at once (it restores at most 8).
const MAX_WINDOWS: usize = 16;
/// What one app may keep about where it was (a folder path, a tab).
const MAX_PARAMS_BYTES: usize = 4 * 1024;
/// Far outside any screen: a window there is a bug, not a position.
const MAX_COORDINATE: f64 = 100_000.0;
/// Scroll positions and typed text of one window.
const MAX_VIEW_BYTES: usize = 192 * 1024;
const MAX_VIEW_SCROLLS: usize = 32;
const MAX_VIEW_FIELDS: usize = 64;
const MAX_FIELD_BYTES: usize = 64 * 1024;
/// A scroll area or a field is named by where it is in the window.
const MAX_VIEW_NAME: usize = 512;
/// Pixels scrolled: far more than any page.
const MAX_SCROLL: f64 = 10_000_000.0;
/// Dialogs open in one window (one may open another on top).
const MAX_DIALOGS: usize = 4;
/// Header naming the page that saved (echoed as the event's `origin`).
pub const ORIGIN_HEADER: &str = "x-rast-desktop";
/// Realtime event published to the account's pages after every save.
pub const SESSION_EVENT: &str = "desktop.session";

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/desktop/session", get(get_session).put(put_session))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopSession {
    pub version: u32,
    pub windows: Vec<SavedWindow>,
    /// Key of the focused window.
    #[serde(default)]
    pub focused: Option<String>,
    /// Size of the desktop the session was saved on.
    #[serde(default)]
    pub stage: Option<Size>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedWindow {
    /// Stable id of the window within the session.
    pub key: String,
    pub app_id: String,
    #[serde(default)]
    pub params: Option<Value>,
    /// Position and size when neither maximized nor snapped (`None`: the
    /// default place, e.g. a window first opened on a phone).
    #[serde(default)]
    pub rect: Option<Rect>,
    pub state: WindowState,
    #[serde(default)]
    pub minimized: bool,
    /// Scroll positions and typed text inside the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<WindowView>,
    /// Dialogs open in the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialogs: Vec<SavedDialog>,
}

/// A dialog the app opens again: its name, what it is about, what was typed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedDialog {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<WindowView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindowView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scroll: Vec<ScrollPosition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldText>,
}

/// A scrolled area: pixels (on a window as wide as `width`) and the share
/// of the way down (`ratio`, for windows of another width).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScrollPosition {
    pub path: String,
    pub x: f64,
    pub y: f64,
    pub ratio: f64,
    pub width: f64,
}

/// Text typed into a field of the window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FieldText {
    pub field: String,
    pub value: String,
}

impl WindowView {
    fn validate(&self, app_id: &str) -> Result<(), String> {
        if self.scroll.len() > MAX_VIEW_SCROLLS || self.fields.len() > MAX_VIEW_FIELDS {
            return Err(format!(
                "{app_id} keeps too many scroll positions or fields"
            ));
        }
        for position in &self.scroll {
            let pixels = |v: f64| v.is_finite() && (0.0..=MAX_SCROLL).contains(&v);
            if position.path.len() > MAX_VIEW_NAME
                || !pixels(position.x)
                || !pixels(position.y)
                || !pixels(position.width)
                || !(position.ratio.is_finite() && (0.0..=1.0).contains(&position.ratio))
            {
                return Err(format!("a scroll position of {app_id} is not valid"));
            }
        }
        for field in &self.fields {
            if field.field.is_empty() || field.field.len() > MAX_VIEW_NAME {
                return Err(format!("a field name of {app_id} is not valid"));
            }
            if field.value.len() > MAX_FIELD_BYTES {
                return Err(format!(
                    "text typed in {app_id} is longer than {MAX_FIELD_BYTES} bytes"
                ));
            }
        }
        let size = serde_json::to_vec(self)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MAX_VIEW_BYTES {
            return Err(format!(
                "what {app_id} keeps is larger than {MAX_VIEW_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WindowState {
    Normal,
    Maximized,
    Left,
    Right,
}

fn plain_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn coordinate(value: f64) -> bool {
    value.is_finite() && value.abs() <= MAX_COORDINATE
}

fn positive(value: f64) -> bool {
    coordinate(value) && value > 0.0
}

impl DesktopSession {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported desktop session version {}",
                self.version
            ));
        }
        if self.windows.len() > MAX_WINDOWS {
            return Err(format!("at most {MAX_WINDOWS} windows can be saved"));
        }
        let mut keys = std::collections::HashSet::new();
        for window in &self.windows {
            if !plain_id(&window.key, 64) {
                return Err("a window key must be 1-64 letters, digits, '-' or '_'".into());
            }
            if !keys.insert(window.key.as_str()) {
                return Err(format!("window key {:?} appears twice", window.key));
            }
            if !plain_id(&window.app_id, 64) {
                return Err("an app id must be 1-64 letters, digits, '-' or '_'".into());
            }
            if let Some(params) = &window.params {
                let size = serde_json::to_vec(params)
                    .map(|b| b.len())
                    .unwrap_or(usize::MAX);
                if size > MAX_PARAMS_BYTES {
                    return Err(format!(
                        "the saved state of {} is larger than {MAX_PARAMS_BYTES} bytes",
                        window.app_id
                    ));
                }
            }
            if let Some(view) = &window.view {
                view.validate(&window.app_id)?;
            }
            if window.dialogs.len() > MAX_DIALOGS {
                return Err(format!(
                    "at most {MAX_DIALOGS} dialogs per window can be saved"
                ));
            }
            for dialog in &window.dialogs {
                if !plain_id(&dialog.name, 64) {
                    return Err("a dialog name must be 1-64 letters, digits, '-' or '_'".into());
                }
                if let Some(params) = &dialog.params {
                    let size = serde_json::to_vec(params)
                        .map(|b| b.len())
                        .unwrap_or(usize::MAX);
                    if size > MAX_PARAMS_BYTES {
                        return Err(format!(
                            "what the {} dialog is about is larger than {MAX_PARAMS_BYTES} bytes",
                            dialog.name
                        ));
                    }
                }
                if let Some(view) = &dialog.view {
                    view.validate(&window.app_id)?;
                }
            }
            if let Some(r) = window.rect {
                if !(coordinate(r.x) && coordinate(r.y) && positive(r.w) && positive(r.h)) {
                    return Err(format!(
                        "the position of {} is not on a screen",
                        window.app_id
                    ));
                }
            }
        }
        if let Some(focused) = &self.focused {
            if !keys.contains(focused.as_str()) {
                return Err("the focused window is not in the session".into());
            }
        }
        if let Some(stage) = self.stage {
            if !(positive(stage.w) && positive(stage.h)) {
                return Err("the desktop size is not valid".into());
            }
        }
        Ok(())
    }
}

/// GET /api/desktop/session — `{session, updatedAt}`; `session` is null
/// until the account's desktop saved one.
pub async fn get_session(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT state, updated_at FROM desktop_sessions WHERE user_id = ?")
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?;
    let Some(row) = row else {
        return Ok(Json(json!({"session": null, "updatedAt": null})));
    };
    let raw: String = row.get("state");
    let updated_at: i64 = row.get("updated_at");
    // A session saved by another version that this one cannot read starts
    // the desktop empty instead of failing it.
    let session = serde_json::from_str::<DesktopSession>(&raw)
        .ok()
        .filter(|s| s.validate().is_ok());
    Ok(Json(json!({"session": session, "updatedAt": updated_at})))
}

/// PUT /api/desktop/session — replace the account's session.
pub async fn put_session(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Json(session): Json<DesktopSession>,
) -> AppResult<Json<Value>> {
    session.validate().map_err(AppError::bad_request)?;
    let origin = headers
        .get(ORIGIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| plain_id(v, 64))
        .map(str::to_string);
    let raw = serde_json::to_string(&session)
        .map_err(|e| AppError::internal(format!("cannot store the desktop session: {e}")))?;
    // The version only grows, even for two saves in the same millisecond, so
    // pages can tell which of two sessions is newer.
    let updated_at: i64 = sqlx::query_scalar(
        "INSERT INTO desktop_sessions (user_id, state, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET state = excluded.state,
             updated_at = MAX(excluded.updated_at, desktop_sessions.updated_at + 1)
         RETURNING updated_at",
    )
    .bind(user.id)
    .bind(raw)
    .bind(db::now_ms())
    .fetch_one(&state.db)
    .await?;
    state.events.publish_user(
        SESSION_EVENT,
        user.id,
        json!({"session": session, "updatedAt": updated_at, "origin": origin}),
    );
    Ok(Json(json!({"updatedAt": updated_at})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str) -> SavedWindow {
        SavedWindow {
            key: key.into(),
            app_id: "files".into(),
            params: Some(json!({"path": "/Documents"})),
            rect: Some(Rect {
                x: 10.0,
                y: 20.0,
                w: 800.0,
                h: 500.0,
            }),
            state: WindowState::Normal,
            minimized: false,
            view: None,
            dialogs: Vec::new(),
        }
    }

    fn session(windows: Vec<SavedWindow>) -> DesktopSession {
        DesktopSession {
            version: 1,
            windows,
            focused: None,
            stage: Some(Size {
                w: 1920.0,
                h: 1022.0,
            }),
        }
    }

    #[test]
    fn sessions_are_validated() {
        assert!(session(vec![window("a"), window("b")]).validate().is_ok());
        let mut s = session(vec![window("a"), window("a")]);
        assert!(s.validate().unwrap_err().contains("twice"));
        s = session(vec![window("a")]);
        s.focused = Some("b".into());
        assert!(s.validate().is_err());
        s.focused = Some("a".into());
        assert!(s.validate().is_ok());
        s.version = 2;
        assert!(s.validate().is_err());
        let mut w = window("a");
        w.app_id = "../x".into();
        assert!(session(vec![w]).validate().is_err());
        let mut w = window("a");
        w.rect = Some(Rect {
            x: f64::NAN,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        });
        assert!(session(vec![w]).validate().is_err());
        let mut w = window("a");
        w.rect.as_mut().unwrap().w = 0.0;
        assert!(session(vec![w]).validate().is_err());
        let mut w = window("a");
        w.params = Some(json!({"path": "x".repeat(MAX_PARAMS_BYTES)}));
        assert!(session(vec![w]).validate().unwrap_err().contains("larger"));
        let many: Vec<SavedWindow> = (0..=MAX_WINDOWS)
            .map(|i| window(&format!("w{i}")))
            .collect();
        assert!(session(many).validate().is_err());
        // What is inside a window: scroll positions and typed text.
        let mut w = window("a");
        w.view = Some(WindowView {
            scroll: vec![ScrollPosition {
                path: "div.app>div.fs-list".into(),
                x: 0.0,
                y: 1200.0,
                ratio: 0.4,
                width: 900.0,
            }],
            fields: vec![FieldText {
                field: "input|Search|0".into(),
                value: "draft".into(),
            }],
        });
        assert!(session(vec![w.clone()]).validate().is_ok());
        let mut bad = w.clone();
        bad.view.as_mut().unwrap().scroll[0].ratio = 1.5;
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w.clone();
        bad.view.as_mut().unwrap().scroll[0].y = -1.0;
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w.clone();
        bad.view.as_mut().unwrap().fields[0].value = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(session(vec![bad])
            .validate()
            .unwrap_err()
            .contains("longer"));
        let mut bad = w.clone();
        bad.view.as_mut().unwrap().fields = (0..=MAX_VIEW_FIELDS)
            .map(|i| FieldText {
                field: format!("f{i}"),
                value: String::new(),
            })
            .collect();
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w;
        bad.view.as_mut().unwrap().fields = (0..4)
            .map(|i| FieldText {
                field: format!("f{i}"),
                value: "x".repeat(MAX_FIELD_BYTES - 16),
            })
            .collect();
        assert!(session(vec![bad])
            .validate()
            .unwrap_err()
            .contains("larger"));
        assert!(serde_json::from_value::<DesktopSession>(json!({
            "version": 1,
            "windows": [{"key": "a", "appId": "files", "state": "normal",
                         "view": {"fields": [{"field": "f", "value": "v", "secret": 1}]}}]
        }))
        .is_err());
        // Dialogs open in a window, with what was typed in them.
        let mut w = window("a");
        w.dialogs = vec![SavedDialog {
            name: "new-folder".into(),
            params: Some(json!({"folder": "/Projects"})),
            view: Some(WindowView {
                scroll: Vec::new(),
                fields: vec![FieldText {
                    field: "input|Folder name|0".into(),
                    value: "Reports".into(),
                }],
            }),
        }];
        assert!(session(vec![w.clone()]).validate().is_ok());
        let mut bad = w.clone();
        bad.dialogs[0].name = "new folder".into();
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w.clone();
        bad.dialogs[0].params = Some(json!({"x": "y".repeat(MAX_PARAMS_BYTES)}));
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w.clone();
        bad.dialogs = vec![bad.dialogs[0].clone(); MAX_DIALOGS + 1];
        assert!(session(vec![bad]).validate().is_err());
        let mut bad = w;
        bad.dialogs[0].view.as_mut().unwrap().fields[0].value = "x".repeat(MAX_FIELD_BYTES + 1);
        assert!(session(vec![bad]).validate().is_err());
        // Unknown fields are refused rather than stored.
        assert!(serde_json::from_value::<DesktopSession>(json!({
            "version": 1, "windows": [], "extra": true
        }))
        .is_err());
        assert!(serde_json::from_value::<DesktopSession>(json!({
            "version": 1,
            "windows": [{"key": "a", "appId": "files", "state": "fullscreen"}]
        }))
        .is_err());
    }
}
