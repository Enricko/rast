//! Per-user and root agents: the File Station file agent, the PTY agent and the terminal manager.

pub mod fileagent;
pub mod homefs;
pub mod rootfs;
pub mod terminal;

// Modules of lower crates keep their `crate::<module>` paths.
use rast_apps::packages;
use rast_base::{access, auth, config, db, error, events, realtime, storage, users};
use rast_helper::native;

#[cfg(test)]
mod test_support {
    /// A temp directory that is safe to hold a unix socket: `sun_path` is
    /// limited to about 108 bytes, so a long `TMPDIR` falls back to `/tmp`.
    pub fn socket_tempdir() -> tempfile::TempDir {
        // Room for the longest socket file name used in the tests.
        const MAX_DIR_LEN: usize = 70;
        let tmp = std::env::temp_dir();
        let base = if tmp.as_os_str().len() <= MAX_DIR_LEN - 16 {
            tmp
        } else {
            std::path::PathBuf::from("/tmp")
        };
        tempfile::Builder::new()
            .prefix("rast-")
            .tempdir_in(base)
            .expect("temp dir")
    }
}
