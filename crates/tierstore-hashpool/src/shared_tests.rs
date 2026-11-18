//! The pool's bound, pause and order, on a pool with no threads: the test is the thread, taking
//! one cell at a time, so every count is exact and nothing depends on timing. The wait a paused
//! call records is a step of a manual clock.
//!
//! Catches: a 33rd cell admitted while 32 are held; a paused call never woken, woken out of turn,
//! or woken without a slot; a later arrival taking a waiter's slot; a recheck cell taking the
//! slot a fill waits for, or recheck cells filling the pool; a thread hashing a recheck cell
//! before a queued fill; a dropped waiter keeping its place or the slot it was handed; a caller
//! that stops waiting leaking its slot; a lost verdict read as a match; the caller's buffer
//! shared with the pool after the verdict; queued cells dropped at shutdown.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::future::BoxFuture;
use futures::task::{ArcWake, waker};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::verify::{CellJob, Verdict, VerifyPriority};

use super::{ByPriority, Core, PoolStats};
use crate::plan::{QUEUE_CELLS, RECHECK_CELLS};

const FILL: VerifyPriority = VerifyPriority::Fill;
const RECHECK: VerifyPriority = VerifyPriority::Recheck;

/// Counts the wakes of one call.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl ArcWake for Wakes {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// One `verify` call, polled by hand.
struct Call<'a> {
    fut: BoxFuture<'a, (Verdict, Bytes)>,
    wakes: Arc<Wakes>,
}

impl Call<'_> {
    /// Polls once; the verdict if it is ready.
    fn poll(&mut self) -> Option<(Verdict, Bytes)> {
        let w = waker(Arc::clone(&self.wakes));
        match Pin::new(&mut self.fut).poll(&mut Context::from_waker(&w)) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => None,
        }
    }

    fn wakes(&self) -> usize {
        self.wakes.0.load(Ordering::SeqCst)
    }
}

struct Rig {
    core: Core,
    clock: ManualClock,
}

impl Rig {
    fn new() -> Self {
        let clock = ManualClock::new();
        Self {
            core: Core::new(Arc::new(clock.clone())),
            clock,
        }
    }

    /// A call for cell `cell` whose table digest is right.
    fn good(&self, cell: u32, priority: VerifyPriority) -> Call<'_> {
        self.call(job(cell, priority, true))
    }

    fn call(&self, job: CellJob) -> Call<'_> {
        Call {
            fut: Box::pin(self.core.verify(job)),
            wakes: Arc::default(),
        }
    }

    /// Polls a new call once and checks it is pending; returns it.
    fn pending(&self, cell: u32, priority: VerifyPriority) -> Call<'_> {
        let mut c = self.good(cell, priority);
        assert!(
            c.poll().is_none(),
            "cell {cell} answered before a thread ran"
        );
        c
    }

    /// Does one thread's step: hashes the next queued cell. False when none is queued.
    fn hash_one(&self) -> bool {
        match self.core.try_next() {
            Some(q) => {
                self.core.process(q);
                true
            }
            None => false,
        }
    }

    fn stats(&self) -> PoolStats {
        self.core.stats()
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Cell `cell` of 64 bytes of its own index; with `right`, the table's digest is its sha256.
fn job(cell: u32, priority: VerifyPriority, right: bool) -> CellJob {
    let bytes = Bytes::from(vec![u8::try_from(cell % 251).unwrap(); 64]);
    let mut expected = sha256(&bytes);
    if !right {
        expected[31] ^= 1;
    }
    CellJob {
        cell,
        expected,
        bytes,
        priority,
    }
}

/// Fills `n` slots with pending calls of `priority`, cells numbered from `first`.
fn hold(rig: &Rig, first: u32, n: usize, priority: VerifyPriority) -> Vec<Call<'_>> {
    (0..n)
        .map(|i| rig.pending(first + u32::try_from(i).unwrap(), priority))
        .collect()
}

#[test]
fn the_33rd_fill_waits_until_a_cell_is_hashed_and_then_holds_the_freed_slot() {
    let rig = Rig::new();
    let mut held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let s = rig.stats();
    assert_eq!((s.held, s.queued, s.waiting, s.busy), (32, 32, 0, 0));
    assert_eq!(s.paused, ByPriority::default());

    let mut late = rig.pending(32, FILL);
    let s = rig.stats();
    assert_eq!(
        (s.held, s.queued, s.waiting),
        (32, 32, 1),
        "the 33rd cell is not admitted"
    );
    assert_eq!(
        s.paused,
        ByPriority {
            fill: 1,
            recheck: 0
        }
    );
    // Polled again while still in line, it stays there.
    assert!(late.poll().is_none());
    assert_eq!(rig.stats().waiting, 1);

    rig.clock.advance(7);
    assert!(rig.hash_one());
    assert_eq!(late.wakes(), 1, "the waiter is woken when a slot frees");
    let s = rig.stats();
    assert_eq!(
        (s.held, s.queued, s.waiting),
        (32, 31, 0),
        "its slot is held for it"
    );
    assert_eq!(s.pause_wait_ms, 7, "the wait is the clock's 7 ms");

    let (v, _) = held[0].poll().unwrap();
    assert_eq!(v, Verdict::Match);
    assert!(late.poll().is_none(), "admitted, it waits for a thread now");
    assert_eq!(rig.stats().queued, 32);

    while rig.hash_one() {}
    for c in held.iter_mut().skip(1).chain([&mut late]) {
        assert_eq!(c.poll().unwrap().0, Verdict::Match);
    }
    let s = rig.stats();
    assert_eq!((s.held, s.queued, s.high_water), (0, 0, 32));
    assert_eq!(
        s.verified,
        ByPriority {
            fill: 33,
            recheck: 0
        }
    );
}

#[test]
fn waiting_fills_are_handed_slots_first_come_first_served() {
    let rig = Rig::new();
    let _held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let a = rig.pending(100, FILL);
    let b = rig.pending(101, FILL);
    let c = rig.pending(102, FILL);

    assert!(rig.hash_one());
    assert_eq!((a.wakes(), b.wakes(), c.wakes()), (1, 0, 0));
    assert!(rig.hash_one());
    assert_eq!((a.wakes(), b.wakes(), c.wakes()), (1, 1, 0));
    assert_eq!(rig.stats().waiting, 1);
}

#[test]
fn a_waiting_fill_is_handed_the_slot_before_a_recheck_cell_that_waited_longer() {
    let rig = Rig::new();
    let _held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let mut recheck = rig.pending(100, RECHECK);
    let mut fill = rig.pending(101, FILL);

    assert!(rig.hash_one());
    assert_eq!((fill.wakes(), recheck.wakes()), (1, 0));
    assert!(fill.poll().is_none());
    assert!(rig.hash_one());
    assert_eq!(
        recheck.wakes(),
        1,
        "the next freed slot goes to the recheck cell"
    );
    assert!(recheck.poll().is_none());
    assert_eq!(
        rig.stats().paused,
        ByPriority {
            fill: 1,
            recheck: 1
        }
    );
}

#[test]
fn recheck_cells_hold_at_most_half_the_slots_so_they_never_pause_a_fill() {
    let rig = Rig::new();
    let _rechecks = hold(&rig, 0, RECHECK_CELLS, RECHECK);
    let mut extra = rig.pending(16, RECHECK);
    let s = rig.stats();
    assert_eq!(
        (s.held, s.held_recheck, s.waiting),
        (16, 16, 1),
        "the 17th recheck cell waits"
    );

    // The other half takes fills at once.
    let _fills = hold(&rig, 100, QUEUE_CELLS - RECHECK_CELLS, FILL);
    let s = rig.stats();
    assert_eq!(
        (s.held, s.paused.fill),
        (32, 0),
        "no fill paused for recheck cells"
    );
    let mut late_fill = rig.pending(200, FILL);
    assert_eq!(
        rig.stats().paused.fill,
        1,
        "a 33rd cell pauses whatever its kind"
    );

    // Fills are hashed first, and a freed fill slot goes to the waiting fill.
    assert!(rig.hash_one());
    assert_eq!(
        rig.stats().verified,
        ByPriority {
            fill: 1,
            recheck: 0
        }
    );
    assert_eq!((late_fill.wakes(), extra.wakes()), (1, 0));
    assert!(late_fill.poll().is_none());

    // The recheck cell waits for a recheck slot even when the pool has room.
    while rig.stats().verified.fill < 17 {
        assert!(rig.hash_one());
    }
    let s = rig.stats();
    assert_eq!((s.held, s.held_recheck, s.waiting), (16, 16, 1));
    assert_eq!(extra.wakes(), 0);

    assert!(rig.hash_one());
    assert_eq!(
        rig.stats().verified,
        ByPriority {
            fill: 17,
            recheck: 1
        }
    );
    assert_eq!(extra.wakes(), 1);
    assert!(extra.poll().is_none());
    assert_eq!(rig.stats().held_recheck, 16);
}

#[test]
fn a_thread_takes_a_queued_fill_before_recheck_cells_queued_earlier() {
    let rig = Rig::new();
    let mut r1 = rig.pending(1, RECHECK);
    let mut r2 = rig.pending(2, RECHECK);
    let mut f = rig.pending(3, FILL);

    assert!(rig.hash_one());
    assert!(f.poll().is_some());
    assert!(r1.poll().is_none() && r2.poll().is_none());
    assert!(rig.hash_one());
    assert!(r1.poll().is_some());
    assert!(r2.poll().is_none());
    assert!(rig.hash_one());
    assert!(r2.poll().is_some());
    assert!(!rig.hash_one());
}

#[test]
fn a_waiter_dropped_in_line_leaves_it_and_the_next_one_gets_the_slot() {
    let rig = Rig::new();
    let _held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let a = rig.pending(100, FILL);
    let b = rig.pending(101, FILL);
    drop(a);
    assert_eq!(rig.stats().waiting, 1);
    assert_eq!(rig.stats().held, 32, "leaving the line frees no slot");

    assert!(rig.hash_one());
    assert_eq!(b.wakes(), 1);
    assert_eq!(rig.stats().waiting, 0);
}

#[test]
fn a_waiter_dropped_after_it_was_handed_a_slot_hands_the_slot_on() {
    let rig = Rig::new();
    let _held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let a = rig.pending(100, FILL);
    let mut b = rig.pending(101, RECHECK);

    assert!(rig.hash_one());
    assert_eq!((a.wakes(), b.wakes()), (1, 0));
    drop(a);
    assert_eq!(b.wakes(), 1, "the slot a dropped waiter was handed goes on");
    let s = rig.stats();
    assert_eq!((s.held, s.held_recheck, s.waiting), (32, 1, 0));

    assert!(b.poll().is_none());
    drop(b);
    assert_eq!(
        rig.stats().held,
        32,
        "a queued cell keeps its slot until it is hashed"
    );
    while rig.hash_one() {}
    let s = rig.stats();
    assert_eq!((s.held, s.held_recheck, s.queued), (0, 0, 0));
}

#[test]
fn a_handed_slot_nobody_else_wants_is_freed_when_its_waiter_drops() {
    let rig = Rig::new();
    let _held = hold(&rig, 0, QUEUE_CELLS, FILL);
    let a = rig.pending(100, FILL);
    assert!(rig.hash_one());
    assert_eq!(rig.stats().held, 32);
    drop(a);
    assert_eq!(rig.stats().held, 31);
}

#[test]
fn a_caller_that_stops_waiting_for_its_verdict_still_frees_its_slot() {
    let rig = Rig::new();
    let c = rig.pending(1, FILL);
    drop(c);
    assert_eq!((rig.stats().held, rig.stats().queued), (1, 1));
    assert!(rig.hash_one());
    let s = rig.stats();
    assert_eq!((s.held, s.queued), (0, 0));
    assert_eq!(s.verified.fill, 1);
}

#[test]
fn a_wrong_digest_is_a_mismatch_counted_by_priority() {
    let rig = Rig::new();
    let mut calls = vec![
        rig.call(job(1, FILL, false)),
        rig.call(job(2, RECHECK, false)),
        rig.call(job(3, RECHECK, true)),
    ];
    for c in &mut calls {
        assert!(c.poll().is_none());
    }
    while rig.hash_one() {}
    let verdicts: Vec<Verdict> = calls.iter_mut().map(|c| c.poll().unwrap().0).collect();
    assert_eq!(
        verdicts,
        [Verdict::Mismatch, Verdict::Mismatch, Verdict::Match]
    );
    let s = rig.stats();
    assert_eq!(
        s.verified,
        ByPriority {
            fill: 1,
            recheck: 2
        }
    );
    assert_eq!(
        s.mismatched,
        ByPriority {
            fill: 1,
            recheck: 1
        }
    );
}

#[test]
fn the_bytes_come_back_as_the_only_handle_to_the_buffer() {
    let rig = Rig::new();
    let mut buf = BytesMut::with_capacity(1 << 20);
    buf.extend_from_slice(b"one cell");
    let bytes = buf.freeze();
    let ptr = bytes.as_ptr();
    let mut c = rig.call(CellJob {
        cell: 0,
        expected: sha256(b"one cell"),
        bytes,
        priority: FILL,
    });
    assert!(c.poll().is_none());
    assert!(rig.hash_one());
    let (v, back) = c.poll().unwrap();
    assert_eq!(v, Verdict::Match);
    assert_eq!(back.as_ptr(), ptr, "the same buffer");
    let reusable = back.try_into_mut().expect("no other handle is left");
    assert!(reusable.capacity() >= 1 << 20);
}

#[test]
fn a_cell_whose_verdict_is_lost_is_a_mismatch_with_its_bytes_back() {
    let rig = Rig::new();
    let mut c = rig.good(9, FILL);
    assert!(c.poll().is_none());
    // A thread that dies with the cell: the cell is taken and never answered.
    drop(rig.core.try_next().unwrap());
    let (v, bytes) = c.poll().unwrap();
    assert_eq!(v, Verdict::Mismatch);
    assert_eq!(bytes, Bytes::from(vec![9; 64]));
}

#[test]
fn a_shut_down_pool_hands_out_its_queued_cells_and_then_none() {
    let rig = Rig::new();
    let _a = rig.pending(1, RECHECK);
    let _b = rig.pending(2, FILL);
    rig.core.shut_down();
    let first = rig.core.next_blocking().unwrap();
    assert_eq!(rig.stats().busy, 1);
    rig.core.process(first);
    assert!(rig.core.next_blocking().is_some());
    assert!(rig.core.next_blocking().is_none());
}

#[test]
fn the_stats_debug_names_every_count() {
    let text = format!("{:?}", Rig::new().stats());
    for field in [
        "busy",
        "queued",
        "held",
        "held_recheck",
        "high_water",
        "waiting",
        "paused",
        "pause_wait_ms",
        "verified",
        "mismatched",
    ] {
        assert!(text.contains(field), "{field} in {text}");
    }
}
