//! Asking a placement what a path is: `locate`, `resolve`, `stat` and `list`.
//!
//! The in-memory part of a lookup is a snapshot of the placement's tree and the trees it holds by
//! digest, taken under the lock; the records are then read from their files with no lock held,
//! one directory at a time, so a lookup of `a/b/c` reads three records and none of the rest. A
//! tree that is an image is in memory whole and is read in place: a lookup is a binary search of
//! each directory's children, and nothing is read from a file.

use std::os::unix::fs::FileExt as _;
use std::sync::Arc;

use super::TreeStore;
use super::files::{RECORDS, Source};
use super::location::{Located, PlacementId, under};
use super::state::TreeInfo;
use crate::tree_image::{ImageEntry, ImageKind, ImagePathError};
use crate::{Mode, RecordChild, TreeDigest, TreeImage, TreeRecord};

/// Why a question about a path was not answered.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// The store has no such placement: it was never made, it ended, or it was released and
    /// nothing holds it.
    #[error("no such placement")]
    NoPlacement,
    /// No such path in the tree.
    #[error("no such path")]
    NotFound,
    /// A name of the path is below a file or a symbolic link, or a listing was asked of a file.
    #[error("not a directory")]
    NotADirectory,
    /// The tree's files do not say what they should: a record or a source is missing or unreadable.
    #[error("the tree's files are damaged: {0}")]
    Corrupt(String),
}

/// What a file's bytes are: a range of the pack `pack`, starting at `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpan {
    /// The key of the object that holds the bytes, as the upstream names it.
    pub pack: String,
    /// The offset of the file's first byte in the object.
    pub offset: u64,
}

/// What a path of a placement is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A directory.
    Dir {
        /// Its mode.
        mode: Mode,
        /// How many children it has.
        entries: usize,
    },
    /// A regular file.
    File {
        /// Its mode.
        mode: Mode,
        /// Its length.
        size: u64,
        /// The SHA-256 of its bytes.
        sha256: TreeDigest,
        /// Where its bytes are fetched from; none for an empty file.
        span: Option<FileSpan>,
    },
    /// A symbolic link.
    Symlink {
        /// What it says.
        target: String,
    },
}

/// The kind of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
}

/// The kind and size of a path, with no data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// What it is.
    pub kind: Kind,
    /// The length of a file or of a link's target; 0 for a directory.
    pub size: u64,
    /// The mode: a link's is 0777.
    pub mode: Mode,
}

/// A page of the children of a directory, in name order. A directory of a tree that is an image
/// has no record and no digest of its own: its child entry says the digest of no bytes' tree,
/// all zeros, and a caller that wants what is below it lists its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The children of the page.
    pub entries: Vec<RecordChild>,
    /// The name to pass as `after` for the next page; none at the end.
    pub next: Option<String>,
}

/// What a path leads to.
enum Node {
    Dir(TreeRecord),
    File {
        mode: Mode,
        size: u64,
        sha256: TreeDigest,
    },
    Symlink(String),
}

/// The trees a lookup reads: the placement's, and the ones it holds by digest.
pub(super) struct Snapshot {
    pub(super) root: TreeDigest,
    closure: Vec<Arc<TreeInfo>>,
}

impl Snapshot {
    /// The image the placement's tree is, if it is one.
    pub(super) fn image(&self) -> Option<&Arc<TreeImage>> {
        self.closure.first().and_then(|tree| tree.image())
    }
}

/// The entry of `image` that `path` leads to.
fn image_entry<'a>(image: &'a TreeImage, path: &str) -> Result<ImageEntry<'a>, ReadError> {
    image.find_entry(path).map_err(|why| match why {
        ImagePathError::NotADirectory => ReadError::NotADirectory,
        ImagePathError::NotFound => ReadError::NotFound,
    })
}

/// The span of a file of an image: its own.
pub(super) fn image_span(span: Option<(&Arc<str>, u64)>) -> Option<FileSpan> {
    span.map(|(key, offset)| FileSpan {
        pack: key.to_string(),
        offset,
    })
}

/// The child entry of a listing that the entry `entry` of an image is.
fn image_child(entry: ImageEntry<'_>) -> RecordChild {
    let name = entry.name.to_owned();
    match entry.kind {
        ImageKind::Dir { .. } => RecordChild::Dir {
            name,
            digest: TreeDigest::from_bytes([0; 32]),
        },
        ImageKind::File {
            mode, size, sha256, ..
        } => RecordChild::File {
            name,
            mode,
            size,
            sha256,
        },
        ImageKind::Symlink { target } => RecordChild::Symlink {
            name,
            target: target.to_owned(),
        },
    }
}

impl TreeStore {
    /// The placement the key `key` of the bucket `bucket` falls under, if there is one alive and
    /// not released, and the path of the key below its root.
    #[must_use]
    pub fn locate_s3(&self, bucket: &str, key: &str) -> Option<Located> {
        self.locate(&format!("s3\0{bucket}\0"), key)
    }

    /// The placement the path `path` under the mount falls under, if there is one alive and not
    /// released, and the path below its root. `path` has no `/` at either end.
    #[must_use]
    pub fn locate_fs(&self, path: &str) -> Option<Located> {
        let mut located = self.locate("fs\0\0", &format!("{path}/"))?;
        if located.path.ends_with('/') {
            located.path.pop();
        }
        Some(located)
    }

    fn locate(&self, scope: &str, key: &str) -> Option<Located> {
        let state = self.shared.lock();
        let now = self.shared.now();
        let (id, below) = under(&state.table, scope, key)?;
        let placement = state.placements.get(&id).filter(|p| p.locatable(now))?;
        Some(Located {
            placement: id,
            tree: placement.tree,
            path: below.to_owned(),
        })
    }

    /// The trees of the placement, if it is alive.
    pub(super) fn snapshot(&self, id: &PlacementId) -> Result<Snapshot, ReadError> {
        let state = self.shared.lock();
        let now = self.shared.now();
        let placement = state
            .placements
            .get(id)
            .filter(|placement| placement.retained(now))
            .ok_or(ReadError::NoPlacement)?;
        let own = state.trees.get(&placement.tree);
        let closure = own
            .into_iter()
            .flat_map(|info| &info.closure)
            .filter_map(|tree| state.trees.get(tree).cloned())
            .collect();
        Ok(Snapshot {
            root: placement.tree,
            closure,
        })
    }

    /// The record `digest`, read from the file of the first tree that holds it.
    pub(super) fn record(
        &self,
        snapshot: &Snapshot,
        digest: &TreeDigest,
    ) -> Result<TreeRecord, ReadError> {
        let held = snapshot.closure.iter().find_map(|tree| {
            let place = tree.records()?.0.get(digest)?;
            Some((tree, *place))
        });
        let (tree, place) =
            held.ok_or_else(|| ReadError::Corrupt(format!("record {digest} is missing")))?;
        let path = super::files::tree_dir(&self.shared.root, &tree.root).join(RECORDS);
        let damaged = |why: String| ReadError::Corrupt(format!("record {digest}: {why}"));
        let file = std::fs::File::open(path).map_err(|why| damaged(why.to_string()))?;
        let mut bytes = vec![0; place.len as usize];
        file.read_exact_at(&mut bytes, place.offset)
            .map_err(|why| damaged(why.to_string()))?;
        TreeRecord::parse(&bytes).map_err(|why| damaged(why.to_string()))
    }

    /// What `path` of the tree of `snapshot` leads to.
    fn walk(&self, snapshot: &Snapshot, path: &str) -> Result<Node, ReadError> {
        let mut node = Node::Dir(self.record(snapshot, &snapshot.root)?);
        let names = path.split('/').filter(|_| !path.is_empty());
        for name in names {
            let Node::Dir(record) = node else {
                return Err(ReadError::NotADirectory);
            };
            let children = record.children();
            let at = children
                .binary_search_by(|child| child.name().cmp(name))
                .map_err(|_| ReadError::NotFound)?;
            node = match &children[at] {
                RecordChild::Dir { digest, .. } => Node::Dir(self.record(snapshot, digest)?),
                RecordChild::File {
                    mode, size, sha256, ..
                } => Node::File {
                    mode: *mode,
                    size: *size,
                    sha256: *sha256,
                },
                RecordChild::Symlink { target, .. } => Node::Symlink(target.clone()),
            };
        }
        Ok(node)
    }

    /// What `path` of the placement is. A file says where its bytes are in a pack; the cache says
    /// whether it holds them ([`LocalCache::span_of`](super::super::LocalCache::span_of)).
    ///
    /// # Errors
    /// [`ReadError`].
    pub fn resolve(&self, id: &PlacementId, path: &str) -> Result<Resolved, ReadError> {
        let snapshot = self.snapshot(id)?;
        if let Some(image) = snapshot.image() {
            return Ok(match image_entry(image, path)?.kind {
                ImageKind::Dir { mode, children } => Resolved::Dir {
                    mode,
                    entries: children.len(),
                },
                ImageKind::File {
                    mode,
                    size,
                    sha256,
                    span,
                } => Resolved::File {
                    mode,
                    size,
                    sha256,
                    span: image_span(span),
                },
                ImageKind::Symlink { target } => Resolved::Symlink {
                    target: target.to_owned(),
                },
            });
        }
        Ok(match self.walk(&snapshot, path)? {
            Node::Dir(record) => Resolved::Dir {
                mode: record.mode(),
                entries: record.children().len(),
            },
            Node::File { mode, size, sha256 } => Resolved::File {
                mode,
                size,
                sha256,
                span: Self::span(&snapshot, &sha256, size)?,
            },
            Node::Symlink(target) => Resolved::Symlink { target },
        })
    }

    /// Where the bytes of a file of `size` bytes whose SHA-256 is `sha256` are fetched from: the
    /// first of the placement's trees that has a source for it. An empty file has none.
    pub(super) fn span(
        snapshot: &Snapshot,
        sha256: &TreeDigest,
        size: u64,
    ) -> Result<Option<FileSpan>, ReadError> {
        if size == 0 {
            return Ok(None);
        }
        let found = snapshot.closure.iter().find_map(|tree| {
            let sources = tree.records()?.1;
            let source: &Source = sources.files.get(sha256.as_bytes())?;
            Some((sources, source))
        });
        let (sources, source) =
            found.ok_or_else(|| ReadError::Corrupt(format!("no source for {sha256}")))?;
        Ok(Some(FileSpan {
            pack: sources.packs[source.pack as usize].clone(),
            offset: source.offset,
        }))
    }

    /// The kind and size of `path` of the placement, with no source read.
    ///
    /// # Errors
    /// [`ReadError`].
    pub fn stat(&self, id: &PlacementId, path: &str) -> Result<Stat, ReadError> {
        let snapshot = self.snapshot(id)?;
        if let Some(image) = snapshot.image() {
            return Ok(match image_entry(image, path)?.kind {
                ImageKind::Dir { mode, .. } => Stat {
                    kind: Kind::Dir,
                    size: 0,
                    mode,
                },
                ImageKind::File { mode, size, .. } => Stat {
                    kind: Kind::File,
                    size,
                    mode,
                },
                ImageKind::Symlink { target } => Stat {
                    kind: Kind::Symlink,
                    size: target.len() as u64,
                    mode: Mode::SYMLINK,
                },
            });
        }
        Ok(match self.walk(&snapshot, path)? {
            Node::Dir(record) => Stat {
                kind: Kind::Dir,
                size: 0,
                mode: record.mode(),
            },
            Node::File { mode, size, .. } => Stat {
                kind: Kind::File,
                size,
                mode,
            },
            Node::Symlink(target) => Stat {
                kind: Kind::Symlink,
                size: target.len() as u64,
                mode: Mode::SYMLINK,
            },
        })
    }

    /// At most `limit` (at least one) children of the directory `path` of the placement, in name
    /// order, those after the name `after` if one is given.
    ///
    /// # Errors
    /// [`ReadError`]; [`ReadError::NotADirectory`] if `path` is not a directory.
    pub fn list(
        &self,
        id: &PlacementId,
        path: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Listing, ReadError> {
        let snapshot = self.snapshot(id)?;
        if let Some(image) = snapshot.image() {
            return image_list(image, path, after, limit);
        }
        let Node::Dir(record) = self.walk(&snapshot, path)? else {
            return Err(ReadError::NotADirectory);
        };
        let children = record.children();
        let first = after.map_or(0, |after| children.partition_point(|c| c.name() <= after));
        let rest = &children[first..];
        let page = &rest[..rest.len().min(limit.max(1))];
        let next = (page.len() < rest.len())
            .then(|| page.last().map(|last| last.name().to_owned()))
            .flatten();
        Ok(Listing {
            entries: page.to_vec(),
            next,
        })
    }
}

/// [`TreeStore::list`] of the directory `path` of `image`.
fn image_list(
    image: &TreeImage,
    path: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Listing, ReadError> {
    let dir = image_entry(image, path)?;
    let ImageKind::Dir { children, .. } = dir.kind else {
        return Err(ReadError::NotADirectory);
    };
    let rest = after.map_or(children, |after| image.children_after(dir.index, after));
    let take = rest.len().min(limit.max(1));
    let page = rest.start..rest.start + take as u32;
    let entries: Vec<RecordChild> = page
        .clone()
        .filter_map(|index| image.entry(index))
        .map(image_child)
        .collect();
    let next = (page.end < rest.end)
        .then(|| entries.last().map(|last| last.name().to_owned()))
        .flatten();
    Ok(Listing { entries, next })
}
