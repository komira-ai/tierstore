//! [`FileEntry`]: the cells of one pack on Linux (section 2, 5.1, 7, 8).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustix::fs::{FallocateFlags, FlockOperation};
use tierstore_fetch_api::cell::{CellByte, CellState, STATE_MASK, Transition};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EvictStep, HolderCounters,
    PunchReport, ReaderCounters, Settle, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId};
use tierstore_fetch_api::page::{
    CELL_SHIFT, CounterRegion, GROUP_CELLS, StatePageLayout, epoch_of,
};

use crate::journal::{Journal, Op};
use crate::map::SEQ;
use crate::state::StatePage;

/// A holder's region of this entry and the counters it lives in.
type Attached = (CounterRegion, Arc<dyn HolderCounters>);

/// The files of one entry.
#[derive(Debug, Clone)]
pub(crate) struct EntryPaths {
    /// The directory of both, `packs-v2/aa/bb`.
    pub(crate) dir: PathBuf,
    /// The sparse data file, `<hash>.pack`.
    pub(crate) data: PathBuf,
    /// The journal, `<hash>.jnl`.
    pub(crate) journal: PathBuf,
}

/// One cache entry on Linux: a sparse data file held under `flock(LOCK_EX)`, a read-only
/// descriptor of it for mounts, the sealed state memfd whose bytes are the cells, the journal,
/// in-process reader counters, the counter regions of attached holders and the holds.
///
/// The process that opened the entry holds the lock for the entry's life (the lock is released
/// when the last reference drops); another process cannot open the entry, so it reads and evicts
/// nothing of it (7).
#[derive(Debug)]
pub struct FileEntry {
    pack: PackId,
    size: u64,
    layout: StatePageLayout,
    paths: EntryPaths,
    /// Read-write, locked: the store's writes, reads and punches.
    data: File,
    /// Read-only: the descriptor `open-pack` passes, so a mount cannot write the pack.
    data_ro: File,
    state: StatePage,
    journal: Mutex<Journal>,
    /// The readers of this process (a re-hash reading a cell), per 16-cell group.
    readers: Vec<AtomicU32>,
    holders: Mutex<BTreeMap<HolderId, u32>>,
    attached: Mutex<BTreeMap<HolderId, Attached>>,
    /// Cells published since the last commit.
    undurable: Mutex<BTreeSet<u32>>,
    last_use: AtomicU64,
}

/// The lock of a mutex whose data stays consistent across a panic of its holder (every critical
/// section here is a plain update of a map or a file append).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The error of `op` on `pack`'s files.
fn io_err(pack: &PackId, op: &'static str) -> impl FnOnce(io::Error) -> EntryError {
    let pack = pack.clone();
    move |source| EntryError::Io { pack, op, source }
}

impl FileEntry {
    /// Opens the entry of `pack` (`size` bytes) at `paths`: locks the data file, then empties it
    /// and sizes it sparse, empties the journal, and makes the state memfd with every cell
    /// `absent`. Nothing is changed on disk unless the lock is won. A pack of more than
    /// `u32::MAX` cells is [`EntryError::TooLarge`]; a data file another process (or another open
    /// of it) has locked is [`EntryError::Io`] with the op `"flock"`.
    pub(crate) fn open(pack: &PackId, size: u64, paths: EntryPaths) -> Result<Self, EntryError> {
        let layout = StatePageLayout::for_pack(size).ok_or_else(|| EntryError::TooLarge {
            pack: pack.clone(),
            size,
        })?;
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.data)
            .map_err(io_err(pack, "open data file"))?;
        rustix::fs::flock(&data, FlockOperation::NonBlockingLockExclusive)
            .map_err(io::Error::from)
            .map_err(io_err(pack, "flock"))?;
        // Locked: the file is this process's. Nothing on it is trusted (T1b restores from the
        // journal), so its blocks are released and it is the pack's size again, sparse.
        data.set_len(0)
            .map_err(io_err(pack, "truncate data file"))?;
        data.set_len(size).map_err(io_err(pack, "size data file"))?;
        let data_ro = File::open(&paths.data).map_err(io_err(pack, "open data file read-only"))?;
        let journal = Journal::create(&paths.journal).map_err(io_err(pack, "create journal"))?;
        let state = StatePage::create(layout).map_err(io_err(pack, "create state memfd"))?;
        Ok(Self {
            pack: pack.clone(),
            size,
            layout,
            paths,
            data,
            data_ro,
            state,
            journal: Mutex::new(journal),
            readers: (0..layout.group_count())
                .map(|_| AtomicU32::new(0))
                .collect(),
            holders: Mutex::new(BTreeMap::new()),
            attached: Mutex::new(BTreeMap::new()),
            undurable: Mutex::new(BTreeSet::new()),
            last_use: AtomicU64::new(0),
        })
    }

    /// The data file's path.
    #[must_use]
    pub fn data_path(&self) -> &Path {
        &self.paths.data
    }

    /// The journal's path.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.paths.journal
    }

    /// The number of 16-cell groups, the length of a holder's region for this pack.
    #[must_use]
    pub fn group_count(&self) -> u32 {
        self.layout.group_count()
    }

    /// The region `holder` attached, if any.
    pub(crate) fn region_of(&self, holder: HolderId) -> Option<CounterRegion> {
        lock(&self.attached).get(&holder).map(|(r, _)| *r)
    }

    /// Records `holder`'s `region` (the store checked it and mapped it) and takes the holder's
    /// hold.
    pub(crate) fn attach_region(
        &self,
        holder: HolderId,
        region: CounterRegion,
        counters: Arc<dyn HolderCounters>,
    ) {
        lock(&self.attached).insert(holder, (region, counters));
        self.hold(holder);
    }

    /// The lifeline of `holder` hung up: forgets its region and every hold of it. Whether it had
    /// either.
    pub(crate) fn forget_holder(&self, holder: HolderId) -> bool {
        let region = lock(&self.attached).remove(&holder).is_some();
        let held = lock(&self.holders).remove(&holder).is_some();
        region || held
    }

    /// The paths, for the store to unlink them.
    pub(crate) fn paths(&self) -> &EntryPaths {
        &self.paths
    }

    fn slot(&self, cell: u32) -> Result<&AtomicU8, EntryError> {
        self.state
            .cell(cell)
            .ok_or_else(|| EntryError::CellOutOfRange {
                pack: self.pack.clone(),
                cell,
                cells: self.layout.cell_count,
            })
    }

    /// The CAS of `t` on `cell`, from the byte loaded (the `durable` bit included, so a transition
    /// that keeps it keeps it); whether it won. A cell outside the pack never wins.
    fn cas(&self, cell: u32, t: Transition) -> bool {
        self.state.cell(cell).is_some_and(|slot| {
            slot.fetch_update(SEQ, SEQ, |b| t.apply(CellByte(b)).map(|n| n.0))
                .is_ok()
        })
    }

    /// The state of `cell`'s byte, `None` outside the pack or for a byte that is no state.
    fn state_of(&self, cell: u32) -> Option<CellState> {
        self.load(cell).and_then(CellByte::state)
    }

    /// The pack's byte range of `run` (clipped at the pack's end), as `(offset, length)`.
    fn byte_range(&self, run: CellRun) -> (u64, u64) {
        let start = u64::from(run.first) << CELL_SHIFT;
        let end = ((u64::from(run.first) + u64::from(run.count)) << CELL_SHIFT).min(self.size);
        (start, end - start)
    }

    /// The runs of consecutive cells of an ascending list.
    fn runs_of(cells: &[u32]) -> Vec<CellRun> {
        let mut runs: Vec<CellRun> = Vec::new();
        for &c in cells {
            match runs.last_mut() {
                Some(r) if r.first + r.count == c => r.count += 1,
                _ => runs.push(CellRun { first: c, count: 1 }),
            }
        }
        runs
    }

    /// Sets the byte of `cell` with a plain store: a test's way to make the states a restart makes
    /// (`recheck`), which only T1b's replay makes in production.
    #[cfg(test)]
    pub(crate) fn force(&self, cell: u32, byte: CellByte) {
        self.state
            .cell(cell)
            .expect("cell in the pack")
            .store(byte.0, SEQ);
    }
}

impl StateView for FileEntry {
    fn cell_count(&self) -> u32 {
        self.layout.cell_count
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.state.cell(cell).map(|c| CellByte(c.load(SEQ)))
    }
}

impl ReaderCounters for FileEntry {
    fn enter(&self, group: u32) {
        if let Some(c) = self.readers.get(group as usize) {
            c.fetch_add(1, SEQ);
        }
    }

    fn leave(&self, group: u32) {
        if let Some(c) = self.readers.get(group as usize) {
            c.fetch_sub(1, SEQ);
        }
    }
}

impl Entry for FileEntry {
    fn pack(&self) -> &PackId {
        &self.pack
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn state(&self) -> &dyn StateView {
        self
    }

    fn claim(&self, run: CellRun) -> Vec<ClaimedCell> {
        run.cells()
            .filter(|c| self.cas(*c, Transition::Claim))
            .map(ClaimedCell::won)
            .collect()
    }

    fn abandon(&self, cell: ClaimedCell) {
        self.cas(cell.cell(), Transition::Abandon);
    }

    /// Checks the cell is claimed and the length, writes the bytes with `pwrite` while the cell is
    /// still `claimed` (no reader serves a claimed cell), then CAS `claimed -> present`. A wrong
    /// length or a failed write returns the cell to `absent`.
    fn publish(&self, cell: ClaimedCell, bytes: &[u8]) -> Result<(), EntryError> {
        let c = cell.cell();
        if self.state_of(c) != Some(CellState::Claimed) {
            return Err(EntryError::NotClaimed {
                pack: self.pack.clone(),
                cell: c,
            });
        }
        let (offset, len) = self.byte_range(CellRun { first: c, count: 1 });
        let written = if bytes.len() as u64 == len {
            self.data
                .write_all_at(bytes, offset)
                .map_err(io_err(&self.pack, "pwrite"))
        } else {
            Err(EntryError::CellLength {
                pack: self.pack.clone(),
                cell: c,
                expected: len,
                got: bytes.len() as u64,
            })
        };
        if let Err(e) = written {
            self.cas(c, Transition::Abandon);
            return Err(e);
        }
        // The cell was claimed and only its token, consumed here, leaves that state.
        self.cas(c, Transition::Publish);
        lock(&self.undurable).insert(c);
        Ok(())
    }

    fn mark_bad(&self, cell: ClaimedCell) {
        self.cas(cell.cell(), Transition::MarkBad);
    }

    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        self.cas(cell, step.transition())
    }

    /// Checks every cell first; then one `PUNCH` record per non-empty run and one journal sync;
    /// then `fallocate(PUNCH_HOLE | KEEP_SIZE)` per run. The journal lock is held from the check to
    /// the last punch, so two punches of one entry do not interleave their records and holes.
    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        let mut journal = lock(&self.journal);
        for cell in runs.iter().flat_map(|r| r.cells()) {
            let byte = CellByte(self.slot(cell)?.load(SEQ));
            if byte.state() != Some(CellState::Evicting) {
                return Err(EntryError::NotEvicting {
                    pack: self.pack.clone(),
                    cell,
                });
            }
        }
        let records = journal
            .append(Op::Punch, runs)
            .map_err(io_err(&self.pack, "journal PUNCH"))?;
        let mut report = PunchReport {
            journal_syncs: u32::from(records > 0),
            records,
            bytes_punched: 0,
        };
        let hole = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
        for run in runs.iter().filter(|r| r.count > 0) {
            let (offset, len) = self.byte_range(*run);
            rustix::fs::fallocate(&self.data, hole, offset, len)
                .map_err(io::Error::from)
                .map_err(io_err(&self.pack, "punch"))?;
            report.bytes_punched += len;
        }
        Ok(report)
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        self.cas(cell, how.transition())
    }

    fn group_readers(&self, group: u32) -> u64 {
        let in_process = self
            .readers
            .get(group as usize)
            .map_or(0, |c| u64::from(c.load(SEQ)));
        lock(&self.attached)
            .values()
            .filter_map(|(region, counters)| counters.load(region.counter_offset(group)?))
            .fold(in_process, |sum, n| sum + u64::from(n))
    }

    fn counts(&self) -> CellCounts {
        // Indexed by the 3 state bits; 6 and 7 are no state and stay 0.
        let mut by_state = [0u32; 8];
        for cell in 0..self.layout.cell_count {
            let byte = self.state.cell(cell).map_or(0, |c| c.load(SEQ));
            by_state[usize::from(byte & STATE_MASK)] += 1;
        }
        let n = |s: CellState| by_state[usize::from(s.bits())];
        CellCounts {
            absent: n(CellState::Absent),
            claimed: n(CellState::Claimed),
            present: n(CellState::Present),
            recheck: n(CellState::Recheck),
            evicting: n(CellState::Evicting),
            bad: n(CellState::Bad),
        }
    }

    fn last_use(&self) -> u64 {
        self.last_use.load(SEQ)
    }

    fn touch(&self, now_ms: u64, group: u32) {
        let last = self.last_use.fetch_max(now_ms, SEQ).max(now_ms);
        if let Some(epoch) = self.state.epoch(group) {
            epoch.store(epoch_of(last), SEQ);
        }
    }

    fn touch_epoch(&self, group: u32) -> Option<u32> {
        self.state.epoch(group).map(|e| e.load(SEQ))
    }

    /// Reads under the reader protocol of 5.1, so the evictor cannot punch the cell mid-read: the
    /// group's in-process counter is raised, then the cell's state is loaded (`SeqCst` both); only a
    /// `recheck` or `present` cell is read; the counter is lowered after the read.
    fn read_cell(&self, cell: u32) -> Result<Vec<u8>, EntryError> {
        let slot = self.slot(cell)?;
        let group = cell / GROUP_CELLS;
        self.enter(group);
        let byte = CellByte(slot.load(SEQ));
        let read = match byte.state() {
            Some(CellState::Recheck | CellState::Present) => {
                let (offset, len) = self.byte_range(CellRun {
                    first: cell,
                    count: 1,
                });
                // A cell is at most 1 MiB.
                let mut bytes = vec![0u8; len as usize];
                self.data
                    .read_exact_at(&mut bytes, offset)
                    .map(|()| bytes)
                    .map_err(io_err(&self.pack, "pread"))
            }
            _ => Err(EntryError::NotReadable {
                pack: self.pack.clone(),
                cell,
                state: byte,
            }),
        };
        self.leave(group);
        read
    }

    fn fds(&self) -> Option<EntryFds<'_>> {
        Some(EntryFds {
            data: self.data_ro.as_fd(),
            state: self.state.fd(),
        })
    }

    fn hold(&self, holder: HolderId) {
        *lock(&self.holders).entry(holder).or_default() += 1;
    }

    fn release(&self, holder: HolderId) -> bool {
        let mut holders = lock(&self.holders);
        let Some(count) = holders.get_mut(&holder) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            holders.remove(&holder);
        }
        true
    }

    fn holders(&self) -> u32 {
        // One map entry per distinct holder; holder ids are per mount session and Prepare.
        lock(&self.holders).len() as u32
    }

    /// One `fdatasync` of the data file for the cells published since the last commit, then the
    /// `durable` bit on each that is still `present` (a cell evicted meanwhile loses the CAS), then
    /// one `ADD` record per run of them and one journal sync.
    ///
    /// A failed `fdatasync` is returned and its cells are never marked durable, not even by a later
    /// commit: after a failed sync Linux may have dropped the dirty pages and cleared the error, so
    /// a second sync could succeed without the bytes being on disk. A restart then does not restore
    /// them (section 8).
    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        let cells = std::mem::take(&mut *lock(&self.undurable));
        if cells.is_empty() {
            return Ok(CommitReport::default());
        }
        self.data
            .sync_data()
            .map_err(io_err(&self.pack, "fdatasync"))?;
        let durable: Vec<u32> = cells
            .into_iter()
            .filter(|c| self.cas(*c, Transition::Durable))
            .collect();
        let records = lock(&self.journal)
            .append(Op::Add, &Self::runs_of(&durable))
            .map_err(io_err(&self.pack, "journal ADD"))?;
        Ok(CommitReport {
            data_syncs: 1,
            // At most the pack's cell count, a u32.
            cells_durable: durable.len() as u32,
            records,
        })
    }
}
