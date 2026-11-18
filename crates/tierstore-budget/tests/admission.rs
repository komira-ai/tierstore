//! Admission: the demand reserve (section 14 row "Demand reserve granted with prepare saturating
//! the node"), the degraded `k` charge (rows "Degraded read: ... the client charges `k` parts"
//! and "Budget with one storage node down (fake cluster)"), what a permit returns, cancel
//! safety, and the load a `LOCATE` answer reports.
//!
//! Catches:
//! - `prepare` or `background` taking the demand reserve, or a drive's last permit while demand
//!   waits for it;
//! - a degraded part charged one part, or not charged on each source drive;
//! - drive-read load with a node down that differs from 2.0 times the healthy load (3.0 times per
//!   surviving drive);
//! - a charge larger than a level that can never run;
//! - a cancelled acquire that keeps a permit or a reservation;
//! - a permit that returns too little or too much;
//! - state kept for a device the client has no traffic with.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::BTreeMap;
use std::pin::pin;

use futures::poll;
use support::{PAST_COOLDOWN, Rig, bare, degraded, drive, part, signals};
use tierstore_budget::{BudgetConfig, Topology};
use tierstore_fetch_api::budget::{Budget, Charge, Permit};
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveState};
use tierstore_format::ids::NodeId;

#[test]
fn prepare_saturating_the_node_leaves_the_demand_reserve() {
    // LATER: ceiling 3 at the share; the reserve is max(1, 25% of 3) = 1.
    let rig = Rig::later();
    let prepare: Vec<Permit> = (0..2)
        .map(|i| rig.acquire(part(Class::Prepare, drive(1, i))))
        .collect();
    assert!(
        rig.try_acquire(part(Class::Prepare, drive(1, 5))).is_none(),
        "prepare stops at the ceiling less the reserve"
    );
    assert!(
        rig.try_acquire(part(Class::Background, drive(1, 5)))
            .is_none()
    );
    let demand = rig.acquire(part(Class::Demand, drive(1, 5)));
    assert_eq!(rig.budget.snapshot().parts_in_flight, 3);
    assert!(
        rig.try_acquire(part(Class::Demand, drive(1, 6))).is_none(),
        "the ceiling"
    );
    drop(prepare);
    drop(demand);
    assert_eq!(rig.budget.snapshot().parts_in_flight, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_drive_holds_one_permit_for_queued_demand() {
    let rig = Rig::new(BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    }));
    let d = drive(2, 4);
    let p1 = rig.acquire(part(Class::Prepare, d));
    let p2 = rig.acquire(part(Class::Prepare, d));
    // Demand queues for d.
    let mut demand = pin!(rig.budget.acquire(part(Class::Demand, d)));
    assert!(poll!(demand.as_mut()).is_pending());
    drop(p1);
    // The freed permit is the demand's: prepare may not take a drive's last permit while demand
    // waits for it.
    assert!(rig.try_acquire(part(Class::Prepare, d)).is_none());
    let granted = poll!(demand.as_mut());
    assert!(granted.is_ready());
    drop(granted);
    // No demand waits now: prepare takes the drive's permits again.
    drop(p2);
    let again = rig.acquire(part(Class::Prepare, d));
    let and_again = rig.acquire(part(Class::Prepare, d));
    drop((again, and_again));
}

#[tokio::test(flavor = "current_thread")]
async fn demand_waiting_for_the_ceiling_goes_before_prepare() {
    let rig = Rig::later();
    let held: Vec<Permit> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    let mut demand = pin!(rig.budget.acquire(bare(Class::Demand)));
    assert!(poll!(demand.as_mut()).is_pending());
    let mut prepare = pin!(rig.budget.acquire(bare(Class::Prepare)));
    assert!(poll!(prepare.as_mut()).is_pending());
    let mut held = held.into_iter();
    drop(held.next());
    // One part is free: prepare is under its cap (1 of 2) but demand waits at the ceiling.
    assert!(poll!(prepare.as_mut()).is_pending());
    let d = poll!(demand.as_mut());
    assert!(d.is_ready());
    drop(d);
    drop(held);
    assert!(poll!(prepare.as_mut()).is_ready());
}

#[test]
fn a_degraded_read_is_charged_k_parts_and_one_permit_per_source() {
    let rig = Rig::new(BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    }));
    let sources = [drive(1, 0), drive(1, 1), drive(2, 0), drive(2, 1)];
    let p = rig.acquire(degraded(Class::Demand, 1, &sources));
    let s = rig.budget.snapshot();
    assert_eq!(s.parts_in_flight, 4);
    assert_eq!(s.nodes[&NodeId(1)].in_flight, 4, "the reconstructor's node");
    for d in sources {
        assert_eq!(s.devices[&d].in_flight, 1, "{d:?}");
    }
    drop(p);
    let s = rig.budget.snapshot();
    assert_eq!(s.parts_in_flight, 0);
    assert_eq!(s.nodes[&NodeId(1)].in_flight, 0);
    assert!(s.devices.values().all(|v| v.in_flight == 0));
}

#[tokio::test(flavor = "current_thread")]
async fn a_charge_larger_than_the_ceiling_runs_alone() {
    // At the floor ceiling of 2, a degraded `k = 4` part waits for the ceiling to empty, then runs
    // alone; nothing else starts beside it.
    let rig = Rig::later();
    for _ in 0..3 {
        rig.network_window(None, 10);
    }
    rig.network_window(None, 40);
    rig.network_window(None, 40);
    assert_eq!(rig.budget.snapshot().ceiling, 2);
    let one = rig.acquire(bare(Class::Demand));
    let sources = [drive(1, 0), drive(1, 1), drive(2, 0), drive(2, 1)];
    let mut big = pin!(rig.budget.acquire(degraded(Class::Demand, 3, &sources)));
    assert!(poll!(big.as_mut()).is_pending());
    drop(one);
    let big = poll!(big.as_mut());
    assert!(big.is_ready());
    assert_eq!(rig.budget.snapshot().parts_in_flight, 4);
    assert!(rig.try_acquire(bare(Class::Demand)).is_none());
    drop(big);
    assert!(rig.try_acquire(bare(Class::Demand)).is_some());
}

#[test]
fn a_charge_naming_one_drive_twice_takes_two_of_its_permits() {
    let rig = Rig::later();
    let d = drive(3, 3);
    let c = Charge {
        class: Class::Demand,
        parts: 2,
        devices: vec![d, d],
        node: Some(NodeId(3)),
    };
    let p = rig.acquire(c);
    assert_eq!(rig.budget.snapshot().devices[&d].in_flight, 2);
    assert!(rig.try_acquire(part(Class::Demand, d)).is_none());
    drop(p);
    assert_eq!(rig.budget.snapshot().devices[&d].in_flight, 0);
}

#[test]
fn one_storage_node_down_doubles_drive_reads_and_triples_them_per_surviving_drive() {
    // Room for every part at once, so the charges are counted, not queued.
    // (A lone client at ten streams per drive has a share of 450 parts; the topology's
    // streams set only the share here.)
    let mut config = BudgetConfig::new(Topology {
        clients: 1,
        drive_streams: 10,
        ..support::later()
    });
    config.ceiling_parts = Some(1_000);
    config.drive_permits = 100;
    let reads = |down: Option<u8>| -> BTreeMap<DeviceRef, u32> {
        let rig = Rig::new(config.clone());
        let mut held = Vec::new();
        // Two parts on every drive (the healthy load at full share, s* per drive).
        for n in 1..=3u8 {
            for i in 0..15u16 {
                for _ in 0..2 {
                    let c = if Some(n) == down {
                        // A data shard on the node that is down: a degraded read of `k = 4` from
                        // four drives of the two survivors, spread evenly.
                        let base = u16::try_from(held.len() % 30).unwrap() / 2 * 2;
                        let pick = |j: u16| {
                            let s = (base + j) % 30;
                            let node = if s < 15 { 1 } else { 2 };
                            drive(node, s % 15)
                        };
                        let sources: Vec<_> = (0..4).map(pick).collect();
                        degraded(Class::Prepare, 1, &sources)
                    } else {
                        part(Class::Prepare, drive(n, i))
                    };
                    held.push(rig.acquire(c));
                }
            }
        }
        let s = rig.budget.snapshot();
        s.devices.iter().map(|(d, v)| (*d, v.in_flight)).collect()
    };
    let healthy = reads(None);
    let down = reads(Some(3));
    let total = |m: &BTreeMap<DeviceRef, u32>| m.values().sum::<u32>();
    assert_eq!(total(&healthy), 90);
    assert_eq!(
        total(&down),
        180,
        "2/3 x 1 + 1/3 x 4 = 2.0 times the healthy volume"
    );
    let surviving: u32 = down
        .iter()
        .filter(|(d, _)| d.node() != NodeId(3))
        .map(|(_, n)| n)
        .sum();
    assert_eq!(surviving, 180);
    // Per surviving drive: 6 against 2, three times the healthy load.
    for (d, n) in &down {
        if d.node() != NodeId(3) {
            assert_eq!(*n, 6, "{d:?}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_acquire_takes_nothing_and_leaves_no_reservation() {
    let rig = Rig::later();
    let d = drive(1, 1);
    let p1 = rig.acquire(part(Class::Prepare, d));
    let p2 = rig.acquire(part(Class::Demand, d));
    {
        let mut demand = pin!(rig.budget.acquire(part(Class::Demand, d)));
        assert!(poll!(demand.as_mut()).is_pending());
    }
    drop(p1);
    // The cancelled demand reserved nothing: prepare takes the freed permit.
    let p3 = rig.try_acquire(part(Class::Prepare, d));
    assert!(p3.is_some());
    let s = rig.budget.snapshot();
    assert_eq!((s.parts_in_flight, s.devices[&d].in_flight), (2, 2));
    drop((p2, p3));
    assert_eq!(rig.budget.snapshot().parts_in_flight, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_demand_at_the_ceiling_lets_prepare_in() {
    let rig = Rig::later();
    let held: Vec<Permit> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    {
        let mut demand = pin!(rig.budget.acquire(bare(Class::Demand)));
        assert!(poll!(demand.as_mut()).is_pending());
    }
    let mut held = held.into_iter();
    drop(held.next());
    drop(held.next());
    assert!(rig.try_acquire(bare(Class::Prepare)).is_some());
}

#[test]
fn a_charge_with_no_device_or_node_meets_the_ceiling_alone() {
    let rig = Rig::later();
    let held: Vec<Permit> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    let s = rig.budget.snapshot();
    assert!(s.devices.is_empty() && s.nodes.is_empty());
    assert!(rig.try_acquire(bare(Class::Demand)).is_none());
    drop(held);
    // A charge of zero parts counts as one.
    let zero = Charge {
        parts: 0,
        ..bare(Class::Demand)
    };
    let p = rig.acquire(zero);
    assert_eq!(rig.budget.snapshot().parts_in_flight, 1);
    drop(p);
}

#[test]
fn a_locate_answer_is_a_queue_wait_sample_of_a_device_with_traffic() {
    let rig = Rig::later();
    let d = drive(2, 2);
    // No traffic with `d`: a LOCATE answer leaves no state.
    rig.budget.observe_locate(d, &signals(Some(500), None));
    assert!(rig.budget.snapshot().devices.is_empty());
    rig.drive_window(d, 20, 10);
    // Two windows whose LOCATE answers say d's queue wait has risen to 60 ms; d's own answers
    // say nothing of it.
    for _ in 0..2 {
        for _ in 0..16 {
            rig.budget.observe_locate(d, &signals(Some(60), None));
            rig.observe(support::response(
                d,
                Some(20),
                signals(None, None),
                tierstore_fetch_api::budget::Outcome::Served,
            ));
        }
        rig.advance(PAST_COOLDOWN);
        rig.observe(support::response(
            d,
            Some(20),
            signals(None, None),
            tierstore_fetch_api::budget::Outcome::Served,
        ));
    }
    assert_eq!(rig.budget.snapshot().devices[&d].permits, 1);
    // A LOCATE answer naming the drive `stalled` is not a load signal.
    let e = drive(2, 3);
    rig.drive_window(e, 20, 10);
    for _ in 0..2 {
        for _ in 0..16 {
            rig.budget
                .observe_locate(e, &signals(Some(5_000), Some(DriveState::Stalled)));
            rig.served(e, 20, 10);
        }
        rig.advance(PAST_COOLDOWN);
        rig.served(e, 20, 10);
    }
    assert_eq!(rig.budget.snapshot().devices[&e].permits, 2);
}

#[test]
fn a_budget_is_shared_by_its_clones_and_debugs_by_name() {
    let rig = Rig::later();
    let other = rig.budget.clone();
    let p = other.acquire(bare(Class::Demand));
    let p = futures::FutureExt::now_or_never(p).expect("fits");
    assert_eq!(rig.budget.snapshot().parts_in_flight, 1);
    drop(p);
    assert_eq!(format!("{:?}", rig.budget), "ClientBudget { .. }");
}

#[test]
fn a_charge_larger_than_a_drive_or_node_runs_alone_there() {
    // A drive at one permit (after a 503) takes a charge naming it twice when idle, and nothing
    // beside it.
    let rig = Rig::new(BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    }));
    let d = drive(1, 0);
    rig.busy(d);
    assert_eq!(rig.budget.snapshot().devices[&d].permits, 1);
    let twice = Charge {
        class: Class::Demand,
        parts: 2,
        devices: vec![d, d],
        node: Some(NodeId(1)),
    };
    let one = rig.acquire(part(Class::Demand, d));
    assert!(
        rig.try_acquire(twice.clone()).is_none(),
        "not beside another part"
    );
    drop(one);
    let p = rig.acquire(twice);
    assert!(rig.try_acquire(part(Class::Demand, d)).is_none());
    drop(p);
    // A node limited to one part takes a `k = 4` degraded part when idle, and nothing beside it.
    let mut config = BudgetConfig::new(Topology {
        clients: 1,
        ..support::later()
    });
    config.node_permits = Some(1);
    let rig = Rig::new(config);
    let sources = [drive(1, 0), drive(1, 1), drive(2, 0), drive(2, 1)];
    let p = rig.acquire(degraded(Class::Demand, 3, &sources));
    assert_eq!(rig.budget.snapshot().nodes[&NodeId(3)].in_flight, 4);
    assert!(rig.try_acquire(part(Class::Demand, drive(3, 0))).is_none());
    drop(p);
    assert!(rig.try_acquire(part(Class::Demand, drive(3, 0))).is_some());
}

#[test]
fn with_a_node_down_its_failures_move_nothing_and_degraded_parts_meet_the_default_limits() {
    let rig = Rig::later();
    // Every drive of node 3 fails at the transport for several windows.
    for _ in 0..3 {
        for d in support::drives_of(3) {
            for _ in 0..16 {
                rig.observe(support::response(
                    d,
                    None,
                    signals(None, None),
                    tierstore_fetch_api::budget::Outcome::Transport,
                ));
            }
        }
        rig.advance(PAST_COOLDOWN);
    }
    let s = rig.budget.snapshot();
    assert!(
        s.devices.is_empty() && s.nodes.is_empty(),
        "no state from failures"
    );
    assert_eq!(
        (s.ceiling, s.ceiling_step),
        (3, tierstore_budget::LevelStep::None)
    );
    // Its data shards are read degraded from four drives of nodes 1 and 2: each is charged four
    // parts, so one runs at a time under LATER's ceiling of 3, with one permit on each source.
    let a = [drive(1, 0), drive(1, 1), drive(2, 0), drive(2, 1)];
    let b = [drive(1, 2), drive(1, 3), drive(2, 2), drive(2, 3)];
    let pa = rig.acquire(degraded(Class::Demand, 1, &a));
    assert!(rig.try_acquire(degraded(Class::Demand, 2, &b)).is_none());
    assert!(rig.try_acquire(part(Class::Demand, drive(1, 9))).is_none());
    let s = rig.budget.snapshot();
    assert_eq!(s.parts_in_flight, 4);
    for d in a {
        assert_eq!(s.devices[&d].in_flight, 1, "{d:?}");
    }
    drop(pa);
    let pb = rig.acquire(degraded(Class::Demand, 2, &b));
    drop(pb);
}

#[tokio::test(flavor = "current_thread")]
async fn a_demand_waiting_at_the_ceiling_registers_on_a_device_new_to_the_client() {
    let rig = Rig::later();
    let held: Vec<Permit> = (0..3).map(|_| rig.acquire(bare(Class::Demand))).collect();
    let d = drive(2, 9);
    {
        let mut demand = pin!(rig.budget.acquire(part(Class::Demand, d)));
        assert!(poll!(demand.as_mut()).is_pending());
        assert_eq!(rig.budget.snapshot().devices[&d].demand_waiting, 1);
    }
    assert_eq!(rig.budget.snapshot().devices[&d].demand_waiting, 0);
    drop(held);
}
