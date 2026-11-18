//! An in-memory entry store that follows the CAS rules of section 2.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::lock;
use crate::cell::{CellByte, CellState, Transition};
use crate::entry::{
    ClaimedCell, CommitReport, Entry, EntryError, EntrySpec, EntryStore, EvictStep, ReaderCounters,
    Settle, StateView,
};
use crate::ids::{CellRun, HolderId, PackId};
use crate::page::{CELL_SHIFT, StatePageLayout};

/// One entry in memory: atomic cell bytes, the pack's bytes, reader counters per group (it is its
/// own [`ReaderCounters`], as the in-process store of macOS is) and holds.
#[derive(Debug)]
pub struct MemEntry {
    pack: PackId,
    size: u64,
    cells: Vec<AtomicU8>,
    readers: Vec<AtomicU32>,
    data: Mutex<Vec<u8>>,
    holders: Mutex<BTreeMap<HolderId, u32>>,
    undurable: Mutex<BTreeSet<u32>>,
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
            undurable: Mutex::new(BTreeSet::new()),
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
        if step != EvictStep::Finish {
            return self.cas(cell, step.transition());
        }
        // The bytes are zeroed while the cell is still evicting, and the CAS to absent happens
        // under the same lock, so a claim and publish that follow cannot be zeroed.
        let (start, end) = self.cell_range(cell);
        let mut data = lock(&self.data);
        if self.load(cell).and_then(CellByte::state) != Some(CellState::Evicting) {
            return false;
        }
        data[start..end].fill(0);
        self.cas(cell, Transition::FinishEvict)
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        self.cas(cell, how.transition())
    }

    fn group_readers(&self, group: u32) -> u64 {
        self.readers
            .get(group as usize)
            .map_or(0, |c| u64::from(c.load(Ordering::SeqCst)))
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
}
