//! Counter regions and the remap on growth (5.1; section 14 rows "A mount grows its counter memfd
//! past the server's mapping: the server remaps before acknowledging `open-pack`, and the evictor
//! reads the new region's counters" and "More than 128 holders on one pack are all served").
//!
//! Catches: an `open-pack` acknowledged before the server's mapping reaches the region (the
//! evictor would read 0 for a live reader, or past its mapping); a remap that drops the counters of
//! the regions already attached; a remap past the end of the memfd (a later read would fault and
//! kill the server); a memfd that can shrink accepted; a failed remap that still takes the hold or
//! records the region; counters that a cap on holders per pack would refuse; a misaligned or
//! out-of-mapping read that returns a value.

use std::io;
use std::sync::Arc;

use rustix::fs::SealFlags;
use tierstore_fetch_api::entry::{EntryError, EntryStore, HolderCounters};
use tierstore_fetch_api::ids::HolderId;
use tierstore_fetch_api::page::CounterRegion;

use super::{MIB, MountCounters, pack, scratch, spec};
use crate::{CountersError, FileEntryStore, MemfdCounters};

fn region(offset: u64, groups: u32) -> CounterRegion {
    CounterRegion { offset, groups }
}

#[test]
fn a_counter_memfd_that_can_shrink_is_refused() {
    let unsealed = MountCounters::with_seals(4096, SealFlags::empty());
    let err = MemfdCounters::new(unsealed.fd()).unwrap_err();
    assert!(
        matches!(err, CountersError::NotSealed { seals: 0 }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "the counter memfd is not sealed against shrinking (F_SEAL_SHRINK); its seals are 0x0"
    );
    // Sealed against growing only: still refused, the seals named.
    let grow_only = MountCounters::with_seals(4096, SealFlags::GROW);
    let err = MemfdCounters::new(grow_only.fd()).unwrap_err();
    assert!(
        matches!(err, CountersError::NotSealed { seals: 4 }),
        "{err}"
    );
}

#[test]
fn a_descriptor_that_is_not_a_sealable_memfd_is_refused() {
    // A pipe is not shared memory on any host (a regular file would be on a tmpfs /tmp).
    let (reader, _writer) = std::io::pipe().unwrap();
    let err = MemfdCounters::new(reader.into()).unwrap_err();
    assert!(matches!(err, CountersError::Seals { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        format!(
            "reading the seals of the counter memfd: {}",
            io::Error::from_raw_os_error(libc::EINVAL)
        )
    );
}

#[test]
fn counters_are_not_mapped_until_the_first_open_pack() {
    let mount = MountCounters::new(4096);
    let counters = MemfdCounters::new(mount.fd()).unwrap();
    assert_eq!(counters.len(), 0);
    assert!(counters.is_empty());
    assert_eq!(counters.maps(), 0);
    assert_eq!(counters.load(0), None);
    assert_eq!(format!("{counters:?}"), "MemfdCounters { len: 0, maps: 0 }");
    // Ensuring nothing maps nothing.
    counters.ensure_len(0).unwrap();
    assert_eq!(counters.maps(), 0);
}

#[test]
fn attach_maps_the_region_before_it_returns_and_the_evictor_reads_the_mounts_counters() {
    let dir = scratch("counters-attach");
    let store = FileEntryStore::new(dir.path()).unwrap();
    // 40 cells: three groups.
    let entry = store.open(&spec("a", 40 * MIB)).unwrap();
    let mount = MountCounters::new(4096);
    let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store
        .register_holder(HolderId(1), counters.clone())
        .unwrap();
    store
        .attach(&pack("a"), HolderId(1), region(64, 3))
        .unwrap();
    // The mapping reaches the region's end (a page: never past the memfd's 4096 bytes).
    assert_eq!(counters.len(), 4096);
    assert_eq!(counters.maps(), 1);
    assert_eq!(entry.holders(), 1);
    mount.enter(64 + 2 * 2);
    mount.enter(64 + 2 * 2);
    assert_eq!(entry.group_readers(2), 2);
    assert_eq!(entry.group_readers(0), 0);
    // A group outside the pack counts 0 (the region has no counter for it).
    assert_eq!(entry.group_readers(3), 0);
    mount.leave(64 + 2 * 2);
    assert_eq!(entry.group_readers(2), 1);
    assert_eq!(counters.load(64 + 2 * 2), Some(1));
    // A misaligned offset, or one past the mapping, is no counter.
    assert_eq!(counters.load(65), None);
    assert_eq!(counters.load(4096), None);
    assert_eq!(
        format!("{counters:?}"),
        "MemfdCounters { len: 4096, maps: 1 }"
    );
}

#[test]
fn a_mount_that_grows_its_memfd_past_the_mapping_is_remapped_before_open_pack_returns() {
    let dir = scratch("counters-remap");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let a = store.open(&spec("a", 16 * MIB)).unwrap();
    let b = store.open(&spec("b", 64 * MIB)).unwrap();
    let mut mount = MountCounters::new(4096);
    let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store
        .register_holder(HolderId(9), counters.clone())
        .unwrap();
    store.attach(&pack("a"), HolderId(9), region(0, 1)).unwrap();
    mount.enter(0);
    assert_eq!(counters.maps(), 1);
    // The mount opens a second pack and allocates its region past the first page.
    mount.grow(3 * 4096);
    store
        .attach(&pack("b"), HolderId(9), region(2 * 4096 + 16, 4))
        .unwrap();
    assert_eq!(counters.maps(), 2);
    assert_eq!(counters.len(), 3 * 4096);
    // The evictor reads the new region's counter, and the old region's counter survived the remap.
    mount.enter(2 * 4096 + 16 + 2 * 3);
    assert_eq!(b.group_readers(3), 1);
    assert_eq!(a.group_readers(0), 1);
    // A region inside the current mapping is attached without a remap.
    let c = store.open(&spec("c", MIB)).unwrap();
    store.attach(&pack("c"), HolderId(9), region(8, 1)).unwrap();
    assert_eq!(counters.maps(), 2);
    mount.enter(8);
    assert_eq!(c.group_readers(0), 1);
}

#[test]
fn a_remap_never_reaches_past_the_end_of_the_memfd() {
    // 6000 bytes: a region ending at 5000 rounds up to 8192, which would be past the end.
    let mount = MountCounters::new(6000);
    let counters = MemfdCounters::new(mount.fd()).unwrap();
    counters.ensure_len(5000).unwrap();
    assert_eq!(counters.len(), 6000);
    mount.enter(5998);
    assert_eq!(counters.load(5998), Some(1));
    assert_eq!(counters.load(6000), None);
}

#[test]
fn a_region_past_the_end_of_the_memfd_is_refused_with_no_hold_and_no_region() {
    let dir = scratch("counters-short");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let entry = store.open(&spec("a", 16 * MIB)).unwrap();
    let mut mount = MountCounters::new(4096);
    let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store
        .register_holder(HolderId(3), counters.clone())
        .unwrap();
    let err = store
        .attach(&pack("a"), HolderId(3), region(4096, 1))
        .unwrap_err();
    assert!(
        matches!(err, EntryError::CountersMap { len: 4104, .. }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "mapping 4104 bytes of a holder's counters: the counter memfd has 4096 bytes"
    );
    assert_eq!(entry.holders(), 0);
    assert_eq!(counters.maps(), 0);
    // Once the mount has grown the memfd, the same region attaches.
    mount.grow(8192);
    store
        .attach(&pack("a"), HolderId(3), region(4096, 1))
        .unwrap();
    assert_eq!(entry.holders(), 1);
    assert_eq!(counters.len(), 8192);
}

#[test]
fn a_region_at_the_top_of_the_offset_range_is_refused_and_never_overflows() {
    let dir = scratch("counters-top");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let entry = store.open(&spec("a", MIB)).unwrap();
    let mount = MountCounters::new(4096);
    let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store
        .register_holder(HolderId(8), counters.clone())
        .unwrap();
    // Aligned and not overflowing (its end is u64::MAX - 7), so it passes the region check; its
    // end rounded up to a page would overflow.
    let top = region(0xFFFF_FFFF_FFFF_FFF0, 1);
    assert!(top.is_valid());
    let err = store.attach(&pack("a"), HolderId(8), top).unwrap_err();
    assert!(
        matches!(err, EntryError::CountersMap { len, .. } if len == u64::MAX - 7),
        "{err}"
    );
    assert_eq!(entry.holders(), 0);
    assert_eq!(counters.maps(), 0);
}

#[test]
fn more_than_128_holders_on_one_pack_are_all_attached_and_all_counted() {
    let dir = scratch("counters-many");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let entry = store.open(&spec("toolchain", 4096 * MIB)).unwrap();
    let mounts: Vec<MountCounters> = (0..200).map(|_| MountCounters::new(4096)).collect();
    for (i, mount) in mounts.iter().enumerate() {
        let holder = HolderId(i as u64);
        let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
        store.register_holder(holder, counters).unwrap();
        // 4 GiB: 256 groups, a 512-byte region.
        store
            .attach(&pack("toolchain"), holder, region(0, 256))
            .unwrap();
        mount.enter(2 * 255);
    }
    assert_eq!(entry.holders(), 200);
    assert_eq!(entry.group_readers(255), 200);
    assert_eq!(entry.group_readers(0), 0);
}
