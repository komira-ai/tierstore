//! `StateView`, `ReaderCounters`, `Entry` and `EntryStore`: a cache entry and its cells (5.1, 5.6,
//! 7, 8).
//!
//! Two implementations follow these traits: the Linux one (T1: a sparse data file, the sealed
//! state memfd, the holders' counter memfds and lifelines, `flock`) and the in-process one of
//! macOS (T-M1: atomics in memory, one process). The same CAS rules and the same evictor ordering
//! hold for both; the loom model of T10a runs against the traits so it covers both.

use std::sync::Arc;

use crate::cell::{CellByte, Transition};
use crate::ids::{CellRun, HolderId, PackId};

/// A read-only view of one pack's cell bytes: what a mount maps. Every load is `SeqCst` (5.1:
/// the reader and the evictor each store to one location and then load the other's).
///
/// Section 14 rows: "Mount's state mapping is read-only"; "Reader with a forced loss of its
/// counter: post-read recheck discards the bytes"; the loom row of 5.1.
pub trait StateView: Send + Sync {
    /// The number of cells.
    fn cell_count(&self) -> u32;

    /// The byte of `cell`, or `None` outside the pack.
    fn load(&self, cell: u32) -> Option<CellByte>;

    /// Whether every cell of `run` is servable (present), the check of reader steps 2 and 4.
    /// A run that leaves the pack is not ready.
    fn ready(&self, run: CellRun) -> bool {
        run.cells()
            .all(|c| self.load(c).is_some_and(CellByte::servable))
    }
}

/// The reader counters a mount increments before it reads a group and decrements after it
/// commits the reply (5.1 reader steps 1 and 5). On Linux they live in the mount's counter memfd;
/// in process (macOS) in the entry.
///
/// Section 14 rows: the loom row of 5.1; "More than 128 holders on one pack are all served".
pub trait ReaderCounters: Send + Sync {
    /// `fetch_add(counter of group, 1, SeqCst)`.
    fn enter(&self, group: u32);
    /// `fetch_sub(counter of group, 1, SeqCst)`.
    fn leave(&self, group: u32);
}

/// The right to publish one claimed cell. It is neither `Clone` nor `Copy`, and publishing or
/// abandoning it consumes it, so one claim publishes at most once. The constructor
/// ([`ClaimedCell::won`]) is public only so that [`Entry`] implementations in other crates can
/// make tokens; exactly-once rests on an implementation making one only for a won
/// `absent -> claimed` CAS. A caller never constructs one: it gets them from [`Entry::claim`]. A hedge and its original share the token
/// (the fill keeps it in one slot), so whichever first delivers a verifying cell publishes.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a claimed cell must be published or abandoned"]
pub struct ClaimedCell {
    cell: u32,
}

impl ClaimedCell {
    /// The token of a cell an entry has just won with `absent -> claimed`. Call it only for a won
    /// CAS, and only from an [`Entry`] implementation; a caller of an entry never does.
    pub fn won(cell: u32) -> Self {
        Self { cell }
    }

    /// The cell.
    #[must_use]
    pub fn cell(&self) -> u32 {
        self.cell
    }
}

/// A step of the evictor's per-cell protocol (5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvictStep {
    /// `present -> evicting` (step 1).
    Begin,
    /// `evicting -> present` after `evict_wait` (step 3).
    Abort,
    /// `evicting -> absent` after the journaled punch (step 4).
    Finish,
}

impl EvictStep {
    /// The cell transition of the step.
    #[must_use]
    pub const fn transition(self) -> Transition {
        match self {
            Self::Begin => Transition::BeginEvict,
            Self::Abort => Transition::AbortEvict,
            Self::Finish => Transition::FinishEvict,
        }
    }
}

/// The settling of a cell that is neither being fetched nor evicted (section 2 and 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Settle {
    /// `recheck -> present`: the re-hash matched.
    RecheckOk,
    /// `recheck -> absent`.
    RecheckFailed,
    /// `bad -> absent`: the bad cell's 60 s expired.
    ExpireBad,
}

impl Settle {
    /// The cell transition.
    #[must_use]
    pub const fn transition(self) -> Transition {
        match self {
            Self::RecheckOk => Transition::RecheckOk,
            Self::RecheckFailed => Transition::RecheckFailed,
            Self::ExpireBad => Transition::ExpireBad,
        }
    }
}

/// What one commit of the journal did for one entry (section 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitReport {
    /// `fdatasync`s of the data file: 0 or 1 per commit.
    pub data_syncs: u32,
    /// Cells newly marked durable.
    pub cells_durable: u32,
    /// Journal records appended.
    pub records: u32,
}

/// Why an entry operation failed.
#[derive(Debug, thiserror::Error)]
pub enum EntryError {
    /// A cell index outside the pack.
    #[error("{pack:?}: cell {cell} outside a pack of {cells} cells")]
    CellOutOfRange {
        /// The entry.
        pack: PackId,
        /// The cell asked for.
        cell: u32,
        /// The pack's cell count.
        cells: u32,
    },
    /// Bytes of the wrong length for the cell.
    #[error("{pack:?}: cell {cell} needs {expected} bytes, {got} given")]
    CellLength {
        /// The entry.
        pack: PackId,
        /// The cell.
        cell: u32,
        /// The cell's length.
        expected: u64,
        /// The bytes given.
        got: u64,
    },
    /// The cell was not `claimed` when its claim was published (a defect of the caller or of the
    /// implementation: a claim's cell is only ever left by its token).
    #[error("{pack:?}: cell {cell} is not claimed")]
    NotClaimed {
        /// The entry.
        pack: PackId,
        /// The cell.
        cell: u32,
    },
    /// The pack is already open with another size: a `PackId` names one size.
    #[error("{pack:?}: opened with size {size}, the entry has {existing}")]
    SizeMismatch {
        /// The entry.
        pack: PackId,
        /// The size asked.
        size: u64,
        /// The entry's size.
        existing: u64,
    },
    /// The pack is larger than the store can hold as one entry.
    #[error("{pack:?}: a pack of {size} bytes is too large")]
    TooLarge {
        /// The entry.
        pack: PackId,
        /// Its size.
        size: u64,
    },
    /// The data file or the journal failed.
    #[error("{pack:?}: {op}: {source}")]
    Io {
        /// The entry.
        pack: PackId,
        /// What was being done.
        op: &'static str,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

/// One cache entry: the cells of one pack (server side).
///
/// Section 14 rows each implementation passes: "N readers on spans of one pack: one `PackFill`;
/// each cell claimed once"; "Demand sub-range plus overlapping grid part"; "Hedge and original
/// both deliver a cell: one publishes"; the loom row of 5.1; "Live reader stuck beyond
/// `evict_wait`"; "Crash between `pwrite` and journal append"; "Crash after a batched PUNCH
/// fsync"; "Process crash, same boot ... different boot"; "Journal batching"; "Eviction removes
/// an unheld entry's path while a mount holds the data fd"; "`EntryStore` conformance suite".
pub trait Entry: Send + Sync {
    /// The pack.
    fn pack(&self) -> &PackId;

    /// The pack's size in bytes.
    fn size(&self) -> u64;

    /// The cell bytes.
    fn state(&self) -> &dyn StateView;

    /// CAS `absent -> claimed` on each cell of `run`; the cells won, in ascending order. Cells
    /// that are `claimed`, `present` or in any other state are skipped (4.2).
    fn claim(&self, run: CellRun) -> Vec<ClaimedCell>;

    /// `claimed -> absent`: the attempt that owned the cell failed.
    fn abandon(&self, cell: ClaimedCell);

    /// Writes the verified `bytes` of the claimed cell, then CAS `claimed -> present` (3.3:
    /// bytes are verified before they touch the file). A publish that fails because the bytes are
    /// not the cell's length returns the cell to `absent` (the token is consumed, so the cell
    /// must not stay claimed); one that fails with [`EntryError::NotClaimed`] leaves the state as
    /// it is.
    fn publish(&self, cell: ClaimedCell, bytes: &[u8]) -> Result<(), EntryError>;

    /// `claimed -> bad`: the second verification failure of the cell (4.6).
    fn mark_bad(&self, cell: ClaimedCell);

    /// One step of the evictor's protocol on one cell; whether the CAS won.
    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool;

    /// A `recheck` or `bad` cell settled; whether the CAS won.
    fn settle(&self, cell: u32, how: Settle) -> bool;

    /// The sum of every holder's reader counter of `group` (5.1 evictor step 2), `SeqCst`.
    fn group_readers(&self, group: u32) -> u64;

    /// Takes a hold for `holder` (a refcount per holder; 7).
    fn hold(&self, holder: HolderId);

    /// Drops `holder`'s hold; whether it held one.
    fn release(&self, holder: HolderId) -> bool;

    /// The number of holders.
    fn holders(&self) -> u32;

    /// Makes the present cells written since the last commit durable: one `fdatasync` of the
    /// data file, the `durable` bit set on each, one journal record per run (section 8). The
    /// committer calls it once per dirty entry per interval.
    fn commit_batch(&self) -> Result<CommitReport, EntryError>;
}

/// What a store needs to open an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySpec {
    /// The pack.
    pub pack: PackId,
    /// Its size.
    pub size: u64,
}

/// The node cache's entries (section 2, 7, 8).
///
/// Section 14 rows: "`EntryStore` conformance suite passes on the memfd implementation (Linux)
/// and the in-process implementation"; "No eviction before the grace window; an entry locked by
/// a second process is never evicted"; "Layout migration".
pub trait EntryStore: Send + Sync {
    /// The entry of `spec.pack`, created (`absent` cells) if there is none. Opening an existing
    /// entry with another size is an error of the caller: a `PackId` names one size.
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError>;

    /// The entry of `pack`, if there is one.
    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>>;

    /// Removes a whole unheld entry (7: eviction order 1); whether it removed one. A held entry
    /// is not removed.
    fn remove(&self, pack: &PackId) -> Result<bool, EntryError>;

    /// The packs of every entry.
    fn packs(&self) -> Vec<PackId>;
}
