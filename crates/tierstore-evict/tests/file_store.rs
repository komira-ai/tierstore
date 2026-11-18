//! The evictor over the Linux entry store (T1) on real files: the punch releases the blocks with
//! one journal sync per entry, a whole unheld entry's files are removed, and an entry another
//! store (standing for another process) has open is never evicted by this one.
//!
//! Section 14 row: "No eviction before the grace window; an entry locked by a second process is
//! never evicted".

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use support::*;
use tierstore_entry::FileEntryStore;
use tierstore_evict::{EvictConfig, Evictor, EvictorLock};
use tierstore_fetch_api::cell::CellState::{Absent, Present};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::ids::HolderId;

fn evictor(store: Arc<dyn EntryStore>, cache: &std::path::Path) -> Evictor {
    let clock = ManualClock::new();
    let e = Evictor::new(
        store,
        EvictorLock::acquire(cache).unwrap(),
        Arc::new(clock.clone()),
        ClockPause::new(&clock, 500),
        EvictConfig::new(64 * MIB),
        metrics(),
    );
    clock.advance(EvictConfig::GRACE_MS);
    e
}

#[test]
fn cold_groups_of_a_held_entry_are_punched_on_disk_with_one_journal_sync() {
    let dir = scratch("file-store-punch");
    let store = Arc::new(FileEntryStore::new(dir.path()).unwrap());
    let entry = store.open(&spec(1, 32 * MIB)).unwrap();
    publish(entry.as_ref(), 0..32);
    entry.commit_batch().unwrap();
    entry.hold(HolderId(1));
    entry.touch(1_000, 0);
    entry.touch(9_000, 1);
    let data = store.file_entry(&pack(1)).unwrap().data_path().to_owned();
    let blocks_before = std::fs::metadata(&data).unwrap().blocks();

    let ev = evictor(store.clone(), dir.path());
    let r = ev.evict(16 * MIB).unwrap();
    assert_eq!(r.cells_punched, 16);
    assert_eq!(r.bytes_punched, 16 * MIB);
    assert_eq!(r.journal_syncs, 1);
    assert_eq!(states(entry.as_ref(), 0..16), [Absent; 16]);
    assert_eq!(states(entry.as_ref(), 16..32), [Present; 16]);
    // 16 MiB of 512-byte blocks released.
    let blocks_after = std::fs::metadata(&data).unwrap().blocks();
    assert_eq!(blocks_before - blocks_after, 16 * MIB / 512);
    assert_eq!(entry.read_cell(16).unwrap(), cell_bytes(32 * MIB, 16));
}

#[test]
fn an_entry_open_in_another_store_is_never_evicted_by_this_one() {
    let dir = scratch("file-store-two");
    // Store A (another process) has pack 1; this process's store B has pack 2.
    let a = FileEntryStore::new(dir.path()).unwrap();
    let theirs = a.open(&spec(1, 4 * MIB)).unwrap();
    publish(theirs.as_ref(), 0..4);
    let b = Arc::new(FileEntryStore::new(dir.path()).unwrap());
    assert!(b.open(&spec(1, 4 * MIB)).is_err(), "A's flock keeps B out");
    let ours = b.open(&spec(2, 4 * MIB)).unwrap();
    publish(ours.as_ref(), 0..4);
    let their_data = a.file_entry(&pack(1)).unwrap().data_path().to_owned();
    let our_data = b.file_entry(&pack(2)).unwrap().data_path().to_owned();

    let ev = evictor(b.clone(), dir.path());
    let r = ev.evict(0).unwrap();
    assert_eq!(r.entries_removed, 1);
    assert_eq!(r.used_after, 0);
    assert!(!our_data.exists());
    assert!(their_data.exists());
    assert_eq!(states(theirs.as_ref(), 0..4), [Present; 4]);
    assert_eq!(theirs.read_cell(3).unwrap(), cell_bytes(4 * MIB, 3));
}
