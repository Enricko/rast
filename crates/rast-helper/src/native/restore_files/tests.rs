use super::*;
use crate::native::backup_reader::archive::{write_end, write_header, write_padding};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const BASE: u32 = 100_000;
const SERVICE: u32 = 990;
/// `t-x` has the user id 5: its identity is `rast-u5`.
const ME: u32 = BASE + 5;
/// `t-y` has the user id 6.
const OTHER: u32 = BASE + 6;
const SHARE_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
const RW_GID: u32 = BASE + 1_000_001;

fn host() -> HostAccounts {
    HostAccounts::parse(
        "root:x:0:0:root:/root:/bin/bash\n\
         rast:x:990:990::/var/lib/rast:/usr/sbin/nologin\n\
         rast-u5:x:100005:100005::/srv/rast/homes/t-x:/bin/bash\n\
         rast-u6:x:100006:100006::/srv/rast/homes/t-y:/bin/bash\n",
        "root:x:0:\nrast:x:990:\nrast-u5:x:100005:\nrast-u6:x:100006:\n\
         rast-s0123456789abcdef-rw:x:1100001:rast-u5\n",
    )
}

fn facts() -> Facts {
    Facts {
        accounts: HashMap::from([("t-x".to_string(), 5), ("t-y".to_string(), 6)]),
        shares: HashMap::from([("team".to_string(), SHARE_ID.to_string())]),
    }
}

fn options(target: Target, log: &OwnerLog) -> Options {
    Options {
        uid_base: BASE,
        service_uid: SERVICE,
        service_gid: SERVICE,
        trees: Vec::new(),
        target,
        owners: Owners::Record(log.clone()),
    }
}

fn meta(path: &str, kind: Kind, mode: u32, uid: u32, gid: u32) -> EntryMeta {
    EntryMeta {
        path: path.as_bytes().to_vec(),
        kind,
        mode,
        uid,
        gid,
        mtime: 1_700_000_000,
        acl_access: None,
        acl_default: None,
    }
}

/// A Backup Center archive (uncompressed), built as the backup writes it.
#[derive(Default)]
struct Archive(Vec<u8>);

impl Archive {
    fn entry(mut self, meta: EntryMeta, data: &[u8]) -> Self {
        write_header(&mut self.0, &meta).unwrap();
        self.0.extend_from_slice(data);
        write_padding(&mut self.0, data.len() as u64).unwrap();
        self
    }

    fn dir(self, path: &str, mode: u32, uid: u32, gid: u32) -> Self {
        self.entry(meta(path, Kind::Dir, mode, uid, gid), b"")
    }

    fn file(self, path: &str, mode: u32, uid: u32, gid: u32, data: &[u8]) -> Self {
        let size = data.len() as u64;
        self.entry(meta(path, Kind::File { size }, mode, uid, gid), data)
    }

    /// A file in a GNU header, as `tar::Builder::append_data` writes the
    /// files the service read (with a `L` entry first for a long path).
    fn gnu(mut self, path: &str, data: &[u8]) -> Self {
        let name = path.as_bytes();
        if name.len() >= 100 {
            let mut long = name.to_vec();
            long.push(0);
            self.0
                .extend_from_slice(&gnu_block(b"././@LongLink", b'L', 0o644, long.len() as u64));
            self.0.extend_from_slice(&long);
            write_padding(&mut self.0, long.len() as u64).unwrap();
        }
        self.0.extend_from_slice(&gnu_block(
            &name[..name.len().min(100)],
            0,
            0o600,
            data.len() as u64,
        ));
        self.0.extend_from_slice(data);
        write_padding(&mut self.0, data.len() as u64).unwrap();
        self
    }

    fn manifest(self, preserved: &[&str]) -> Self {
        let json = serde_json::json!({ "version": 1, "preservedTrees": preserved }).to_string();
        self.gnu("manifest.json", json.as_bytes())
    }

    fn raw(mut self, block: [u8; 512]) -> Self {
        self.0.extend_from_slice(&block);
        self
    }

    fn end(mut self) -> Vec<u8> {
        write_end(&mut self.0).unwrap();
        self.0
    }
}

fn checksum(block: &mut [u8; 512]) {
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
}

/// A GNU header as the `tar` crate writes it: uid and gid never set,
/// octal fields with a trailing NUL, a base-256 size from 8 GiB on.
fn gnu_block(name: &[u8], typeflag: u8, mode: u32, size: u64) -> [u8; 512] {
    let mut block = [0u8; 512];
    block[..name.len()].copy_from_slice(name);
    block[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    if size >= 1 << 33 {
        block[124] = 0x80;
        block[128..136].copy_from_slice(&size.to_be_bytes());
    } else {
        block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    }
    block[136..148].copy_from_slice(format!("{:011o}\0", 1_700_000_000u64).as_bytes());
    block[156] = typeflag;
    block[257..265].copy_from_slice(b"ustar  \0");
    checksum(&mut block);
    block
}

/// A ustar header of another entry type (a device, a FIFO).
fn ustar_block(path: &str, typeflag: u8, mode: u32, uid: u32) -> [u8; 512] {
    let mut out = Vec::new();
    write_header(&mut out, &meta(path, Kind::File { size: 0 }, mode, uid, 0)).unwrap();
    let mut block: [u8; 512] = out[..512].try_into().unwrap();
    block[156] = typeflag;
    checksum(&mut block);
    block
}

fn run(archive: &[u8], options: &Options) -> Report {
    restore(&mut &archive[..], options, &facts(), &host()).unwrap()
}

fn scratch(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("restore")
}

fn owner_of(log: &OwnerLog, path: &Path) -> (u32, u32) {
    let meta = std::fs::symlink_metadata(path).unwrap();
    *log.lock()
        .unwrap()
        .get(&(meta.dev(), meta.ino()))
        .unwrap_or_else(|| panic!("no owner recorded for {}", path.display()))
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

/// A HOME in one-HOME mode, as the backup reader stores it.
fn one_home() -> Archive {
    Archive::default()
        .dir("users/homes/t-x", 0o700, ME, ME)
        .dir("users/homes/t-x/docs", 0o750, ME, ME)
        .file("users/homes/t-x/docs/a.txt", 0o640, ME, ME, b"alpha")
        .file(
            "users/homes/t-x/notes",
            0o600,
            SERVICE,
            SERVICE,
            b"from the service",
        )
}

#[test]
fn a_one_home_home_is_restored_with_its_owners_into_a_new_folder() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = one_home()
        .entry(
            meta(
                "users/homes/t-x/docs/link",
                Kind::Symlink {
                    target: b"a.txt".to_vec(),
                },
                0o777,
                ME,
                ME,
            ),
            b"",
        )
        .entry(
            meta(
                "users/homes/t-x/docs/again",
                Kind::HardLink {
                    target: b"users/homes/t-x/docs/a.txt".to_vec(),
                },
                0o640,
                ME,
                ME,
            ),
            b"",
        )
        .gnu("users/shared/legacy/report.txt", b"legacy")
        .gnu("database/rast.db", b"not user files")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    assert!(report.ok, "{report:?}");
    assert_eq!(report.mode, "scratch");
    let home = scratch(&dir).join("homes/t-x");
    assert_eq!(owner_of(&log, &home), (ME, ME));
    assert_eq!(mode_of(&home), 0o700);
    assert_eq!(owner_of(&log, &home.join("docs")), (ME, ME));
    assert_eq!(mode_of(&home.join("docs")), 0o750);
    assert_eq!(std::fs::read(home.join("docs/a.txt")).unwrap(), b"alpha");
    assert_eq!(mode_of(&home.join("docs/a.txt")), 0o640);
    assert_eq!(owner_of(&log, &home.join("notes")), (SERVICE, SERVICE));
    assert_eq!(
        std::fs::read_link(home.join("docs/link")).unwrap(),
        Path::new("a.txt")
    );
    assert_eq!(
        std::fs::metadata(home.join("docs/again")).unwrap().ino(),
        std::fs::metadata(home.join("docs/a.txt")).unwrap().ino()
    );
    // A folder the service read: files only, all the service's.
    let legacy = scratch(&dir).join("shared/legacy");
    assert_eq!(std::fs::read(legacy.join("report.txt")).unwrap(), b"legacy");
    assert_eq!(owner_of(&log, &legacy), (SERVICE, SERVICE));
    assert_eq!(
        owner_of(&log, &legacy.join("report.txt")),
        (SERVICE, SERVICE)
    );
    assert_eq!(mode_of(&legacy), 0o700);
    // The run's folder is root's alone; nothing else was written.
    assert_eq!(mode_of(&scratch(&dir)), 0o700);
    assert!(!scratch(&dir).join("database").exists());
    let statuses: Vec<(&str, &str, &str)> = report
        .trees
        .iter()
        .map(|tree| {
            (
                tree.tree.as_str(),
                tree.layout.as_str(),
                tree.status.as_str(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            ("users/homes/t-x", "one-home", "restored"),
            ("users/shared/legacy", "legacy", "restored")
        ]
    );
}

/// Review F1: a set-uid file of root inside a HOME would be a root shell
/// for its owner after a raw `tar` restore.
#[test]
fn a_set_uid_file_of_root_in_a_home_is_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = one_home()
        .file("users/homes/t-x/sh", 0o4755, 0, 0, b"#!")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    assert!(!report.ok);
    assert_eq!(report.refused_count, 1);
    assert!(
        report.refused[0].starts_with("users/homes/t-x/sh: owned by uid 0"),
        "{:?}",
        report.refused
    );
    assert!(!scratch(&dir).exists(), "nothing stays after a refusal");
    assert_eq!(report.trees[0].status, "refused");
    assert!(describe(&report)
        .iter()
        .any(|line| line.contains("nothing was written")));
}

#[test]
fn set_id_bits_never_land_on_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = one_home()
        .file("users/homes/t-x/tool", 0o6755, ME, ME, b"#!")
        .dir("users/homes/t-x/group", 0o2770, ME, ME)
        .gnu("users/homes/legacy/run", b"#!")
        .manifest(&["users/homes/t-x"])
        .end();
    // The GNU header of the legacy file says 0600: make it set-uid.
    let mut archive = archive;
    let at = archive
        .windows(b"users/homes/legacy/run".len())
        .position(|window| window == b"users/homes/legacy/run")
        .unwrap();
    let mut block: [u8; 512] = archive[at..at + 512].try_into().unwrap();
    block[100..108].copy_from_slice(b"0004755\0");
    checksum(&mut block);
    archive[at..at + 512].copy_from_slice(&block);

    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    assert!(report.ok, "{report:?}");
    assert_eq!(report.set_id_cleared, 3);
    assert_eq!(mode_of(&scratch(&dir).join("homes/t-x/tool")), 0o755);
    assert_eq!(mode_of(&scratch(&dir).join("homes/t-x/group")), 0o770);
    assert_eq!(mode_of(&scratch(&dir).join("homes/legacy/run")), 0o755);
}

#[test]
fn devices_and_fifos_refuse_the_archive() {
    for (typeflag, what) in [
        (b'3', "character device"),
        (b'4', "block device"),
        (b'6', "FIFO"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let log = OwnerLog::default();
        let archive = one_home()
            .raw(ustar_block("users/homes/t-x/dev", typeflag, 0o666, ME))
            .manifest(&["users/homes/t-x"])
            .end();
        let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
        assert!(!report.ok, "a {what} must refuse the run");
        assert!(
            report
                .refused
                .iter()
                .any(|reason| reason.contains("is not allowed")),
            "{what}: {:?}",
            report.refused
        );
        assert!(!scratch(&dir).exists(), "{what}: nothing stays");

        let archive = Archive::default()
            .raw(gnu_block(b"users/homes/legacy/dev", typeflag, 0o666, 0))
            .manifest(&[])
            .end();
        let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
        assert!(!report.ok, "a {what} in a GNU header must refuse the run");
        assert!(!scratch(&dir).exists());
    }
}

#[test]
fn the_manifest_never_decides_which_folders_keep_owners() {
    let log = OwnerLog::default();
    // The HOME's folder belongs to the service, as in a HOME the service
    // owns: the restore would give the service its owners.
    let archive = Archive::default()
        .dir("users/homes/t-x", 0o700, SERVICE, SERVICE)
        .file("users/homes/t-x/a", 0o600, SERVICE, SERVICE, b"a")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert!(report.refused[0].contains("in one-HOME mode it belongs to its account's identity"));

    // Owners the manifest does not list.
    let archive = one_home().manifest(&[]).end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert!(report.refused[0].contains("does not list it under preservedTrees"));

    // Listed, but stored as the service read it.
    let archive = Archive::default()
        .gnu("users/homes/t-x/a", b"a")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert!(report.refused[0].contains("as the service read it"));

    // A manifest that names something else.
    let archive = one_home()
        .manifest(&["users/homes/t-x", "users/homes/\u{1b}[2J"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert_eq!(
        report.refused,
        ["manifest.json: lists 1 invalid folder names under preservedTrees"]
    );

    // No manifest: not a Backup Center archive.
    let archive = one_home().end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert!(report.refused[0].starts_with("manifest.json: missing"));

    // An account the database does not know, or one whose identity is
    // someone else on this host.
    let archive = Archive::default()
        .dir("users/homes/t-z", 0o700, BASE + 7, BASE + 7)
        .manifest(&["users/homes/t-z"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("no RAST account has this name"));
    let taken = HostAccounts::parse(
        &format!("rast:x:990:990::/:/bin/false\nmallory:x:{ME}:{ME}::/:/bin/sh\n"),
        "rast:x:990:\n",
    );
    let archive = one_home().manifest(&["users/homes/t-x"]).end();
    let report = restore(
        &mut &archive[..],
        &options(Target::DryRun, &log),
        &facts(),
        &taken,
    )
    .unwrap();
    assert!(!report.ok);
    assert!(
        report.refused[0].contains("belongs to the host account mallory"),
        "{report:?}"
    );
}

#[test]
fn entries_of_a_home_belong_to_its_account_or_the_service() {
    let log = OwnerLog::default();
    let archive = one_home()
        .file("users/homes/t-x/theirs", 0o600, OTHER, OTHER, b"x")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(!report.ok);
    assert!(report.refused[0].starts_with("users/homes/t-x/theirs: owned by uid 100006"));

    // A group outside the layout becomes the owner's own; ACL entries for
    // accounts outside the layout are dropped.
    let dir = tempfile::tempdir().unwrap();
    let mut shared_file = meta(
        "users/homes/t-x/shared.txt",
        Kind::File { size: 1 },
        0o640,
        ME,
        0,
    );
    shared_file.acl_access = Some(Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 6,
        },
        Entry {
            tag: Tag::User(OTHER),
            perm: 4,
        },
        Entry {
            tag: Tag::User(4242),
            perm: 6,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 4,
        },
        Entry {
            tag: Tag::Mask,
            perm: 6,
        },
        Entry {
            tag: Tag::Other,
            perm: 0,
        },
    ]));
    let archive = one_home()
        .entry(shared_file, b"s")
        .manifest(&["users/homes/t-x"])
        .end();
    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    if !report.ok && report.refused.iter().any(|r| r.contains("ACL")) {
        eprintln!("skipped: the temporary file system has no ACLs");
        return;
    }
    assert!(report.ok, "{report:?}");
    assert_eq!(report.groups_changed, 1);
    assert_eq!(report.acl_entries_dropped, 1);
    let path = scratch(&dir).join("homes/t-x/shared.txt");
    assert_eq!(owner_of(&log, &path), (ME, ME));
    let file = std::fs::File::open(&path).unwrap();
    let acl = acl::read(file.as_raw_fd(), acl::ACCESS_XATTR)
        .unwrap()
        .unwrap();
    let named: Vec<u32> = acl.named_users().map(|(uid, _)| uid).collect();
    assert_eq!(named, [OTHER]);
}

/// A migrated shared folder, as the backup reader stores it.
fn group_share() -> Archive {
    Archive::default()
        .dir("users/shared/team", 0o2770, 0, RW_GID)
        .dir("users/shared/team/plans", 0o2770, ME, RW_GID)
        .file("users/shared/team/plans/q1.txt", 0o4660, ME, RW_GID, b"q1")
        .file(
            "users/shared/team/by-service",
            0o640,
            SERVICE,
            SERVICE,
            b"s",
        )
}

#[test]
fn a_migrated_shared_folder_keeps_its_group_layout() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = group_share().manifest(&["users/shared/team"]).end();
    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    assert!(report.ok, "{report:?}");
    let team = scratch(&dir).join("shared/team");
    assert_eq!(owner_of(&log, &team), (0, RW_GID));
    assert_eq!(mode_of(&team), 0o2770);
    assert_eq!(owner_of(&log, &team.join("plans")), (ME, RW_GID));
    assert_eq!(
        mode_of(&team.join("plans")),
        0o2770,
        "set-gid stays on folders"
    );
    assert_eq!(mode_of(&team.join("plans/q1.txt")), 0o660, "never on files");
    assert_eq!(owner_of(&log, &team.join("by-service")), (SERVICE, SERVICE));
    assert_eq!(report.set_id_cleared, 1);
    assert_eq!(report.trees[0].layout, "group");

    // Root owns only the container itself.
    let archive = group_share()
        .file("users/shared/team/root", 0o644, 0, 0, b"r")
        .manifest(&["users/shared/team"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("owned by root below the top"));

    // An owner that is no RAST identity.
    let archive = group_share()
        .file("users/shared/team/x", 0o644, 1000, 1000, b"x")
        .manifest(&["users/shared/team"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("neither the service nor the identity"));
    // The identity of a removed account: the service's. One that another
    // host account holds is refused.
    let dir = tempfile::tempdir().unwrap();
    let archive = group_share()
        .file("users/shared/team/gone", 0o644, BASE + 9, RW_GID, b"g")
        .manifest(&["users/shared/team"])
        .end();
    let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
    assert!(report.ok, "{report:?}");
    assert_eq!(report.owners_changed, 1);
    assert_eq!(
        owner_of(&log, &scratch(&dir).join("shared/team/gone")),
        (SERVICE, SERVICE)
    );
    let taken = HostAccounts::parse(
        &format!("rast:x:990:990::/:/bin/false\nrast-u5:x:{ME}:{ME}::/:/bin/sh\nmallory:x:{0}:{0}::/:/bin/sh\n", BASE + 9),
        &format!("rast-s0123456789abcdef-rw:x:{RW_GID}:\n"),
    );
    let report = restore(
        &mut &archive[..],
        &options(Target::DryRun, &log),
        &facts(),
        &taken,
    )
    .unwrap();
    assert!(
        report.refused[0].contains("neither the service nor the identity"),
        "{report:?}"
    );

    // A container whose group is not its share's.
    let archive = Archive::default()
        .dir("users/shared/team", 0o2770, 0, 0)
        .manifest(&["users/shared/team"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("is not a shared folder group"));
    let archive = Archive::default()
        .dir("users/shared/team", 0o2770, 0, RW_GID + 1)
        .manifest(&["users/shared/team"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("rast-s0123456789abcdef-rw has gid 1100001"));
    // A container the database does not know.
    let archive = Archive::default()
        .dir("users/shared/other", 0o2770, 0, RW_GID)
        .manifest(&["users/shared/other"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("no shared folder has this container"));
}

#[test]
fn no_entry_passes_through_a_link_or_leaves_its_folder() {
    let log = OwnerLog::default();
    let cases: [(Archive, &str); 4] = [
        (
            one_home()
                .entry(
                    meta(
                        "users/homes/t-x/etc",
                        Kind::Symlink {
                            target: b"/etc".to_vec(),
                        },
                        0o777,
                        ME,
                        ME,
                    ),
                    b"",
                )
                .file("users/homes/t-x/etc/passwd", 0o644, ME, ME, b"root::0:0"),
            "its folder is a file or a symbolic link",
        ),
        (
            one_home().entry(
                meta(
                    "users/homes/t-x/shadow",
                    Kind::HardLink {
                        target: b"users/homes/t-y/secret".to_vec(),
                    },
                    0o600,
                    ME,
                    ME,
                ),
                b"",
            ),
            "outside its folder",
        ),
        (
            one_home().file("users/homes/t-x/missing/a", 0o600, ME, ME, b"a"),
            "its folder is not in the archive before it",
        ),
        (
            one_home().file("users/homes/t-x/docs/a.txt", 0o600, ME, ME, b"again"),
            "appears twice",
        ),
    ];
    for (archive, reason) in cases {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive.manifest(&["users/homes/t-x"]).end();
        let report = run(&archive, &options(Target::Scratch(scratch(&dir)), &log));
        assert!(!report.ok, "{reason}");
        assert!(
            report.refused.iter().any(|text| text.contains(reason)),
            "{reason}: {:?}",
            report.refused
        );
        assert!(!scratch(&dir).exists(), "{reason}: nothing stays");
    }
    // `..` never gets past the codec.
    let archive = Archive::default()
        .raw(gnu_block(
            b"users/homes/legacy/../../../etc/cron.d/x",
            0,
            0o644,
            0,
        ))
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.refused[0].contains("invalid entry path"));
}

#[test]
fn the_target_is_a_new_folder_reached_without_links() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = one_home().manifest(&["users/homes/t-x"]).end();
    std::fs::create_dir(scratch(&dir)).unwrap();
    let error = restore(
        &mut &archive[..],
        &options(Target::Scratch(scratch(&dir)), &log),
        &facts(),
        &host(),
    )
    .unwrap_err();
    assert!(error.contains("exists"), "{error}");

    std::os::unix::fs::symlink(dir.path(), dir.path().join("link")).unwrap();
    let error = restore(
        &mut &archive[..],
        &options(Target::Scratch(dir.path().join("link/new")), &log),
        &facts(),
        &host(),
    )
    .unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    assert!(!dir.path().join("new").exists());
}

#[test]
fn a_dry_run_writes_nothing_and_a_selection_restores_only_its_folders() {
    let dir = tempfile::tempdir().unwrap();
    let log = OwnerLog::default();
    let archive = one_home()
        .gnu("users/homes/legacy/a", b"a")
        .manifest(&["users/homes/t-x", "users/shared/broken"])
        .end();
    let report = run(&archive, &options(Target::DryRun, &log));
    assert!(report.ok, "{report:?}");
    assert!(report.trees.iter().all(|tree| tree.status == "checked"));
    assert_eq!(
        report.notes.len(),
        1,
        "a listed folder without entries is noted"
    );
    assert!(log.lock().unwrap().is_empty());

    let mut only = options(Target::Scratch(scratch(&dir)), &log);
    only.trees = vec!["users/homes/legacy".into()];
    let report = run(&archive, &only);
    assert!(report.ok, "{report:?}");
    assert_eq!(report.trees.len(), 1);
    assert!(scratch(&dir).join("homes/legacy/a").exists());
    assert!(!scratch(&dir).join("homes/t-x").exists());

    let mut missing = options(Target::DryRun, &log);
    missing.trees = vec!["users/homes/nobody".into()];
    let report = run(&archive, &missing);
    assert!(!report.ok);
    assert_eq!(report.refused, ["users/homes/nobody: not in the archive"]);
    assert!(parse_tree("users/homes/../x").is_err());
    assert!(parse_tree("users/other/x").is_err());
    assert!(parse_tree("users/shared/team").is_ok());
}

#[test]
fn the_live_storage_is_filled_but_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(storage.join("homes/legacy")).unwrap();
    std::fs::write(storage.join("homes/legacy/keep"), b"live").unwrap();
    std::fs::create_dir_all(storage.join("shared")).unwrap();
    let log = OwnerLog::default();
    let archive = one_home()
        .gnu("users/homes/legacy/keep", b"from the backup")
        .manifest(&["users/homes/t-x"])
        .end();
    let target = Target::Storage {
        storage: storage.clone(),
        staging: ".restore-test".into(),
    };
    let report = run(&archive, &options(target, &log));
    assert!(report.ok, "{report:?}");
    assert_eq!(report.target, None, "the staging folder is gone");
    assert!(!storage.join(".restore-test").exists());
    assert_eq!(
        std::fs::read(storage.join("homes/t-x/docs/a.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(owner_of(&log, &storage.join("homes/t-x")), (ME, ME));
    assert_eq!(
        std::fs::read(storage.join("homes/legacy/keep")).unwrap(),
        b"live",
        "an existing folder is never replaced"
    );
    let legacy = report
        .trees
        .iter()
        .find(|tree| tree.tree == "users/homes/legacy")
        .unwrap();
    assert_eq!(legacy.status, "exists");
    assert!(describe(&report).iter().any(
        |line| line.starts_with("users/homes/legacy: exists") && line.contains("move it aside")
    ));
}

#[test]
fn backup_archives_read_gnu_files_and_long_names() {
    let long = format!("users/homes/legacy/{}/file.txt", "d".repeat(120));
    let mut archive = Archive::default().gnu(&long, b"long").0;
    // A file of 9 GiB: its size is base-256.
    archive.extend_from_slice(&gnu_block(b"big", 0, 0o600, 9 << 30));
    let mut input = &archive[..];
    let Next::Entry(first) = archive::read_backup_entry(&mut input).unwrap() else {
        panic!("an entry");
    };
    assert_eq!(first.path, long.as_bytes());
    assert_eq!(first.kind, Kind::File { size: 4 });
    assert_eq!((first.uid, first.gid, first.mode), (0, 0, 0o600));
    let mut data = [0u8; 4];
    archive::read_exact(&mut input, &mut data).unwrap();
    archive::read_padding(&mut input, 4).unwrap();
    let Next::Entry(big) = archive::read_backup_entry(&mut input).unwrap() else {
        panic!("an entry");
    };
    assert_eq!(big.kind, Kind::File { size: 9 << 30 });

    // GNU links, an owner name, a long name without its entry.
    for block in [
        gnu_block(b"users/homes/legacy/l", b'2', 0o777, 0),
        gnu_block(b"users/homes/legacy/l", b'1', 0o644, 0),
        {
            let mut block = gnu_block(b"users/homes/legacy/a", 0, 0o644, 0);
            block[265..269].copy_from_slice(b"root");
            checksum(&mut block);
            block
        },
    ] {
        assert!(archive::read_backup_entry(&mut &block[..]).is_err());
    }
    let mut lone = gnu_block(b"././@LongLink", b'L', 0o644, 4).to_vec();
    lone.extend_from_slice(&[b'a', b'b', b'c', 0]);
    lone.resize(1024, 0);
    lone.extend_from_slice(&ustar_block("x", b'0', 0o644, 0));
    assert!(archive::read_backup_entry(&mut &lone[..]).is_err());
}

#[test]
fn host_accounts_and_reports_are_read_strictly() {
    let accounts = host();
    assert_eq!(accounts.user("rast"), Some((990, 990)));
    assert_eq!(accounts.user("rast-u5"), Some((ME, ME)));
    assert_eq!(accounts.user("nobody"), None);
    let parsed = HostAccounts::parse("bad line\nx:x:notanumber:1::/:/\n", "");
    assert_eq!(parsed.user("x"), None);

    let log = OwnerLog::default();
    let archive = one_home().manifest(&["users/homes/t-x"]).end();
    let report = run(&archive, &options(Target::DryRun, &log));
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["mode"], "dry-run");
    assert_eq!(json["trees"][0]["layout"], "one-home");
    assert_eq!(json["refusedCount"], 0);
    assert!(describe(&report)
        .last()
        .unwrap()
        .contains("every entry passed the checks"));
}

#[test]
fn the_service_lock_is_taken_but_never_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rast.lock");
    assert!(matches!(
        take_service_lock(&path).unwrap(),
        ServiceLock::Absent
    ));
    assert!(!path.exists(), "never created");
    std::fs::write(&path, b"").unwrap();
    let held = take_service_lock(&path).unwrap();
    assert!(matches!(held, ServiceLock::Held(_)));
    let other = std::fs::File::open(&path).unwrap();
    assert!(
        other.try_lock().is_err(),
        "held for the length of the restore"
    );
    drop(held);
    other.try_lock().unwrap();
    assert!(matches!(
        take_service_lock(&path).unwrap(),
        ServiceLock::Busy
    ));
    drop(other);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &path).unwrap();
    assert!(take_service_lock(&path).is_err(), "never through a link");
}
