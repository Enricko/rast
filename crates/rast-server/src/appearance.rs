//! Each account's appearance, as classic RAST's *Personalize* kept it: the colour
//! mode, the accent and the wallpaper. It lives on the server so the desktop looks
//! the same in every browser the account signs in from.
//!
//! * `GET /api/appearance`: the account's appearance (the defaults until it saves one).
//! * `PUT /api/appearance`: change the fields the body names; the rest stay.
//!
//! Both answer `{"appearance": {...}}`. An account sees and changes only its own.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::api::{ApiError, ApiResult};
use crate::{auth, db};

pub(crate) const THEMES: &[&str] = &["light", "dark", "system"];
pub(crate) const ACCENTS: &[&str] = &[
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
/// The wallpapers the desktop ships.
pub(crate) const WALLPAPERS: &[&str] = &[
    "alpine-dawn",
    "forest-mist",
    "coastal-calm",
    "desert-dusk",
    "mountain-valley",
];
/// A bundled picture, or one solid colour.
pub(crate) const KINDS: &[&str] = &["builtin", "color"];
pub(crate) const FITS: &[&str] = &["cover", "contain", "center"];
pub(crate) const POSITIONS: &[&str] = &[
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
/// The darkening drawn over the wallpaper, in percent.
pub(crate) const MAX_OVERLAY: i64 = 60;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Appearance {
    pub(crate) theme: String,
    pub(crate) accent: String,
    pub(crate) wallpaper: Wallpaper,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Wallpaper {
    pub(crate) kind: String,
    pub(crate) builtin_id: String,
    /// `#RRGGBB`: the solid colour, and what shows around a contained picture.
    pub(crate) color: String,
    pub(crate) fit: String,
    pub(crate) position: String,
    pub(crate) overlay: i64,
}

impl Default for Appearance {
    /// Classic RAST's first look: light, Ocean Blue, Alpine Dawn.
    fn default() -> Self {
        Self {
            theme: "light".to_owned(),
            accent: "ocean".to_owned(),
            wallpaper: Wallpaper {
                kind: "builtin".to_owned(),
                builtin_id: "alpine-dawn".to_owned(),
                color: "#1B2430".to_owned(),
                fit: "cover".to_owned(),
                position: "center".to_owned(),
                overlay: 20,
            },
        }
    }
}

/// A `PUT` body: only the fields to change.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Change {
    theme: Option<String>,
    accent: Option<String>,
    wallpaper: Option<WallpaperChange>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WallpaperChange {
    kind: Option<String>,
    builtin_id: Option<String>,
    color: Option<String>,
    fit: Option<String>,
    position: Option<String>,
    overlay: Option<i64>,
}

/// `value` when it is one of `allowed`, else a 400 naming what was wrong.
fn one_of(value: String, allowed: &[&str], what: &str) -> ApiResult<String> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(ApiError::bad_request(format!(
            "Choose a {what}: {}.",
            allowed.join(", ")
        )))
    }
}

/// `#RRGGBB`, upper-cased.
fn color(value: &str) -> ApiResult<String> {
    let hex = value.strip_prefix('#').unwrap_or("");
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(format!("#{}", hex.to_ascii_uppercase()))
    } else {
        Err(ApiError::bad_request(
            "Pick a solid colour in the format #RRGGBB.",
        ))
    }
}

impl Appearance {
    /// Apply `change`, checking every field it names; nothing changes on an error.
    pub(crate) fn apply(&self, change: Change) -> ApiResult<Self> {
        let mut next = self.clone();
        if let Some(theme) = change.theme {
            next.theme = one_of(theme, THEMES, "colour mode")?;
        }
        if let Some(accent) = change.accent {
            next.accent = one_of(accent, ACCENTS, "colour")?;
        }
        if let Some(wallpaper) = change.wallpaper {
            let to = &mut next.wallpaper;
            if let Some(kind) = wallpaper.kind {
                to.kind = one_of(kind, KINDS, "wallpaper kind")?;
            }
            if let Some(id) = wallpaper.builtin_id {
                to.builtin_id = one_of(id, WALLPAPERS, "wallpaper")?;
            }
            if let Some(value) = wallpaper.color {
                to.color = color(&value)?;
            }
            if let Some(fit) = wallpaper.fit {
                to.fit = one_of(fit, FITS, "picture fit")?;
            }
            if let Some(position) = wallpaper.position {
                to.position = one_of(position, POSITIONS, "picture position")?;
            }
            if let Some(overlay) = wallpaper.overlay {
                if !(0..=MAX_OVERLAY).contains(&overlay) {
                    return Err(ApiError::bad_request(
                        "The dark overlay must be between 0% and 60%.",
                    ));
                }
                to.overlay = overlay;
            }
        }
        Ok(next)
    }

    /// A stored row read back, with anything no longer offered (a wallpaper a later
    /// version dropped) replaced by its default.
    pub(crate) fn sanitized(mut self) -> Self {
        let fallback = Self::default();
        let keep = |value: &mut String, allowed: &[&str], default: &str| {
            if !allowed.contains(&value.as_str()) {
                *value = default.to_owned();
            }
        };
        keep(&mut self.theme, THEMES, &fallback.theme);
        keep(&mut self.accent, ACCENTS, &fallback.accent);
        let (to, default) = (&mut self.wallpaper, &fallback.wallpaper);
        keep(&mut to.kind, KINDS, &default.kind);
        keep(&mut to.builtin_id, WALLPAPERS, &default.builtin_id);
        keep(&mut to.fit, FITS, &default.fit);
        keep(&mut to.position, POSITIONS, &default.position);
        if color(&to.color).is_err() {
            to.color.clone_from(&default.color);
        }
        to.overlay = to.overlay.clamp(0, MAX_OVERLAY);
        self
    }
}

pub(crate) async fn mine(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let signed = auth::require(&state, &headers).await?;
    let appearance = db::appearance_of(&state.pool, signed.user.id)
        .await?
        .unwrap_or_default();
    Ok(Json(json!({ "appearance": appearance })))
}

pub(crate) async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(change): Json<Change>,
) -> ApiResult<Json<Value>> {
    let signed = auth::require(&state, &headers).await?;
    let appearance = db::appearance_of(&state.pool, signed.user.id)
        .await?
        .unwrap_or_default()
        .apply(change)?;
    db::save_appearance(&state.pool, signed.user.id, &appearance).await?;
    Ok(Json(json!({ "appearance": appearance })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(body: serde_json::Value) -> Change {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn a_change_touches_only_the_fields_it_names() {
        let start = Appearance::default();
        let next = start
            .apply(change(json!({
                "accent": "plum",
                "wallpaper": {"fit": "contain", "position": "top-left", "overlay": 45}
            })))
            .unwrap();
        assert_eq!(next.theme, "light");
        assert_eq!(next.accent, "plum");
        assert_eq!(next.wallpaper.builtin_id, "alpine-dawn");
        assert_eq!(
            (
                next.wallpaper.fit.as_str(),
                next.wallpaper.position.as_str(),
                next.wallpaper.overlay
            ),
            ("contain", "top-left", 45)
        );
    }

    #[test]
    fn unknown_values_are_refused_and_change_nothing() {
        let start = Appearance::default();
        for bad in [
            json!({"theme": "sepia"}),
            json!({"accent": "neon"}),
            json!({"wallpaper": {"builtinId": "../../etc/passwd"}}),
            json!({"wallpaper": {"kind": "upload"}}),
            json!({"wallpaper": {"color": "red"}}),
            json!({"wallpaper": {"color": "#12345"}}),
            json!({"wallpaper": {"overlay": 61}}),
            json!({"wallpaper": {"overlay": -1}}),
            json!({"accent": "plum", "wallpaper": {"fit": "stretch"}}),
        ] {
            assert!(start.apply(change(bad.clone())).is_err(), "{bad}");
        }
        assert!(
            serde_json::from_value::<Change>(json!({"widgets": []})).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn colours_are_stored_upper_case() {
        let next = Appearance::default()
            .apply(change(
                json!({"wallpaper": {"kind": "color", "color": "#2e4756"}}),
            ))
            .unwrap();
        assert_eq!(next.wallpaper.color, "#2E4756");
    }

    #[test]
    fn a_stored_value_no_longer_offered_reads_as_the_default() {
        let defaults = Appearance::default();
        let stored = Appearance {
            accent: "neon".to_owned(),
            wallpaper: Wallpaper {
                builtin_id: "retired".to_owned(),
                overlay: 99,
                ..defaults.wallpaper
            },
            ..Appearance::default()
        };
        let read = stored.sanitized();
        assert_eq!(read.accent, "ocean");
        assert_eq!(read.wallpaper.builtin_id, "alpine-dawn");
        assert_eq!(read.wallpaper.overlay, MAX_OVERLAY);
    }
}
