//! A tree of the mount, for lookups: flattened (its entries in the order of their indices, and for
//! each directory the indices of its children), or an image read in place (below). A tree a placement shows at an fs location also has the
//! mode of each entry, as stored, and the path of the location.
//!
//! A placed tree that is a version 2 tree list is held as its image ([`TreeImage`]) and read in
//! place: an entry's index is its place in the image, a directory's children are a range of
//! indices sorted by name, and an entry is made from its record when it is asked for. Nothing is
//! held per entry.

use std::ops::Range;
use std::sync::Arc;

use crate::tree_image::ImageKind;
use crate::tree_manifest::{BlobSource, EntryKind, TreeEntry};
use crate::{Mode, TreeDigest, TreeImage};

/// The entries of one tree.
#[derive(Debug)]
pub(super) struct Tree {
    body: Body,
    /// The fs location of a placed tree: names joined by `/`.
    location: Option<String>,
}

/// How the entries of a tree are held.
#[derive(Debug)]
enum Body {
    /// Every entry, made once.
    Listed {
        entries: Vec<TreeEntry>,
        /// `starts[i]..starts[i + 1]` is where the children of entry `i` are in `kids`.
        starts: Vec<u32>,
        /// The indices of the children of every entry, grouped by parent, each group in the order
        /// of the manifest (sorted by name).
        kids: Vec<u32>,
        /// The mode of every entry, by index, for a placed tree; empty for a tree of manifests,
        /// whose modes follow from the kinds.
        modes: Vec<u16>,
    },
    /// A version 2 tree list, read in place.
    Image(TreeImage),
}

/// The indices of the children of a directory, in name order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kids<'a> {
    /// Listed one by one.
    Listed(&'a [u32]),
    /// The indices of a range: the children of a directory of an image.
    Range(Range<u32>),
}

impl Kids<'_> {
    /// Those from the `at`-th on, in order.
    pub(super) fn from(&self, at: usize) -> impl Iterator<Item = u32> + '_ {
        let (listed, range) = match self {
            Self::Listed(kids) => (kids.get(at..).unwrap_or_default(), 0..0),
            Self::Range(range) => {
                let start =
                    u32::try_from(at).map_or(range.end, |at| range.start.saturating_add(at));
                (&[][..], start.min(range.end)..range.end)
            }
        };
        listed.iter().copied().chain(range)
    }

    /// The place of the first that `before` is false of: they are all before the others.
    pub(super) fn partition_point(&self, before: impl Fn(u32) -> bool) -> usize {
        match self {
            Self::Listed(kids) => kids.partition_point(|kid| before(*kid)),
            Self::Range(range) => {
                let (mut low, mut high) = (range.start, range.end);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if before(middle) {
                        low = middle + 1;
                    } else {
                        high = middle;
                    }
                }
                (low - range.start) as usize
            }
        }
    }
}

impl Tree {
    /// The tree of `entries`, which `flatten` made: the root first, a parent before its children.
    pub(super) fn new(entries: Vec<TreeEntry>) -> Self {
        Self::listed(entries, Vec::new(), None)
    }

    /// The tree of `entries` with the stored `modes`, at `location` if it is placed.
    fn listed(entries: Vec<TreeEntry>, modes: Vec<u16>, location: Option<String>) -> Self {
        let mut starts = vec![0u32; entries.len() + 1];
        for parent in entries.iter().filter_map(|entry| entry.parent) {
            starts[parent as usize + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }
        let mut next = starts.clone();
        let mut kids = vec![0u32; entries.len().saturating_sub(1)];
        for entry in &entries {
            if let Some(parent) = entry.parent {
                kids[next[parent as usize] as usize] = entry.index;
                next[parent as usize] += 1;
            }
        }
        Self {
            body: Body::Listed {
                entries,
                starts,
                kids,
                modes,
            },
            location,
        }
    }

    /// The tree of `entries` that a placement shows at the fs location `location`, with the mode
    /// of each entry in `modes`, by index.
    pub(super) fn placed(entries: Vec<TreeEntry>, modes: Vec<u16>, location: String) -> Self {
        Self::listed(entries, modes, Some(location))
    }

    /// The tree a placement shows at the fs location `location` that is the image `image`.
    pub(super) fn image(image: TreeImage, location: String) -> Self {
        Self {
            body: Body::Image(image),
            location: Some(location),
        }
    }

    /// The stored mode of the entry `index` of a placed tree; none for a tree of manifests.
    pub(super) fn mode(&self, index: u32) -> Option<u16> {
        match &self.body {
            Body::Listed { modes, .. } => modes.get(index as usize).copied(),
            Body::Image(image) => image.entry(index).map(|entry| match entry.kind {
                ImageKind::Dir { mode, .. } | ImageKind::File { mode, .. } => mode.bits(),
                ImageKind::Symlink { .. } => Mode::SYMLINK.bits(),
            }),
        }
    }

    /// The fs location a placed tree is shown at; none for a tree of manifests.
    pub(super) fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// How many entries the tree has, the root included.
    pub(super) fn len(&self) -> usize {
        match &self.body {
            Body::Listed { entries, .. } => entries.len(),
            Body::Image(image) => image.len(),
        }
    }

    /// The entry with index `index`.
    pub(super) fn entry(&self, index: u32) -> Option<TreeEntry> {
        let image = match &self.body {
            Body::Listed { entries, .. } => return entries.get(index as usize).cloned(),
            Body::Image(image) => image,
        };
        let entry = image.entry(index)?;
        let kind = match entry.kind {
            ImageKind::Dir { .. } => EntryKind::Dir {
                digest: TreeDigest::from_bytes([0; 32]),
                size: 0,
            },
            ImageKind::File {
                mode,
                size,
                sha256,
                span,
            } => EntryKind::File {
                digest: sha256,
                size,
                exec: mode.bits() & 0o111 != 0,
                source: span.map(|(key, offset)| BlobSource {
                    key: Arc::clone(key),
                    offset,
                }),
            },
            ImageKind::Symlink { target } => EntryKind::Symlink {
                target: Arc::from(target),
            },
        };
        Some(TreeEntry {
            index,
            parent: entry.parent,
            name: Arc::from(entry.name),
            kind,
        })
    }

    /// The indices of the children of the directory `dir`, by name; none for anything else.
    pub(super) fn children(&self, dir: u32) -> Kids<'_> {
        match &self.body {
            Body::Listed { starts, kids, .. } => Kids::Listed(listed(starts, kids, dir)),
            Body::Image(image) => Kids::Range(image.children(dir)),
        }
    }

    /// The index of the child of `dir` called `name`.
    pub(super) fn child(&self, dir: u32, name: &str) -> Option<u32> {
        match &self.body {
            Body::Listed {
                entries,
                starts,
                kids,
                ..
            } => {
                let kids = listed(starts, kids, dir);
                let at = kids
                    .binary_search_by(|&kid| entries[kid as usize].name.as_ref().cmp(name))
                    .ok()?;
                Some(kids[at])
            }
            Body::Image(image) => image.child(dir, name),
        }
    }

    /// The link count of the directory `dir`: 2 and its subdirectories.
    pub(super) fn links(&self, dir: u32) -> u32 {
        let dirs = match &self.body {
            Body::Listed {
                entries,
                starts,
                kids,
                ..
            } => listed(starts, kids, dir)
                .iter()
                .filter(|&&kid| matches!(entries[kid as usize].kind, EntryKind::Dir { .. }))
                .count(),
            Body::Image(image) => image.children(dir).filter(|kid| image.is_dir(*kid)).count(),
        };
        2 + u32::try_from(dirs).unwrap_or(u32::MAX)
    }
}

/// The children of `dir` among the listed `kids`, grouped by the `starts` of their parents.
fn listed<'a>(starts: &[u32], kids: &'a [u32], dir: u32) -> &'a [u32] {
    let (from, to) = (starts[dir as usize], starts[dir as usize + 1]);
    &kids[from as usize..to as usize]
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
