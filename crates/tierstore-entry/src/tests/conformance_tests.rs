//! The `EntryStore` conformance suite of `tierstore-harness` on the Linux store (section 14:
//! "`EntryStore` conformance suite passes on the memfd implementation (Linux) and the in-process
//! implementation"), with real counter memfds: each holder's counters are a memfd sealed
//! against shrinking that the mount side maps read-write and the store maps read-only through
//! [`MemfdCounters`], growing as the cases attach regions past the server's mapping.
//!
//! Catches: any divergence of the Linux store from the contracts the in-memory fake and the
//! in-process store are held to (claim, publish, the evictor's steps, punch and commit reports,
//! counts, touch, holds, attach and its checks, drop_holder, more than 128 holders).

use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{EntryStore, HolderCounters};
use tierstore_fetch_api::ids::PackId;
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_harness::conformance::{EntryStoreFixture, MountCounters as Mount};
use tierstore_testkit::dir::TestDir;

use super::{MountCounters, scratch};
use crate::{FileEntryStore, MemfdCounters};

/// The length of each holder's memfd: the mount has grown it past every region a case attaches
/// (the largest ends near 4 KiB), while the server's mapping starts empty and grows only when
/// `attach` asks for it.
const MEMFD_LEN: u64 = 64 * 1024;

/// The mount's side of one counter memfd.
struct LinuxMount(Mutex<MountCounters>);

impl Mount for LinuxMount {
    fn add(&self, offset: u64, delta: u16) {
        let m = self.0.lock().unwrap();
        m.counter(offset).fetch_add(delta, crate::map::SEQ);
    }

    fn sub(&self, offset: u64, delta: u16) {
        let m = self.0.lock().unwrap();
        m.counter(offset).fetch_sub(delta, crate::map::SEQ);
    }
}

/// Fresh Linux stores, each in its own scratch directory, kept so `restore_as_recheck` can reach
/// the concrete entry.
#[derive(Default)]
struct LinuxFixture {
    stores: Mutex<Vec<(TestDir, Arc<FileEntryStore>)>>,
}

impl EntryStoreFixture for LinuxFixture {
    fn store(&self) -> Arc<dyn EntryStore> {
        let dir = scratch("conformance-linux");
        let store = Arc::new(FileEntryStore::new(dir.path()).unwrap());
        self.stores.lock().unwrap().push((dir, store.clone()));
        store
    }

    fn holder(&self, len: u64) -> (Arc<dyn HolderCounters>, Arc<dyn Mount>) {
        let mount = MountCounters::new(len.max(MEMFD_LEN));
        let counters = MemfdCounters::new(mount.fd()).unwrap();
        (Arc::new(counters), Arc::new(LinuxMount(Mutex::new(mount))))
    }

    /// Writes `bytes` at the cell's offset of the data file, then sets the cell `recheck`.
    fn restore_as_recheck(&self, store: &dyn EntryStore, pack: &PackId, cell: u32, bytes: &[u8]) {
        let stores = self.stores.lock().unwrap();
        let (_, file_store) = stores
            .iter()
            .find(|(_, s)| std::ptr::addr_eq(Arc::as_ptr(s), store))
            .expect("a store of this fixture");
        let entry = file_store.file_entry(pack).expect("an open pack");
        std::fs::OpenOptions::new()
            .write(true)
            .open(entry.data_path())
            .unwrap()
            .write_all_at(bytes, u64::from(cell) << CELL_SHIFT)
            .unwrap();
        entry.force(cell, CellByte::new(CellState::Recheck, false));
    }
}

conformance_cases!(LinuxFixture::default());
