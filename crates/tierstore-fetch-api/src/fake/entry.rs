//! An in-memory entry store that follows the CAS rules of section 2.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::lock;
use crate::cell::{CellByte, CellState, STATE_MASK, Transition};
use crate::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntryInfo, EntrySpec,
    EntryStore, EvictStep, HolderCounters, PunchReport, ReaderCounters, Settle, StateView,
};
use crate::ids::{CellRun, HolderId, PackId};
use crate::page::{CELL_SHIFT, CounterRegion, StatePageLayout, epoch_of};

/// A holder's region of this entry and the counters it lives in.
type Attached = (CounterRegion, Arc<dyn HolderCounters>);

/// One entry in memory: atomic cell bytes, the pack's bytes, reader counters per group (it is its
/// own [`ReaderCounters`], as the in-process store of macOS is), the counter regions of attached
/// holders, touch epochs and holds. It has no descriptors ([`Entry::fds`] is `None`).
#[derive(Debug)]
pub struct MemEntry {
    pack: PackId,
    size: u64,
    cells: Vec<AtomicU8>,
    readers: Vec<AtomicU32>,
    data: Mutex<Vec<u8>>,
    holders: Mutex<BTreeMap<HolderId, u32>>,
    attached: Mutex<BTreeMap<HolderId, Attached>>,
    undurable: Mutex<BTreeSet<u32>>,
    last_use: AtomicU64,
    epochs: Vec<AtomicU32>,
}

impl MemEntry {
    /// An entry of `spec` with every cell `absent`, or `None` for a pack of more than `u32::MAX`
    /// cells.
    #[must_use]
    pub fn new(spec: &EntrySpec) -> Option<Self> {
        let layout = StatePageLayout::for_pack(spec.size)?;
        let data = vec![0u8; usize::try_from(spec.size).ok()?];
        Some(Self {
            pack: spec.pack.clone(),
            size: spec.size,
            cells: (0..layout.cell_count).map(|_| AtomicU8::new(0)).collect(),
            readers: (0..layout.group_count())
                .map(|_| AtomicU32::new(0))
                .collect(),
            data: Mutex::new(data),
            holders: Mutex::new(BTreeMap::new()),
            attached: Mutex::new(BTreeMap::new()),
            undurable: Mutex::new(BTreeSet::new()),
            last_use: AtomicU64::new(0),
            epochs: (0..layout.group_count())
                .map(|_| AtomicU32::new(0))
                .collect(),
        })
    }

    /// What a restart in another boot does to a cell (section 8): its `bytes` are on disk and the
    /// cell is `recheck`, whatever it was. A fake-only method, so a test can settle a recheck
    /// cell. The bytes must be the cell's length.
    pub fn restore_as_recheck(&self, cell: u32, bytes: &[u8]) -> Result<(), EntryError> {
        let slot = self.slot(cell)?;
        let (start, end) = self.cell_range(cell);
        if bytes.len() != end - start {
            return Err(EntryError::CellLength {
                pack: self.pack.clone(),
                cell,
                expected: (end - start) as u64,
                got: bytes.len() as u64,
            });
        }
        let mut data = lock(&self.data);
        data[start..end].copy_from_slice(bytes);
        slot.store(CellByte::new(CellState::Recheck, false).0, Ordering::SeqCst);
        lock(&self.undurable).remove(&cell);
        Ok(())
    }

    /// Attaches `holder`'s `region` (already checked by the store): records it and takes the
    /// holder's hold, once; the same region again changes nothing.
    fn attach_region(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
        counters: &Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        let mut attached = lock(&self.attached);
        match attached.get(&holder) {
            Some((known, _)) if *known == region => return Ok(()),
            Some(_) => {
                return Err(EntryError::BadRegion {
                    pack: pack.clone(),
                    region,
                });
            }
            None => {}
        }
        counters.ensure_len(region.end().unwrap_or(0))?;
        attached.insert(holder, (region, counters.clone()));
        self.hold(holder);
        Ok(())
    }

    /// The lifeline of `holder` hung up: drops its region and every hold of it. Whether it had
    /// either.
    fn forget_holder(&self, holder: HolderId) -> bool {
        let region = lock(&self.attached).remove(&holder).is_some();
        let held = lock(&self.holders).remove(&holder).is_some();
        region || held
    }

    fn slot(&self, cell: u32) -> Result<&AtomicU8, EntryError> {
        self.cells
            .get(cell as usize)
            .ok_or_else(|| EntryError::CellOutOfRange {
                pack: self.pack.clone(),
                cell,
                cells: self.state().cell_count(),
            })
    }

    /// The bytes of the pack in `start..end` as the entry holds them (zero where nothing was
    /// published).
    #[must_use]
    pub fn bytes(&self, start: usize, end: usize) -> Vec<u8> {
        lock(&self.data)
            .get(start..end)
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    }

    fn cas(&self, cell: u32, t: Transition) -> bool {
        let Some(slot) = self.cells.get(cell as usize) else {
            return false;
        };
        let mut current = slot.load(Ordering::SeqCst);
        loop {
            let Some(next) = t.apply(CellByte(current)) else {
                return false;
            };
            match slot.compare_exchange(current, next.0, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    fn cell_range(&self, cell: u32) -> (usize, usize) {
        let start = u64::from(cell) << CELL_SHIFT;
        let end = (start + (1 << CELL_SHIFT)).min(self.size);
        // The entry's bytes are in memory, so every offset inside the pack fits a usize.
        (start as usize, end as usize)
    }
}

impl StateView for MemEntry {
    fn cell_count(&self) -> u32 {
        // `new` bounds the count by u32::MAX.
        self.cells.len() as u32
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.cells
            .get(cell as usize)
            .map(|c| CellByte(c.load(Ordering::SeqCst)))
    }
}

impl ReaderCounters for MemEntry {
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

impl Entry for MemEntry {
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
        if self.load(c).and_then(CellByte::state) != Some(CellState::Claimed) {
            return Err(EntryError::NotClaimed {
                pack: self.pack.clone(),
                cell: c,
            });
        }
        let (start, end) = self.cell_range(c);
        if bytes.len() != end - start {
            // The failed publish returns the cell to absent, so it is not left claimed.
            self.cas(c, Transition::Abandon);
            return Err(EntryError::CellLength {
                pack: self.pack.clone(),
                cell: c,
                expected: (end - start) as u64,
                got: bytes.len() as u64,
            });
        }
        lock(&self.data)[start..end].copy_from_slice(bytes);
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

    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        // The bytes are zeroed under the data lock after every cell was seen evicting, and a
        // cell leaves `evicting` only by the caller's CAS, so a claimable cell is never zeroed.
        let mut data = lock(&self.data);
        for cell in runs.iter().flat_map(|r| r.cells()) {
            let byte = self.slot(cell)?.load(Ordering::SeqCst);
            if CellByte(byte).state() != Some(CellState::Evicting) {
                return Err(EntryError::NotEvicting {
                    pack: self.pack.clone(),
                    cell,
                });
            }
        }
        let mut report = PunchReport::default();
        for run in runs.iter().filter(|r| r.count > 0) {
            report.records += 1;
            for cell in run.cells() {
                let (start, end) = self.cell_range(cell);
                data[start..end].fill(0);
                report.bytes_punched += (end - start) as u64;
            }
        }
        report.journal_syncs = u32::from(report.records > 0);
        Ok(report)
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        self.cas(cell, how.transition())
    }

    fn group_readers(&self, group: u32) -> u64 {
        let in_process = self
            .readers
            .get(group as usize)
            .map_or(0, |c| u64::from(c.load(Ordering::SeqCst)));
        lock(&self.attached)
            .values()
            .filter_map(|(region, counters)| counters.load(region.counter_offset(group)?))
            .fold(in_process, |sum, n| sum + u64::from(n))
    }

    fn counts(&self) -> CellCounts {
        // Indexed by the 3 state bits; 6 and 7 are no state and stay 0.
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
        let slot = self.slot(cell)?;
        let (start, end) = self.cell_range(cell);
        let data = lock(&self.data);
        let byte = CellByte(slot.load(Ordering::SeqCst));
        match byte.state() {
            Some(CellState::Recheck | CellState::Present) => Ok(data[start..end].to_vec()),
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
        // One map entry per distinct holder; a test does not make 2^32 of them.
        lock(&self.holders).len() as u32
    }

    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        let cells = std::mem::take(&mut *lock(&self.undurable));
        let durable: Vec<u32> = cells
            .into_iter()
            .filter(|c| self.cas(*c, Transition::Durable))
            .collect();
        let runs = durable
            .iter()
            .zip(durable.iter().skip(1))
            .filter(|(a, b)| **a + 1 != **b)
            .count()
            + usize::from(!durable.is_empty());
        Ok(CommitReport {
            data_syncs: u32::from(!durable.is_empty()),
            cells_durable: durable.len() as u32,
            records: runs as u32,
        })
    }
}

/// An entry store in memory.
#[derive(Debug, Default)]
pub struct MemEntryStore {
    entries: Mutex<BTreeMap<PackId, Arc<MemEntry>>>,
    holders: Mutex<BTreeMap<HolderId, Arc<dyn HolderCounters>>>,
}

impl MemEntryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry of `pack` as its concrete type, for a test reader that takes its counters.
    #[must_use]
    pub fn mem_entry(&self, pack: &PackId) -> Option<Arc<MemEntry>> {
        lock(&self.entries).get(pack).cloned()
    }
}

impl EntryStore for MemEntryStore {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        let mut entries = lock(&self.entries);
        if let Some(e) = entries.get(&spec.pack) {
            if e.size != spec.size {
                return Err(EntryError::SizeMismatch {
                    pack: spec.pack.clone(),
                    size: spec.size,
                    existing: e.size,
                });
            }
            return Ok(e.clone());
        }
        let entry = Arc::new(MemEntry::new(spec).ok_or(EntryError::TooLarge {
            pack: spec.pack.clone(),
            size: spec.size,
        })?);
        entries.insert(spec.pack.clone(), entry.clone());
        Ok(entry)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        lock(&self.entries)
            .get(pack)
            .map(|e| e.clone() as Arc<dyn Entry>)
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        let mut entries = lock(&self.entries);
        match entries.get(pack) {
            Some(e) if e.holders() == 0 => {
                entries.remove(pack);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn packs(&self) -> Vec<PackId> {
        lock(&self.entries).keys().cloned().collect()
    }

    fn entries(&self) -> Vec<EntryInfo> {
        lock(&self.entries)
            .values()
            .map(|e| EntryInfo {
                pack: e.pack.clone(),
                size: e.size,
                last_use: e.last_use(),
                holders: e.holders(),
                counts: e.counts(),
            })
            .collect()
    }

    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        lock(&self.holders).entry(holder).or_insert(counters);
        Ok(())
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        let entry = lock(&self.entries)
            .get(pack)
            .cloned()
            .ok_or_else(|| EntryError::UnknownPack { pack: pack.clone() })?;
        let counters = lock(&self.holders)
            .get(&holder)
            .cloned()
            .ok_or(EntryError::UnknownHolder { holder })?;
        // One counter per 16-cell group of the entry's state page.
        let groups = entry.epochs.len() as u32;
        if !region.is_valid() || region.groups != groups {
            return Err(EntryError::BadRegion {
                pack: pack.clone(),
                region,
            });
        }
        entry.attach_region(pack, holder, region, &counters)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        lock(&self.holders).remove(&holder);
        let released = lock(&self.entries)
            .values()
            .filter(|e| e.forget_holder(holder))
            .count();
        // The store holds one entry per pack, and a test does not open 2^32 of them.
        released as u32
    }
}
