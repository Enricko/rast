//! Restore must refuse while a server holds the instance lock.
//!
//! This test lives in its own integration-test binary on purpose. The instance
//! lock is a `flock` on `<data_dir>/rast.lock`; a flock belongs to the open
//! file description, so a child process that sits between `fork` and `exec`
//! (spawned by any other test in the same process) briefly holds a copy of the
//! descriptor and keeps the lock alive after `drop(server)`. With this binary
//! containing only this one test, nothing else in the process forks while the
//! lock is held, which makes the final assertions deterministic.
//!
//! Run with:
//!     cargo test --test instance_lock
#![cfg(target_os = "linux")]

use std::path::PathBuf;

use rast::{
    backup::{create, restore_database},
    config::{Config, FileConfig},
    db,
};

#[tokio::test]
async fn restore_refuses_while_a_server_holds_the_instance_lock() {
    // Regression: the guard used `systemctl is-active rast.service`, which
    // missed servers started any other way and blocked unrelated data
    // directories. A running server holds `<data_dir>/rast.lock`.
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_parts(
        None,
        FileConfig {
            data_dir: Some(dir.path().join("data")),
            storage_dir: Some(dir.path().join("storage")),
            ..Default::default()
        },
        |_| None,
    )
    .unwrap();
    cfg.ensure_dirs().unwrap();
    let pool = db::init(&cfg).await.unwrap();
    let meta = create(&cfg, Some(&pool), &["db".into()], "lock test", None, None)
        .await
        .unwrap();
    let archive = PathBuf::from(&meta.path);

    let server = db::InstanceLock::try_acquire(&cfg).unwrap().expect("lock");
    let err = restore_database(&cfg, &archive).await.unwrap_err();
    assert!(err.to_string().contains("server is running"), "{err}");
    // A second holder is refused while the first is alive.
    assert!(db::InstanceLock::try_acquire(&cfg).unwrap().is_none());

    drop(server);
    let restored = restore_database(&cfg, &archive).await.unwrap();
    assert_eq!(restored["restored"], true);
    // The lock is released after the restore finished.
    assert!(db::InstanceLock::try_acquire(&cfg).unwrap().is_some());
}
