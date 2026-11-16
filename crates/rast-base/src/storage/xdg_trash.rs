//! The freedesktop.org Trash specification 1.0 as data (ADR 0005, section 5):
//! `.trashinfo` files, the `directorysizes` cache, the opaque recycle-bin ids
//! of one-HOME accounts and the mapping of an entry's original location to a
//! logical path.
//!
//! Everything here is pure: the per-user file agent does the file system
//! work. A trash belongs to the user, so every app and terminal of the user
//! (and, in a shared folder, every member until the folder is closed) can
//! write into it: `.trashinfo` and `directorysizes` contents are untrusted
//! input. Parsing is bounded, a `Path` must decode to UTF-8 without control
//! characters, and [`home_origin`] and [`share_origin`] accept only plain
//! names below the root they are asked about: a crafted `Path` can never name
//! a place outside HOME or outside the share container that holds the trash.
//!
//! Layout:
//!
//! ```text
//! ~/.local/share/Trash/{files,info}/      the home trash
//! ~/.local/share/Trash/directorysizes     "<bytes> <info mtime> <encoded name>"
//! <container>/.Trash-<uid>/{files,info}/  the trash of a shared folder
//! ```

use std::collections::BTreeMap;

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::storage::safe_path::validate_component;

/// Suffix of the files in `info/`.
pub const INFO_SUFFIX: &str = ".trashinfo";
/// Name of the size cache next to `files/` and `info/`.
pub const DIRECTORY_SIZES: &str = "directorysizes";
/// The home trash, relative to HOME.
pub const HOME_TRASH: [&str; 3] = [".local", "share", "Trash"];
/// `$XDG_DATA_HOME` relative to HOME: relative `Path`s of the home trash
/// start there (the specification's "directory in which the trash
/// directory resides").
pub const DATA_HOME: [&str; 2] = [".local", "share"];
/// Largest `.trashinfo` file read.
pub const MAX_INFO_BYTES: usize = 16 * 1024;
/// Largest `directorysizes` file read.
pub const MAX_DIRECTORY_SIZES_BYTES: usize = 4 * 1024 * 1024;
/// Prefix of the ids of home trash entries.
const HOME_ID: &str = "h:";
/// Prefix of the ids of shared-folder trash entries (`s:<shareId>:<name>`).
const SHARE_ID: &str = "s:";

/// Characters escaped in `Path`: everything but the unreserved characters,
/// `/` and the sub-delimiters GIO leaves alone (RFC 2396, as the
/// specification asks).
const PATH_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@');

/// Whether `name` is a trash folder at the top of a shared folder
/// (`.Trash` or `.Trash-<uid>`): hidden in listings and never served as a
/// path.
pub fn is_trash_dir_name(name: &str) -> bool {
    name == ".Trash" || name.starts_with(".Trash-")
}

/// The trash folder of `uid` at the top of a shared folder.
pub fn share_trash_name(uid: u32) -> String {
    format!(".Trash-{uid}")
}

/// The name of the info file of the trash entry `name`.
pub fn info_name(name: &str) -> String {
    format!("{name}{INFO_SUFFIX}")
}

/// The trash entry an info file name stands for.
pub fn entry_name(info_file: &str) -> Option<&str> {
    info_file
        .strip_suffix(INFO_SUFFIX)
        .filter(|name| validate_component(name).is_ok())
}

/// Percent-encode a `Path` value.
pub fn encode_path(path: &str) -> String {
    utf8_percent_encode(path, PATH_ESCAPE).to_string()
}

/// Decode a `Path` value: it must be UTF-8 without control characters.
pub fn decode_path(value: &str) -> Option<String> {
    let decoded = percent_decode_str(value).decode_utf8().ok()?;
    if decoded.is_empty() || decoded.chars().any(char::is_control) {
        return None;
    }
    Some(decoded.into_owned())
}

/// What one `.trashinfo` file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashInfo {
    /// The decoded `Path`: absolute, or relative to the folder that holds
    /// the trash folder.
    pub path: String,
    /// `DeletionDate` in milliseconds since 1970, when it parses.
    pub deleted_ms: Option<i64>,
}

/// The contents of a `.trashinfo` file.
pub fn format_info(path: &str, deleted_ms: i64) -> String {
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_path(path),
        format_local(deleted_ms)
    )
}

/// Parse a `.trashinfo` file. The first group must be `[Trash Info]` and
/// hold one valid `Path`; other groups, comments and unknown keys are
/// ignored, as desktop entries allow.
pub fn parse_info(bytes: &[u8]) -> Option<TrashInfo> {
    if bytes.len() > MAX_INFO_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut in_group = false;
    let mut seen_group = false;
    let mut path: Option<String> = None;
    let mut date: Option<i64> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if seen_group && in_group {
                break;
            }
            in_group = line == "[Trash Info]";
            if !in_group {
                // The specification's group comes first.
                return None;
            }
            seen_group = true;
            continue;
        }
        if !in_group {
            return None;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Path" => {
                if path.is_some() {
                    return None;
                }
                path = Some(decode_path(value.trim())?);
            }
            "DeletionDate" => date = parse_local(value.trim()),
            _ => {}
        }
    }
    Some(TrashInfo {
        path: path?,
        deleted_ms: date,
    })
}

/// `YYYY-MM-DDThh:mm:ss` in local time, as the specification asks.
pub fn format_local(ms: i64) -> String {
    let seconds = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is a plain out-parameter; localtime_r is thread-safe.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&seconds, &mut tm).is_null() };
    if !ok {
        return "1970-01-01T00:00:00".to_string();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Parse a local `YYYY-MM-DDThh:mm:ss` date (fractions and a trailing
/// zone are not part of the format and are refused).
pub fn parse_local(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i32> {
        let part = value.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // SAFETY: `tm` is fully initialised; mktime only reads and normalises it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = second;
    tm.tm_isdst = -1;
    let seconds = unsafe { libc::mktime(&mut tm) };
    if seconds == -1 && !(year == 1969 && month == 12 && day == 31) {
        return None;
    }
    // A day that does not exist (31 April) was normalised into the next month.
    if tm.tm_mday != day || tm.tm_mon != month - 1 {
        return None;
    }
    #[allow(clippy::useless_conversion)] // time_t is i32 on some targets
    i64::from(seconds).checked_mul(1000)
}

/// One line of `directorysizes`: the size of a trashed folder, valid while
/// its info file keeps the modification time `mtime` (seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirSize {
    pub bytes: u64,
    pub mtime: i64,
}

/// Parse `directorysizes`, ignoring lines that do not parse.
pub fn parse_directory_sizes(bytes: &[u8]) -> BTreeMap<String, DirSize> {
    let mut out = BTreeMap::new();
    let bytes = &bytes[..bytes.len().min(MAX_DIRECTORY_SIZES_BYTES)];
    let Ok(text) = std::str::from_utf8(bytes) else {
        return out;
    };
    for line in text.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(size), Some(mtime), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(size), Ok(mtime)) = (size.parse::<u64>(), mtime.parse::<i64>()) else {
            continue;
        };
        let Some(name) = decode_path(name) else {
            continue;
        };
        if validate_component(&name).is_ok() {
            out.insert(name, DirSize { bytes: size, mtime });
        }
    }
    out
}

/// The contents of `directorysizes`.
pub fn format_directory_sizes(sizes: &BTreeMap<String, DirSize>) -> String {
    let mut out = String::new();
    for (name, size) in sizes {
        // `/` cannot be part of a name; encode it anyway for the format.
        let encoded = utf8_percent_encode(name, PATH_ESCAPE)
            .to_string()
            .replace('/', "%2F");
        out.push_str(&format!("{} {} {encoded}\n", size.bytes, size.mtime));
    }
    out
}

/// Which trash an id names, and the entry's name in its `files/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashId {
    Home(String),
    Share { share_id: String, name: String },
}

impl TrashId {
    /// The opaque id of the API (`TrashEntryDto.id`).
    pub fn encode(&self) -> String {
        match self {
            Self::Home(name) => format!("{HOME_ID}{}", hex::encode(name)),
            Self::Share { share_id, name } => {
                format!("{SHARE_ID}{share_id}:{}", hex::encode(name))
            }
        }
    }

    /// Parse an id from a request. The share id is checked by the caller
    /// against the shares the server named.
    pub fn parse(id: &str) -> Option<Self> {
        let name = |hex_name: &str| -> Option<String> {
            if hex_name.len() > 2 * crate::storage::MAX_NAME_BYTES {
                return None;
            }
            let name = String::from_utf8(hex::decode(hex_name).ok()?).ok()?;
            validate_component(&name).ok().map(|_| name)
        };
        if let Some(rest) = id.strip_prefix(HOME_ID) {
            return name(rest).map(Self::Home);
        }
        let rest = id.strip_prefix(SHARE_ID)?;
        let (share_id, hex_name) = rest.split_once(':')?;
        let valid_share = share_id.len() == 36
            && share_id
                .bytes()
                .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid_share {
            return None;
        }
        Some(Self::Share {
            share_id: share_id.to_string(),
            name: name(hex_name)?,
        })
    }

    /// The share id of a shared-folder entry.
    pub fn share_id(&self) -> Option<&str> {
        match self {
            Self::Home(_) => None,
            Self::Share { share_id, .. } => Some(share_id),
        }
    }

    /// The name in `files/`.
    pub fn name(&self) -> &str {
        match self {
            Self::Home(name) | Self::Share { name, .. } => name,
        }
    }
}

/// Plain names below a root: the segments of `rest` (`a/b/c`), every one a
/// valid component.
fn segments(rest: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = rest.split('/').map(str::to_string).collect();
    if parts.is_empty() || parts.iter().any(|part| validate_component(part).is_err()) {
        return None;
    }
    Some(parts)
}

/// `path` below the absolute folder `root`, as segments.
fn below(path: &str, root: &str) -> Option<Vec<String>> {
    let root = root.trim_end_matches('/');
    let rest = path.strip_prefix(root)?.strip_prefix('/')?;
    segments(rest)
}

/// Where an entry of the home trash came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeOrigin {
    /// Segments below HOME.
    Home(Vec<String>),
    /// A shared folder's container (a direct child of the shared root) and
    /// segments below it: a file an app moved from a shared folder into the
    /// home trash (both on one file system).
    Share {
        container: String,
        segments: Vec<String>,
    },
}

/// The original location of a home trash entry, when it is a plain path
/// below HOME or below a container of the shared root (`home` and
/// `shared_root` canonical). Relative paths start at `~/.local/share`.
/// `None`: anywhere else, or not a plain path (`..`, `.`, `//`).
pub fn home_origin(path: &str, home: &str, shared_root: &str) -> Option<HomeOrigin> {
    if !path.starts_with('/') {
        let mut out: Vec<String> = DATA_HOME.iter().map(|s| s.to_string()).collect();
        out.extend(segments(path)?);
        return Some(HomeOrigin::Home(out));
    }
    if let Some(found) = below(path, home) {
        return Some(HomeOrigin::Home(found));
    }
    let mut found = below(path, shared_root)?;
    let container = found.remove(0);
    if found.is_empty() {
        return None;
    }
    Some(HomeOrigin::Share {
        container,
        segments: found,
    })
}

/// The original location of an entry of a shared folder's trash, below
/// its container (`container` is its canonical absolute path): relative
/// paths start at the container. `None`: anywhere else, or not a plain path.
pub fn share_origin(path: &str, container: &str) -> Option<Vec<String>> {
    if path.starts_with('/') {
        below(path, container)
    } else {
        segments(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `gio trash` wrote on this host (GLib 2.86, 2026-11-16) for
    /// `My file 100%.txt`, `Ünï cødé #?.md` and the folder `a dir`, with
    /// the test HOME renamed to `/home/alice`.
    const GIO_FIXTURES: [(&str, &str); 3] = [
        (
            "[Trash Info]\nPath=/home/alice/docs/My%20file%20100%25.txt\nDeletionDate=2026-11-16T19:47:34\n",
            "/home/alice/docs/My file 100%.txt",
        ),
        (
            "[Trash Info]\nPath=/home/alice/docs/%C3%9Cn%C3%AF%20c%C3%B8d%C3%A9%20%23%3F.md\nDeletionDate=2026-11-16T19:47:34\n",
            "/home/alice/docs/Ünï cødé #?.md",
        ),
        (
            "[Trash Info]\nPath=/home/alice/docs/a%20dir\nDeletionDate=2026-11-16T19:47:34\n",
            "/home/alice/docs/a dir",
        ),
    ];

    #[test]
    fn files_written_by_gio_are_read() {
        let date = parse_local("2026-11-16T19:47:34").unwrap();
        for (text, path) in GIO_FIXTURES {
            let info = parse_info(text.as_bytes()).unwrap();
            assert_eq!(info.path, path);
            assert_eq!(info.deleted_ms, Some(date));
            // What RAST writes reads back the same, and GIO's escaping of
            // these names is reproduced exactly.
            assert_eq!(format_info(path, date), text);
        }
    }

    #[test]
    fn paths_round_trip_through_the_encoding() {
        for path in [
            "/home/a/with space",
            "/home/a/100%",
            "/home/a/Ünïcødé/日本語.txt",
            "relative/name#1?.txt",
            "/home/a/semi;colon=and+plus@x:y",
        ] {
            let encoded = encode_path(path);
            assert!(!encoded.contains(' ') && !encoded.contains('#') && !encoded.contains('?'));
            assert_eq!(decode_path(&encoded).as_deref(), Some(path));
            let info = parse_info(format_info(path, 0).as_bytes()).unwrap();
            assert_eq!(info.path, path);
        }
        // A newline is a control character: never a valid path, so an
        // info file can never carry a second key through it.
        assert_eq!(decode_path("/home/a/x%0APath=/etc"), None);
        assert_eq!(decode_path("/home/a/x%00"), None);
        assert_eq!(decode_path("%FF%FE"), None);
        assert_eq!(decode_path(""), None);
    }

    #[test]
    fn malformed_info_files_are_refused() {
        for bad in [
            "",
            "Path=/a\n",
            "[Desktop Entry]\nPath=/a\n",
            "[Trash Info]\nDeletionDate=2026-01-01T00:00:00\n",
            "[Trash Info]\nPath=/a\nPath=/b\n",
            "[Trash Info]\nPath=/a%0Ab\n",
            "[Trash Info]\nPath=\n",
        ] {
            assert_eq!(parse_info(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(parse_info(&[0xff, 0xfe]), None);
        let huge = format!("[Trash Info]\nPath=/a\n#{}\n", "x".repeat(MAX_INFO_BYTES));
        assert_eq!(parse_info(huge.as_bytes()), None);
        // Comments, CRLF, unknown keys and later groups are fine; a bad
        // date only loses the date.
        let info = parse_info(
            b"# c\r\n[Trash Info]\r\nX-Other=1\r\nPath=/a/b\r\nDeletionDate=yesterday\r\n[Later]\r\nPath=/x\r\n",
        )
        .unwrap();
        assert_eq!(info.path, "/a/b");
        assert_eq!(info.deleted_ms, None);
    }

    #[test]
    fn local_dates_round_trip_and_bad_ones_are_refused() {
        for text in [
            "2026-11-16T19:47:34",
            "2000-02-29T00:00:00",
            "1999-12-31T23:59:59",
        ] {
            let ms = parse_local(text).unwrap();
            assert_eq!(format_local(ms), text);
        }
        for bad in [
            "2026-11-16 19:47:34",
            "2026-11-16T19:47:34Z",
            "2026-11-16T19:47:34.5",
            "2026-13-01T00:00:00",
            "2026-04-31T00:00:00",
            "2026-11-16T24:00:00",
            "+026-11-16T19:47:34",
            "",
        ] {
            assert_eq!(parse_local(bad), None, "{bad}");
        }
    }

    #[test]
    fn directory_sizes_round_trip_and_ignore_garbage() {
        let mut sizes = BTreeMap::new();
        sizes.insert(
            "a dir".to_string(),
            DirSize {
                bytes: 42,
                mtime: 1_700_000_000,
            },
        );
        sizes.insert("100% ünï".to_string(), DirSize { bytes: 7, mtime: 1 });
        let text = format_directory_sizes(&sizes);
        assert_eq!(parse_directory_sizes(text.as_bytes()), sizes);
        let parsed =
            parse_directory_sizes(b"x y z\n12 34\n5 6 ..\n5 6 %2E%2E\n-1 6 n\n9 9 ok\n9 9 a%0Ab\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["ok"].bytes, 9);
    }

    #[test]
    fn ids_are_opaque_and_strict() {
        let home = TrashId::Home("my file.2".to_string());
        assert_eq!(TrashId::parse(&home.encode()), Some(home.clone()));
        assert!(home.encode().starts_with("h:"));
        let share = TrashId::Share {
            share_id: "00000000-0000-4000-8000-000000000001".to_string(),
            name: "Report: Q4?".to_string(),
        };
        assert_eq!(TrashId::parse(&share.encode()), Some(share.clone()));
        assert_eq!(
            share.share_id(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        for bad in [
            "",
            "h:",
            "h:zz",
            &format!("h:{}", hex::encode("..")),
            &format!("h:{}", hex::encode("a/b")),
            &format!("h:{}", hex::encode("a\nb")),
            &format!("s:not-a-uuid:{}", hex::encode("a")),
            &format!(
                "s:00000000-0000-4000-8000-00000000000Z:{}",
                hex::encode("a")
            ),
            "s:00000000-0000-4000-8000-000000000001",
            "x:61",
            "1b2c3d",
        ] {
            assert_eq!(TrashId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn origins_stay_below_their_root() {
        let home = "/srv/rast/homes/alice";
        let shared = "/srv/rast/shared";
        let segs = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            home_origin("/srv/rast/homes/alice/docs/a b", home, shared),
            Some(HomeOrigin::Home(segs(&["docs", "a b"])))
        );
        assert_eq!(
            home_origin("notes/x", home, shared),
            Some(HomeOrigin::Home(segs(&[".local", "share", "notes", "x"])))
        );
        assert_eq!(
            home_origin("/srv/rast/shared/team-1/plan.odt", home, shared),
            Some(HomeOrigin::Share {
                container: "team-1".to_string(),
                segments: segs(&["plan.odt"]),
            })
        );
        for outside in [
            "/etc/passwd",
            "/srv/rast/homes/alice",
            "/srv/rast/homes/alice/",
            "/srv/rast/homes/alicex/a",
            "/srv/rast/homes/alice/../bob/a",
            "/srv/rast/homes/alice/./a",
            "/srv/rast/homes/alice//a",
            "/srv/rast/shared/team-1",
            "/srv/rast/shared/../homes/bob/a",
            "../x",
            "a/../../x",
            "a\\b/c",
        ] {
            assert_eq!(home_origin(outside, home, shared), None, "{outside}");
        }
        assert_eq!(home_origin("../x", home, shared), None);
        let container = "/srv/rast/shared/team-1";
        assert_eq!(
            share_origin("sub/a.txt", container),
            Some(segs(&["sub", "a.txt"]))
        );
        assert_eq!(
            share_origin("/srv/rast/shared/team-1/a.txt", container),
            Some(segs(&["a.txt"]))
        );
        for outside in [
            "/srv/rast/shared/team-2/a.txt",
            "/srv/rast/shared/team-10/a.txt",
            "/srv/rast/homes/alice/a",
            "../team-2/a",
            "a/../../b",
            "/srv/rast/shared/team-1",
        ] {
            assert_eq!(share_origin(outside, container), None, "{outside}");
        }
    }

    #[test]
    fn trash_folders_of_shared_folders_are_recognised() {
        assert!(is_trash_dir_name(".Trash"));
        assert!(is_trash_dir_name(".Trash-200003"));
        assert!(is_trash_dir_name(".Trash-x"));
        assert!(!is_trash_dir_name(".Trashcan"));
        assert!(!is_trash_dir_name("Trash"));
        assert_eq!(share_trash_name(200003), ".Trash-200003");
        assert_eq!(entry_name("a b.trashinfo"), Some("a b"));
        assert_eq!(entry_name(".trashinfo"), None);
        assert_eq!(entry_name("a.txt"), None);
    }
}
