//! Desktop session API: the windows an account has open on its desktop.
//!
//! The browser saves the session as windows open, move and close, and loads
//! it when the desktop starts, so a reload, another tab or another device
//! shows the same windows: which apps, where, how big, minimized, maximized
//! or snapped, their stacking order and where each app was (`params`, which
//! the app reads when it opens). Each account only ever reads and writes its
//! own session.
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
