//! `Budget` types (`budget`).
//!
//! Proves a `Permit` runs its release exactly once, when it is dropped (not before, not twice),
//! that its `Debug` shows no closure, and that the charge and observation types compare by what they
//! carry.
//!
//! Catches:
//! - a permit that releases at creation, on a move, or never;
//! - a permit that releases twice (a double return of permits to the controller);
//! - a `Debug` that prints more than the type's name.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tierstore_fetch_api::budget::{Charge, Observation, Outcome, Permit};
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals};
use tierstore_format::ids::NodeId;

fn counted() -> (Arc<AtomicUsize>, Permit) {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let p = Permit::new(move || {
        n2.fetch_add(1, Ordering::SeqCst);
    });
    (n, p)
}

#[test]
fn a_permit_releases_exactly_once_when_it_is_dropped() {
    let (n, p) = counted();
    assert_eq!(n.load(Ordering::SeqCst), 0, "not released by being made");
    let moved = p;
    assert_eq!(n.load(Ordering::SeqCst), 0, "not released by a move");
    drop(moved);
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn every_permit_of_a_set_releases_once() {
    let n = Arc::new(AtomicUsize::new(0));
    let permits: Vec<Permit> = (0..5)
        .map(|_| {
            let n = n.clone();
            Permit::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();
    assert_eq!(n.load(Ordering::SeqCst), 0);
    drop(permits);
    assert_eq!(n.load(Ordering::SeqCst), 5);
}

#[test]
fn a_permit_debug_is_its_name_only() {
    let (_, p) = counted();
    assert_eq!(format!("{p:?}"), "Permit { .. }");
}

#[test]
fn a_charge_compares_by_class_parts_devices_and_node() {
    let c = Charge {
        class: Class::Demand,
        parts: 4,
        devices: vec![DeviceRef::Hot(NodeId(1)), DeviceRef::Hot(NodeId(2))],
        node: Some(NodeId(1)),
    };
    let mut other = c.clone();
    assert_eq!(c, other);
    other.parts = 1;
    assert_ne!(c, other);
}

#[test]
fn observations_compare_by_what_they_carry() {
    let resp = Observation::Response {
        device: DeviceRef::Hot(NodeId(3)),
        size_class: 3,
        first_byte_ms: Some(12),
        signals: DriveSignals::default(),
        outcome: Outcome::Served,
    };
    assert_eq!(resp.clone(), resp);
    assert_ne!(
        resp,
        Observation::Response {
            device: DeviceRef::Hot(NodeId(3)),
            size_class: 3,
            first_byte_ms: None,
            signals: DriveSignals::default(),
            outcome: Outcome::Busy,
        }
    );
    let net = Observation::Network {
        node: None,
        ms_per_mib: 8,
    };
    assert_ne!(net, Observation::ClientsActive(8));
    assert_eq!(Observation::ClientsActive(2), Observation::ClientsActive(2));
    for (a, b) in [
        (Outcome::Served, Outcome::Busy),
        (Outcome::NotLoad, Outcome::Transport),
    ] {
        assert_ne!(a, b);
    }
}
