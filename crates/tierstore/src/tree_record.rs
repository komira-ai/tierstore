//! The tree record: the canonical binary form of one directory of a created tree, and the builder
//! that makes the records of a whole tree from the entries of a request.
//!
//! `docs/design/node-local-trees.md` (sections 3.2 and 4) is the reference, and
//! `docs/stored-formats.md` lists the format. A record is the bytes
//!
//! ```text
//! "TSTR"  version u8 (1)  mode u16 (big endian)  count uvarint  child*
//! child:  kind u8 (1 file, 2 dir, 3 symlink)  name_len uvarint  name
//!         file:    mode u16 (big endian)  size uvarint  sha256 (32 bytes)
//!         dir:     sha256 of the child's record (32 bytes)
//!         symlink: target_len uvarint  target
//! ```
//!
//! with the children strictly sorted by name, bytewise, and every varint in its shortest form, so
//! one directory has one encoding and its digest is the SHA-256 of those bytes. The digest of the
//! root's record is the digest of the tree. Where a file's bytes are fetched from is in no record,
//! so it is not part of any digest.
//!
//! Nothing here reads or writes anything: [`build_tree`] turns entries into records and digests,
//! and [`TreeRecord::parse`] reads one record.

use std::collections::{BTreeMap, HashSet};

use sha2::{Digest as _, Sha256};

use crate::tree_manifest::{BlobSource, MAX_DEPTH, MAX_ENTRIES, TreeDigest, is_valid_name};

/// The first four bytes of a record.
const MAGIC: &[u8; 4] = b"TSTR";

/// The only version of the record format.
const VERSION: u8 = 1;

const KIND_FILE: u8 = 1;
const KIND_DIR: u8 = 2;
const KIND_SYMLINK: u8 = 3;

/// The most children of one directory: the limit `trees.max_dir_entries` will default to.
pub const MAX_DIR_ENTRIES: usize = 1_000_000;

/// Why a record was refused, or why entries could not be made into a tree. The text of each
/// begins with the name the design gives the refusal.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// The record does not begin with `TSTR`.
    #[error("not a tree record: bad magic")]
    BadMagic,
    /// A version other than 1.
    #[error("unsupported tree version {0}")]
    UnsupportedVersion(u8),
    /// The record ends inside a field.
    #[error("tree record is truncated")]
    Truncated,
    /// Bytes after the last child.
    #[error("tree record has trailing bytes")]
    TrailingBytes,
    /// A varint with a more significant byte of zero, so not in its shortest form.
    #[error("tree record has a varint that is not in its shortest form")]
    NonMinimalVarint,
    /// A varint that does not fit in 64 bits.
    #[error("tree record has a varint over 64 bits")]
    VarintOverflow,
    /// A child kind other than 1, 2 and 3.
    #[error("unsupported entry kind {0}")]
    UnknownKind(u8),
    /// A name or a symlink target that is not UTF-8.
    #[error("tree record has text that is not UTF-8")]
    NotUtf8,
    /// A mode with a bit above 07777.
    #[error("bad mode {0:o}: bits above 07777")]
    ModeTooLarge(u16),
    /// A mode text that is not one to four octal digits.
    #[error("bad mode {0:?}")]
    BadMode(String),
    /// A name, or the name in a path, that is empty, `.`, `..`, has a `/` or a NUL, or begins
    /// `.wh.`; the text is the entry's path.
    #[error("unsupported entry name in {0:?}")]
    UnsupportedName(String),
    /// A name that does not sort after the one ahead of it (or equals it).
    #[error("children not strictly sorted by name: {name:?} follows {previous:?}")]
    Unsorted {
        /// The name ahead.
        previous: String,
        /// The name that does not sort after it.
        name: String,
    },
    /// Two entries with one path.
    #[error("duplicate path {0:?}")]
    Duplicate(String),
    /// A path that is a file, a symlink or a tree and also a directory another path lies below.
    #[error("file and directory at one path {0:?}")]
    FileAndDirectory(String),
    /// A `tree` entry at the root.
    #[error("a tree entry may not be the root")]
    RootTree,
    /// A file of size 0 that has a source.
    #[error("file {0:?} has size 0 and a source")]
    EmptyFileWithSource(String),
    /// A source whose offset plus the file's size does not fit in 64 bits.
    #[error("range overflow in the source of {0:?}")]
    RangeOverflow(String),
    /// More entries than the limit.
    #[error("tree too large: more than {0} entries")]
    TooManyEntries(u64),
    /// A path deeper than the limit.
    #[error("tree too deep at {0:?}")]
    TooDeep(String),
    /// A directory with more children than the limit.
    #[error("directory too large at {0:?}")]
    DirectoryTooLarge(String),
}

/// Unix permission bits with setuid, setgid and sticky: 0 to 07777. The file-type bits are not
/// carried; the kind of the entry says the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mode(u16);

impl Mode {
    /// The mode of a file with none given: 0644.
    pub const FILE: Self = Self(0o644);

    /// The mode of a directory with none given, and of the root: 0755.
    pub const DIR: Self = Self(0o755);

    /// The mode a symbolic link is served with, as one always is: 0777. It is in no record.
    pub const SYMLINK: Self = Self(0o777);

    /// The mode with these bits, or [`RecordError::ModeTooLarge`] for a bit above 07777.
    pub fn new(bits: u16) -> Result<Self, RecordError> {
        if bits > 0o7777 {
            return Err(RecordError::ModeTooLarge(bits));
        }
        Ok(Self(bits))
    }

    /// Reads one to four octal digits, as the request writes a mode (`"0644"`, `"4755"`, `"1777"`).
    pub fn parse_octal(text: &str) -> Result<Self, RecordError> {
        let digits = text.bytes().all(|b| matches!(b, b'0'..=b'7'));
        if !(1..=4).contains(&text.len()) || !digits {
            return Err(RecordError::BadMode(text.to_string()));
        }
        let bits = text
            .bytes()
            .fold(0u16, |acc, b| acc * 8 + u16::from(b - b'0'));
        Ok(Self(bits))
    }

    /// The bits, 0 to 07777.
    #[must_use]
    pub fn bits(self) -> u16 {
        self.0
    }
}

/// One child of a directory record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordChild {
    /// A regular file.
    File {
        /// The name.
        name: String,
        /// The permission bits.
        mode: Mode,
        /// The length of the contents.
        size: u64,
        /// The SHA-256 of the contents.
        sha256: TreeDigest,
    },
    /// A directory, by the digest of its record.
    Dir {
        /// The name.
        name: String,
        /// The digest of the child's record.
        digest: TreeDigest,
    },
    /// A symbolic link, with its target as the client wrote it.
    Symlink {
        /// The name.
        name: String,
        /// What the link says.
        target: String,
    },
}

impl RecordChild {
    /// The name of the child.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::File { name, .. } | Self::Dir { name, .. } | Self::Symlink { name, .. } => name,
        }
    }
}

/// The record of one directory: its mode and its children, sorted by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRecord {
    mode: Mode,
    children: Vec<RecordChild>,
}

impl TreeRecord {
    /// A record of `children`, which the caller has made with valid names, strictly sorted.
    pub(crate) fn from_sorted(mode: Mode, children: Vec<RecordChild>) -> Self {
        Self { mode, children }
    }

    /// A record of `children`, which must have valid names and be strictly sorted by name.
    pub fn new(mode: Mode, children: Vec<RecordChild>) -> Result<Self, RecordError> {
        for child in &children {
            if !is_valid_name(child.name()) {
                return Err(RecordError::UnsupportedName(child.name().to_string()));
            }
        }
        for pair in children.windows(2) {
            if pair[0].name() >= pair[1].name() {
                return Err(RecordError::Unsorted {
                    previous: pair[0].name().to_string(),
                    name: pair[1].name().to_string(),
                });
            }
        }
        Ok(Self { mode, children })
    }

    /// The directory's own mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The children, sorted by name.
    #[must_use]
    pub fn children(&self) -> &[RecordChild] {
        &self.children
    }

    /// The canonical bytes of the record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.children.len() * 48);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.mode.0.to_be_bytes());
        put_uvarint(&mut out, self.children.len() as u64);
        for child in &self.children {
            match child {
                RecordChild::File {
                    name,
                    mode,
                    size,
                    sha256,
                } => {
                    out.push(KIND_FILE);
                    put_text(&mut out, name);
                    out.extend_from_slice(&mode.0.to_be_bytes());
                    put_uvarint(&mut out, *size);
                    out.extend_from_slice(sha256.as_bytes());
                }
                RecordChild::Dir { name, digest } => {
                    out.push(KIND_DIR);
                    put_text(&mut out, name);
                    out.extend_from_slice(digest.as_bytes());
                }
                RecordChild::Symlink { name, target } => {
                    out.push(KIND_SYMLINK);
                    put_text(&mut out, name);
                    put_text(&mut out, target);
                }
            }
        }
        out
    }

    /// The digest of the record: the SHA-256 of its canonical bytes.
    #[must_use]
    pub fn digest(&self) -> TreeDigest {
        TreeDigest::from_bytes(Sha256::digest(self.encode()).into())
    }

    /// Reads one record, refusing whatever is not the canonical form of one: another magic or
    /// version, a mode above 07777, an unknown kind, a bad name, children not strictly sorted, a
    /// varint that is not the shortest, a truncated record, trailing bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, RecordError> {
        let mut reader = Reader { bytes };
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(RecordError::BadMagic);
        }
        let version = reader.u8()?;
        if version != VERSION {
            return Err(RecordError::UnsupportedVersion(version));
        }
        let mode = reader.mode()?;
        let count = reader.uvarint()?;
        let mut children = Vec::new();
        for _ in 0..count {
            children.push(reader.child()?);
        }
        if !reader.bytes.is_empty() {
            return Err(RecordError::TrailingBytes);
        }
        Self::new(mode, children)
    }
}

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_uvarint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// The unread rest of a record.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], RecordError> {
        if len > self.bytes.len() {
            return Err(RecordError::Truncated);
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }

    fn mode(&mut self) -> Result<Mode, RecordError> {
        let bytes = self.take(2)?;
        Mode::new(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn digest(&mut self) -> Result<TreeDigest, RecordError> {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(self.take(32)?);
        Ok(TreeDigest::from_bytes(bytes))
    }

    /// A varint of at most 64 bits in its shortest form.
    fn uvarint(&mut self) -> Result<u64, RecordError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err(RecordError::VarintOverflow);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(RecordError::NonMinimalVarint);
                }
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// A length and that many bytes of UTF-8.
    fn text(&mut self) -> Result<String, RecordError> {
        let len = usize::try_from(self.uvarint()?).unwrap_or(usize::MAX);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| RecordError::NotUtf8)
    }

    fn child(&mut self) -> Result<RecordChild, RecordError> {
        let kind = self.u8()?;
        let name = self.text()?;
        match kind {
            KIND_FILE => {
                let mode = self.mode()?;
                let size = self.uvarint()?;
                let sha256 = self.digest()?;
                Ok(RecordChild::File {
                    name,
                    mode,
                    size,
                    sha256,
                })
            }
            KIND_DIR => Ok(RecordChild::Dir {
                name,
                digest: self.digest()?,
            }),
            KIND_SYMLINK => Ok(RecordChild::Symlink {
                name,
                target: self.text()?,
            }),
            other => Err(RecordError::UnknownKind(other)),
        }
    }
}

/// The limits [`build_tree`] holds a request to. The defaults are those of the format; a node
/// may set stricter ones (`trees.max_entries`, `trees.max_depth`, `trees.max_dir_entries`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeLimits {
    /// The most entries below the root, counting the directories the paths imply.
    pub max_entries: u64,
    /// The most names on one path.
    pub max_depth: usize,
    /// The most children of one directory.
    pub max_dir_entries: usize,
}

impl Default for TreeLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_depth: MAX_DEPTH,
            max_dir_entries: MAX_DIR_ENTRIES,
        }
    }
}

/// An entry of a create request, below the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEntry {
    /// The `/`-separated names from the root; never empty.
    pub path: String,
    /// What the entry is.
    pub kind: NewKind,
}

/// What a [`NewEntry`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewKind {
    /// A file: its contents' SHA-256 and size are required; with no `mode` it is 0644; with no
    /// `from` the bytes are the whole object `cas/l/<sha256>`.
    File {
        /// The permission bits, if the client wrote any.
        mode: Option<Mode>,
        /// The SHA-256 of the contents.
        sha256: TreeDigest,
        /// The length of the contents.
        size: u64,
        /// Where to fetch the bytes from; in no digest.
        from: Option<BlobSource>,
    },
    /// An explicit directory, which is how an empty one is made, or a mode given to one the paths
    /// imply; with no `mode` it is 0755.
    Dir {
        /// The permission bits, if the client wrote any.
        mode: Option<Mode>,
    },
    /// A symbolic link.
    Symlink {
        /// What the link says, kept as written.
        target: String,
    },
    /// A tree created before, shared by its digest: the path is a directory with that record.
    Tree {
        /// The digest of the child's root record.
        digest: TreeDigest,
    },
}

/// A file of a built tree, for the fetch that follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltFile {
    /// The path from the root.
    pub path: String,
    /// The SHA-256 of the contents.
    pub sha256: TreeDigest,
    /// The length of the contents.
    pub size: u64,
    /// Where the bytes come from, if the entry said.
    pub source: Option<BlobSource>,
}

/// The result of [`build_tree`].
#[derive(Debug, PartialEq, Eq)]
pub struct BuiltTree {
    /// The digest of the tree: that of the root's record.
    pub root: TreeDigest,
    /// The record of every directory the entries made, each once, children before the
    /// directories that hold them and the root's last. A `tree` entry's record is not among them.
    pub records: Vec<(TreeDigest, TreeRecord)>,
    /// The files, sorted by path.
    pub files: Vec<BuiltFile>,
}

/// A child slot of a directory while a tree is built.
enum Slot {
    Dir(usize),
    File {
        mode: Mode,
        size: u64,
        sha256: TreeDigest,
    },
    Symlink(String),
    Tree(TreeDigest),
}

/// A directory while a tree is built. The directories are numbered in the order they appear, so
/// a directory's number is below its children's.
struct Node {
    mode: Option<Mode>,
    explicit: bool,
    children: BTreeMap<String, Slot>,
}

impl Node {
    fn implied() -> Self {
        Self {
            mode: None,
            explicit: false,
            children: BTreeMap::new(),
        }
    }
}

/// Builds the records of a tree from the entries of a request, bottom-up. The order of `entries`
/// and whether a subtree is sent as entries or as a `tree` child do not change any digest; neither
/// does a `from`. Modes default before hashing: 0644 for a file, 0755 for a directory and for the
/// root unless `root_mode` says.
///
/// The entry limit counts the entries sent and the directories they imply, not what a `tree` child
/// holds.
pub fn build_tree(
    root_mode: Option<Mode>,
    entries: Vec<NewEntry>,
    limits: &TreeLimits,
) -> Result<BuiltTree, RecordError> {
    let mut dirs = vec![Node {
        mode: root_mode,
        explicit: true,
        children: BTreeMap::new(),
    }];
    let mut files = Vec::new();
    let mut count = 0u64;
    for entry in entries {
        place(&mut dirs, &mut files, &mut count, entry, limits)?;
    }
    files.sort_by(|a: &BuiltFile, b: &BuiltFile| a.path.cmp(&b.path));
    let mut digests = vec![TreeDigest::from_bytes([0; 32]); dirs.len()];
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for (index, node) in dirs.into_iter().enumerate().rev() {
        let children = node
            .children
            .into_iter()
            .map(|(name, slot)| match slot {
                Slot::Dir(child) => RecordChild::Dir {
                    name,
                    digest: digests[child],
                },
                Slot::Tree(digest) => RecordChild::Dir { name, digest },
                Slot::File { mode, size, sha256 } => RecordChild::File {
                    name,
                    mode,
                    size,
                    sha256,
                },
                Slot::Symlink(target) => RecordChild::Symlink { name, target },
            })
            .collect();
        let record = TreeRecord {
            mode: node.mode.unwrap_or(Mode::DIR),
            children,
        };
        let digest = record.digest();
        digests[index] = digest;
        if seen.insert(digest) {
            records.push((digest, record));
        }
    }
    Ok(BuiltTree {
        root: digests[0],
        records,
        files,
    })
}

/// Puts one entry in the directories, making those its path implies.
fn place(
    dirs: &mut Vec<Node>,
    files: &mut Vec<BuiltFile>,
    count: &mut u64,
    entry: NewEntry,
    limits: &TreeLimits,
) -> Result<(), RecordError> {
    let NewEntry { path, kind } = entry;
    if path.is_empty() && matches!(kind, NewKind::Tree { .. }) {
        return Err(RecordError::RootTree);
    }
    if path.matches('/').count() >= limits.max_depth {
        return Err(RecordError::TooDeep(path));
    }
    let mut names = path.split('/');
    let leaf = names.next_back().unwrap_or(&path);
    if !is_valid_name(leaf) || !names.clone().all(is_valid_name) {
        return Err(RecordError::UnsupportedName(path));
    }
    let mut dir = 0;
    for name in names {
        dir = match dirs[dir].children.get(name) {
            Some(Slot::Dir(next)) => *next,
            Some(_) => return Err(RecordError::FileAndDirectory(path)),
            None => {
                let next = dirs.len();
                add(dirs, dir, name, Slot::Dir(next), count, &path, limits)?;
                dirs.push(Node::implied());
                next
            }
        };
    }
    let existing = dirs[dir].children.get(leaf);
    let slot = match (kind, existing) {
        (NewKind::Dir { mode }, Some(&Slot::Dir(next))) => {
            let node = &mut dirs[next];
            if node.explicit {
                return Err(RecordError::Duplicate(path));
            }
            node.explicit = true;
            node.mode = mode;
            return Ok(());
        }
        (_, Some(Slot::Dir(_))) => return Err(RecordError::FileAndDirectory(path)),
        (_, Some(_)) => return Err(RecordError::Duplicate(path)),
        (NewKind::Dir { mode }, None) => {
            dirs.push(Node {
                mode,
                explicit: true,
                children: BTreeMap::new(),
            });
            Slot::Dir(dirs.len() - 1)
        }
        (NewKind::Symlink { target }, None) => Slot::Symlink(target),
        (NewKind::Tree { digest }, None) => Slot::Tree(digest),
        (
            NewKind::File {
                mode,
                sha256,
                size,
                from,
            },
            None,
        ) => {
            if size == 0 && from.is_some() {
                return Err(RecordError::EmptyFileWithSource(path));
            }
            if from
                .as_ref()
                .is_some_and(|s| s.offset.checked_add(size).is_none())
            {
                return Err(RecordError::RangeOverflow(path));
            }
            files.push(BuiltFile {
                path: path.clone(),
                sha256,
                size,
                source: from,
            });
            Slot::File {
                mode: mode.unwrap_or(Mode::FILE),
                size,
                sha256,
            }
        }
    };
    add(dirs, dir, leaf, slot, count, &path, limits)
}

/// Adds `slot` as `name` in directory `dir`, counting it against the limits.
fn add(
    dirs: &mut [Node],
    dir: usize,
    name: &str,
    slot: Slot,
    count: &mut u64,
    path: &str,
    limits: &TreeLimits,
) -> Result<(), RecordError> {
    *count += 1;
    if *count > limits.max_entries {
        return Err(RecordError::TooManyEntries(limits.max_entries));
    }
    let children = &mut dirs[dir].children;
    if children.len() >= limits.max_dir_entries {
        return Err(RecordError::DirectoryTooLarge(path.to_string()));
    }
    children.insert(name.to_string(), slot);
    Ok(())
}
