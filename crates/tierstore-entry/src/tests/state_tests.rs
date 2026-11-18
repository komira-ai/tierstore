//! The sealed state memfd is read-only for every mount (5.1; section 14 row "Mount's state
//! mapping is read-only: a `PROT_WRITE` mmap of the passed state memfd and a `write(2)` fail").
//!
//! Catches: a state memfd passed without one of the four seals, sealed before the server's own
//! mapping (the server could no longer publish), or that a mount can map writable, write, resize
//! or reseal through the passed descriptor or a reopen of it; a mount's mapping that does not
//! see the header or the server's later CAS and touch; a private copy that leaks into the
//! server's view.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32};

use rustix::fs::SealFlags;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EvictStep, StateView};
use tierstore_fetch_api::page::{MOUNT_SEALS, StatePageLayout, epoch_of, sealed_for_mounts};
use vm_memory::{FileOffset, MmapRegion};

use super::{MIB, open, publish, run, scratch};
use crate::map::{SEQ, atomic, map_shared};

/// The state memfd as a mount receives it: its own descriptor of the memfd.
fn passed(e: &crate::FileEntry) -> Arc<File> {
    let fd: OwnedFd = e.fds().unwrap().state.try_clone_to_owned().unwrap();
    Arc::new(File::from(fd))
}

/// Asserts that a mapping failed with `EPERM` (the mapping's error says the errno's text).
fn assert_eperm(e: &io::Error) {
    assert_eq!(
        e.to_string(),
        io::Error::from_raw_os_error(libc::EPERM).to_string()
    );
}

#[test]
fn the_passed_state_memfd_carries_every_mount_seal() {
    let dir = scratch("state-seals");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    let seals = rustix::fs::fcntl_get_seals(e.fds().unwrap().state).unwrap();
    assert_eq!(seals.bits() & MOUNT_SEALS, MOUNT_SEALS);
    assert!(sealed_for_mounts(seals.bits()));
}

#[test]
fn a_writable_shared_mapping_of_the_passed_state_memfd_fails_with_eperm() {
    let dir = scratch("state-prot-write");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    let file = passed(&e);
    let len = StatePageLayout::for_pack(3 * MIB).unwrap().len();
    let err = map_shared(&file, len, libc::PROT_READ | libc::PROT_WRITE).unwrap_err();
    assert_eperm(&err);
    // A reopen through /proc of the passed descriptor, read-write, opens (seals do not refuse an
    // open) but can neither map it writable nor write it.
    let reopened = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .unwrap();
    assert_eq!(
        reopened.write_all_at(&[2], 64).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    let err = map_shared(&Arc::new(reopened), len, libc::PROT_READ | libc::PROT_WRITE).unwrap_err();
    assert_eperm(&err);
}

#[test]
fn write_resize_and_reseal_of_the_passed_state_memfd_fail_with_eperm() {
    let dir = scratch("state-write");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    let file = passed(&e);
    let len = StatePageLayout::for_pack(3 * MIB).unwrap().len();
    let errno = |r: io::Result<()>| r.unwrap_err().raw_os_error();
    // write(2) at a cell byte: a mount cannot mark a cell present.
    assert_eq!(errno(file.write_all_at(&[2], 64)), Some(libc::EPERM));
    assert_eq!(errno(file.set_len(len - 4096).map(drop)), Some(libc::EPERM));
    assert_eq!(errno(file.set_len(len + 4096).map(drop)), Some(libc::EPERM));
    let reseal = rustix::fs::fcntl_add_seals(&*file, SealFlags::WRITE).map_err(io::Error::from);
    assert_eq!(errno(reseal), Some(libc::EPERM));
    // Nothing a mount tried changed the page.
    assert_eq!(e.load(0).unwrap().state(), Some(CellState::Absent));
    assert_eq!(file.metadata().unwrap().len(), len);
}

#[test]
fn a_read_only_mapping_sees_the_header_and_every_later_cas_and_touch_of_the_server() {
    let dir = scratch("state-live");
    let (_store, e) = open(&dir, "p", 33 * MIB);
    let layout = StatePageLayout::for_pack(33 * MIB).unwrap();
    let file = passed(&e);
    let map = map_shared(&file, layout.len(), libc::PROT_READ).unwrap();
    // The header, read through the mapping, describes the page.
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0).unwrap();
    assert_eq!(StatePageLayout::from_header(&header), Some(layout));
    let cell = |c: u32| {
        atomic::<AtomicU8>(&map, 64 + u64::from(c))
            .unwrap()
            .load(SEQ)
    };
    let epoch = |g: u32| {
        atomic::<AtomicU32>(&map, layout.epochs_offset() + 4 * u64::from(g))
            .unwrap()
            .load(SEQ)
    };
    assert_eq!(cell(32), CellState::Absent.bits());
    publish(&e, 32, 7);
    assert_eq!(cell(32), CellState::Present.bits());
    assert!(e.evict_cell_cas(32, EvictStep::Begin));
    assert_eq!(cell(32), CellState::Evicting.bits());
    e.touch(5_000_250, 2);
    assert_eq!(epoch(2), epoch_of(5_000_250));
    assert_eq!(epoch(0), 0);
    assert_eq!(file.metadata().unwrap().len(), layout.len());
}

#[test]
fn a_private_writable_mapping_changes_only_the_mounts_own_copy() {
    let dir = scratch("state-private");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let len = StatePageLayout::for_pack(2 * MIB).unwrap().len();
    let private: MmapRegion<()> = MmapRegion::build(
        Some(FileOffset::from_arc(passed(&e), 0)),
        len as usize,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE,
    )
    .unwrap();
    atomic::<AtomicU8>(&private, 64)
        .unwrap()
        .store(CellState::Present.bits(), SEQ);
    assert_eq!(e.load(0).unwrap().state(), Some(CellState::Absent));
    assert!(!e.state().ready(run(0, 1)));
    assert_eq!(e.claim(run(0, 1)).len(), 1);
}

#[test]
fn the_page_of_an_empty_pack_is_one_page_with_no_cell() {
    let dir = scratch("state-empty");
    let (_store, e) = open(&dir, "p", 0);
    let file = passed(&e);
    assert_eq!(file.metadata().unwrap().len(), 4096);
    assert_eq!(e.cell_count(), 0);
    assert_eq!(e.load(0), None);
    assert_eq!(e.touch_epoch(0), None);
    assert!(e.fds().unwrap().data.as_fd().try_clone_to_owned().is_ok());
}
