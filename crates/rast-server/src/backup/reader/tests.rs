use std::collections::HashMap;
use std::io::BufReader as StdBufReader;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use sqlx::SqlitePool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::*;
use crate::backup::restore_files::Inputs;
use crate::backup::{self, BackupEnv, BackupMeta, Manifest};
use crate::config::{Config, FileConfig};
use crate::db;
use crate::native::acl::{self, Acl, Entry, Tag};
use crate::native::backup_reader::{run, Listen, OwnerRules, ReaderOptions};
use crate::native::restore_files::{Facts, HostAccounts, OwnerLog, Owners};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The owner and group a new file of this test gets.
fn me() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    static ME: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *ME.get_or_init(|| {
        let file = tempfile::NamedTempFile::new().unwrap();
        let meta = file.as_file().metadata().unwrap();
        (meta.uid(), meta.gid())
    })
}

fn euid() -> u32 {
    me().0
}

fn egid() -> u32 {
    me().1
}

/// A temp folder short enough for unix socket paths.
fn socket_dir() -> tempfile::TempDir {
    let tmp = std::env::temp_dir();
    let base = if tmp.as_os_str().len() <= 60 {
        tmp
    } else {
        PathBuf::from("/tmp")
    };
    tempfile::Builder::new()
        .prefix("rast-")
        .tempdir_in(base)
        .unwrap()
}

struct Fx {
    dir: tempfile::TempDir,
    sockets: tempfile::TempDir,
    cfg: Config,
    pool: SqlitePool,
}

async fn fixture(pilot: Option<&[&str]>) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_parts(
        None,
        FileConfig {
            data_dir: Some(dir.path().join("data")),
            storage_dir: Some(dir.path().join("storage")),
            native_enabled: Some(false),
            native_pilot_prefixes: pilot
                .map(|prefixes| prefixes.iter().map(|prefix| prefix.to_string()).collect()),
            ..Default::default()
        },
        |_| None,
    )
    .unwrap();
    cfg.ensure_dirs().unwrap();
    std::fs::create_dir_all(cfg.shared_dir()).unwrap();
    let pool = db::init(&cfg).await.unwrap();
    for name in ["alice", "t-carol"] {
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at, updated_at)
             VALUES (?, 'x', 'user', 1, 1)",
        )
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (id, name, container) in [("s1", "Team", "team"), ("s2", "t-lab", "t-lab")] {
        sqlx::query(
            "INSERT INTO shares (id, name, container_path, created_at, updated_at)
             VALUES (?, ?, ?, 1, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(container)
        .execute(&pool)
        .await
        .unwrap();
    }
    Fx {
        dir,
        sockets: socket_dir(),
        cfg,
        pool,
    }
}

type Calls = Arc<Mutex<Vec<String>>>;

/// A helper that answers `backup_reader_start` with `reply` (or refuses).
fn fake_helper(socket: &Path, reply: Result<Value, String>) -> Calls {
    let calls: Calls = Arc::default();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let log = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(read)
                .read_line(&mut line)
                .await
                .is_err()
            {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            log.lock()
                .unwrap()
                .push(request["op"].as_str().unwrap().to_string());
            let answer = match &reply {
                Ok(data) => json!({"id": request["id"], "ok": true, "data": data}),
                Err(message) => json!({"id": request["id"], "ok": false, "error": {
                    "code": "unavailable", "message": message}}),
            };
            let mut bytes = serde_json::to_vec(&answer).unwrap();
            bytes.push(b'\n');
            let _ = write.write_all(&bytes).await;
        }
    });
    calls
}

/// The real reader, in a thread, with rules that treat the test account
/// as an identity and as root.
fn real_reader(fx: &Fx, rules: OwnerRules) -> PathBuf {
    let socket = fx.sockets.path().join("reader.sock");
    let options = ReaderOptions {
        homes: fx.cfg.homes_dir(),
        shared: fx.cfg.shared_dir(),
        rules,
        token: TOKEN.into(),
        peer_uid: Some(euid()),
        idle: Duration::from_secs(30),
        listen: Listen::Path(socket.clone()),
    };
    std::thread::spawn(move || run(options));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    socket
}

fn my_rules() -> OwnerRules {
    OwnerRules {
        home_owners: euid()..=euid(),
        container_owner: euid(),
        container_groups: egid()..=egid(),
        service: None,
    }
}

fn helper_for(fx: &Fx, reader_socket: &Path) -> (BackupEnv, Calls) {
    let helper = fx.sockets.path().join("helper.sock");
    let calls = fake_helper(
        &helper,
        Ok(json!({"unit": "rast-backup-reader", "socket": reader_socket, "token": TOKEN})),
    );
    (one_home_env(Some(NativeClient::new(helper))), calls)
}

/// The service is another uid, so every folder the test account owns is
/// in one-HOME mode, and migrated containers belong to the test account.
fn one_home_env(native: Option<NativeClient>) -> BackupEnv {
    BackupEnv {
        service_uid: Some(euid().wrapping_add(1)),
        container_owner_uid: euid(),
        native,
        min_free_bytes: 0,
    }
}

async fn backup_users(fx: &Fx, env: &BackupEnv) -> BackupMeta {
    backup::create_with(
        &fx.cfg,
        Some(&fx.pool),
        &["users".into()],
        "",
        None,
        None,
        env,
    )
    .await
    .unwrap()
}

#[derive(Debug)]
struct Seen {
    kind: tar::EntryType,
    /// Absent for entries the service wrote (no owner fields).
    uid: Option<u64>,
    gid: Option<u64>,
    mode: u32,
    link: Option<String>,
    pax: HashMap<String, String>,
    data: Vec<u8>,
}

/// The PAX records of every entry (short paths only), read raw: tar-rs
/// splits PAX data at newlines, so it cannot parse ACL values in the GNU
/// tar form (one entry per line), which GNU tar itself writes.
fn raw_pax(path: &str) -> HashMap<String, HashMap<String, String>> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = tar::Archive::new(GzDecoder::new(StdBufReader::new(file)));
    let mut found = HashMap::new();
    let mut pending: Option<HashMap<String, String>> = None;
    for entry in archive.entries().unwrap().raw(true) {
        let mut entry = entry.unwrap();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if entry.header().entry_type() == tar::EntryType::XHeader {
            let mut records = HashMap::new();
            let mut rest = &data[..];
            while !rest.is_empty() {
                let space = rest.iter().position(|byte| *byte == b' ').unwrap();
                let length: usize = std::str::from_utf8(&rest[..space])
                    .unwrap()
                    .parse()
                    .unwrap();
                let record = std::str::from_utf8(&rest[space + 1..length - 1]).unwrap();
                let (key, value) = record.split_once('=').unwrap();
                records.insert(key.to_string(), value.to_string());
                rest = &rest[length..];
            }
            pending = Some(records);
            continue;
        }
        if let Some(records) = pending.take() {
            let path = String::from_utf8_lossy(&entry.path_bytes())
                .trim_end_matches('/')
                .to_string();
            found.insert(path, records);
        }
    }
    found
}

/// Every entry of a backup archive, read with tar-rs (as `verify` does).
fn read_archive(path: &str) -> (HashMap<String, Seen>, Manifest) {
    let mut pax_by_path = raw_pax(path);
    let file = std::fs::File::open(path).unwrap();
    let mut archive = tar::Archive::new(GzDecoder::new(StdBufReader::new(file)));
    let mut seen = HashMap::new();
    let mut manifest = None;
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry
            .path()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let pax = pax_by_path.remove(&path).unwrap_or_default();
        let header = entry.header().clone();
        let link = entry
            .link_name()
            .unwrap()
            .map(|link| link.to_string_lossy().into_owned());
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if path == "manifest.json" {
            manifest = Some(serde_json::from_slice(&data).unwrap());
        }
        seen.insert(
            path,
            Seen {
                kind: header.entry_type(),
                uid: header.uid().ok(),
                gid: header.gid().ok(),
                mode: header.mode().unwrap(),
                link,
                pax,
                data,
            },
        );
    }
    (seen, manifest.expect("manifest.json"))
}

fn acl_with_user(uid: u32) -> Acl {
    Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 7,
        },
        Entry {
            tag: Tag::User(uid),
            perm: 5,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 5,
        },
        Entry {
            tag: Tag::Mask,
            perm: 5,
        },
        Entry {
            tag: Tag::Other,
            perm: 0,
        },
    ])
}

#[tokio::test]
async fn one_home_homes_and_migrated_shares_keep_owners_modes_and_acls() {
    let fx = fixture(None).await;
    let home = fx.cfg.home_for("alice");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::create_dir_all(home.join(".cache/big")).unwrap();
    std::fs::write(home.join(".cache/big/blob"), b"cache").unwrap();
    std::fs::write(home.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::set_permissions(
        home.join("docs/a.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::hard_link(home.join("docs/a.txt"), home.join("docs/b.txt")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", home.join("passwd")).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let team = fx.cfg.shared_dir().join("team");
    std::fs::create_dir_all(team.join(".Trash-200007/files")).unwrap();
    std::fs::write(team.join(".Trash-200007/files/old"), b"old").unwrap();
    std::fs::write(team.join("plan.txt"), b"plan").unwrap();
    std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o2770)).unwrap();
    let acls = {
        let docs = std::fs::File::open(home.join("docs")).unwrap();
        acl::write(
            docs.as_raw_fd(),
            acl::ACCESS_XATTR,
            &acl_with_user(4_000_001),
        )
        .is_ok()
            && acl::write(
                docs.as_raw_fd(),
                acl::DEFAULT_XATTR,
                &acl_with_user(4_000_002),
            )
            .is_ok()
    };
    let reader = real_reader(&fx, my_rules());
    let (env, calls) = helper_for(&fx, &reader);

    let meta = backup_users(&fx, &env).await;
    assert!(meta.warnings.is_empty(), "{:?}", meta.warnings);
    assert_eq!(*calls.lock().unwrap(), ["backup_reader_start"]);

    let (seen, manifest) = read_archive(&meta.path);
    assert_eq!(
        manifest.preserved_trees,
        ["users/homes/alice", "users/shared/team"]
    );
    let root = &seen["users/homes/alice"];
    assert_eq!(root.kind, tar::EntryType::Directory);
    assert_eq!(
        (root.uid, root.gid),
        (Some(euid() as u64), Some(egid() as u64))
    );
    assert_eq!(root.mode & 0o7777, 0o700);
    assert_eq!(seen["users/shared/team"].mode & 0o7777, 0o2770);

    // One of the two names holds the data, the other links to it inside
    // the same tree.
    let (stored, linked) = match seen["users/homes/alice/docs/a.txt"].kind {
        tar::EntryType::Regular => (
            "users/homes/alice/docs/a.txt",
            "users/homes/alice/docs/b.txt",
        ),
        _ => (
            "users/homes/alice/docs/b.txt",
            "users/homes/alice/docs/a.txt",
        ),
    };
    let file = &seen[stored];
    assert_eq!(file.data, b"alpha");
    assert_eq!(file.mode & 0o7777, 0o640);
    assert_eq!(
        (file.uid, file.gid),
        (Some(euid() as u64), Some(egid() as u64))
    );
    assert_eq!(seen[linked].kind, tar::EntryType::Link);
    assert_eq!(seen[linked].link.as_deref(), Some(stored));
    let link = &seen["users/homes/alice/passwd"];
    assert_eq!(link.kind, tar::EntryType::Symlink);
    assert_eq!(link.link.as_deref(), Some("/etc/passwd"));
    assert!(link.data.is_empty(), "a link is never followed");
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"plan");
    assert!(!seen
        .keys()
        .any(|path| path.contains(".cache") || path.contains(".Trash")));
    if acls {
        let docs = &seen["users/homes/alice/docs"];
        assert_eq!(
            docs.pax.get("SCHILY.acl.access").map(String::as_str),
            Some("user::rwx\nuser:4000001:r-x\ngroup::r-x\nmask::r-x\nother::---")
        );
        assert_eq!(
            docs.pax.get("SCHILY.acl.default").map(String::as_str),
            Some("user::rwx\nuser:4000002:r-x\ngroup::r-x\nmask::r-x\nother::---")
        );
    }

    let preserved: Vec<&str> = manifest
        .entries
        .iter()
        .filter(|entry| entry.owners_preserved)
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(preserved, [stored, "users/shared/team/plan.txt"]);
    let verified = backup::verify(Path::new(&meta.path)).await.unwrap();
    assert_eq!(verified["ok"], true, "{verified}");
    assert_eq!(verified["manifest"]["ownersPreserved"], 2);
    assert_eq!(
        verified["manifest"]["preservedTrees"],
        json!(["users/homes/alice", "users/shared/team"])
    );
    drop(fx.dir);
}

#[tokio::test]
async fn legacy_folders_are_read_by_the_service_without_the_helper() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    std::fs::write(fx.cfg.home_for("alice").join("a.txt"), b"a").unwrap();
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    let (mut env, calls) = helper_for(&fx, Path::new("/nonexistent/reader.sock"));
    // The service owns every folder (legacy); containers of root only are
    // migrated.
    env.service_uid = Some(euid());
    env.container_owner_uid = 0;

    let meta = backup_users(&fx, &env).await;
    assert!(meta.warnings.is_empty(), "{:?}", meta.warnings);
    assert!(calls.lock().unwrap().is_empty());
    let (seen, manifest) = read_archive(&meta.path);
    assert!(manifest.preserved_trees.is_empty());
    assert!(manifest.entries.iter().all(|entry| !entry.owners_preserved));
    assert_eq!(seen["users/homes/alice/a.txt"].data, b"a");
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"p");
    drop(fx.dir);
}

#[tokio::test]
async fn the_native_pilot_and_unknown_folders_keep_homes_away_from_the_reader() {
    let fx = fixture(Some(&["t-"])).await;
    for name in ["alice", "stranger"] {
        std::fs::create_dir_all(fx.cfg.home_for(name)).unwrap();
        std::fs::write(fx.cfg.home_for(name).join("a.txt"), b"a").unwrap();
    }
    // `Team` is outside the pilot: the service reads it as before.
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    let (env, calls) = helper_for(&fx, Path::new("/nonexistent/reader.sock"));

    let meta = backup_users(&fx, &env).await;
    assert!(calls.lock().unwrap().is_empty(), "the reader is not needed");
    assert_eq!(meta.warnings.len(), 2, "{:?}", meta.warnings);
    let warning = |name: &str| {
        meta.warnings
            .iter()
            .find(|warning| warning.starts_with(&format!("users/homes/{name}: skipped")))
            .unwrap_or_else(|| panic!("no warning for {name}: {:?}", meta.warnings))
    };
    assert!(warning("alice").contains("one-HOME mode"));
    assert!(warning("alice").contains("outside the native pilot"));
    assert!(warning("stranger").contains("no RAST account"));
    let (seen, manifest) = read_archive(&meta.path);
    assert!(manifest.preserved_trees.is_empty());
    assert!(!seen.keys().any(|path| path.starts_with("users/homes/")));
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"p");
    drop(fx.dir);
}

#[tokio::test]
async fn native_off_skips_one_home_homes_with_a_warning() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    let meta = backup_users(&fx, &one_home_env(None)).await;
    assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
    assert!(meta.warnings[0].starts_with("users/homes/alice: skipped"));
    assert!(meta.warnings[0].contains("native Linux apps are off"));
    let (seen, _) = read_archive(&meta.path);
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"p");
    drop(fx.dir);
}

#[tokio::test]
async fn without_the_reader_homes_are_skipped_and_shares_fall_back() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    std::fs::write(fx.cfg.home_for("alice").join("a.txt"), b"a").unwrap();
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    let helper = fx.sockets.path().join("helper.sock");
    let calls = fake_helper(&helper, Err("systemd-run failed".into()));
    let env = one_home_env(Some(NativeClient::new(helper)));

    let meta = backup_users(&fx, &env).await;
    assert_eq!(*calls.lock().unwrap(), ["backup_reader_start"]);
    assert_eq!(meta.warnings.len(), 2, "{:?}", meta.warnings);
    assert!(meta.warnings[0].starts_with("users/homes/alice: skipped"));
    assert!(meta.warnings[0].contains("backup reader is unavailable"));
    assert!(meta.warnings[1].starts_with("users/shared/team: read by the RAST service"));
    let (seen, manifest) = read_archive(&meta.path);
    assert!(manifest.preserved_trees.is_empty());
    assert!(!seen.contains_key("users/homes/alice/a.txt"));
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"p");
    drop(fx.dir);
}

#[tokio::test]
async fn a_tree_the_reader_refuses_is_a_warning_and_a_share_falls_back() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    std::fs::write(fx.cfg.home_for("alice").join("a.txt"), b"a").unwrap();
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    // The reader's own owner check disagrees with the server's.
    let reader = real_reader(
        &fx,
        OwnerRules {
            home_owners: euid().wrapping_add(7)..=euid().wrapping_add(7),
            container_owner: euid(),
            container_groups: egid().wrapping_add(7)..=egid().wrapping_add(7),
            service: None,
        },
    );
    let (env, _) = helper_for(&fx, &reader);

    let meta = backup_users(&fx, &env).await;
    assert_eq!(meta.warnings.len(), 2, "{:?}", meta.warnings);
    assert!(meta.warnings[0].starts_with("users/homes/alice: skipped"));
    assert!(meta.warnings[0].contains("refused it (not_one_home)"));
    assert!(meta.warnings[1].starts_with("users/shared/team: read by the RAST service"));
    assert!(meta.warnings[1].contains("legacy_container"));
    let (seen, manifest) = read_archive(&meta.path);
    assert!(manifest.preserved_trees.is_empty());
    assert_eq!(seen["users/shared/team/plan.txt"].data, b"p");
    drop(fx.dir);
}

// ---------------------------------------------------------------------------
// The stream from the reader is untrusted
// ---------------------------------------------------------------------------

fn meta(path: &[u8], kind: Kind) -> EntryMeta {
    EntryMeta {
        path: path.to_vec(),
        kind,
        mode: 0o644,
        uid: 200_001,
        gid: 200_001,
        mtime: 1,
        acl_access: None,
        acl_default: None,
    }
}

/// An inner stream: entries with their data, the end, and a trailer that
/// matches unless `trailer` replaces it.
fn stream(entries: &[(EntryMeta, &[u8])], trailer: Option<Trailer>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut counted = Trailer {
        done: true,
        ..Trailer::default()
    };
    for (entry, data) in entries {
        archive::write_header(&mut out, entry).unwrap();
        counted.entries += 1;
        if let Kind::File { size } = entry.kind {
            out.extend_from_slice(data);
            archive::write_padding(&mut out, size).unwrap();
            counted.files += 1;
            counted.bytes += size;
        }
    }
    archive::write_end(&mut out).unwrap();
    out.extend(serde_json::to_vec(&trailer.unwrap_or(counted)).unwrap());
    out.push(b'\n');
    out
}

fn copy(input: &[u8]) -> (Result<Trailer, String>, Vec<u8>, Vec<ManifestEntry>) {
    copy_within(input, None)
}

fn copy_within(
    input: &[u8],
    budget: Option<u64>,
) -> (Result<Trailer, String>, Vec<u8>, Vec<ManifestEntry>) {
    let mut out = Vec::new();
    let mut manifest = Vec::new();
    let mut received = Received::default();
    let mut reader = input;
    let mut space = SpaceGuard::new(std::env::temp_dir(), 0);
    let result = match copy_entries(
        &mut reader,
        "users/homes/t-alice",
        budget,
        &mut out,
        &mut manifest,
        &mut received,
        &mut space,
    ) {
        Ok(trailer) => Ok(trailer),
        Err(CopyError::Stream(reason)) => Err(reason),
        Err(CopyError::Budget(budget)) => Err(format!("budget {budget}")),
        Err(CopyError::Archive(error)) => panic!("{error}"),
    };
    (result, out, manifest)
}

/// The outer entries, parsed back with the strict codec.
fn outer(out: &[u8]) -> Vec<EntryMeta> {
    let mut input = out;
    let mut entries = Vec::new();
    // The outer archive has no end blocks yet: stop when it is consumed.
    while !input.is_empty() {
        match archive::read_entry(&mut input).unwrap() {
            Next::Entry(entry) => {
                let mut data = vec![0u8; entry.data_size() as usize];
                archive::read_exact(&mut input, &mut data).unwrap();
                archive::read_padding(&mut input, entry.data_size()).unwrap();
                entries.push(entry);
            }
            Next::End => break,
        }
    }
    entries
}

#[test]
fn entries_are_rewritten_under_their_tree() {
    let input = stream(
        &[
            (meta(b".", Kind::Dir), b""),
            (meta(b"docs", Kind::Dir), b""),
            (meta(b"docs/a.txt", Kind::File { size: 3 }), b"abc"),
            (
                meta(
                    b"docs/b.txt",
                    Kind::HardLink {
                        target: b"docs/a.txt".to_vec(),
                    },
                ),
                b"",
            ),
            (
                meta(
                    b"up",
                    Kind::Symlink {
                        target: b"../../t-bob".to_vec(),
                    },
                ),
                b"",
            ),
        ],
        None,
    );
    let (result, out, manifest) = copy(&input);
    assert!(result.unwrap().done);
    let paths: Vec<String> = outer(&out)
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    assert_eq!(
        paths,
        [
            "users/homes/t-alice",
            "users/homes/t-alice/docs",
            "users/homes/t-alice/docs/a.txt",
            "users/homes/t-alice/docs/b.txt",
            "users/homes/t-alice/up",
        ]
    );
    let entries = outer(&out);
    assert_eq!(
        entries[3].kind,
        Kind::HardLink {
            target: b"users/homes/t-alice/docs/a.txt".to_vec()
        }
    );
    // A symlink target is data: kept as it is, never resolved.
    assert_eq!(
        entries[4].kind,
        Kind::Symlink {
            target: b"../../t-bob".to_vec()
        }
    );
    assert_eq!(entries[2].uid, 200_001);
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].path, "users/homes/t-alice/docs/a.txt");
    assert_eq!(manifest[0].sha256, hex::encode(Sha256::digest(b"abc")));
    assert!(manifest[0].owners_preserved);
}

#[test]
fn a_broken_or_lying_stream_is_refused() {
    let root = (meta(b".", Kind::Dir), &b""[..]);
    let file = (meta(b"a.txt", Kind::File { size: 3 }), &b"abc"[..]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "not start with the folder",
            stream(std::slice::from_ref(&file), None),
        ),
        ("appears twice", stream(&[root.clone(), root.clone()], None)),
        (
            "does not match",
            stream(
                &[root.clone(), file.clone()],
                Some(Trailer {
                    done: true,
                    entries: 2,
                    files: 1,
                    bytes: 4,
                    ..Trailer::default()
                }),
            ),
        ),
        (
            "invalid trailer",
            stream(
                std::slice::from_ref(&root),
                Some(Trailer {
                    done: false,
                    entries: 1,
                    ..Trailer::default()
                }),
            ),
        ),
        (
            "invalid trailer",
            stream(
                std::slice::from_ref(&root),
                Some(Trailer {
                    done: true,
                    entries: 1,
                    problems: vec!["x".repeat(MAX_PROBLEM_CHARS + 1)],
                    ..Trailer::default()
                }),
            ),
        ),
        ("data after the trailer", {
            let mut bytes = stream(std::slice::from_ref(&root), None);
            bytes.extend_from_slice(b"more");
            bytes
        }),
        ("no trailer", {
            let mut bytes = stream(std::slice::from_ref(&root), None);
            bytes.truncate(bytes.len() - 20);
            bytes
        }),
        ("invalid trailer", {
            let mut bytes = Vec::new();
            archive::write_header(&mut bytes, &root.0).unwrap();
            archive::write_end(&mut bytes).unwrap();
            bytes.extend_from_slice(b"{\"done\":true,\"entries\":1,\"path\":\"/etc\"}\n");
            bytes
        }),
    ];
    for (expected, input) in cases {
        let (result, _, _) = copy(&input);
        let reason = result.unwrap_err();
        assert!(reason.contains(expected), "{expected}: {reason}");
    }

    // An oversized PAX header after the root.
    let mut input = Vec::new();
    archive::write_header(&mut input, &root.0).unwrap();
    let mut pax = meta(b"x", Kind::File { size: 0 });
    pax.path = vec![b'p'; 200];
    let mut header = Vec::new();
    archive::write_header(&mut header, &pax).unwrap();
    // Rewrite the PAX block's size field to 1 MiB (and its checksum).
    header[124..136].copy_from_slice(b"00004000000\0");
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header[..512].iter().map(|byte| *byte as u32).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    input.extend_from_slice(&header);
    let (result, out, _) = copy(&input);
    assert!(result.is_err());
    assert_eq!(outer(&out).len(), 1, "only the root was written");
}

#[test]
fn a_file_cut_off_by_the_reader_is_padded_and_left_out_of_the_manifest() {
    // The second file announces 10 000 bytes and sends 100.
    let mut cut = Vec::new();
    archive::write_header(&mut cut, &meta(b".", Kind::Dir)).unwrap();
    archive::write_header(&mut cut, &meta(b"a.txt", Kind::File { size: 3 })).unwrap();
    cut.extend_from_slice(b"abc");
    archive::write_padding(&mut cut, 3).unwrap();
    archive::write_header(&mut cut, &meta(b"big.bin", Kind::File { size: 10_000 })).unwrap();
    cut.extend_from_slice(&[9u8; 100]);

    let (result, out, manifest) = copy(&cut);
    let reason = result.unwrap_err();
    assert!(reason.contains("ended inside a file"), "{reason}");
    assert!(reason.contains("users/homes/t-alice/big.bin"), "{reason}");
    assert_eq!(out.len() % 512, 0, "the archive stays block aligned");
    let entries = outer(&out);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].kind, Kind::File { size: 10_000 });
    let paths: Vec<&str> = manifest.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["users/homes/t-alice/a.txt"]);
}

/// A reader that answers every connection with `bytes`.
fn fake_reader(fx: &Fx, bytes: Vec<u8>) -> PathBuf {
    let socket = fx.sockets.path().join("fake-reader.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stream
                .read(&mut byte)
                .map(|read| read == 1)
                .unwrap_or(false)
                && byte[0] != b'\n'
            {
                line.push(byte[0]);
            }
            let _ = stream.write_all(&bytes);
        }
    });
    socket
}

#[tokio::test]
async fn a_broken_stream_keeps_the_backup_valid_and_says_so() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    let mut bytes = b"{\"ok\":true}\n".to_vec();
    archive::write_header(&mut bytes, &meta(b".", Kind::Dir)).unwrap();
    archive::write_header(&mut bytes, &meta(b"a.txt", Kind::File { size: 3 })).unwrap();
    bytes.extend_from_slice(b"abc");
    archive::write_padding(&mut bytes, 3).unwrap();
    archive::write_header(&mut bytes, &meta(b"big.bin", Kind::File { size: 10_000 })).unwrap();
    bytes.extend_from_slice(&[9u8; 100]);
    let reader = fake_reader(&fx, bytes);
    let (env, _) = helper_for(&fx, &reader);

    let meta = backup_users(&fx, &env).await;
    assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
    assert!(meta.warnings[0].starts_with("users/homes/alice: incomplete"));
    assert!(meta.warnings[0].contains("after 2 entries"));
    let (seen, manifest) = read_archive(&meta.path);
    assert_eq!(manifest.preserved_trees, ["users/homes/alice"]);
    assert_eq!(seen["users/homes/alice/a.txt"].data, b"abc");
    assert_eq!(seen["users/homes/alice/big.bin"].data.len(), 10_000);
    let verified = backup::verify(Path::new(&meta.path)).await.unwrap();
    assert_eq!(verified["ok"], true, "{verified}");
    drop(fx.dir);
}

#[tokio::test]
async fn a_reader_that_refuses_or_talks_nonsense_is_a_warning() {
    for (reply, expected) in [
        (
            &b"{\"ok\":false,\"error\":{\"code\":\"not_found\",\"message\":\"no such HOME\"}}\n"[..],
            "refused it (not_found): no such HOME",
        ),
        (&b"{\"ok\":true,\"extra\":1}\n"[..], "unexpected reply"),
        (&b"no newline"[..], "did not answer"),
    ] {
        let fx = fixture(None).await;
        std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
        let reader = fake_reader(&fx, reply.to_vec());
        let (env, _) = helper_for(&fx, &reader);
        let meta = backup_users(&fx, &env).await;
        assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
        assert!(meta.warnings[0].starts_with("users/homes/alice: skipped"));
        assert!(meta.warnings[0].contains(expected), "{:?}", meta.warnings);
        drop(fx.dir);
    }
}

#[test]
fn the_helper_reply_is_checked() {
    let good = json!({"socket": "/run/rast/native/backup-reader/reader.sock", "token": TOKEN});
    let access = parse_access(&good).unwrap();
    assert_eq!(access.token, TOKEN);
    for bad in [
        json!({"socket": "reader.sock", "token": TOKEN}),
        json!({"socket": format!("/{}", "x".repeat(120)), "token": TOKEN}),
        json!({"socket": "/run/r.sock", "token": "short"}),
        json!({"socket": "/run/r.sock", "token": "z".repeat(64)}),
        json!({"socket": "/run/r.sock"}),
        json!({}),
    ] {
        assert!(parse_access(&bad).is_err(), "{bad}");
    }
}

#[test]
fn only_unexpected_losses_become_warnings() {
    let quiet = Trailer {
        done: true,
        excluded: 4,
        special: 2,
        vanished: 1,
        ..Trailer::default()
    };
    assert!(trailer_warnings("users/homes/t-alice", &quiet).is_empty());
    let loud = Trailer {
        done: true,
        unreadable: 2,
        other_file_systems: 1,
        problems: vec![
            "docs/x: cannot open it\u{7}".into(),
            "mnt: another mount".into(),
        ],
        ..Trailer::default()
    };
    let warnings = trailer_warnings("users/homes/t-alice", &loud);
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings[0].contains("2 unreadable, 1 on other file systems or mounts"));
    assert_eq!(warnings[1], "users/homes/t-alice/docs/x: cannot open it ");
    // Entries of other owners (F6) and sparse files (F7) are lost against
    // the user's will: they are warnings.
    let left_out = Trailer {
        done: true,
        other_owner: 1,
        sparse: 2,
        ..Trailer::default()
    };
    let warnings = trailer_warnings("users/homes/t-alice", &left_out);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("1 owned by accounts this folder may not hold, 2 sparse files"),
        "{warnings:?}"
    );
}

// ---------------------------------------------------------------------------
// Budgets and free space (review F7)
// ---------------------------------------------------------------------------

#[test]
fn a_tree_budget_is_its_quota_plus_slack() {
    assert_eq!(tree_budget(None), None);
    assert_eq!(tree_budget(Some(0)), None);
    assert_eq!(tree_budget(Some(1 << 30)), Some((1 << 30) + TREE_SLACK));
    let big = 100u64 << 40;
    assert_eq!(tree_budget(Some(big)), Some(big + big / 10));
    assert_eq!(tree_budget(Some(u64::MAX)), Some(u64::MAX));
}

/// A reader that announces a 1 TiB file: nothing of it is written, the
/// tree is incomplete and the entries before it stay.
#[test]
fn a_file_beyond_the_budget_is_never_written() {
    let mut input = Vec::new();
    archive::write_header(&mut input, &meta(b".", Kind::Dir)).unwrap();
    archive::write_header(&mut input, &meta(b"a.txt", Kind::File { size: 3 })).unwrap();
    input.extend_from_slice(b"abc");
    archive::write_padding(&mut input, 3).unwrap();
    archive::write_header(&mut input, &meta(b"huge", Kind::File { size: 1 << 40 })).unwrap();
    // The stream ends there.
    let (result, out, manifest) = copy_within(&input, Some(1 << 20));
    assert_eq!(result.unwrap_err(), format!("budget {}", 1 << 20));
    assert!(out.len() < 8 * 1024, "{}", out.len());
    let entries = outer(&out);
    let paths: Vec<&[u8]> = entries.iter().map(|entry| &entry.path[..]).collect();
    assert_eq!(
        paths,
        [&b"users/homes/t-alice"[..], b"users/homes/t-alice/a.txt"]
    );
    assert_eq!(manifest.len(), 1);
}

#[tokio::test]
async fn the_budget_comes_from_the_quota_and_ends_the_tree_with_a_warning() {
    let fx = fixture(None).await;
    sqlx::query("UPDATE users SET quota_bytes = ? WHERE username = 'alice'")
        .bind(1_000_000i64)
        .execute(&fx.pool)
        .await
        .unwrap();
    std::fs::create_dir_all(fx.cfg.home_for("alice")).unwrap();
    let budget = tree_budget(Some(1_000_000)).unwrap();
    let mut bytes = b"{\"ok\":true}\n".to_vec();
    archive::write_header(&mut bytes, &meta(b".", Kind::Dir)).unwrap();
    archive::write_header(&mut bytes, &meta(b"a.txt", Kind::File { size: 3 })).unwrap();
    bytes.extend_from_slice(b"abc");
    archive::write_padding(&mut bytes, 3).unwrap();
    archive::write_header(&mut bytes, &meta(b"huge", Kind::File { size: budget + 1 })).unwrap();
    let reader = fake_reader(&fx, bytes);
    let (env, _) = helper_for(&fx, &reader);

    let meta = backup_users(&fx, &env).await;
    assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
    assert!(
        meta.warnings[0].starts_with(
            "users/homes/alice: incomplete, the folder is larger than its backup budget"
        ),
        "{:?}",
        meta.warnings
    );
    assert!(meta.size_bytes < 64 * 1024, "{}", meta.size_bytes);
    let (seen, manifest) = read_archive(&meta.path);
    assert_eq!(manifest.preserved_trees, ["users/homes/alice"]);
    assert_eq!(seen["users/homes/alice/a.txt"].data, b"abc");
    assert!(!seen.contains_key("users/homes/alice/huge"));
    let verified = backup::verify(Path::new(&meta.path)).await.unwrap();
    assert_eq!(verified["ok"], true, "{verified}");
    drop(fx.dir);
}

#[tokio::test]
async fn a_users_backup_stops_before_the_disk_is_full() {
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("legacy")).unwrap();
    std::fs::write(fx.cfg.home_for("legacy").join("a.txt"), b"a").unwrap();
    let mut env = one_home_env(None);
    // The service owns everything here (legacy), and no file system has
    // this much free.
    env.service_uid = Some(euid());
    env.min_free_bytes = u64::MAX;
    let error = backup::create_with(
        &fx.cfg,
        Some(&fx.pool),
        &["users".into()],
        "",
        None,
        None,
        &env,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("bytes free, less than"), "{error}");
    // A database-only backup does not measure.
    let meta = backup::create_with(
        &fx.cfg,
        Some(&fx.pool),
        &["db".into()],
        "",
        None,
        None,
        &env,
    )
    .await
    .unwrap();
    assert!(meta.warnings.is_empty());
    let mut guard = SpaceGuard::new(fx.dir.path().to_path_buf(), u64::MAX);
    assert!(guard.wrote(1).is_ok(), "measured only every 64 MiB");
    assert!(guard.wrote(64 << 20).is_err());
    drop(fx.dir);
}

/// Review F1: what a users backup holds goes back with
/// `rast restore-files`, checked entry by entry, never with raw `tar`.
fn restore_inputs(cfg: &Config, identity: Option<(i64, u32)>) -> (Config, Inputs, OwnerLog) {
    let mut cfg = cfg.clone();
    let (mut passwd, mut group) = (String::new(), String::new());
    if let Some((id, uid)) = identity {
        cfg.linux_uid_base = uid - u32::try_from(id).unwrap();
        passwd = format!("rast-u{id}:x:{uid}:{uid}::/:/bin/sh\n");
        group = format!("rast-u{id}:x:{uid}:\n");
    }
    let log = OwnerLog::default();
    let inputs = Inputs {
        facts: Facts::default(),
        host: HostAccounts::parse(&passwd, &group),
        service_uid: euid().wrapping_add(1),
        service_gid: egid().wrapping_add(1),
        owners: Owners::Record(log.clone()),
    };
    (cfg, inputs, log)
}

fn recorded(log: &OwnerLog, path: &Path) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).unwrap();
    log.lock().unwrap()[&(meta.dev(), meta.ino())]
}

#[tokio::test]
async fn a_one_home_backup_comes_back_with_restore_files() {
    use crate::backup::restore_files::{load_facts, run_with, Mode, RunOptions};
    use std::os::unix::fs::MetadataExt;
    if euid() < 2 {
        eprintln!("skipped: runs as an unprivileged account");
        return;
    }
    let fx = fixture(None).await;
    let home = fx.cfg.home_for("alice");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::write(home.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::set_permissions(
        home.join("docs/a.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::hard_link(home.join("docs/a.txt"), home.join("docs/b.txt")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", home.join("passwd")).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let team = fx.cfg.shared_dir().join("team");
    std::fs::create_dir_all(&team).unwrap();
    std::fs::write(team.join("plan.txt"), b"plan").unwrap();
    let reader = real_reader(&fx, my_rules());
    let (env, _calls) = helper_for(&fx, &reader);
    let meta = backup_users(&fx, &env).await;
    assert!(meta.warnings.is_empty(), "{:?}", meta.warnings);

    let facts = load_facts(&fx.cfg.db_path()).await.unwrap();
    let alice = facts.accounts["alice"];
    assert_eq!(facts.shares["team"], "s1");
    let (cfg, mut inputs, log) = restore_inputs(&fx.cfg, Some((alice, euid())));
    inputs.facts = facts;

    // The container belongs to the test account, not to root: the whole
    // archive is refused, and a dry run writes nothing anyway.
    let all = RunOptions {
        archive: PathBuf::from(&meta.path),
        trees: Vec::new(),
        mode: Mode::DryRun,
    };
    let report = run_with(&cfg, all, inputs.clone()).await.unwrap();
    assert!(!report.ok);
    assert!(
        report.refused[0].starts_with("users/shared/team: the container belongs to uid"),
        "{:?}",
        report.refused
    );

    let target = fx.dir.path().join("restored");
    let alice_only = RunOptions {
        archive: PathBuf::from(&meta.path),
        trees: vec!["users/homes/alice".into()],
        mode: Mode::Scratch(Some(target.clone())),
    };
    let report = run_with(&cfg, alice_only, inputs).await.unwrap();
    assert!(report.ok, "{report:?}");
    let restored = target.join("homes/alice");
    assert_eq!(
        std::fs::read(restored.join("docs/a.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(
        std::fs::metadata(restored.join("docs/a.txt"))
            .unwrap()
            .mode()
            & 0o7777,
        0o640
    );
    assert_eq!(
        std::fs::metadata(restored.join("docs/a.txt"))
            .unwrap()
            .ino(),
        std::fs::metadata(restored.join("docs/b.txt"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        std::fs::read_link(restored.join("passwd")).unwrap(),
        Path::new("/etc/passwd")
    );
    assert_eq!(recorded(&log, &restored).0, euid());
    assert_eq!(recorded(&log, &restored.join("docs/a.txt")).0, euid());
    assert!(!target.join("shared/team").exists());
    drop(fx.dir);
}

#[tokio::test]
async fn a_legacy_backup_comes_back_as_the_services_files() {
    use crate::backup::restore_files::{run_with, Mode, RunOptions};
    let fx = fixture(None).await;
    std::fs::create_dir_all(fx.cfg.home_for("alice").join("deep/er")).unwrap();
    std::fs::write(fx.cfg.home_for("alice").join("deep/er/a.txt"), b"a").unwrap();
    let long = "n".repeat(150);
    std::fs::write(fx.cfg.home_for("alice").join(&long), b"long").unwrap();
    std::fs::create_dir_all(fx.cfg.shared_dir().join("team")).unwrap();
    std::fs::write(fx.cfg.shared_dir().join("team/plan.txt"), b"p").unwrap();
    let (mut env, _calls) = helper_for(&fx, Path::new("/nonexistent/reader.sock"));
    env.service_uid = Some(euid());
    env.container_owner_uid = 0;
    let meta = backup_users(&fx, &env).await;
    assert!(meta.warnings.is_empty(), "{:?}", meta.warnings);

    let (cfg, inputs, log) = restore_inputs(&fx.cfg, None);
    let target = fx.dir.path().join("restored");
    let options = RunOptions {
        archive: PathBuf::from(&meta.path),
        trees: Vec::new(),
        mode: Mode::Scratch(Some(target.clone())),
    };
    let report = run_with(&cfg, options, inputs).await.unwrap();
    assert!(report.ok, "{report:?}");
    let service = (euid().wrapping_add(1), egid().wrapping_add(1));
    let alice = target.join("homes/alice");
    assert_eq!(std::fs::read(alice.join("deep/er/a.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(alice.join(&long)).unwrap(), b"long");
    for path in [
        alice.clone(),
        alice.join("deep"),
        alice.join("deep/er"),
        alice.join("deep/er/a.txt"),
        target.join("shared/team/plan.txt"),
    ] {
        assert_eq!(recorded(&log, &path), service, "{}", path.display());
    }
    let layouts: Vec<&str> = report
        .trees
        .iter()
        .map(|tree| tree.layout.as_str())
        .collect();
    assert_eq!(layouts, ["legacy", "legacy"]);
    drop(fx.dir);
}
