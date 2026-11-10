//! The inode numbers of the mount (invariant I1, `docs/design/virtual-build-fs.md` section 6.3).
//!
//! A 64-bit number is `tree_number << 24 | index`. Tree number 0 is the namespace: index 1 is the
//! mount's root (`FUSE_ROOT_ID`), index 2 is `trees/` and index 3 is `views/`, which this release
//! does not make (index 0 is never used). A tree number from 1 up is given to a tree digest the
//! first time it is attached after the mount was made, and an index is the entry's position in the
//! tree flattened depth first in manifest order, the root being 0. So the number of an entry is a
//! function of the manifests and the table of tree numbers alone.

/// The bits of an index, below the tree number.
const INDEX_BITS: u32 = 24;

/// The inode number of the mount's root.
pub(super) const ROOT: u64 = 1;

/// The inode number of `trees/`.
pub(super) const TREES: u64 = 2;

/// The tree numbers there are room for in the top bits of the inode numbers whose top bit is
/// clear (the ranges of the views are above them).
pub(super) const MOST_TREES: u64 = (1 << 39) - 1;

/// The inode number of entry `index` of tree `tree`.
pub(super) fn of(tree: u64, index: u32) -> u64 {
    (tree << INDEX_BITS) | u64::from(index)
}

/// The tree number and the index an inode number is made of.
pub(super) fn split(ino: u64) -> (u64, u32) {
    // The mask keeps 24 bits: they fit a `u32`.
    (ino >> INDEX_BITS, (ino & ((1 << INDEX_BITS) - 1)) as u32)
}

#[cfg(test)]
#[path = "ino_tests.rs"]
mod tests;
