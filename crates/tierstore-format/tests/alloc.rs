//! The per-node id allocator for block and stripe ids (10.2).
//!
//! Catches:
//! - a first id that is not counter 1 of the allocator's node, or a `next` that repeats or skips a
//!   counter;
//! - a run that does not start at the first of `n` consecutive counters, or after which the next id
//!   is not `first + n`, or a run of 0 that is accepted;
//! - an `observe` that lowers the allocator, does not raise it above an id of its own node, or
//!   reacts to an id of another node;
//! - an allocator that hands out a counter above 2^56 - 1, hands out fewer than the counters left,
//!   or uses up counters on a run that does not fit;
//! - a block allocator and a stripe allocator that behave differently, or ids that overlap or leave
//!   gaps when several threads allocate at once.

use std::collections::HashSet;

use tierstore_format::FormatError;
use tierstore_format::alloc::{AllocatedId, IdAllocator};
use tierstore_format::ids::{BlockId, COUNTER_MAX, NodeId, StripeId};

const NODE: NodeId = NodeId(7);

fn used_up() -> FormatError {
    FormatError::Invalid {
        what: "id allocator",
        reason: "the node's counters are used up",
    }
}

fn empty_run() -> FormatError {
    FormatError::Invalid {
        what: "id allocator",
        reason: "a run of no ids",
    }
}

fn block(counter: u64) -> BlockId {
    BlockId::new(NODE, counter).unwrap_or_else(|e| panic!("{e}"))
}

fn stripe(counter: u64) -> StripeId {
    StripeId::new(NODE, counter).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_first_id_has_counter_one_and_the_allocators_node() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    let id = alloc.next().unwrap();
    assert_eq!(id, block(1));
    assert_eq!(id.counter(), 1);
    assert_eq!(id.node(), NODE);
}

#[test]
fn the_node_accessor_returns_the_node_given() {
    assert_eq!(IdAllocator::<BlockId>::new(NodeId(0)).node(), NodeId(0));
    assert_eq!(
        IdAllocator::<StripeId>::new(NodeId(255)).node(),
        NodeId(255)
    );
}

#[test]
fn ids_of_the_highest_node_keep_their_node_bits() {
    let alloc = IdAllocator::<StripeId>::new(NodeId(255));
    let id = alloc.next().unwrap();
    assert_eq!(id.node(), NodeId(255));
    assert_eq!(id.counter(), 1);
    assert_eq!(id.raw(), (255u64 << 56) | 1);
}

#[test]
fn next_increments_by_one_each_call() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    for want in 1..=50 {
        assert_eq!(alloc.next().unwrap(), block(want));
    }
}

#[test]
fn a_run_returns_the_first_of_n_and_the_next_id_follows_the_run() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    assert_eq!(alloc.next_run(5).unwrap(), block(1));
    assert_eq!(alloc.next().unwrap(), block(6));
    assert_eq!(alloc.next_run(1).unwrap(), block(7));
    assert_eq!(alloc.next_run(1000).unwrap(), block(8));
    assert_eq!(alloc.next().unwrap(), block(1008));
}

#[test]
fn a_run_of_zero_is_refused_and_uses_no_counter() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    assert_eq!(alloc.next_run(0).unwrap_err(), empty_run());
    assert_eq!(alloc.next().unwrap(), block(1));
    assert_eq!(alloc.next_run(0).unwrap_err(), empty_run());
    assert_eq!(alloc.next().unwrap(), block(2));
}

#[test]
fn observe_raises_the_allocator_above_an_id_of_its_own_node() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(41));
    assert_eq!(alloc.next().unwrap(), block(42));
    alloc.observe(block(100));
    assert_eq!(alloc.next_run(3).unwrap(), block(101));
    assert_eq!(alloc.next().unwrap(), block(104));
}

#[test]
fn observe_never_lowers_the_allocator() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(10));
    alloc.observe(block(3));
    assert_eq!(alloc.next().unwrap(), block(11));
    alloc.observe(block(11));
    assert_eq!(alloc.next().unwrap(), block(12));
    alloc.observe(block(12));
    alloc.observe(block(1));
    assert_eq!(alloc.next().unwrap(), block(13));
}

#[test]
fn observe_of_a_counter_of_zero_changes_nothing() {
    let alloc = IdAllocator::<StripeId>::new(NODE);
    alloc.observe(StripeId::from_raw(0));
    alloc.observe(StripeId::new(NODE, 0).unwrap());
    assert_eq!(alloc.next().unwrap(), stripe(1));
}

#[test]
fn observe_ignores_an_id_of_another_node() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(BlockId::new(NodeId(8), 900).unwrap());
    alloc.observe(BlockId::new(NodeId(0), COUNTER_MAX).unwrap());
    alloc.observe(BlockId::new(NodeId(6), 5).unwrap());
    assert_eq!(alloc.next().unwrap(), block(1));
}

#[test]
fn observe_of_the_largest_counter_leaves_no_id_to_allocate() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(COUNTER_MAX));
    assert_eq!(alloc.next().unwrap_err(), used_up());
    assert_eq!(alloc.next().unwrap_err(), used_up());
    assert_eq!(alloc.next_run(1).unwrap_err(), used_up());
    alloc.observe(block(5));
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

#[test]
fn an_allocator_near_the_limit_hands_out_exactly_the_counters_left() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(COUNTER_MAX - 3));
    assert_eq!(alloc.next().unwrap(), block(COUNTER_MAX - 2));
    assert_eq!(alloc.next().unwrap(), block(COUNTER_MAX - 1));
    assert_eq!(alloc.next().unwrap(), block(COUNTER_MAX));
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

#[test]
fn a_run_that_fills_the_remaining_counters_exactly_succeeds_then_nothing_is_left() {
    let alloc = IdAllocator::<StripeId>::new(NODE);
    alloc.observe(stripe(COUNTER_MAX - 4));
    let first = alloc.next_run(4).unwrap();
    assert_eq!(first, stripe(COUNTER_MAX - 3));
    assert_eq!(first.counter() + 3, COUNTER_MAX);
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

#[test]
fn a_run_that_would_pass_the_limit_errors_and_uses_no_counter() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(COUNTER_MAX - 4));
    // Four counters are left: COUNTER_MAX - 3 ..= COUNTER_MAX.
    assert_eq!(alloc.next_run(5).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(u64::MAX).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(COUNTER_MAX).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(2).unwrap(), block(COUNTER_MAX - 3));
    assert_eq!(alloc.next_run(3).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(2).unwrap(), block(COUNTER_MAX - 1));
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

#[test]
fn a_fresh_allocator_refuses_a_run_larger_than_all_counters_and_allows_all_of_them() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    assert_eq!(alloc.next_run(COUNTER_MAX + 1).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(u64::MAX).unwrap_err(), used_up());
    assert_eq!(alloc.next().unwrap(), block(1));

    let all = IdAllocator::<StripeId>::new(NODE);
    assert_eq!(all.next_run(COUNTER_MAX).unwrap(), stripe(1));
    assert_eq!(all.next().unwrap_err(), used_up());
}

#[test]
fn a_run_error_after_observing_leaves_the_allocator_usable_for_smaller_runs() {
    let alloc = IdAllocator::<StripeId>::new(NODE);
    alloc.observe(stripe(COUNTER_MAX - 10));
    assert_eq!(alloc.next_run(11).unwrap_err(), used_up());
    assert_eq!(alloc.next_run(10).unwrap(), stripe(COUNTER_MAX - 9));
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

fn exercise<I: AllocatedId + PartialEq + std::fmt::Debug>(alloc: &IdAllocator<I>) {
    let first = alloc.next().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first.allocator(), NODE);
    assert_eq!(first.count(), 1);
    let run = alloc.next_run(4).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run.allocator(), NODE);
    assert_eq!(run.count(), 2);
    assert_eq!(alloc.next().unwrap_or_else(|e| panic!("{e}")).count(), 6);
}

#[test]
fn block_and_stripe_allocators_behave_the_same_through_the_generic_trait() {
    exercise(&IdAllocator::<BlockId>::new(NODE));
    exercise(&IdAllocator::<StripeId>::new(NODE));
}

#[test]
fn block_and_stripe_allocators_keep_separate_counters() {
    let blocks = IdAllocator::<BlockId>::new(NODE);
    let stripes = IdAllocator::<StripeId>::new(NODE);
    assert_eq!(blocks.next_run(10).unwrap(), block(1));
    assert_eq!(stripes.next().unwrap(), stripe(1));
    blocks.observe(block(99));
    assert_eq!(stripes.next().unwrap(), stripe(2));
    assert_eq!(blocks.next().unwrap(), block(100));
}

#[test]
fn make_builds_the_id_of_a_node_and_counter_and_refuses_a_counter_above_the_limit() {
    assert_eq!(BlockId::make(NODE, 9).unwrap(), block(9));
    assert_eq!(
        StripeId::make(NODE, COUNTER_MAX).unwrap(),
        stripe(COUNTER_MAX)
    );
    assert_eq!(
        BlockId::make(NODE, COUNTER_MAX + 1).unwrap_err(),
        FormatError::Invalid {
            what: "block id",
            reason: "counter above 2^56 - 1",
        }
    );
    assert!(StripeId::make(NODE, COUNTER_MAX + 1).is_err());
}

#[test]
fn threads_calling_next_get_disjoint_ids_covering_the_exact_count() {
    const THREADS: usize = 8;
    const PER_THREAD: u64 = 2000;
    let alloc = IdAllocator::<BlockId>::new(NODE);
    let all: Vec<Vec<u64>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                s.spawn(|| {
                    (0..PER_THREAD)
                        .map(|_| alloc.next().unwrap().counter())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total = THREADS as u64 * PER_THREAD;
    let mut seen = HashSet::new();
    for per_thread in &all {
        // Each thread sees its own ids in increasing order.
        assert!(per_thread.windows(2).all(|w| w[0] < w[1]));
        for counter in per_thread {
            assert!(seen.insert(*counter), "counter {counter} handed out twice");
        }
    }
    assert_eq!(seen.len() as u64, total);
    assert_eq!(seen.iter().min(), Some(&1));
    assert_eq!(seen.iter().max(), Some(&total));
    assert_eq!(alloc.next().unwrap(), block(total + 1));
}

#[test]
fn threads_mixing_next_and_runs_get_disjoint_ranges_without_gaps() {
    const THREADS: u64 = 6;
    const ROUNDS: u64 = 300;
    let alloc = IdAllocator::<StripeId>::new(NODE);
    // Each thread allocates a run of (thread + 1) ids and then one id, ROUNDS times.
    let ranges: Vec<Vec<(u64, u64)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let alloc = &alloc;
                s.spawn(move || {
                    let mut got = Vec::new();
                    for _ in 0..ROUNDS {
                        let first = alloc.next_run(t + 1).unwrap().counter();
                        got.push((first, t + 1));
                        got.push((alloc.next().unwrap().counter(), 1));
                    }
                    got
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total: u64 = (0..THREADS).map(|t| ROUNDS * (t + 2)).sum();
    let mut covered = vec![false; total as usize + 1];
    for (first, len) in ranges.into_iter().flatten() {
        for counter in first..first + len {
            assert!(
                !covered[counter as usize],
                "counter {counter} handed out twice"
            );
            covered[counter as usize] = true;
        }
    }
    assert!(!covered[0]);
    assert_eq!(covered.iter().filter(|c| **c).count() as u64, total);
    assert!(covered[1..].iter().all(|c| *c), "a counter was skipped");
    assert_eq!(alloc.next().unwrap(), stripe(total + 1));
}

#[test]
fn threads_racing_for_the_last_counters_hand_out_exactly_those_and_no_more() {
    const LEFT: u64 = 1000;
    let alloc = IdAllocator::<BlockId>::new(NODE);
    alloc.observe(block(COUNTER_MAX - LEFT));
    let granted: Vec<Vec<u64>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    let mut got = Vec::new();
                    while let Ok(id) = alloc.next() {
                        got.push(id.counter());
                    }
                    got
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all: Vec<u64> = granted.into_iter().flatten().collect();
    all.sort_unstable();
    let want: Vec<u64> = (COUNTER_MAX - LEFT + 1..=COUNTER_MAX).collect();
    assert_eq!(all.len() as u64, LEFT);
    assert_eq!(all, want);
    assert_eq!(alloc.next().unwrap_err(), used_up());
}

#[test]
fn concurrent_observes_leave_the_allocator_above_the_largest_id_seen() {
    let alloc = IdAllocator::<BlockId>::new(NODE);
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let alloc = &alloc;
            s.spawn(move || {
                for i in 0..500u64 {
                    alloc.observe(block(1 + i * 4 + t));
                }
            });
        }
    });
    // The largest counter observed is 1 + 499 * 4 + 3 = 2000.
    assert_eq!(alloc.next().unwrap(), block(2001));
}
