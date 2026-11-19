//! `reserve` at job assignment (section 7): the 0.9 x budget rule, the `oversize` and `enospc`
//! refusals, each counted by reason, reservations released when dropped, and a held-only cache
//! that admits a new job because punch eviction makes the room.
//!
//! Section 14 row: "Held-only cache: punch eviction admits a new job; admission refuses when
//! estimate exceeds 0.9 x budget".

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::*;
use tierstore_evict::{Admission, CacheDevice, EvictConfig, ReserveError, Statvfs};
use tierstore_fetch_api::cell::CellState::{Absent, Present};
use tierstore_fetch_api::entry::{Entry, EntryStore};
use tierstore_fetch_api::ids::HolderId;
use tierstore_metrics::Cache;
use tierstore_metrics::labels::AdmissionReason;

/// A budget of 100 cells: jobs may hold 90.
const BUDGET: u64 = 100 * MIB;

fn admission(store: Arc<dyn EntryStore>, free: u64) -> (Admission, Arc<Cache>) {
    let m = metrics();
    let a = Admission::new(
        store,
        FakeDevice::new(free),
        EvictConfig::new(BUDGET),
        m.clone(),
    );
    (a, m)
}

fn refusals(m: &Cache, reason: AdmissionReason) -> u64 {
    m.admission_refusals_total.with(reason).get()
}

#[test]
fn jobs_are_admitted_up_to_nine_tenths_of_the_budget_and_no_further() {
    let (a, m) = admission(TestStore::new(), BUDGET);
    let first = a.reserve(60 * MIB).unwrap();
    assert_eq!(first.bytes(), 60 * MIB);
    // Exactly at the limit is admitted.
    let second = a.reserve(30 * MIB).unwrap();
    assert_eq!(a.held_bytes(), 90 * MIB);
    assert_eq!(m.held_bytes.with(()).get(), (90 * MIB) as f64);

    let e = a.reserve(1).unwrap_err();
    assert!(matches!(
        e,
        ReserveError::Budget { estimate: 1, held, limit } if held == 90 * MIB && limit == 90 * MIB
    ));
    assert_eq!(e.reason(), Some(AdmissionReason::Budget));
    assert_eq!(refusals(&m, AdmissionReason::Budget), 1);
    assert_eq!(a.held_bytes(), 90 * MIB, "a refusal charges nothing");

    // A job that ends releases its bytes.
    drop(first);
    assert_eq!(a.held_bytes(), 30 * MIB);
    assert_eq!(m.held_bytes.with(()).get(), (30 * MIB) as f64);
    let third = a.reserve(60 * MIB).unwrap();
    drop((second, third));
    assert_eq!(a.held_bytes(), 0);
    assert_eq!(m.held_bytes.with(()).get(), 0.0);
}

#[test]
fn a_needed_set_larger_than_the_budget_is_refused_as_oversize() {
    let (a, m) = admission(TestStore::new(), BUDGET);
    let e = a.reserve(BUDGET + 1).unwrap_err();
    assert!(
        matches!(e, ReserveError::Oversize { estimate, budget } if estimate == BUDGET + 1 && budget == BUDGET)
    );
    assert_eq!(e.reason(), Some(AdmissionReason::Oversize));
    assert_eq!(refusals(&m, AdmissionReason::Oversize), 1);
    // The whole budget is not oversize, but it is over 0.9 of it.
    let e = a.reserve(BUDGET).unwrap_err();
    assert_eq!(e.reason(), Some(AdmissionReason::Budget));
    assert_eq!(refusals(&m, AdmissionReason::Oversize), 1);
    // A job needing nothing new is always admitted.
    assert_eq!(a.reserve(0).unwrap().bytes(), 0);
}

#[test]
fn a_device_that_cannot_hold_the_reserved_bytes_refuses_as_enospc() {
    let store = TestStore::new();
    // 10 cells cached; with 20 free the device can give the cache 30.
    store.insert(mem(1, 10, 0..10));
    let (a, m) = admission(store, 20 * MIB);
    let _held = a.reserve(25 * MIB).unwrap();
    let ok = a.reserve(5 * MIB).unwrap();
    let e = a.reserve(1).unwrap_err();
    assert!(matches!(
        e,
        ReserveError::Enospc { estimate: 1, held, available } if held == 30 * MIB && available == 30 * MIB
    ));
    assert_eq!(e.reason(), Some(AdmissionReason::Enospc));
    assert_eq!(refusals(&m, AdmissionReason::Enospc), 1);
    assert_eq!(refusals(&m, AdmissionReason::Budget), 0);
    drop(ok);
    assert!(a.reserve(5 * MIB).is_ok());
}

#[test]
fn a_device_whose_space_cannot_be_read_is_an_error_not_a_refusal() {
    let m = metrics();
    let a = Admission::new(
        TestStore::new(),
        FakeDevice::failing(),
        EvictConfig::new(BUDGET),
        m.clone(),
    );
    let e = a.reserve(MIB).unwrap_err();
    assert_eq!(
        e.to_string(),
        "reading the free space of the cache device: statvfs failed"
    );
    assert_eq!(e.reason(), None);
    for r in [
        AdmissionReason::Budget,
        AdmissionReason::Enospc,
        AdmissionReason::Oversize,
    ] {
        assert_eq!(refusals(&m, r), 0);
    }
    assert_eq!(a.held_bytes(), 0);
}

#[test]
fn a_held_only_cache_admits_a_new_job_and_punch_eviction_makes_its_room() {
    // The cache is full: 96 cells, all in entries mounts hold. The running jobs reserved what
    // they needed of them, 40 cells; the rest of the held cells are cold.
    let store = TestStore::new();
    let mut held = Vec::new();
    for n in 1..=3 {
        let e = mem(n, 32, 0..32);
        e.hold(HolderId(u64::from(n)));
        e.touch(u64::from(n) * 1_000, 0);
        e.touch(u64::from(n) * 10_000, 1);
        store.insert(e.clone());
        held.push(e);
    }
    let (a, m) = admission(store.clone(), 4 * MIB);
    let _running = a.reserve(40 * MIB).unwrap();

    // A new job needing 40 more cells fits 0.9 x 100 with the 40 held.
    let job = a.reserve(40 * MIB).unwrap();
    // One needing 11 more beyond that does not.
    assert_eq!(
        a.reserve(11 * MIB).unwrap_err().reason(),
        Some(AdmissionReason::Budget)
    );
    assert_eq!(refusals(&m, AdmissionReason::Budget), 1);

    // The evictor makes the new job's room by punching the held entries' coldest groups: no
    // held entry is removed.
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(BUDGET - job.bytes()).unwrap();
    assert_eq!(r.entries_removed, 0);
    assert!(store.removals.lock().unwrap().is_empty());
    assert_eq!(r.cells_punched, 48);
    assert!(r.used_after <= BUDGET - job.bytes());
    // The cold groups (group 0 of each) went first.
    for e in &held {
        assert_eq!(states(e.as_ref(), [0, 15]), [Absent, Absent]);
        assert_eq!(state(e.as_ref(), 16), Present);
        assert_eq!(e.holders(), 1);
    }
}

#[test]
fn statvfs_reads_the_free_space_of_the_cache_directory() {
    let dir = scratch("statvfs");
    assert!(Statvfs::new(dir.path()).free_bytes().unwrap() > 0);
    let missing = Statvfs::new(dir.path().join("missing"));
    assert_eq!(
        missing.free_bytes().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn the_admit_limit_rounds_down_and_does_not_overflow() {
    assert_eq!(EvictConfig::new(1000).admit_limit(), 900);
    assert_eq!(EvictConfig::new(999).admit_limit(), 899);
    assert_eq!(
        EvictConfig::new(u64::MAX).admit_limit(),
        (u128::from(u64::MAX) * 9 / 10) as u64
    );
    let c = EvictConfig {
        admit_permille: 2000,
        ..EvictConfig::new(u64::MAX)
    };
    assert_eq!(c.admit_limit(), u64::MAX);
}

#[test]
fn refusals_and_the_account_say_what_happened() {
    let budget = ReserveError::Budget {
        estimate: 3,
        held: 4,
        limit: 5,
    };
    assert_eq!(
        budget.to_string(),
        "4 bytes held and 3 bytes needed exceed 5 bytes, 0.9 of the budget"
    );
    let oversize = ReserveError::Oversize {
        estimate: 7,
        budget: 6,
    };
    assert_eq!(
        oversize.to_string(),
        "7 bytes needed exceed the cache budget of 6 bytes"
    );
    let enospc = ReserveError::Enospc {
        estimate: 1,
        held: 2,
        available: 2,
    };
    assert_eq!(
        enospc.to_string(),
        "2 bytes held and 1 bytes needed exceed the 2 bytes the cache device can give"
    );
    let (a, _) = admission(TestStore::new(), BUDGET);
    let r = a.reserve(3).unwrap();
    assert_eq!(
        format!("{a:?}"),
        "Admission { config: EvictConfig { budget_bytes: 104857600, admit_permille: 900, evict_wait_ms: 2000, grace_ms: 60000 }, held: 3, .. }"
    );
    assert!(format!("{r:?}").contains("bytes: 3"));
    let c = a.clone();
    drop(r);
    assert_eq!(c.held_bytes(), 0, "clones share one account");
}
