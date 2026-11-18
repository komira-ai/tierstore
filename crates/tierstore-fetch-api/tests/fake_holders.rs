//! Holders, counter regions and the lifeline on `MemEntryStore`, and `MemCounters` (5.1, 7).
//!
//! Proves that `attach` checks the region and remaps the holder's counters before it returns,
//! that `group_readers` sums the in-process counters and every attached holder's counter, that
//! `drop_holder` (the lifeline's `POLLHUP`) releases holds and removes counts, that a holder whose
//! connection flaps keeps its counts, and the behaviour of the fake counters.
//!
//! Catches:
//! - an attach that acknowledges before the counters are mapped to the region's end, that takes a
//!   second hold for the same region, or that accepts a misaligned, overflowing or mis-sized
//!   region, a second region of one holder for one pack, an unknown holder or an unknown pack;
//! - a failed remap that still records the region or the hold;
//! - a `group_readers` that misses a holder, counts a region of another group, or counts a
//!   dropped holder;
//! - a `drop_holder` that leaves a hold or a region, miscounts the entries it released, or that a
//!   re-registration after a flap defeats;
//! - fake counters that read past their mapping, remap without growing, or ignore their limit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::os::fd::AsFd;
use std::sync::Arc;

use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryFds, EntrySpec, EntryStore, HolderCounters, ReaderCounters,
};
use tierstore_fetch_api::fake::{DEFAULT_LIMIT, MemCounters, MemEntryStore};
use tierstore_fetch_api::ids::{HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CounterRegion;

const MIB: u64 = 1 << 20;

fn pack(s: &str) -> PackId {
    PackId {
        key: PackKey(s.to_owned()),
        root: PackRoot::Digest([2; 32]),
    }
}

/// A store with the pack `p` of 40 cells (3 groups) and `p2` of 17 cells (2 groups).
fn store() -> MemEntryStore {
    let s = MemEntryStore::new();
    for (name, cells) in [("p", 40), ("p2", 17)] {
        s.open(&EntrySpec {
            pack: pack(name),
            size: cells * MIB,
        })
        .unwrap();
    }
    s
}

fn region(offset: u64, groups: u32) -> CounterRegion {
    CounterRegion { offset, groups }
}

fn register(s: &MemEntryStore, holder: u64, counters: &Arc<MemCounters>) {
    s.register_holder(HolderId(holder), counters.clone())
        .unwrap();
}

#[test]
fn attach_remaps_the_counters_past_their_mapping_before_it_returns_and_takes_a_hold() {
    let s = store();
    let c = Arc::new(MemCounters::new(0));
    register(&s, 1, &c);
    assert_eq!(c.len(), 0);
    s.attach(&pack("p"), HolderId(1), region(1024, 3)).unwrap();
    // The region ends at 1024 + 8 (6 bytes rounded to 8).
    assert_eq!(c.len(), 1032);
    assert_eq!(c.remaps(), 1);
    assert!(c.load(1024 + 4).is_some());
    assert_eq!(s.get(&pack("p")).unwrap().holders(), 1);
}

#[test]
fn attach_of_a_region_inside_the_mapping_does_not_remap() {
    let s = store();
    let c = Arc::new(MemCounters::new(4096));
    register(&s, 1, &c);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
    assert_eq!(c.remaps(), 0);
    assert_eq!(c.len(), 4096);
}

#[test]
fn group_readers_sum_the_in_process_counters_and_every_attached_holder() {
    let s = store();
    let (c1, c2) = (
        Arc::new(MemCounters::new(64)),
        Arc::new(MemCounters::new(64)),
    );
    register(&s, 1, &c1);
    register(&s, 2, &c2);
    s.attach(&pack("p"), HolderId(1), region(8, 3)).unwrap();
    s.attach(&pack("p"), HolderId(2), region(32, 3)).unwrap();
    let e = s.mem_entry(&pack("p")).unwrap();
    e.enter(1);
    // Group 1 of holder 1 is at 8 + 2, of holder 2 at 32 + 2.
    assert!(c1.set(10, 3));
    assert!(c2.set(34, 5));
    assert!(c2.set(32, 7));
    assert_eq!(e.group_readers(1), 1 + 3 + 5);
    assert_eq!(e.group_readers(0), 7);
    assert_eq!(e.group_readers(2), 0);
    // A group outside the pack counts nothing, attached holders or not.
    assert_eq!(e.group_readers(3), 0);
    // The mount decrements by adding 0xFFFF (wrapping), as `fetch_sub(1)` does.
    assert!(c2.add(34, u16::MAX));
    assert_eq!(e.group_readers(1), 1 + 3 + 4);
}

#[test]
fn a_region_counts_only_the_entry_it_was_attached_to() {
    let s = store();
    let c = Arc::new(MemCounters::new(64));
    register(&s, 1, &c);
    s.attach(&pack("p2"), HolderId(1), region(16, 2)).unwrap();
    assert!(c.set(16, 9));
    assert_eq!(s.mem_entry(&pack("p2")).unwrap().group_readers(0), 9);
    assert_eq!(s.mem_entry(&pack("p")).unwrap().group_readers(0), 0);
}

#[test]
fn drop_holder_after_the_lifeline_hangs_up_releases_its_holds_and_removes_its_counts() {
    let s = store();
    let c = Arc::new(MemCounters::new(64));
    register(&s, 1, &c);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
    s.attach(&pack("p2"), HolderId(1), region(8, 2)).unwrap();
    // A hold taken outside `attach` (a Prepare of the same holder id) is dropped too.
    let p = s.get(&pack("p")).unwrap();
    p.hold(HolderId(1));
    p.hold(HolderId(1));
    assert!(c.set(0, 4));
    assert_eq!(p.group_readers(0), 4);
    assert!(!s.remove(&pack("p")).unwrap());
    assert_eq!(s.drop_holder(HolderId(1)), 2);
    assert_eq!(p.group_readers(0), 0);
    assert_eq!(p.holders(), 0);
    assert!(!p.release(HolderId(1)));
    assert!(s.remove(&pack("p")).unwrap());
    assert!(s.remove(&pack("p2")).unwrap());
    // The holder is gone: it neither attaches nor is dropped twice.
    assert_eq!(s.drop_holder(HolderId(1)), 0);
}

#[test]
fn an_attach_after_the_holder_was_dropped_is_an_unknown_holder() {
    let s = store();
    let c = Arc::new(MemCounters::new(64));
    register(&s, 1, &c);
    s.drop_holder(HolderId(1));
    let err = s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap_err();
    assert!(matches!(err, EntryError::UnknownHolder { holder } if holder == HolderId(1)));
    // It may register again (a new process with the same id): a fresh mapping.
    register(&s, 1, &c);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
}

#[test]
fn drop_holder_counts_the_entries_that_held_it_and_leaves_the_others_alone() {
    let s = store();
    s.open(&EntrySpec {
        pack: pack("p3"),
        size: MIB,
    })
    .unwrap();
    let c = Arc::new(MemCounters::new(64));
    register(&s, 1, &c);
    register(&s, 2, &c);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
    s.attach(&pack("p2"), HolderId(2), region(0, 2)).unwrap();
    s.get(&pack("p3")).unwrap().hold(HolderId(1));
    assert_eq!(s.drop_holder(HolderId(1)), 2);
    assert_eq!(s.get(&pack("p2")).unwrap().holders(), 1);
    assert_eq!(s.drop_holder(HolderId(2)), 1);
}

#[test]
fn a_flapping_connection_keeps_counts_regions_and_the_single_hold() {
    let s = store();
    let first = Arc::new(MemCounters::new(64));
    register(&s, 1, &first);
    s.attach(&pack("p"), HolderId(1), region(8, 3)).unwrap();
    assert!(first.set(8, 6));
    // The reconnect re-handshakes with the same holder id (no `drop_holder`), registers again with
    // the counters it already has, and re-sends open-pack with the same region.
    let again = Arc::new(MemCounters::new(0));
    s.register_holder(HolderId(1), again.clone()).unwrap();
    s.attach(&pack("p"), HolderId(1), region(8, 3)).unwrap();
    let p = s.get(&pack("p")).unwrap();
    assert_eq!(p.group_readers(0), 6);
    assert_eq!(p.holders(), 1);
    // One lifeline hangup drops the single hold: the re-attach took no second one.
    assert_eq!(s.drop_holder(HolderId(1)), 1);
    assert_eq!(p.holders(), 0);
    assert_eq!(again.len(), 0);
}

#[test]
fn a_second_registration_of_a_live_holder_keeps_the_first_counters() {
    let s = store();
    let first = Arc::new(MemCounters::new(64));
    let second = Arc::new(MemCounters::new(64));
    register(&s, 1, &first);
    register(&s, 1, &second);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
    assert!(first.set(0, 2));
    assert!(second.set(0, 9));
    assert_eq!(s.get(&pack("p")).unwrap().group_readers(0), 2);
}

#[test]
fn another_region_for_the_same_pack_and_holder_is_refused_and_changes_nothing() {
    let s = store();
    let c = Arc::new(MemCounters::new(0));
    register(&s, 1, &c);
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
    assert!(c.set(0, 3));
    let err = s
        .attach(&pack("p"), HolderId(1), region(64, 3))
        .unwrap_err();
    assert!(
        matches!(&err, EntryError::BadRegion { pack: p, region: r } if *p == pack("p") && *r == region(64, 3)),
        "{err:?}"
    );
    // The first region is still the one that counts, and no mapping grew for the refused one.
    assert_eq!(s.get(&pack("p")).unwrap().group_readers(0), 3);
    assert_eq!(c.len(), 8);
    assert_eq!(c.remaps(), 1);
}

#[test]
fn a_region_that_is_misaligned_overflowing_or_not_the_packs_group_count_is_refused() {
    let s = store();
    let c = Arc::new(MemCounters::new(0));
    register(&s, 1, &c);
    for bad in [
        region(4, 3),
        region(u64::MAX - 7, 3),
        region(0, 2),
        region(0, 4),
        region(0, 0),
    ] {
        let err = s.attach(&pack("p"), HolderId(1), bad).unwrap_err();
        assert!(
            matches!(&err, EntryError::BadRegion { region: r, .. } if *r == bad),
            "{err:?}"
        );
    }
    assert_eq!(c.len(), 0);
    assert_eq!(s.get(&pack("p")).unwrap().holders(), 0);
}

#[test]
fn attach_to_a_pack_with_no_entry_or_by_a_holder_that_never_registered_is_refused() {
    let s = store();
    let c = Arc::new(MemCounters::new(0));
    register(&s, 1, &c);
    let err = s
        .attach(&pack("nope"), HolderId(1), region(0, 3))
        .unwrap_err();
    assert!(matches!(&err, EntryError::UnknownPack { pack: p } if *p == pack("nope")));
    let err = s.attach(&pack("p"), HolderId(9), region(0, 3)).unwrap_err();
    assert!(matches!(&err, EntryError::UnknownHolder { holder } if *holder == HolderId(9)));
}

#[test]
fn a_remap_that_fails_attaches_nothing() {
    let s = store();
    let c = Arc::new(MemCounters::with_limit(0, 64));
    register(&s, 1, &c);
    let err = s
        .attach(&pack("p"), HolderId(1), region(1024, 3))
        .unwrap_err();
    assert!(matches!(err, EntryError::CountersMap { len: 1032, .. }));
    assert_eq!(s.get(&pack("p")).unwrap().holders(), 0);
    // The same holder can attach a region that fits, so nothing was recorded for the failed one.
    s.attach(&pack("p"), HolderId(1), region(0, 3)).unwrap();
}

#[test]
fn counters_report_their_length_in_bytes_rounded_up_to_a_u16() {
    assert_eq!(MemCounters::new(0).len(), 0);
    assert!(MemCounters::new(0).is_empty());
    assert_eq!(MemCounters::new(5).len(), 6);
    assert!(!MemCounters::new(5).is_empty());
}

#[test]
fn counters_load_what_was_set_and_nothing_past_the_mapping_or_off_a_u16() {
    let c = MemCounters::new(8);
    assert_eq!(c.load(2), Some(0));
    assert!(c.set(2, 41));
    assert!(c.add(2, 1));
    assert_eq!(c.load(2), Some(42));
    assert_eq!(c.load(6), Some(0));
    assert_eq!(c.load(8), None);
    assert_eq!(c.load(3), None);
    assert!(!c.set(8, 1));
    assert!(!c.add(3, 1));
    assert!(!c.set(u64::MAX, 1));
}

#[test]
fn counters_grow_to_the_length_asked_and_count_only_the_growths() {
    let c = MemCounters::new(8);
    c.ensure_len(8).unwrap();
    c.ensure_len(4).unwrap();
    assert_eq!(c.remaps(), 0);
    assert!(c.set(0, 5));
    c.ensure_len(21).unwrap();
    assert_eq!(c.len(), 22);
    assert_eq!(c.remaps(), 1);
    // The counters that were there stay, and the new ones start at 0.
    assert_eq!(c.load(0), Some(5));
    assert_eq!(c.load(20), Some(0));
}

#[test]
fn counters_refuse_to_grow_past_their_limit_and_keep_their_mapping() {
    let c = MemCounters::with_limit(8, 16);
    c.ensure_len(16).unwrap();
    let err = c.ensure_len(17).unwrap_err();
    assert_eq!(
        err.to_string(),
        "mapping 17 bytes of a holder's counters: out of memory"
    );
    assert_eq!(c.len(), 16);
    assert_eq!(c.remaps(), 1);
    assert_eq!(DEFAULT_LIMIT, 1 << 26);
    assert!(MemCounters::new(0).ensure_len(DEFAULT_LIMIT + 1).is_err());
}

#[test]
fn entry_fds_are_two_borrowed_descriptors_that_can_be_copied() {
    let stdin = std::io::stdin();
    let fds = EntryFds {
        data: stdin.as_fd(),
        state: stdin.as_fd(),
    };
    let copy = fds;
    assert!(format!("{copy:?}").contains("data"));
}
