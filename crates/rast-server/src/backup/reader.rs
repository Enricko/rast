//! Backup Center's side of the backup reader (ADR 0005, D12).
//!
//! The helper starts the root reader (`backup_reader_start`); this module
//! asks it for one HOME or one shared folder container per connection and
//! copies the stream into the backup archive under `users/homes/<name>/` or
//! `users/shared/<container>/`, keeping numeric owners, modes and ACLs.
//!
//! The reader is root, but what it streams comes from the users' files, so
//! the stream is untrusted input here: it is parsed with the strict tar
//! subset of [`archive`] (bounded headers, relative normalised paths, no
//! device or other entry types), must start with the folder itself, must
//! end with the end-of-archive blocks and one bounded trailer whose counts
//! match what was received, and every entry is written again by this
//! server (never copied through byte for byte). A stream that breaks keeps
//! the entries before the break and becomes a warning; a file cut off in
//! the middle is padded so the archive stays valid, and is left out of the
//! manifest.
//!
//! Each tree has a byte budget (its account's or shared folder's quota
//! plus slack, [`tree_budget`]; review F7): a file that would exceed it is
//! not written and ends the tree with a warning, so a padded file is never
//! larger than the budget either. While data is copied, the free space of
//! the backup's file system is checked ([`super::SpaceGuard`]); below the
//! floor the backup fails instead of filling the disk.

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{ManifestEntry, SpaceGuard};
use crate::error::{AppError, AppResult};
use crate::native::backup_reader::archive::{self, EntryMeta, Kind, Next};
use crate::native::backup_reader::{
    read_line, ReaderReply, ReaderRequest, Trailer, TreeKind, MAX_PROBLEMS, MAX_PROBLEM_CHARS,
    MAX_REPLY_LINE, MAX_TRAILER_LINE, MAX_TREE_PATH,
};
use crate::native::client::NativeClient;

/// The server gives up when the reader sends nothing for this long.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// Problems of one tree repeated in the backup warnings.
const PROBLEMS_SHOWN: usize = 5;
/// Longest reader message kept in a warning.
const MAX_MESSAGE_CHARS: usize = 300;

/// The socket and token of a running reader.
#[derive(Debug, Clone)]
pub struct ReaderAccess {
    pub socket: PathBuf,
    pub token: String,
}

/// Start (or find) the reader through the helper.
pub async fn start(native: &NativeClient) -> Result<ReaderAccess, String> {
    let reply = native
        .call_json("backup_reader_start", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    parse_access(&reply)
}

/// The helper's reply: an absolute socket path and a 64-digit hex token.
pub fn parse_access(reply: &Value) -> Result<ReaderAccess, String> {
    let socket = reply
        .get("socket")
        .and_then(Value::as_str)
        .filter(|socket| socket.starts_with('/') && socket.len() < 108 && !socket.contains('\0'));
    let token = reply
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()));
    match (socket, token) {
        (Some(socket), Some(token)) => Ok(ReaderAccess {
            socket: PathBuf::from(socket),
            token: token.to_string(),
        }),
        _ => Err("unexpected reply from the helper".to_string()),
    }
}

/// Room a tree gets beyond its quota: the reader's budget for holes
/// ([`crate::native::backup_reader::MAX_TREE_HOLES`]) and files that grew
/// while quota was being counted.
pub const TREE_SLACK: u64 = 2 << 30;

/// The byte budget of a tree whose account or shared folder has `quota`
/// bytes (`None` or 0: no limit, no budget): the quota, plus a tenth of it
/// or [`TREE_SLACK`], whichever is larger.
pub fn tree_budget(quota: Option<u64>) -> Option<u64> {
    let quota = quota.filter(|quota| *quota > 0)?;
    Some(quota.saturating_add((quota / 10).max(TREE_SLACK)))
}

/// One tree to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub kind: TreeKind,
    /// The username or the container name.
    pub name: String,
    /// The most file bytes the archive takes from it ([`tree_budget`]).
    pub budget: Option<u64>,
}

impl Tree {
    /// Its place in the archive.
    pub fn prefix(&self) -> String {
        match self.kind {
            TreeKind::Home => format!("users/homes/{}", self.name),
            TreeKind::Shared => format!("users/shared/{}", self.name),
        }
    }
}

/// What reading one tree gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was written: the reader could not be reached or refused the
    /// tree (the reason).
    Refused(String),
    /// The tree was written; `complete` is false when the stream broke.
    Written {
        complete: bool,
        files: u64,
        bytes: u64,
        warnings: Vec<String>,
    },
}

/// An agent message, without control characters and bounded.
fn bounded(message: &str, limit: usize) -> String {
    let mut out: String = message
        .chars()
        .take(limit)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if message.chars().nth(limit).is_some() {
        out.push('…');
    }
    out
}

enum CopyError {
    /// The reader's stream is broken or invalid (a warning).
    Stream(String),
    /// The tree is larger than its budget (a warning).
    Budget(u64),
    /// Writing the backup archive failed, or its file system is almost
    /// full (the backup fails).
    Archive(io::Error),
}

#[derive(Default)]
struct Received {
    entries: u64,
    files: u64,
    bytes: u64,
}

fn joined(prefix: &str, relative: &[u8]) -> Vec<u8> {
    let mut path = prefix.as_bytes().to_vec();
    path.push(b'/');
    path.extend_from_slice(relative);
    path
}

/// Copy one entry's data (`size` bytes) into the archive, hashing it.
/// When the stream breaks, the rest is zeros so the archive stays valid.
fn copy_data<R: Read + ?Sized, W: Write + ?Sized>(
    input: &mut R,
    out: &mut W,
    size: u64,
    buffer: &mut [u8],
    space: &mut SpaceGuard,
) -> Result<Result<String, String>, io::Error> {
    let mut hasher = Sha256::new();
    let mut remaining = size;
    let mut failure: Option<String> = None;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        if failure.is_none() {
            match input.read(&mut buffer[..want]) {
                Ok(0) => failure = Some("the stream ended inside a file".to_string()),
                Ok(read) => {
                    hasher.update(&buffer[..read]);
                    out.write_all(&buffer[..read])?;
                    space.wrote(read as u64)?;
                    remaining -= read as u64;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => failure = Some(format!("cannot read the stream: {error}")),
            }
        }
        buffer[..want].fill(0);
        out.write_all(&buffer[..want])?;
        space.wrote(want as u64)?;
        remaining -= want as u64;
    }
    archive::write_padding(out, size)?;
    Ok(match failure {
        None => Ok(hex::encode(hasher.finalize())),
        Some(failure) => Err(failure),
    })
}

fn copy_entries<R: Read + ?Sized, W: Write + ?Sized>(
    input: &mut R,
    prefix: &str,
    budget: Option<u64>,
    out: &mut W,
    manifest: &mut Vec<ManifestEntry>,
    received: &mut Received,
    space: &mut SpaceGuard,
) -> Result<Trailer, CopyError> {
    let stream = |error: archive::ArchiveError| CopyError::Stream(error.0);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let entry = match archive::read_entry(input).map_err(stream)? {
            Next::End => break,
            Next::Entry(entry) => entry,
        };
        let is_root = entry.path == archive::ROOT;
        if received.entries == 0 && !(is_root && entry.kind == Kind::Dir) {
            return Err(CopyError::Stream(
                "the stream does not start with the folder itself".into(),
            ));
        }
        if received.entries > 0 && is_root {
            return Err(CopyError::Stream("the folder itself appears twice".into()));
        }
        let too_long = |path: &[u8]| path.len() > MAX_TREE_PATH;
        if too_long(&entry.path)
            || matches!(&entry.kind, Kind::HardLink { target } if too_long(target))
        {
            return Err(CopyError::Stream("an entry path is too long".into()));
        }
        let path = if is_root {
            prefix.as_bytes().to_vec()
        } else {
            joined(prefix, &entry.path)
        };
        let kind = match entry.kind {
            Kind::HardLink { target } => Kind::HardLink {
                target: joined(prefix, &target),
            },
            other => other,
        };
        let outer = EntryMeta {
            path,
            kind,
            ..entry
        };
        // Checked before the header is written: nothing of a file beyond
        // the budget reaches the archive, padding included.
        if let (Kind::File { size }, Some(budget)) = (&outer.kind, budget) {
            if received.bytes.saturating_add(*size) > budget {
                return Err(CopyError::Budget(budget));
            }
        }
        archive::write_header(out, &outer).map_err(CopyError::Archive)?;
        if let Kind::File { size } = outer.kind {
            let shown = String::from_utf8_lossy(&outer.path).into_owned();
            match copy_data(input, out, size, &mut buffer, space).map_err(CopyError::Archive)? {
                Ok(sha256) => {
                    archive::read_padding(input, size).map_err(stream)?;
                    manifest.push(ManifestEntry {
                        path: shown,
                        size,
                        sha256,
                        owners_preserved: true,
                    });
                    received.files += 1;
                    received.bytes += size;
                }
                Err(failure) => {
                    return Err(CopyError::Stream(format!(
                        "{failure} ({}; it is padded with zeros and not in the manifest)",
                        bounded(&shown, MAX_MESSAGE_CHARS)
                    )))
                }
            }
        }
        received.entries += 1;
    }
    archive::read_end(input).map_err(stream)?;
    let line = read_line(input, MAX_TRAILER_LINE)
        .map_err(|error| CopyError::Stream(format!("no trailer after the archive: {error}")))?;
    let trailer: Trailer = serde_json::from_slice(&line)
        .map_err(|error| CopyError::Stream(format!("invalid trailer: {error}")))?;
    let mut extra = [0u8; 1];
    if matches!(input.read(&mut extra), Ok(read) if read > 0) {
        return Err(CopyError::Stream("data after the trailer".into()));
    }
    if !trailer.done
        || trailer.problems.len() > MAX_PROBLEMS
        || trailer
            .problems
            .iter()
            .any(|problem| problem.chars().count() > MAX_PROBLEM_CHARS)
    {
        return Err(CopyError::Stream("invalid trailer".into()));
    }
    if (trailer.entries, trailer.files, trailer.bytes)
        != (received.entries, received.files, received.bytes)
    {
        return Err(CopyError::Stream(
            "the trailer does not match the archive".into(),
        ));
    }
    Ok(trailer)
}

/// The warnings of a complete tree: what the reader left out against its
/// will (unreadable, changed, other file systems, too deep or long names).
/// Caches, recycle bins, Flatpak, special files and entries that vanished
/// during the backup are expected and not reported.
fn trailer_warnings(prefix: &str, trailer: &Trailer) -> Vec<String> {
    let counts = [
        (trailer.unreadable, "unreadable"),
        (trailer.changed, "changed while they were read"),
        (
            trailer.other_file_systems,
            "on other file systems or mounts",
        ),
        (trailer.too_deep, "nested too deeply"),
        (trailer.too_long, "with names that are too long"),
        (
            trailer.other_owner,
            "owned by accounts this folder may not hold",
        ),
        (trailer.sparse, "sparse files beyond the budget for holes"),
    ];
    let listed: Vec<String> = counts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
    if listed.is_empty() {
        return Vec::new();
    }
    let mut warnings = vec![format!(
        "{prefix}: read through the backup reader, but some entries are missing or incomplete ({})",
        listed.join(", ")
    )];
    for problem in trailer.problems.iter().take(PROBLEMS_SHOWN) {
        warnings.push(format!("{prefix}/{}", bounded(problem, MAX_PROBLEM_CHARS)));
    }
    warnings
}

/// Read one tree through the reader into `out` (the backup archive). An
/// error means writing the backup archive failed.
pub fn archive_tree<W: Write + ?Sized>(
    access: &ReaderAccess,
    tree: &Tree,
    out: &mut W,
    manifest: &mut Vec<ManifestEntry>,
    space: &mut SpaceGuard,
) -> AppResult<Outcome> {
    let prefix = tree.prefix();
    let stream = match UnixStream::connect(&access.socket) {
        Ok(stream) => stream,
        Err(error) => {
            return Ok(Outcome::Refused(format!(
                "cannot reach the backup reader: {error}"
            )))
        }
    };
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let request = ReaderRequest {
        token: access.token.clone(),
        tree: tree.kind,
        name: tree.name.clone(),
    };
    let mut line = serde_json::to_vec(&request)
        .map_err(|error| AppError::internal(format!("cannot encode a reader request: {error}")))?;
    line.push(b'\n');
    if let Err(error) = (&stream).write_all(&line) {
        return Ok(Outcome::Refused(format!(
            "cannot reach the backup reader: {error}"
        )));
    }
    let mut input = BufReader::with_capacity(256 * 1024, &stream);
    let reply = match read_line(&mut input, MAX_REPLY_LINE) {
        Ok(reply) => reply,
        Err(error) => {
            return Ok(Outcome::Refused(format!(
                "the backup reader did not answer: {error}"
            )))
        }
    };
    let reply: ReaderReply = match serde_json::from_slice(&reply) {
        Ok(reply) => reply,
        Err(_) => {
            return Ok(Outcome::Refused(
                "unexpected reply from the backup reader".into(),
            ))
        }
    };
    if !reply.ok {
        let (code, message) = reply
            .error
            .map(|error| (error.code, error.message))
            .unwrap_or_else(|| ("unknown".into(), "no reason given".into()));
        return Ok(Outcome::Refused(format!(
            "the backup reader refused it ({}): {}",
            bounded(&code, 40),
            bounded(&message, MAX_MESSAGE_CHARS)
        )));
    }
    let mut received = Received::default();
    match copy_entries(
        &mut input,
        &prefix,
        tree.budget,
        out,
        manifest,
        &mut received,
        space,
    ) {
        Ok(trailer) => Ok(Outcome::Written {
            complete: true,
            files: received.files,
            bytes: received.bytes,
            warnings: trailer_warnings(&prefix, &trailer),
        }),
        Err(CopyError::Archive(error)) => Err(AppError::internal(format!(
            "cannot write the backup archive: {error}"
        ))),
        Err(CopyError::Budget(budget)) => Ok(Outcome::Written {
            complete: false,
            files: received.files,
            bytes: received.bytes,
            warnings: vec![format!(
                "{prefix}: incomplete, the folder is larger than its backup budget of {budget} \
                 bytes (its quota plus slack); the files after the first {} entries are left out",
                received.entries
            )],
        }),
        Err(CopyError::Stream(reason)) => Ok(Outcome::Written {
            complete: false,
            files: received.files,
            bytes: received.bytes,
            warnings: vec![format!(
                "{prefix}: incomplete, the backup reader stream broke after {} entries: {}; \
                 the entries before it are in the archive",
                received.entries,
                bounded(&reason, MAX_MESSAGE_CHARS)
            )],
        }),
    }
}

#[cfg(test)]
mod tests;
