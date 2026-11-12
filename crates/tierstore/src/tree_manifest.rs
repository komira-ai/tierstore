//! Tree manifests: the listing of one directory of a tree that tierstore serves as a file system,
//! and the flattening of a whole tree into numbered entries.
//!
//! A manifest is JSON, version 1 (`crates/tierstore/docs/tree-manifest.md` is the reference).
//! Its digest is the SHA-256 of its bytes as stored. Nothing here reads or writes anything: the
//! caller hands [`flatten`] the digest of a root and a function that returns the bytes of a
//! manifest by digest.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The most entries below a tree's root, counted across all its directories. The root makes
/// 2^24 inodes, and an index is 24 bits.
pub const MAX_ENTRIES: u64 = (1 << 24) - 1;

/// The largest tree manifest, in bytes.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

/// The deepest entry of a tree: the number of names on its path from the root.
pub const MAX_DEPTH: usize = 4096;

/// The only version of the manifest format.
const VERSION: u64 = 1;

/// The prefix of the names a view keeps for whiteouts, which no entry of a tree may have.
const WHITEOUT_PREFIX: &str = ".wh.";

/// Why a manifest or a tree was refused. The text of each begins with the name the design
/// gives the refusal.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// The manifest is over [`MAX_MANIFEST_BYTES`].
    #[error("manifest too large: {size} bytes, at most {MAX_MANIFEST_BYTES}")]
    ManifestTooLarge {
        /// The size of the manifest in bytes.
        size: usize,
    },
    /// The bytes do not hash to the digest they were fetched by.
    #[error("manifest digest mismatch: expected {expected}, the bytes hash to {actual}")]
    DigestMismatch {
        /// The digest asked for.
        expected: TreeDigest,
        /// The digest of the bytes.
        actual: TreeDigest,
    },
    /// Not JSON, not the shape of a manifest, or a field of the wrong type.
    #[error("malformed manifest: {0}")]
    Malformed(String),
    /// A `v` other than 1.
    #[error("unsupported manifest version")]
    UnsupportedVersion,
    /// A `kind` other than `dir`, `file` and `symlink`.
    #[error("unsupported entry kind")]
    UnsupportedKind,
    /// A name that is empty, `.`, `..`, has a `/` or a NUL, or begins `.wh.`.
    #[error("unsupported entry name {0:?}")]
    UnsupportedName(String),
    /// A name that sorts before the one ahead of it.
    #[error("entries not sorted by name: {name:?} follows {previous:?}")]
    Unsorted {
        /// The name ahead.
        previous: String,
        /// The name that sorts before it.
        name: String,
    },
    /// A name that occurs twice in one directory.
    #[error("duplicate entry name {0:?}")]
    Duplicate(String),
    /// A `sha256` that is not 64 lowercase hex digits.
    #[error("malformed digest of entry {0:?}")]
    MalformedDigest(String),
    /// A file of size 0 that has a `source`.
    #[error("file {0:?} has size 0 and a source")]
    EmptyFileWithSource(String),
    /// A `source` whose offset plus the file's size does not fit in 64 bits.
    #[error("source of file {0:?} runs past 2^64")]
    SourceOverflow(String),
    /// More than [`MAX_ENTRIES`] entries below the root.
    #[error("tree too large: more than {MAX_ENTRIES} entries")]
    TreeTooLarge,
    /// An entry deeper than [`MAX_DEPTH`].
    #[error("tree too deep: more than {MAX_DEPTH} levels")]
    TreeTooDeep,
    /// The caller's function could not give the bytes of a manifest.
    #[error("cannot fetch manifest {digest}: {reason}")]
    Fetch {
        /// The digest asked for.
        digest: TreeDigest,
        /// The caller's error, as text.
        reason: String,
    },
}

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeDigest([u8; 32]);

impl TreeDigest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The 32 bytes of the digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Reads 64 lowercase hex digits; anything else is `None`.
    pub fn from_hex(text: &str) -> Option<Self> {
        let lowercase = text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let mut out = [0u8; 32];
        if lowercase && hex::decode_to_slice(text, &mut out).is_ok() {
            Some(Self(out))
        } else {
            None
        }
    }
}

impl fmt::Display for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TreeDigest({self})")
    }
}

/// Where a file's bytes are in the store: the length is the file's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobSource {
    /// The object key; shared by every place the file is listed.
    pub key: Arc<str>,
    /// The offset of the first byte in that object.
    pub offset: u64,
}

/// What an entry is, with the attributes of its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory: the digest of its manifest, and that manifest's size in bytes.
    Dir {
        /// The digest of the child manifest.
        digest: TreeDigest,
        /// The size of the child manifest as the writer recorded it; not checked.
        size: u64,
    },
    /// A regular file.
    File {
        /// The digest of the contents.
        digest: TreeDigest,
        /// The length of the contents.
        size: u64,
        /// Whether the file is executable.
        exec: bool,
        /// Where the bytes are; without one they are the object `cas/l/<digest>`. Never set
        /// for a file of size 0.
        source: Option<BlobSource>,
    },
    /// A symbolic link.
    Symlink {
        /// What the link says; shared by every place the link is listed.
        target: Arc<str>,
    },
}

/// One child of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeChild {
    /// The name, unique in its directory.
    pub name: Arc<str>,
    /// What it is.
    pub kind: EntryKind,
}

/// A parsed, checked tree manifest: the children of one directory in the order written.
#[derive(Debug, PartialEq, Eq)]
pub struct TreeManifest {
    /// The children, sorted by name bytewise, each name unique.
    pub children: Vec<TreeChild>,
    size: u64,
}

impl TreeManifest {
    /// The length of the bytes this manifest was read from.
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// An entry of a flattened tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// The position in the tree flattened depth first in manifest order; the root is 0.
    pub index: u32,
    /// The index of the directory that holds it; `None` for the root.
    pub parent: Option<u32>,
    /// The name; empty for the root.
    pub name: Arc<str>,
    /// What it is.
    pub kind: EntryKind,
}

#[derive(Deserialize)]
struct Head {
    v: serde_json::Value,
}

#[derive(Deserialize)]
struct Body {
    children: Vec<Raw>,
}

#[derive(Deserialize)]
struct RawSource {
    key: String,
    offset: u64,
}

/// A child as written. The kind selects the fields, and a kind this reader does not know takes
/// whatever fields it has, so that it is refused by name and not as malformed.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Raw {
    Dir {
        name: String,
        sha256: String,
        size: u64,
    },
    File {
        name: String,
        sha256: String,
        size: u64,
        exec: Option<bool>,
        source: Option<RawSource>,
    },
    Symlink {
        name: String,
        target: String,
    },
    #[serde(other)]
    Unknown,
}

/// Reads the manifest in `bytes`, which must hash to `expected`.
///
/// The size is judged first, so that no more than [`MAX_MANIFEST_BYTES`] are ever hashed.
pub fn parse_manifest(bytes: &[u8], expected: &TreeDigest) -> Result<TreeManifest, TreeError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TreeError::ManifestTooLarge { size: bytes.len() });
    }
    let actual = TreeDigest::of(bytes);
    if actual != *expected {
        return Err(TreeError::DigestMismatch {
            expected: *expected,
            actual,
        });
    }
    let head: Head = serde_json::from_slice(bytes).map_err(malformed)?;
    if head.v != VERSION {
        return Err(TreeError::UnsupportedVersion);
    }
    let body: Body = serde_json::from_slice(bytes).map_err(malformed)?;
    let mut children: Vec<TreeChild> = Vec::with_capacity(body.children.len());
    for raw in body.children {
        let child = child_of(raw)?;
        if let Some(previous) = children.last() {
            if previous.name == child.name {
                return Err(TreeError::Duplicate(child.name.to_string()));
            }
            if previous.name > child.name {
                return Err(TreeError::Unsorted {
                    previous: previous.name.to_string(),
                    name: child.name.to_string(),
                });
            }
        }
        children.push(child);
    }
    Ok(TreeManifest {
        children,
        size: bytes.len() as u64,
    })
}

fn malformed(error: serde_json::Error) -> TreeError {
    TreeError::Malformed(error.to_string())
}

fn child_of(raw: Raw) -> Result<TreeChild, TreeError> {
    let (name, kind) = match raw {
        Raw::Unknown => return Err(TreeError::UnsupportedKind),
        Raw::Dir { name, sha256, size } => {
            check_name(&name)?;
            let digest = digest_of(&name, &sha256)?;
            (name, EntryKind::Dir { digest, size })
        }
        Raw::File {
            name,
            sha256,
            size,
            exec,
            source,
        } => {
            check_name(&name)?;
            let digest = digest_of(&name, &sha256)?;
            if source.is_some() && size == 0 {
                return Err(TreeError::EmptyFileWithSource(name));
            }
            if source
                .as_ref()
                .is_some_and(|s| s.offset.checked_add(size).is_none())
            {
                return Err(TreeError::SourceOverflow(name));
            }
            let source = source.map(|s| BlobSource {
                key: s.key.into(),
                offset: s.offset,
            });
            let exec = exec.unwrap_or(false);
            (
                name,
                EntryKind::File {
                    digest,
                    size,
                    exec,
                    source,
                },
            )
        }
        Raw::Symlink { name, target } => {
            check_name(&name)?;
            (
                name,
                EntryKind::Symlink {
                    target: target.into(),
                },
            )
        }
    };
    Ok(TreeChild {
        name: name.into(),
        kind,
    })
}

fn check_name(name: &str) -> Result<(), TreeError> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
        || name.starts_with(WHITEOUT_PREFIX);
    if bad {
        return Err(TreeError::UnsupportedName(name.to_string()));
    }
    Ok(())
}

fn digest_of(name: &str, text: &str) -> Result<TreeDigest, TreeError> {
    TreeDigest::from_hex(text).ok_or_else(|| TreeError::MalformedDigest(name.to_string()))
}

/// What is below a directory: its entries counted across all levels, and how many levels.
#[derive(Clone, Copy)]
struct Below {
    entries: u64,
    height: usize,
}

impl Below {
    /// What a manifest's own children make, before any directory in it is added.
    fn of(manifest: &TreeManifest) -> Self {
        Self {
            entries: manifest.children.len() as u64,
            height: usize::from(!manifest.children.is_empty()),
        }
    }

    /// Adds the subtree `child`, below a directory entry of a manifest whose children are at
    /// `depth`.
    fn absorb(&mut self, child: Self, depth: usize) -> Result<(), TreeError> {
        self.entries += child.entries;
        if self.entries > MAX_ENTRIES {
            return Err(TreeError::TreeTooLarge);
        }
        if depth + child.height > MAX_DEPTH {
            return Err(TreeError::TreeTooDeep);
        }
        self.height = self.height.max(child.height + 1);
        Ok(())
    }
}

/// A frame of the walk: a manifest, the next child to look at, and what the children seen so
/// far add up to.
struct Frame {
    digest: TreeDigest,
    manifest: Arc<TreeManifest>,
    next: usize,
    below: Below,
}

/// The manifests of a tree by digest, once the tree is within the limits, and its entry count.
struct Measured {
    manifests: HashMap<TreeDigest, Arc<TreeManifest>>,
    entries: u64,
}

/// Fetches the manifest `digest` and opens a frame for it. Its children are at `depth`: a
/// manifest with any is refused when that is over [`MAX_DEPTH`]. `opened` is the children of
/// all the distinct manifests opened so far, which this one's are added to: each is held once,
/// and every one is in the tree at least once, so a sum over [`MAX_ENTRIES`] is a tree over it
/// and nothing more is fetched.
fn open<E: fmt::Display>(
    digest: &TreeDigest,
    depth: usize,
    opened: &mut u64,
    fetch: &mut impl FnMut(&TreeDigest) -> Result<Vec<u8>, E>,
) -> Result<Frame, TreeError> {
    let bytes = fetch(digest).map_err(|e| TreeError::Fetch {
        digest: *digest,
        reason: e.to_string(),
    })?;
    let manifest = Arc::new(parse_manifest(&bytes, digest)?);
    let below = Below::of(&manifest);
    *opened += below.entries;
    if *opened > MAX_ENTRIES {
        return Err(TreeError::TreeTooLarge);
    }
    if below.height > 0 && depth > MAX_DEPTH {
        return Err(TreeError::TreeTooDeep);
    }
    Ok(Frame {
        digest: *digest,
        manifest,
        next: 0,
        below,
    })
}

/// Fetches every manifest of the tree once and counts its entries and levels, refusing a tree
/// over a limit as soon as the walk passes it. It allocates per distinct manifest, never per
/// entry, so a tree of 2^24 entries made of shared subtrees costs a few manifests, and the
/// children of the manifests it holds never add up to more than [`MAX_ENTRIES`]. The walk
/// keeps its own stack, so its depth is not the thread's.
fn measure<E: fmt::Display>(
    root: &TreeDigest,
    mut fetch: impl FnMut(&TreeDigest) -> Result<Vec<u8>, E>,
) -> Result<Measured, TreeError> {
    let mut manifests = HashMap::new();
    let mut finished: HashMap<TreeDigest, Below> = HashMap::new();
    let mut opened = 0;
    let mut current = open(root, 1, &mut opened, &mut fetch)?;
    manifests.insert(*root, current.manifest.clone());
    let mut parents: Vec<Frame> = Vec::new();
    loop {
        // The children of `current` are at this depth.
        let depth = parents.len() + 1;
        // The next directory of this manifest that is not yet measured, absorbing those that are.
        let mut descend = None;
        while let Some(child) = current.manifest.children.get(current.next) {
            current.next += 1;
            if let EntryKind::Dir { digest, .. } = &child.kind {
                match finished.get(digest) {
                    Some(known) => current.below.absorb(*known, depth)?,
                    None => {
                        descend = Some(*digest);
                        break;
                    }
                }
            }
        }
        if let Some(digest) = descend {
            let child = open(&digest, depth + 1, &mut opened, &mut fetch)?;
            manifests.insert(digest, child.manifest.clone());
            parents.push(std::mem::replace(&mut current, child));
        } else {
            finished.insert(current.digest, current.below);
            let Some(mut parent) = parents.pop() else {
                return Ok(Measured {
                    manifests,
                    entries: current.below.entries,
                });
            };
            parent.below.absorb(current.below, depth - 1)?;
            current = parent;
        }
    }
}

/// Flattens the tree whose root manifest has digest `root`: every entry below it, depth first
/// in manifest order, preceded by the root as index 0. The same manifests give the same
/// indices. A directory that occurs more than once is listed at each place, with its
/// manifest fetched once.
///
/// `fetch` returns the bytes of the manifest with the digest it is given. A tree over a limit is
/// refused before any entry is made.
pub fn flatten<E: fmt::Display>(
    root: &TreeDigest,
    fetch: impl FnMut(&TreeDigest) -> Result<Vec<u8>, E>,
) -> Result<Vec<TreeEntry>, TreeError> {
    let measured = measure(root, fetch)?;
    let mut entries = Vec::with_capacity(measured.entries as usize + 1);
    let root_manifest = measured.manifests[root].clone();
    entries.push(TreeEntry {
        index: 0,
        parent: None,
        name: "".into(),
        kind: EntryKind::Dir {
            digest: *root,
            size: root_manifest.size(),
        },
    });
    let mut stack = vec![(root_manifest, 0usize, 0u32)];
    while let Some((manifest, next, parent)) = stack.last_mut() {
        let Some(child) = manifest.children.get(*next) else {
            stack.pop();
            continue;
        };
        *next += 1;
        let parent = *parent;
        let index = entries.len() as u32;
        let descend = match &child.kind {
            EntryKind::Dir { digest, .. } => Some(measured.manifests[digest].clone()),
            _ => None,
        };
        entries.push(TreeEntry {
            index,
            parent: Some(parent),
            name: child.name.clone(),
            kind: child.kind.clone(),
        });
        if let Some(manifest) = descend {
            stack.push((manifest, 0, index));
        }
    }
    Ok(entries)
}

#[cfg(test)]
pub(crate) fn count_entries<E: fmt::Display>(
    root: &TreeDigest,
    fetch: impl FnMut(&TreeDigest) -> Result<Vec<u8>, E>,
) -> Result<u64, TreeError> {
    measure(root, fetch).map(|m| m.entries)
}
