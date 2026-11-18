//! Reply payloads: the entry and attribute replies with their validity, and the entry lists of
//! READDIR and READDIRPLUS. A payload goes to [`crate::Replier::ok`] as its bytes (a struct's
//! `as_bytes`, a list's [`DirentBuf::as_bytes`]); CREATE answers an [`EntryOut`] followed by
//! an [`super::OpenOut`].

use std::time::Duration;

use super::wire::{Attr, AttrOut, Dirent, DirentPlus, EntryOut};

/// The longest name the kernel accepts in a directory entry (`FUSE_NAME_MAX` of the kernel's
/// `fs/fuse/fuse_i.h`).
pub const NAME_MAX: usize = 1024;

impl EntryOut {
    /// The entry of node `nodeid`, its name valid for `entry_valid` and its attributes for
    /// `attr_valid`.
    #[must_use]
    pub fn new(
        nodeid: u64,
        generation: u64,
        entry_valid: Duration,
        attr_valid: Duration,
        attr: Attr,
    ) -> Self {
        Self {
            nodeid,
            generation,
            entry_valid: entry_valid.as_secs(),
            attr_valid: attr_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr,
        }
    }
}

impl AttrOut {
    /// The attributes `attr`, valid for `attr_valid`.
    #[must_use]
    pub fn new(attr_valid: Duration, attr: Attr) -> Self {
        Self {
            attr_valid: attr_valid.as_secs(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            dummy: 0,
            attr,
        }
    }
}

/// The type of a directory entry: the kinds of node a tree or the put directory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A directory (`DT_DIR`).
    Directory,
    /// A regular file (`DT_REG`).
    File,
    /// A symbolic link (`DT_LNK`).
    Symlink,
}

impl EntryKind {
    /// The `DT_*` value of `fuse_dirent.type`.
    #[must_use]
    pub fn dtype(self) -> u32 {
        match self {
            Self::Directory => 4,
            Self::File => 8,
            Self::Symlink => 10,
        }
    }
}

/// Why an entry was not added to a directory list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DirPushError {
    /// The entry does not fit in the reply's size: the list is complete for this request, and
    /// the next READDIR resumes at the last entry's `off`.
    #[error("the directory entry does not fit in the reply")]
    Full,
    /// The name is empty, longer than [`NAME_MAX`], or holds `/` or NUL, which the kernel
    /// refuses (it fails the whole READDIR with `EIO`).
    #[error("the directory entry's name is empty, too long, or holds '/' or NUL")]
    InvalidName,
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && !name.iter().any(|&b| b == b'/' || b == 0)
}

/// `n` rounded up to a multiple of 8 (`FUSE_REC_ALIGN`).
fn rec_align(n: usize) -> usize {
    n.next_multiple_of(8)
}

/// Appends one record (`head`, the name, zero padding to 8 bytes) if it fits in `max`.
fn push_record(
    buf: &mut Vec<u8>,
    max: usize,
    head: &[u8],
    name: &[u8],
) -> Result<(), DirPushError> {
    if !valid_name(name) {
        return Err(DirPushError::InvalidName);
    }
    let len = rec_align(head.len() + name.len());
    if buf.len() + len > max {
        return Err(DirPushError::Full);
    }
    buf.extend_from_slice(head);
    buf.extend_from_slice(name);
    buf.resize(buf.len() + len - head.len() - name.len(), 0);
    Ok(())
}

/// The name length as `fuse_dirent.namelen`; a valid name is at most [`NAME_MAX`] bytes.
fn namelen(name: &[u8]) -> u32 {
    u32::try_from(name.len()).unwrap_or(u32::MAX)
}

/// The entries of one READDIR reply, at most `max` bytes (the request's `size`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirentBuf {
    buf: Vec<u8>,
    max: usize,
}

impl DirentBuf {
    /// An empty list for a reply of at most `max` bytes.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    /// Appends the entry `name` of node `ino`; `off` is the READDIR offset that resumes after
    /// it.
    ///
    /// # Errors
    ///
    /// [`DirPushError::Full`] if it does not fit (the list is unchanged);
    /// [`DirPushError::InvalidName`] for a name the kernel refuses.
    pub fn push(
        &mut self,
        ino: u64,
        off: u64,
        kind: EntryKind,
        name: &[u8],
    ) -> Result<(), DirPushError> {
        let head = Dirent {
            ino,
            off,
            namelen: namelen(name),
            typ: kind.dtype(),
        };
        push_record(&mut self.buf, self.max, head.as_bytes(), name)
    }

    /// The reply payload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// The entries of one READDIRPLUS reply, at most `max` bytes (the request's `size`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirentPlusBuf {
    buf: Vec<u8>,
    max: usize,
}

impl DirentPlusBuf {
    /// An empty list for a reply of at most `max` bytes.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    /// Appends the entry `name` of node `ino` with its lookup reply `entry` (an `entry` with
    /// `nodeid` 0, as for `.` and `..`, gives the kernel the name only); `off` is the
    /// READDIRPLUS offset that resumes after it.
    ///
    /// # Errors
    ///
    /// As [`DirentBuf::push`].
    pub fn push(
        &mut self,
        ino: u64,
        off: u64,
        kind: EntryKind,
        name: &[u8],
        entry: EntryOut,
    ) -> Result<(), DirPushError> {
        let head = DirentPlus {
            entry_out: entry,
            dirent: Dirent {
                ino,
                off,
                namelen: namelen(name),
                typ: kind.dtype(),
            },
        };
        push_record(&mut self.buf, self.max, head.as_bytes(), name)
    }

    /// The reply payload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}
