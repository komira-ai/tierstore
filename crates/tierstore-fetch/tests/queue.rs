//! Proves the queue of one class (design 4.5): deficit round robin across jobs with a quantum in
//! bytes, oldest job first, `(rank, offset)` order within a job, replacing a fill's items, and
//! the active-pack window.
//!
//! Catches: jobs that do not alternate at equal size; a quantum counted in items instead of
//! bytes; an item larger than the quantum that is never picked; a newer job picked before the
//! oldest; a higher rank or a lower offset picked out of order; stale items of a replaced fill
//! still picked; a window that lets a fourth fill start, that blocks the items of active fills,
//! that does not reopen on deactivate, or that blocks when unbounded; a blocked job that stalls
//! the other jobs; a pick that spins forever on a zero quantum.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_fetch::queue::{ClassQueue, ItemRef, Queued};
use tierstore_fetch_api::ids::{JobId, Rank};

const A: JobId = JobId(1);
const B: JobId = JobId(2);

fn item(job: JobId, rank: u32, offset: u64, size: u64, fill: u64, n: u64) -> Queued {
    Queued {
        job,
        rank: Rank(rank),
        offset,
        size,
        what: ItemRef { fill, item: n },
    }
}

/// Items `0..count` of `fill` for `job`, all of `size` bytes, at rank 0, offsets in item order.
fn items(job: JobId, fill: u64, count: u64, size: u64) -> Vec<Queued> {
    (0..count)
        .map(|n| item(job, 0, n * 100, size, fill, n))
        .collect()
}

/// Picks until nothing is left; each pick as (job, fill, item).
fn drain(q: &mut ClassQueue, quantum: u64) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    while let Some(p) = q.pick(quantum) {
        out.push((p.job.0, p.what.fill, p.what.item));
    }
    out
}

fn jobs_of(picks: &[(u64, u64, u64)]) -> Vec<u64> {
    picks.iter().map(|p| p.0).collect()
}

/// Two jobs with equal-size items at equal rank alternate A, B, A, B.
#[test]
fn two_jobs_with_equal_items_alternate() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 3, 4));
    q.set_fill(2, &items(B, 2, 3, 4));
    let picks = drain(&mut q, 4);
    assert_eq!(jobs_of(&picks), vec![1, 2, 1, 2, 1, 2]);
    assert!(q.is_empty());
}

/// The quantum is in bytes: a job with items of a quarter of the quantum gets four picks per turn.
#[test]
fn a_job_with_quarter_quantum_items_gets_four_picks_per_turn() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 8, 1));
    q.set_fill(2, &items(B, 2, 2, 4));
    let picks = drain(&mut q, 4);
    assert_eq!(jobs_of(&picks), vec![1, 1, 1, 1, 2, 1, 1, 1, 1, 2]);
}

/// An item larger than the quantum is picked after enough rounds, and a small item of another job
/// goes first meanwhile.
#[test]
fn an_item_larger_than_the_quantum_is_picked_after_enough_rounds() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 1, 10));
    q.set_fill(2, &items(B, 2, 1, 4));
    let picks = drain(&mut q, 4);
    assert_eq!(picks, vec![(2, 2, 0), (1, 1, 0)]);
}

/// A lone job's item larger than the quantum is still returned by one pick.
#[test]
fn a_lone_item_larger_than_the_quantum_is_picked() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 1, 1000));
    let got = q.pick(3).unwrap();
    assert_eq!(
        (got.job, got.size, got.what),
        (A, 1000, ItemRef { fill: 1, item: 0 })
    );
    assert!(q.is_empty());
}

/// A quantum of zero is treated as one byte, so the pick still returns.
#[test]
fn a_zero_quantum_still_picks() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 1, 3));
    assert_eq!(q.pick(0).unwrap().what, ItemRef { fill: 1, item: 0 });
}

/// The job that queued first is picked first, whatever the job ids.
#[test]
fn the_oldest_job_is_picked_first_on_a_tie() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(JobId(9), 1, 1, 4));
    q.set_fill(2, &items(JobId(1), 2, 1, 4));
    assert_eq!(q.pick(100).unwrap().job, JobId(9));
    assert_eq!(q.pick(100).unwrap().job, JobId(1));
}

/// A job that emptied and came back joins at the back of the round.
#[test]
fn a_job_that_comes_back_joins_behind_the_others() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 1, 4));
    q.set_fill(2, &items(B, 2, 1, 4));
    q.set_fill(1, &[]);
    q.set_fill(3, &items(A, 3, 1, 4));
    assert_eq!(jobs_of(&drain(&mut q, 100)), vec![2, 1]);
}

/// Within a job, items go by rank, then by offset: a lower rank of a later offset goes before a
/// higher rank.
#[test]
fn within_a_job_items_go_by_rank_then_offset() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(
        1,
        &[
            item(A, 5, 0, 1, 1, 0),
            item(A, 1, 100, 1, 1, 1),
            item(A, 1, 50, 1, 1, 2),
            item(A, 0, 900, 1, 1, 3),
        ],
    );
    let picks = drain(&mut q, 1 << 20);
    assert_eq!(picks, vec![(1, 1, 3), (1, 1, 2), (1, 1, 1), (1, 1, 0)]);
}

/// Rank and offset order holds across fills of the same job.
#[test]
fn within_a_job_the_order_holds_across_fills() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &[item(A, 2, 0, 1, 1, 0)]);
    q.set_fill(2, &[item(A, 1, 0, 1, 2, 0)]);
    assert_eq!(drain(&mut q, 1 << 20), vec![(1, 2, 0), (1, 1, 0)]);
}

/// set_fill replaces the items of a fill: the old ones are gone.
#[test]
fn set_fill_replaces_the_items_of_a_fill() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 2, 4));
    q.set_fill(1, &[item(A, 0, 0, 4, 1, 7)]);
    assert_eq!(q.len(), 1);
    assert_eq!(drain(&mut q, 4), vec![(1, 1, 7)]);
}

/// Replacing a fill leaves the items of other fills of the same job alone, and may move the fill
/// to another job.
#[test]
fn set_fill_leaves_other_fills_and_may_change_the_job() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 1, 4));
    q.set_fill(2, &items(A, 2, 1, 4));
    q.set_fill(1, &items(B, 1, 1, 4));
    assert_eq!(q.len(), 2);
    assert_eq!(drain(&mut q, 4), vec![(1, 2, 0), (2, 1, 0)]);
}

/// Replacing a fill with no items empties the queue.
#[test]
fn set_fill_with_no_items_removes_the_fill() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 3, 4));
    q.set_fill(1, &[]);
    assert!(q.is_empty());
    assert_eq!(q.pick(4), None);
}

/// An item that was picked is not removed twice or brought back when its fill is replaced.
#[test]
fn replacing_a_fill_after_a_pick_keeps_only_the_new_items() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 2, 4));
    assert_eq!(q.pick(4).unwrap().what.item, 0);
    q.set_fill(1, &[item(A, 0, 0, 4, 1, 9)]);
    assert_eq!(q.len(), 1);
    assert_eq!(drain(&mut q, 4), vec![(1, 1, 9)]);
}

/// With a window of 3 and three active fills, an item of a fourth fill is not picked.
#[test]
fn a_fourth_fill_is_not_picked_while_three_are_active() {
    let mut q = ClassQueue::new(3);
    for f in [1, 2, 3] {
        q.activate(f);
    }
    q.set_fill(4, &items(A, 4, 1, 4));
    assert_eq!(q.pick(4), None);
    assert_eq!(q.len(), 1);
}

/// With the window full, items of the active fills are still picked, and the fourth stays queued.
#[test]
fn items_of_active_fills_are_picked_with_the_window_full() {
    let mut q = ClassQueue::new(3);
    for f in [1, 2, 3] {
        q.activate(f);
    }
    q.set_fill(4, &items(A, 4, 1, 4));
    q.set_fill(2, &items(A, 2, 1, 4));
    assert_eq!(q.pick(4).unwrap().what.fill, 2);
    assert_eq!(q.pick(4), None);
    assert_eq!(q.len(), 1);
}

/// A blocked item of a lower rank does not hold back an eligible item of the same job.
#[test]
fn a_blocked_item_does_not_hold_back_an_eligible_item_of_its_job() {
    let mut q = ClassQueue::new(1);
    q.activate(2);
    q.set_fill(4, &[item(A, 0, 0, 4, 4, 0)]);
    q.set_fill(2, &[item(A, 5, 0, 4, 2, 0)]);
    assert_eq!(q.pick(4).unwrap().what.fill, 2);
}

/// A job whose items are all blocked does not hold back another job's items.
#[test]
fn a_job_with_only_blocked_items_does_not_hold_back_another_job() {
    let mut q = ClassQueue::new(1);
    q.activate(2);
    q.set_fill(4, &items(A, 4, 1, 4));
    q.set_fill(2, &items(B, 2, 1, 4));
    assert_eq!(q.pick(4).unwrap().job, B);
    assert_eq!(q.pick(4), None);
}

/// Deactivating a fill opens the window again for the blocked fill.
#[test]
fn deactivate_opens_the_window_again() {
    let mut q = ClassQueue::new(3);
    for f in [1, 2, 3] {
        q.activate(f);
    }
    q.set_fill(4, &items(A, 4, 1, 4));
    assert_eq!(q.pick(4), None);
    q.deactivate(1);
    assert_eq!(q.pick(4).unwrap().what.fill, 4);
}

/// A window with room (two of three active) lets a new fill's items through.
#[test]
fn a_window_with_room_lets_a_new_fill_through() {
    let mut q = ClassQueue::new(3);
    q.activate(1);
    q.activate(2);
    q.set_fill(4, &items(A, 4, 1, 4));
    assert_eq!(q.pick(4).unwrap().what.fill, 4);
}

/// Activating the same fill twice counts it once.
#[test]
fn activating_a_fill_twice_takes_one_place_of_the_window() {
    let mut q = ClassQueue::new(2);
    q.activate(1);
    q.activate(1);
    q.set_fill(4, &items(A, 4, 1, 4));
    assert_eq!(q.pick(4).unwrap().what.fill, 4);
}

/// A window of usize::MAX never blocks, however many fills are active.
#[test]
fn an_unbounded_window_never_blocks() {
    let mut q = ClassQueue::new(usize::MAX);
    for f in 0..100 {
        q.activate(f);
    }
    q.set_fill(1000, &items(A, 1000, 1, 4));
    assert_eq!(q.pick(4).unwrap().what.fill, 1000);
}

/// is_active tells whether a fill is activated, and deactivating an unknown fill changes nothing.
#[test]
fn is_active_follows_activate_and_deactivate() {
    let mut q = ClassQueue::new(3);
    assert!(!q.is_active(1));
    q.activate(1);
    assert!(q.is_active(1));
    assert!(!q.is_active(2));
    q.deactivate(2);
    assert!(q.is_active(1));
    q.deactivate(1);
    assert!(!q.is_active(1));
}

/// An empty queue picks nothing and reports length 0 and empty.
#[test]
fn an_empty_queue_picks_none() {
    let mut q = ClassQueue::new(3);
    assert_eq!(q.pick(4), None);
    assert_eq!(q.len(), 0);
    assert!(q.is_empty());
}

/// len counts the queued items of all jobs and falls with each pick.
#[test]
fn len_counts_the_queued_items_and_falls_with_each_pick() {
    let mut q = ClassQueue::new(usize::MAX);
    q.set_fill(1, &items(A, 1, 3, 4));
    q.set_fill(2, &items(B, 2, 2, 4));
    assert_eq!(q.len(), 5);
    assert!(!q.is_empty());
    let first = q.pick(4).unwrap();
    assert_eq!((first.job, first.size), (A, 4));
    assert_eq!(q.len(), 4);
}
