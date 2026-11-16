use super::*;

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;

const SHARE: &str = "0123abcd-4567-4ef0-8123-456789abcdef";
const RW: &str = "rast-s0123abcd45674ef0-rw";
const RO: &str = "rast-s0123abcd45674ef0-ro";

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Fixture {
    dir: tempfile::TempDir,
    options: NativeOptions,
}

impl Fixture {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn shared(&self) -> PathBuf {
        self.options.storage_dir.join("shared")
    }

    fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.root().join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn groups(&self) -> String {
        std::fs::read_to_string(self.root().join("group.db")).unwrap()
    }

    fn add_group(&self, line: &str) {
        let mut text = self.groups();
        text.push_str(line);
        text.push('\n');
        std::fs::write(self.root().join("group.db"), text).unwrap();
    }

    fn add_identity(&self, user_id: i64, uid: u32) {
        let mut text = std::fs::read_to_string(self.root().join("passwd.db")).unwrap();
        text.push_str(&format!(
            "rast-u{user_id}:x:{uid}:{uid}:RAST Linux app:/nonexistent:/usr/sbin/nologin\n"
        ));
        std::fs::write(self.root().join("passwd.db"), text).unwrap();
    }

    async fn ensure_groups(&self) {
        share_group_ensure(
            &self.options,
            ShareGroupOp {
                share_id: SHARE.into(),
                container: None,
            },
        )
        .await
        .unwrap();
    }
}

/// Helper options whose account tools work on `group.db` and `passwd.db`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let data = root.join("data");
    let run = root.join("run");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let mut options = NativeOptions::for_test(root.join("native.sock"), data, run);
    options.storage_dir = root.join("storage");
    std::fs::create_dir_all(options.storage_dir.join("shared")).unwrap();
    std::fs::write(root.join("group.db"), "").unwrap();
    std::fs::write(root.join("passwd.db"), "").unwrap();
    let group = root.join("group.db");
    let passwd = root.join("passwd.db");
    let log = |name: &str| {
        format!(
            "printf '%s\\n' \"$*\" >> '{}'\n",
            root.join(format!("{name}.log")).display()
        )
    };
    options.getent_bin = write_script(
        root,
        "getent",
        &format!(
            "#!/bin/sh\ncase \"$1\" in group) f='{group}';; passwd) f='{passwd}';; *) exit 1;; esac\n\
             shift\nmissing=0\nfor key in \"$@\"; do\n\
             line=$(awk -F: -v k=\"$key\" '$1==k || $3==k {{print; exit}}' \"$f\")\n\
             if [ -n \"$line\" ]; then echo \"$line\"; else missing=1; fi\ndone\n\
             [ $missing = 1 ] && exit 2\nexit 0\n",
            group = group.display(),
            passwd = passwd.display()
        ),
    );
    options.groupadd_bin = write_script(
        root,
        "groupadd",
        &format!(
            "#!/bin/sh\n{log}min=0; max=0; gid=''\n\
             while [ $# -gt 1 ]; do case \"$1\" in\n\
             -K) case \"$2\" in GID_MIN=*) min=${{2#GID_MIN=}};; GID_MAX=*) max=${{2#GID_MAX=}};; esac; shift 2;;\n\
             -g) gid=$2; shift 2;;\n*) shift;; esac; done\nname=$1\n\
             if [ -z \"$gid\" ]; then\n\
             gid=$(awk -F: -v a=\"$min\" -v b=\"$max\" 'BEGIN{{m=a-1}} $3>=a && $3<=b && $3>m {{m=$3}} END{{print m+1}}' '{group}')\nfi\n\
             echo \"$name:x:$gid:\" >> '{group}'\n",
            log = log("groupadd"),
            group = group.display()
        ),
    );
    options.gpasswd_bin = write_script(
        root,
        "gpasswd",
        &format!(
            "#!/bin/sh\n{log}[ \"$1\" = -M ] || exit 1\n\
             awk -F: -v OFS=: -v n=\"$3\" -v m=\"$2\" '$1==n {{$4=m}} {{print}}' '{group}' > '{group}.new'\n\
             mv '{group}.new' '{group}'\n",
            log = log("gpasswd"),
            group = group.display()
        ),
    );
    options.groupdel_bin = write_script(
        root,
        "groupdel",
        &format!(
            "#!/bin/sh\n{log}awk -F: -v n=\"$1\" '$1!=n' '{group}' > '{group}.new'\n\
             mv '{group}.new' '{group}'\n",
            log = log("groupdel"),
            group = group.display()
        ),
    );
    Fixture { dir, options }
}

/// Whether the file system of `dir` stores POSIX ACLs (probed on a new
/// folder next to it, so `dir` keeps its mode).
fn acls_supported(dir: &Path) -> bool {
    let probe = dir.parent().unwrap().join(".acl-probe");
    std::fs::create_dir_all(&probe).unwrap();
    let file = std::fs::File::open(&probe).unwrap();
    let acl = Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 7,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 5,
        },
        Entry {
            tag: Tag::Other,
            perm: 5,
        },
    ]);
    let supported = acl::write(file.as_raw_fd(), acl::ACCESS_XATTR, &acl).is_ok();
    drop(file);
    std::fs::remove_dir(&probe).unwrap();
    supported
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

fn read_acl(path: &Path, xattr: &str) -> Option<Acl> {
    let file = std::fs::File::open(path).unwrap();
    acl::read(file.as_raw_fd(), xattr).unwrap()
}

#[test]
fn names_ranges_and_arguments_are_fixed_by_the_share_id() {
    let groups = ShareGroups::for_share(SHARE).unwrap();
    assert_eq!((groups.rw.as_str(), groups.ro.as_str()), (RW, RO));
    assert_eq!(groups.rw.len(), 25);
    for bad in [
        "",
        "0123ABCD-4567-4ef0-8123-456789abcdef",
        "0123abcd-4567-1ef0-8123-456789abcdef",
        "0123abcd-4567-4ef0-c123-456789abcdef",
        "0123abcd45674ef08123456789abcdef",
        "../../etc/passwd",
    ] {
        assert!(ShareGroups::for_share(bad).is_err(), "{bad}");
    }
    let range = share_gid_range(200_000).unwrap();
    assert_eq!(range, 1_200_001..=2_199_999);
    // Identities (base + 1 ..= base + 1 000 000) never reach the range.
    assert!(*identity_uid_range(200_000).end() < *range.start());
    assert!(share_gid_range(u32::MAX - 10).is_err());
    let args: Vec<String> = groupadd_share_args(RW, &range, Some(1_200_009))
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect();
    assert_eq!(
        args,
        [
            "-K",
            "GID_MIN=1200001",
            "-K",
            "GID_MAX=2199999",
            "-g",
            "1200009",
            RW
        ]
    );
    let entry = parse_group_line("rast-s1-rw:x:1200001:rast-u1,rast-u2").unwrap();
    assert_eq!(entry.gid, 1_200_001);
    assert_eq!(entry.members, ["rast-u1", "rast-u2"]);
    assert!(parse_group_line("rast-s1-rw:x:1200001:")
        .unwrap()
        .members
        .is_empty());
    assert!(parse_group_line("broken").is_err());
    assert_eq!(user_id_of("rast-u42"), Some(42));
    for bad in ["rast-u0", "rast-u042", "rast-u", "alice", "rast-u1x"] {
        assert_eq!(user_id_of(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn groups_are_created_once_with_gids_from_the_range() {
    let fx = fixture();
    let value = share_group_ensure(
        &fx.options,
        ShareGroupOp {
            share_id: SHARE.into(),
            container: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(value["rw"]["name"], RW);
    assert_eq!(value["rw"]["gid"], 1_200_001);
    assert_eq!(value["ro"]["gid"], 1_200_002);
    assert_eq!(value["rw"]["created"], true);
    let again = share_group_ensure(
        &fx.options,
        ShareGroupOp {
            share_id: SHARE.into(),
            container: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(again["rw"]["created"], false);
    assert_eq!(again["ro"]["gid"], 1_200_002);
    assert_eq!(fx.log("groupadd").len(), 2);

    // Removal deletes both, and only groups in the range.
    let removed = share_group_remove(
        &fx.options,
        ShareIdOp {
            share_id: SHARE.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(removed["removed"].as_array().unwrap().len(), 2);
    assert!(fx.groups().trim().is_empty());
    let none = share_group_remove(
        &fx.options,
        ShareIdOp {
            share_id: SHARE.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(none["removed"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_group_with_the_name_outside_the_range_is_a_conflict() {
    let fx = fixture();
    fx.add_group(&format!("{RW}:x:1000:"));
    let error = share_group_ensure(
        &fx.options,
        ShareGroupOp {
            share_id: SHARE.into(),
            container: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "identity_conflict");
    let error = share_group_remove(
        &fx.options,
        ShareIdOp {
            share_id: SHARE.into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "identity_conflict");
    assert!(fx.log("groupdel").is_empty());
}

#[tokio::test]
async fn a_restored_container_gets_its_read_only_gid_back() {
    let fx = fixture();
    let container = fx.shared().join("team");
    std::fs::create_dir(&container).unwrap();
    if !acls_supported(&container) {
        return;
    }
    let file = std::fs::File::open(&container).unwrap();
    let acl = Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 7,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 7,
        },
        Entry {
            tag: Tag::Group(1_200_050),
            perm: 5,
        },
        Entry {
            tag: Tag::Mask,
            perm: 7,
        },
        Entry {
            tag: Tag::Other,
            perm: 0,
        },
    ]);
    acl::write(file.as_raw_fd(), acl::ACCESS_XATTR, &acl).unwrap();
    let value = share_group_ensure(
        &fx.options,
        ShareGroupOp {
            share_id: SHARE.into(),
            container: Some("team".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(value["ro"]["gid"], 1_200_050);
    assert!(fx.log("groupadd")[1].contains("-g 1200050"));
}

#[tokio::test]
async fn members_replace_both_groups_and_report_what_changed() {
    let fx = fixture();
    fx.ensure_groups().await;
    for id in [1, 2, 3] {
        fx.add_identity(id, 200_000 + id as u32);
    }
    let op = |write: Vec<i64>, read: Vec<i64>| ShareMembersSetOp {
        share_id: SHARE.into(),
        write,
        read,
    };
    let value = share_members_set(&fx.options, op(vec![1, 2], vec![3]))
        .await
        .unwrap();
    assert_eq!(value["write"]["added"], json!([1, 2]));
    assert_eq!(value["read"]["added"], json!([3]));
    assert!(fx
        .groups()
        .contains(&format!("{RW}:x:1200001:rast-u1,rast-u2")));
    assert!(fx.groups().contains(&format!("{RO}:x:1200002:rast-u3")));
    assert_eq!(fx.log("gpasswd").len(), 2);

    // Nothing changes: no gpasswd call.
    share_members_set(&fx.options, op(vec![1, 2], vec![3]))
        .await
        .unwrap();
    assert_eq!(fx.log("gpasswd").len(), 2);

    // User 2 loses write access and keeps read access; a member that is
    // not a RAST identity is removed too.
    let text = fx.groups().replace("rast-u3", "rast-u3,mallory");
    std::fs::write(fx.root().join("group.db"), text).unwrap();
    let value = share_members_set(&fx.options, op(vec![1], vec![2, 3]))
        .await
        .unwrap();
    assert_eq!(value["write"]["removed"], json!([2]));
    assert_eq!(value["read"]["added"], json!([2]));
    assert_eq!(value["read"]["removedOther"], 1);
    assert!(fx
        .groups()
        .contains(&format!("{RO}:x:1200002:rast-u2,rast-u3\n")));

    // Everyone out: an empty list.
    share_members_set(&fx.options, op(vec![], vec![]))
        .await
        .unwrap();
    assert!(fx.groups().contains(&format!("{RW}:x:1200001:\n")));
    assert_eq!(fx.log("gpasswd").last().unwrap(), &format!("-M  {RO}"));
}

#[tokio::test]
async fn member_lists_are_validated_before_anything_changes() {
    let fx = fixture();
    fx.ensure_groups().await;
    fx.add_identity(1, 200_001);
    fx.add_identity(2, 999);
    let too_many: Vec<i64> = (1..=MAX_SHARE_MEMBERS as i64 + 1).collect();
    for (write, read, code) in [
        (vec![1, 1], vec![], "invalid_request"),
        (vec![1], vec![1], "invalid_request"),
        (vec![0], vec![], "invalid_request"),
        (vec![MAX_USER_ID + 1], vec![], "invalid_request"),
        (too_many, vec![], "invalid_request"),
    ] {
        let error = share_members_set(
            &fx.options,
            ShareMembersSetOp {
                share_id: SHARE.into(),
                write: write.clone(),
                read,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, code, "{:?}", write.len());
    }
    assert!(fx.log("gpasswd").is_empty());
    // Groups must exist first.
    let other = fixture();
    other.add_identity(1, 200_001);
    let error = share_members_set(
        &other.options,
        ShareMembersSetOp {
            share_id: SHARE.into(),
            write: vec![1],
            read: vec![],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "not_found");
}

#[tokio::test]
async fn users_without_their_linux_account_are_left_out_and_reported() {
    let fx = fixture();
    fx.ensure_groups().await;
    fx.add_identity(1, 200_001);
    let value = share_members_set(
        &fx.options,
        ShareMembersSetOp {
            share_id: SHARE.into(),
            write: vec![1, 4],
            read: vec![5],
        },
    )
    .await
    .unwrap();
    assert_eq!(value["skipped"], json!([4, 5]));
    assert_eq!(value["write"]["added"], json!([1]));
    assert_eq!(value["read"]["added"], json!([]));
    assert!(fx.groups().contains(&format!("{RW}:x:1200001:rast-u1\n")));
    assert!(fx.groups().contains(&format!("{RO}:x:1200002:\n")));
}

#[tokio::test]
async fn accounts_with_an_unexpected_uid_are_never_added_and_always_removed() {
    let fx = fixture();
    fx.ensure_groups().await;
    fx.add_identity(1, 200_001);
    fx.add_identity(3, 200_003);
    // `rast-u2` exists with someone else's uid, and is already a member of
    // the read-only group (added by hand, or before the uid changed).
    fx.add_identity(2, 999);
    let text = fx.groups().replace(
        &format!("{RO}:x:1200002:"),
        &format!("{RO}:x:1200002:rast-u2"),
    );
    std::fs::write(fx.root().join("group.db"), text).unwrap();
    let op = |write: Vec<i64>, read: Vec<i64>| ShareMembersSetOp {
        share_id: SHARE.into(),
        write,
        read,
    };

    // Granting it write access: the others still change, it is left out
    // and taken out of the group it was in.
    let value = share_members_set(&fx.options, op(vec![1, 2], vec![3]))
        .await
        .unwrap();
    assert_eq!(
        value["conflicts"],
        json!([{"userId": 2, "uid": 999, "expectedUid": 200_002}])
    );
    assert_eq!(value["write"]["added"], json!([1]));
    assert_eq!(value["read"]["added"], json!([3]));
    assert_eq!(value["read"]["removed"], json!([2]));
    assert!(fx.groups().contains(&format!("{RW}:x:1200001:rast-u1\n")));
    assert!(fx.groups().contains(&format!("{RO}:x:1200002:rast-u3\n")));

    // Revoking everyone still works while the conflict lasts.
    let value = share_members_set(&fx.options, op(vec![], vec![2]))
        .await
        .unwrap();
    assert_eq!(value["write"]["removed"], json!([1]));
    assert_eq!(value["read"]["removed"], json!([3]));
    assert!(fx.groups().contains(&format!("{RW}:x:1200001:\n")));
    assert!(fx.groups().contains(&format!("{RO}:x:1200002:\n")));

    // The same check for `rast doctor`.
    let conflicts = identity_conflicts(&fx.options, &BTreeSet::from([1, 2, 3, 4]))
        .await
        .unwrap();
    assert_eq!(
        conflicts,
        vec![IdentityConflict {
            user_id: 2,
            uid: 999,
            expected_uid: 200_002
        }]
    );
    let report =
        crate::native::doctor_identities_with(&fx.options, &BTreeSet::from([1, 2, 3])).await;
    assert_eq!(report.warnings, 1);
    assert!(
        report.lines[0].starts_with(
            "[WARN] the account rast-u2 has uid 999, but RAST expects uid 200002 for user 2"
        ),
        "{:?}",
        report.lines
    );
    let report = crate::native::doctor_identities_with(&fx.options, &BTreeSet::from([1, 3])).await;
    assert_eq!(report.warnings, 0);
    assert_eq!(
        report.lines,
        vec!["[    ] Linux identities ... ok (2 users checked)"]
    );
}

/// A legacy container with folders, files, a link, a hard-linked file and a
/// set-uid file.
fn legacy_tree(fx: &Fixture) -> PathBuf {
    let team = fx.shared().join("team");
    std::fs::create_dir_all(team.join("docs/deep")).unwrap();
    std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::write(team.join("docs/plan.txt"), b"plan").unwrap();
    std::fs::set_permissions(
        team.join("docs/plan.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::write(team.join("docs/deep/run.sh"), b"#!/bin/sh").unwrap();
    std::fs::set_permissions(
        team.join("docs/deep/run.sh"),
        std::fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    std::fs::write(fx.root().join("outside.txt"), b"outside").unwrap();
    std::fs::set_permissions(
        fx.root().join("outside.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    symlink(fx.root().join("outside.txt"), team.join("docs/link")).unwrap();
    std::fs::write(team.join("hard.txt"), b"h").unwrap();
    std::fs::hard_link(team.join("hard.txt"), fx.root().join("hard-elsewhere.txt")).unwrap();
    team
}

#[tokio::test]
async fn preparing_a_share_applies_the_group_layout_without_following_links() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    // Only the container.
    let value = share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(value["report"]["changes"]["count"], 1);
    assert_eq!(mode(&team), 0o2770);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o644);
    let rast = journal_owner();
    let expected = Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 7,
        },
        Entry {
            tag: Tag::User(rast),
            perm: 7,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 7,
        },
        Entry {
            tag: Tag::Group(1_200_002),
            perm: 5,
        },
        Entry {
            tag: Tag::Mask,
            perm: 7,
        },
        Entry {
            tag: Tag::Other,
            perm: 0,
        },
    ]);
    assert_eq!(read_acl(&team, acl::ACCESS_XATTR), Some(expected.clone()));
    assert_eq!(read_acl(&team, acl::DEFAULT_XATTR), Some(expected.clone()));

    // Everything in it.
    let value = share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: true,
        },
    )
    .await
    .unwrap();
    let report = &value["report"];
    assert_eq!(report["changes"]["count"], 4, "{report}");
    assert_eq!(report["symlinks"]["paths"], json!(["docs/link"]));
    assert_eq!(report["hardLinks"]["paths"], json!(["hard.txt"]));
    assert_eq!(report["containerPrepared"], true);
    assert_eq!(mode(&team.join("docs")), 0o2770);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o660);
    // Set-uid is dropped; executables stay executable for the group.
    assert_eq!(mode(&team.join("docs/deep/run.sh")), 0o770);
    let file_acl = read_acl(&team.join("docs/plan.txt"), acl::ACCESS_XATTR).unwrap();
    assert_eq!(
        file_acl.describe(),
        format!("u::rw-,u:{rast}:rw-,g::rw-,g:1200002:r--,m::rw-,o::---")
    );
    assert_eq!(
        read_acl(&team.join("docs/deep"), acl::DEFAULT_XATTR),
        Some(expected)
    );
    // The link, its target and the hard-linked file are unchanged.
    assert_eq!(mode(&fx.root().join("outside.txt")), 0o600);
    assert_eq!(
        read_acl(&fx.root().join("outside.txt"), acl::ACCESS_XATTR),
        None
    );
    assert_eq!(read_acl(&team.join("hard.txt"), acl::ACCESS_XATTR), None);

    // Again: nothing left to change.
    let again = share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(again["report"]["changes"]["count"], 0);
}

#[tokio::test]
async fn entries_of_other_accounts_are_never_changed() {
    // A member can hard-link a file of another account into a share (any
    // file they can reach, with `fs.protected_hardlinks = 0`). Once the
    // original name is gone the file has one link, and root must still not
    // give it the share's group, bits and ACL. Tests cannot chown, so the
    // test user plays that other account: the layout names another uid as
    // the `rast` service.
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    let mut layout = Layout::for_share(&fx.options, SHARE).await.unwrap();
    layout.rast_uid = u32::MAX - 20;
    assert!(!layout.owners().managed(journal_owner()));
    let docs_mode = mode(&team.join("docs"));
    let (fd, stat) = open_container(&fx.options, "team").unwrap();
    let report = prepare_tree(&layout, &fd, &stat, true, true).unwrap();
    let mut other = report.other_owner.paths.clone();
    other.sort();
    assert_eq!(
        other,
        ["docs", "docs/deep", "docs/deep/run.sh", "docs/plan.txt"]
    );
    assert_eq!(report.hard_links.paths, ["hard.txt"]);
    assert_eq!(report.changes.paths, ["."], "only the container");
    assert_eq!(mode(&team.join("docs")), docs_mode);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o644);
    assert_eq!(mode(&team.join("docs/deep/run.sh")), 0o4755);
    for path in ["docs", "docs/plan.txt", "docs/deep/run.sh"] {
        assert_eq!(
            read_acl(&team.join(path), acl::ACCESS_XATTR),
            None,
            "{path}"
        );
    }
    assert_eq!(read_acl(&team.join("docs"), acl::DEFAULT_XATTR), None);

    // The rollback walk leaves them alone too.
    let mut visited = Vec::new();
    let mut walked = TreeReport::default();
    walk(&fd, &layout.owners(), &mut walked, |found, _| {
        visited.push(found.path.to_string());
        Ok(())
    })
    .unwrap();
    assert!(visited.is_empty(), "{visited:?}");
    assert_eq!(walked.other_owner.count, 4);
}

#[test]
fn a_folder_with_too_many_names_is_refused_before_they_are_all_read() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big");
    std::fs::create_dir_all(big.join("sub")).unwrap();
    for index in 0..20 {
        std::fs::write(big.join(format!("f{index}")), b"").unwrap();
    }
    for index in 0..5 {
        std::fs::write(big.join(format!("sub/g{index}")), b"").unwrap();
    }
    let fd = open_dir(&big).unwrap();

    // One folder: never more names in memory than the limit.
    assert_eq!(dir_names(fd.as_raw_fd(), 21).unwrap().unwrap().len(), 21);
    assert!(dir_names(fd.as_raw_fd(), 20).unwrap().is_none());

    // A walk counts the names it has read in every open folder.
    let owners = Owners {
        root_uid: u32::MAX - 15,
        rast_uid: journal_owner(),
        identity_uids: (1, 0),
    };
    let mount = fd_mount_id(fd.as_raw_fd()).unwrap();
    let walk_with = |limit: u64| {
        let mut report = TreeReport::default();
        walk_limited(&fd, mount, &owners, limit, &mut report, |_, _| Ok(()))
    };
    assert!(walk_with(26).is_ok());
    let error = walk_with(25).unwrap_err();
    assert_eq!(error.code, "forbidden");
    assert!(error.message.contains("more than 25 entries"), "{error:?}");
    // Refused before the walk got anywhere: the top folder alone is over.
    let mut visited = 0;
    let mut report = TreeReport::default();
    let error = walk_limited(&fd, mount, &owners, 10, &mut report, |_, _| {
        visited += 1;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code, "forbidden");
    assert_eq!(visited, 0);
}

#[test]
fn the_walk_leaves_the_recycle_bins_of_members_alone() {
    let dir = tempfile::tempdir().unwrap();
    let me = journal_owner();
    let top = dir.path().join("team");
    let mine = rast_base::storage::xdg_trash::share_trash_name(me);
    let other = rast_base::storage::xdg_trash::share_trash_name(me + 1);
    for folder in [
        format!("{mine}/files"),
        format!("{other}/files"),
        format!("docs/{mine}"),
    ] {
        std::fs::create_dir_all(top.join(folder)).unwrap();
    }
    std::fs::write(top.join(format!("{mine}/files/x")), b"x").unwrap();
    let fd = open_dir(&top).unwrap();
    let owners = Owners {
        root_uid: u32::MAX - 15,
        rast_uid: u32::MAX - 16,
        identity_uids: (me, me),
    };
    let mount = fd_mount_id(fd.as_raw_fd()).unwrap();
    let mut report = TreeReport::default();
    let mut visited = Vec::new();
    walk_limited(&fd, mount, &owners, 100, &mut report, |found, _| {
        visited.push(found.path.to_string());
        Ok(())
    })
    .unwrap();
    visited.sort();
    // Only the top `.Trash-<owner>`; a name that is not its owner's and one
    // deeper down are walked like any folder.
    assert_eq!(report.user_trash, 1);
    let mut expected = vec![
        "docs".to_string(),
        format!("docs/{mine}"),
        other.clone(),
        format!("{other}/files"),
    ];
    expected.sort();
    assert_eq!(visited, expected);
}

#[tokio::test]
async fn containers_are_real_folders_in_the_shared_root() {
    let fx = fixture();
    fx.ensure_groups().await;
    std::fs::create_dir(fx.root().join("elsewhere")).unwrap();
    symlink(fx.root().join("elsewhere"), fx.shared().join("linked")).unwrap();
    std::fs::write(fx.shared().join("plain"), b"x").unwrap();
    for (container, code) in [
        ("linked", "forbidden"),
        ("plain", "forbidden"),
        ("missing", "not_found"),
        ("../storage", "invalid_request"),
        ("a/b", "invalid_request"),
    ] {
        let error = share_dir_prepare(
            &fx.options,
            ShareDirPrepareOp {
                share_id: SHARE.into(),
                container: container.into(),
                recursive: true,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, code, "{container}");
    }
    assert_eq!(mode(&fx.root().join("elsewhere")) & 0o2000, 0);
}

#[tokio::test]
async fn the_migration_journals_first_and_rolls_back_exactly() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    // A terminal grant of an identity (0.18.x) on a folder.
    let docs = std::fs::File::open(team.join("docs")).unwrap();
    let granted = Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 7,
        },
        Entry {
            tag: Tag::User(200_007),
            perm: 7,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 5,
        },
        Entry {
            tag: Tag::Mask,
            perm: 7,
        },
        Entry {
            tag: Tag::Other,
            perm: 5,
        },
    ]);
    acl::write(docs.as_raw_fd(), acl::ACCESS_XATTR, &granted).unwrap();
    drop(docs);
    // ACL entries that RAST does not manage: listed by the dry run.
    let plan = std::fs::File::open(team.join("docs/plan.txt")).unwrap();
    let foreign = Acl::new(vec![
        Entry {
            tag: Tag::UserObj,
            perm: 6,
        },
        Entry {
            tag: Tag::User(1001),
            perm: 6,
        },
        Entry {
            tag: Tag::GroupObj,
            perm: 4,
        },
        Entry {
            tag: Tag::Group(2000),
            perm: 4,
        },
        Entry {
            tag: Tag::Mask,
            perm: 6,
        },
        Entry {
            tag: Tag::Other,
            perm: 4,
        },
    ]);
    acl::write(plan.as_raw_fd(), acl::ACCESS_XATTR, &foreign).unwrap();
    drop(plan);
    let deep = std::fs::File::open(team.join("docs/deep")).unwrap();
    acl::write(deep.as_raw_fd(), acl::DEFAULT_XATTR, &foreign).unwrap();
    drop(deep);
    // A set-gid folder keeps the bit on rollback; a set-gid file loses it.
    std::fs::set_permissions(
        team.join("docs/deep"),
        std::fs::Permissions::from_mode(0o2755),
    )
    .unwrap();
    std::fs::write(team.join("docs/sgid.bin"), b"x").unwrap();
    std::fs::set_permissions(
        team.join("docs/sgid.bin"),
        std::fs::Permissions::from_mode(0o2750),
    )
    .unwrap();
    let before: Vec<(PathBuf, u32, Option<Acl>)> = [
        "",
        "docs",
        "docs/deep",
        "docs/plan.txt",
        "docs/sgid.bin",
        "docs/deep/run.sh",
    ]
    .iter()
    .map(|path| {
        let full = team.join(path);
        (
            full.clone(),
            mode(&full),
            read_acl(&full, acl::ACCESS_XATTR),
        )
    })
    .collect();
    let op = || ShareDirOp {
        share_id: SHARE.into(),
        container: "team".into(),
    };

    let scan = shares_migrate_scan(&fx.options, op()).await.unwrap();
    assert_eq!(scan["journal"], Value::Null);
    assert_eq!(scan["report"]["changes"]["count"], 6);
    assert_eq!(
        scan["report"]["identityAclEntries"]["paths"],
        json!(["docs"])
    );
    let mut foreign_paths: Vec<&str> = scan["report"]["foreignAclEntries"]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    foreign_paths.sort_unstable();
    assert_eq!(
        foreign_paths,
        [
            "docs/deep: default u:1001:rw-, default g:2000:r--",
            "docs/plan.txt: u:1001:rw-, g:2000:r--",
        ]
    );
    assert_eq!(mode(&team), 0o750, "the scan changes nothing");

    let applied = shares_migrate_apply(&fx.options, op()).await.unwrap();
    assert_eq!(applied["resumed"], false);
    let stamp = applied["journal"].as_str().unwrap().to_string();
    assert_eq!(mode(&team), 0o2770);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o660);
    assert!(read_acl(&team.join("docs"), acl::ACCESS_XATTR)
        .unwrap()
        .named_users()
        .all(|(uid, _)| uid != 200_007));
    let journal_dir = fx.options.data_dir.join("migrations/shares").join(SHARE);
    let journal = journal_dir.join(&stamp).join(JOURNAL_FILE);
    assert_eq!(mode(&journal), 0o600);
    assert_eq!(mode(&journal_dir), 0o700);

    // A second apply resumes with the same journal (the state before).
    let again = shares_migrate_apply(&fx.options, op()).await.unwrap();
    assert_eq!(again["resumed"], true);
    assert_eq!(again["journal"], stamp.as_str());
    assert_eq!(again["report"]["changes"]["count"], 0);

    // A file created after the migration is left alone by the rollback.
    std::fs::write(team.join("docs/new.txt"), b"new").unwrap();
    let rolled = shares_migrate_rollback(&fx.options, op()).await.unwrap();
    assert_eq!(rolled["report"]["restored"], 6, "{rolled}");
    // Every entry that lost a bit is reported, and listed next to the
    // journal.
    assert_eq!(rolled["report"]["droppedBitsCount"], 2);
    let mut dropped: Vec<Value> = rolled["report"]["droppedBits"].as_array().unwrap().clone();
    dropped.sort_by_key(|entry| entry["path"].as_str().unwrap().to_string());
    assert_eq!(
        dropped,
        [
            json!({"path": "docs/deep/run.sh", "recorded": "4755", "restored": "0755"}),
            json!({"path": "docs/sgid.bin", "recorded": "2750", "restored": "0750"}),
        ]
    );
    let list = journal_dir
        .join(format!("{stamp}{ROLLED_BACK_SUFFIX}"))
        .join(DROPPED_BITS_FILE);
    assert_eq!(mode(&list), 0o600);
    let mut lines: Vec<String> = std::fs::read_to_string(&list)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        ["2750 0750 docs/sgid.bin", "4755 0755 docs/deep/run.sh"]
    );
    assert_eq!(
        rolled["report"]["droppedBitsFile"],
        format!("{SHARE}/{stamp}{ROLLED_BACK_SUFFIX}/{DROPPED_BITS_FILE}")
    );
    assert_eq!(
        rolled["report"]["notRecorded"]["paths"],
        json!(["docs/new.txt"])
    );
    for (path, mode_before, acl_before) in &before {
        // Set-uid is never restored.
        let expected = if path.is_dir() {
            *mode_before
        } else {
            mode_before & 0o1777
        };
        assert_eq!(mode(path), expected, "{}", path.display());
        assert_eq!(
            &read_acl(path, acl::ACCESS_XATTR),
            acl_before,
            "{}",
            path.display()
        );
    }
    assert_eq!(read_acl(&team, acl::DEFAULT_XATTR), None);
    assert!(journal_dir
        .join(format!("{stamp}{ROLLED_BACK_SUFFIX}"))
        .is_dir());
    let error = shares_migrate_rollback(&fx.options, op())
        .await
        .unwrap_err();
    assert_eq!(error.code, "not_found");
    // A new apply starts over with a new journal.
    let fresh = shares_migrate_apply(&fx.options, op()).await.unwrap();
    assert_eq!(fresh["resumed"], false);
}

#[tokio::test]
async fn a_tampered_journal_is_refused() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    let op = || ShareDirOp {
        share_id: SHARE.into(),
        container: "team".into(),
    };
    let applied = shares_migrate_apply(&fx.options, op()).await.unwrap();
    let journal_dir = fx.options.data_dir.join("migrations/shares").join(SHARE);
    let journal = journal_dir
        .join(applied["journal"].as_str().unwrap())
        .join(JOURNAL_FILE);
    let original = std::fs::read_to_string(&journal).unwrap();
    let edit = |find: &str, replace: &str| {
        let lines: Vec<String> = original
            .lines()
            .map(|line| {
                if line.contains("\"p\":\"docs/plan.txt\"") {
                    let mut record: Value = serde_json::from_str(line).unwrap();
                    record[find] = serde_json::from_str(replace).unwrap();
                    record.to_string()
                } else {
                    line.to_string()
                }
            })
            .collect();
        std::fs::write(&journal, lines.join("\n")).unwrap();
    };
    for (field, value) in [
        ("mode", "65535"),
        ("gid", "0"),
        ("uid", "0"),
        ("p", "\"../../etc/shadow\""),
        ("p", "\"/etc/shadow\""),
        ("acl", "\"zz\""),
        ("dacl", "\"02000000\""),
        ("extra", "1"),
    ] {
        edit(field, value);
        let error = shares_migrate_rollback(&fx.options, op())
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "forbidden",
            "{field}={value}: {}",
            error.message
        );
    }
    // A journal folder that others can open is refused as a whole.
    std::fs::write(&journal, &original).unwrap();
    std::fs::set_permissions(&journal_dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let error = shares_migrate_rollback(&fx.options, op())
        .await
        .unwrap_err();
    assert_eq!(error.code, "forbidden");
    std::fs::set_permissions(&journal_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A refused rollback changed nothing.
    assert_eq!(mode(&team), 0o2770);
    // A set-uid mode in the journal is restored without the bit.
    edit("mode", "3565"); // 0o6755
    assert!(shares_migrate_rollback(&fx.options, op()).await.is_ok());
    assert_eq!(mode(&team), 0o750);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o755);
}

#[tokio::test]
async fn a_released_container_is_closed_to_the_old_groups() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    let op = || ShareDirOp {
        share_id: SHARE.into(),
        container: "team".into(),
    };
    share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: true,
        },
    )
    .await
    .unwrap();
    let value = share_dir_release(&fx.options, op()).await.unwrap();
    assert_eq!(value["released"], true);
    assert_eq!(mode(&team), 0o700);
    assert_eq!(read_acl(&team, acl::ACCESS_XATTR), None);
    assert_eq!(read_acl(&team, acl::DEFAULT_XATTR), None);
    // The data stays as it was.
    assert_eq!(std::fs::read(team.join("docs/plan.txt")).unwrap(), b"plan");
    std::fs::remove_dir_all(&team).unwrap();
    let value = share_dir_release(&fx.options, op()).await.unwrap();
    assert_eq!(value["released"], false);
}

#[tokio::test]
async fn removing_a_container_never_follows_links() {
    let fx = fixture();
    let team = legacy_tree(&fx);
    let value = share_dir_remove(
        &fx.options,
        ShareDirOp {
            share_id: SHARE.into(),
            container: "team".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(value["removed"], true);
    assert!(!team.exists());
    assert_eq!(
        std::fs::read(fx.root().join("outside.txt")).unwrap(),
        b"outside"
    );
    assert!(fx.root().join("hard-elsewhere.txt").exists());
    let again = share_dir_remove(
        &fx.options,
        ShareDirOp {
            share_id: SHARE.into(),
            container: "team".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(again["removed"], false);
    let meta = std::fs::metadata(fx.shared()).unwrap();
    assert!(meta.is_dir() && meta.nlink() >= 2);
}

#[test]
fn mount_ids_tell_mount_points_apart() {
    // Tests cannot mount; `/proc` is a mount point below `/` on Linux.
    let root = open_dir(Path::new("/")).unwrap();
    let root_mount = fd_mount_id(root.as_raw_fd()).unwrap();
    if let Ok(proc_mount) = mount_id(root.as_raw_fd(), c"proc") {
        assert_ne!(
            proc_mount, root_mount,
            "a mount point reports the mount on top"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("file"), b"x").unwrap();
    symlink("nowhere", dir.path().join("link")).unwrap();
    let fd = open_dir(dir.path()).unwrap();
    let own = fd_mount_id(fd.as_raw_fd()).unwrap();
    for name in [c"sub", c"file", c"link"] {
        assert_eq!(mount_id(fd.as_raw_fd(), name).unwrap(), own, "{name:?}");
    }
}

#[tokio::test]
async fn walks_and_removals_never_enter_another_mount() {
    // A bind mount on the same file system keeps `st_dev`; only its mount
    // id differs. Tests cannot mount, so the container's mount id is
    // replaced by one no entry has: every entry is then "another mount".
    let fx = fixture();
    let team = legacy_tree(&fx);
    let (fd, _) = open_container(&fx.options, "team").unwrap();
    let own = fd_mount_id(fd.as_raw_fd()).unwrap();
    let owners = Owners::of(&fx.options).unwrap();
    let mut visited = Vec::new();
    let mut report = TreeReport::default();
    walk_limited(
        &fd,
        own.wrapping_add(1),
        &owners,
        MAX_TREE_ENTRIES,
        &mut report,
        |found, _| {
            visited.push(found.path.to_string());
            Ok(())
        },
    )
    .unwrap();
    assert!(visited.is_empty(), "{visited:?}");
    assert_eq!(report.dirs + report.files, 0, "nothing was entered");
    assert!(
        report.other_device.paths.contains(&"docs".to_string()),
        "{report:?}"
    );
    // With the right mount id the same walk visits the tree.
    let mut report = TreeReport::default();
    walk_limited(&fd, own, &owners, MAX_TREE_ENTRIES, &mut report, |_, _| {
        Ok(())
    })
    .unwrap();
    assert!(
        report.dirs > 0 && report.other_device.count == 0,
        "{report:?}"
    );

    // A removal stops before it touches anything below a mount point.
    let error = remove_contents(&fd, own.wrapping_add(1), 0, &mut 0).unwrap_err();
    assert_eq!(error.code, "forbidden");
    assert!(error.message.contains("mount"), "{error:?}");
    assert_eq!(std::fs::read(team.join("docs/plan.txt")).unwrap(), b"plan");
    assert!(team.join("docs/deep").is_dir());
}

fn denied_uids(path: &Path) -> Vec<u32> {
    read_acl(path, acl::ACCESS_XATTR)
        .unwrap()
        .named_users()
        .filter(|(_, perm)| *perm == 0)
        .map(|(uid, _)| uid)
        .collect()
}

#[tokio::test]
async fn deny_entries_are_set_exactly_and_kept_by_every_prepare() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    let deny = |ids: Vec<i64>| ShareDenySetOp {
        share_id: SHARE.into(),
        container: "team".into(),
        user_ids: ids,
    };
    // A legacy container is left alone: its access is not by group.
    let legacy = share_deny_set(&fx.options, deny(vec![3])).await.unwrap();
    assert_eq!(legacy["applied"], false);
    assert_eq!(read_acl(&team, acl::ACCESS_XATTR), None);

    share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: false,
        },
    )
    .await
    .unwrap();
    let uid = |id: i64| {
        identity_ids(&fx.options.validation_context(), id)
            .unwrap()
            .uid
    };
    let set = share_deny_set(&fx.options, deny(vec![4, 3])).await.unwrap();
    assert_eq!(
        (set["applied"].clone(), set["changed"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(set["userIds"], json!([3, 4]));
    assert_eq!(denied_uids(&team), [uid(3), uid(4)]);
    // Only the access ACL of the container: new entries do not inherit it.
    assert!(read_acl(&team, acl::DEFAULT_XATTR)
        .unwrap()
        .named_users()
        .all(|(_, perm)| perm != 0));
    assert_eq!(mode(&team), 0o2770);

    // The recursive prepare (migration, repair) keeps them, and does not
    // count them as old per-user entries.
    let again = share_dir_prepare(
        &fx.options,
        ShareDirPrepareOp {
            share_id: SHARE.into(),
            container: "team".into(),
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(again["report"]["containerPrepared"], true, "{again}");
    assert_eq!(again["report"]["identityAclEntries"]["count"], 0);
    assert_eq!(again["report"]["foreignAclEntries"]["count"], 0);
    assert_eq!(denied_uids(&team), [uid(3), uid(4)]);

    // The list is the whole set: 3 goes, 4 stays.
    let unchanged = share_deny_set(&fx.options, deny(vec![3, 4])).await.unwrap();
    assert_eq!(unchanged["changed"], false);
    share_deny_set(&fx.options, deny(vec![4])).await.unwrap();
    assert_eq!(denied_uids(&team), [uid(4)]);
    share_deny_set(&fx.options, deny(vec![])).await.unwrap();
    assert!(denied_uids(&team).is_empty());
    let base = read_acl(&team, acl::ACCESS_XATTR).unwrap();
    assert_eq!(base, read_acl(&team, acl::DEFAULT_XATTR).unwrap());

    for bad in [vec![0], vec![3, 3], (1..=257).collect::<Vec<i64>>()] {
        let error = share_deny_set(&fx.options, deny(bad)).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}

#[tokio::test]
async fn the_repair_pass_reapplies_the_layout_of_migrated_shares_only() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    if !acls_supported(&team) {
        return;
    }
    let op = || ShareDirOp {
        share_id: SHARE.into(),
        container: "team".into(),
    };
    // Not migrated yet: nothing changes (the migration journals first).
    let skipped = share_dir_repair(&fx.options, op()).await.unwrap();
    assert_eq!(skipped["repaired"], false);
    assert_eq!(skipped["reason"], "not_migrated");
    assert_eq!(mode(&team), 0o750);
    assert_eq!(mode(&team.join("docs/plan.txt")), 0o644);

    shares_migrate_apply(&fx.options, op()).await.unwrap();
    // Since then: a file moved in from elsewhere (legacy File Station, a
    // rename from HOME) with its own mode and no ACL, and a narrowed one.
    std::fs::write(team.join("docs/moved.txt"), b"m").unwrap();
    std::fs::set_permissions(
        team.join("docs/moved.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::set_permissions(
        team.join("docs/plan.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let repaired = share_dir_repair(&fx.options, op()).await.unwrap();
    assert_eq!(repaired["repaired"], true);
    assert_eq!(repaired["report"]["changes"]["count"], 2, "{repaired}");
    for file in ["docs/moved.txt", "docs/plan.txt"] {
        assert_eq!(mode(&team.join(file)), 0o660, "{file}");
        assert!(read_acl(&team.join(file), acl::ACCESS_XATTR)
            .unwrap()
            .named_groups()
            .any(|(gid, perm)| gid == 1_200_002 && perm == 4));
    }
    let clean = share_dir_repair(&fx.options, op()).await.unwrap();
    assert_eq!(clean["report"]["changes"]["count"], 0);

    // A file of the rast service hard-linked in (its database, say, with
    // `fs.protected_hardlinks = 0`): rast is an owner the pass manages, but
    // a file with a second name is never changed, only reported.
    std::fs::write(fx.root().join("rast-secret"), b"s").unwrap();
    std::fs::set_permissions(
        fx.root().join("rast-secret"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::hard_link(fx.root().join("rast-secret"), team.join("docs/linked")).unwrap();
    let repaired = share_dir_repair(&fx.options, op()).await.unwrap();
    assert_eq!(repaired["report"]["changes"]["count"], 0, "{repaired}");
    assert!(repaired["report"]["hardLinks"]["paths"]
        .as_array()
        .unwrap()
        .contains(&json!("docs/linked")));
    assert_eq!(mode(&fx.root().join("rast-secret")), 0o600);
    assert_eq!(read_acl(&team.join("docs/linked"), acl::ACCESS_XATTR), None);
}

#[test]
fn process_status_gives_the_real_and_effective_uid_and_the_groups() {
    let status = "Name:\tbash\nUid:\t200003\t200004\t200003\t200003\n\
                  Gid:\t200003\t200003\t200003\t200003\nGroups:\t4 1200001 \n";
    assert_eq!(
        parse_proc_status(status),
        Some((200_003, 200_004, vec![4, 1_200_001]))
    );
    assert_eq!(
        parse_proc_status("Uid:\t1\t2\t3\t4\nGroups:\n"),
        Some((1, 2, vec![]))
    );
    for bad in [
        "",
        "Uid:\t1\n",
        "Groups:\t1\n",
        "Uid:\tx\tx\nGroups:\t1\n",
        "Uid:\t1\t2\nGroups:\tx\n",
    ] {
        assert_eq!(parse_proc_status(bad), None, "{bad:?}");
    }
    assert!(is_share_group_name(RW) && is_share_group_name(RO));
    for bad in [
        "rast-s0123-rw",
        "rast-s0123abcd45674ef0-rx",
        "rast-u3",
        "rast-s0123ABCD45674ef0-rw",
    ] {
        assert!(!is_share_group_name(bad), "{bad}");
    }
}

#[test]
fn the_unit_of_a_process_is_the_first_cgroup_below_the_slices() {
    let unit = |text: &str| parse_proc_cgroup(text);
    assert_eq!(
        unit("0::/rast.slice/rast-linux.slice/rast-xpra-u3.service\n").as_deref(),
        Some("rast-xpra-u3.service")
    );
    // A cgroup below the unit (a delegated unit names it) is never read.
    assert_eq!(
        unit("0::/system.slice/rast-term-u200003-ab.service/rast-xpra-u4.service\n").as_deref(),
        Some("rast-term-u200003-ab.service")
    );
    // cgroup v1: the systemd hierarchy.
    assert_eq!(
        unit("12:cpu,cpuacct:/x\n1:name=systemd:/rast.slice/rast-linux.slice/rast-files-u200003.service\n")
            .as_deref(),
        Some("rast-files-u200003.service")
    );
    assert_eq!(
        unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.service\n").as_deref(),
        Some("user@1000.service")
    );
    for none in [
        "",
        "0::/\n",
        "0::/rast.slice/rast-linux.slice\n",
        "3:cpu:/x\n",
    ] {
        assert_eq!(unit(none), None, "{none:?}");
    }

    let own = |unit: &str| is_user_unit(unit, 3, 200_003);
    for yes in [
        "rast-xpra-u3.service",
        "rast-files-u200003.service",
        "rast-app-u200003-code-0.service",
        "rast-term-u200003-0123456789abcdef0123456789abcdef.service",
    ] {
        assert!(own(yes), "{yes}");
    }
    for no in [
        "rast-xpra-u30.service",
        "rast-xpra-u3.scope",
        "rast-files-u2000031.service",
        "rast-app-u2000031-code-0.service",
        "rast-app-u200003-.service",
        "rast-term-u200004-ab.service",
        "user@200003.service",
        "rast.service",
    ] {
        assert!(!own(no), "{no}");
    }
}

#[tokio::test]
async fn session_groups_name_the_share_groups_processes_carry() {
    let mut fx = fixture();
    fx.ensure_groups().await;
    fx.add_group("rast-other:x:1200003:");
    let proc = fx.root().join("proc");
    let uid = |id: i64| {
        identity_ids(&fx.options.validation_context(), id)
            .unwrap()
            .uid
    };
    let linux = "0::/rast.slice/rast-linux.slice";
    let term3 = format!("0::/system.slice/rast-term-u{}-ab.service", uid(3));
    let files4 = format!("{linux}/rast-files-u{}.service", uid(4));
    for (pid, ruid, euid, groups, cgroup) in [
        (
            "100",
            uid(3),
            uid(3),
            "4 1200001 1200002",
            format!("{linux}/rast-xpra-u3.service"),
        ),
        ("101", uid(3), uid(3), "1200003 27", term3),
        ("102", uid(4), uid(4), "", files4.clone()),
        // Not asked about.
        (
            "103",
            uid(5),
            uid(5),
            "1200001",
            format!("{linux}/rast-xpra-u5.service"),
        ),
        ("104", 1000, 1000, "1200001", files4.clone()),
        // User 3's identity outside user 3's units: a set-uid program of
        // user 3 run by user 4 (it carries user 4's groups), a process in
        // user 4's session, one outside every RAST unit.
        ("106", uid(4), uid(3), "1200004", files4.clone()),
        ("107", uid(3), uid(3), "1200004", files4),
        (
            "108",
            uid(3),
            uid(3),
            "1200004",
            "0::/user.slice/user-1000.slice/session-2.scope".into(),
        ),
        // Without a cgroup file nothing is known: not counted.
        ("109", uid(3), uid(3), "1200004", String::new()),
    ] {
        std::fs::create_dir_all(proc.join(pid)).unwrap();
        std::fs::write(
            proc.join(pid).join("status"),
            format!("Name:\tx\nUid:\t{ruid}\t{euid}\t{euid}\t{euid}\nGroups:\t{groups}\n"),
        )
        .unwrap();
        if !cgroup.is_empty() {
            std::fs::write(proc.join(pid).join("cgroup"), format!("{cgroup}\n")).unwrap();
        }
    }
    std::fs::create_dir_all(proc.join("105")).unwrap();
    std::fs::write(proc.join("105/status"), "garbage").unwrap();
    std::fs::create_dir_all(proc.join("sys")).unwrap();
    symlink("100", proc.join("self")).unwrap();
    fx.options.test_proc_dir = Some(proc);

    let op = |ids: Vec<i64>| ShareSessionGroupsOp { user_ids: ids };
    let reply = share_session_groups(&fx.options, op(vec![3, 4, 9]))
        .await
        .unwrap();
    assert_eq!(
        reply,
        json!({
            "users": [
                {"userId": 3, "processes": 2, "gids": [1_200_001, 1_200_002, 1_200_003]},
                {"userId": 4, "processes": 1, "gids": []},
            ],
            // A gid of the range without a share group has no name.
            "groups": {"1200001": RW, "1200002": RO},
        })
    );
    for bad in [vec![], vec![0], vec![3, 3], (1..=201).collect::<Vec<i64>>()] {
        let error = share_session_groups(&fx.options, op(bad))
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}

#[tokio::test]
async fn operations_on_one_shared_folder_never_overlap() {
    let fx = fixture();
    fx.ensure_groups().await;
    let team = legacy_tree(&fx);
    let op = || ShareDirOp {
        share_id: SHARE.into(),
        container: "team".into(),
    };
    // While one operation works on the share, the others are refused with
    // `share_busy` (the server tries again); other shares are not held up.
    let held = ShareLock::acquire(&fx.options, SHARE, SHARE_LOCK_WAIT)
        .await
        .unwrap();
    let busy = |result: Result<Value, NativeError>| {
        let error = result.unwrap_err();
        assert_eq!(error.code, "share_busy", "{error:?}");
    };
    busy(share_dir_repair(&fx.options, op()).await);
    busy(shares_migrate_apply(&fx.options, op()).await);
    busy(share_dir_release(&fx.options, op()).await);
    busy(
        share_deny_set(
            &fx.options,
            ShareDenySetOp {
                share_id: SHARE.into(),
                container: "team".into(),
                user_ids: vec![3],
            },
        )
        .await,
    );
    let other = ShareLock::acquire(
        &fx.options,
        "33333333-4444-4555-8666-777777777777",
        SHARE_LOCK_WAIT,
    )
    .await;
    assert!(other.is_ok());
    drop(held);
    assert_eq!(mode(&team), 0o750, "nothing changed meanwhile");

    // Apply and rollback also hold a `flock` in the journal folder: a
    // second helper process (here: a descriptor of the test) holding it
    // keeps both out, the in-process lock notwithstanding.
    let root = open_journal_root(&fx.options, SHARE, true)
        .unwrap()
        .unwrap();
    let foreign = MigrationLock::acquire(&root, SHARE).unwrap();
    busy(shares_migrate_apply(&fx.options, op()).await);
    drop(foreign);
    shares_migrate_apply(&fx.options, op()).await.unwrap();
    let foreign = MigrationLock::acquire(&root, SHARE).unwrap();
    busy(shares_migrate_rollback(&fx.options, op()).await);
    assert!(
        MigrationLock::acquire(&root, SHARE).is_err(),
        "the lock is exclusive"
    );
    drop(foreign);
    shares_migrate_rollback(&fx.options, op()).await.unwrap();
    // The lock file is no journal.
    assert!(active_journal(&fx.options, SHARE).unwrap().is_none());
}
