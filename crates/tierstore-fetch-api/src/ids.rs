//! Identity of packs and of the parties of a fill (section 2, 4.5).

use tierstore_format::ids::Generation;

/// The object key of a pack (or of an S3-front object) in the storage tier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackKey(pub String);

/// What pins a pack's bytes (section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackRoot {
    /// `pack_root` computed from the list's cell digests (3.1): two lists naming the same bytes
    /// give the same root, and a changed pack cannot alias an old entry.
    Digest([u8; 32]),
    /// An S3-front object with no cell digests (`digest=none`, section 9): its ETag, which for a
    /// stored object is its generation.
    Etag(Generation),
}

/// A pack's identity: one cache entry per `PackId`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackId {
    /// The object key.
    pub key: PackKey,
    /// The root that pins its bytes.
    pub root: PackRoot,
}

/// A holder of packs: a mount session or a Prepare (section 7). A mount keeps its holder id
/// across reconnects of its protocol connection (5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HolderId(pub u64);

/// A job, for deficit round robin across jobs (4.5) and the `x-ts-job` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct JobId(pub u64);

/// The rank of a cell within a job (shared packs first, then tree order; 4.5, 6.1), the
/// `x-ts-rank` header. Lower is sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Rank(pub u32);

/// The class of a fetch (4.5): `demand` > `prepare` > `background`. The same class travels to
/// the drive (`x-ts-class`) so the drive's scheduler applies the same order (10.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// A read that waits for the cells.
    Demand,
    /// Prepare at job assignment.
    Prepare,
    /// Everything else: demoted Prepare parts, recheck sweeps.
    Background,
}

impl Class {
    /// The value of the `x-ts-class` header.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Demand => "demand",
            Self::Prepare => "prepare",
            Self::Background => "background",
        }
    }

    /// The class an `x-ts-class` header names.
    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        match value {
            "demand" => Some(Self::Demand),
            "prepare" => Some(Self::Prepare),
            "background" => Some(Self::Background),
            _ => None,
        }
    }
}

/// A run of cells of one pack: `count` cells from `first`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellRun {
    /// The first cell.
    pub first: u32,
    /// The number of cells.
    pub count: u32,
}

impl CellRun {
    /// The cells of the run, in ascending order (empty when `count` is 0; it stops at
    /// `u32::MAX`).
    pub fn cells(self) -> impl Iterator<Item = u32> {
        (self.first..=u32::MAX).take(self.count as usize)
    }
}

/// A byte range of a pack or object: `len` bytes from `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    /// The first byte.
    pub offset: u64,
    /// The number of bytes.
    pub len: u64,
}

impl ByteRange {
    /// The end of the range (exclusive), or `None` when it overflows.
    #[must_use]
    pub fn end(self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }

    /// The cells of `1 << shift` bytes that cover the range: none for an empty range, one
    /// whose end overflows, one whose cell numbers do not fit a `u32`, and a shift of 64 or more.
    #[must_use]
    pub fn cells(self, shift: u8) -> Option<CellRun> {
        let end = self.end()?;
        if self.len == 0 {
            return None;
        }
        if shift >= 64 {
            return None;
        }
        let first = u32::try_from(self.offset >> shift).ok()?;
        let last = u32::try_from((end - 1) >> shift).ok()?;
        Some(CellRun {
            first,
            count: last - first + 1,
        })
    }
}

/// A tree reference as the mount names it in `open-pack` (the tree id the list is stored under,
/// 3.5).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeRef(pub String);
