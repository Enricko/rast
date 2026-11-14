//! Native Terminal: interactive PTY shells for RAST users.
//!
//! Unlike the Docker exec terminal (`crate::api::terminal`), this one runs the
//! approved shell directly on the server as the mapped unprivileged
//! `rast-u<userId>` identity, provisioned by the privileged native helper.
//! It does not require Docker, Xpra, a graphical session or the native Linux
//! apps feature: the helper only creates the identity/home and a transient
//! systemd unit running the per-session agent (`crate::terminal::agent`).
//!
//! * [`protocol`] — server ↔ agent framing and the one-time token handshake.
//! * [`agent`] — the per-user process that owns the PTY and the shell.
//! * [`manager`] — server-side session lifecycle, reconnect grace, limits.
//! * [`env`] — PATH/runtime variables composed from managed runtimes.

pub mod agent;
pub mod env;
pub mod manager;
pub mod protocol;

/// The stable application id used by the access-control system and the
/// frontend registry.
pub const APP_ID: &str = "terminal";

/// Maximum number of PTY sessions one account may have open.
pub const DEFAULT_MAX_SESSIONS: u32 = 8;
