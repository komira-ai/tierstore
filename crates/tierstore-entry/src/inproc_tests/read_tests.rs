//! The read path of the in-process front (5.6, the NFS `read` handler): the reader protocol of
//! 5.1 against the entry's own counters ([`InProcEntry::read_at`]) and the read that waits on the
//! engine ([`read`]).
//!
//! Section 14 rows: the reader half of the loom row of 5.1 (counters raised across the read, the
//! state loaded after them, the post-read recheck), on the in-process store; "Reader with a
//! forced loss of its counter: post-read recheck discards the bytes" (an eviction that begins
//! during the read is caught by the recheck); "Pack lost mid-job: ... waiters get EIO" (the read
//! of the front ends `Lost` or `Eio`).
//!
//! Catches: a read that serves a cell that is not present, before or after its `pread`; a read
//! whose counters are not raised while it reads (so the evictor could punch under it) or are left
//! raised after it; a read straddling two groups that counts only one; a read past the pack that
//! reads anything; a `bad` cell that is waited on instead of failing; a read that does not wait
//! for a missing cell, does not read again after the wait, or loops without end; a wait's EIO or
//! loss that is not passed on; a read that does not touch its groups.

use futures::FutureExt as _;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::{PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{Entry, EntryError, EvictStep};
use tierstore_fetch_api::fake::FakeEngine;
use tierstore_fetch_api::ids::{ByteRange, JobId};

use super::{BOOT, MIB, cell_bytes, open, pack, publish, run, scratch, store};
use crate::inproc::{InProcEntry, MAX_ROUNDS, ReadError, ReadOutcome, read};

/// A pack of 18 cells (two 16-cell groups), its last cell 100 bytes.
const SIZE: u64 = 17 * MIB + 100;

/// The engine's spec of the test pack.
fn pack_spec() -> PackSpec {
    PackSpec {
        id: pack("p"),
        size: SIZE,
        digests: None,
    }
}

/// The bytes of `[offset, offset + len)` of a pack whose cell `c` is filled with `c + 1`.
fn expected(offset: u64, len: u64) -> Vec<u8> {
    (offset..offset + len)
        .map(|o| (o >> 20) as u8 + 1)
        .collect()
}

/// Publishes `cells` of `e` with the fill of [`expected`].
fn fill(e: &InProcEntry, cells: impl IntoIterator<Item = u32>) {
    for c in cells {
        publish(e, c, c as u8 + 1);
    }
}

#[test]
fn a_read_of_present_cells_straddling_two_groups_returns_their_bytes_and_lowers_its_counters() {
    let dir = scratch("inproc-read");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, 14..18);
    let offset = 15 * MIB - 10;
    let len = 2 * MIB + 110;
    let mut buf = vec![0u8; len as usize];
    assert_eq!(e.read_at(offset, &mut buf).unwrap(), ReadOutcome::Read);
    assert_eq!(buf, expected(offset, len));
    assert_eq!((e.group_readers(0), e.group_readers(1)), (0, 0));
    // The short last cell, to the pack's last byte.
    let mut tail = vec![0u8; 100];
    assert_eq!(e.read_at(17 * MIB, &mut tail).unwrap(), ReadOutcome::Read);
    assert_eq!(tail, cell_bytes(&e, 17, 18));
}

#[test]
fn the_counters_of_every_covered_group_are_raised_while_the_bytes_are_read() {
    let dir = scratch("inproc-read-counters");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, 15..17);
    let mut buf = vec![0u8; 2 * MIB as usize];
    let mut seen = None;
    let outcome = e
        .read_with(15 * MIB, &mut buf, |data, buf| {
            seen = Some((e.group_readers(0), e.group_readers(1)));
            std::os::unix::fs::FileExt::read_exact_at(data, buf, 15 * MIB)
        })
        .unwrap();
    assert_eq!(outcome, ReadOutcome::Read);
    assert_eq!(
        seen,
        Some((1, 1)),
        "the evictor sees the reader on both groups"
    );
    assert_eq!((e.group_readers(0), e.group_readers(1)), (0, 0));
}

#[test]
fn an_eviction_that_begins_during_the_read_is_caught_by_the_post_read_recheck() {
    let dir = scratch("inproc-read-recheck");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, 0..3);
    let mut buf = vec![0u8; 3 * MIB as usize];
    let outcome = e
        .read_with(0, &mut buf, |_, _| {
            // A counter lost (forced clear) lets the evictor win the cell mid-read.
            assert!(e.evict_cell_cas(2, EvictStep::Begin));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        outcome,
        ReadOutcome::NotReady {
            cell: 2,
            bad: false
        }
    );
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn a_read_over_a_missing_or_bad_cell_reads_nothing_and_says_which_cell() {
    let dir = scratch("inproc-read-not-ready");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, [0, 2]);
    let mut buf = vec![0u8; 3 * MIB as usize];
    let mut preads = 0;
    let outcome = e
        .read_with(0, &mut buf, |_, _| {
            preads += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        outcome,
        ReadOutcome::NotReady {
            cell: 1,
            bad: false
        }
    );
    assert_eq!(preads, 0, "no byte is read when a cell is not present");
    let won = e.claim(run(1, 1));
    e.mark_bad(won.into_iter().next().unwrap());
    assert_eq!(
        e.read_at(0, &mut buf).unwrap(),
        ReadOutcome::NotReady { cell: 1, bad: true }
    );
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn an_empty_read_reads_nothing_and_a_read_past_the_pack_or_a_failed_pread_is_an_error() {
    let dir = scratch("inproc-read-bounds");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    assert_eq!(e.read_at(SIZE + 5, &mut []).unwrap(), ReadOutcome::Read);
    let mut buf = vec![0u8; 2];
    for offset in [SIZE - 1, u64::MAX] {
        assert!(matches!(
            e.read_at(offset, &mut buf),
            Err(EntryError::CellOutOfRange {
                cell: 18,
                cells: 18,
                ..
            })
        ));
    }
    fill(&e, [0]);
    let err = e
        .read_with(0, &mut buf, |_, _| Err(std::io::Error::other("disk")))
        .err()
        .unwrap();
    assert!(matches!(err, EntryError::Io { op: "pread", .. }));
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn a_ready_read_makes_no_wait_and_touches_every_covered_group() {
    let dir = scratch("inproc-read-ready");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, 15..17);
    let engine = FakeEngine::new();
    let clock = ManualClock::new();
    clock.advance(7_000);
    let bytes = read(
        &e,
        &engine,
        &pack_spec(),
        &clock,
        15 * MIB,
        2 * MIB as usize,
        JobId(1),
    )
    .now_or_never()
    .unwrap()
    .unwrap();
    assert_eq!(bytes, expected(15 * MIB, 2 * MIB));
    assert!(engine.waits().is_empty());
    assert_eq!((e.touch_epoch(0), e.touch_epoch(1)), (Some(7), Some(7)));
    assert_eq!(e.last_use(), 7_000);
    // An empty read touches nothing.
    let none = read(&e, &engine, &pack_spec(), &clock, 0, 0, JobId(1))
        .now_or_never()
        .unwrap()
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn a_read_of_a_missing_cell_waits_for_it_and_reads_again_once_it_is_present() {
    let dir = scratch("inproc-read-wait");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    fill(&e, [0]);
    let engine = FakeEngine::new();
    engine.park(pack("p"));
    let clock = ManualClock::new();
    let spec = pack_spec();
    let mut reading = Box::pin(read(
        &e,
        &engine,
        &spec,
        &clock,
        0,
        2 * MIB as usize,
        JobId(3),
    ));
    assert!((&mut reading).now_or_never().is_none(), "the read waits");
    assert_eq!(engine.parked(&pack("p")), 1);
    assert_eq!(
        engine.waits(),
        vec![(
            pack("p"),
            ByteRange {
                offset: 0,
                len: 2 * MIB
            },
            JobId(3)
        )]
    );
    // The fill publishes the cell, then the engine answers the wait.
    fill(&e, [1]);
    assert_eq!(engine.complete(&pack("p"), WaitOutcome::Ready), 1);
    let bytes = futures::executor::block_on(reading).unwrap();
    assert_eq!(bytes, expected(0, 2 * MIB));
    assert_eq!(engine.waits().len(), 1);
}

#[test]
fn a_bad_cell_a_failed_wait_and_a_lost_fill_end_the_read_with_their_error() {
    let dir = scratch("inproc-read-errors");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    let engine = FakeEngine::new();
    let clock = ManualClock::new();
    let spec = pack_spec();
    let go = |offset: u64| {
        read(&e, &engine, &spec, &clock, offset, 10, JobId(1))
            .now_or_never()
            .unwrap()
    };

    engine.set_outcome(pack("p"), WaitOutcome::Eio { cell: 0 });
    assert!(matches!(go(0), Err(ReadError::Eio { cell: 0, .. })));
    engine.set_outcome(pack("p"), WaitOutcome::Lost);
    assert!(matches!(go(0), Err(ReadError::Lost { .. })));
    assert_eq!(engine.waits().len(), 2);

    let won = e.claim(run(2, 1));
    e.mark_bad(won.into_iter().next().unwrap());
    assert!(matches!(go(2 * MIB), Err(ReadError::Eio { cell: 2, .. })));
    assert_eq!(engine.waits().len(), 2, "a bad cell is not waited on");

    assert!(matches!(
        go(SIZE),
        Err(ReadError::Entry(EntryError::CellOutOfRange { .. }))
    ));
    assert_eq!(e.state().load(2).unwrap().state(), Some(CellState::Bad));
}

#[test]
fn a_read_whose_cells_never_come_gives_up_after_the_last_round() {
    let dir = scratch("inproc-read-unsettled");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", SIZE);
    // An engine that answers ready while the cell stays absent.
    let engine = FakeEngine::new();
    let clock = ManualClock::new();
    let err = read(&e, &engine, &pack_spec(), &clock, 0, 1, JobId(1))
        .now_or_never()
        .unwrap()
        .err()
        .unwrap();
    assert!(matches!(
        err,
        ReadError::Unsettled {
            rounds: MAX_ROUNDS,
            ..
        }
    ));
    assert_eq!(engine.waits().len(), MAX_ROUNDS as usize);
    assert_eq!(e.group_readers(0), 0);
}
