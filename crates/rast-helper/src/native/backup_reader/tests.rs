use super::archive::{EntryMeta, Kind, Next};
use super::*;
use crate::native::acl::{self, Acl, Entry, Tag};
use std::os::unix::fs::PermissionsExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn egid() -> u32 {
    // SAFETY: getegid has no preconditions.
    unsafe { libc::getegid() }
}

/// A homes and a shared root whose folders the test account owns, with
/// rules that treat the test account as the identity and as root.
struct Roots {
    dir: tempfile::TempDir,
    options: ReaderOptions,
}

fn roots() -> Roots {
    let dir = tempfile::tempdir().unwrap();
    let homes = dir.path().join("storage/homes");
    let shared = dir.path().join("storage/shared");
    std::fs::create_dir_all(&homes).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let options = ReaderOptions {
        homes,
        shared,
        rules: OwnerRules {
            home_owners: euid()..=euid(),
            container_owner: euid(),
            container_groups: egid()..=egid(),
        },
        token: TOKEN.into(),
        peer_uid: None,
        idle: Duration::from_secs(5),
        listen: Listen::Systemd,
    };
    Roots { dir, options }
}

/// A temp folder short enough for a unix socket path (about 108 bytes).
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

fn request(tree: &str, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"token": TOKEN, "tree": tree, "name": name})).unwrap()
}

#[test]
fn requests_are_strict_and_name_one_folder_only() {
    assert_eq!(
        check_request(&request("home", "t-alice"), TOKEN).unwrap(),
        (TreeKind::Home, "t-alice".to_string())
    );
    assert_eq!(
        check_request(&request("shared", "Team Files"), TOKEN).unwrap(),
        (TreeKind::Shared, "Team Files".to_string())
    );
    let refused = |line: &[u8]| check_request(line, TOKEN).unwrap_err();
    // Unknown fields (a path in particular), missing fields, other trees.
    for line in [
        json!({"token": TOKEN, "tree": "home", "name": "alice", "path": "/etc"}),
        json!({"token": TOKEN, "tree": "home", "name": "alice", "follow": true}),
        json!({"token": TOKEN, "tree": "home"}),
        json!({"tree": "home", "name": "alice"}),
        json!({"token": TOKEN, "tree": "root", "name": "etc"}),
        json!({"token": TOKEN, "tree": "Home", "name": "alice"}),
        json!({"token": TOKEN, "tree": "home", "name": 7}),
    ] {
        let error = refused(&serde_json::to_vec(&line).unwrap());
        assert_eq!(error.code, "invalid_request", "{line}");
    }
    assert_eq!(refused(b"not json").code, "invalid_request");
    // Names: one valid username or container, nothing that walks.
    for name in [
        "",
        ".",
        "..",
        "../etc",
        "a/b",
        "/etc",
        "Alice",
        " alice",
        "alice\0x",
        "rast-u5",
        &"a".repeat(33),
    ] {
        assert_eq!(
            refused(&request("home", name)).code,
            "invalid_request",
            "{name:?}"
        );
    }
    for name in ["", ".", "..", "a/b", "/srv", "a\0b", &"x".repeat(300)] {
        assert_eq!(
            refused(&request("shared", name)).code,
            "invalid_request",
            "{name:?}"
        );
    }
    // The token is checked before the name.
    let wrong = serde_json::to_vec(&json!({"token": "f".repeat(64), "tree": "home", "name": ".."}))
        .unwrap();
    assert_eq!(refused(&wrong).code, "forbidden");
    let short =
        serde_json::to_vec(&json!({"token": "0123", "tree": "home", "name": "alice"})).unwrap();
    assert_eq!(refused(&short).code, "forbidden");
}

#[test]
fn only_real_folders_in_the_new_layout_are_opened() {
    let mut fx = roots();
    let homes = fx.options.homes.clone();
    std::fs::create_dir(homes.join("alice")).unwrap();
    assert!(open_tree(&fx.options, TreeKind::Home, "alice").is_ok());
    // A symlinked HOME is never followed, even into another HOME.
    std::os::unix::fs::symlink(homes.join("alice"), homes.join("mallory")).unwrap();
    let error = open_tree(&fx.options, TreeKind::Home, "mallory").unwrap_err();
    assert_eq!(error.code, "forbidden");
    assert!(error.message.contains("symbolic link"), "{error:?}");
    std::os::unix::fs::symlink("/etc", homes.join("eve")).unwrap();
    assert_eq!(
        open_tree(&fx.options, TreeKind::Home, "eve")
            .unwrap_err()
            .code,
        "forbidden"
    );
    std::fs::write(homes.join("file"), b"x").unwrap();
    assert_eq!(
        open_tree(&fx.options, TreeKind::Home, "file")
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        open_tree(&fx.options, TreeKind::Home, "nobody")
            .unwrap_err()
            .code,
        "not_found"
    );
    // A shared folder is looked up in the shared root only.
    assert_eq!(
        open_tree(&fx.options, TreeKind::Shared, "alice")
            .unwrap_err()
            .code,
        "not_found"
    );
    std::fs::create_dir(fx.options.shared.join("team")).unwrap();
    assert!(open_tree(&fx.options, TreeKind::Shared, "team").is_ok());

    // A HOME that is not owned by an identity is the service's own (legacy).
    fx.options.rules.home_owners = euid().wrapping_add(1)..=euid().wrapping_add(1);
    let error = open_tree(&fx.options, TreeKind::Home, "alice").unwrap_err();
    assert_eq!(error.code, "not_one_home");
    // A container needs root as its owner and a shared folder group.
    fx.options.rules.container_groups = egid().wrapping_add(1)..=egid().wrapping_add(1);
    assert_eq!(
        open_tree(&fx.options, TreeKind::Shared, "team")
            .unwrap_err()
            .code,
        "legacy_container"
    );
    fx.options.rules.container_groups = egid()..=egid();
    fx.options.rules.container_owner = euid().wrapping_add(1);
    assert_eq!(
        open_tree(&fx.options, TreeKind::Shared, "team")
            .unwrap_err()
            .code,
        "legacy_container"
    );
    drop(fx.dir);
}

#[test]
fn owner_rules_follow_the_uid_base() {
    let rules = OwnerRules::for_uid_base(200_000).unwrap();
    assert_eq!(rules.home_owners, 200_001..=1_200_000);
    assert_eq!(rules.container_owner, 0);
    assert_eq!(rules.container_groups, 1_200_001..=2_199_999);
    assert!(!rules.home_owners.contains(&0));
    assert!(OwnerRules::for_uid_base(u32::MAX - 10).is_err());
}

/// The entries of a stream (data included), after the reply line, and the
/// trailer.
fn parse_stream(stream: &[u8]) -> (Vec<(EntryMeta, Vec<u8>)>, Trailer) {
    let mut input = stream;
    let mut entries = Vec::new();
    while let Next::Entry(entry) = archive::read_entry(&mut input).unwrap() {
        let mut data = vec![0u8; entry.data_size() as usize];
        archive::read_exact(&mut input, &mut data).unwrap();
        archive::read_padding(&mut input, entry.data_size()).unwrap();
        entries.push((entry, data));
    }
    archive::read_end(&mut input).unwrap();
    let line = read_line(&mut input, MAX_TRAILER_LINE).unwrap();
    assert!(input.is_empty(), "nothing may follow the trailer");
    (entries, serde_json::from_slice(&line).unwrap())
}

fn find<'a>(entries: &'a [(EntryMeta, Vec<u8>)], path: &str) -> Option<&'a (EntryMeta, Vec<u8>)> {
    entries
        .iter()
        .find(|(entry, _)| entry.path == path.as_bytes())
}

fn stream_of(
    options: &ReaderOptions,
    kind: TreeKind,
    name: &str,
) -> (Vec<(EntryMeta, Vec<u8>)>, Trailer) {
    let root = open_tree(options, kind, name).unwrap();
    let mut out = Vec::new();
    let summary = walk::stream_tree(&mut out, kind, root).unwrap();
    archive::write_end(&mut out).unwrap();
    write_line(&mut out, &summary).unwrap();
    parse_stream(&out)
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

#[test]
fn a_home_is_archived_with_owners_modes_acls_and_links_never_followed() {
    let fx = roots();
    let home = fx.options.homes.join("alice");
    std::fs::create_dir_all(home.join("docs/deep")).unwrap();
    std::fs::write(home.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::set_permissions(
        home.join("docs/a.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::write(home.join("docs/deep/b.bin"), vec![7u8; 70_000]).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A hard link: stored once, then as a link.
    std::fs::hard_link(home.join("docs/a.txt"), home.join("docs/again.txt")).unwrap();
    // Links to outside and to another HOME: stored as links.
    std::os::unix::fs::symlink("/etc/shadow", home.join("shadow")).unwrap();
    let other = fx.options.homes.join("bob");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("secret.txt"), b"bob only").unwrap();
    std::os::unix::fs::symlink(&other, home.join("bob-home")).unwrap();
    // Left out: caches, the recycle bin, Flatpak, a FIFO.
    for excluded in [
        ".cache/x",
        ".local/share/Trash/files",
        ".local/share/flatpak/app",
    ] {
        std::fs::create_dir_all(home.join(excluded)).unwrap();
        std::fs::write(home.join(excluded).join("big"), b"skip me").unwrap();
    }
    std::fs::create_dir_all(home.join(".local/share/kept")).unwrap();
    std::fs::write(home.join(".local/share/kept/k"), b"k").unwrap();
    let fifo = CString::new(home.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    // An ACL on a folder (access and default) and on a file, when the
    // file system has ACLs.
    let acls = {
        let docs = std::fs::File::open(home.join("docs")).unwrap();
        let file = std::fs::File::open(home.join("docs/a.txt")).unwrap();
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
            && acl::write(
                file.as_raw_fd(),
                acl::ACCESS_XATTR,
                &acl_with_user(4_000_003),
            )
            .is_ok()
    };

    let (entries, trailer) = stream_of(&fx.options, TreeKind::Home, "alice");
    let paths: Vec<String> = entries
        .iter()
        .map(|(entry, _)| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    // The root first, as `.`, with the HOME's own owner and mode.
    let (root, _) = &entries[0];
    assert_eq!(root.path, b".");
    assert_eq!(root.kind, Kind::Dir);
    assert_eq!(root.mode, 0o700);
    assert_eq!((root.uid, root.gid), (euid(), egid()));

    // Exactly one of the two names of the hard-linked file carries the
    // data (whichever the listing returns first); the other is a link to it.
    type Named<'a> = Vec<&'a (EntryMeta, Vec<u8>)>;
    let names: Named = ["docs/a.txt", "docs/again.txt"]
        .iter()
        .map(|path| find(&entries, path).unwrap())
        .collect();
    let (stored, linked): (Named, Named) = names
        .into_iter()
        .partition(|(entry, _)| matches!(entry.kind, Kind::File { .. }));
    assert_eq!(
        (stored.len(), linked.len()),
        (1, 1),
        "{stored:?} {linked:?}"
    );
    let (file, data) = stored[0];
    assert_eq!(data, b"alpha");
    assert_eq!(file.kind, Kind::File { size: 5 });
    assert_eq!((file.uid, file.gid), (euid(), egid()));
    assert_eq!(
        linked[0].0.kind,
        Kind::HardLink {
            target: file.path.clone()
        }
    );
    assert!(linked[0].1.is_empty());
    assert_eq!(trailer.hard_links, 1);
    let (big, data) = find(&entries, "docs/deep/b.bin").unwrap();
    assert_eq!(big.kind, Kind::File { size: 70_000 });
    assert!(data.iter().all(|byte| *byte == 7));

    let (shadow, data) = find(&entries, "shadow").unwrap();
    assert_eq!(
        shadow.kind,
        Kind::Symlink {
            target: b"/etc/shadow".to_vec()
        }
    );
    assert!(data.is_empty());
    let (bob, _) = find(&entries, "bob-home").unwrap();
    assert!(matches!(bob.kind, Kind::Symlink { .. }));
    assert!(
        !paths.iter().any(|path| path.contains("secret")),
        "{paths:?}"
    );

    for gone in [
        ".cache",
        ".local/share/Trash",
        ".local/share/flatpak",
        "pipe",
    ] {
        assert!(
            !paths
                .iter()
                .any(|path| path == gone || path.starts_with(&format!("{gone}/"))),
            "{gone} must be left out: {paths:?}"
        );
    }
    assert!(paths.contains(&".local/share/kept/k".to_string()));
    assert_eq!(trailer.excluded, 3);
    assert_eq!(trailer.special, 1);
    assert!(trailer.done);
    assert_eq!(trailer.files, 3);
    assert_eq!(trailer.bytes, 5 + 70_000 + 1);
    assert_eq!(trailer.entries as usize, entries.len());
    assert!(trailer.problems.is_empty(), "{:?}", trailer.problems);

    if acls {
        let (docs, _) = find(&entries, "docs").unwrap();
        assert_eq!(docs.acl_access, Some(acl_with_user(4_000_001)));
        assert_eq!(docs.acl_default, Some(acl_with_user(4_000_002)));
        assert_eq!(file.acl_access, Some(acl_with_user(4_000_003)));
        let (shadow, _) = find(&entries, "shadow").unwrap();
        assert_eq!(shadow.acl_access, None);
    }
}

#[test]
fn a_shared_folder_leaves_out_its_recycle_bins_only() {
    let fx = roots();
    let team = fx.options.shared.join("team");
    std::fs::create_dir_all(team.join(".Trash-200007/files")).unwrap();
    std::fs::create_dir_all(team.join(".Trash/200008")).unwrap();
    std::fs::create_dir_all(team.join(".Trash-notes")).unwrap();
    std::fs::create_dir_all(team.join("plans/.Trash-200007")).unwrap();
    std::fs::write(team.join("plans/q1.txt"), b"q1").unwrap();
    std::fs::create_dir_all(team.join(".cache")).unwrap();
    let (entries, trailer) = stream_of(&fx.options, TreeKind::Shared, "team");
    let paths: Vec<String> = entries
        .iter()
        .map(|(entry, _)| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    assert!(!paths.iter().any(|path| path.starts_with(".Trash-200007")));
    assert!(!paths
        .iter()
        .any(|path| path.starts_with(".Trash/") || path == ".Trash"));
    // Only at the top, and only `.Trash-<uid>`; a HOME's exclusions do not
    // apply here.
    for kept in [
        ".Trash-notes",
        "plans/.Trash-200007",
        "plans/q1.txt",
        ".cache",
    ] {
        assert!(paths.contains(&kept.to_string()), "{kept}: {paths:?}");
    }
    assert_eq!(trailer.excluded, 2);
}

#[test]
fn unreadable_entries_are_reported_and_the_stream_stays_whole() {
    let fx = roots();
    let home = fx.options.homes.join("alice");
    std::fs::create_dir_all(home.join("locked")).unwrap();
    std::fs::write(home.join("locked/x"), b"x").unwrap();
    std::fs::write(home.join("closed.txt"), b"closed").unwrap();
    std::fs::set_permissions(
        home.join("closed.txt"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(home.join("locked")).is_ok() {
        // Running as root: permissions do not apply.
        std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        return;
    }
    let (entries, trailer) = stream_of(&fx.options, TreeKind::Home, "alice");
    std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Neither is in the stream (root, with CAP_DAC_READ_SEARCH, reads both
    // in production); the rest of the HOME is.
    assert!(find(&entries, "locked").is_none());
    assert!(find(&entries, "closed.txt").is_none());
    assert!(find(&entries, ".").is_some());
    assert_eq!(trailer.unreadable, 2, "{trailer:?}");
    assert_eq!(trailer.problems.len(), 2);
    assert!(trailer.done);
}

/// The whole connection over a socket: token, reply, archive, trailer.
#[test]
fn the_reader_answers_one_tree_per_connection() {
    let dir = socket_dir();
    let mut fx = roots();
    std::fs::create_dir(fx.options.homes.join("alice")).unwrap();
    std::fs::write(fx.options.homes.join("alice/a.txt"), b"a").unwrap();
    let socket = dir.path().join("r.sock");
    fx.options.listen = Listen::Path(socket.clone());
    fx.options.peer_uid = Some(euid());
    fx.options.idle = Duration::from_millis(1500);
    let options = fx.options;
    let reader = std::thread::spawn(move || run(options));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let exchange = |line: &[u8]| -> Vec<u8> {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(line).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        reply
    };
    let reply = exchange(&request("home", "alice"));
    let mut input = &reply[..];
    let first = read_line(&mut input, MAX_REPLY_LINE).unwrap();
    let parsed: ReaderReply = serde_json::from_slice(&first).unwrap();
    assert!(parsed.ok);
    let (entries, trailer) = parse_stream(input);
    assert_eq!(find(&entries, "a.txt").unwrap().1, b"a");
    assert!(trailer.done);

    let refused = exchange(
        &serde_json::to_vec(&json!({"token": "x".repeat(64), "tree": "home", "name": "alice"}))
            .unwrap(),
    );
    let reply: ReaderReply = serde_json::from_slice(refused.strip_suffix(b"\n").unwrap()).unwrap();
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "forbidden");
    let missing = exchange(&request("home", "nobody"));
    let reply: ReaderReply = serde_json::from_slice(missing.strip_suffix(b"\n").unwrap()).unwrap();
    assert_eq!(reply.error.unwrap().code, "not_found");
    // Idle: the reader stops by itself and removes its socket.
    reader.join().unwrap().unwrap();
    assert!(!socket.exists());
    drop(fx.dir);
}

#[test]
fn other_accounts_get_no_answer() {
    let dir = socket_dir();
    let mut fx = roots();
    std::fs::create_dir(fx.options.homes.join("alice")).unwrap();
    let socket = dir.path().join("r.sock");
    fx.options.listen = Listen::Path(socket.clone());
    fx.options.peer_uid = Some(euid().wrapping_add(1));
    fx.options.idle = Duration::from_millis(800);
    let options = fx.options;
    let reader = std::thread::spawn(move || run(options));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stream = UnixStream::connect(&socket).unwrap();
    let _ = stream.write_all(&request("home", "alice"));
    let _ = stream.write_all(b"\n");
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    assert!(reply.is_empty(), "{}", String::from_utf8_lossy(&reply));
    reader.join().unwrap().unwrap();
    drop(fx.dir);
}

// ---------------------------------------------------------------------------
// backup_reader_start
// ---------------------------------------------------------------------------

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A script that logs its argv (unit separators) to `<name>.log`.
fn logging_script(dir: &Path, name: &str, extra: &str) -> PathBuf {
    let log = dir.join(format!("{name}.log"));
    write_script(
        dir,
        name,
        &format!(
            "#!/bin/sh\nline=\"\"\nsep=\"\"\nfor arg in \"$@\"; do line=\"$line$sep$arg\"; \
             sep=$(printf '\\037'); done\nprintf '%s\\n' \"$line\" >> \"{log}\"\n{extra}\nexit 0\n",
            log = log.display()
        ),
    )
}

fn logged(path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect()
}

#[tokio::test]
async fn backup_reader_start_builds_a_read_only_socket_activated_unit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let data = root.join("data");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    let mut options = NativeOptions::for_test(root.join("native.sock"), data, run_dir.clone());
    options.homes_dir = root.join("storage/homes");
    options.storage_dir = root.join("storage");
    options.rast_uid = Some(euid().wrapping_add(1));
    options.systemctl_bin = logging_script(
        root,
        "systemctl",
        &format!(
            "[ \"$1\" = show ] && [ -f '{root}/active' ] && \
             printf 'Id=%s\\nActiveState=active\\nLoadState=loaded\\nMainPID=0\\n' \"$2\"",
            root = root.display()
        ),
    );
    options.systemd_run_bin = logging_script(
        root,
        "systemd-run",
        "for arg in \"$@\"; do case \"$arg\" in --socket-property=ListenStream=*) \
         touch \"${arg#--socket-property=ListenStream=}\";; esac; done",
    );

    let reply = backup_reader_start(&options).await.unwrap();
    let reader_dir = run_dir.join("backup-reader");
    assert_eq!(reply["unit"], UNIT);
    assert_eq!(
        reply["socket"],
        reader_dir.join("reader.sock").display().to_string()
    );
    let token = reply["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    let token_file = reader_dir.join("reader.token");
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), token);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&token_file), 0o600);
    assert_eq!(mode(&reader_dir), 0o750);

    let runs = logged(&root.join("systemd-run.log"));
    assert_eq!(runs.len(), 1);
    let args = &runs[0];
    let has = |value: String| assert!(args.contains(&value), "missing {value}: {args:?}");
    has(format!("--unit={UNIT}"));
    has(format!(
        "--socket-property=ListenStream={}",
        reader_dir.join("reader.sock").display()
    ));
    has("--socket-property=SocketUser=root".into());
    has("--socket-property=SocketGroup=0".into());
    has("--socket-property=SocketMode=0660".into());
    has(format!(
        "--property=LoadCredential=token:{}",
        token_file.display()
    ));
    for property in [
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=read-only",
        "PrivateTmp=yes",
        "PrivateDevices=yes",
        "PrivateNetwork=yes",
        "RestrictAddressFamilies=AF_UNIX",
        "CapabilityBoundingSet=CAP_DAC_READ_SEARCH",
        "MemoryMax=256M",
        "TasksMax=16",
    ] {
        has(format!("--property={property}"));
    }
    // Nothing is writable for it, and it runs as root (no User=).
    assert!(
        !args.iter().any(|arg| arg.contains("ReadWritePaths")),
        "{args:?}"
    );
    assert!(!args.iter().any(|arg| arg.starts_with("--property=User=")));
    // The token never appears on the command line.
    assert!(!args.iter().any(|arg| arg.contains(&token)));
    let command = &args[args.iter().position(|arg| arg == "--").unwrap() + 1..];
    assert_eq!(command[1], "backup-reader");
    assert_eq!(
        command[2..],
        [
            "--homes".to_string(),
            options.homes_dir.display().to_string(),
            "--shared".to_string(),
            options.storage_dir.join("shared").display().to_string(),
            "--uid-base".to_string(),
            options.uid_base.to_string(),
            "--peer-uid".to_string(),
            euid().wrapping_add(1).to_string(),
            "--idle-seconds".to_string(),
            IDLE_SECONDS.to_string(),
        ]
    );

    // A running reader is reused with its token.
    std::fs::write(root.join("active"), b"").unwrap();
    let again = backup_reader_start(&options).await.unwrap();
    assert_eq!(again["token"], token.as_str());
    assert_eq!(logged(&root.join("systemd-run.log")).len(), 1);

    // A planted link instead of the runtime folder is refused.
    std::fs::remove_file(root.join("active")).unwrap();
    std::fs::remove_dir_all(&reader_dir).unwrap();
    std::os::unix::fs::symlink(root, &reader_dir).unwrap();
    let error = backup_reader_start(&options).await.unwrap_err();
    assert_eq!(error.code, "forbidden");
}
