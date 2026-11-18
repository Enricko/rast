//! RAST foundation: errors, configuration, database, events, authentication, access rules, jobs and File Station storage.

pub mod access;
pub mod appearance;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod jobs;
pub mod realtime;
pub mod secrets;
pub mod storage;
pub mod users;

/// Product name.
pub const NAME: &str = "RAST";
/// Semantic version of this build (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Product description used in UI/metadata.
pub const DESCRIPTION: &str = "Resource Administration & Storage Toolkit";
