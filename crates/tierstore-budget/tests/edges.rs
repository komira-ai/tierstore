//! The controller's edges: windows that close inside a cooldown, windows with no sample, a level
//! whose decrease is held by its cooldown, a level that stays in its decrease state while the
//! drive evidence holds, the ceiling's way back while a node is still in decrease, a drive's last
//! permit held for demand at one permit (NOW), and the gauges of the ladder steps.
//!
//! Catches:
//! - a decrease state cleared inside its cooldown, or while over a third of the drives are still
//!   in decrease;
//! - a second decrease of a level inside its cooldown;
//! - a window of answers that carry no signal read as clean evidence (or as load);
//! - `P` restored while a node is still in decrease;
//! - `prepare` taking a drive's only permit while demand waits for it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::pin::pin;

use futures::poll;
use support::{MIB, PAST_COOLDOWN, Rig, bare, drive, part, response, signals};
use tierstore_budget::{BudgetConfig, DriveStep, LevelStep};
use tierstore_fetch_api::budget::{Budget, Observation, Outcome};
use tierstore_fetch_api::ids::Class;
use tierstore_format::ids::NodeId;
use tierstore_metrics::labels::BudgetReason;

#[test]
fn the_gauges_of_the_ladder_steps() {
    assert_eq!(
        [DriveStep::Full, DriveStep::Permits, DriveStep::Pacing].map(DriveStep::gauge),
        [0, 1, 2]
    );
    assert_eq!(
        [
            LevelStep::None,
            LevelStep::FewerParts,
            LevelStep::SmallerPart
        ]
        .map(LevelStep::gauge),
        [0, 1, 2]
    );
}

#[test]
fn a_window_that_closes_inside_the_cooldown_keeps_the_decrease_state() {
    let rig = Rig::later();
    for i in 0..15 {
        rig.served(drive(1, i), 20, 10);
    }
    let d = drive(1, 0);
    for _ in 0..14 {
        rig.served(d, 20, 10);
    }
    rig.advance(PAST_COOLDOWN);
    // The 503 decreases d and is the window's sixteenth answer: the window closes clean, inside
    // the cooldown it just started.
    rig.busy(d);
    let v = rig.budget.snapshot().devices[&d];
    assert!(v.decrease, "not cleared inside the cooldown");
    assert_eq!(v.permits, 1);
}

#[test]
fn answers_without_signals_are_no_evidence() {
    let rig = Rig::later();
    for i in 0..15 {
        rig.served(drive(1, i), 20, 10);
    }
    let d = drive(1, 1);
    rig.drive_window(d, 20, 10);
    rig.busy(d);
    rig.advance(PAST_COOLDOWN);
    // A window of answers with neither a first byte nor a queue wait: no evidence, so the
    // decrease state stands.
    for _ in 0..16 {
        rig.observe(response(d, None, signals(None, None), Outcome::Served));
    }
    rig.advance(PAST_COOLDOWN);
    rig.observe(response(d, None, signals(None, None), Outcome::Served));
    assert!(rig.budget.snapshot().devices[&d].decrease);
    // A window of 503s alone: each is a decrease, and the window says nothing more.
    for _ in 0..2 {
        rig.advance(PAST_COOLDOWN);
        rig.busy(d);
    }
    for _ in 0..16 {
        rig.busy(d);
    }
    let v = rig.budget.snapshot().devices[&d];
    assert_eq!((v.permits, v.pace_ms), (1, 500));
}

#[test]
fn a_level_inside_its_cooldown_holds_a_second_decrease() {
    // Traffic on two drives of node 1 only: each drive in decrease is over a third of node 1's
    // and of all.
    let rig = Rig::later();
    let (a, b) = (drive(1, 0), drive(1, 1));
    rig.served(a, 20, 10);
    rig.served(b, 20, 10);
    let _p = rig.acquire(part(Class::Demand, a));
    rig.busy(a);
    let s = rig.budget.snapshot();
    assert_eq!((s.nodes[&NodeId(1)].limit, s.ceiling), (Some(2), 2));
    rig.busy(b);
    let s = rig.budget.snapshot();
    assert_eq!(
        (s.nodes[&NodeId(1)].limit, s.ceiling, s.reason),
        (Some(2), 2, BudgetReason::Cooldown)
    );
}

#[test]
fn a_level_stays_in_decrease_while_cooling_or_while_its_drives_are() {
    let rig = Rig::later();
    let (a, b) = (drive(1, 0), drive(1, 1));
    rig.served(a, 20, 10);
    rig.served(b, 20, 10);
    let n1 = Some(NodeId(1));
    for _ in 0..15 {
        rig.observe(Observation::Network {
            node: n1,
            ms_per_mib: 10,
        });
    }
    rig.advance(PAST_COOLDOWN);
    rig.busy(a);
    // Node 1's and the client's windows close clean inside the cooldown of the decrease `a`
    // caused: both stay in decrease.
    rig.observe(Observation::Network {
        node: n1,
        ms_per_mib: 10,
    });
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].step, LevelStep::FewerParts);
    assert_eq!(s.ceiling_step, LevelStep::FewerParts);
    // Past the cooldown, clean network windows: still in decrease, because `a` (one of the two
    // drives with traffic) still is.
    rig.network_window(n1, 10);
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].step, LevelStep::FewerParts);
    assert_eq!(s.ceiling_step, LevelStep::FewerParts);
    // `a` leaves its decrease state at a clean window; the levels follow at their next clean
    // windows.
    rig.drive_window(a, 20, 10);
    assert!(!rig.budget.snapshot().devices[&a].decrease);
    rig.network_window(n1, 10);
    let s = rig.budget.snapshot();
    assert_eq!(s.nodes[&NodeId(1)].step, LevelStep::None);
    assert_eq!(s.ceiling_step, LevelStep::None);
}

#[test]
fn the_part_size_waits_for_every_node_to_leave_its_decrease_state() {
    let rig = Rig::later();
    // Traffic on three drives of node 1 and six of node 2.
    for i in 0..3 {
        rig.served(drive(1, i), 20, 10);
    }
    for i in 0..6 {
        rig.served(drive(2, i), 20, 10);
    }
    rig.network_window(None, 10);
    for _ in 0..2 {
        rig.network_window(None, 40);
        rig.network_window(None, 40);
    }
    assert_eq!(rig.budget.snapshot().ceiling, 2);
    assert_eq!(rig.budget.part_size(u64::MAX), 32 * MIB);
    // Two of node 1's three drives in decrease: node 1 falls; two of nine drives in all do not
    // move the client.
    rig.busy(drive(1, 0));
    rig.busy(drive(1, 1));
    assert_eq!(
        rig.budget.snapshot().nodes[&NodeId(1)].step,
        LevelStep::FewerParts
    );
    // A clean client window with a backlog: `P` stays while node 1 is in decrease, and the parts
    // grow instead.
    let held: Vec<_> = (0..2).map(|_| rig.acquire(bare(Class::Demand))).collect();
    assert!(rig.try_acquire(bare(Class::Demand)).is_none());
    drop(held);
    rig.network_window(None, 10);
    let s = rig.budget.snapshot();
    assert_eq!((s.ceiling, s.reason), (3, BudgetReason::SlowStart));
    assert_eq!(rig.budget.part_size(u64::MAX), 32 * MIB);
}

#[tokio::test(flavor = "current_thread")]
async fn at_one_permit_prepare_never_takes_a_drive_demand_waits_for() {
    // NOW: one stream per SMR drive, so one permit.
    let rig = Rig::new(BudgetConfig::new(support::now_profile()));
    let d = drive(1, 0);
    let p = rig.acquire(part(Class::Prepare, d));
    let mut demand = pin!(rig.budget.acquire(part(Class::Demand, d)));
    assert!(poll!(demand.as_mut()).is_pending());
    drop(p);
    assert!(rig.try_acquire(part(Class::Prepare, d)).is_none());
    assert!(rig.try_acquire(part(Class::Background, d)).is_none());
    let granted = poll!(demand.as_mut());
    assert!(granted.is_ready());
}
