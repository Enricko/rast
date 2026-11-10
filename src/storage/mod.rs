//! Storage subsystem: safe path resolution, quota accounting, recycle bin and
//! filesystem operations.
//!
//! OWNER: teammate `backend-storage`. This file is a compiling placeholder that
//! defines the interface the rest of the server relies on. Replace it with the
//! real implementation, keeping the constructor signature.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{config::Config, error::AppResult};

#[derive(Debug)]
pub struct Storage {
    pub storage_root: PathBuf,
    pub homes_root: PathBuf,
    pub shared_root: PathBuf,
    pub trash_root: PathBuf,
    pub max_upload_bytes: u64,
}

impl Storage {
    pub fn new(cfg: &Config) -> AppResult<Self> {
        Ok(Self {
            storage_root: cfg.storage_dir.clone(),
            homes_root: cfg.homes_dir(),
            shared_root: cfg.shared_dir(),
            trash_root: cfg.trash_dir(),
            max_upload_bytes: cfg.max_upload_bytes,
        })
    }

    pub fn as_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}
