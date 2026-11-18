//! The quantities of 4.3 (section 14 row "Budget: floor, share, ceiling and bytes in flight equal
//! the table of 4.3 for (D, N, s*, P) and are recomputed when P changes; share is at least one
//! part at every P"), for both deployment profiles of section 21, with and without the
//! jobscheduler's hint.
//!
//! Catches:
//! - a share, floor, ceiling or bytes-in-flight figure that differs from the table;
//! - bytes in flight not recomputed when `P` steps down;
//! - a share of zero parts (many clients, or an install with no cold tier);
//! - a stale or missing hint used in place of `N`, or a fresh one ignored;
//! - a demand reserve that is not `max(1, 25%)` of the ceiling, or that takes the whole ceiling.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{MIB, Rig, later, now_profile};
use tierstore_budget::quantities::reserve;
use tierstore_budget::{BudgetConfig, Quantities, Topology};
use tierstore_fetch_api::budget::{Budget, Observation};

#[test]
fn later_equals_the_table_of_4_3() {
    let config = BudgetConfig::new(later());
    let q = Quantities::compute(&config, 31, 64 * MIB);
    assert_eq!(
        q,
        Quantities {
            clients_active: 31,
            stream_capacity: 90,
            share: 3,
            floor_parts: 1,
            floor_bytes: 16 * MIB,
            ceiling_parts: 6,
            demand_reserve: 2,
            part_size: 64 * MIB,
            bytes_in_flight_at_share: 192 * MIB,
            drive_permits: 2,
            node_permits: None,
        }
    );
    // Across the 31 clients: 496 MiB of floors, and about 5.8 GiB in flight at share.
    assert_eq!(q.floor_bytes * 31, 496 * MIB);
    assert_eq!(q.bytes_in_flight_at_share * 31, 5_952 * MIB);
    // At P = 16 MiB: 48 MiB per client.
    let q16 = Quantities::compute(&config, 31, 16 * MIB);
    assert_eq!(q16.bytes_in_flight_at_share, 48 * MIB);
    assert_eq!(q16.share, 3, "the share is in parts, whatever P is");
}

#[test]
fn now_profile_quantities() {
    // Twelve SMR drives at one stream, six clients: G = 12, share 2, ceiling 4, one permit per
    // drive.
    let q = Quantities::compute(&BudgetConfig::new(now_profile()), 6, 64 * MIB);
    assert_eq!(
        (
            q.stream_capacity,
            q.share,
            q.ceiling_parts,
            q.drive_permits,
            q.demand_reserve
        ),
        (12, 2, 4, 1, 1)
    );
    assert_eq!(q.bytes_in_flight_at_share, 128 * MIB);
}

#[test]
fn the_share_is_at_least_one_part() {
    let config = BudgetConfig::new(later());
    for clients in [91, 1_000, u32::MAX] {
        for p in [16 * MIB, 32 * MIB, 64 * MIB] {
            let q = Quantities::compute(&config, clients, p);
            assert_eq!(q.share, 1, "{clients} clients at P = {p}");
            assert_eq!(q.ceiling_parts, 2);
            assert_eq!(q.bytes_in_flight_at_share, p);
        }
    }
    // A hint of zero active clients divides by one.
    assert_eq!(Quantities::compute(&config, 0, 64 * MIB).clients_active, 1);
    assert_eq!(Quantities::compute(&config, 0, 64 * MIB).share, 90);
}

#[test]
fn an_install_without_a_cold_tier_shares_its_hot_queue_depth() {
    let hot_only = Topology {
        cold_drives: 0,
        hot_devices: 3,
        clients: 31,
        drive_streams: 0,
        hot_queue_depth: 64,
    };
    let q = Quantities::compute(&BudgetConfig::new(hot_only), 31, 64 * MIB);
    assert_eq!((q.stream_capacity, q.share, q.ceiling_parts), (192, 7, 14));
    // No device at all: the share is still one part.
    let no_device = Topology {
        hot_devices: 0,
        ..hot_only
    };
    let q = Quantities::compute(&BudgetConfig::new(no_device), 31, 64 * MIB);
    assert_eq!((q.stream_capacity, q.share, q.ceiling_parts), (0, 1, 2));
}

#[test]
fn a_measured_knee_replaces_the_starting_ceiling() {
    let mut config = BudgetConfig::new(later());
    config.ceiling_parts = Some(9);
    config.node_permits = Some(20);
    let q = Quantities::compute(&config, 31, 64 * MIB);
    assert_eq!(
        (q.ceiling_parts, q.demand_reserve, q.node_permits),
        (9, 3, Some(20))
    );
}

#[test]
fn the_demand_reserve_is_a_quarter_of_the_ceiling_and_at_least_one_part() {
    let mut config = BudgetConfig::new(later());
    let got: Vec<u32> = [2, 3, 4, 5, 6, 8, 9, 100]
        .iter()
        .map(|&c| reserve(&config, c))
        .collect();
    assert_eq!(got, vec![1, 1, 1, 2, 2, 2, 3, 25]);
    // Never the whole ceiling: `prepare` keeps one part.
    config.reserve_percent = 100;
    assert_eq!(reserve(&config, 6), 5);
    assert_eq!(reserve(&config, 2), 1);
    config.reserve_percent = 0;
    assert_eq!(reserve(&config, 6), 1);
}

#[test]
fn the_jobscheduler_hint_divides_the_share_while_fresh() {
    let rig = Rig::later();
    assert_eq!(rig.budget.quantities().clients_active, 31, "no hint: N");
    rig.observe(Observation::ClientsActive(10));
    let q = rig.budget.quantities();
    assert_eq!((q.clients_active, q.share, q.ceiling_parts), (10, 9, 18));
    rig.advance(10_000);
    assert_eq!(rig.budget.quantities().share, 9, "10 s old is still fresh");
    rig.advance(1);
    let q = rig.budget.quantities();
    assert_eq!((q.clients_active, q.share), (31, 3), "older than 10 s: N");
    // A newer hint replaces the older one.
    rig.observe(Observation::ClientsActive(45));
    assert_eq!(rig.budget.quantities().share, 2);
}

#[test]
fn bytes_in_flight_follow_p_and_part_size_follows_the_size_class() {
    let rig = Rig::later();
    assert_eq!(rig.budget.part_size(10 * MIB), 32 * MIB);
    assert_eq!(rig.budget.part_size(32 * MIB), 32 * MIB);
    assert_eq!(rig.budget.part_size(512 * MIB), 32 * MIB);
    assert_eq!(rig.budget.part_size(512 * MIB + 1), 64 * MIB);
    assert_eq!(rig.budget.part_size(u64::MAX), 64 * MIB);
    assert_eq!(rig.budget.quantities().bytes_in_flight_at_share, 192 * MIB);
}
