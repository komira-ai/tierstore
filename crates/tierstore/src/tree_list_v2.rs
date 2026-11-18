//! The tree list, version 2 (the `TSTREE` format): a tree as fixed 32-byte records in
//! breadth-first order over a table of digests, a table of pack keys and a blob of names, which a
//! reader checks in one walk over its own bytes, and that holds the reads to issue.
//!
//! `crates/tierstore/docs/tree-manifest.md` ("The tree list, version 2") is the reference. The
//! object is a 64-byte header and six sections, each 8-byte aligned, in this order:
//!
//! ```text
//! header       magic "TSTREE\0\0", major 1, minor, required flags, six (offset, count)
//! entries      count x 32 bytes: the root first, then breadth first, each directory's children
//!              next to one another and sorted by name, bytewise
//! sha256       count x 32 bytes: the SHA-256 of entry i's bytes (zero for a directory or link)
//! key offsets  (keys + 1) x 4 bytes: where each pack key begins in the key blob, then its end
//! keys         the pack keys, back to back, each 1 to 1024 bytes
//! names        the names of the entries and the targets of the links, back to back
//! ranges       count x 24 bytes: {key, offset, length} of the packs, sorted and merged
//! ```
//!
//! [`TreeListV2::open`] checks the header, where each section is, the keys and the ranges; it
//! borrows the sections from the bytes and copies only the keys. [`TreeListV2::check`] then walks
//! the entries once, directory by directory: it checks each child and hands its caller the span of
//! every file whose digest it has not met before as soon as the file is checked. What it gives
//! makes the tree store's records ([`Checked::build`]); [`TreeListV2::build`] does both. The
//! ranges ([`TreeListV2::ranges`]) are the reads to issue, pack by pack, merged across small
//! gaps: the mount provider reads the spans the walk handed out a range at a time, each span
//! checked against its own digest as its bytes pass.
//!
//! The object is named and stored as a version 1 list is: `cas/l/<SHA-256 of its bytes>`
//! ([`tree_list_key`](crate::tree_list_key)), create-only. Neither function here checks that
//! digest: the mount provider hashes the object on another thread while it walks it, and places
//! the tree only once the digest is the one the key names.
//!
//! [`BuiltTree`]: crate::BuiltTree

use std::sync::Arc;

use crate::tree_list::MAX_TREE_LIST_BYTES;

pub use crate::tree_list_v2_build::{Checked, ListSpan};

/// The first eight bytes of every version 2 tree list. A version 1 list begins `{`.
pub const TREE_LIST_V2_MAGIC: [u8; 8] = *b"TSTREE\0\0";

/// The major version this module reads and writes; a reader refuses any other.
pub(crate) const MAJOR: u16 = 1;

/// The length of the header.
pub(crate) const HEADER_LEN: usize = 64;

/// The length of an entry record, and of a digest.
pub(crate) const RECORD_LEN: usize = 32;

/// The length of a range record.
pub(crate) const RANGE_LEN: usize = 24;

/// The kind of a file, in the top four bits of `kind_mode`.
pub(crate) const KIND_FILE: u8 = 0;
/// The kind of a directory.
pub(crate) const KIND_DIR: u8 = 1;
/// The kind of a symbolic link.
pub(crate) const KIND_SYMLINK: u8 = 2;

/// The longest key of a pack, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;

/// Whether `bytes` begin as a version 2 tree list does.
#[must_use]
pub fn is_tree_list_v2(bytes: &[u8]) -> bool {
    bytes.starts_with(&TREE_LIST_V2_MAGIC)
}

/// Why a version 2 tree list was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeListV2Error {
    /// The object is over [`MAX_TREE_LIST_BYTES`].
    #[error("tree list too large: {size} bytes, at most {max}")]
    TooLarge {
        /// The size of the object in bytes.
        size: usize,
        /// The bound.
        max: usize,
    },
    /// The object does not begin with [`TREE_LIST_V2_MAGIC`].
    #[error("not a version 2 tree list: bad magic")]
    BadMagic,
    /// A major version other than 1.
    #[error("unsupported tree list version {0}")]
    UnsupportedVersion(u16),
    /// The header, or where it says a section is, is not what the format says.
    #[error("tree list header: {0}")]
    Header(HeaderFault),
    /// A pack key that is not what the format says, numbered from 0.
    #[error("tree list key {index}: {fault}")]
    Key {
        /// The number of the key.
        index: u32,
        /// What is wrong with it.
        fault: KeyFault,
    },
    /// A range that names no key, is empty, or ends past 2^64, numbered from 0.
    #[error("tree list range {0}: no such key, empty, or past 2^64")]
    Range(u32),
    /// An entry that is not what the format says, numbered from 0 (the root).
    #[error("tree list entry {index}: {fault}")]
    Entry {
        /// The number of the entry.
        index: u32,
        /// What is wrong with it.
        fault: EntryFault,
    },
    /// Entries from this number on are in no directory's children.
    #[error("tree list entries from {0} on are in no directory")]
    Orphans(u32),
    /// Paths that together are over the bound: a tree whose version 1 list could not be stored,
    /// and whose paths the walk would otherwise make from far fewer bytes of shared names.
    #[error("tree list paths over {max} bytes together")]
    PathBytes {
        /// The bound, [`MAX_TREE_LIST_BYTES`].
        max: usize,
    },
    /// The entries are well formed, but not a tree this node makes: over a limit.
    #[error("the tree is refused: {0}")]
    Tree(crate::RecordError),
}

/// What is wrong with the header of a version 2 tree list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderFault {
    /// The object is shorter than a header.
    #[error("truncated")]
    Truncated,
    /// A required flag this reader does not know.
    #[error("unknown required flags {0:#x}")]
    RequiredFlags(u32),
    /// A section whose offset is not a multiple of 8.
    #[error("the {0} section is not 8-byte aligned")]
    Misaligned(Section),
    /// A section that begins before the end of the one before it.
    #[error("the {0} section overlaps the one before it")]
    Overlap(Section),
    /// A section that ends past the end of the object.
    #[error("the {0} section is past the end")]
    PastEnd(Section),
    /// An entry count of 0: there is always the root. (The size of the object bounds the count
    /// from above: at most 2^23 records of 32 bytes fit in 256 MiB.)
    #[error("bad entry count {0}")]
    EntryCount(u32),
    /// A digest table whose count is not the entry count.
    #[error("{0} digests for another count of entries")]
    DigestCount(u32),
    /// Key offsets that do not end at the end of the key blob, or point past it.
    #[error("the key offsets do not end at the end of the keys")]
    KeysLength,
}

/// A section of a version 2 tree list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// The entry records.
    Entries,
    /// The digests of the entries.
    Digests,
    /// The offsets of the keys.
    KeyOffsets,
    /// The bytes of the keys.
    Keys,
    /// The bytes of the names and targets.
    Names,
    /// The ranges to read.
    Ranges,
}

/// The sections, in the order of the header and of the object.
const SECTIONS: [Section; 6] = [
    Section::Entries,
    Section::Digests,
    Section::KeyOffsets,
    Section::Keys,
    Section::Names,
    Section::Ranges,
];

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Entries => "entry",
            Self::Digests => "sha256",
            Self::KeyOffsets => "key offset",
            Self::Keys => "key",
            Self::Names => "name",
            Self::Ranges => "range",
        })
    }
}

/// What is wrong with one pack key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyFault {
    /// The first key does not begin at 0.
    #[error("does not begin at 0")]
    Start,
    /// A key of no bytes: its end is not after its start.
    #[error("empty")]
    Empty,
    /// A key over [`MAX_KEY_BYTES`].
    #[error("over 1024 bytes")]
    TooLong,
    /// Not UTF-8.
    #[error("not UTF-8")]
    NotUtf8,
}

/// What is wrong with one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryFault {
    /// Entry 0 is not a directory with parent 0 and no name.
    #[error("the root is not a directory with no name and parent 0")]
    Root,
    /// A kind other than 0 (file), 1 (directory) and 2 (link).
    #[error("unknown kind {0}")]
    UnknownKind(u8),
    /// A directory that no directory before it holds.
    #[error("a directory before the children of the directories before it")]
    Unreachable,
    /// A directory whose first child is not the entry after the children of the directories
    /// before it, or whose children run past the last entry.
    #[error("children not where the breadth-first order puts them")]
    Children,
    /// A child whose parent is not the directory whose children it is among.
    #[error("parent {0} is not the directory that holds it")]
    Parent(u32),
    /// A name past the end of the name blob.
    #[error("name past the end of the names")]
    NameBounds,
    /// A name that is empty, `.`, `..`, not UTF-8, or has a `/` or a NUL.
    #[error("bad name")]
    BadName,
    /// A name that does not sort after its sibling's before it, bytewise: two siblings of one
    /// name, or children out of order.
    #[error("name not after its sibling's")]
    NotAscending,
    /// A path over 4096 bytes.
    #[error("path over 4096 bytes")]
    PathTooLong,
    /// A file of size above 0 whose key index is not in the key table.
    #[error("no key {0}")]
    Key(u32),
    /// A file whose offset plus size does not fit in 64 bits.
    #[error("range overflow")]
    RangeOverflow,
    /// A file of size 0 whose digest is not the SHA-256 of no bytes.
    #[error("a file of size 0 has the digest of no bytes")]
    EmptyDigest,
    /// A link target that is empty, over 4096 bytes, past the end of the names, not UTF-8, or
    /// has a NUL.
    #[error("bad target")]
    Target,
}

/// A range of a pack the writer says the tree reads: a hint, checked only to be in bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRange {
    /// The number of the key of the pack, in [`TreeListV2::keys`].
    pub key: u32,
    /// The first byte.
    pub offset: u64,
    /// The number of bytes, above 0.
    pub len: u64,
}

/// The little-endian `u16` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The little-endian `u32` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The little-endian `u64` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut eight = [0; 8];
    eight.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(eight)
}

/// A version 2 tree list, its header, keys and ranges checked, over the bytes of the object.
#[derive(Clone, Debug)]
pub struct TreeListV2<'a> {
    /// The entry records, `count` of 32 bytes.
    pub(crate) entries: &'a [u8],
    /// The digests, `count` of 32 bytes.
    pub(crate) digests: &'a [u8],
    pub(crate) count: u32,
    /// The keys, each shared by the files whose bytes it holds.
    pub(crate) keys: Vec<Arc<str>>,
    /// The names and targets.
    pub(crate) names: &'a [u8],
    /// The range records.
    ranges: &'a [u8],
}

impl<'a> TreeListV2<'a> {
    /// Opens the list in `bytes`: checks the size, the magic, the version, the required flags,
    /// that the sections are aligned, in order and within the object, the counts, every key and
    /// every range. The entries are checked by [`build`](Self::build), as it walks them.
    pub fn open(bytes: &'a [u8]) -> Result<Self, TreeListV2Error> {
        if bytes.len() > MAX_TREE_LIST_BYTES {
            return Err(TreeListV2Error::TooLarge {
                size: bytes.len(),
                max: MAX_TREE_LIST_BYTES,
            });
        }
        if !is_tree_list_v2(bytes) {
            return Err(TreeListV2Error::BadMagic);
        }
        let header = TreeListV2Error::Header;
        if bytes.len() < HEADER_LEN {
            return Err(header(HeaderFault::Truncated));
        }
        let major = le_u16(bytes, 8);
        if major != MAJOR {
            return Err(TreeListV2Error::UnsupportedVersion(major));
        }
        let flags = le_u32(bytes, 12);
        if flags != 0 {
            return Err(header(HeaderFault::RequiredFlags(flags)));
        }
        let mut slices: [&[u8]; 6] = [&[]; 6];
        let mut counts = [0u32; 6];
        let mut previous_end = HEADER_LEN;
        for (at, section) in SECTIONS.iter().enumerate() {
            let offset = le_u32(bytes, 16 + 8 * at) as usize;
            let count = le_u32(bytes, 20 + 8 * at);
            // No sum or product overflows: a count is below 2^32, an offset too, and a section
            // is at most 32 bytes a count.
            let len = match section {
                Section::Entries | Section::Digests => count as usize * RECORD_LEN,
                Section::KeyOffsets => (count as usize + 1) * 4,
                Section::Keys | Section::Names => count as usize,
                Section::Ranges => count as usize * RANGE_LEN,
            };
            if !offset.is_multiple_of(8) {
                return Err(header(HeaderFault::Misaligned(*section)));
            }
            if offset < previous_end {
                return Err(header(HeaderFault::Overlap(*section)));
            }
            let end = offset + len;
            slices[at] = bytes
                .get(offset..end)
                .ok_or(header(HeaderFault::PastEnd(*section)))?;
            counts[at] = count;
            previous_end = end;
        }
        let count = counts[0];
        if count == 0 {
            return Err(header(HeaderFault::EntryCount(count)));
        }
        if counts[1] != count {
            return Err(header(HeaderFault::DigestCount(counts[1])));
        }
        let keys = keys_of(slices[2], slices[3])?;
        let list = Self {
            entries: slices[0],
            digests: slices[1],
            count,
            keys,
            names: slices[4],
            ranges: slices[5],
        };
        for index in 0..counts[5] {
            list.range(index).ok_or(TreeListV2Error::Range(index))?;
        }
        Ok(list)
    }

    /// The keys of the objects that hold the files' bytes, which a span or a range names by its
    /// number.
    #[must_use]
    pub fn keys(&self) -> &[Arc<str>] {
        &self.keys
    }

    /// How many entries the list has, the root included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the list has nothing but the root.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 1
    }

    /// The ranges of the packs the writer says the tree reads, in the order of the list: a hint
    /// of what to read, merged across small gaps, which [`open`](Self::open) checked to name a key
    /// and to be in bounds, and nothing more.
    pub fn ranges(&self) -> impl Iterator<Item = ListRange> + '_ {
        let count = (self.ranges.len() / RANGE_LEN) as u32;
        (0..count).filter_map(|index| self.range(index))
    }

    /// Range `index`, if it names a key, is not empty and ends within 64 bits.
    fn range(&self, index: u32) -> Option<ListRange> {
        let at = index as usize * RANGE_LEN;
        let key = le_u32(self.ranges, at);
        let offset = le_u64(self.ranges, at + 8);
        let len = le_u64(self.ranges, at + 16);
        let valid =
            (key as usize) < self.keys.len() && len > 0 && offset.checked_add(len).is_some();
        valid.then_some(ListRange { key, offset, len })
    }
}

/// The keys of the key offsets `offsets` into `blob`: each 1 to [`MAX_KEY_BYTES`] bytes of UTF-8,
/// the first at 0, the last ending at the end of `blob`.
fn keys_of(offsets: &[u8], blob: &[u8]) -> Result<Vec<Arc<str>>, TreeListV2Error> {
    let count = offsets.len() / 4 - 1;
    let fault = |index: usize, fault| TreeListV2Error::Key {
        index: index as u32,
        fault,
    };
    if le_u32(offsets, 0) != 0 {
        return Err(fault(0, KeyFault::Start));
    }
    let mut keys = Vec::with_capacity(count);
    for index in 0..count {
        let start = le_u32(offsets, index * 4) as usize;
        let end = le_u32(offsets, index * 4 + 4) as usize;
        if end <= start {
            return Err(fault(index, KeyFault::Empty));
        }
        if end - start > MAX_KEY_BYTES {
            return Err(fault(index, KeyFault::TooLong));
        }
        let raw = blob
            .get(start..end)
            .ok_or(TreeListV2Error::Header(HeaderFault::KeysLength))?;
        let key = std::str::from_utf8(raw).map_err(|_| fault(index, KeyFault::NotUtf8))?;
        keys.push(Arc::from(key));
    }
    if le_u32(offsets, count * 4) as usize != blob.len() {
        return Err(TreeListV2Error::Header(HeaderFault::KeysLength));
    }
    Ok(keys)
}
