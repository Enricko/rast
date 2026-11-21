//! Small line icons drawn in their element's colour: `icon(&glyphs::SUN, 14.0).color(ink)`.
//! The desktop's own icons (`desktop::icon`) carry fixed colours; these follow the accent and
//! the colour mode, as classic RAST's inline icons did.

use rune::Icon;

/// The RAST mark on the Start button.
pub(super) static LOGO: Icon = Icon {
    name: "rast-logo",
    view: 24.0,
    body: r#"<path d="M12 2.6 20.4 7v10L12 21.4 3.6 17V7z" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="M12 7.2 15.8 9.4v4.4L12 16l-3.8-2.2V9.4z" fill="currentColor"/>"#,
};

pub(super) static SUN: Icon = Icon {
    name: "rast-sun",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="12" cy="12" r="4.2"/><path d="M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M18.5 5.5l-1.7 1.7M7.2 16.8l-1.7 1.7"/></g>"#,
};

pub(super) static MOON: Icon = Icon {
    name: "rast-moon",
    view: 24.0,
    body: r#"<path d="M20.2 14.6A8.4 8.4 0 0 1 9.4 3.8a8.4 8.4 0 1 0 10.8 10.8z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>"#,
};

pub(super) static MONITOR: Icon = Icon {
    name: "rast-monitor",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12.5" rx="2"/><path d="M8.5 20h7M12 16.5V20"/></g>"#,
};

pub(super) static INFO: Icon = Icon {
    name: "rast-info",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M12 11v5m0-8h.01"/></g>"#,
};

/// A picture: *Reset to default* (the default wallpaper) and *Personalize…*.
pub(super) static PICTURE: Icon = Icon {
    name: "rast-picture",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="m3 16 5-5 4 4 3-3 6 6"/><circle cx="16" cy="8.5" r="1"/></g>"#,
};

/// A turn back: *Discard changes* and *Try again*.
pub(super) static UNDO: Icon = Icon {
    name: "rast-undo",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 12a7.5 7.5 0 1 0 2.2-5.3L4.5 9"/><path d="M4.5 4.5V9H9"/></g>"#,
};

pub(super) static CHECK: Icon = Icon {
    name: "rast-check",
    view: 24.0,
    body: r#"<path d="m5 12.5 4.5 4.5L19 7.5" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) static CLOSE: Icon = Icon {
    name: "rast-close",
    view: 24.0,
    body: r#"<path d="M6.5 6.5l11 11M17.5 6.5l-11 11" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

/// *Show desktop*: every window out of the way.
pub(super) static DESKTOP: Icon = Icon {
    name: "rast-desktop",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="14" rx="2"/><path d="M3 14.5h18"/></g>"#,
};

pub(super) static GEAR: Icon = Icon {
    name: "rast-gear",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 2.8v2.6M12 18.6v2.6M2.8 12h2.6M18.6 12h2.6M5.5 5.5l1.8 1.8M16.7 16.7l1.8 1.8M18.5 5.5l-1.8 1.8M7.3 16.7l-1.8 1.8"/></g>"#,
};

pub(super) static KEYBOARD: Icon = Icon {
    name: "rast-keyboard",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="6" width="19" height="12" rx="2"/><path d="M6.5 10h.01M10 10h.01M14 10h.01M17.5 10h.01M8 14h8"/></g>"#,
};

pub(super) static SIGN_OUT: Icon = Icon {
    name: "rast-sign-out",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4.5h3.5a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H14"/><path d="M10 16.5 5.5 12 10 7.5M5.5 12H15"/></g>"#,
};

// Control Panel's sections and actions, drawn as classic RAST's icon set drew them.

pub(super) static USER: Icon = Icon {
    name: "rast-user",
    view: 24.0,
    body: r#"<circle cx="12" cy="8.4" r="3.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.8 20.2c.9-3.8 3.7-5.8 7.2-5.8s6.3 2 7.2 5.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

pub(super) static USERS: Icon = Icon {
    name: "rast-users",
    view: 24.0,
    body: r#"<circle cx="9.2" cy="8.6" r="3.2" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3.4 19.4c.7-3.2 3-4.9 5.8-4.9s5.1 1.7 5.8 4.9" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M15.4 5.9a3 3 0 0 1 0 5.6M17 14.9c2 .5 3.3 2 3.6 4.2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
};

pub(super) static WALLPAPER: Icon = Icon {
    name: "rast-wallpaper",
    view: 24.0,
    body: r#"<rect x="3.6" y="4.8" width="16.8" height="14.4" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="9" cy="9.6" r="1.7" fill="currentColor"/><path d="m4.8 17.2 4.8-4.9 3.2 3 2.5-2.6 3.9 4.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>"#,
};

/// Shared folders, and the Groups view of *Users & Groups*.
pub(super) static SHARE: Icon = Icon {
    name: "rast-share",
    view: 24.0,
    body: r#"<circle cx="7.2" cy="12" r="2.7" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="17" cy="6.2" r="2.4" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="17" cy="17.8" r="2.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m9.6 10.8 5-3.2M9.6 13.2l5 3.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
};

pub(super) static DISK: Icon = Icon {
    name: "rast-disk",
    view: 24.0,
    body: r#"<ellipse cx="12" cy="6.4" rx="7.4" ry="2.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.6 6.4v11.2c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9V6.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M4.6 12c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9" fill="none" stroke="currentColor" stroke-width="1.4"/>"#,
};

pub(super) static ACTIVITY: Icon = Icon {
    name: "rast-activity",
    view: 24.0,
    body: r#"<path d="M3.4 12.4h4l2.6-6.8 4.1 13 2.5-6.2h4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) static SETTINGS: Icon = Icon {
    name: "rast-settings",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="3.1" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M12 3.4v2.4M12 18.2v2.4M3.4 12h2.4M18.2 12h2.4M5.9 5.9l1.7 1.7M16.4 16.4l1.7 1.7M18.1 5.9l-1.7 1.7M7.6 16.4l-1.7 1.7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

/// The connection to the RAST server, in the desktop app without a server session.
pub(super) static NETWORK: Icon = Icon {
    name: "rast-network",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="8.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3.8 12h16.4M12 3.6c2.4 2.4 3.6 5.3 3.6 8.4s-1.2 6-3.6 8.4c-2.4-2.4-3.6-5.3-3.6-8.4S9.6 6 12 3.6z" fill="none" stroke="currentColor" stroke-width="1.4"/>"#,
};

pub(super) static PLUS: Icon = Icon {
    name: "rast-plus",
    view: 24.0,
    body: r#"<path d="M12 5.2v13.6M5.2 12h13.6" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/>"#,
};

// The Users table: its search field, row actions and their menu.

pub(super) static SEARCH: Icon = Icon {
    name: "rast-search",
    view: 24.0,
    body: r#"<circle cx="10.8" cy="10.8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="m15.4 15.4 4.4 4.4" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>"#,
};

/// A row's actions: three dots, one above the other.
pub(super) static MORE: Icon = Icon {
    name: "rast-more",
    view: 24.0,
    body: r#"<circle cx="12" cy="5.2" r="1.5" fill="currentColor"/><circle cx="12" cy="12" r="1.5" fill="currentColor"/><circle cx="12" cy="18.8" r="1.5" fill="currentColor"/>"#,
};

pub(super) static SHIELD_ALERT: Icon = Icon {
    name: "rast-shield-alert",
    view: 24.0,
    body: r#"<path d="M12 3.2 19 6v5.6c0 4.3-2.9 7.4-7 9.2-4.1-1.8-7-4.9-7-9.2V6z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="M12 7.8v4.6M12 16.4v.2" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/>"#,
};

pub(super) static EDIT: Icon = Icon {
    name: "rast-edit",
    view: 24.0,
    body: r#"<path d="M4.6 19.4h3.1L19 8.1a2 2 0 0 0 0-2.8l-.3-.3a2 2 0 0 0-2.8 0L4.6 16.3z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="m13.9 7.2 2.9 2.9" stroke="currentColor" stroke-width="1.6"/>"#,
};

pub(super) static KEY: Icon = Icon {
    name: "rast-key",
    view: 24.0,
    body: r#"<circle cx="8.4" cy="15.6" r="4.1" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m11.3 12.7 8.2-8.2M16.6 7.4l2.2 2.2M14.1 9.9l2.2 2.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) static SESSIONS: Icon = Icon {
    name: "rast-sessions",
    view: 24.0,
    body: r#"<rect x="3.6" y="5.2" width="12.8" height="9.2" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M7.6 18.6h12.8V9.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>"#,
};

pub(super) static POWER: Icon = Icon {
    name: "rast-power",
    view: 24.0,
    body: r#"<path d="M12 3.6v8.2" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/><path d="M6.6 6.6a7.6 7.6 0 1 0 10.8 0" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>"#,
};

/// *Unlock account*: classic RAST's lock with its shackle open.
pub(super) static UNLOCK: Icon = Icon {
    name: "rast-unlock",
    view: 24.0,
    body: r#"<rect x="5.4" y="10.4" width="13.2" height="9.4" rx="1.8" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8.4 10.4V7.8a3.6 3.6 0 0 1 7-1.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

/// A select's arrow, as classic RAST's fields drew it: a small solid triangle.
pub(super) static CARET_DOWN: Icon = Icon {
    name: "rast-caret-down",
    view: 10.0,
    body: r#"<path d="M0 2.5h10L5 7.5z" fill="currentColor"/>"#,
};
