//! The tar format of the backup reader (ADR 0005, D12).
//!
//! One small, strict subset of POSIX pax, written by the reader and read
//! back by the server, which then writes the same subset into the backup
//! archive under `users/homes/<name>/` or `users/shared/<container>/`:
//!
//! * ustar headers (`ustar\0` `00`) with numeric owners only (empty user and
//!   group names, so `tar --same-owner` restores the uid and gid);
//! * entry types: regular file, directory, symbolic link (stored, never
//!   followed) and hard link (to a file stored earlier in the same tree);
//! * one optional pax extended header (`x`) per entry with the records
//!   `path`, `linkpath` (when longer than the ustar fields), `uid`, `gid`,
//!   `size`, `mtime` (when out of the ustar range), `hdrcharset=BINARY`
//!   (a name that is not UTF-8), and the POSIX ACLs as GNU tar and bsdtar
//!   write them: `SCHILY.acl.access` and `SCHILY.acl.default`, one entry
//!   per line with numeric ids (`user:1001:r-x`).
//!
//! The reader is root, but what it reads belongs to the users (names,
//! sizes, link targets, ACLs), so [`read_entry`] treats the stream as
//! untrusted: every header is checked (checksum, magic, octal fields), pax
//! headers are at most [`MAX_PAX_BYTES`], names and link targets at most
//! [`MAX_PATH_BYTES`], paths must be relative and normalised, and anything
//! else (GNU long names, global headers, devices, FIFOs, a prefix field,
//! unknown pax records) refuses the stream.
//!
//! [`read_backup_entry`] reads a whole Backup Center archive for
//! `rast restore-files` (review F1): the same subset, plus the GNU headers
//! that the backup's own writer (`tar::Builder`) uses for the files the
//! service read and for the database and configuration: regular files,
//! with a GNU long name (`L`) before a path longer than 100 bytes.

use std::fmt;
use std::io::{self, Read, Write};

use crate::native::acl::{Acl, Entry, Tag};

/// Size of one tar block.
pub const BLOCK: usize = 512;
/// Longest path or hard link target accepted (bytes).
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest symbolic link target accepted (bytes; Linux allows 4095).
pub const MAX_LINK_BYTES: usize = 4096;
/// Largest pax extended header accepted.
pub const MAX_PAX_BYTES: usize = 64 * 1024;
/// Most entries accepted in one ACL.
pub const MAX_ACL_ENTRIES: usize = 1024;

/// Largest value of a 7-digit octal field (mode, uid, gid).
const MAX_OCTAL_7: u64 = 0o7_777_777;
/// Largest value of an 11-digit octal field (size, mtime).
const MAX_OCTAL_11: u64 = 0o77_777_777_777;
/// Name of the pax header entries (as Python's tarfile and bsdtar write it).
const PAX_NAME: &[u8] = b"././@PaxHeader";
/// Magic and version of a GNU header (`ustar  \0`).
const GNU_MAGIC: &[u8] = b"ustar  \0";
/// Name of a GNU long name entry.
const GNU_LONG_NAME: &[u8] = b"././@LongLink";
/// The path of a tree's root folder in the reader's stream.
pub const ROOT: &[u8] = b".";

/// What an entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File {
        size: u64,
    },
    /// A symbolic link and its target, stored as it is.
    Symlink {
        target: Vec<u8>,
    },
    /// A second name of a file stored earlier (its archive path).
    HardLink {
        target: Vec<u8>,
    },
}

/// One entry header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMeta {
    /// The archive path, without a trailing `/` (a folder gets one when it
    /// is written).
    pub path: Vec<u8>,
    pub kind: Kind,
    /// Permission bits, set-uid, set-gid and sticky (`0o7777` at most).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Seconds since the epoch (0 for anything earlier).
    pub mtime: u64,
    pub acl_access: Option<Acl>,
    pub acl_default: Option<Acl>,
}

impl EntryMeta {
    /// The size of the data that follows the header.
    pub fn data_size(&self) -> u64 {
        match self.kind {
            Kind::File { size } => size,
            _ => 0,
        }
    }
}

/// Why a stream was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveError(pub String);

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArchiveError {}

fn invalid(message: impl Into<String>) -> ArchiveError {
    ArchiveError(message.into())
}

// ---------------------------------------------------------------------------
// ACL text (`user::rwx\nuser:1001:r-x\n…`, as GNU tar writes it)
// ---------------------------------------------------------------------------

fn perm_text(perm: u16) -> [u8; 3] {
    [
        if perm & 4 != 0 { b'r' } else { b'-' },
        if perm & 2 != 0 { b'w' } else { b'-' },
        if perm & 1 != 0 { b'x' } else { b'-' },
    ]
}

/// An ACL in the text form of `SCHILY.acl.*`: one entry per line, numeric
/// ids, no comments.
pub fn acl_text(acl: &Acl) -> String {
    acl.0
        .iter()
        .map(|entry| {
            let perm = String::from_utf8_lossy(&perm_text(entry.perm)).into_owned();
            match entry.tag {
                Tag::UserObj => format!("user::{perm}"),
                Tag::User(uid) => format!("user:{uid}:{perm}"),
                Tag::GroupObj => format!("group::{perm}"),
                Tag::Group(gid) => format!("group:{gid}:{perm}"),
                Tag::Mask => format!("mask::{perm}"),
                Tag::Other => format!("other::{perm}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the text form of `SCHILY.acl.*` strictly: entries separated by a
/// newline (or a comma), `user`, `group`, `mask` or `other`, numeric ids
/// only, `rwx` permissions; no duplicates.
pub fn parse_acl_text(text: &[u8]) -> Result<Acl, ArchiveError> {
    let text = std::str::from_utf8(text).map_err(|_| invalid("an ACL is not text"))?;
    let mut entries = Vec::new();
    for item in text.split(['\n', ',']) {
        if entries.len() >= MAX_ACL_ENTRIES {
            return Err(invalid("an ACL has too many entries"));
        }
        let mut fields = item.split(':');
        let (Some(tag), Some(qualifier), Some(perm), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(invalid(format!("invalid ACL entry {item:?}")));
        };
        let id = if qualifier.is_empty() {
            None
        } else if qualifier.len() <= 10 && qualifier.bytes().all(|b| b.is_ascii_digit()) {
            Some(
                qualifier
                    .parse::<u32>()
                    .map_err(|_| invalid(format!("invalid ACL id {qualifier:?}")))?,
            )
        } else {
            return Err(invalid(format!("invalid ACL id {qualifier:?}")));
        };
        let tag = match (tag, id) {
            ("user", None) => Tag::UserObj,
            ("user", Some(id)) => Tag::User(id),
            ("group", None) => Tag::GroupObj,
            ("group", Some(id)) => Tag::Group(id),
            ("mask", None) => Tag::Mask,
            ("other", None) => Tag::Other,
            _ => return Err(invalid(format!("invalid ACL entry {item:?}"))),
        };
        let perm = match perm.as_bytes() {
            [r, w, x]
                if matches!(r, b'r' | b'-')
                    && matches!(w, b'w' | b'-')
                    && matches!(x, b'x' | b'-') =>
            {
                (u16::from(*r == b'r') << 2) | (u16::from(*w == b'w') << 1) | u16::from(*x == b'x')
            }
            _ => return Err(invalid(format!("invalid ACL permissions {perm:?}"))),
        };
        if entries.iter().any(|known: &Entry| known.tag == tag) {
            return Err(invalid("an ACL names one entry twice"));
        }
        entries.push(Entry { tag, perm });
    }
    Ok(Acl::new(entries))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// One pax record: `<length> <key>=<value>\n`, where the length counts the
/// whole record including its own digits.
fn pax_record(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    let base = key.len() + value.len() + 3;
    let mut length = base + 1;
    loop {
        let total = base + length.to_string().len();
        if total == length {
            break;
        }
        length = total;
    }
    out.extend_from_slice(length.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn write_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&text.as_bytes()[text.len() - digits..]);
    field[digits] = 0;
}

fn copy_field(field: &mut [u8], value: &[u8]) {
    let length = value.len().min(field.len());
    field[..length].copy_from_slice(&value[..length]);
}

/// A ustar header block. Values that do not fit are written as 0 (their
/// pax record carries them).
fn ustar_block(name: &[u8], typeflag: u8, meta: &EntryMeta, size: u64, link: &[u8]) -> [u8; BLOCK] {
    let mut block = [0u8; BLOCK];
    copy_field(&mut block[0..100], name);
    write_octal(&mut block[100..108], u64::from(meta.mode & 0o7777));
    let fit7 = |value: u32| {
        let value = u64::from(value);
        if value <= MAX_OCTAL_7 {
            value
        } else {
            0
        }
    };
    let fit11 = |value: u64| if value <= MAX_OCTAL_11 { value } else { 0 };
    write_octal(&mut block[108..116], fit7(meta.uid));
    write_octal(&mut block[116..124], fit7(meta.gid));
    write_octal(&mut block[124..136], fit11(size));
    write_octal(&mut block[136..148], fit11(meta.mtime));
    block[156] = typeflag;
    copy_field(&mut block[157..257], link);
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    write_octal(&mut block[329..337], 0);
    write_octal(&mut block[337..345], 0);
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    let checksum = format!("{sum:06o}");
    block[148..154].copy_from_slice(&checksum.as_bytes()[checksum.len() - 6..]);
    block[154] = 0;
    block[155] = b' ';
    block
}

/// Zero bytes that pad `size` bytes of data to a whole block.
pub fn padding(size: u64) -> usize {
    (BLOCK - (size % BLOCK as u64) as usize) % BLOCK
}

const ZEROS: [u8; BLOCK] = [0u8; BLOCK];

/// Pad `size` bytes of data that were just written to a whole block.
pub fn write_padding<W: Write + ?Sized>(out: &mut W, size: u64) -> io::Result<()> {
    out.write_all(&ZEROS[..padding(size)])
}

/// The end of an archive: two zero blocks.
pub fn write_end<W: Write + ?Sized>(out: &mut W) -> io::Result<()> {
    out.write_all(&ZEROS)?;
    out.write_all(&ZEROS)
}

/// Write the header of one entry (a pax header first when it needs one).
/// The data of a file follows, then [`write_padding`].
pub fn write_header<W: Write + ?Sized>(out: &mut W, meta: &EntryMeta) -> io::Result<()> {
    let mut name = meta.path.clone();
    if meta.kind == Kind::Dir && !name.ends_with(b"/") {
        name.push(b'/');
    }
    let (typeflag, link, size): (u8, &[u8], u64) = match &meta.kind {
        Kind::Dir => (b'5', b"", 0),
        Kind::File { size } => (b'0', b"", *size),
        Kind::Symlink { target } => (b'2', target, 0),
        Kind::HardLink { target } => (b'1', target, 0),
    };
    let mut pax = Vec::new();
    let mut binary = false;
    if name.len() > 100 {
        binary |= std::str::from_utf8(&name).is_err();
        pax_record(&mut pax, "path", &name);
    }
    if link.len() > 100 {
        binary |= std::str::from_utf8(link).is_err();
        pax_record(&mut pax, "linkpath", link);
    }
    if u64::from(meta.uid) > MAX_OCTAL_7 {
        pax_record(&mut pax, "uid", meta.uid.to_string().as_bytes());
    }
    if u64::from(meta.gid) > MAX_OCTAL_7 {
        pax_record(&mut pax, "gid", meta.gid.to_string().as_bytes());
    }
    if size > MAX_OCTAL_11 {
        pax_record(&mut pax, "size", size.to_string().as_bytes());
    }
    if meta.mtime > MAX_OCTAL_11 {
        pax_record(&mut pax, "mtime", meta.mtime.to_string().as_bytes());
    }
    if binary {
        pax_record(&mut pax, "hdrcharset", b"BINARY");
    }
    if let Some(acl) = &meta.acl_access {
        pax_record(&mut pax, "SCHILY.acl.access", acl_text(acl).as_bytes());
    }
    if let Some(acl) = &meta.acl_default {
        pax_record(&mut pax, "SCHILY.acl.default", acl_text(acl).as_bytes());
    }
    if !pax.is_empty() {
        let header = EntryMeta {
            path: PAX_NAME.to_vec(),
            kind: Kind::File {
                size: pax.len() as u64,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: meta.mtime.min(MAX_OCTAL_11),
            acl_access: None,
            acl_default: None,
        };
        out.write_all(&ustar_block(PAX_NAME, b'x', &header, pax.len() as u64, b""))?;
        out.write_all(&pax)?;
        write_padding(out, pax.len() as u64)?;
    }
    out.write_all(&ustar_block(&name, typeflag, meta, size, link))
}

// ---------------------------------------------------------------------------
// Reading (strict)
// ---------------------------------------------------------------------------

/// What [`read_entry`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Entry(EntryMeta),
    /// The first zero block of the end of the archive.
    End,
}

/// Read exactly `buffer.len()` bytes.
pub fn read_exact<R: Read + ?Sized>(input: &mut R, buffer: &mut [u8]) -> Result<(), ArchiveError> {
    input
        .read_exact(buffer)
        .map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => {
                invalid("the stream ended before the end of the archive")
            }
            _ => invalid(format!("cannot read the stream: {error}")),
        })
}

/// Read and check the padding after `size` bytes of data (zeros only).
pub fn read_padding<R: Read + ?Sized>(input: &mut R, size: u64) -> Result<(), ArchiveError> {
    let mut pad = [0u8; BLOCK];
    let pad = &mut pad[..padding(size)];
    read_exact(input, pad)?;
    if pad.iter().any(|byte| *byte != 0) {
        return Err(invalid("data padding is not zero"));
    }
    Ok(())
}

fn parse_octal(field: &[u8], what: &str) -> Result<u64, ArchiveError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0 || *byte == b' ')
        .unwrap_or(field.len());
    let (digits, rest) = field.split_at(end);
    if digits.is_empty()
        || digits.len() > 11
        || !digits.iter().all(|byte| (b'0'..=b'7').contains(byte))
        || !rest.iter().all(|byte| *byte == 0 || *byte == b' ')
    {
        return Err(invalid(format!("invalid {what} field")));
    }
    let mut value = 0u64;
    for digit in digits {
        value = value * 8 + u64::from(digit - b'0');
    }
    Ok(value)
}

fn c_field(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    &field[..end]
}

fn check_block(block: &[u8; BLOCK]) -> Result<(), ArchiveError> {
    if &block[257..263] != b"ustar\0" || &block[263..265] != b"00" {
        return Err(invalid("not a ustar header"));
    }
    let recorded = parse_octal(&block[148..156], "checksum")?;
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    if sum != recorded {
        return Err(invalid("header checksum mismatch"));
    }
    // Only numeric owners and no ustar prefix.
    if block[265..329].iter().any(|byte| *byte != 0) || block[345..500].iter().any(|b| *b != 0) {
        return Err(invalid("unexpected owner names or path prefix"));
    }
    Ok(())
}

/// Whether `path` is relative and normalised: non-empty components, none
/// of them `.` or `..`.
pub fn valid_relative(path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.contains(&0)
        && path
            .split(|byte| *byte == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}

#[derive(Default)]
struct Pax {
    path: Option<Vec<u8>>,
    linkpath: Option<Vec<u8>>,
    uid: Option<u64>,
    gid: Option<u64>,
    size: Option<u64>,
    mtime: Option<u64>,
    acl_access: Option<Acl>,
    acl_default: Option<Acl>,
}

fn decimal(value: &[u8], what: &str) -> Result<u64, ArchiveError> {
    if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid(format!("invalid pax {what}")));
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| invalid(format!("invalid pax {what}")))
}

fn parse_pax(mut data: &[u8]) -> Result<Pax, ArchiveError> {
    let mut pax = Pax::default();
    let mut seen: Vec<&[u8]> = Vec::new();
    while !data.is_empty() {
        let space = data
            .iter()
            .take(8)
            .position(|byte| *byte == b' ')
            .ok_or_else(|| invalid("invalid pax record"))?;
        let length = decimal(&data[..space], "record length")? as usize;
        if length <= space + 1 || length > data.len() || data[length - 1] != b'\n' {
            return Err(invalid("invalid pax record"));
        }
        let record = &data[space + 1..length - 1];
        data = &data[length..];
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| invalid("invalid pax record"))?;
        let (key, value) = (&record[..equals], &record[equals + 1..]);
        if seen.contains(&key) {
            return Err(invalid("a pax record appears twice"));
        }
        seen.push(key);
        match key {
            b"path" | b"linkpath" => {
                if value.is_empty() || value.len() > MAX_PATH_BYTES || value.contains(&0) {
                    return Err(invalid("invalid pax path"));
                }
                if key == b"path" {
                    pax.path = Some(value.to_vec());
                } else {
                    pax.linkpath = Some(value.to_vec());
                }
            }
            b"uid" => pax.uid = Some(decimal(value, "uid")?),
            b"gid" => pax.gid = Some(decimal(value, "gid")?),
            b"size" => pax.size = Some(decimal(value, "size")?),
            b"mtime" => pax.mtime = Some(decimal(value, "mtime")?),
            b"hdrcharset" if value == b"BINARY" => {}
            b"SCHILY.acl.access" => pax.acl_access = Some(parse_acl_text(value)?),
            b"SCHILY.acl.default" => pax.acl_default = Some(parse_acl_text(value)?),
            _ => {
                return Err(invalid(format!(
                    "unexpected pax record {:?}",
                    String::from_utf8_lossy(key)
                )))
            }
        }
    }
    Ok(pax)
}

fn id(value: u64, what: &str) -> Result<u32, ArchiveError> {
    u32::try_from(value).map_err(|_| invalid(format!("{what} out of range")))
}

/// Read the next entry header (and its pax header). The caller then reads
/// [`EntryMeta::data_size`] bytes and [`read_padding`]. After [`Next::End`]
/// the second zero block of the end follows.
pub fn read_entry<R: Read + ?Sized>(input: &mut R) -> Result<Next, ArchiveError> {
    let mut block = [0u8; BLOCK];
    read_exact(input, &mut block)?;
    if block.iter().all(|byte| *byte == 0) {
        return Ok(Next::End);
    }
    read_ustar(block, input)
}

/// Read the next entry of a Backup Center archive: [`read_entry`]'s subset,
/// or a regular file in a GNU header as `tar::Builder` writes it (numeric
/// fields in octal, base-256 or all zero; no owner names, devices or GNU
/// extensions; a long name only as a `L` entry right before it).
pub fn read_backup_entry<R: Read + ?Sized>(input: &mut R) -> Result<Next, ArchiveError> {
    let mut block = [0u8; BLOCK];
    read_exact(input, &mut block)?;
    if block.iter().all(|byte| *byte == 0) {
        return Ok(Next::End);
    }
    if &block[257..265] == GNU_MAGIC {
        read_gnu(block, input)
    } else {
        read_ustar(block, input)
    }
}

/// A numeric field of a GNU header: octal, base-256 (first byte `0x80`) or
/// all zero bytes (a field `tar::Builder` never set).
fn gnu_number(field: &[u8], what: &str) -> Result<u64, ArchiveError> {
    if field.iter().all(|byte| *byte == 0) {
        return Ok(0);
    }
    if field[0] & 0x80 == 0 {
        return parse_octal(field, what);
    }
    let mut bytes = field.to_vec();
    bytes[0] &= 0x7f;
    let (high, low) = bytes.split_at(bytes.len().saturating_sub(8));
    if high.iter().any(|byte| *byte != 0) {
        return Err(invalid(format!("invalid {what} field")));
    }
    let mut value = [0u8; 8];
    value[8 - low.len()..].copy_from_slice(low);
    Ok(u64::from_be_bytes(value))
}

fn check_gnu_block(block: &[u8; BLOCK]) -> Result<(), ArchiveError> {
    let recorded = parse_octal(&block[148..156], "checksum")?;
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    if sum != recorded {
        return Err(invalid("header checksum mismatch"));
    }
    // No link name, owner names, devices, times, offsets or sparse maps.
    if block[157..257].iter().any(|byte| *byte != 0) || block[265..].iter().any(|b| *b != 0) {
        return Err(invalid("unexpected fields in a GNU header"));
    }
    Ok(())
}

/// A regular file in a GNU header (after its long name, if any).
fn read_gnu<R: Read + ?Sized>(mut block: [u8; BLOCK], input: &mut R) -> Result<Next, ArchiveError> {
    check_gnu_block(&block)?;
    let mut long_name: Option<Vec<u8>> = None;
    if block[156] == b'L' {
        if c_field(&block[0..100]) != GNU_LONG_NAME {
            return Err(invalid("invalid GNU long name entry"));
        }
        let size = gnu_number(&block[124..136], "size")?;
        if size == 0 || size > MAX_PATH_BYTES as u64 + 1 {
            return Err(invalid("invalid GNU long name"));
        }
        let mut data = vec![0u8; size as usize];
        read_exact(input, &mut data)?;
        read_padding(input, size)?;
        if data.pop() != Some(0) || data.contains(&0) {
            return Err(invalid("invalid GNU long name"));
        }
        long_name = Some(data);
        read_exact(input, &mut block)?;
        if &block[257..265] != GNU_MAGIC {
            return Err(invalid("a GNU long name without its entry"));
        }
        check_gnu_block(&block)?;
    }
    if !matches!(block[156], b'0' | 0) {
        return Err(invalid(format!(
            "entry type {:?} is not allowed in a GNU header",
            char::from(block[156])
        )));
    }
    let mode = parse_octal(&block[100..108], "mode")?;
    if mode > 0o7777 {
        return Err(invalid("invalid mode"));
    }
    let path = long_name.unwrap_or_else(|| c_field(&block[0..100]).to_vec());
    if !valid_relative(&path) {
        return Err(invalid(format!(
            "invalid entry path {:?}",
            String::from_utf8_lossy(&path)
        )));
    }
    Ok(Next::Entry(EntryMeta {
        path,
        kind: Kind::File {
            size: gnu_number(&block[124..136], "size")?,
        },
        mode: mode as u32,
        uid: id(gnu_number(&block[108..116], "uid")?, "uid")?,
        gid: id(gnu_number(&block[116..124], "gid")?, "gid")?,
        mtime: gnu_number(&block[136..148], "mtime")?,
        acl_access: None,
        acl_default: None,
    }))
}

/// The rest of [`read_entry`] after a non-zero `block`.
fn read_ustar<R: Read + ?Sized>(
    mut block: [u8; BLOCK],
    input: &mut R,
) -> Result<Next, ArchiveError> {
    check_block(&block)?;
    let mut pax = Pax::default();
    if block[156] == b'x' {
        let size = parse_octal(&block[124..136], "size")?;
        if size > MAX_PAX_BYTES as u64 {
            return Err(invalid(format!(
                "an extended header is larger than {MAX_PAX_BYTES} bytes"
            )));
        }
        let mut data = vec![0u8; size as usize];
        read_exact(input, &mut data)?;
        read_padding(input, size)?;
        pax = parse_pax(&data)?;
        read_exact(input, &mut block)?;
        if block.iter().all(|byte| *byte == 0) {
            return Err(invalid("an extended header without its entry"));
        }
        check_block(&block)?;
    }
    let typeflag = block[156];
    let header_size = parse_octal(&block[124..136], "size")?;
    let size = pax.size.unwrap_or(header_size);
    let mode = parse_octal(&block[100..108], "mode")?;
    if mode > 0o7777 {
        return Err(invalid("invalid mode"));
    }
    let uid = id(
        pax.uid
            .map_or_else(|| parse_octal(&block[108..116], "uid"), Ok)?,
        "uid",
    )?;
    let gid = id(
        pax.gid
            .map_or_else(|| parse_octal(&block[116..124], "gid"), Ok)?,
        "gid",
    )?;
    let mtime = pax
        .mtime
        .map_or_else(|| parse_octal(&block[136..148], "mtime"), Ok)?;
    let mut path = pax.path.unwrap_or_else(|| c_field(&block[0..100]).to_vec());
    let link = pax
        .linkpath
        .unwrap_or_else(|| c_field(&block[157..257]).to_vec());
    let is_dir = typeflag == b'5';
    if path.ends_with(b"/") {
        if !is_dir {
            return Err(invalid("only a folder name may end with '/'"));
        }
        path.pop();
    }
    if path != ROOT && !valid_relative(&path) {
        return Err(invalid(format!(
            "invalid entry path {:?}",
            String::from_utf8_lossy(&path)
        )));
    }
    if matches!(typeflag, b'0' | b'5') && !link.is_empty() {
        return Err(invalid("only a link has a target"));
    }
    let kind = match typeflag {
        b'0' => Kind::File { size },
        b'5' => Kind::Dir,
        b'2' => {
            if link.is_empty() || link.len() > MAX_LINK_BYTES {
                return Err(invalid("invalid symbolic link target"));
            }
            Kind::Symlink { target: link }
        }
        b'1' => {
            if !valid_relative(&link) {
                return Err(invalid("invalid hard link target"));
            }
            Kind::HardLink { target: link }
        }
        other => {
            return Err(invalid(format!(
                "entry type {:?} is not allowed",
                char::from(other)
            )))
        }
    };
    if !matches!(kind, Kind::File { .. }) && size != 0 {
        return Err(invalid("only a file has data"));
    }
    if pax.acl_default.is_some() && !is_dir {
        return Err(invalid("only a folder has a default ACL"));
    }
    if matches!(kind, Kind::Symlink { .. } | Kind::HardLink { .. }) && pax.acl_access.is_some() {
        return Err(invalid("a link has no ACL"));
    }
    Ok(Next::Entry(EntryMeta {
        path,
        kind,
        mode: mode as u32,
        uid,
        gid,
        mtime,
        acl_access: pax.acl_access,
        acl_default: pax.acl_default,
    }))
}

/// After [`Next::End`]: the second zero block.
pub fn read_end<R: Read + ?Sized>(input: &mut R) -> Result<(), ArchiveError> {
    let mut block = [0u8; BLOCK];
    read_exact(input, &mut block)?;
    if block.iter().any(|byte| *byte != 0) {
        return Err(invalid("the end of the archive is not two zero blocks"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(path: &str, kind: Kind) -> EntryMeta {
        EntryMeta {
            path: path.as_bytes().to_vec(),
            kind,
            mode: 0o640,
            uid: 200_007,
            gid: 1_200_001,
            mtime: 1_700_000_000,
            acl_access: None,
            acl_default: None,
        }
    }

    fn acl() -> Acl {
        parse_acl_text(
            b"user::rwx\nuser:200007:r-x\ngroup::r-x\ngroup:1200002:rw-\nmask::rwx\nother::---",
        )
        .unwrap()
    }

    fn roundtrip(entry: &EntryMeta, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_header(&mut out, entry).unwrap();
        out.extend_from_slice(data);
        write_padding(&mut out, data.len() as u64).unwrap();
        assert_eq!(out.len() % BLOCK, 0);
        out
    }

    #[test]
    fn acl_text_matches_gnu_tar_and_is_parsed_strictly() {
        let acl = acl();
        assert_eq!(
            acl_text(&acl),
            "user::rwx\nuser:200007:r-x\ngroup::r-x\ngroup:1200002:rw-\nmask::rwx\nother::---"
        );
        assert_eq!(parse_acl_text(acl_text(&acl).as_bytes()).unwrap(), acl);
        // Commas, as bsdtar also writes them.
        assert_eq!(
            parse_acl_text(b"user::rwx,group::r-x,other::---")
                .unwrap()
                .0
                .len(),
            3
        );
        for bad in [
            &b""[..],
            b"user::rwx\n",
            b"user:alice:rwx",
            b"user::rwxx",
            b"user::rw",
            b"mask:5:rwx",
            b"owner::rwx",
            b"user::rwx\nuser::r--",
            b"user:1:r-x:extra",
            b"user:99999999999:rwx",
            b"#effective:r--",
            b"user::\xffwx",
        ] {
            assert!(
                parse_acl_text(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn entries_roundtrip_with_owners_modes_acls_and_long_names() {
        let mut dir = meta("docs", Kind::Dir);
        dir.mode = 0o2770;
        dir.acl_access = Some(acl());
        dir.acl_default = Some(acl());
        let long = format!("{}/{}", "a".repeat(120), "é".repeat(40));
        let file = meta(&long, Kind::File { size: 5 });
        let mut huge_ids = meta("big", Kind::File { size: 0 });
        huge_ids.uid = 3_000_000_000;
        huge_ids.gid = 2_100_000;
        let link = meta(
            "link",
            Kind::Symlink {
                target: b"/etc/shadow".to_vec(),
            },
        );
        let hard = meta(
            "again",
            Kind::HardLink {
                target: long.as_bytes().to_vec(),
            },
        );
        let mut raw = meta("x", Kind::File { size: 0 });
        raw.path = b"bin\xff\xfe".repeat(30);
        let mut stream = Vec::new();
        stream.extend(roundtrip(&dir, b""));
        stream.extend(roundtrip(&file, b"hello"));
        stream.extend(roundtrip(&huge_ids, b""));
        stream.extend(roundtrip(&link, b""));
        stream.extend(roundtrip(&hard, b""));
        stream.extend(roundtrip(&raw, b""));
        write_end(&mut stream).unwrap();

        let mut input = &stream[..];
        for (expected, data) in [
            (&dir, &b""[..]),
            (&file, b"hello"),
            (&huge_ids, b""),
            (&link, b""),
            (&hard, b""),
            (&raw, b""),
        ] {
            let Next::Entry(entry) = read_entry(&mut input).unwrap() else {
                panic!("entry expected");
            };
            assert_eq!(&entry, expected);
            let mut buffer = vec![0u8; entry.data_size() as usize];
            read_exact(&mut input, &mut buffer).unwrap();
            assert_eq!(buffer, data);
            read_padding(&mut input, entry.data_size()).unwrap();
        }
        assert_eq!(read_entry(&mut input).unwrap(), Next::End);
        read_end(&mut input).unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn short_entries_have_no_pax_header_and_folders_end_with_a_slash() {
        let mut out = Vec::new();
        let mut entry = meta("docs", Kind::Dir);
        entry.uid = 1000;
        entry.gid = 1000;
        write_header(&mut out, &entry).unwrap();
        assert_eq!(out.len(), BLOCK);
        assert_eq!(&out[..5], b"docs/");
        assert_eq!(out[156], b'5');
        assert_eq!(&out[257..265], b"ustar\x0000");
        // Empty owner names: tar restores the numeric ids.
        assert!(out[265..329].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn the_reader_refuses_what_the_writer_never_writes() {
        let good = roundtrip(&meta("a.txt", Kind::File { size: 1 }), b"x");
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        let mut checksum = good.clone();
        checksum[0] = b'b';
        cases.push(("checksum", checksum));
        let mut magic = good.clone();
        magic[257] = b'x';
        cases.push(("ustar", magic));
        // Rewrite a field and fix the checksum.
        let patched = |offset: usize, bytes: &[u8]| {
            let mut block = good.clone();
            block[offset..offset + bytes.len()].copy_from_slice(bytes);
            block[148..156].copy_from_slice(b"        ");
            let sum: u32 = block[..BLOCK].iter().map(|b| u32::from(*b)).sum();
            let text = format!("{sum:06o}\0 ");
            block[148..156].copy_from_slice(text.as_bytes());
            block
        };
        cases.push(("entry type", patched(156, b"3"))); // a character device
        cases.push(("entry type", patched(156, b"L"))); // a GNU long name
        cases.push(("entry type", patched(156, b"g"))); // a global pax header
        cases.push(("path", patched(0, b"../a.tx")));
        cases.push(("path", patched(0, b"/etc/pa")));
        cases.push(("path", patched(0, b"a//b.tx")));
        cases.push(("owner names", patched(265, b"root")));
        cases.push(("owner names", patched(345, b"x")));
        cases.push(("mode", patched(100, b"0017777")));
        cases.push(("size field", patched(124, b"9")));
        cases.push(("end", good[..BLOCK + 10].to_vec()));
        for (what, stream) in cases {
            let mut input = &stream[..];
            let result = read_entry(&mut input).and_then(|next| match next {
                Next::Entry(entry) => {
                    let mut data = vec![0u8; entry.data_size() as usize];
                    read_exact(&mut input, &mut data)?;
                    read_padding(&mut input, entry.data_size())
                }
                Next::End => Ok(()),
            });
            let error = result.expect_err(what);
            assert!(error.0.contains(what), "{what}: {error}");
        }
    }

    fn with_pax(records: &[u8]) -> Vec<u8> {
        let header = meta(
            "p",
            Kind::File {
                size: records.len() as u64,
            },
        );
        let mut out = ustar_block(PAX_NAME, b'x', &header, records.len() as u64, b"").to_vec();
        out.extend_from_slice(records);
        write_padding(&mut out, records.len() as u64).unwrap();
        out.extend_from_slice(&ustar_block(
            b"a",
            b'0',
            &meta("a", Kind::File { size: 0 }),
            0,
            b"",
        ));
        out
    }

    #[test]
    fn pax_headers_are_bounded_and_strict() {
        let mut record = Vec::new();
        pax_record(&mut record, "path", b"ok/name");
        let Next::Entry(entry) = read_entry(&mut &with_pax(&record)[..]).unwrap() else {
            panic!()
        };
        assert_eq!(entry.path, b"ok/name");

        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        let mut unknown = Vec::new();
        pax_record(&mut unknown, "SCHILY.xattr.security.capability", b"x");
        cases.push(("unexpected pax record", unknown));
        let mut twice = Vec::new();
        pax_record(&mut twice, "uid", b"1");
        pax_record(&mut twice, "uid", b"2");
        cases.push(("twice", twice));
        cases.push(("invalid pax record", b"99 path=x\n".to_vec()));
        cases.push(("invalid pax record", b"garbage".to_vec()));
        let mut traversal = Vec::new();
        pax_record(&mut traversal, "path", b"a/../../etc");
        cases.push(("invalid entry path", traversal));
        let mut default_on_file = Vec::new();
        pax_record(&mut default_on_file, "SCHILY.acl.default", b"user::rwx");
        cases.push(("default ACL", default_on_file));
        let mut big_uid = Vec::new();
        pax_record(&mut big_uid, "uid", b"4294967296");
        cases.push(("uid out of range", big_uid));
        for (what, records) in cases {
            let error = read_entry(&mut &with_pax(&records)[..]).expect_err(what);
            assert!(error.0.contains(what), "{what}: {error}");
        }

        // Larger than the limit: refused before it is read.
        let mut huge = vec![0u8; BLOCK];
        huge.copy_from_slice(&ustar_block(
            PAX_NAME,
            b'x',
            &meta("p", Kind::File { size: 0 }),
            MAX_PAX_BYTES as u64 + 1,
            b"",
        ));
        let error = read_entry(&mut &huge[..]).unwrap_err();
        assert!(error.0.contains("larger than"), "{error}");
    }

    #[test]
    fn pax_record_lengths_count_their_own_digits() {
        for size in [
            0usize, 1, 2, 3, 4, 5, 6, 7, 90, 91, 92, 93, 94, 95, 996, 997, 998, 999,
        ] {
            let mut record = Vec::new();
            pax_record(&mut record, "path", &vec![b'a'; size]);
            let text = String::from_utf8(record.clone()).unwrap();
            let length: usize = text.split(' ').next().unwrap().parse().unwrap();
            assert_eq!(length, record.len(), "{size}");
        }
    }
}
