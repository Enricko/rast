//! One HOME per user (ADR 0005): File Station requests of a one-HOME account
//! are answered by that account's own file agent ([`agent`]), which runs as
//! `rast-u<id>` with its root at the HOME. The API layer
//! (`crate::api::homefs`) keeps doing every RAST check and forwards.
//!
//! A user is in one-HOME mode when the HOME directory is owned by the uid of
//! their Linux identity; a HOME owned by the `rast` service is a legacy home,
//! served in-process as before. No database column records the mode.

pub mod agent;

/// Prefix of chunked upload ids that belong to a per-user agent (the root
/// agent uses `sys-`).
pub const UPLOAD_PREFIX: &str = "usr-";
/// Folder in HOME that holds the `~/Shared/<name>` links to shared folders.
pub const SHARED_DIR: &str = "Shared";
