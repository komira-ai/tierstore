//! [`InProcEntry`]: the cells of one pack in process (5.6 (a)).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};

use rustix::fs::FlockOperation;
use tierstore_fetch_api::cell::{CellByte, CellState, STATE_MASK, Transition};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntrySpec, EvictStep,
    HolderCounters, PunchReport, ReaderCounters, Settle, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId};
use tierstore_fetch_api::page::{
    CELL_SHIFT, CounterRegion, GROUP_CELLS, StatePageLayout, epoch_of,
};
use tierstore_journal::{
    BootId, CellStates, DirFiles, EntryFiles, EntryJournal, FileKind, Recovery,
};

use super::{SEQ, lock};

/// A holder's region of this entry and the counters it lives in.
type Attached = (CounterRegion, Arc<dyn HolderCounters>);

/// The files of an entry, shared by the entry (which writes and reads the data file) and its
/// journal (which orders the syncs).
pub(crate) type Files = Arc<DirFiles>;

/// One cache entry in process: the sparse data file held under `flock(LOCK_EX)`, its journal and
/// snapshot ([`EntryJournal`], shared with the Linux store's design: section 8), the cell bytes
/// and touch epochs as atomics on the heap, a reader counter per 16-cell group, the counter
/// regions of attached holders and the holds.
///
/// The NFS server of macOS runs in the tierstore process, so nothing is passed to another
/// process: no state memfd, no counter memfd, no descriptor ([`Entry::fds`] is `None`). Its reads
/// take this entry's own counters ([`ReaderCounters`]) under the reader protocol of 5.1
/// ([`InProcEntry::read_at`]). Every transition is the same `SeqCst` CAS as the Linux store's.
///
/// The process that opened the entry holds the lock for the entry's life (the lock is released
/// when the last reference drops); another process cannot open the entry, so it reads and evicts
/// nothing of it (7).
#[derive(Debug)]
pub struct InProcEntry {
    pack: PackId,
    size: u64,
    layout: StatePageLayout,
    files: Files,
    journal: EntryJournal<Files>,
    cells: Box<[AtomicU8]>,
    epochs: Box<[AtomicU32]>,
    /// The readers of this process (the NFS server's reads, a re-hash), per 16-cell group.
    readers: Box<[AtomicU32]>,
    last_use: AtomicU64,
    holders: Mutex<BTreeMap<HolderId, u32>>,
    attached: Mutex<BTreeMap<HolderId, Attached>>,
    /// Whether the volume releases a punched run inside the file; when it does not,
    /// [`Entry::punch`] refuses and eviction takes whole entries only (5.6 (b)).
    punch: bool,
}

/// The error of `op` on `pack`'s files.
pub(crate) fn io_err(pack: &PackId, op: &'static str) -> impl FnOnce(io::Error) -> EntryError {
    let pack = pack.clone();
    move |source| EntryError::Io { pack, op, source }
}

impl InProcEntry {
    /// Opens the entry `spec` in the directory `dir`, its files named `<stem>.pack` and so on, in
    /// boot `boot` (`None` when the boot id could not be read): opens the data file (creating
    /// it), locks it, gives it the pack's size if it has another (a power loss can lose an
    /// `ftruncate` that was never synced), syncs the directory when it created the file, then
    /// replays the snapshot and journal ([`EntryJournal::open`]). Cells committed in this boot
    /// come back `present` (and durable), cells of another boot `recheck`, every other `absent`.
    ///
    /// Nothing is changed on disk unless the lock is won: a data file another process (or another
    /// store in this one: `flock` is per open file) has locked is [`EntryError::Io`] with the op
    /// `"flock"`. A pack of more than `u32::MAX` cells is [`EntryError::TooLarge`], before any
    /// file is made.
    pub(crate) fn open(
        spec: &EntrySpec,
        dir: &Path,
        stem: &str,
        boot: Option<BootId>,
        punch: bool,
    ) -> Result<(Self, Recovery), EntryError> {
        let pack = &spec.pack;
        let layout = StatePageLayout::for_pack(spec.size).ok_or_else(|| EntryError::TooLarge {
            pack: pack.clone(),
            size: spec.size,
        })?;
        std::fs::create_dir_all(dir).map_err(io_err(pack, "make entry directory"))?;
        let created = !dir
            .join(format!("{stem}{}", FileKind::Data.suffix()))
            .exists();
        let files = Arc::new(DirFiles::open(dir, stem).map_err(io_err(pack, "open data file"))?);
        rustix::fs::flock(files.data(), FlockOperation::NonBlockingLockExclusive)
            .map_err(io::Error::from)
            .map_err(io_err(pack, "flock"))?;
        let len = files
            .data()
            .metadata()
            .map_err(io_err(pack, "stat data file"))?
            .len();
        if len != spec.size {
            files
                .set_len(FileKind::Data, spec.size)
                .map_err(io_err(pack, "size data file"))?;
        }
        if created {
            files
                .sync_dir()
                .map_err(io_err(pack, "fsync entry directory"))?;
        }
        let (journal, recovery) = EntryJournal::open(files.clone(), spec, boot)?;
        let groups = layout.group_count();
        let entry = Self {
            pack: pack.clone(),
            size: spec.size,
            layout,
            files,
            journal,
            cells: recovery.cells.iter().map(|b| AtomicU8::new(b.0)).collect(),
            epochs: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            readers: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            last_use: AtomicU64::new(recovery.last_use),
            holders: Mutex::new(BTreeMap::new()),
            attached: Mutex::new(BTreeMap::new()),
            punch,
        };
        Ok((entry, recovery))
    }

    /// The path of the entry's `file`.
    #[must_use]
    pub fn path(&self, file: FileKind) -> std::path::PathBuf {
        self.files.path(file)
    }

    /// Whether [`Entry::punch`] releases runs (the volume punches inside a file; 5.6 (b)).
    #[must_use]
    pub fn can_punch(&self) -> bool {
        self.punch
    }

    /// The number of 16-cell groups, the length of a holder's region for this pack.
    pub(crate) fn group_count(&self) -> u32 {
        self.layout.group_count()
    }

    /// Folds the journal into a new snapshot with the last use (section 8: at entry close), so the
    /// next open in this boot replays nothing.
    pub fn close(&self) -> Result<(), EntryError> {
        self.journal.fold(Entry::last_use(self))
    }

    /// The region `holder` attached, if any.
    pub(crate) fn region_of(&self, holder: HolderId) -> Option<CounterRegion> {
        lock(&self.attached).get(&holder).map(|(r, _)| *r)
    }

    /// Records `holder`'s `region` (the store checked it and mapped it) and takes its hold.
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

    /// The data file, for reads.
    pub(crate) fn data(&self) -> &std::fs::File {
        self.files.data()
    }

    pub(crate) fn slot(&self, cell: u32) -> Result<&AtomicU8, EntryError> {
        self.cells
            .get(cell as usize)
            .ok_or_else(|| EntryError::CellOutOfRange {
                pack: self.pack.clone(),
                cell,
                cells: self.layout.cell_count,
            })
    }

    /// The CAS of `t` on `cell`, from the byte loaded (the `durable` bit included); whether it
    /// won. A cell outside the pack never wins.
    fn cas(&self, cell: u32, t: Transition) -> bool {
        self.cells.get(cell as usize).is_some_and(|slot| {
            slot.fetch_update(SEQ, SEQ, |b| t.apply(CellByte(b)).map(|n| n.0))
                .is_ok()
        })
    }

    /// The state of `cell`, `None` outside the pack.
    fn state_of(&self, cell: u32) -> Option<CellState> {
        StateView::load(self, cell).and_then(CellByte::state)
    }

    /// The pack's byte range of `run` (clipped at the pack's end), as `(offset, length)`.
    pub(crate) fn byte_range(&self, run: CellRun) -> (u64, u64) {
        let start = u64::from(run.first) << CELL_SHIFT;
        let end = ((u64::from(run.first) + u64::from(run.count)) << CELL_SHIFT).min(self.size);
        (start, end - start)
    }

    /// Sets the byte of `cell` with a plain store: a test's way to make a state no trait method
    /// makes in place (`recheck`, which a restart in another boot makes).
    #[cfg(test)]
    pub(crate) fn force(&self, cell: u32, byte: CellByte) {
        self.cells[cell as usize].store(byte.0, SEQ);
    }
}

impl CellStates for InProcEntry {
    fn load(&self, cell: u32) -> Option<CellByte> {
        StateView::load(self, cell)
    }

    fn mark_durable(&self, cell: u32) -> bool {
        self.cas(cell, Transition::Durable)
    }

    fn last_use(&self) -> u64 {
        self.last_use.load(SEQ)
    }
}

impl StateView for InProcEntry {
    fn cell_count(&self) -> u32 {
        self.layout.cell_count
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.cells.get(cell as usize).map(|c| CellByte(c.load(SEQ)))
    }
}

impl ReaderCounters for InProcEntry {
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

impl Entry for InProcEntry {
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
    /// still `claimed` (no reader serves a claimed cell), then CAS `claimed -> present` and tells
    /// the journal, so the next commit makes it durable. A wrong length or a failed write returns
    /// the cell to `absent`.
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
            self.files
                .write_at(FileKind::Data, offset, bytes)
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
        // Claimed, and only its token (consumed here) leaves that state.
        self.cas(c, Transition::Publish);
        self.journal.published(c);
        Ok(())
    }

    fn mark_bad(&self, cell: ClaimedCell) {
        self.cas(cell.cell(), Transition::MarkBad);
    }

    /// The CAS of the step. An abort that returns a cell that is not durable to `present` tells
    /// the journal again: a commit that met the cell `evicting` left it pending, and this makes
    /// sure the next commit sees it.
    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        let won = self.cas(cell, step.transition());
        let undurable = StateView::load(self, cell).is_some_and(|b| !b.durable());
        if won && step == EvictStep::Abort && undurable {
            self.journal.published(cell);
        }
        won
    }

    /// On a volume that punches: the journal's batch ([`EntryJournal::punch`]: one `PUNCH`
    /// record per run, one journal sync, then the punches). On one that does not
    /// ([`InProcEntry::can_punch`] is `false`), the cells are checked as the journal checks them
    /// ([`EntryError::CellOutOfRange`], [`EntryError::NotEvicting`]) and then a non-empty batch is
    /// refused with [`EntryError::Io`] (`"punch"`, [`io::ErrorKind::Unsupported`]) before any
    /// effect: the evictor aborts the cells back to `present` and evicts whole entries instead.
    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        if self.punch {
            return self.journal.punch(self, runs);
        }
        for cell in runs.iter().flat_map(|r| r.cells()) {
            if CellByte(self.slot(cell)?.load(SEQ)).state() != Some(CellState::Evicting) {
                return Err(EntryError::NotEvicting {
                    pack: self.pack.clone(),
                    cell,
                });
            }
        }
        if runs.iter().all(|r| r.count == 0) {
            return Ok(PunchReport::default());
        }
        Err(EntryError::Io {
            pack: self.pack.clone(),
            op: "punch",
            source: io::Error::new(
                io::ErrorKind::Unsupported,
                "the volume does not punch inside a file: evict whole entries",
            ),
        })
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        let won = self.cas(cell, how.transition());
        if won {
            self.journal.settled(cell, how);
        }
        won
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
        for cell in &self.cells {
            by_state[usize::from(cell.load(SEQ) & STATE_MASK)] += 1;
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
        if let Some(epoch) = self.epochs.get(group as usize) {
            epoch.store(epoch_of(last), SEQ);
        }
    }

    fn touch_epoch(&self, group: u32) -> Option<u32> {
        self.epochs.get(group as usize).map(|e| e.load(SEQ))
    }

    /// Reads under the reader protocol of 5.1, so the evictor cannot punch the cell mid-read: the
    /// group's counter is raised, then the cell's state is loaded (`SeqCst` both); only a
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
                std::os::unix::fs::FileExt::read_exact_at(self.data(), &mut bytes, offset)
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

    /// `None`: an in-process entry passes no descriptor.
    fn fds(&self) -> Option<EntryFds<'_>> {
        None
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

    /// The journal's commit ([`EntryJournal::commit_batch`]): one `F_FULLFSYNC` (macOS) or
    /// `fdatasync` (Linux) of the data file for the cells published since the last commit, the
    /// `durable` bit on each still `present`, one `ADD` record per run and one journal sync.
    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        self.journal.commit_batch(self)
    }
}
