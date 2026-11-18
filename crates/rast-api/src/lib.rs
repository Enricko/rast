//! Shared API types for RAST.
//!
//! Pure data: no filesystem, process or async code, and no dependency on any
//! other workspace crate, so it also builds for `wasm32-unknown-unknown`.
//!
//! TypeScript definitions are generated into `frontend/src/generated/` by
//! `ts-rs` when the tests run (`cargo test -p rast-api`); the output directory
//! is set by `TS_RS_EXPORT_DIR` in `.cargo/config.toml`. Never edit the
//! generated files by hand.

pub mod files;

pub use files::{BreadcrumbDto, EntryDto, ListingDto, QuotaDto};
