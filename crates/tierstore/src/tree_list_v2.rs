//! The tree list, version 2: the entries of a tree as fixed little-endian records over tables of
//! the distinct pack keys, names and link targets, which a reader checks once and walks once.
//!
//! `crates/tierstore/docs/tree-manifest.md` ("The tree list, version 2") is the reference. The
//! object is a header, three string tables and the records:
//!
//! ```text
//! header   magic "TSLIST\r\n", version 2, header length, record length, record count,
//!          and where the key, name and target tables and the records begin
//! keys     each object key a file's bytes are in, once, strictly ascending
//! names    each name of an entry, once, strictly ascending
//! targets  each symbolic link target, once, strictly ascending
//! records  {kind, mode, name, parent, key or target, size, offset, sha256}, 64 bytes each:
//!          every directory first, each after its parent; then the links; then the files,
//!          ordered by key and offset, the files of size 0 last
//! ```
//!
//! [`TreeListV2::open`] checks the header, where each section is and the key table, and nothing
//! else: it copies nothing and allocates once for the keys. [`TreeListV2::build`] then walks the
//! records once, in their order: it checks each, hands every span to read to its caller as it
//! meets it, each digest once, and puts the entry in the tree; after the walk it checks the name
//! and target tables and hashes the directories, children before parents. Because the files are
//! ordered by key and offset, the spans come pack by pack, each pack's in ascending offset; because
//! the names are ascending, a directory's children sort by their name's number.
//!
//! The object is named and stored as a version 1 list is: `cas/l/<SHA-256 of its bytes>`
//! ([`tree_list_key`](crate::tree_list_key)), create-only. Neither reader here checks that digest:
//! the caller does, beside the walk ([`server::provider`](crate) hashes on another thread while
//! the walk runs and refuses the list before its tree is placed).

use std::sync::Arc;

use crate::tree_list::{MAX_NAME_BYTES, MAX_TARGET_BYTES, MAX_TREE_LIST_BYTES};
use crate::tree_manifest::is_valid_name;
use crate::tree_record::RecordError;

pub use crate::tree_list_v2_build::ListSpan;

/// The first eight bytes of every version 2 tree list. A version 1 list begins `{`.
pub const TREE_LIST_V2_MAGIC: [u8; 8] = *b"TSLIST\r\n";

/// The version this module reads and writes.
pub(crate) const VERSION: u32 = 2;

/// The length of the header this module writes, and the least a reader takes.
pub(crate) const HEADER_LEN: u32 = 40;

/// The length of a record this module writes, and the least a reader takes.
pub(crate) const RECORD_LEN: u32 = 64;

/// The index that names nothing: the parent of an entry at the root, the key of a file of size 0.
pub(crate) const NONE: u32 = u32::MAX;

/// The kind of a directory record.
pub(crate) const KIND_DIR: u8 = 1;
/// The kind of a symbolic link record.
pub(crate) const KIND_SYMLINK: u8 = 2;
/// The kind of a file record.
pub(crate) const KIND_FILE: u8 = 3;

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
    /// A version other than 2.
    #[error("unsupported tree list version {0}")]
    UnsupportedVersion(u32),
    /// The header, or where it says a section is, does not fit the object.
    #[error("tree list header: {0}")]
    Header(HeaderFault),
    /// A string of a table that is not what the format says.
    #[error("tree list {table} {index}: {fault}")]
    Table {
        /// The table.
        table: Table,
        /// The number of the string in it, from 0.
        index: u32,
        /// What is wrong with it.
        fault: StringFault,
    },
    /// A record that is not what the format says, numbered from 0.
    #[error("tree list record {index}: {fault}")]
    Record {
        /// The number of the record.
        index: u32,
        /// What is wrong with it.
        fault: RecordFault,
    },
    /// The records are well formed, but not a tree this node makes: a name twice in one
    /// directory, or over a limit.
    #[error("the tree is refused: {0}")]
    Tree(RecordError),
}

/// What is wrong with the header of a version 2 tree list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderFault {
    /// The object is shorter than a header.
    #[error("truncated")]
    Truncated,
    /// A header length below 40 or past the end of the object.
    #[error("bad header length {0}")]
    HeaderLength(u32),
    /// A record length below 64.
    #[error("bad record length {0}")]
    RecordLength(u32),
    /// A section that does not fit in the object.
    #[error("the {0} section is past the end")]
    Section(Section),
}

/// A section of a version 2 tree list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// A string table.
    Table(Table),
    /// The records.
    Records,
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table(table) => write!(f, "{table}"),
            Self::Records => f.write_str("record"),
        }
    }
}

/// A string table of a version 2 tree list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    /// The keys of the objects that hold the bytes of the files.
    Keys,
    /// The names of the entries.
    Names,
    /// The targets of the symbolic links.
    Targets,
}

impl std::fmt::Display for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keys => "key",
            Self::Names => "name",
            Self::Targets => "target",
        })
    }
}

/// What is wrong with one string of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StringFault {
    /// Its end is before its start, or past the end of the object.
    #[error("past the end of its table")]
    Bounds,
    /// Not UTF-8.
    #[error("not UTF-8")]
    NotUtf8,
    /// Not after the string before it, bytewise.
    #[error("not strictly ascending")]
    NotAscending,
    /// A key that is empty or over [`MAX_KEY_BYTES`].
    #[error("bad key")]
    BadKey,
    /// A name that is empty, `.`, `..`, over 255 bytes, has a `/` or a NUL, or begins `.wh.`.
    #[error("bad name")]
    BadName,
    /// A target that is empty, over 4096 bytes or has a NUL.
    #[error("bad target")]
    BadTarget,
}

/// What is wrong with one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordFault {
    /// A kind other than 1 (directory), 2 (link) and 3 (file).
    #[error("unknown kind {0}")]
    UnknownKind(u8),
    /// A record of a kind that comes before the kind of the record before it.
    #[error("out of order: the directories come first, then the links, then the files")]
    KindOrder,
    /// A mode with a bit above 07777.
    #[error("bad mode {0:o}")]
    BadMode(u16),
    /// A name index that is not in the name table.
    #[error("no name {0}")]
    Name(u32),
    /// A parent that is not a directory record before this one.
    #[error("parent {0} is not a directory before it")]
    Parent(u32),
    /// A key index that is not in the key table.
    #[error("no key {0}")]
    Key(u32),
    /// A target index that is not in the target table.
    #[error("no target {0}")]
    Target(u32),
    /// A file of size 0 with a key, or a larger one without one.
    #[error("a file of size 0 has no key, and a larger one has one")]
    KeyAndSize,
    /// A file whose offset plus size does not fit in 64 bits.
    #[error("range overflow")]
    RangeOverflow,
    /// A file whose key and offset sort before the file's before it.
    #[error("files not sorted by key and offset")]
    Unsorted,
    /// A file of size 0 whose digest is not the SHA-256 of no bytes.
    #[error("a file of size 0 has the digest of no bytes")]
    EmptyDigest,
}

/// One string table: `count` ends, each the end of a string in the bytes that follow them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Strings<'a> {
    table: Table,
    count: u32,
    /// The `count` little-endian ends.
    ends: &'a [u8],
    /// The bytes of the strings, the first at 0.
    data: &'a [u8],
}

impl<'a> Strings<'a> {
    /// The table at `at` of `bytes`, if its count, its ends and its last end fit.
    fn at(bytes: &'a [u8], at: u32, table: Table) -> Result<Self, TreeListV2Error> {
        let past = || TreeListV2Error::Header(HeaderFault::Section(Section::Table(table)));
        let at = at as usize;
        let count = read_u32(bytes, at).ok_or_else(past)?;
        let ends_at = at + 4;
        let ends_len = count as usize * 4;
        let ends = bytes.get(ends_at..ends_at + ends_len).ok_or_else(past)?;
        let data_at = ends_at + ends_len;
        let len = match count {
            0 => 0,
            _ => read_u32(ends, ends_len - 4).unwrap_or(0) as usize,
        };
        let data = bytes.get(data_at..data_at + len).ok_or_else(past)?;
        Ok(Self {
            table,
            count,
            ends,
            data,
        })
    }

    /// How many strings the table has.
    pub(crate) fn len(&self) -> u32 {
        self.count
    }

    /// The bytes of string `index` (below [`len`](Self::len)), or a fault if its ends do not hold.
    fn raw(&self, index: u32) -> Result<&'a [u8], StringFault> {
        let i = index as usize;
        let end = read_u32(self.ends, i * 4).unwrap_or(0) as usize;
        let start = match i {
            0 => 0,
            _ => read_u32(self.ends, i * 4 - 4).unwrap_or(0) as usize,
        };
        self.data.get(start..end).ok_or(StringFault::Bounds)
    }

    /// String `index`, which [`check`](Self::check) has found good.
    pub(crate) fn get(&self, index: u32) -> &'a str {
        self.raw(index)
            .ok()
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .unwrap_or_default()
    }

    /// Checks every string: in bounds, UTF-8, strictly ascending, and accepted by `valid`.
    pub(crate) fn check(
        &self,
        valid: fn(&str) -> bool,
        bad: StringFault,
    ) -> Result<(), TreeListV2Error> {
        let mut previous: Option<&[u8]> = None;
        for index in 0..self.count {
            let fault = |fault| TreeListV2Error::Table {
                table: self.table,
                index,
                fault,
            };
            let raw = self.raw(index).map_err(fault)?;
            let text = std::str::from_utf8(raw).map_err(|_| fault(StringFault::NotUtf8))?;
            if previous.is_some_and(|previous| previous >= raw) {
                return Err(fault(StringFault::NotAscending));
            }
            if !valid(text) {
                return Err(fault(bad));
            }
            previous = Some(raw);
        }
        Ok(())
    }
}

/// Whether `key` may be the key of a pack: 1 to [`MAX_KEY_BYTES`] bytes.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_BYTES
}

/// Whether `name` may be the name of an entry.
pub(crate) fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES && is_valid_name(name)
}

/// Whether `target` may be the target of a link: 1 to 4096 bytes, no NUL.
pub(crate) fn valid_target(target: &str) -> bool {
    !target.is_empty() && target.len() <= MAX_TARGET_BYTES && !target.contains('\0')
}

/// The little-endian `u32` at `at` of `bytes`, if it is there.
pub(crate) fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let four = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([four[0], four[1], four[2], four[3]]))
}

/// The little-endian `u64` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u64(bytes: &[u8]) -> u64 {
    let mut eight = [0; 8];
    eight.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(eight)
}

/// A version 2 tree list, its header and key table checked, over the bytes of the object.
#[derive(Clone, Debug)]
pub struct TreeListV2<'a> {
    /// The records, `count` of `record_len` bytes each.
    pub(crate) records: &'a [u8],
    pub(crate) count: u32,
    pub(crate) record_len: usize,
    /// The keys, each shared by the files whose bytes it holds.
    pub(crate) keys: Vec<Arc<str>>,
    pub(crate) names: Strings<'a>,
    pub(crate) targets: Strings<'a>,
}

impl<'a> TreeListV2<'a> {
    /// Opens the list in `bytes`: checks the size, the magic, the version, that every section is
    /// within the object, and every key. The records, the names and the targets are checked by
    /// [`build`](Self::build), as it walks them.
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
        let field = |at| read_u32(bytes, at).ok_or(TreeListV2Error::Header(HeaderFault::Truncated));
        let version = field(8)?;
        if version != VERSION {
            return Err(TreeListV2Error::UnsupportedVersion(version));
        }
        let header_len = field(12)?;
        let record_len = field(16)?;
        let count = field(20)?;
        let (keys_at, names_at, targets_at, records_at) =
            (field(24)?, field(28)?, field(32)?, field(36)?);
        if header_len < HEADER_LEN || header_len as usize > bytes.len() {
            return Err(TreeListV2Error::Header(HeaderFault::HeaderLength(
                header_len,
            )));
        }
        if record_len < RECORD_LEN {
            return Err(TreeListV2Error::Header(HeaderFault::RecordLength(
                record_len,
            )));
        }
        let records_len = u64::from(count) * u64::from(record_len);
        let records = usize::try_from(records_len)
            .ok()
            .and_then(|len| bytes.get(records_at as usize..(records_at as usize).checked_add(len)?))
            .ok_or(TreeListV2Error::Header(HeaderFault::Section(
                Section::Records,
            )))?;
        let key_table = Strings::at(bytes, keys_at, Table::Keys)?;
        let names = Strings::at(bytes, names_at, Table::Names)?;
        let targets = Strings::at(bytes, targets_at, Table::Targets)?;
        key_table.check(valid_key, StringFault::BadKey)?;
        let keys = (0..key_table.len())
            .map(|index| Arc::from(key_table.get(index)))
            .collect();
        Ok(Self {
            records,
            count,
            record_len: record_len as usize,
            keys,
            names,
            targets,
        })
    }

    /// How many entries the list has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the list has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}
