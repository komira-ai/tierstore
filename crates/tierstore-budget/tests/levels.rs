//! The controller's three levels (section 14 row "Controller with three levels"): two drives of
//! one storage node answer `503` and doubled queue waits (a hot pack); only those drives' permits
//! fall and parts in flight to every other drive are unchanged; the storage node's level falls
//! only when over a third of its drives with traffic are in decrease or its network signal
//! doubles, and leaves the other two storage nodes unchanged; the client ceiling falls only on the
//! client's own network signal or when over a third of all drives with traffic are in decrease; a
//! `stalled` or `indexing` answer changes no permit.
//!
//! Catches:
//! - one hot pack throttling its node or the farm;
//! - a slow drive hidden in a median pooled across drives;
//! - a node level moved by a third of its drives exactly (the rule is "over a third");
//! - a `stalled`, `indexing`, transport or not-a-load answer read as load.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{PAST_COOLDOWN, Rig, drive, drives_of, part, response, signals};
use tierstore_budget::{BudgetConfig, DriveStep, LevelStep, Topology};
use tierstore_fetch_api::budget::{Observation, Outcome, Permit};
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, DriveState};
use tierstore_format::ids::NodeId;
use tierstore_metrics::labels::BudgetReason;

/// LATER's 45 drives with one client, so its ceiling (180 parts) holds a part on every drive.
fn lone_client() -> Rig {
    Rig::new(BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    }))
}

fn all_drives() -> Vec<DeviceRef> {
    (1..=3).flat_map(drives_of).collect()
}

/// One part in flight on every drive, and one answer from each (traffic).
fn busy_farm(rig: &Rig) -> Vec<Permit> {
    let permits = all_drives()
        .into_iter()
        .map(|d| rig.acquire(part(Class::Prepare, d)))
        .collect();
    for d in all_drives() {
        rig.served(d, 20, 10);
    }
    permits
}

#[test]
fn a_hot_pack_throttles_its_drives_and_nothing_else() {
    let rig = lone_client();
    let _permits = busy_farm(&rig);
    let hot = [drive(1, 0), drive(1, 1)];
    for d in hot {
        rig.busy(d);
    }
    let s = rig.budget.snapshot();
    for (d, v) in &s.devices {
        let in_hot = hot.contains(d);
        assert_eq!(v.permits, if in_hot { 1 } else { 2 }, "{d:?}");
        assert_eq!(v.decrease, in_hot, "{d:?}");
        assert_eq!(v.in_flight, 1, "{d:?}: parts in flight are unchanged");
    }
    assert_eq!(s.devices.len(), 45);
    for n in 1..=3 {
        assert_eq!(s.nodes[&NodeId(n)].limit, None, "node {n}");
        assert_eq!(s.nodes[&NodeId(n)].step, LevelStep::None, "node {n}");
    }
    assert_eq!(s.ceiling, 90);
    assert_eq!(s.ceiling_step, LevelStep::None);
    assert_eq!(s.reason, BudgetReason::Drive503);

    // The same drives' queue waits double for two windows: they step to pacing; the drives
    // beside them, whose waits do not move, keep their permits.
    let quiet = [drive(1, 2), drive(1, 3)];
    rig.drive_windows(&[hot[0], hot[1], quiet[0], quiet[1]], 20, 10);
    for _ in 0..2 {
        rig.drive_windows(&hot, 20, 40);
        rig.drive_windows(&quiet, 20, 10);
    }
    let s = rig.budget.snapshot();
    for d in hot {
        assert_eq!(s.devices[&d].step, DriveStep::Pacing, "{d:?}");
        assert_eq!(s.devices[&d].pace_ms, 250, "{d:?}");
    }
    for d in quiet {
        assert_eq!(s.devices[&d].permits, 2, "{d:?}");
        assert!(!s.devices[&d].decrease, "{d:?}");
    }
    assert_eq!(s.reason, BudgetReason::DriveQueueWait);
    assert_eq!(s.nodes[&NodeId(1)].limit, None);
}

#[test]
fn a_node_falls_only_when_over_a_third_of_its_drives_are_in_decrease() {
    let rig = lone_client();
    let _permits = busy_farm(&rig);
    // Five of node 1's fifteen drives: a third, not over it.
    for d in &drives_of(1)[..5] {
        rig.busy(*d);
    }
    assert_eq!(rig.budget.snapshot().nodes[&NodeId(1)].limit, None);
    // The sixth: over a third.
    rig.busy(drive(1, 5));
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].limit, Some(67));
    assert_eq!(s.nodes[&NodeId(1)].step, LevelStep::FewerParts);
    assert_eq!(s.reason, BudgetReason::NodeDrives);
    for n in [2, 3] {
        assert_eq!(s.nodes[&NodeId(n)].limit, None, "node {n}");
    }
    assert_eq!(s.ceiling, 90, "6 of 45 drives is not over a third of all");

    // Fifteen drives of all 45 in decrease: a third, not over it; the sixteenth moves the
    // ceiling, and leaves nodes 2 and 3 (five drives each) unchanged.
    for d in drives_of(2).iter().take(5).chain(&drives_of(3)[..4]) {
        rig.busy(*d);
    }
    assert_eq!(rig.budget.snapshot().ceiling, 90);
    rig.busy(drive(3, 4));
    let s = rig.budget.snapshot();
    assert_eq!(s.ceiling, 67);
    assert_eq!(s.ceiling_step, LevelStep::FewerParts);
    assert_eq!(s.reason, BudgetReason::ClientDrives);
    for n in [2, 3] {
        assert_eq!(s.nodes[&NodeId(n)].limit, None, "node {n}");
    }
}

#[test]
fn drives_without_recent_traffic_do_not_count() {
    let rig = lone_client();
    // Node 1's fifteen drives answered once, then nothing for over ten minutes; two drives
    // answer again and one of them is busy: one of two drives with traffic is over a third.
    for d in drives_of(1) {
        rig.served(d, 20, 10);
    }
    rig.advance(600_001);
    rig.served(drive(1, 1), 20, 10);
    rig.busy(drive(1, 0));
    // Counted with the thirteen silent drives it would be 1 of 15, not over a third.
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].limit, Some(67));
    assert_eq!(s.ceiling, 67);
    assert_eq!(s.reason, BudgetReason::ClientDrives);
}

/// One network window of each storage node; `slow` sends `ms` per MiB, the others 10.
fn node_windows(rig: &Rig, slow: Option<NodeId>, ms: u32) {
    let at = |n: NodeId| if Some(n) == slow { ms } else { 10 };
    for _ in 0..15 {
        for n in 1..=3 {
            let node = Some(NodeId(n));
            rig.observe(Observation::Network {
                node,
                ms_per_mib: at(NodeId(n)),
            });
        }
    }
    rig.advance(PAST_COOLDOWN);
    for n in 1..=3 {
        rig.observe(Observation::Network {
            node: Some(NodeId(n)),
            ms_per_mib: at(NodeId(n)),
        });
    }
}

#[test]
fn a_slow_node_link_lowers_that_node_alone() {
    let rig = lone_client();
    let _permits = busy_farm(&rig);
    node_windows(&rig, None, 10);
    node_windows(&rig, Some(NodeId(1)), 30);
    assert_eq!(
        rig.budget.snapshot().nodes[&NodeId(1)].limit,
        None,
        "one window is not enough"
    );
    node_windows(&rig, Some(NodeId(1)), 30);
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].limit, Some(67));
    assert_eq!(s.reason, BudgetReason::NodeNetwork);
    for n in [2, 3] {
        assert_eq!(s.nodes[&NodeId(n)].limit, None, "node {n}");
    }
    assert_eq!(s.ceiling, 90, "the client's median is unchanged");
    for v in s.devices.values() {
        assert_eq!(v.permits, 2, "no drive is touched");
    }
}

#[test]
fn twice_the_baseline_exactly_is_not_a_decrease() {
    let rig = lone_client();
    node_windows(&rig, None, 10);
    for _ in 0..3 {
        node_windows(&rig, Some(NodeId(1)), 20);
    }
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].limit, None);
}

#[test]
fn the_ceiling_falls_on_the_clients_own_network_signal() {
    let rig = lone_client();
    rig.network_window(None, 10);
    rig.network_window(None, 25);
    assert_eq!(rig.budget.snapshot().ceiling, 90);
    rig.network_window(None, 25);
    let s = rig.budget.snapshot();
    assert_eq!(s.ceiling, 67);
    assert_eq!(s.reason, BudgetReason::ClientNetwork);
    assert!(s.nodes.is_empty(), "no node was named");
    assert!(s.devices.is_empty(), "no device was answered");
}

#[test]
fn a_window_between_two_slow_ones_resets_the_count() {
    let rig = lone_client();
    rig.network_window(None, 10);
    rig.network_window(None, 25);
    rig.network_window(None, 10);
    rig.network_window(None, 25);
    assert_eq!(rig.budget.snapshot().ceiling, 90);
}

#[test]
fn stalled_indexing_transport_and_not_load_answers_change_no_permit() {
    let rig = lone_client();
    let d = drive(1, 0);
    let _p = rig.acquire(part(Class::Prepare, d));
    let before = rig.budget.snapshot();
    let stalled = signals(Some(5_000), Some(DriveState::Stalled));
    let indexing = signals(None, Some(DriveState::Indexing));
    for _ in 0..3 {
        for o in [
            response(d, None, stalled, Outcome::Busy),
            response(d, Some(9_000), indexing, Outcome::Served),
            response(d, None, DriveSignals::default(), Outcome::NotLoad),
            response(d, None, DriveSignals::default(), Outcome::Transport),
        ] {
            for _ in 0..16 {
                rig.observe(o.clone());
            }
            rig.advance(PAST_COOLDOWN);
        }
    }
    let after = rig.budget.snapshot();
    assert_eq!(after.devices, before.devices);
    assert_eq!(after.ceiling, before.ceiling);
    // A drive the client never sent a part to keeps no state from such answers either.
    rig.observe(response(drive(2, 0), None, stalled, Outcome::Busy));
    assert!(!rig.budget.snapshot().devices.contains_key(&drive(2, 0)));
    // An `up` state is a load signal like no state at all.
    rig.observe(response(
        d,
        None,
        signals(None, Some(DriveState::Up)),
        Outcome::Busy,
    ));
    assert_eq!(rig.budget.snapshot().devices[&d].permits, 1);
}

#[test]
fn a_first_byte_doubling_in_one_size_class_lowers_the_drive() {
    let rig = lone_client();
    let d = drive(2, 7);
    let other = drive(2, 8);
    // Traffic on four more drives: one drive in decrease of six is not over a third.
    for i in 0..4 {
        rig.served(drive(2, i), 20, 10);
    }
    let fb = |class: u8, ms: u32, dev: DeviceRef| {
        rig.observe(Observation::Response {
            device: dev,
            size_class: class,
            first_byte_ms: Some(ms),
            signals: DriveSignals::default(),
            outcome: Outcome::Served,
        });
    };
    // Size class 0 answers fast and class 2 slow; each class has its own baseline, so class 2's
    // slower first byte is not a decrease.
    for round in 0..3 {
        for _ in 0..8 {
            fb(0, 20, d);
            fb(2, 200, d);
            fb(0, 20, other);
        }
        rig.advance(PAST_COOLDOWN);
        if round < 2 {
            fb(0, 20, d);
            fb(0, 20, other);
        }
    }
    assert_eq!(rig.budget.snapshot().devices[&d].permits, 2);
    // Class 0 doubles on d alone for two windows.
    for _ in 0..2 {
        for _ in 0..16 {
            fb(0, 50, d);
            fb(0, 20, other);
        }
        rig.advance(PAST_COOLDOWN);
        fb(0, 50, d);
        fb(0, 20, other);
    }
    let s = rig.budget.snapshot();
    assert_eq!(s.devices[&d].permits, 1);
    assert_eq!(s.devices[&other].permits, 2);
    assert_eq!(s.reason, BudgetReason::DriveFirstByte);
}
