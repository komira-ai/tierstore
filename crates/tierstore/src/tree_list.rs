//! The tree list: every file, directory and symbolic link of a tree in one object, one line each,
//! which a client writes to the store before it asks a node for the tree.
//!
//! `crates/tierstore/docs/tree-manifest.md` ("The tree list") is the reference. The object is
//! JSON Lines, each line ended by `\n` and no other whitespace:
//!
//! ```text
//! {"format":"tierstore.tree-list","version":1}
//! {"path":"bin/cc","kind":"file","mode":493,"size":10,"key":"out/a1/0","offset":4096,"sha256":"<64 hex>"}
//! {"path":"include/empty.h","kind":"file","mode":420,"size":0}
//! {"path":"lib","kind":"symlink","target":"bin"}
//! {"path":"tmp","kind":"dir","mode":1023}
//! ```
//!
//! The entries are sorted by path, bytewise, with no path twice. The object is named
//! `cas/l/<SHA-256 of its bytes>` ([`tree_list_key`]) and written create-only, so a client that
//! sends the same tree again finds it written. Its key is the `ref` a job hands the mount provider
//! (`server::provider`): the provider reads the object, checks that it hashes to its name and makes
//! the node-local tree of exactly these entries. It is the flat successor of the per-directory
//! manifest of [`parse_manifest`](crate::parse_manifest), whose reader refuses it (it has no `v`).
//!
//! Nothing here reads or writes anything: [`parse_tree_list`] reads bytes the caller has,
//! [`TreeList::encode`] writes the bytes a client stores, and [`TreeList::into_entries`] gives the
//! entries of a tree for [`build_tree`](crate::build_tree), which judges what is below a file, the
//! entry and depth limits and the names a tree may not have.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::tree_manifest::{BlobSource, TreeDigest, is_valid_name};
use crate::tree_record::{Mode, NewEntry, NewKind};

/// The `format` of the first line.
const FORMAT: &str = "tierstore.tree-list";

/// The version this module reads and writes.
const VERSION: u64 = 1;

/// The first line of every tree list of this version, its newline included.
const HEADER: &str = "{\"format\":\"tierstore.tree-list\",\"version\":1}\n";

/// The prefix of the key a tree list is stored under: `cas/l/<64 hex>`.
pub const TREE_LIST_PREFIX: &str = "cas/l/";

/// The largest tree list, in bytes: the bound of one create request (`trees.max_request_bytes`).
pub const MAX_TREE_LIST_BYTES: usize = 256 * 1024 * 1024;

/// The longest path of an entry, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// The longest name on a path, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// The longest target of a symbolic link, in bytes.
pub const MAX_TARGET_BYTES: usize = 4096;

/// The SHA-256 of no bytes: the contents of every file of size 0.
pub(crate) const EMPTY_SHA256: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// Why a tree list was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeListError {
    /// The object is over [`MAX_TREE_LIST_BYTES`].
    #[error("tree list too large: {size} bytes, at most {max}")]
    TooLarge {
        /// The size of the object in bytes.
        size: usize,
        /// The bound.
        max: usize,
    },
    /// The bytes do not hash to the digest of the key they were read from.
    #[error("tree list digest mismatch: expected {expected}, the bytes hash to {actual}")]
    DigestMismatch {
        /// The digest the key names.
        expected: TreeDigest,
        /// The digest of the bytes.
        actual: TreeDigest,
    },
    /// A line that is not what the format says, numbered from 1 (the header).
    #[error("tree list line {line}: {fault}")]
    Line {
        /// The number of the line.
        line: usize,
        /// What is wrong with it.
        fault: LineFault,
    },
    /// A file of size above 0 with no `sha256`, in a list made in code (a read list refuses one at
    /// its line): a node-local tree's digest covers the contents.
    #[error("file {0:?} has no sha256")]
    MissingSha256(String),
    /// A name over 255 bytes, which a version 2 list cannot hold: written by
    /// [`TreeList::encode_v2`] only.
    #[error("name too long in {0:?}")]
    NameTooLong(String),
}

/// What is wrong with one line of a tree list.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LineFault {
    /// The object does not end with a newline; the line is the last.
    #[error("no newline at the end")]
    NoEnd,
    /// Not a JSON object, or a member of the wrong type.
    #[error("malformed: {0}")]
    Malformed(String),
    /// A member the line needs is absent.
    #[error("{what} without {member}")]
    Missing {
        /// The header or the kind of the entry.
        what: &'static str,
        /// The member.
        member: &'static str,
    },
    /// A header whose `format` is not `tierstore.tree-list`.
    #[error("unknown format {0:?}")]
    UnknownFormat(String),
    /// A header whose `version` is not 1.
    #[error("unsupported version")]
    UnsupportedVersion,
    /// A `kind` other than `file`, `dir` and `symlink`.
    #[error("unknown kind {0:?}")]
    UnknownKind(String),
    /// A path that is empty, over [`MAX_PATH_BYTES`], or has a name that is empty, `.`, `..`,
    /// over [`MAX_NAME_BYTES`], or has a NUL.
    #[error("bad path {0:?}")]
    BadPath(String),
    /// A target that is empty, over [`MAX_TARGET_BYTES`] or has a NUL.
    #[error("bad target of {0:?}")]
    BadTarget(String),
    /// A `mode` with a bit above 07777.
    #[error("bad mode {0}")]
    BadMode(u64),
    /// A `sha256` that is not 64 lowercase hex digits.
    #[error("malformed digest")]
    MalformedDigest,
    /// A file of size 0 with a `key` or an `offset`, or a larger one without both.
    #[error("a file of size 0 has no key or offset, and a larger one has both")]
    KeyAndOffset,
    /// A file whose offset plus size does not fit in 64 bits.
    #[error("range overflow")]
    RangeOverflow,
    /// A path the line before has too.
    #[error("duplicate path {0:?}")]
    Duplicate(String),
    /// A path that sorts before the one of the line before.
    #[error("not sorted: {path:?} follows {previous:?}")]
    Unsorted {
        /// The path of the line before.
        previous: String,
        /// The path of this line.
        path: String,
    },
}

/// What an entry of a tree list is, with the members of its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// A regular file.
    File {
        /// The permission bits, setuid, setgid and sticky.
        mode: Mode,
        /// The length of the contents.
        size: u64,
        /// Where the bytes are: the `size` bytes at `offset` of the object `key`. None exactly for
        /// a file of size 0, which needs no bytes.
        source: Option<BlobSource>,
        /// The SHA-256 of the contents: a read list has it for every file of size above 0, and may
        /// leave it out for one of size 0.
        sha256: Option<TreeDigest>,
    },
    /// A directory listed on its own: an empty one, or one whose mode is not 0755.
    Dir {
        /// The permission bits, setuid, setgid and sticky.
        mode: Mode,
    },
    /// A symbolic link.
    Symlink {
        /// What the link says, kept as written.
        target: String,
    },
}

/// One entry of a tree list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    /// The `/`-separated names from the root.
    pub path: String,
    /// What it is.
    pub kind: ListKind,
}

/// A tree list: the entries of one tree. A directory the paths below it imply need not be listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeList {
    /// The entries, in the order of the object (sorted by path, when read).
    pub entries: Vec<ListEntry>,
}

#[derive(Deserialize)]
struct RawHeader {
    format: Option<String>,
    version: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawEntry {
    path: String,
    kind: String,
    mode: Option<u64>,
    size: Option<u64>,
    key: Option<String>,
    offset: Option<u64>,
    sha256: Option<String>,
    target: Option<String>,
}

/// The key a tree list of these bytes is stored under: `cas/l/<SHA-256 of the bytes>`.
#[must_use]
pub fn tree_list_key(bytes: &[u8]) -> String {
    format!("{TREE_LIST_PREFIX}{}", TreeDigest::of(bytes))
}

/// The digest a key of the form `cas/l/<64 lowercase hex>` names; any other key is `None`.
#[must_use]
pub fn tree_list_digest(key: &str) -> Option<TreeDigest> {
    key.strip_prefix(TREE_LIST_PREFIX)
        .and_then(TreeDigest::from_hex)
}

/// Reads the tree list in `bytes`, which must hash to `expected` (the digest its key names).
///
/// The size is judged first, so that no more than [`MAX_TREE_LIST_BYTES`] are ever hashed; then
/// the digest; then each line in order, the header first. Members this reader does not know are
/// ignored; a format, a version or a kind it does not know is refused, since a reader that left
/// out an entry would make a wrong tree.
pub fn parse_tree_list(bytes: &[u8], expected: &TreeDigest) -> Result<TreeList, TreeListError> {
    parse_bounded(bytes, expected, MAX_TREE_LIST_BYTES)
}

/// [`parse_tree_list`] with the bound `max` in place of [`MAX_TREE_LIST_BYTES`].
pub(crate) fn parse_bounded(
    bytes: &[u8],
    expected: &TreeDigest,
    max: usize,
) -> Result<TreeList, TreeListError> {
    if bytes.len() > max {
        return Err(TreeListError::TooLarge {
            size: bytes.len(),
            max,
        });
    }
    let actual = TreeDigest::of(bytes);
    if actual != *expected {
        return Err(TreeListError::DigestMismatch {
            expected: *expected,
            actual,
        });
    }
    let Some(body) = bytes.strip_suffix(b"\n") else {
        let line = bytes.split(|b| *b == b'\n').count();
        return Err(at(line, LineFault::NoEnd));
    };
    let mut list = TreeList::default();
    for (index, line) in body.split(|b| *b == b'\n').enumerate() {
        let number = index + 1;
        if index == 0 {
            check_header(line).map_err(|fault| at(number, fault))?;
            continue;
        }
        let entry = entry_of(line).map_err(|fault| at(number, fault))?;
        if let Some(previous) = list.entries.last() {
            if previous.path == entry.path {
                return Err(at(number, LineFault::Duplicate(entry.path)));
            }
            if previous.path > entry.path {
                let fault = LineFault::Unsorted {
                    previous: previous.path.clone(),
                    path: entry.path,
                };
                return Err(at(number, fault));
            }
        }
        list.entries.push(entry);
    }
    Ok(list)
}

fn at(line: usize, fault: LineFault) -> TreeListError {
    TreeListError::Line { line, fault }
}

fn malformed(error: serde_json::Error) -> LineFault {
    LineFault::Malformed(error.to_string())
}

fn check_header(line: &[u8]) -> Result<(), LineFault> {
    let header: RawHeader = serde_json::from_slice(line).map_err(malformed)?;
    let missing = |member| LineFault::Missing {
        what: "header",
        member,
    };
    let format = header.format.ok_or_else(|| missing("format"))?;
    if format != FORMAT {
        return Err(LineFault::UnknownFormat(format));
    }
    match header.version {
        None => Err(missing("version")),
        Some(version) if version == VERSION => Ok(()),
        Some(_) => Err(LineFault::UnsupportedVersion),
    }
}

fn check_path(path: &str) -> Result<(), LineFault> {
    let names_ok = path
        .split('/')
        .all(|name| name.len() <= MAX_NAME_BYTES && is_valid_name(name));
    if path.len() > MAX_PATH_BYTES || !names_ok {
        return Err(LineFault::BadPath(path.to_owned()));
    }
    Ok(())
}

fn mode_of(bits: Option<u64>, what: &'static str) -> Result<Mode, LineFault> {
    let bits = bits.ok_or(LineFault::Missing {
        what,
        member: "mode",
    })?;
    u16::try_from(bits)
        .ok()
        .and_then(|small| Mode::new(small).ok())
        .ok_or(LineFault::BadMode(bits))
}

fn entry_of(line: &[u8]) -> Result<ListEntry, LineFault> {
    let raw: RawEntry = serde_json::from_slice(line).map_err(malformed)?;
    check_path(&raw.path)?;
    let kind = match raw.kind.as_str() {
        "file" => file_of(&raw)?,
        "dir" => ListKind::Dir {
            mode: mode_of(raw.mode, "dir")?,
        },
        "symlink" => {
            let target = raw.target.ok_or(LineFault::Missing {
                what: "symlink",
                member: "target",
            })?;
            if target.is_empty() || target.len() > MAX_TARGET_BYTES || target.contains('\0') {
                return Err(LineFault::BadTarget(raw.path));
            }
            ListKind::Symlink { target }
        }
        _ => return Err(LineFault::UnknownKind(raw.kind)),
    };
    Ok(ListEntry {
        path: raw.path,
        kind,
    })
}

fn file_of(raw: &RawEntry) -> Result<ListKind, LineFault> {
    let mode = mode_of(raw.mode, "file")?;
    let size = raw.size.ok_or(LineFault::Missing {
        what: "file",
        member: "size",
    })?;
    let sha256 = match &raw.sha256 {
        None => None,
        Some(text) => Some(TreeDigest::from_hex(text).ok_or(LineFault::MalformedDigest)?),
    };
    let source = match (&raw.key, raw.offset, size) {
        (None, None, 0) => None,
        (Some(key), Some(offset), 1..) => {
            if offset.checked_add(size).is_none() {
                return Err(LineFault::RangeOverflow);
            }
            Some(BlobSource {
                key: key.as_str().into(),
                offset,
            })
        }
        _ => return Err(LineFault::KeyAndOffset),
    };
    // A node-local tree's digest covers each file's contents, and each span is checked against it
    // as it is filled: a file with bytes needs its digest.
    if size > 0 && sha256.is_none() {
        return Err(LineFault::Missing {
            what: "file",
            member: "sha256",
        });
    }
    Ok(ListKind::File {
        mode,
        size,
        source,
        sha256,
    })
}

impl TreeList {
    /// The entries of the tree, for [`build_tree`](crate::build_tree), in the order of the list. A
    /// file of size 0 with no `sha256` is given the SHA-256 of no bytes; a larger one with none is
    /// refused ([`TreeListError::MissingSha256`]).
    pub fn into_entries(self) -> Result<Vec<NewEntry>, TreeListError> {
        self.entries
            .into_iter()
            .map(|entry| {
                let kind = match entry.kind {
                    ListKind::File {
                        mode,
                        size,
                        source,
                        sha256,
                    } => {
                        let sha256 = match (sha256, size) {
                            (Some(sha256), _) => sha256,
                            (None, 0) => TreeDigest::from_bytes(EMPTY_SHA256),
                            (None, _) => return Err(TreeListError::MissingSha256(entry.path)),
                        };
                        NewKind::File {
                            mode: Some(mode),
                            sha256,
                            size,
                            from: source,
                        }
                    }
                    ListKind::Dir { mode } => NewKind::Dir { mode: Some(mode) },
                    ListKind::Symlink { target } => NewKind::Symlink { target },
                };
                Ok(NewEntry {
                    path: entry.path,
                    kind,
                })
            })
            .collect()
    }

    /// The bytes a writer stores: the header line, then one line for each entry, sorted by path
    /// bytewise, each a compact JSON object with its members in the order of the reference (`path`,
    /// `kind`, then `mode`, `size`, `key`, `offset` and `sha256` of a file, `mode` of a directory,
    /// `target` of a link), a member that is not set left out, and every line ended by `\n`. The
    /// same entries in any order are therefore the same bytes and the same key.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut entries: Vec<&ListEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut out = String::from(HEADER);
        for entry in entries {
            out.push_str("{\"path\":");
            push_string(&mut out, &entry.path);
            match &entry.kind {
                ListKind::File {
                    mode,
                    size,
                    source,
                    sha256,
                } => {
                    let _ = write!(
                        out,
                        ",\"kind\":\"file\",\"mode\":{},\"size\":{size}",
                        mode.bits()
                    );
                    if let Some(source) = source {
                        out.push_str(",\"key\":");
                        push_string(&mut out, &source.key);
                        let _ = write!(out, ",\"offset\":{}", source.offset);
                    }
                    if let Some(sha256) = sha256 {
                        let _ = write!(out, ",\"sha256\":\"{sha256}\"");
                    }
                }
                ListKind::Dir { mode } => {
                    let _ = write!(out, ",\"kind\":\"dir\",\"mode\":{}", mode.bits());
                }
                ListKind::Symlink { target } => {
                    out.push_str(",\"kind\":\"symlink\",\"target\":");
                    push_string(&mut out, target);
                }
            }
            out.push_str("}\n");
        }
        out.into_bytes()
    }
}

/// Appends `text` as a JSON string: `"` and `\` escaped with a backslash, every character below
/// U+0020 as `\u00XX` (lowercase hex), and every other character as itself in UTF-8.
fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
