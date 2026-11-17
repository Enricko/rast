use super::*;

use std::os::unix::fs::{symlink, PermissionsExt};

use crate::native::acl::{Entry, Tag};

/// The identity of user 7 (`linux_uid_base` 200 000 in tests).
const UID: u32 = 200_007;
const USER: &str = "alice";

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Fixture {
    dir: tempfile::TempDir,
    options: NativeOptions,
    owners: OwnerMap,
}

fn key(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

fn acl_entry(tag: Tag, perm: u16) -> Entry {
    Entry { tag, perm }
}

impl Fixture {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.options.homes_dir.join(USER)
    }

    fn identity_home(&self) -> PathBuf {
        self.options.linux_users_dir().join("7")
    }

    fn profile(&self, app: &str) -> PathBuf {
        self.options.profiles_dir().join(app).join("7")
    }

    fn legacy_trash(&self) -> PathBuf {
        self.options.storage_dir.join(".trash/7")
    }

    /// Give `path` to the identity (as the migration would see it).
    fn give(&self, path: &Path) {
        self.owners.lock().unwrap().insert(key(path), (UID, UID));
    }

    /// Give `path` and everything below it to the identity.
    fn give_tree(&self, path: &Path) {
        self.give(path);
        if std::fs::symlink_metadata(path).unwrap().is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap().path();
                if !std::fs::symlink_metadata(&entry).unwrap().is_symlink() {
                    self.give_tree(&entry);
                }
            }
        }
    }

    /// The owner the migration gave `path` (its real one otherwise).
    fn owner(&self, path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        self.owners
            .lock()
            .unwrap()
            .get(&key(path))
            .map(|owner| owner.0)
            .unwrap_or_else(|| std::fs::symlink_metadata(path).unwrap().uid())
    }

    fn passwd_home(&self) -> PathBuf {
        PathBuf::from(
            std::fs::read_to_string(self.root().join("passwd-home"))
                .unwrap()
                .trim(),
        )
    }

    fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.root().join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn op(&self) -> HomeMigrateOp {
        HomeMigrateOp {
            user_id: 7,
            username: USER.into(),
        }
    }

    async fn scan(&self) -> Value {
        home_migrate_scan(&self.options, self.op()).await.unwrap()
    }

    async fn apply(&self) -> Result<Value, NativeError> {
        home_migrate_apply(&self.options, self.op()).await
    }

    async fn rollback(&self) -> Result<Value, NativeError> {
        home_migrate_rollback(&self.options, self.op()).await
    }

    fn journal_root(&self) -> PathBuf {
        self.options.data_dir.join("migrations/home/7")
    }

    fn journals(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.journal_root())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .filter(|name| name != "lock")
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn set_acl(path: &Path, xattr: &str, entries: Vec<Entry>) {
    let file = std::fs::File::open(path).unwrap();
    acl::write(file.as_raw_fd(), xattr, &Acl::new(entries)).unwrap();
}

fn read_acl(path: &Path, xattr: &str) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).unwrap();
    acl::read_raw(file.as_raw_fd(), xattr).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

/// The 0.18.3 read grant on a folder and on a file of the HOME:
/// `u:rast-u7:rx` with its mask, and `d:u:rast-u7:rx` on the folder.
fn grant_0_18_3(dir: &Path, file: &Path) {
    let access = |user: u16, group: u16| {
        vec![
            acl_entry(Tag::UserObj, user),
            acl_entry(Tag::User(UID), 5),
            acl_entry(Tag::GroupObj, group),
            acl_entry(Tag::Mask, 5),
            acl_entry(Tag::Other, 4),
        ]
    };
    set_acl(dir, acl::ACCESS_XATTR, access(7, 5));
    set_acl(dir, acl::DEFAULT_XATTR, access(7, 5));
    set_acl(file, acl::ACCESS_XATTR, access(6, 4));
}

/// A legacy account: rast-u7 with its legacy home, a HOME of the service
/// (the test account), the identity home and two app profiles.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let data = root.join("data");
    let run = root.join("run");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let mut options = NativeOptions::for_test(root.join("native.sock"), data.clone(), run);
    options.storage_dir = root.join("storage");
    options.homes_dir = options.storage_dir.join("homes");
    options.rast_uid = Some(euid());
    let owners: OwnerMap = Arc::default();
    options.test_owners = Some(owners.clone());
    std::fs::create_dir_all(root.join("proc")).unwrap();
    options.test_proc_dir = Some(root.join("proc"));
    std::fs::write(
        root.join("passwd-home"),
        data.join("linux-users/7").display().to_string(),
    )
    .unwrap();
    options.getent_bin = write_script(
        &root,
        "getent",
        &format!(
            "#!/bin/sh\n[ \"$1\" = passwd ] && [ \"$2\" = rast-u7 ] || exit 2\n\
             echo \"rast-u7:x:{UID}:{UID}:RAST Linux app:$(cat '{root}/passwd-home'):/usr/sbin/nologin\"\n",
            root = root.display()
        ),
    );
    options.usermod_bin = write_script(
        &root,
        "usermod",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{root}/usermod.log'\n\
             [ -f '{root}/usermod.fail' ] && rm '{root}/usermod.fail' && exit 1\n\
             printf '%s' \"$2\" > '{root}/passwd-home'\n",
            root = root.display()
        ),
    );
    // `show` lists the units in `units` as active; `stop` removes one
    // (unless its name contains `stuck`).
    options.systemctl_bin = write_script(
        &root,
        "systemctl",
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{root}/systemctl.log'
case "$1" in
  show)
    [ -f '{root}/units' ] || exit 0
    while read -r unit; do
      [ -n "$unit" ] && printf 'Id=%s.service\nActiveState=active\nLoadState=loaded\nMainPID=1\n\n' "$unit"
    done < '{root}/units'
    exit 0 ;;
  stop)
    name=${{2%.service}}
    case "$name" in *stuck*) exit 0 ;; esac
    [ -f '{root}/units' ] && grep -vx "$name" '{root}/units' > '{root}/units.new'; mv '{root}/units.new' '{root}/units' 2>/dev/null
    exit 0 ;;
esac
exit 0
"#,
            root = root.display()
        ),
    );
    let fx = Fixture {
        dir,
        options,
        owners,
    };
    // The HOME of the service.
    std::fs::create_dir_all(fx.home()).unwrap();
    std::fs::set_permissions(fx.home(), std::fs::Permissions::from_mode(0o750)).unwrap();
    // The identity home and its profiles.
    std::fs::create_dir_all(fx.identity_home()).unwrap();
    fx.give(&fx.identity_home());
    for app in ["code", "chrome"] {
        std::fs::create_dir_all(fx.profile(app)).unwrap();
        fx.give(&fx.profile(app));
    }
    fx
}

/// A HOME with the 0.18.3 grant, a foreign ACL entry, a hard link and a
/// link, and profiles with entries to merge and one conflict.
fn populate(fx: &Fixture) {
    let home = fx.home();
    write(&home.join("docs/a.txt"), "a");
    std::fs::set_permissions(
        home.join("docs/a.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    grant_0_18_3(&home.join("docs"), &home.join("docs/a.txt"));
    write(&home.join("shared-with-bob.txt"), "b");
    set_acl(
        &home.join("shared-with-bob.txt"),
        acl::ACCESS_XATTR,
        vec![
            acl_entry(Tag::UserObj, 6),
            acl_entry(Tag::User(UID), 5),
            acl_entry(Tag::User(1001), 6),
            acl_entry(Tag::GroupObj, 4),
            acl_entry(Tag::Mask, 7),
            acl_entry(Tag::Other, 4),
        ],
    );
    write(&home.join("linked.txt"), "l");
    std::fs::hard_link(home.join("linked.txt"), home.join("linked-too.txt")).unwrap();
    symlink("/etc", home.join("etc-link")).unwrap();
    // Made in a terminal of the legacy account.
    write(&home.join("from-terminal.txt"), "t");
    fx.give(&home.join("from-terminal.txt"));

    // The identity home: the session's auth file and an app config.
    let identity = fx.identity_home();
    write(&identity.join(".Xauthority"), "cookie");
    write(&identity.join(".config/pulse/cookie"), "pulse");
    write(&identity.join(".bash_history"), "ls");
    fx.give_tree(&identity);
    // Two profiles: VS Code merges; Chrome conflicts on the pulse cookie.
    let code = fx.profile("code");
    write(&code.join("home/.vscode/argv.json"), "{}");
    write(&code.join("config/Code/User/settings.json"), "{}");
    write(&code.join("cache/Code/c"), "c");
    write(&code.join("tmp/run"), "r");
    fx.give_tree(&code);
    let chrome = fx.profile("chrome");
    write(
        &chrome.join("config/google-chrome/Default/Preferences"),
        "{}",
    );
    write(&chrome.join("config/pulse/cookie"), "other");
    write(
        &chrome.join("data/applications/x.desktop"),
        "[Desktop Entry]",
    );
    fx.give_tree(&chrome);
}

fn sample_paths(value: &Value) -> Vec<String> {
    value["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_dry_run_reports_owners_acls_merges_and_conflicts_without_changes() {
    let fx = fixture();
    populate(&fx);
    let before = read_acl(&fx.home().join("docs/a.txt"), acl::ACCESS_XATTR);
    let owners_before = fx.owners.lock().unwrap().clone();
    let scan = fx.scan().await;
    assert_eq!(scan["mode"], "legacy", "{scan}");
    assert_eq!(scan["journal"], Value::Null);
    let tree = &scan["tree"];
    // The HOME, docs, docs/a.txt and shared-with-bob.txt change owner; the
    // hard links stay.
    assert_eq!(tree["changeOwner"]["count"], 4, "{tree}");
    assert_eq!(tree["identityOwned"], 1);
    assert_eq!(tree["hardLinks"]["count"], 2);
    assert_eq!(tree["symlinks"]["count"], 1);
    let acl = sample_paths(&tree["identityAclEntries"]);
    for path in ["docs", "docs/a.txt", "shared-with-bob.txt"] {
        assert!(acl.contains(&path.to_string()), "{acl:?}");
    }
    assert_eq!(
        sample_paths(&tree["foreignAclEntries"]),
        vec!["shared-with-bob.txt: u:1001:rw-".to_string()]
    );
    let merge = &scan["merge"];
    assert_eq!(merge["regenerated"], json!([".Xauthority"]));
    assert_eq!(
        sample_paths(&merge["conflicts"]),
        vec!["chrome: config/pulse/cookie".to_string()]
    );
    let sources: Vec<&str> = merge["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["identity", "chrome", "code"]);
    let code = merge["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["source"] == "code")
        .unwrap();
    // home/.vscode, config/Code, cache (the whole folder): `tmp` stays.
    assert_eq!(code["moves"], 3, "{code}");
    // `tmp` is not counted: it is not merged.
    assert_eq!(code["files"], 3);
    // Nothing changed.
    assert_eq!(*fx.owners.lock().unwrap(), owners_before);
    assert_eq!(fx.owner(&fx.home()), euid());
    assert_eq!(
        read_acl(&fx.home().join("docs/a.txt"), acl::ACCESS_XATTR),
        before
    );
    assert!(fx.journals().is_empty());
    assert!(!fx.home().join(".config").exists());
    assert!(fx.log("usermod").is_empty());
}

#[tokio::test]
async fn apply_gives_the_home_to_the_identity_strips_the_grant_and_merges() {
    let fx = fixture();
    populate(&fx);
    let home = fx.home();
    std::fs::write(
        fx.root().join("units"),
        "rast-app-u200007-code-1\nrast-xpra-u7\n",
    )
    .unwrap();
    let applied = fx.apply().await.unwrap();
    assert_eq!(applied["alreadyOneHome"], false, "{applied}");
    assert_eq!(
        applied["stopped"],
        json!(["rast-app-u200007-code-1", "rast-xpra-u7"])
    );
    // The HOME and its entries are the identity's; hard links are not.
    for path in [
        "",
        "docs",
        "docs/a.txt",
        "shared-with-bob.txt",
        "from-terminal.txt",
    ] {
        assert_eq!(fx.owner(&home.join(path)), UID, "{path}");
    }
    assert_eq!(fx.owner(&home.join("linked.txt")), euid());
    assert_eq!(mode(&home), 0o700);
    // The 0.18.3 grant is gone: no ACL left, the group bits are the
    // group's own again.
    assert_eq!(read_acl(&home.join("docs/a.txt"), acl::ACCESS_XATTR), None);
    assert_eq!(mode(&home.join("docs/a.txt")), 0o644);
    assert_eq!(read_acl(&home.join("docs"), acl::ACCESS_XATTR), None);
    assert_eq!(read_acl(&home.join("docs"), acl::DEFAULT_XATTR), None);
    // A foreign entry stays, with its mask; the identity's is gone.
    let kept =
        Acl::decode(&read_acl(&home.join("shared-with-bob.txt"), acl::ACCESS_XATTR).unwrap())
            .unwrap();
    assert_eq!(
        kept.0,
        vec![
            acl_entry(Tag::UserObj, 6),
            acl_entry(Tag::User(1001), 6),
            acl_entry(Tag::GroupObj, 4),
            acl_entry(Tag::Mask, 6),
            acl_entry(Tag::Other, 4),
        ]
    );
    // The merge: the identity's .config moved as a whole, VS Code merged
    // into it, Chrome's conflicting cookie stayed.
    assert_eq!(
        std::fs::read_to_string(home.join(".config/pulse/cookie")).unwrap(),
        "pulse"
    );
    assert!(home.join(".config/Code/User/settings.json").is_file());
    assert!(home
        .join(".config/google-chrome/Default/Preferences")
        .is_file());
    assert!(home.join(".vscode/argv.json").is_file());
    assert!(home.join(".cache/Code/c").is_file());
    assert!(home.join(".local/share/applications/x.desktop").is_file());
    assert!(home.join(".bash_history").is_file());
    assert!(fx.profile("chrome").join("config/pulse/cookie").is_file());
    assert!(fx.profile("code").join("tmp/run").is_file());
    assert!(fx.identity_home().join(".Xauthority").is_file());
    assert!(!home.join(".Xauthority").exists());
    // `.local` was made for the identity.
    assert_eq!(fx.owner(&home.join(".local")), UID);
    assert_eq!(mode(&home.join(".local")), 0o700);
    // The passwd home, the journal and its marker.
    assert_eq!(fx.passwd_home(), home);
    let journals = fx.journals();
    assert_eq!(journals.len(), 1);
    let journal = fx.journal_root().join(&journals[0]);
    for file in [JOURNAL_FILE, PLAN_FILE, APPLIED_FILE] {
        assert!(journal.join(file).is_file(), "{file}");
        assert_eq!(mode(&journal.join(file)), 0o600);
    }
    assert!(fx
        .log("systemctl")
        .iter()
        .any(|line| line == "stop rast-app-u200007-code-1.service"));

    // A second run changes nothing and leaves the session running.
    std::fs::write(fx.root().join("units"), "rast-xpra-u7\n").unwrap();
    let stops = fx.log("systemctl").len();
    let again = fx.apply().await.unwrap();
    assert_eq!(again["alreadyOneHome"], true, "{again}");
    assert_eq!(again["stopped"], json!([]));
    assert_eq!(fx.log("usermod").len(), 1);
    assert!(!fx.log("systemctl")[stops..]
        .iter()
        .any(|line| line.starts_with("stop ")));
    assert_eq!(fx.scan().await["mode"], "oneHome");
}

#[tokio::test]
async fn rollback_restores_the_home_exactly_and_moves_everything_back() {
    let fx = fixture();
    populate(&fx);
    let home = fx.home();
    let docs_acl = read_acl(&home.join("docs"), acl::ACCESS_XATTR);
    let docs_default = read_acl(&home.join("docs"), acl::DEFAULT_XATTR);
    let file_acl = read_acl(&home.join("docs/a.txt"), acl::ACCESS_XATTR);
    let file_mode = mode(&home.join("docs/a.txt"));
    fx.apply().await.unwrap();
    // The person works in one-HOME mode for a while.
    write(&home.join("new.txt"), "n");
    fx.give(&home.join("new.txt"));

    let rolled = fx.rollback().await.unwrap();
    let report = &rolled["report"];
    assert!(report["restored"].as_u64().unwrap() >= 4, "{report}");
    assert_eq!(
        sample_paths(&report["returned"]),
        vec!["new.txt".to_string()]
    );
    // Owners, ACLs (the 0.18.3 grant included) and modes as they were.
    for path in ["", "docs", "docs/a.txt", "shared-with-bob.txt", "new.txt"] {
        assert_eq!(fx.owner(&home.join(path)), euid(), "{path}");
    }
    assert_eq!(fx.owner(&home.join("from-terminal.txt")), UID);
    assert_eq!(mode(&home), 0o750);
    assert_eq!(read_acl(&home.join("docs"), acl::ACCESS_XATTR), docs_acl);
    assert_eq!(
        read_acl(&home.join("docs"), acl::DEFAULT_XATTR),
        docs_default
    );
    assert_eq!(
        read_acl(&home.join("docs/a.txt"), acl::ACCESS_XATTR),
        file_acl
    );
    assert_eq!(mode(&home.join("docs/a.txt")), file_mode);
    // Everything that moved is back where it was; `.local` is gone.
    assert!(fx.identity_home().join(".config/pulse/cookie").is_file());
    assert!(fx
        .profile("code")
        .join("config/Code/User/settings.json")
        .is_file());
    assert!(fx.profile("code").join("home/.vscode/argv.json").is_file());
    assert!(fx.profile("code").join("cache/Code/c").is_file());
    assert!(fx
        .profile("chrome")
        .join("data/applications/x.desktop")
        .is_file());
    assert!(!home.join(".config").exists());
    assert!(!home.join(".local").exists());
    assert_eq!(fx.passwd_home(), fx.identity_home());
    let journals = fx.journals();
    assert_eq!(journals.len(), 1);
    assert!(journals[0].ends_with(ROLLED_BACK_SUFFIX));
    assert_eq!(fx.scan().await["mode"], "legacy");
    // Nothing left to roll back; a new migration starts over.
    assert_eq!(fx.rollback().await.unwrap_err().code, "not_found");
    let again = fx.apply().await.unwrap();
    assert_eq!(again["resumed"], false);
    assert_eq!(fx.journals().len(), 2);
}

#[tokio::test]
async fn an_interrupted_apply_resumes_from_its_journal() {
    let fx = fixture();
    populate(&fx);
    std::fs::write(fx.root().join("usermod.fail"), b"").unwrap();
    let error = fx.apply().await.unwrap_err();
    assert_eq!(error.code, "command_failed");
    // The tree and the merge ran; the HOME itself and passwd did not.
    assert_eq!(fx.owner(&fx.home().join("docs")), UID);
    assert_eq!(fx.owner(&fx.home()), euid());
    assert!(fx.home().join(".vscode").is_dir());
    let journals = fx.journals();
    assert_eq!(journals.len(), 1);
    assert!(!fx
        .journal_root()
        .join(&journals[0])
        .join(APPLIED_FILE)
        .exists());
    let scan = fx.scan().await;
    assert_eq!(scan["journal"], journals[0]);
    assert_eq!(scan["applied"], false);

    let resumed = fx.apply().await.unwrap();
    assert_eq!(resumed["resumed"], true, "{resumed}");
    assert_eq!(resumed["journal"], journals[0]);
    assert!(resumed["steps"]["already"].as_u64().unwrap() > 0);
    assert_eq!(fx.owner(&fx.home()), UID);
    assert_eq!(fx.passwd_home(), fx.home());
    // The rollback still restores the HOME as the first run found it.
    fx.rollback().await.unwrap();
    assert_eq!(fx.owner(&fx.home().join("docs")), euid());
    assert!(fx.profile("code").join("home/.vscode").is_dir());
}

#[tokio::test]
async fn links_are_moved_or_reported_but_never_followed() {
    let fx = fixture();
    populate(&fx);
    // A folder outside, reachable through links in the HOME and a profile.
    let outside = fx.root().join("outside");
    write(&outside.join("secret.txt"), "s");
    set_acl(
        &outside.join("secret.txt"),
        acl::ACCESS_XATTR,
        vec![
            acl_entry(Tag::UserObj, 6),
            acl_entry(Tag::User(UID), 4),
            acl_entry(Tag::GroupObj, 4),
            acl_entry(Tag::Mask, 4),
            acl_entry(Tag::Other, 0),
        ],
    );
    let outside_acl = read_acl(&outside.join("secret.txt"), acl::ACCESS_XATTR);
    symlink(&outside, fx.home().join("docs/out")).unwrap();
    symlink(&outside, fx.profile("code").join("config/out")).unwrap();
    fx.give(&fx.profile("code").join("config/out"));
    fx.apply().await.unwrap();
    assert_eq!(fx.owner(&outside), euid());
    assert_eq!(fx.owner(&outside.join("secret.txt")), euid());
    assert_eq!(
        read_acl(&outside.join("secret.txt"), acl::ACCESS_XATTR),
        outside_acl
    );
    // The profile's link moved as a link.
    let moved = fx.home().join(".config/out");
    assert!(std::fs::symlink_metadata(&moved).unwrap().is_symlink());
    fx.rollback().await.unwrap();
    assert!(
        std::fs::symlink_metadata(fx.profile("code").join("config/out"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        read_acl(&outside.join("secret.txt"), acl::ACCESS_XATTR),
        outside_acl
    );
}

#[tokio::test]
async fn a_profile_of_another_identity_is_never_merged() {
    let fx = fixture();
    populate(&fx);
    // A profile folder of rast-u8, renamed into place by the service.
    let foreign = fx.options.profiles_dir().join("evil/7");
    write(&foreign.join("home/.bashrc"), "x");
    let scan = fx.scan().await;
    assert!(
        sample_paths(&scan["merge"]["skipped"]).contains(
            &"evil: the profile does not belong to the account (not the account's)".to_string()
        ) || sample_paths(&scan["merge"]["skipped"])
            .iter()
            .any(|line| line.starts_with("evil:")),
        "{scan}"
    );
    fx.apply().await.unwrap();
    assert!(!fx.home().join(".bashrc").exists());
    assert!(foreign.join("home/.bashrc").is_file());
}

#[tokio::test]
async fn a_tampered_journal_is_refused() {
    let fx = fixture();
    populate(&fx);
    std::fs::write(fx.root().join("usermod.fail"), b"").unwrap();
    fx.apply().await.unwrap_err();
    let journal = fx.journal_root().join(&fx.journals()[0]).join(JOURNAL_FILE);
    let text = std::fs::read_to_string(&journal).unwrap();
    // A line that would give an entry to root.
    let tampered = text.replacen(&format!("\"uid\":{}", euid()), "\"uid\":0", 2);
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&journal, tampered).unwrap();
    let error = fx.rollback().await.unwrap_err();
    assert_eq!(error.code, "forbidden", "{error:?}");
    // A journal others can read is refused too.
    std::fs::write(&journal, text).unwrap();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(fx.rollback().await.unwrap_err().code, "forbidden");
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
    // An unknown field in the plan.
    let plan = fx.journal_root().join(&fx.journals()[0]).join(PLAN_FILE);
    let original = std::fs::read_to_string(&plan).unwrap();
    std::fs::write(&plan, original.replacen("{\"op\"", "{\"x\":1,\"op\"", 1)).unwrap();
    assert_eq!(fx.rollback().await.unwrap_err().code, "forbidden");
    std::fs::write(&plan, original).unwrap();
    fx.rollback().await.unwrap();
}

#[tokio::test]
async fn running_processes_and_units_that_do_not_stop_block_the_migration() {
    let fx = fixture();
    populate(&fx);
    let pid = fx.root().join("proc/4242");
    std::fs::create_dir_all(&pid).unwrap();
    std::fs::write(
        pid.join("status"),
        format!("Name:\tcode\nUid:\t{UID}\t{UID}\t{UID}\t{UID}\nGroups:\t\n"),
    )
    .unwrap();
    let scan = fx.scan().await;
    assert_eq!(scan["processes"], 1);
    let error = fx.apply().await.unwrap_err();
    assert_eq!(error.code, "identity_in_use", "{error:?}");
    assert!(fx.journals().is_empty());
    assert_eq!(fx.owner(&fx.home()), euid());
    std::fs::remove_dir_all(&pid).unwrap();

    std::fs::write(fx.root().join("units"), "rast-term-u200007-stuck\n").unwrap();
    assert_eq!(fx.scan().await["units"], json!(["rast-term-u200007-stuck"]));
    let error = fx.apply().await.unwrap_err();
    assert_eq!(error.code, "identity_in_use");
    assert!(error.message.contains("rast-term-u200007-stuck"));
    // Units of other accounts are not this account's.
    std::fs::write(
        fx.root().join("units"),
        "rast-app-u200008-code-1\nrast-xpra-u70\n",
    )
    .unwrap();
    fx.apply().await.unwrap();
}

#[tokio::test]
async fn requests_are_validated() {
    let fx = fixture();
    for (user_id, username) in [(0, USER), (7, "Alice"), (7, "../x"), (7, "")] {
        let op = HomeMigrateOp {
            user_id,
            username: username.into(),
        };
        let error = home_migrate_scan(&fx.options, op).await.unwrap_err();
        assert_eq!(error.code, "invalid_request", "{user_id} {username}");
    }
    // No identity yet: nothing to migrate.
    let mut options = fx.options.clone();
    options.getent_bin = write_script(fx.root(), "getent-none", "#!/bin/sh\nexit 2\n");
    let error = home_migrate_apply(&options, fx.op()).await.unwrap_err();
    assert_eq!(error.code, "not_found");
    // A passwd home elsewhere is a conflict.
    std::fs::write(fx.root().join("passwd-home"), "/etc").unwrap();
    assert_eq!(fx.apply().await.unwrap_err().code, "identity_conflict");
}

#[tokio::test]
async fn a_home_of_another_owner_or_a_link_is_refused() {
    let fx = fixture();
    // A HOME that is a link to the identity home.
    std::fs::remove_dir(fx.home()).unwrap();
    symlink(fx.identity_home(), fx.home()).unwrap();
    assert_eq!(fx.apply().await.unwrap_err().code, "forbidden");
    std::fs::remove_file(fx.home()).unwrap();
    std::fs::create_dir(fx.home()).unwrap();
    fx.owners
        .lock()
        .unwrap()
        .insert(key(&fx.home()), (1001, 1001));
    assert_eq!(fx.scan().await["mode"], "foreign");
    assert_eq!(fx.apply().await.unwrap_err().code, "identity_conflict");
    // A HOME made in one-HOME mode needs no migration.
    fx.give(&fx.home());
    std::fs::write(
        fx.root().join("passwd-home"),
        fx.home().display().to_string(),
    )
    .unwrap();
    let applied = fx.apply().await.unwrap();
    assert_eq!(applied["alreadyOneHome"], true);
    assert_eq!(applied["journal"], Value::Null);
}

// ---------------------------------------------------------------------------
// The recycle bin
// ---------------------------------------------------------------------------

const ID1: &str = "00000000-0000-4000-8000-000000000001";
const ID2: &str = "00000000-0000-4000-8000-000000000002";
const ID3: &str = "00000000-0000-4000-8000-000000000003";
const ID4: &str = "00000000-0000-4000-8000-000000000004";
const MISSING: &str = "00000000-0000-4000-8000-00000000000f";
/// 2026-11-16 10:00:00 UTC.
const DELETED: i64 = 1_794_823_200_000;

fn entry(id: &str, path: &str, container: Option<&str>) -> LegacyTrashEntry {
    LegacyTrashEntry {
        id: id.into(),
        deleted_at: DELETED,
        path: path.into(),
        container: container.map(str::to_string),
    }
}

async fn trash(fx: &Fixture, entries: Vec<LegacyTrashEntry>) -> Result<Value, NativeError> {
    home_migrate_trash(
        &fx.options,
        HomeTrashOp {
            user_id: 7,
            username: USER.into(),
            entries,
        },
    )
    .await
}

fn statuses(reply: &Value) -> Vec<(String, String)> {
    reply["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| {
            (
                result["status"].as_str().unwrap().to_string(),
                result["name"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_legacy_recycle_bin_moves_into_xdg_trash_and_back() {
    let fx = fixture();
    let legacy = fx.legacy_trash();
    write(&legacy.join(ID1), "one");
    write(&legacy.join(ID2).join("inner.txt"), "two");
    write(&legacy.join(ID3), "three");
    write(&legacy.join(ID4), "team");
    let team = fx.options.storage_dir.join("shared/team");
    std::fs::create_dir_all(&team).unwrap();
    // A migrated shared folder: set-gid, with the rw group (3000 here).
    std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o2770)).unwrap();
    fx.owners
        .lock()
        .unwrap()
        .insert(key(&team), (u32::MAX - 15, 3000));
    // Before the HOME is migrated, nothing moves.
    let error = trash(&fx, vec![entry(ID1, "docs/a.txt", None)])
        .await
        .unwrap_err();
    assert_eq!(error.code, "forbidden");
    fx.apply().await.unwrap();

    let reply = trash(
        &fx,
        vec![
            entry(ID1, "docs/a.txt", None),
            entry(ID2, "old folder", None),
            entry(ID3, "elsewhere/a.txt", None),
            entry(ID4, "plans/q4.txt", Some("team")),
            entry(MISSING, "gone.txt", None),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        statuses(&reply),
        [
            ("moved".into(), "a.txt".into()),
            ("moved".into(), "old folder".into()),
            ("moved".into(), "a.txt.2".into()),
            ("moved".into(), "q4.txt".into()),
            ("missing".into(), String::new()),
        ],
        "{reply}"
    );
    let home_trash = fx.home().join(".local/share/Trash");
    assert_eq!(
        std::fs::read_to_string(home_trash.join("files/a.txt")).unwrap(),
        "one"
    );
    assert!(home_trash.join("files/old folder/inner.txt").is_file());
    let info = std::fs::read_to_string(home_trash.join("info/a.txt.2.trashinfo")).unwrap();
    assert_eq!(
        rast_base::storage::xdg_trash::parse_info(info.as_bytes())
            .unwrap()
            .path,
        format!("{}/elsewhere/a.txt", fx.home().display())
    );
    assert!(info.contains("DeletionDate=2026-11-16T"));
    for path in [
        home_trash.clone(),
        home_trash.join("files/a.txt"),
        home_trash.join("files/old folder"),
        home_trash.join("files/old folder/inner.txt"),
        home_trash.join("info/a.txt.trashinfo"),
    ] {
        assert_eq!(fx.owner(&path), UID, "{}", path.display());
    }
    let team_trash = fx
        .options
        .storage_dir
        .join("shared/team")
        .join(rast_base::storage::xdg_trash::share_trash_name(UID));
    let info = std::fs::read_to_string(team_trash.join("info/q4.txt.trashinfo")).unwrap();
    assert!(info.contains("Path=plans/q4.txt"), "{info}");
    assert_eq!(fx.owner(&team_trash), UID);
    // The shared folder's bin keeps its group and set-gid bit, like the
    // agent's; its entries get that group.
    assert_eq!(mode(&team_trash), 0o2700);
    let group = |path: &Path| {
        fx.owners
            .lock()
            .unwrap()
            .get(&key(path))
            .copied()
            .unwrap()
            .1
    };
    assert_eq!(
        group(&team_trash.join("files/q4.txt")),
        group(&team_trash.join("files"))
    );
    assert_eq!(group(&home_trash.join("files/a.txt")), UID);
    assert!(!legacy.join(ID1).exists());

    // The same batch again (a run that stopped before the rows were
    // deleted): nothing moves twice.
    let again = trash(&fx, vec![entry(ID1, "docs/a.txt", None)])
        .await
        .unwrap();
    assert_eq!(statuses(&again), [("already".into(), "a.txt".into())]);

    let rolled = fx.rollback().await.unwrap();
    let mut restored: Vec<String> = rolled["report"]["trashRestored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    restored.sort();
    assert_eq!(restored, [ID1, ID2, ID3, ID4]);
    for id in [ID1, ID3, ID4] {
        assert!(legacy.join(id).is_file(), "{id}");
        assert_eq!(fx.owner(&legacy.join(id)), euid());
    }
    assert_eq!(fx.owner(&legacy.join(ID2).join("inner.txt")), euid());
    // The trash folders the migration made are gone again.
    assert!(!fx.home().join(".local").exists());
    assert!(!team_trash.exists());
}

#[tokio::test]
async fn recycle_bin_batches_are_validated() {
    let fx = fixture();
    fx.apply().await.unwrap();
    let bad = [
        vec![],
        vec![entry("not-a-uuid", "a", None)],
        vec![entry("ABCDEF00-0000-4000-8000-000000000001", "a", None)],
        vec![entry(ID1, "../a", None)],
        vec![entry(ID1, "/etc/passwd", None)],
        vec![entry(ID1, "a//b", None)],
        vec![entry(ID1, "a", Some("a/b"))],
        vec![entry(ID1, "a", None), entry(ID1, "b", None)],
        (0..=MAX_TRASH_BATCH)
            .map(|n| entry(&format!("00000000-0000-4000-8000-{n:012x}"), "a", None))
            .collect(),
    ];
    for entries in bad {
        let count = entries.len();
        let error = trash(&fx, entries).await.unwrap_err();
        assert_eq!(error.code, "invalid_request", "{count} entries");
    }
    let mut negative = entry(ID1, "a", None);
    negative.deleted_at = -1;
    assert_eq!(
        trash(&fx, vec![negative]).await.unwrap_err().code,
        "invalid_request"
    );
}

#[test]
fn stripping_keeps_foreign_entries_and_drops_identity_ones() {
    let owners = Owners {
        root: 0,
        rast: 999,
        uid: UID,
        gid: UID,
        identities: 200_001..=1_200_000,
        test_mode: true,
        overlay: None,
    };
    let only_grant = Acl::new(vec![
        acl_entry(Tag::UserObj, 7),
        acl_entry(Tag::User(UID), 5),
        acl_entry(Tag::GroupObj, 4),
        acl_entry(Tag::Mask, 5),
        acl_entry(Tag::Other, 4),
    ]);
    let stripped = strip(Some(&only_grant.encode()), &owners);
    assert!(stripped.identity);
    assert_eq!(stripped.action, AclAction::Remove { group: 4 });
    assert_eq!(stripped.foreign, None);
    let none = strip(None, &owners);
    assert_eq!(none.action, AclAction::Keep);
    let foreign = Acl::new(vec![
        acl_entry(Tag::UserObj, 7),
        acl_entry(Tag::Group(3000), 5),
        acl_entry(Tag::GroupObj, 0),
        acl_entry(Tag::Mask, 5),
        acl_entry(Tag::Other, 0),
    ]);
    let kept = strip(Some(&foreign.encode()), &owners);
    assert!(!kept.identity);
    assert_eq!(kept.action, AclAction::Keep);
    assert_eq!(kept.foreign.as_deref(), Some("g:3000:r-x"));
    // Garbage is left alone.
    assert_eq!(strip(Some(b"junk"), &owners).action, AclAction::Keep);
}

#[test]
fn plan_steps_and_roots_are_strict() {
    for key in ["identity", "code/config", "code/home", "code/cache"] {
        assert_eq!(Root::parse(key).unwrap().key(), key);
    }
    for key in [
        "",
        "code",
        "code/tmp",
        "../x/config",
        "code/config/x",
        "Code/config",
    ] {
        assert!(Root::parse(key).is_none(), "{key}");
    }
    let step = |op, dst: &str, root: Option<&str>, src: &str| Step {
        op,
        dst: dst.into(),
        root: root.map(str::to_string),
        src: src.into(),
        d: false,
        dev: 1,
        ino: 1,
    };
    assert!(step(StepKind::Move, ".config", Some("code/config"), "")
        .validate()
        .is_ok());
    assert!(step(StepKind::Mkdir, ".local", None, "").validate().is_ok());
    for bad in [
        step(StepKind::Move, "", Some("code/config"), ""),
        step(StepKind::Move, "../x", Some("code/config"), ""),
        step(StepKind::Move, "x", Some("identity"), ""),
        step(StepKind::Move, "x", Some("code/config"), "a/../b"),
        step(StepKind::Move, "x", None, "a"),
        step(StepKind::Mkdir, "x", Some("code/config"), ""),
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    assert!(valid_uuid(ID1));
    assert!(valid_uuid("abcdef00-0000-4000-8000-000000000001"));
    assert!(!valid_uuid("ABCDEF00-0000-4000-8000-000000000001"));
    assert!(!valid_uuid("00000000-0000-4000-8000-00000000000g"));
}
