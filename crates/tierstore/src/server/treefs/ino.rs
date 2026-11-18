//! The inode numbers of the mount (invariant I1, `docs/design/virtual-build-fs.md` section 6.3).
//!
//! A 64-bit number is `tree_number << 24 | index`. Tree number 0 is the namespace: index 1 is the
//! mount's root (`FUSE_ROOT_ID`), index 2 is `trees/`, index 3 is `views/`, which this release
//! does not make, and index 4 is `put/`, the put directory (index 0 is never used). A tree number from 1 up is given to a tree digest the
//! first time it is attached after the mount was made, and an index is the entry's position in the
//! tree flattened depth first in manifest order, the root being 0. So the number of an entry is a
//! function of the manifests and the table of tree numbers alone.
//!
//! A placement shown at an fs location (`core/placed.rs`) has a tree number of its own, given to a
//! key made of its id, and its entries are numbered as the node gives the tree: a tree of records
//! depth first in name order, a version 2 tree list (an image) by each entry's place in the list,
//! which is breadth first. Either way the number is a function of the tree alone. The
//! directories the mount passes through above the locations (`jobs/` of `jobs/1f3a/root`) are
//! numbered by the hash of their path, in the range whose top two bits are `0b10`: the part of the
//! range of the views that the put directory left.

/// The bits of an index, below the tree number.
const INDEX_BITS: u32 = 24;

/// The inode number of the mount's root.
pub(super) const ROOT: u64 = 1;

/// The inode number of `trees/`.
pub(super) const TREES: u64 = 2;

/// The inode number of `put/`.
#[cfg(any(target_os = "linux", test))]
pub(super) const PUT: u64 = 4;

/// The top two bits, set in the inode number of every directory and file below `put/`: a part of
/// the range above the trees that the views would have had, and that no tree number reaches.
const PUT_RANGE: u64 = 0b11 << 62;

/// Set, below [`PUT_RANGE`], in the inode number of a file of the put directory, and clear in that
/// of a directory.
#[cfg(any(target_os = "linux", test))]
const PUT_FILE: u64 = 1 << 61;

/// The bits below [`PUT_FILE`]: a directory's hash, or a file's number.
#[cfg(any(target_os = "linux", test))]
const PUT_LOW: u64 = PUT_FILE - 1;

/// The top two bits of the inode number of a directory above the fs location of a placement.
const PLACED_RANGE: u64 = 0b10 << 62;

/// The bits below the top two: a directory's hash.
const LOW_62: u64 = (1 << 62) - 1;

/// The inode number of the directory above the fs locations whose path hashes to `hash`.
pub(super) fn placed_dir(hash: u64) -> u64 {
    PLACED_RANGE | (hash & LOW_62)
}

/// Whether `ino` is the number of a directory above the fs locations.
pub(super) fn is_placed_dir(ino: u64) -> bool {
    ino & PUT_RANGE == PLACED_RANGE
}

/// The inode number of the put directory's directory whose path hashes to `hash`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn put_dir(hash: u64) -> u64 {
    PUT_RANGE | (hash & PUT_LOW)
}

/// The inode number of the put directory's file numbered `id` in the stage, for the whole of its
/// life: staged, renamed, committed.
#[cfg(any(target_os = "linux", test))]
pub(super) fn put_file(id: u64) -> u64 {
    PUT_RANGE | PUT_FILE | (id & PUT_LOW)
}

/// What an inode number of the put directory is.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PutIno {
    /// `put/` itself.
    Root,
    /// A directory below it, by the hash of its path.
    Dir,
    /// A file, by its number in the stage.
    File(u64),
}

/// What `ino` is in the put directory, or none for an inode number outside it.
#[cfg(any(target_os = "linux", test))]
pub(super) fn put_of(ino: u64) -> Option<PutIno> {
    if ino == PUT {
        Some(PutIno::Root)
    } else if ino & PUT_RANGE != PUT_RANGE {
        None
    } else if ino & PUT_FILE == 0 {
        Some(PutIno::Dir)
    } else {
        Some(PutIno::File(ino & PUT_LOW))
    }
}

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
