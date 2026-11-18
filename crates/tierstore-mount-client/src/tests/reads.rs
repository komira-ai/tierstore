//! State reads and counter regions (5.1), against the real server over the real entry store: the
//! state memfd the mount maps is the entry's, and the server reads the mount's counters through
//! T1's own mapping of the mount's counter memfd.
//!
//! Proves: a read holds the reader counter of every 16-cell group it covers (one that straddles
//! two groups holds both) from the check of its cells until its guard is dropped, and the
//! server's evictor sees exactly those counts; a read of a cell that is not present gives its
//! counters back at once; the post-read recheck fails once the evictor won a covered cell, and the
//! read is not ready until the eviction is aborted; a `bad` cell is reported for EIO and an absent
//! one is not; an empty read is ready and holds nothing; the bytes read from the passed data fd
//! are the pack's; the mount's counter memfd grows past its first chunk as it opens packs, the
//! server remaps before it acknowledges each `open-pack`, and the counters of the first and of the
//! newest region are both read by the server.

use std::sync::Arc;

use crate::{Attempt, OpenSpec};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::{Entry, EvictStep, ReaderCounters, StateView};
use tierstore_fetch_api::ids::{ByteRange, HolderId, PackId, PackKey, PackRoot, TreeRef};
use tierstore_fetch_api::page::{CounterRegion, PAGE_BYTES};

use crate::tests::common::{FakeList, MIB, PACKS, ServerSide, pack_byte, run, spec};

fn bytes(offset: u64, len: u64) -> ByteRange {
    ByteRange { offset, len }
}

/// The reader counts the server sees for the first three groups of pack `i`.
fn readers(side: &ServerSide, i: usize) -> [u64; 3] {
    let e = side.entry(i);
    [e.group_readers(0), e.group_readers(1), e.group_readers(2)]
}

#[test]
fn a_read_holds_the_counters_of_its_groups_until_its_guard_is_dropped() {
    let side = ServerSide::new("reads-counters");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    // Cells 15 and 16: the last cell of group 0 and the first of group 1.
    let read = bytes(15 * MIB + 7, MIB);

    assert!(matches!(
        pack.begin_read(read),
        Attempt::NotReady { bad: None }
    ));
    assert_eq!(readers(&side, 0), [0, 0, 0]);

    side.publish(0, run(15, 2));
    let Attempt::Ready(guard) = pack.begin_read(read) else {
        panic!("cells 15 and 16 are present");
    };
    assert_eq!(readers(&side, 0), [1, 1, 0]);
    assert_eq!(pack.counters().load(0), Some(1));
    assert_eq!(pack.counters().load(1), Some(1));
    assert_eq!(pack.counters().load(3), None);
    let mut buf = vec![0u8; MIB as usize];
    let n = rustix::io::pread(guard.view().data(), &mut buf, read.offset).unwrap();
    assert_eq!(n, buf.len());
    assert!(
        buf.iter()
            .enumerate()
            .all(|(i, b)| *b == pack_byte(read.offset + i as u64))
    );
    assert!(guard.recheck());
    // A second reader of the same group counts too.
    let Attempt::Ready(second) = pack.begin_read(bytes(16 * MIB, 1)) else {
        panic!("cell 16 is present");
    };
    assert_eq!(readers(&side, 0), [1, 2, 0]);
    drop(guard);
    assert_eq!(readers(&side, 0), [0, 1, 0]);
    drop(second);
    assert_eq!(readers(&side, 0), [0, 0, 0]);
}

#[test]
fn the_post_read_recheck_fails_once_the_evictor_wins_a_cell() {
    let side = ServerSide::new("reads-recheck");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.publish(0, run(3, 2));
    let read = bytes(3 * MIB, 2 * MIB);
    let Attempt::Ready(guard) = pack.begin_read(read) else {
        panic!("cells 3 and 4 are present");
    };
    let entry = side.entry(0);
    // The evictor wins cell 4 (step 1) and then sees this reader's count (step 2).
    assert!(entry.evict_cell_cas(4, EvictStep::Begin));
    assert_eq!(entry.group_readers(0), 1);
    assert!(!guard.recheck());
    drop(guard);
    assert_eq!(entry.group_readers(0), 0);
    assert!(matches!(
        pack.begin_read(read),
        Attempt::NotReady { bad: None }
    ));
    assert_eq!(entry.group_readers(0), 0);
    // The eviction is aborted (a reader stayed past `evict_wait`): the cell is servable again.
    assert!(entry.evict_cell_cas(4, EvictStep::Abort));
    assert!(matches!(pack.begin_read(read), Attempt::Ready(g) if g.recheck()));
}

#[test]
fn a_bad_cell_is_reported_for_eio_and_an_absent_one_is_not() {
    let side = ServerSide::new("reads-bad");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(1)).wait().unwrap();
    side.publish(1, run(1, 1));
    let entry = side.entry(1);
    let mut won = entry.claim(run(2, 1));
    entry.mark_bad(won.pop().unwrap());

    assert!(matches!(
        pack.begin_read(bytes(MIB, 2 * MIB)),
        Attempt::NotReady { bad: Some(2) }
    ));
    // The first cell that is not present is absent: no EIO.
    assert!(matches!(
        pack.begin_read(bytes(0, 3 * MIB)),
        Attempt::NotReady { bad: None }
    ));
    // The short last cell, absent.
    assert!(matches!(
        pack.begin_read(bytes(3 * MIB, 5)),
        Attempt::NotReady { bad: None }
    ));
    // An empty read holds nothing, and its recheck passes.
    let Attempt::Ready(empty) = pack.begin_read(bytes(PACKS[1].1, 10)) else {
        panic!("an empty read is ready");
    };
    assert!(empty.recheck());
    assert_eq!(entry.group_readers(0), 0);
    drop(empty);
    assert_eq!(pack.view().cell_count(), 4);
    assert_eq!(pack.size(), PACKS[1].1);
    assert_eq!(pack.layout().cell_count, 4);
    assert_eq!(pack.id(), &spec(1).pack);
    assert!(pack.broken().is_none());
}

#[test]
fn the_counter_memfd_grows_past_a_chunk_and_the_server_remaps_before_each_open() {
    // Packs of 64 GiB: 4096 groups, an 8 KiB region each; a 64 KiB chunk holds eight.
    const PACK_SIZE: u64 = 64 << 30;
    const COUNT: usize = 9;
    let side = ServerSide::new("reads-grow");
    let big = TreeRef("tree-big".into());
    let keys: Vec<(String, u64)> = (0..COUNT)
        .map(|i| (format!("big-{i}"), PACK_SIZE))
        .collect();
    side.trees.set(big.clone(), FakeList::of(&keys));
    let client = side.client(1, &ManualClock::new());
    let id = |i: usize| PackId {
        key: PackKey(format!("big-{i}")),
        root: PackRoot::Digest([i as u8 + 1; 32]),
    };
    let packs: Vec<_> = (0..COUNT)
        .map(|i| {
            client
                .open(OpenSpec {
                    tree: big.clone(),
                    index: i as u32,
                    pack: id(i),
                    size: PACK_SIZE,
                })
                .wait()
                .unwrap()
        })
        .collect();
    let regions: Vec<CounterRegion> = packs.iter().map(|p| p.counters().region()).collect();
    for (i, r) in regions.iter().enumerate().take(8) {
        assert_eq!(
            *r,
            CounterRegion {
                offset: 8192 * i as u64,
                groups: 4096
            }
        );
    }
    // The ninth does not fit the first chunk: it starts the second.
    assert_eq!(
        regions[8],
        CounterRegion {
            offset: 64 << 10,
            groups: 4096
        }
    );
    // One mapping per open whose region ends past the server's mapping (each is a page
    // multiple: every open grew it).
    assert_eq!(side.mapper.maps(HolderId(1)), COUNT as u32);
    assert!(regions[8].end().unwrap() > 8 * PAGE_BYTES);

    // The server reads the counters of the first region and of the newest one.
    for p in [&packs[0], &packs[8]] {
        p.counters().enter(4095);
        p.counters().enter(4095);
        p.counters().leave(4095);
        // A group outside the region is not counted.
        p.counters().enter(4096);
        p.counters().leave(4096);
    }
    let entry = |i: usize| side.store.file_entry(&id(i)).unwrap();
    assert_eq!(entry(0).group_readers(4095), 1);
    assert_eq!(entry(8).group_readers(4095), 1);
    assert_eq!(entry(1).group_readers(4095), 0);
    assert_eq!(Arc::strong_count(&packs[0]), 2);
}
