//! [`FsEntry`]: an `Entry` over a [`MemFs`], journaled.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use tierstore_fetch_api::cell::{CellByte, CellState, STATE_MASK, Transition};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntrySpec, EvictStep,
    PunchReport, ReaderCounters, Settle, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId};
use tierstore_fetch_api::page::{CELL_SHIFT, GROUP_CELLS, epoch_of};

use super::fs::MemFs;
use super::lock;
use crate::boot::BootId;
use crate::cells::CellStates;
use crate::files::{EntryFiles, FileKind};
use crate::journal::{EntryJournal, Recovery};

/// An entry whose cells are atomics in memory and whose files are a [`MemFs`]: `publish`
/// writes the data file, `commit_batch` and `punch` go through its [`EntryJournal`], and
/// [`FsEntry::open`] on the file system a crash left ([`MemFs::crash`]) restores it as a restart
/// would. Readers are in process (it is its own [`ReaderCounters`]); it attaches no holder's
/// counters and has no descriptors.
///
/// It also checks the order of section 8 as it runs: [`FsEntry::early_durable`] counts the
/// cells whose `durable` bit was set while their bytes were not yet synced (always 0 for a
/// journal that keeps the order).
#[derive(Debug)]
pub struct FsEntry {
    pack: PackId,
    size: u64,
    fs: MemFs,
    journal: EntryJournal<MemFs>,
    cells: Vec<AtomicU8>,
    readers: Vec<AtomicU32>,
    epochs: Vec<AtomicU32>,
    last_use: AtomicU64,
    holders: Mutex<BTreeMap<HolderId, u32>>,
    early_durable: AtomicU32,
}

impl FsEntry {
    /// Opens the entry `spec` on `fs` in boot `boot`, as the store does at start or when a pack
    /// is first opened: sets the data file to the pack's size if it is missing or of another
    /// length (a power loss can lose an `ftruncate` that was never synced), syncing the directory
    /// when it created the file, then [`EntryJournal::open`], and starts the cells with what it
    /// restored.
    pub fn open(
        fs: MemFs,
        spec: &EntrySpec,
        boot: Option<BootId>,
    ) -> Result<(Self, Recovery), EntryError> {
        let io = |op, source| EntryError::Io {
            pack: spec.pack.clone(),
            op,
            source,
        };
        let existing = fs.current(FileKind::Data).map(|d| d.len() as u64);
        if existing != Some(spec.size) {
            fs.set_len(FileKind::Data, spec.size)
                .map_err(|e| io("set the data file's length", e))?;
        }
        if existing.is_none() {
            fs.sync_dir()
                .map_err(|e| io("fsync the entry directory", e))?;
        }
        let (journal, recovery) = EntryJournal::open(fs.clone(), spec, boot)?;
        let groups = recovery.cells.len().div_ceil(GROUP_CELLS as usize);
        let entry = Self {
            pack: spec.pack.clone(),
            size: spec.size,
            fs,
            journal,
            cells: recovery.cells.iter().map(|b| AtomicU8::new(b.0)).collect(),
            readers: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            epochs: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            last_use: AtomicU64::new(recovery.last_use),
            holders: Mutex::new(BTreeMap::new()),
            early_durable: AtomicU32::new(0),
        };
        Ok((entry, recovery))
    }

    /// The journal.
    #[must_use]
    pub fn journal(&self) -> &EntryJournal<MemFs> {
        &self.journal
    }

    /// The file system.
    #[must_use]
    pub fn fs(&self) -> &MemFs {
        &self.fs
    }

    /// Closes the entry: folds the journal with the last use (section 8, "at entry close").
    pub fn close(&self) -> Result<(), EntryError> {
        self.journal.fold(Entry::last_use(self))
    }

    /// The cells whose `durable` bit was set before their bytes were synced.
    #[must_use]
    pub fn early_durable(&self) -> u32 {
        self.early_durable.load(Ordering::SeqCst)
    }

    /// The byte range of `cell` (the last cell may be short).
    fn range(&self, cell: u32) -> (u64, u64) {
        let start = u64::from(cell) << CELL_SHIFT;
        let end = (start + (1 << CELL_SHIFT)).min(self.size);
        (start, end - start)
    }

    fn slot(&self, cell: u32) -> Result<&AtomicU8, EntryError> {
        self.cells
            .get(cell as usize)
            .ok_or_else(|| EntryError::CellOutOfRange {
                pack: self.pack.clone(),
                cell,
                cells: self.cell_count(),
            })
    }

    fn cas(&self, cell: u32, t: Transition) -> bool {
        self.cells.get(cell as usize).is_some_and(|slot| {
            slot.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| {
                t.apply(CellByte(b)).map(|next| next.0)
            })
            .is_ok()
        })
    }
}

impl CellStates for FsEntry {
    fn load(&self, cell: u32) -> Option<CellByte> {
        StateView::load(self, cell)
    }

    fn mark_durable(&self, cell: u32) -> bool {
        let (offset, len) = self.range(cell);
        if !self.fs.is_durable(FileKind::Data, offset, len) {
            self.early_durable.fetch_add(1, Ordering::SeqCst);
        }
        self.cas(cell, Transition::Durable)
    }

    fn last_use(&self) -> u64 {
        self.last_use.load(Ordering::SeqCst)
    }
}

impl StateView for FsEntry {
    fn cell_count(&self) -> u32 {
        // Restored from a recovery of at most u32::MAX cells.
        self.cells.len() as u32
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.cells
            .get(cell as usize)
            .map(|c| CellByte(c.load(Ordering::SeqCst)))
    }
}

impl ReaderCounters for FsEntry {
    fn enter(&self, group: u32) {
        if let Some(c) = self.readers.get(group as usize) {
            c.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn leave(&self, group: u32) {
        if let Some(c) = self.readers.get(group as usize) {
            c.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Entry for FsEntry {
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

    fn publish(&self, cell: ClaimedCell, bytes: &[u8]) -> Result<(), EntryError> {
        let c = cell.cell();
        if StateView::load(self, c).and_then(CellByte::state) != Some(CellState::Claimed) {
            return Err(EntryError::NotClaimed {
                pack: self.pack.clone(),
                cell: c,
            });
        }
        let (offset, len) = self.range(c);
        if bytes.len() as u64 != len {
            self.cas(c, Transition::Abandon);
            return Err(EntryError::CellLength {
                pack: self.pack.clone(),
                cell: c,
                expected: len,
                got: bytes.len() as u64,
            });
        }
        if let Err(source) = self.fs.write_at(FileKind::Data, offset, bytes) {
            self.cas(c, Transition::Abandon);
            return Err(EntryError::Io {
                pack: self.pack.clone(),
                op: "pwrite the cell",
                source,
            });
        }
        // Claimed, and only its token (consumed here) leaves that state.
        self.cas(c, Transition::Publish);
        self.journal.published(c);
        Ok(())
    }

    fn mark_bad(&self, cell: ClaimedCell) {
        self.cas(cell.cell(), Transition::MarkBad);
    }

    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        let won = self.cas(cell, step.transition());
        // A cell back to `present` that is not durable must be committed again: a commit that
        // met it `evicting` may have left it out.
        let undurable = StateView::load(self, cell).is_some_and(|b| !b.durable());
        if won && step == EvictStep::Abort && undurable {
            self.journal.published(cell);
        }
        won
    }

    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        self.journal.punch(self, runs)
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        let won = self.cas(cell, how.transition());
        if won {
            self.journal.settled(cell, how);
        }
        won
    }

    fn group_readers(&self, group: u32) -> u64 {
        self.readers
            .get(group as usize)
            .map_or(0, |c| u64::from(c.load(Ordering::SeqCst)))
    }

    fn counts(&self) -> CellCounts {
        let mut by_state = [0u32; 8];
        for cell in &self.cells {
            by_state[usize::from(cell.load(Ordering::SeqCst) & STATE_MASK)] += 1;
        }
        CellCounts {
            absent: by_state[usize::from(CellState::Absent.bits())],
            claimed: by_state[usize::from(CellState::Claimed.bits())],
            present: by_state[usize::from(CellState::Present.bits())],
            recheck: by_state[usize::from(CellState::Recheck.bits())],
            evicting: by_state[usize::from(CellState::Evicting.bits())],
            bad: by_state[usize::from(CellState::Bad.bits())],
        }
    }

    fn last_use(&self) -> u64 {
        self.last_use.load(Ordering::SeqCst)
    }

    fn touch(&self, now_ms: u64, group: u32) {
        let last = self
            .last_use
            .fetch_max(now_ms, Ordering::SeqCst)
            .max(now_ms);
        if let Some(epoch) = self.epochs.get(group as usize) {
            epoch.store(epoch_of(last), Ordering::SeqCst);
        }
    }

    fn touch_epoch(&self, group: u32) -> Option<u32> {
        self.epochs
            .get(group as usize)
            .map(|e| e.load(Ordering::SeqCst))
    }

    fn read_cell(&self, cell: u32) -> Result<Vec<u8>, EntryError> {
        let byte = CellByte(self.slot(cell)?.load(Ordering::SeqCst));
        match byte.state() {
            Some(CellState::Recheck | CellState::Present) => {
                let (offset, len) = self.range(cell);
                self.fs
                    .read_range(FileKind::Data, offset, len)
                    .map_err(|source| EntryError::Io {
                        pack: self.pack.clone(),
                        op: "pread the cell",
                        source,
                    })
            }
            _ => Err(EntryError::NotReadable {
                pack: self.pack.clone(),
                cell,
                state: byte,
            }),
        }
    }

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
        lock(&self.holders).len() as u32
    }

    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        self.journal.commit_batch(self)
    }
}
