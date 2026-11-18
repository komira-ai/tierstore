//! A waiting acquire is woken by what frees it, with no help from the test: each waiter here is
//! a spawned task the test only awaits, after a progress event (the clock's sleepers, or the
//! task still pending after the runtime has run every other task).
//!
//! Catches:
//! - a released permit that wakes no waiter;
//! - a pacing gap whose end wakes no waiter;
//! - a raised ceiling (the jobscheduler's hint) that wakes no waiter.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{PAST_COOLDOWN, Rig, bare, drive, part};
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Permit};
use tierstore_fetch_api::ids::Class;
use tokio::task::JoinHandle;

fn spawn(rig: &Rig, c: Charge) -> JoinHandle<Permit> {
    let b = rig.budget.clone();
    tokio::spawn(async move { b.acquire(c).await })
}

/// Lets every other task of the current-thread runtime run until it waits.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_release_wakes_a_waiter() {
    let rig = Rig::later();
    let held: Vec<_> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    let waiter = spawn(&rig, bare(Class::Demand));
    settle().await;
    assert!(!waiter.is_finished());
    drop(held);
    let p = waiter.await.unwrap();
    assert_eq!(rig.budget.snapshot().parts_in_flight, 1);
    drop(p);
}

#[tokio::test(flavor = "current_thread")]
async fn the_end_of_a_pacing_gap_wakes_a_waiter() {
    let rig = Rig::later();
    let d = drive(1, 0);
    rig.busy(d);
    rig.advance(PAST_COOLDOWN);
    rig.busy(d);
    drop(rig.acquire(part(Class::Demand, d)));
    let waiter = spawn(&rig, part(Class::Demand, d));
    // Progress event: the waiter sleeps on the clock.
    while rig.clock.sleepers() == 0 {
        tokio::task::yield_now().await;
    }
    assert!(!waiter.is_finished());
    rig.advance(250);
    drop(waiter.await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn a_raised_ceiling_wakes_a_waiter() {
    let rig = Rig::later();
    let held: Vec<_> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    let waiter = spawn(&rig, bare(Class::Demand));
    settle().await;
    assert!(!waiter.is_finished());
    // One active client: the share, and so the ceiling, is 90.
    rig.observe(Observation::ClientsActive(1));
    let p = waiter.await.unwrap();
    assert_eq!(rig.budget.snapshot().parts_in_flight, 4);
    drop((p, held));
}
