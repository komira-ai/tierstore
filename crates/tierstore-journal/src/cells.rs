//! Sets of cells, the runs a journal record names, and the hook through which the journal reads
//! and marks an entry's cell bytes.

use tierstore_fetch_api::cell::CellByte;
use tierstore_fetch_api::ids::CellRun;

/// What the journal needs of an entry's cell bytes: [`crate::fake::FsEntry`]'s atomics here, and
/// the state page of the Linux store or the atomics of the in-process store when they use it.
///
/// The journal calls it with its own lock held and never while the entry holds a lock of its
/// own, so an implementation only loads and CASes.
pub trait CellStates {
    /// The byte of `cell` (`SeqCst`), or `None` outside the pack.
    fn load(&self, cell: u32) -> Option<CellByte>;

    /// CAS `present -> present + durable` on `cell`, from the byte loaded
    /// ([`tierstore_fetch_api::cell::Transition::Durable`]); whether it won.
    fn mark_durable(&self, cell: u32) -> bool;

    /// The entry's last use, in milliseconds of the injected clock, for the snapshot.
    fn last_use(&self) -> u64;
}

/// A set of the cells of one pack, as a bitmap of one bit per cell, least significant bit of
/// byte 0 first: the form the snapshot stores ([`crate::format::Snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSet {
    cells: u32,
    bits: Vec<u8>,
}

impl CellSet {
    /// The empty set of a pack of `cells` cells.
    #[must_use]
    pub fn new(cells: u32) -> Self {
        Self {
            cells,
            bits: vec![0; Self::bytes_for(cells)],
        }
    }

    /// The bytes of the bitmap of `cells` cells.
    #[must_use]
    pub fn bytes_for(cells: u32) -> usize {
        // A u32 count of bits is at most 2^29 bytes, which fits a usize on every target.
        cells.div_ceil(8) as usize
    }

    /// The set the bitmap `bits` of `cells` cells holds, or `None` when the bitmap is not
    /// [`CellSet::bytes_for`] bytes or sets a bit past the last cell.
    #[must_use]
    pub fn from_bits(cells: u32, bits: &[u8]) -> Option<Self> {
        if bits.len() != Self::bytes_for(cells) {
            return None;
        }
        let set = Self {
            cells,
            bits: bits.to_vec(),
        };
        let spare = (8 - cells % 8) % 8;
        let last_ok = spare == 0 || bits.last().is_none_or(|b| b >> (8 - spare) == 0);
        last_ok.then_some(set)
    }

    /// The bitmap.
    #[must_use]
    pub fn bits(&self) -> &[u8] {
        &self.bits
    }

    /// The pack's cell count.
    #[must_use]
    pub fn cell_count(&self) -> u32 {
        self.cells
    }

    /// Whether `cell` is in the set (a cell outside the pack never is).
    #[must_use]
    pub fn contains(&self, cell: u32) -> bool {
        cell < self.cells && self.bits[(cell / 8) as usize] & (1 << (cell % 8)) != 0
    }

    /// Adds `cell`; a cell outside the pack is ignored.
    pub fn insert(&mut self, cell: u32) {
        if cell < self.cells {
            self.bits[(cell / 8) as usize] |= 1 << (cell % 8);
        }
    }

    /// Removes `cell`.
    pub fn remove(&mut self, cell: u32) {
        if cell < self.cells {
            self.bits[(cell / 8) as usize] &= !(1 << (cell % 8));
        }
    }

    /// The cells of the set, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.cells).filter(|c| self.contains(*c))
    }

    /// The number of cells in the set.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.bits.iter().map(|b| b.count_ones()).sum()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|b| *b == 0)
    }
}

/// The contiguous runs of `cells`, which must be ascending: one run per maximal sequence of
/// consecutive cells (a repeated cell joins its run).
#[must_use]
pub fn runs_of(cells: &[u32]) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    for &c in cells {
        match runs.last_mut() {
            Some(r) if r.first.checked_add(r.count) == Some(c) => r.count += 1,
            Some(r) if c >= r.first && c - r.first < r.count => {}
            _ => runs.push(CellRun { first: c, count: 1 }),
        }
    }
    runs
}
