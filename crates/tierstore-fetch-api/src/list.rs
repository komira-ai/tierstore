//! `ListL2`: the reader of the tree list with per-cell digests (section 3). T-FMT fixes the bytes
//! and writes the mmap reader; this module fixes what the rest of the read path asks of it.

use crate::ids::{PackKey, PackRoot};

/// `cell_digest` value of the L2 header that names sha256, the only one this revision writes
/// (3.1). A reader refuses any other value with [`ListVersionUnsupported`].
pub const CELL_DIGEST_SHA256: u8 = 1;

/// One cell's digest: a sha256.
pub type CellDigest = [u8; 32];

/// The bit of `pack_flags` that says the pack is fully covered by spans.
pub const PACK_FULLY_COVERED: u8 = 1;

/// A pack of the list (the 48-byte PACKS record of 3.1, decoded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRecord {
    /// The pack's object key.
    pub key: PackKey,
    /// Its size in bytes.
    pub pack_size: u64,
    /// The index of its first digest in CELLS.
    pub cell_first: u64,
    /// `ceil(pack_size / cell)`.
    pub cell_count: u32,
    /// `log2` of the cell size: 20.
    pub cell_shift: u8,
    /// `pack_flags` ([`PACK_FULLY_COVERED`]).
    pub pack_flags: u8,
    /// 0 for every pack written to the storage tier; a silo-origin multipart pack keeps its
    /// recorded value (4.4).
    pub upload_part_size: u64,
}

/// A node of the tree: an index of the list's node records (inode numbers are record indexes,
/// 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeIndex(pub u64);

/// Why a list is refused: a new reader never parses v1 (3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The object is a v1 list.
    V1List,
    /// The tree has no L2 sidecar.
    MissingSidecar,
    /// The L2 header names a digest function this release does not know.
    UnknownCellDigest(u8),
    /// The L2 header names a version this release does not read.
    UnknownVersion(u32),
}

impl Refusal {
    /// The value of the `reason` label of `list_refused_total`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::V1List => "v1_list",
            Self::MissingSidecar => "missing_sidecar",
            Self::UnknownCellDigest(_) => "unknown_cell_digest",
            Self::UnknownVersion(_) => "unknown_version",
        }
    }
}

/// The distinct status a caller receives for a tree that is not mountable through tierstore
/// (3.5): the CAS service takes its miss path on it and never fails the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("tree list not supported: {}", reason.label())]
pub struct ListVersionUnsupported {
    /// Why.
    pub reason: Refusal,
}

/// A mapped L2 list. Every method indexes into the mapping (zero parse); an index outside the
/// list is `None`.
///
/// Section 14 rows: "v1 list or missing L2 sidecar: refused, `ListVersionUnsupported`, no bytes
/// served, no code path parses v1"; "L2 round trip: each cell digest equals independent sha256;
/// deterministic; sorted directories"; "Two lists with the same pack bytes give one PackId; a
/// changed pack a new path".
pub trait ListL2: Send + Sync {
    /// The number of packs.
    fn pack_count(&self) -> u32;

    /// The record of pack `pack`.
    fn pack_record(&self, pack: u32) -> Option<PackRecord>;

    /// The digest of cell `cell` of pack `pack`: `CELLS[cell_first + cell]`.
    fn cell_digest(&self, pack: u32, cell: u32) -> Option<CellDigest>;

    /// `pack_root` of pack `pack` (3.1), computed when first asked and cached per list.
    fn pack_root(&self, pack: u32) -> Option<PackRoot>;

    /// The child named `name` of directory `dir`, by binary search of its sorted records (5.3).
    fn dir_lookup(&self, dir: NodeIndex, name: &[u8]) -> Option<NodeIndex>;
}
