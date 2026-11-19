//! The tests of the in-process store (5.6, T-M1): the `EntryStore` conformance suite on it, its
//! store and entry (layout, `flock`, restart in the same and in another boot, punch and the
//! whole-entry fallback, removal) and its read path (the reader protocol of 5.1 and the read
//! that waits on the engine). They run on Linux in the PR check and on macOS on main.
//!
//! Every test runs in its own scratch directory; no test reads a clock or waits for time (the
//! clock of a touch is a [`ManualClock`] or a number the test passes), and a read in flight is
//! stopped at a parked wait of the fake engine (an explicit progress event), never raced.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{Entry, EntrySpec, EntryStore, HolderCounters, StateView};
use tierstore_fetch_api::fake::MemCounters;
use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_harness::conformance::{EntryStoreFixture, MountCounters};
use tierstore_journal::{BootId, FileKind};
use tierstore_testkit::dir::TestDir;

use crate::inproc::{InProcEntry, InProcStore, punches_inside_a_file};

mod entry_tests;
mod read_tests;
mod store_tests;

pub(crate) const MIB: u64 = 1 << 20;

/// The boot the tests open their stores in, and another.
pub(crate) const BOOT: BootId = BootId([0xb1; 16]);
pub(crate) const OTHER_BOOT: BootId = BootId([0xb2; 16]);

/// A scratch directory for one test.
pub(crate) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

pub(crate) fn pack(s: &str) -> PackId {
    PackId {
        key: PackKey(s.to_owned()),
        root: PackRoot::Digest([3; 32]),
    }
}

pub(crate) fn spec(s: &str, size: u64) -> EntrySpec {
    EntrySpec {
        pack: pack(s),
        size,
    }
}

pub(crate) fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

/// The store of `dir` in boot `boot`.
pub(crate) fn store(dir: &TestDir, boot: Option<BootId>) -> InProcStore {
    InProcStore::new(dir.path(), boot).unwrap()
}

/// Opens pack `name` of `size` bytes in `store`; the concrete entry.
pub(crate) fn open(store: &InProcStore, name: &str, size: u64) -> Arc<InProcEntry> {
    store.open(&spec(name, size)).unwrap();
    store.entry(&pack(name)).unwrap()
}

/// The state of `cell`.
pub(crate) fn state(e: &InProcEntry, cell: u32) -> CellState {
    e.load(cell).unwrap().state().unwrap()
}

/// The bytes a test publishes into `cell` of `e`: `fill`, the cell's length times.
pub(crate) fn cell_bytes(e: &InProcEntry, cell: u32, fill: u8) -> Vec<u8> {
    let start = u64::from(cell) << CELL_SHIFT;
    let len = (start + MIB).min(e.size()) - start;
    vec![fill; len as usize]
}

/// Claims and publishes `cell` with [`cell_bytes`] of `fill`.
pub(crate) fn publish(e: &InProcEntry, cell: u32, fill: u8) {
    let mut won = e.claim(run(cell, 1));
    assert_eq!(won.len(), 1, "cell {cell} was not absent");
    e.publish(won.remove(0), &cell_bytes(e, cell, fill))
        .unwrap();
}

/// The byte a cell has once it is in `s` (not durable).
pub(crate) fn byte(s: CellState) -> CellByte {
    CellByte::new(s, false)
}

/// The length of the entry's journal file.
pub(crate) fn journal_len(e: &InProcEntry) -> u64 {
    std::fs::metadata(e.path(FileKind::Journal)).unwrap().len()
}

/// The mount's side of a [`MemCounters`] (the in-process store maps no memfd: a holder's counters
/// are whatever [`HolderCounters`] the front registers).
struct Mount(Arc<MemCounters>);

impl MountCounters for Mount {
    fn add(&self, offset: u64, delta: u16) {
        self.0.add(offset, delta);
    }

    fn sub(&self, offset: u64, delta: u16) {
        self.0.add(offset, delta.wrapping_neg());
    }
}

/// Fresh in-process stores, each in its own scratch directory, kept so `restore_as_recheck` can
/// reach the concrete entry. With `no_punch` the stores evict as on a volume that cannot punch
/// inside a file (macOS), which the suite's punch case holds to the refusal.
#[derive(Default)]
struct InProcFixture {
    no_punch: bool,
    stores: Mutex<Vec<(TestDir, Arc<InProcStore>)>>,
}

impl InProcFixture {
    /// Stores that cannot punch.
    fn without_punch() -> Self {
        Self {
            no_punch: true,
            ..Self::default()
        }
    }
}

impl EntryStoreFixture for InProcFixture {
    fn store(&self) -> Arc<dyn EntryStore> {
        let dir = scratch("conformance-inproc");
        let mut s = store(&dir, Some(BOOT));
        if self.no_punch {
            s = s.without_punch();
        }
        let s = Arc::new(s);
        self.stores.lock().unwrap().push((dir, s.clone()));
        s
    }

    fn holder(&self, len: u64) -> (Arc<dyn HolderCounters>, Arc<dyn MountCounters>) {
        let counters = Arc::new(MemCounters::new(len));
        (counters.clone(), Arc::new(Mount(counters)))
    }

    /// Writes `bytes` at the cell's offset of the data file, then sets the cell `recheck` (what a
    /// restart in another boot does; the store tests make it by a real restart too).
    fn restore_as_recheck(&self, store: &dyn EntryStore, pack: &PackId, cell: u32, bytes: &[u8]) {
        let stores = self.stores.lock().unwrap();
        let (_, s) = stores
            .iter()
            .find(|(_, s)| std::ptr::addr_eq(Arc::as_ptr(s), store))
            .expect("a store of this fixture");
        let entry = s.entry(pack).expect("an open pack");
        std::fs::OpenOptions::new()
            .write(true)
            .open(entry.path(FileKind::Data))
            .unwrap()
            .write_all_at(bytes, u64::from(cell) << CELL_SHIFT)
            .unwrap();
        entry.force(cell, byte(CellState::Recheck));
    }

    /// What the stores' probe finds (true on a Linux scratch volume, false on macOS), unless they
    /// were made without punch.
    fn punches(&self) -> bool {
        !self.no_punch && punches_inside_a_file(&std::env::temp_dir())
    }
}

/// The suite, each case its own test, on stores as the volume makes them.
mod conformance {
    use super::InProcFixture;

    conformance_cases!(InProcFixture::default());
}

/// The suite on stores that cannot punch: the store of macOS, run on Linux too.
mod conformance_without_punch {
    use super::InProcFixture;

    conformance_cases!(InProcFixture::without_punch());
}
