//! POSIX ACLs as the Linux kernel stores them (`system.posix_acl_access` and
//! `system.posix_acl_default` extended attributes), read and written through
//! open file descriptors only.
//!
//! The helper works on trees that other people can change (shared folders),
//! so it never calls `setfacl` with a path: every ACL is encoded here and set
//! with `fsetxattr` on a descriptor that was opened with `O_NOFOLLOW` and
//! checked.

use std::io;
use std::os::fd::RawFd;

/// The xattr of the access ACL.
pub const ACCESS_XATTR: &str = "system.posix_acl_access";
/// The xattr of the default ACL (directories only).
pub const DEFAULT_XATTR: &str = "system.posix_acl_default";

const VERSION: u32 = 2;
const UNDEFINED_ID: u32 = u32::MAX;
/// Largest ACL read back (the kernel limit for ext4 is one block).
const MAX_XATTR_BYTES: usize = 64 * 1024;

/// What an entry applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tag {
    UserObj,
    User(u32),
    GroupObj,
    Group(u32),
    Mask,
    Other,
}

impl Tag {
    fn code(self) -> u16 {
        match self {
            Self::UserObj => 0x01,
            Self::User(_) => 0x02,
            Self::GroupObj => 0x04,
            Self::Group(_) => 0x08,
            Self::Mask => 0x10,
            Self::Other => 0x20,
        }
    }

    fn id(self) -> u32 {
        match self {
            Self::User(id) | Self::Group(id) => id,
            _ => UNDEFINED_ID,
        }
    }

    fn from_parts(code: u16, id: u32) -> Option<Self> {
        Some(match code {
            0x01 => Self::UserObj,
            0x02 => Self::User(id),
            0x04 => Self::GroupObj,
            0x08 => Self::Group(id),
            0x10 => Self::Mask,
            0x20 => Self::Other,
            _ => return None,
        })
    }
}

/// One entry: a tag and its `rwx` bits (4, 2, 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entry {
    pub tag: Tag,
    pub perm: u16,
}

/// A whole ACL, in the order the kernel requires (by tag, then id).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acl(pub Vec<Entry>);

impl Acl {
    pub fn new(mut entries: Vec<Entry>) -> Self {
        entries.sort_by_key(|entry| (entry.tag.code(), entry.tag.id()));
        entries.dedup_by_key(|entry| entry.tag);
        Self(entries)
    }

    /// The xattr value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.0.len() * 8);
        out.extend_from_slice(&VERSION.to_le_bytes());
        for entry in &self.0 {
            out.extend_from_slice(&entry.tag.code().to_le_bytes());
            out.extend_from_slice(&(entry.perm & 0o7).to_le_bytes());
            out.extend_from_slice(&entry.tag.id().to_le_bytes());
        }
        out
    }

    /// Parse an xattr value; `None` for anything malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (header, rest) = bytes.split_at_checked(4)?;
        if u32::from_le_bytes(header.try_into().ok()?) != VERSION || rest.len() % 8 != 0 {
            return None;
        }
        let mut entries = Vec::with_capacity(rest.len() / 8);
        for chunk in rest.as_chunks::<8>().0 {
            let code = u16::from_le_bytes([chunk[0], chunk[1]]);
            let perm = u16::from_le_bytes([chunk[2], chunk[3]]);
            let id = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            entries.push(Entry {
                tag: Tag::from_parts(code, id)?,
                perm: perm & 0o7,
            });
        }
        Some(Self(entries))
    }

    /// Named group entries (`group:<gid>`).
    pub fn named_groups(&self) -> impl Iterator<Item = (u32, u16)> + '_ {
        self.0.iter().filter_map(|entry| match entry.tag {
            Tag::Group(gid) => Some((gid, entry.perm)),
            _ => None,
        })
    }

    /// Named user entries (`user:<uid>`).
    pub fn named_users(&self) -> impl Iterator<Item = (u32, u16)> + '_ {
        self.0.iter().filter_map(|entry| match entry.tag {
            Tag::User(uid) => Some((uid, entry.perm)),
            _ => None,
        })
    }

    /// `user::…,group::…,other::…` in `getfacl` notation (for reports).
    pub fn describe(&self) -> String {
        let bits = |perm: u16| {
            format!(
                "{}{}{}",
                if perm & 4 != 0 { 'r' } else { '-' },
                if perm & 2 != 0 { 'w' } else { '-' },
                if perm & 1 != 0 { 'x' } else { '-' }
            )
        };
        self.0
            .iter()
            .map(|entry| match entry.tag {
                Tag::UserObj => format!("u::{}", bits(entry.perm)),
                Tag::User(uid) => format!("u:{uid}:{}", bits(entry.perm)),
                Tag::GroupObj => format!("g::{}", bits(entry.perm)),
                Tag::Group(gid) => format!("g:{gid}:{}", bits(entry.perm)),
                Tag::Mask => format!("m::{}", bits(entry.perm)),
                Tag::Other => format!("o::{}", bits(entry.perm)),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn c_name(name: &str) -> std::ffi::CString {
    std::ffi::CString::new(name).expect("xattr names have no NUL")
}

/// The raw value of an ACL xattr of `fd` (`None`: not set, or the file
/// system has no ACLs).
pub fn read_raw(fd: RawFd, xattr: &str) -> io::Result<Option<Vec<u8>>> {
    let name = c_name(xattr);
    let mut buffer = vec![0u8; MAX_XATTR_BYTES];
    // SAFETY: valid fd, NUL-terminated name, `buffer` is writable for its
    // length.
    let size =
        unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if size < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::EOPNOTSUPP) => Ok(None),
            _ => Err(error),
        };
    }
    buffer.truncate(size as usize);
    Ok(Some(buffer))
}

/// The ACL of `fd` (`None`: none set).
pub fn read(fd: RawFd, xattr: &str) -> io::Result<Option<Acl>> {
    match read_raw(fd, xattr)? {
        None => Ok(None),
        Some(raw) => Acl::decode(&raw)
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed ACL")),
    }
}

/// Set an ACL xattr of `fd` to `value`.
pub fn write_raw(fd: RawFd, xattr: &str, value: &[u8]) -> io::Result<()> {
    let name = c_name(xattr);
    // SAFETY: valid fd, NUL-terminated name, `value` is readable for its
    // length.
    let rc = unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn write(fd: RawFd, xattr: &str, acl: &Acl) -> io::Result<()> {
    write_raw(fd, xattr, &acl.encode())
}

/// Remove an ACL xattr of `fd` (missing is not an error).
pub fn remove(fd: RawFd, xattr: &str) -> io::Result<()> {
    let name = c_name(xattr);
    // SAFETY: valid fd and NUL-terminated name.
    if unsafe { libc::fremovexattr(fd, name.as_ptr()) } != 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) => Ok(()),
            _ => Err(error),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn entry(tag: Tag, perm: u16) -> Entry {
        Entry { tag, perm }
    }

    #[test]
    fn acls_are_encoded_in_kernel_order_and_read_back() {
        let acl = Acl::new(vec![
            entry(Tag::Other, 0),
            entry(Tag::Group(1_200_002), 5),
            entry(Tag::User(998), 7),
            entry(Tag::Mask, 7),
            entry(Tag::GroupObj, 7),
            entry(Tag::UserObj, 7),
            entry(Tag::User(997), 7),
        ]);
        let tags: Vec<Tag> = acl.0.iter().map(|entry| entry.tag).collect();
        assert_eq!(
            tags,
            vec![
                Tag::UserObj,
                Tag::User(997),
                Tag::User(998),
                Tag::GroupObj,
                Tag::Group(1_200_002),
                Tag::Mask,
                Tag::Other
            ]
        );
        let bytes = acl.encode();
        assert_eq!(bytes.len(), 4 + 7 * 8);
        assert_eq!(&bytes[..4], &2u32.to_le_bytes());
        assert_eq!(Acl::decode(&bytes), Some(acl.clone()));
        assert_eq!(
            acl.describe(),
            "u::rwx,u:997:rwx,u:998:rwx,g::rwx,g:1200002:r-x,m::rwx,o::---"
        );
        assert_eq!(acl.named_groups().collect::<Vec<_>>(), vec![(1_200_002, 5)]);
        for bad in [&b""[..], &b"\x01\0\0\0"[..], &b"\x02\0\0\0\x01\0"[..]] {
            assert_eq!(Acl::decode(bad), None);
        }
        let mut unknown = 2u32.to_le_bytes().to_vec();
        unknown.extend_from_slice(&[0x40, 0, 7, 0, 0, 0, 0, 0]);
        assert_eq!(Acl::decode(&unknown), None);
    }

    #[test]
    fn acls_are_set_and_removed_through_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let fd = std::fs::File::open(dir.path()).unwrap();
        let acl = Acl::new(vec![
            entry(Tag::UserObj, 7),
            entry(Tag::User(4_000_000), 5),
            entry(Tag::GroupObj, 5),
            entry(Tag::Mask, 5),
            entry(Tag::Other, 0),
        ]);
        match write(fd.as_raw_fd(), DEFAULT_XATTR, &acl) {
            Ok(()) => {}
            // File systems without ACLs (some CI temp folders).
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => return,
            Err(error) => panic!("{error}"),
        }
        assert_eq!(read(fd.as_raw_fd(), DEFAULT_XATTR).unwrap(), Some(acl));
        remove(fd.as_raw_fd(), DEFAULT_XATTR).unwrap();
        remove(fd.as_raw_fd(), DEFAULT_XATTR).unwrap();
        assert_eq!(read(fd.as_raw_fd(), DEFAULT_XATTR).unwrap(), None);
    }
}
