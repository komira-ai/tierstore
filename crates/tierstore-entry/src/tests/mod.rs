//! The tests of the Linux entry store, one binary (the crate's unit tests) so every branch is
//! judged by one run.
//!
//! Every test runs in its own scratch directory and creates its own memfds and socket pairs; no
//! test reads a clock or waits for time (the clock of `touch` is a number the test passes), and
//! the one concurrent test asserts an outcome (each cell won once), never an order or a duration.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicU16;

use rustix::fs::{MemfdFlags, SealFlags};
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{Entry, EntrySpec, EntryStore, StateView};
use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_testkit::dir::TestDir;

use crate::map::{Mapping, SEQ, atomic, map_shared};
use crate::{FileEntry, FileEntryStore};

mod conformance_tests;
mod counters_tests;
mod entry_tests;
mod lifeline_tests;
mod state_tests;
mod store_tests;

pub(crate) const MIB: u64 = 1 << 20;

/// A scratch directory for one test.
pub(crate) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

pub(crate) fn pack(s: &str) -> PackId {
    PackId {
        key: PackKey(s.to_owned()),
        root: PackRoot::Digest([2; 32]),
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

/// A store in `dir` and the entry of pack `name` of `size` bytes, opened in it.
pub(crate) fn open(dir: &TestDir, name: &str, size: u64) -> (FileEntryStore, Arc<FileEntry>) {
    let store = FileEntryStore::new(dir.path()).unwrap();
    store.open(&spec(name, size)).unwrap();
    let entry = store.file_entry(&pack(name)).unwrap();
    (store, entry)
}

/// The state of `cell`.
pub(crate) fn state(e: &FileEntry, cell: u32) -> CellState {
    e.load(cell).unwrap().state().unwrap()
}

/// A cell's worth (`len` bytes) of `fill`.
pub(crate) fn bytes(fill: u8, len: u64) -> Vec<u8> {
    vec![fill; len as usize]
}

/// Claims and publishes `cell` with a full cell of `fill`.
pub(crate) fn publish(e: &FileEntry, cell: u32, fill: u8) {
    let mut won = e.claim(run(cell, 1));
    assert_eq!(won.len(), 1, "cell {cell} was not absent");
    e.publish(won.remove(0), &bytes(fill, MIB)).unwrap();
}

/// The byte a cell has once it is in `s` (not durable).
pub(crate) fn byte(s: CellState) -> CellByte {
    CellByte::new(s, false)
}

/// The mount's side of a counter memfd: a memfd it created, sealed against shrinking, and its own
/// read-write mapping of it, which it remaps when it grows the memfd.
pub(crate) struct MountCounters {
    pub(crate) file: Arc<File>,
    map: Mapping,
}

impl MountCounters {
    /// A counter memfd of `len` bytes with `seals` applied.
    pub(crate) fn with_seals(len: u64, seals: SealFlags) -> Self {
        let fd = rustix::fs::memfd_create(
            "test-counters",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        rustix::fs::ftruncate(&fd, len).unwrap();
        rustix::fs::fcntl_add_seals(&fd, seals).unwrap();
        let file = Arc::new(File::from(fd));
        let map = map_shared(&file, len, libc::PROT_READ | libc::PROT_WRITE).unwrap();
        Self { file, map }
    }

    /// A counter memfd of `len` bytes, sealed as a mount seals it (`F_SEAL_SHRINK`).
    pub(crate) fn new(len: u64) -> Self {
        Self::with_seals(len, SealFlags::SHRINK)
    }

    /// Grows the memfd to `len` bytes and remaps the mount's side.
    pub(crate) fn grow(&mut self, len: u64) {
        rustix::fs::ftruncate(&*self.file, len).unwrap();
        self.map = map_shared(&self.file, len, libc::PROT_READ | libc::PROT_WRITE).unwrap();
    }

    /// The counter at byte `offset`, as the mount increments it.
    pub(crate) fn counter(&self, offset: u64) -> &AtomicU16 {
        atomic::<AtomicU16>(&self.map, offset).unwrap()
    }

    /// Raises the counter at `offset` by one (reader step 1).
    pub(crate) fn enter(&self, offset: u64) {
        self.counter(offset).fetch_add(1, SEQ);
    }

    /// Lowers it by one (reader step 5).
    pub(crate) fn leave(&self, offset: u64) {
        self.counter(offset).fetch_sub(1, SEQ);
    }

    /// A descriptor of the memfd, as the handshake passes it.
    pub(crate) fn fd(&self) -> OwnedFd {
        self.file.as_fd().try_clone_to_owned().unwrap()
    }
}
