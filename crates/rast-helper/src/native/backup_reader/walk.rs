//! The reader's walk of one tree (a HOME or a shared folder container),
//! written as the tar subset of [`super::archive`].
//!
//! Every entry is reached through the descriptor of the folder that holds
//! it (`fstatat` and `openat` with `O_NOFOLLOW`), so a symbolic link is
//! stored as a link and never followed, also not one that a user swaps in
//! while the walk runs: what is opened must be the inode that was checked,
//! otherwise the entry is left out and reported. Entries on another file
//! system or mount (a mount point or a bind mount inside the tree) are left
//! out, and so are the caches, recycle bins and Flatpak installations of
//! [`excluded`]. Below the root, only entries of the owners the tree may
//! hold are stored ([`super::OwnerRules::entry_owners`]; review F6): with
//! `fs.protected_hardlinks = 0` a user can hard-link any file of the same
//! file system into a HOME, and the reader reads every file. Sparse files
//! are stored while the tree's holes stay within [`MAX_TREE_HOLES`]
//! (review F7): quota counts the blocks a file takes, the archive its
//! apparent size. Memory stays bounded: one open folder per level (at most
//! [`MAX_DEPTH`]), one copy buffer, and a capped map of hard links.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::archive::{self, EntryMeta, Kind};
use super::{OwnerRules, Trailer, TreeKind, MAX_PROBLEMS, MAX_PROBLEM_CHARS};
use crate::native::acl::{self, Acl};
use crate::native::homes::{clear_errno, fstat, is_dir, DIR_FLAGS};
use crate::native::shares::{
    dev_of, fd_mount_id, ino_of, is_regular, lstat_at, mount_id, open_at, same_inode, FILE_FLAGS,
};

/// Deepest nesting the walk descends into (one open folder per level).
pub const MAX_DEPTH: usize = 256;
/// Longest path below the tree root: room is left for the
/// `users/shared/<container>/` prefix the server adds.
pub const MAX_TREE_PATH: usize = archive::MAX_PATH_BYTES - 300;
/// Files with more than one name remembered at most (and the bytes of
/// their paths); beyond that a further name is stored as a file again.
const MAX_LINKS: usize = 100_000;
const MAX_LINK_PATH_BYTES: usize = 16 * 1024 * 1024;
const COPY_BUFFER: usize = 64 * 1024;
/// The holes (bytes a file does not take on disk) that the files of one
/// tree may have in total; a sparse file beyond it is left out. Its zeros
/// would cost the backup time and space but the user no quota.
pub const MAX_TREE_HOLES: u64 = 1 << 30;
/// Data segments of one file followed with `SEEK_DATA`/`SEEK_HOLE` at most;
/// beyond them the space the file takes on disk counts as its data.
const MAX_SEGMENTS: usize = 65_536;

/// Whether the walk leaves `relative` out: in a HOME `~/.cache`, the XDG
/// recycle bin `~/.local/share/Trash` and `~/.local/share/flatpak`; at the
/// top of a shared folder the recycle bins `.Trash` and `.Trash-<uid>`.
pub fn excluded(kind: TreeKind, relative: &[u8]) -> bool {
    match kind {
        TreeKind::Home => matches!(
            relative,
            b".cache" | b".local/share/Trash" | b".local/share/flatpak"
        ),
        TreeKind::Shared => {
            relative == b".Trash"
                || relative.strip_prefix(b".Trash-").is_some_and(|uid| {
                    !uid.is_empty() && uid.iter().all(|byte| byte.is_ascii_digit())
                })
        }
    }
}

/// A folder being listed (`fdopendir`); its descriptor is also the parent
/// of the names it returns.
struct DirStream(*mut libc::DIR);

impl DirStream {
    fn open(fd: OwnedFd) -> io::Result<Self> {
        // SAFETY: `fd` is a valid directory descriptor; on success the
        // stream owns it and `closedir` closes it.
        let stream = unsafe { libc::fdopendir(fd.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(fd);
        Ok(Self(stream))
    }

    fn fd(&self) -> RawFd {
        // SAFETY: the stream is open.
        unsafe { libc::dirfd(self.0) }
    }

    fn next_name(&mut self) -> io::Result<Option<CString>> {
        loop {
            clear_errno();
            // SAFETY: the stream is open.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) | None => Ok(None),
                    Some(_) => Err(error),
                };
            }
            // SAFETY: `d_name` of a returned entry is NUL-terminated.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                return Ok(Some(name.to_owned()));
            }
        }
    }
}

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed exactly once.
        unsafe { libc::closedir(self.0) };
    }
}

struct Frame {
    dir: DirStream,
    relative: Vec<u8>,
    depth: usize,
}

fn meta_of(path: Vec<u8>, kind: Kind, stat: &libc::stat) -> EntryMeta {
    EntryMeta {
        path,
        kind,
        mode: stat.st_mode & 0o7777,
        uid: stat.st_uid,
        gid: stat.st_gid,
        mtime: u64::try_from(stat.st_mtime).unwrap_or(0),
        acl_access: None,
        acl_default: None,
    }
}

/// The bytes of the open file `fd` (`size` bytes long, `allocated` bytes on
/// disk) that hold data rather than holes, from `SEEK_DATA` and
/// `SEEK_HOLE`. A file that takes its size on disk has no holes; when the
/// file system cannot tell, the whole file is data. The file offset is back
/// at 0 afterwards.
pub(crate) fn data_bytes(fd: RawFd, size: u64, allocated: u64) -> io::Result<u64> {
    if allocated >= size {
        return Ok(size);
    }
    let end = i64::try_from(size).unwrap_or(i64::MAX);
    let mut offset = 0i64;
    let mut data = 0u64;
    let mut found = None;
    for _ in 0..MAX_SEGMENTS {
        if offset >= end {
            found = Some(data);
            break;
        }
        // SAFETY: valid fd; lseek has no memory effects.
        let start = unsafe { libc::lseek(fd, offset, libc::SEEK_DATA) };
        if start < 0 {
            found = Some(match io::Error::last_os_error().raw_os_error() {
                // Only a hole is left.
                Some(libc::ENXIO) => data,
                _ => size,
            });
            break;
        }
        // SAFETY: as above.
        let stop = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
        if stop < 0 {
            found = Some(size);
            break;
        }
        data = data.saturating_add(u64::try_from(stop.min(end) - start.min(end)).unwrap_or(0));
        offset = stop;
    }
    let data = found.unwrap_or(data.max(allocated));
    // SAFETY: as above.
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data.min(size))
}

fn readlink_at(dir: RawFd, name: &CStr) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; archive::MAX_LINK_BYTES + 1];
    // SAFETY: valid fd, NUL-terminated name, writable buffer of that length.
    let length =
        unsafe { libc::readlinkat(dir, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(buffer)
}

struct Walk<'a, W: Write> {
    out: &'a mut W,
    kind: TreeKind,
    dev: u64,
    /// The mount of the tree root (`None`: the kernel reports no mount ids,
    /// so only the device is compared).
    mount: Option<u64>,
    links: HashMap<(u64, u64), Vec<u8>>,
    link_bytes: usize,
    summary: Trailer,
    buffer: Vec<u8>,
    /// The uids whose entries below the root are stored.
    owners: Vec<RangeInclusive<u32>>,
    /// The holes of the files stored so far.
    holes: u64,
}

/// Stream the tree below `root` (a folder the caller opened and checked)
/// to `out`: the root itself as `.`, then everything below it that `rules`
/// lets the tree hold. Returns the summary for the trailer; an error means
/// the output failed.
pub fn stream_tree<W: Write>(
    out: &mut W,
    kind: TreeKind,
    root: OwnedFd,
    rules: &OwnerRules,
) -> io::Result<Trailer> {
    let stat = fstat(root.as_raw_fd())?;
    let mut walk = Walk {
        out,
        kind,
        dev: dev_of(&stat),
        mount: fd_mount_id(root.as_raw_fd()).ok(),
        links: HashMap::new(),
        link_bytes: 0,
        summary: Trailer::default(),
        buffer: vec![0u8; COPY_BUFFER],
        owners: rules.entry_owners(kind, stat.st_uid),
        holes: 0,
    };
    let mut meta = meta_of(archive::ROOT.to_vec(), Kind::Dir, &stat);
    meta.acl_access = walk.acl(root.as_raw_fd(), acl::ACCESS_XATTR, archive::ROOT);
    meta.acl_default = walk.acl(root.as_raw_fd(), acl::DEFAULT_XATTR, archive::ROOT);
    archive::write_header(walk.out, &meta)?;
    walk.summary.entries += 1;
    let mut stack = vec![Frame {
        dir: DirStream::open(root)?,
        relative: Vec::new(),
        depth: 0,
    }];
    while let Some(top) = stack.len().checked_sub(1) {
        let name = match stack[top].dir.next_name() {
            Ok(Some(name)) => name,
            Ok(None) => {
                stack.pop();
                continue;
            }
            Err(error) => {
                let relative = stack[top].relative.clone();
                walk.summary.unreadable += 1;
                walk.problem(&relative, &format!("cannot list the folder: {error}"));
                stack.pop();
                continue;
            }
        };
        let parent = stack[top].dir.fd();
        let depth = stack[top].depth + 1;
        let mut relative = stack[top].relative.clone();
        if !relative.is_empty() {
            relative.push(b'/');
        }
        relative.extend_from_slice(name.to_bytes());
        if let Some(frame) = walk.entry(parent, &name, relative, depth)? {
            stack.push(frame);
        }
    }
    walk.summary.done = true;
    Ok(walk.summary)
}

impl<W: Write> Walk<'_, W> {
    fn problem(&mut self, path: &[u8], reason: &str) {
        if self.summary.problems.len() >= MAX_PROBLEMS {
            return;
        }
        let shown = if path.is_empty() {
            ".".to_string()
        } else {
            String::from_utf8_lossy(path).into_owned()
        };
        let text: String = format!("{shown}: {reason}")
            .chars()
            .take(MAX_PROBLEM_CHARS)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        self.summary.problems.push(text);
    }

    /// A lookup that failed: the entry vanished, or cannot be read.
    fn lookup_failed(&mut self, path: &[u8], error: &io::Error) {
        match error.raw_os_error() {
            Some(libc::ENOENT) => self.summary.vanished += 1,
            // Swapped for a symbolic link or another type since it was
            // checked.
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                self.summary.changed += 1;
                self.problem(path, "changed while it was read, left out");
            }
            _ => {
                self.summary.unreadable += 1;
                self.problem(path, &format!("cannot read it: {error}"));
            }
        }
    }

    /// Whether the tree may hold an entry of `uid`; if not, it is counted
    /// and reported.
    fn owner_allowed(&mut self, path: &[u8], uid: u32) -> bool {
        if self.owners.iter().any(|range| range.contains(&uid)) {
            return true;
        }
        self.summary.other_owner += 1;
        self.problem(
            path,
            &format!("owned by uid {uid}, not by an account this folder may hold; left out"),
        );
        false
    }

    fn acl(&mut self, fd: RawFd, xattr: &str, path: &[u8]) -> Option<Acl> {
        match acl::read(fd, xattr) {
            Ok(acl) => acl,
            Err(error) => {
                self.problem(path, &format!("cannot read its ACL: {error}"));
                None
            }
        }
    }

    /// Whether `name` in `parent` is on the tree's mount (a mount point or
    /// a bind mount is not).
    fn same_mount(&mut self, parent: RawFd, name: &CStr, path: &[u8]) -> bool {
        let Some(mount) = self.mount else {
            return true;
        };
        match mount_id(parent, name) {
            Ok(id) if id == mount => true,
            Ok(_) => {
                self.summary.other_file_systems += 1;
                self.problem(path, "a mount point, left out");
                false
            }
            Err(error) => {
                self.lookup_failed(path, &error);
                false
            }
        }
    }

    fn entry(
        &mut self,
        parent: RawFd,
        name: &CStr,
        relative: Vec<u8>,
        depth: usize,
    ) -> io::Result<Option<Frame>> {
        if excluded(self.kind, &relative) {
            self.summary.excluded += 1;
            return Ok(None);
        }
        if relative.len() > MAX_TREE_PATH {
            self.summary.too_long += 1;
            self.problem(&relative, "the path is too long, left out");
            return Ok(None);
        }
        let stat = match lstat_at(parent, name) {
            Ok(stat) => stat,
            Err(error) => {
                self.lookup_failed(&relative, &error);
                return Ok(None);
            }
        };
        if dev_of(&stat) != self.dev {
            self.summary.other_file_systems += 1;
            self.problem(&relative, "on another file system, left out");
            return Ok(None);
        }
        let format = stat.st_mode & libc::S_IFMT;
        if !matches!(format, libc::S_IFDIR | libc::S_IFREG | libc::S_IFLNK) {
            // FIFOs, sockets and devices are not backed up.
            self.summary.special += 1;
            return Ok(None);
        }
        if !self.owner_allowed(&relative, stat.st_uid) {
            return Ok(None);
        }
        match format {
            libc::S_IFDIR => self.dir(parent, name, relative, depth, &stat),
            libc::S_IFREG => self.file(parent, name, relative, &stat).map(|()| None),
            _ => self.symlink(parent, name, relative, &stat).map(|()| None),
        }
    }

    fn dir(
        &mut self,
        parent: RawFd,
        name: &CStr,
        relative: Vec<u8>,
        depth: usize,
        stat: &libc::stat,
    ) -> io::Result<Option<Frame>> {
        if !self.same_mount(parent, name, &relative) {
            return Ok(None);
        }
        if depth > MAX_DEPTH {
            self.summary.too_deep += 1;
            self.problem(&relative, "nested too deeply, left out");
            return Ok(None);
        }
        let fd = match open_at(parent, name, DIR_FLAGS) {
            Ok(fd) => fd,
            Err(error) => {
                self.lookup_failed(&relative, &error);
                return Ok(None);
            }
        };
        let opened = fstat(fd.as_raw_fd())?;
        if !same_inode(stat, &opened) || !is_dir(&opened) {
            self.summary.changed += 1;
            self.problem(&relative, "changed while it was read, left out");
            return Ok(None);
        }
        if !self.owner_allowed(&relative, opened.st_uid) {
            return Ok(None);
        }
        let mut meta = meta_of(relative.clone(), Kind::Dir, &opened);
        meta.acl_access = self.acl(fd.as_raw_fd(), acl::ACCESS_XATTR, &relative);
        meta.acl_default = self.acl(fd.as_raw_fd(), acl::DEFAULT_XATTR, &relative);
        archive::write_header(self.out, &meta)?;
        self.summary.entries += 1;
        match DirStream::open(fd) {
            Ok(dir) => Ok(Some(Frame {
                dir,
                relative,
                depth,
            })),
            Err(error) => {
                self.summary.unreadable += 1;
                self.problem(&relative, &format!("cannot list the folder: {error}"));
                Ok(None)
            }
        }
    }

    fn file(
        &mut self,
        parent: RawFd,
        name: &CStr,
        relative: Vec<u8>,
        stat: &libc::stat,
    ) -> io::Result<()> {
        if !self.same_mount(parent, name, &relative) {
            return Ok(());
        }
        let key = (dev_of(stat), ino_of(stat));
        if stat.st_nlink > 1 {
            if let Some(target) = self.links.get(&key) {
                let meta = meta_of(
                    relative,
                    Kind::HardLink {
                        target: target.clone(),
                    },
                    stat,
                );
                archive::write_header(self.out, &meta)?;
                self.summary.entries += 1;
                self.summary.hard_links += 1;
                return Ok(());
            }
        }
        let fd = match open_at(parent, name, FILE_FLAGS) {
            Ok(fd) => fd,
            Err(error) => {
                self.lookup_failed(&relative, &error);
                return Ok(());
            }
        };
        let opened = fstat(fd.as_raw_fd())?;
        if !same_inode(stat, &opened) || !is_regular(&opened) {
            self.summary.changed += 1;
            self.problem(&relative, "changed while it was read, left out");
            return Ok(());
        }
        if !self.owner_allowed(&relative, opened.st_uid) {
            return Ok(());
        }
        let size = u64::try_from(opened.st_size).unwrap_or(0);
        let allocated = u64::try_from(opened.st_blocks)
            .unwrap_or(0)
            .saturating_mul(512);
        let holes = match data_bytes(fd.as_raw_fd(), size, allocated) {
            Ok(data) => size - data,
            Err(error) => {
                self.summary.unreadable += 1;
                self.problem(&relative, &format!("cannot read it: {error}"));
                return Ok(());
            }
        };
        if holes > 0 {
            if self.holes.saturating_add(holes) > MAX_TREE_HOLES {
                self.summary.sparse += 1;
                self.problem(
                    &relative,
                    &format!(
                        "a sparse file of {size} bytes with {holes} bytes of holes, more than \
                         the folder's budget for holes; left out"
                    ),
                );
                return Ok(());
            }
            self.holes += holes;
        }
        let mut meta = meta_of(relative.clone(), Kind::File { size }, &opened);
        meta.acl_access = self.acl(fd.as_raw_fd(), acl::ACCESS_XATTR, &relative);
        archive::write_header(self.out, &meta)?;
        let mut file = std::fs::File::from(fd);
        let mut buffer = std::mem::take(&mut self.buffer);
        let mut remaining = size;
        let mut failure: Option<String> = None;
        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            if failure.is_none() {
                match file.read(&mut buffer[..want]) {
                    Ok(0) => {
                        self.summary.changed += 1;
                        failure = Some(format!(
                            "became {remaining} bytes shorter while it was read; the rest is zeros"
                        ));
                    }
                    Ok(read) => {
                        if let Err(error) = self.out.write_all(&buffer[..read]) {
                            self.buffer = buffer;
                            return Err(error);
                        }
                        remaining -= read as u64;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        self.summary.unreadable += 1;
                        failure = Some(format!("cannot read it ({error}); the rest is zeros"));
                    }
                }
            }
            buffer[..want].fill(0);
            if let Err(error) = self.out.write_all(&buffer[..want]) {
                self.buffer = buffer;
                return Err(error);
            }
            remaining -= want as u64;
        }
        self.buffer = buffer;
        archive::write_padding(self.out, size)?;
        if let Some(failure) = failure {
            self.problem(&relative, &failure);
        }
        self.summary.entries += 1;
        self.summary.files += 1;
        self.summary.bytes += size;
        if opened.st_nlink > 1
            && self.links.len() < MAX_LINKS
            && self.link_bytes + relative.len() <= MAX_LINK_PATH_BYTES
        {
            self.link_bytes += relative.len();
            self.links.insert(key, relative);
        }
        Ok(())
    }

    fn symlink(
        &mut self,
        parent: RawFd,
        name: &CStr,
        relative: Vec<u8>,
        stat: &libc::stat,
    ) -> io::Result<()> {
        let target = match readlink_at(parent, name) {
            Ok(target) => target,
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                self.summary.changed += 1;
                self.problem(&relative, "changed while it was read, left out");
                return Ok(());
            }
            Err(error) => {
                self.lookup_failed(&relative, &error);
                return Ok(());
            }
        };
        if target.is_empty() || target.len() > archive::MAX_LINK_BYTES {
            self.summary.too_long += 1;
            self.problem(&relative, "the link target is too long, left out");
            return Ok(());
        }
        let meta = meta_of(relative, Kind::Symlink { target }, stat);
        archive::write_header(self.out, &meta)?;
        self.summary.entries += 1;
        Ok(())
    }
}
