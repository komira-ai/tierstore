//! What the mount (`tierstore fs`) asks of the tree store, through the fill socket: where a path of
//! the mount is among the fs locations of the placements, the paths of those locations, and the
//! tree of a placement whole, flattened in the order the mount numbers its inodes.
//!
//! The mount numbers the entries of a tree by their place in the tree as it is given, the root
//! being 0 (`server/treefs/ino.rs`), so that the number of an entry is a function of the tree
//! alone and a process that takes the mount over numbers it the same. A tree that is an image is
//! given by its digest ([`TreeStore::mount_tree`]): the mount reads the image from the tree's
//! directory ([`image_path`](super::image_path)) and numbers each entry by its index. A tree of
//! records is given whole, once, as a [`FlatTree`] with the paths of its fs locations (the mount
//! numbers the tree at each location apart), flattened depth first in name order: every record of
//! the tree is read once at each place it is held (a directory a tree holds twice, by one digest,
//! is listed at each place), and each file says where its bytes are, by the pack and the offset
//! that [`TreeStore::fetch_span`] makes readable. A mount of a release before the images asks for
//! the [`FlatTree`] of an image too ([`TreeStore::flat`]): its entries are then in the order of
//! their indices, so that both releases number an image's entries alike.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::TreeStore;
use super::location::{Location, PlacementId};
use super::read::{ReadError, image_span};
use crate::tree_image::ImageKind;
use crate::{RecordChild, TreeDigest, TreeImage, TreeRecord};

/// The most entries a flattened tree may have, its root included: what the 24 bits of the index
/// of an inode number count.
pub(in crate::server) const MAX_FLAT_ENTRIES: usize = 1 << 24;

/// The start of the key of every fs location in the table of locations.
const FS_SCOPE: &str = "fs\0\0";

/// Why a placement was not flattened.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server) enum FlatError {
    /// The placement is not alive, or has no fs location; or its files are damaged.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// The tree flattened has more entries than the mount can number.
    #[error("the tree has more than {MAX_FLAT_ENTRIES} entries")]
    TooLarge,
}

/// A placement's tree, flattened for the mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::server) struct FlatTree {
    /// The fs locations of the placement, in the order of the placement: names joined by `/`.
    pub paths: Vec<String>,
    /// The keys of the packs the files are in: a file names its pack by its place in this list.
    pub packs: Vec<String>,
    /// Every entry: the root first, a directory before what it holds, the children of a directory
    /// in name order.
    pub entries: Vec<FlatEntry>,
}

/// One entry of a [`FlatTree`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::server) struct FlatEntry {
    /// The index of the directory that holds it; none for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// Its name; empty for the root.
    pub name: String,
    /// Its mode, as stored: the permission bits, setuid, setgid and sticky.
    pub mode: u16,
    /// What it is.
    #[serde(flatten)]
    pub kind: FlatKind,
}

/// What an entry of a [`FlatTree`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(in crate::server) enum FlatKind {
    /// A directory.
    Dir,
    /// A regular file.
    File {
        /// Its length.
        size: u64,
        /// The SHA-256 of its bytes, as sixty-four lower-case hex digits.
        sha256: String,
        /// The place in [`FlatTree::packs`] of the pack that holds its bytes; none for an empty
        /// file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack: Option<u32>,
        /// Where its bytes start in the pack.
        #[serde(default)]
        offset: u64,
    },
    /// A symbolic link.
    Symlink {
        /// What it says.
        target: String,
    },
}

/// What the mount reads a placement's tree from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum MountTree {
    /// The tree is the image of this digest, in the tree's directory: read it there.
    Image {
        /// The digest of the tree.
        tree: TreeDigest,
        /// The fs locations of the placement, in the order of the placement.
        paths: Vec<String>,
    },
    /// The tree, flattened.
    Flat(FlatTree),
}

/// Where a path of the mount is among the fs locations of the placements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum MountPlace {
    /// The path is the location of this placement.
    Placement(PlacementId),
    /// The path is not a location, and a location is below it: a directory the mount passes
    /// through.
    Above,
    /// Neither: no location is at or below the path. (A path inside a placement's location is
    /// answered by the mount from the tree it has, and is neither too.)
    Nothing,
}

/// The packs of a tree being flattened, each once, in the order they were first met.
#[derive(Default)]
struct Packs {
    keys: Vec<String>,
    places: HashMap<String, u32>,
}

impl Packs {
    /// The place of the pack `key`, added if it is new.
    fn place(&mut self, key: String) -> u32 {
        if let Some(place) = self.places.get(&key) {
            return *place;
        }
        // At most one pack for each file, and the files are fewer than `MAX_FLAT_ENTRIES`, which
        // fits 32 bits.
        let place = self.keys.len() as u32;
        self.keys.push(key.clone());
        let _ = self.places.insert(key, place);
        place
    }
}

/// The placement id `text` writes: thirty-two lower-case hex digits; anything else is none.
#[must_use]
pub(in crate::server) fn placement_id(text: &str) -> Option<PlacementId> {
    PlacementId::parse(text)
}

impl TreeStore {
    /// Where the path `path` of the mount (names joined by `/`, none at either end) is among the
    /// fs locations of the placements that are alive and not released.
    #[must_use]
    pub(in crate::server) fn mount_place(&self, path: &str) -> MountPlace {
        if let Some(located) = self.locate_fs(path) {
            return if located.path.is_empty() {
                MountPlace::Placement(located.placement)
            } else {
                MountPlace::Nothing
            };
        }
        let key = format!("{FS_SCOPE}{path}/");
        let state = self.shared.lock();
        let now = self.shared.now();
        let mut below = state.table.range(key.clone()..);
        let above = below
            .by_ref()
            .take_while(|(held, _)| held.starts_with(&key))
            .any(|(_, id)| state.placements.get(id).is_some_and(|p| p.locatable(now)));
        if above {
            MountPlace::Above
        } else {
            MountPlace::Nothing
        }
    }

    /// The paths of the fs locations of the placements that are alive and not released, in
    /// order.
    #[must_use]
    pub(in crate::server) fn mount_paths(&self) -> Vec<String> {
        let state = self.shared.lock();
        let now = self.shared.now();
        state
            .table
            .range(FS_SCOPE.to_owned()..)
            .take_while(|(held, _)| held.starts_with(FS_SCOPE))
            .filter(|(_, id)| state.placements.get(*id).is_some_and(|p| p.locatable(now)))
            .filter_map(|(held, _)| held.strip_prefix(FS_SCOPE)?.strip_suffix('/'))
            .map(str::to_owned)
            .collect()
    }

    /// Ends the holds of the run `run` (the mount) on every placement but those in `keep`; how
    /// many ended. A mount that starts says which placements it holds, so the holds of a process
    /// before it that ended without giving them back do not keep placements forever.
    pub(in crate::server) fn holds_live(&self, run: &str, keep: &[PlacementId]) -> usize {
        let mut guard = self.shared.lock();
        let state = &mut *guard;
        let mut ended = 0;
        for placement in state.placements.values_mut() {
            if !keep.contains(&placement.id) && placement.attachments.remove(run) {
                ended += 1;
            }
        }
        let now = self.shared.now();
        let _ = self.shared.purge(state, now);
        ended
    }

    /// What the mount reads the tree of the placement `id` from: the image, by its digest, for a
    /// tree that is one; the tree flattened ([`TreeStore::flat`]) for a tree of records.
    ///
    /// # Errors
    /// As [`TreeStore::flat`].
    pub(in crate::server) fn mount_tree(&self, id: &PlacementId) -> Result<MountTree, FlatError> {
        let paths = self.fs_paths(id)?;
        let snapshot = self.snapshot(id)?;
        if snapshot.image().is_some() {
            return Ok(MountTree::Image {
                tree: snapshot.root,
                paths,
            });
        }
        self.flatten(&snapshot, paths).map(MountTree::Flat)
    }

    /// The paths of the fs locations of the placement `id`, if it is alive and has one.
    fn fs_paths(&self, id: &PlacementId) -> Result<Vec<String>, ReadError> {
        let placed = self.placement(id).ok_or(ReadError::NoPlacement)?;
        let paths: Vec<String> = placed
            .locations
            .into_iter()
            .filter_map(|location| match location {
                Location::Fs { path } => Some(path),
                Location::S3 { .. } => None,
            })
            .collect();
        if paths.is_empty() {
            return Err(ReadError::NoPlacement);
        }
        Ok(paths)
    }

    /// The tree of the placement `id`, flattened for the mount at its fs locations: a tree of
    /// records depth first in name order, an image in the order of its indices.
    ///
    /// # Errors
    /// [`ReadError::NoPlacement`] for a placement that is not alive (one that was released and
    /// that something still holds is alive, and is flattened for its holder) or has no fs location,
    /// [`ReadError::Corrupt`] for a tree whose files are damaged, and [`FlatError::TooLarge`] for
    /// a tree of more entries than [`MAX_FLAT_ENTRIES`].
    pub(in crate::server) fn flat(&self, id: &PlacementId) -> Result<FlatTree, FlatError> {
        let paths = self.fs_paths(id)?;
        let snapshot = self.snapshot(id)?;
        match snapshot.image() {
            Some(image) => Ok(flat_image(image, paths)),
            None => self.flatten(&snapshot, paths),
        }
    }

    /// The tree of records of `snapshot`, flattened depth first in name order.
    fn flatten(
        &self,
        snapshot: &super::read::Snapshot,
        paths: Vec<String>,
    ) -> Result<FlatTree, FlatError> {
        let root = self.record(snapshot, &snapshot.root)?;
        let mut entries = vec![FlatEntry {
            parent: None,
            name: String::new(),
            mode: root.mode().bits(),
            kind: FlatKind::Dir,
        }];
        let mut packs = Packs::default();
        // Depth first: each directory with the place of its next child, and its own index.
        let mut stack: Vec<(TreeRecord, usize, u32)> = vec![(root, 0, 0)];
        while let Some((record, next, dir)) = stack.last_mut() {
            let Some(child) = record.children().get(*next).cloned() else {
                let _ = stack.pop();
                continue;
            };
            *next += 1;
            let parent = Some(*dir);
            if entries.len() >= MAX_FLAT_ENTRIES {
                return Err(FlatError::TooLarge);
            }
            // Below `MAX_FLAT_ENTRIES`, which fits 32 bits.
            let index = entries.len() as u32;
            let entry = match child {
                RecordChild::Dir { name, digest } => {
                    let record = self.record(snapshot, &digest)?;
                    let mode = record.mode().bits();
                    stack.push((record, 0, index));
                    FlatEntry {
                        parent,
                        name,
                        mode,
                        kind: FlatKind::Dir,
                    }
                }
                RecordChild::File {
                    name,
                    mode,
                    size,
                    sha256,
                } => {
                    let span = Self::span(snapshot, &sha256, size)?;
                    let (pack, offset) = span.map_or((None, 0), |span| {
                        (Some(packs.place(span.pack)), span.offset)
                    });
                    FlatEntry {
                        parent,
                        name,
                        mode: mode.bits(),
                        kind: FlatKind::File {
                            size,
                            sha256: sha256.to_string(),
                            pack,
                            offset,
                        },
                    }
                }
                RecordChild::Symlink { name, target } => FlatEntry {
                    parent,
                    name,
                    mode: crate::Mode::SYMLINK.bits(),
                    kind: FlatKind::Symlink { target },
                },
            };
            entries.push(entry);
        }
        Ok(FlatTree {
            paths,
            packs: packs.keys,
            entries,
        })
    }
}

/// The image `image`, flattened in the order of its indices, at the fs locations `paths`.
fn flat_image(image: &TreeImage, paths: Vec<String>) -> FlatTree {
    let mut packs = Packs::default();
    let entries = (0..image.len() as u32)
        .filter_map(|index| image.entry(index))
        .map(|entry| {
            let (mode, kind) = match entry.kind {
                ImageKind::Dir { mode, .. } => (mode.bits(), FlatKind::Dir),
                ImageKind::File {
                    mode,
                    size,
                    sha256,
                    span,
                } => {
                    let span = image_span(span);
                    let (pack, offset) = span.map_or((None, 0), |span| {
                        (Some(packs.place(span.pack)), span.offset)
                    });
                    let kind = FlatKind::File {
                        size,
                        sha256: sha256.to_string(),
                        pack,
                        offset,
                    };
                    (mode.bits(), kind)
                }
                ImageKind::Symlink { target } => (
                    crate::Mode::SYMLINK.bits(),
                    FlatKind::Symlink {
                        target: target.to_owned(),
                    },
                ),
            };
            FlatEntry {
                parent: entry.parent,
                name: entry.name.to_owned(),
                mode,
                kind,
            }
        })
        .collect();
    FlatTree {
        paths,
        packs: packs.keys,
        entries,
    }
}

#[cfg(test)]
#[path = "flat_tests.rs"]
mod tests;
