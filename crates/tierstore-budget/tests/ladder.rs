//! The ladders (section 14 row "Scripted 2x first-byte or drive queue wait with every node at its
//! floor: the ladder steps down and in-flight decreases; a `503` is a decrease signal for its
//! drive"), the cooldown, and the way back up.
//!
//! - A drive: permits `2 -> 1`, then a pacing gap between part starts doubling per window from
//!   250 ms up to 2 s; back up in reverse, one step per clean window with a backlog.
//! - The client ceiling: fewer parts down to its floor (two), then `P` 64 -> 32 -> 16 MiB; back up
//!   in reverse: `P` first, then the parts, doubling toward the share and one per window above it.
//! - A storage node: fewer parts, then `P`; back up one part per window, and unbounded again once
//!   it reaches the ceiling.
//!
//! Catches:
//! - a floor that stops the client (no part granted at the bottom of the ladder);
//! - a ladder that skips or repeats a step, or a pacing gap past 2 s;
//! - a decrease inside the cooldown of the one before it;
//! - an increase without a backlog, or `P` restored after the parts;
//! - a pacing gap that is not waited for, or a waiter that misses the end of it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::pin::pin;

use futures::poll;
use support::{MIB, PAST_COOLDOWN, Rig, bare, drive, part};
use tierstore_budget::{BudgetConfig, DriveStep, LevelStep, Topology};
use tierstore_fetch_api::budget::{Budget, Observation};
use tierstore_fetch_api::ids::Class;
use tierstore_format::ids::NodeId;
use tierstore_metrics::labels::BudgetReason;

#[test]
fn a_drive_steps_down_its_ladder_one_step_per_window_and_never_stops() {
    let rig = Rig::later();
    let d = drive(1, 3);
    let _p = rig.acquire(part(Class::Prepare, d));
    let mut steps = Vec::new();
    for _ in 0..6 {
        rig.busy(d);
        let v = rig.budget.snapshot().devices[&d];
        steps.push((v.permits, v.pace_ms, v.step));
        rig.advance(PAST_COOLDOWN);
    }
    assert_eq!(
        steps,
        vec![
            (1, 0, DriveStep::Permits),
            (1, 250, DriveStep::Pacing),
            (1, 500, DriveStep::Pacing),
            (1, 1_000, DriveStep::Pacing),
            (1, 2_000, DriveStep::Pacing),
            (1, 2_000, DriveStep::Pacing),
        ]
    );
    assert_eq!(rig.budget.snapshot().devices[&d].step.gauge(), 2);
}

#[test]
fn a_second_503_inside_the_cooldown_does_not_decrease_again() {
    let rig = Rig::later();
    let d = drive(2, 0);
    rig.busy(d);
    assert_eq!(rig.budget.snapshot().devices[&d].permits, 1);
    // The cooldown is one window (2 s) with +-50% jitter: at least 1 s.
    rig.advance(999);
    rig.busy(d);
    let s = rig.budget.snapshot();
    assert_eq!(s.devices[&d].pace_ms, 0, "inside the cooldown");
    assert_eq!(s.reason, BudgetReason::Cooldown);
    // At most 3 s.
    rig.advance(2_002);
    rig.busy(d);
    assert_eq!(rig.budget.snapshot().devices[&d].pace_ms, 250);
}

#[tokio::test(flavor = "current_thread")]
async fn a_paced_drive_starts_one_part_per_gap() {
    let rig = Rig::later();
    let d = drive(1, 0);
    rig.busy(d);
    rig.advance(PAST_COOLDOWN);
    rig.busy(d);
    assert_eq!(rig.budget.snapshot().devices[&d].pace_ms, 250);
    let first = rig.acquire(part(Class::Demand, d));
    drop(first);
    // The drive is free, but the next start waits for the gap.
    let mut next = pin!(rig.budget.acquire(part(Class::Demand, d)));
    assert!(poll!(next.as_mut()).is_pending());
    rig.advance(249);
    assert!(poll!(next.as_mut()).is_pending());
    rig.advance(1);
    assert!(poll!(next.as_mut()).is_ready(), "granted when the gap ends");
}

#[test]
fn a_drive_comes_back_up_in_reverse_only_with_a_backlog() {
    let rig = Rig::later();
    let d = drive(3, 9);
    // Traffic on node 3's other drives, so one drive in a decrease state is not over a third
    // of them (which would lower node 3's level and the ceiling, and hold the backlog there).
    for i in 0..15 {
        rig.served(drive(3, i), 20, 10);
    }
    rig.drive_window(d, 20, 10);
    for _ in 0..4 {
        rig.busy(d);
        rig.advance(PAST_COOLDOWN);
    }
    assert_eq!(rig.budget.snapshot().devices[&d].pace_ms, 1_000);

    // A clean window without a backlog clears the decrease state and moves nothing.
    rig.drive_window(d, 20, 10);
    let v = rig.budget.snapshot().devices[&d];
    assert_eq!((v.decrease, v.pace_ms, v.permits), (false, 1_000, 1));

    // With a backlog (a part waits for the drive's one permit), each clean window takes one step
    // back: the gap halves, ends below 250 ms, then the permits grow to two and stay there.
    let mut seen = Vec::new();
    for _ in 0..5 {
        let permits = rig.budget.snapshot().devices[&d].permits;
        let held: Vec<_> = (0..permits)
            .map(|_| rig.acquire(part(Class::Demand, d)))
            .collect();
        assert!(rig.try_acquire(part(Class::Demand, d)).is_none());
        drop(held);
        rig.drive_window(d, 20, 10);
        let v = rig.budget.snapshot().devices[&d];
        seen.push((v.pace_ms, v.permits));
    }
    assert_eq!(seen, vec![(500, 1), (250, 1), (0, 1), (0, 2), (0, 2)]);
    assert_eq!(rig.budget.snapshot().reason, BudgetReason::Increase);
}

#[test]
fn a_hot_device_steps_down_from_what_the_ceiling_lets_it_use() {
    // An NVMe admits 64, but a client never has more than its ceiling in flight: the first step
    // is from the ceiling (3 at LATER's share), not from 64.
    let rig = Rig::later();
    let hot = tierstore_fetch_api::upstream::DeviceRef::Hot(NodeId(2));
    rig.busy(hot);
    let v = rig.budget.snapshot().devices[&hot];
    assert_eq!((v.max, v.permits, v.step), (64, 2, DriveStep::Permits));
    // The legacy upstream likewise (a fresh client: the hot device's decrease moved this one's
    // ceiling).
    let rig = Rig::later();
    let legacy = tierstore_fetch_api::upstream::DeviceRef::Legacy;
    rig.busy(legacy);
    let v = rig.budget.snapshot().devices[&legacy];
    assert_eq!((v.max, v.permits), (u32::MAX, 2));
}

#[test]
fn now_profile_drives_go_straight_to_pacing() {
    // NOW's SMR drives admit one stream: their first step is the pacing gap.
    let rig = Rig::new(BudgetConfig::new(support::now_profile()));
    let d = drive(4, 1);
    rig.busy(d);
    let v = rig.budget.snapshot().devices[&d];
    assert_eq!((v.max, v.permits, v.pace_ms), (1, 1, 250));
}

#[tokio::test(flavor = "current_thread")]
async fn the_ceiling_ladder_lowers_parts_then_the_part_size_and_never_stops() {
    let rig = Rig::later();
    assert_eq!(rig.budget.snapshot().ceiling, 3, "LATER's share");
    rig.network_window(None, 10);
    let mut seen = Vec::new();
    for _ in 0..5 {
        rig.network_window(None, 40);
        rig.network_window(None, 40);
        let s = rig.budget.snapshot();
        seen.push((
            s.ceiling,
            rig.budget.part_size(u64::MAX) / MIB,
            rig.budget.part_size(100 * MIB) / MIB,
            s.ceiling_step,
        ));
    }
    // The baseline is the windowed minimum: it stays at 10 while the windows above it are newer
    // than ten minutes.
    assert_eq!(
        seen,
        vec![
            (2, 64, 32, LevelStep::FewerParts),
            (2, 32, 16, LevelStep::SmallerPart),
            (2, 16, 16, LevelStep::SmallerPart),
            (2, 16, 16, LevelStep::SmallerPart),
            (2, 16, 16, LevelStep::SmallerPart),
        ]
    );
    let s = rig.budget.snapshot();
    assert_eq!(s.quantities.part_size, 16 * MIB);
    assert_eq!(
        s.quantities.bytes_in_flight_at_share,
        48 * MIB,
        "recomputed"
    );
    assert_eq!(s.ceiling_step.gauge(), 2);

    // In flight is two parts now; a third waits; the client still gets parts.
    let a = rig.acquire(bare(Class::Demand));
    let b = rig.acquire(bare(Class::Demand));
    let mut c = pin!(rig.budget.acquire(bare(Class::Demand)));
    assert!(poll!(c.as_mut()).is_pending());
    drop(a);
    assert!(poll!(c.as_mut()).is_ready());
    drop(b);
}

#[test]
fn the_ceiling_comes_back_part_size_first_then_parts() {
    let rig = Rig::later();
    rig.network_window(None, 10);
    for _ in 0..3 {
        rig.network_window(None, 40);
        rig.network_window(None, 40);
    }
    assert_eq!(rig.budget.part_size(u64::MAX), 16 * MIB);
    // The signal returns to its baseline. Each window with a backlog at the ceiling takes one
    // step back.
    let mut seen = Vec::new();
    for _ in 0..7 {
        let s = rig.budget.snapshot();
        let held: Vec<_> = (0..s.ceiling)
            .map(|_| rig.acquire(bare(Class::Demand)))
            .collect();
        assert!(rig.try_acquire(bare(Class::Demand)).is_none());
        drop(held);
        rig.network_window(None, 10);
        let s = rig.budget.snapshot();
        seen.push((rig.budget.part_size(u64::MAX) / MIB, s.ceiling, s.reason));
    }
    assert_eq!(
        seen,
        vec![
            (32, 2, BudgetReason::Increase),
            (64, 2, BudgetReason::Increase),
            (64, 3, BudgetReason::SlowStart),
            (64, 4, BudgetReason::Increase),
            (64, 5, BudgetReason::Increase),
            (64, 6, BudgetReason::Increase),
            (64, 6, BudgetReason::Increase),
        ]
    );
    // A clean window with no backlog moves nothing.
    rig.network_window(None, 10);
    assert_eq!(rig.budget.snapshot().ceiling, 6);
    assert_eq!(rig.budget.snapshot().ceiling_step, LevelStep::None);
}

#[test]
fn slow_start_doubles_toward_the_share() {
    // One client of LATER's 45 drives: share 90. After decreases from 90 to the floor, the way
    // back doubles until the share.
    let rig = Rig::new(BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    }));
    rig.network_window(None, 10);
    for _ in 0..20 {
        rig.network_window(None, 40);
        rig.network_window(None, 40);
    }
    assert_eq!(rig.budget.snapshot().ceiling, 2);
    let mut seen = Vec::new();
    for _ in 0..9 {
        let s = rig.budget.snapshot();
        let held: Vec<_> = (0..s.ceiling)
            .map(|_| rig.acquire(bare(Class::Demand)))
            .collect();
        assert!(rig.try_acquire(bare(Class::Demand)).is_none());
        drop(held);
        rig.network_window(None, 10);
        seen.push(rig.budget.snapshot().ceiling);
    }
    // Two `P` steps first, then 2 -> 4 -> 8 ... -> 90, then +1.
    assert_eq!(seen, vec![2, 2, 4, 8, 16, 32, 64, 90, 91]);
}

#[test]
fn a_node_level_steps_down_to_its_floor_then_the_part_size_and_back() {
    let rig = Rig::later();
    let n = Some(NodeId(1));
    rig.network_window(n, 10);
    let mut seen = Vec::new();
    for _ in 0..4 {
        rig.network_window(n, 40);
        rig.network_window(n, 40);
        let s = rig.budget.snapshot();
        seen.push((
            s.nodes[&NodeId(1)].limit,
            s.nodes[&NodeId(1)].step,
            rig.budget.part_size(u64::MAX) / MIB,
        ));
    }
    // Each node window is also a client window with the same samples: the client falls with it.
    // From the ceiling of 3: 2, 1 (its floor), then P.
    assert_eq!(seen[0], (Some(2), LevelStep::FewerParts, 64));
    assert_eq!(seen[1], (Some(1), LevelStep::FewerParts, 32));
    assert_eq!(seen[2].1, LevelStep::SmallerPart);
    assert_eq!(seen[3].2, 16);
}

/// One network window of each storage node; node `slow` sends `ms` per MiB, the others 10.
fn node_windows(rig: &Rig, slow: u8, ms: u32) {
    let at = |n: u8| if n == slow { ms } else { 10 };
    for _ in 0..15 {
        for n in 1..=3 {
            rig.observe(Observation::Network {
                node: Some(NodeId(n)),
                ms_per_mib: at(n),
            });
        }
    }
    rig.advance(PAST_COOLDOWN);
    for n in 1..=3 {
        rig.observe(Observation::Network {
            node: Some(NodeId(n)),
            ms_per_mib: at(n),
        });
    }
}

/// Holds `count` parts to `node` on distinct drives, and finds the next one refused (a backlog
/// at the node's level).
fn backlog_at_node(rig: &Rig, node: u8, count: u32) -> Vec<tierstore_fetch_api::budget::Permit> {
    let held: Vec<_> = (0..count)
        .map(|i| rig.acquire(part(Class::Demand, drive(node, u16::try_from(i).unwrap()))))
        .collect();
    assert!(
        rig.try_acquire(part(Class::Demand, drive(node, 14)))
            .is_none(),
        "the node's level is full"
    );
    held
}

#[test]
fn an_unbounded_node_level_becomes_unbounded_again_at_the_ceiling() {
    // Fifteen clients: share 6. Node 2 is slow alone; the client's median is the other nodes'.
    let rig = Rig::new(BudgetConfig::new(Topology {
        clients: 15,
        ..support::later()
    }));
    node_windows(&rig, 2, 10);
    node_windows(&rig, 2, 40);
    node_windows(&rig, 2, 40);
    let node = |rig: &Rig| rig.budget.snapshot().nodes[&NodeId(2)].limit;
    assert_eq!(node(&rig), Some(4), "from the ceiling of 6");
    assert_eq!(rig.budget.snapshot().ceiling, 6, "the client is not slow");
    // A clean window without a backlog leaves the decrease state and keeps the limit.
    node_windows(&rig, 2, 10);
    let v = rig.budget.snapshot().nodes[&NodeId(2)];
    assert_eq!((v.limit, v.step), (Some(4), LevelStep::None));
    drop(backlog_at_node(&rig, 2, 4));
    node_windows(&rig, 2, 10);
    assert_eq!(node(&rig), Some(5));
    drop(backlog_at_node(&rig, 2, 5));
    node_windows(&rig, 2, 10);
    assert_eq!(
        node(&rig),
        None,
        "at the ceiling of 6 the node level is unbounded again"
    );
}

#[test]
fn a_configured_node_limit_comes_back_to_its_value() {
    let mut config = BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    });
    config.node_permits = Some(4);
    let rig = Rig::new(config);
    node_windows(&rig, 1, 10);
    node_windows(&rig, 1, 40);
    node_windows(&rig, 1, 40);
    let limit = |rig: &Rig| rig.budget.snapshot().nodes[&NodeId(1)].limit;
    assert_eq!(limit(&rig), Some(3));
    drop(backlog_at_node(&rig, 1, 3));
    node_windows(&rig, 1, 10);
    assert_eq!(limit(&rig), Some(4));
    drop(backlog_at_node(&rig, 1, 4));
    node_windows(&rig, 1, 10);
    assert_eq!(
        limit(&rig),
        Some(4),
        "back to the configured value, and not above it"
    );
}
