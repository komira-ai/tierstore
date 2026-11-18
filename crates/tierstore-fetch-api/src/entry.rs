//! `StateView`, `ReaderCounters`, `Entry` and `EntryStore`: a cache entry and its cells (5.1, 5.6,
//! 7, 8).
//!
//! Two implementations follow these traits: the Linux one (T1: a sparse data file, the sealed
//! state memfd, the holders' counter memfds and lifelines, `flock`) and the in-process one of
//! macOS (T-M1: atomics in memory, one process). The same CAS rules and the same evictor ordering
//! hold for both; the loom model of T10a runs against the traits so it covers both.
//!
//! **Blocking.** The methods of [`Entry`] and [`EntryStore`] do blocking I/O on the Linux
//! implementation (`pwrite`, `fdatasync`, `fallocate`, the journal's `fsync`, `ftruncate`, `mmap`).
//! A caller runs them on a dedicated or blocking thread, never on an async worker.

use std::os::fd::BorrowedFd;
use std::sync::Arc;

use crate::cell::{CellByte, Transition};
use crate::ids::{CellRun, HolderId, PackId};
use crate::page::CounterRegion;

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

/// The server's read-only view of one holder's counter memfd (5.1): the `u16` reader counters of
/// every pack the holder has open, at the offsets its regions name. On Linux it is a read-only
/// mapping of the memfd the holder sent at handshake; in process it is the counters themselves.
///
/// Section 14 rows: "A mount grows its counter memfd past the server's mapping: the server remaps
/// before acknowledging `open-pack`, and the evictor reads the new region's counters"; "Mount
/// killed (lifeline hangup) ...".
pub trait HolderCounters: Send + Sync + std::fmt::Debug {
    /// The bytes of the memfd the server has mapped.
    fn len(&self) -> u64;

    /// Remaps the memfd to at least `len` bytes. [`EntryStore::attach`] calls it before it
    /// acknowledges an `open-pack` whose region ends past [`HolderCounters::len`].
    fn ensure_len(&self, len: u64) -> Result<(), EntryError>;

    /// The counter at byte `offset` of the memfd (`SeqCst`), or `None` when the counter is not
    /// inside the mapping.
    fn load(&self, offset: u64) -> Option<u16>;

    /// Whether nothing is mapped.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The descriptors `open-pack` sends for an entry (5.1, 5.4), borrowed from the entry.
#[derive(Debug, Clone, Copy)]
pub struct EntryFds<'a> {
    /// The pack's data file.
    pub data: BorrowedFd<'a>,
    /// The sealed state memfd ([`crate::page::MOUNT_SEALS`]).
    pub state: BorrowedFd<'a>,
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

/// What one [`Entry::punch`] did (section 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PunchReport {
    /// `fsync`s of the journal: 0 (nothing to punch) or 1 for the whole call.
    pub journal_syncs: u32,
    /// `PUNCH` records appended: one per non-empty run.
    pub records: u32,
    /// Bytes of the pack released (the last cell of a pack may be shorter than a cell).
    pub bytes_punched: u64,
}

/// How many cells of an entry are in each state (the `durable` bit is not a state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellCounts {
    /// Cells with no bytes.
    pub absent: u32,
    /// Cells a fetch attempt owns.
    pub claimed: u32,
    /// Verified cells.
    pub present: u32,
    /// Cells to re-hash.
    pub recheck: u32,
    /// Cells the evictor owns.
    pub evicting: u32,
    /// Cells that failed verification twice.
    pub bad: u32,
}

impl CellCounts {
    /// The cells that count as use in the cache's accounting: `present + claimed + recheck`
    /// (section 7).
    #[must_use]
    pub fn used(&self) -> u64 {
        u64::from(self.present) + u64::from(self.claimed) + u64::from(self.recheck)
    }
}

/// What the evictor and the accounting of section 7 read about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    /// The pack.
    pub pack: PackId,
    /// Its size.
    pub size: u64,
    /// The last use, in milliseconds of the injected clock ([`Entry::last_use`]).
    pub last_use: u64,
    /// The number of holders.
    pub holders: u32,
    /// The cells by state.
    pub counts: CellCounts,
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
    /// The cell is not `evicting`, so it must not be punched ([`Entry::punch`]).
    #[error("{pack:?}: cell {cell} is not evicting")]
    NotEvicting {
        /// The entry.
        pack: PackId,
        /// The cell.
        cell: u32,
    },
    /// The cell is neither `recheck` nor `present`, so its bytes are not read ([`Entry::read_cell`]).
    #[error("{pack:?}: cell {cell} is {state:?}, not recheck or present")]
    NotReadable {
        /// The entry.
        pack: PackId,
        /// The cell.
        cell: u32,
        /// Its byte.
        state: CellByte,
    },
    /// No entry is open for the pack ([`EntryStore::attach`]).
    #[error("{pack:?}: no such entry")]
    UnknownPack {
        /// The pack asked for.
        pack: PackId,
    },
    /// The holder never registered, or its lifeline hung up ([`EntryStore::attach`]).
    #[error("holder {holder:?} is not registered")]
    UnknownHolder {
        /// The holder.
        holder: HolderId,
    },
    /// The counter region is misaligned, overflows, does not count the pack's groups, or is not
    /// the region the holder attached for this pack before ([`EntryStore::attach`]).
    #[error("{pack:?}: bad counter region {region:?}")]
    BadRegion {
        /// The entry.
        pack: PackId,
        /// The region the holder named.
        region: CounterRegion,
    },
    /// The server could not map the holder's counter memfd at `len` bytes
    /// ([`HolderCounters::ensure_len`]).
    #[error("mapping {len} bytes of a holder's counters: {source}")]
    CountersMap {
        /// The length asked.
        len: u64,
        /// The error.
        #[source]
        source: std::io::Error,
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
/// an unheld entry's path while a mount holds the data fd"; "`EntryStore` conformance suite";
/// "Crash after a batched PUNCH fsync and before/after the punch: replay never marks a punched cell
/// `present`; one fsync per batch" (`punch`); "Held-only cache: punch eviction admits a new job"
/// (`counts`, `touch`); "Process crash, same boot ... different boot ... `recheck`" (`read_cell`);
/// "Mount killed (lifeline hangup) ..." (`group_readers` sums every attached holder).
///
/// The methods do blocking I/O on the Linux implementation: call them from a dedicated or
/// blocking thread, never from an async worker.
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

    /// One step of the evictor's protocol on one cell; whether the CAS won. The CAS expects the
    /// byte the implementation loaded, `durable` bit and all (see [`Transition::apply`]), not the
    /// bare state constant. `Finish` changes the state only: the bytes were already released by
    /// [`Entry::punch`].
    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool;

    /// Releases the bytes of `runs`, every cell of which the caller has won with
    /// [`EvictStep::Begin`] and holds `evicting` (5.1 step 4, section 8).
    ///
    /// The order is the contract: one `PUNCH` record per non-empty run is appended to the entry's
    /// journal and the journal is `fsync`ed **once** for the whole call; only then is each run
    /// punched (`fallocate(PUNCH_HOLE | KEEP_SIZE)`). `punch` does not change a cell's state: the
    /// caller then CASes each cell `evicting -> absent` with [`EvictStep::Finish`], so a cell is
    /// never claimable while its bytes are still being released.
    ///
    /// If a cell of `runs` is not `evicting`, the result is [`EntryError::NotEvicting`] and
    /// nothing is journaled or punched; a cell outside the pack is
    /// [`EntryError::CellOutOfRange`], also before any effect. No runs (or only empty runs) is a
    /// report of zeros, with no journal sync.
    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError>;

    /// A `recheck` or `bad` cell settled; whether the CAS won.
    fn settle(&self, cell: u32, how: Settle) -> bool;

    /// The sum of every reader counter of `group` (5.1 evictor step 2), `SeqCst`: the entry's
    /// in-process counters (if it has any) and, for every holder attached with
    /// [`EntryStore::attach`], the counter at that group's offset in the holder's region. A
    /// group outside the pack counts 0.
    fn group_readers(&self, group: u32) -> u64;

    /// The cells by state, from one pass of `SeqCst` loads (7). The pass is not a snapshot: a
    /// cell that changes during it is counted in the state the pass saw.
    fn counts(&self) -> CellCounts;

    /// The last use of the entry, in milliseconds of the injected clock; 0 until the first
    /// [`Entry::touch`].
    fn last_use(&self) -> u64;

    /// Records a use at `now_ms` of the 16-cell `group`: `last_use = max(last_use, now_ms)` and
    /// the group's touch epoch (the `u32` of the state page, [`crate::page::epoch_of`]) becomes
    /// the epoch of the new `last_use`, which is `now_ms / 1000` truncated to a `u32` when the
    /// clock does not go back. A group outside the pack changes `last_use` only.
    fn touch(&self, now_ms: u64, group: u32);

    /// The touch epoch of `group` (0 until it is touched), or `None` outside the pack.
    fn touch_epoch(&self, group: u32) -> Option<u32>;

    /// The bytes of `cell` as they are on disk (the cell's length: the last cell of a pack may be
    /// shorter), so a `recheck` cell can be re-hashed (section 8). A `present` cell can be read
    /// too. Any other state is [`EntryError::NotReadable`] with the byte seen; a cell outside the
    /// pack is [`EntryError::CellOutOfRange`].
    fn read_cell(&self, cell: u32) -> Result<Vec<u8>, EntryError>;

    /// The descriptors `open-pack` sends: the data file and the sealed state memfd. `Some` on the
    /// Linux implementation, which T5 passes in `PackOpened` with `SCM_RIGHTS`; `None` in
    /// process (macOS, the fake), which has neither.
    fn fds(&self) -> Option<EntryFds<'_>>;

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
/// a second process is never evicted"; "Layout migration"; "A mount grows its counter memfd past
/// the server's mapping: the server remaps before acknowledging `open-pack`" (`attach`); "Mount
/// killed (lifeline hangup): counters dropped, eviction proceeds; mount whose protocol connection
/// flaps keeps counters, regions and holds" (`register_holder`, `attach`, `drop_holder`).
///
/// The methods do blocking I/O on the Linux implementation (opening files, `mmap`, `ftruncate`):
/// call them from a dedicated or blocking thread, never from an async worker.
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

    /// What the evictor reads about every entry, in pack order.
    fn entries(&self) -> Vec<EntryInfo>;

    /// Registers a holder's counters at handshake. A second registration of a holder id that is
    /// still live keeps the first: a reconnect of the protocol connection does not re-register
    /// (the counter memfd and the lifeline are unchanged), and a holder id is not live again
    /// until [`EntryStore::drop_holder`].
    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError>;

    /// `open-pack`: attaches `holder`'s counter `region` to the entry of `pack` and takes the
    /// holder's hold. The region is checked first: [`CounterRegion::is_valid`] and `groups` equal
    /// to the pack's group count, else [`EntryError::BadRegion`]. Then
    /// [`HolderCounters::ensure_len`] makes the server's mapping reach [`CounterRegion::end`]
    /// **before this returns**, so the evictor never reads past its mapping. Attaching the same
    /// region again is idempotent (no second hold; a reconnect re-sends `open-pack`); another
    /// region for the same `(pack, holder)` is [`EntryError::BadRegion`]. A pack with no entry is
    /// [`EntryError::UnknownPack`], an unregistered holder [`EntryError::UnknownHolder`].
    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError>;

    /// The lifeline of `holder` hung up (`POLLHUP`, and only that: a flapping protocol connection
    /// never calls it): forgets its counters and its regions, and drops its holds on every entry.
    /// The number of entries that were released.
    fn drop_holder(&self, holder: HolderId) -> u32;
}
