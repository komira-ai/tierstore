//! Section 14 row "Group commit (fake NVMe, injected clock)": no timer wait; a batch never
//! exceeds `batch_max`; every small PUT is acknowledged only after its batch is durable on
//! `min_acks` copies; a crash before the sync leaves none of the batch visible after restart; at
//! most one `fdatasync` per batch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Kind, Sim, config};
use tierstore_format::names::HotName;
use tierstore_harness::disk::{Fault, OpKind};
use tierstore_store_api::index::{Index, Lookup};

#[test]
fn records_queued_while_a_batch_syncs_go_out_together_in_the_next_one() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    sim.net.hold(Some(1), 2, Kind::Replicate);
    let first = sim.append(1, &key, b"first");
    sim.run();
    assert!(
        !first.done(),
        "acknowledged before the replicas made it durable"
    );
    let rest: Vec<_> = (0..5)
        .map(|i| {
            let k = sim.key_on(1, &format!("b{i}"));
            sim.append(1, &k, format!("record {i}").as_bytes())
        })
        .collect();
    sim.run();
    assert!(rest.iter().all(|h| !h.done()));
    // The first batch reaches node 2; the five queued records then go out as one batch.
    sim.net.unhold();
    while !sim.net.held().is_empty() {
        sim.net.release(0);
        sim.run();
    }
    assert!(first.take().unwrap().is_ok());
    for h in rest {
        assert_eq!(h.take().unwrap().unwrap().copies, 3);
    }
    assert_eq!(
        sim.net.batches(1, 2).len(),
        2,
        "one batch, then one with the five queued"
    );
    assert_eq!(sim.net.batches(1, 3).len(), 2);
    let seg = sim.node(1).open_segments()[0].id;
    let path = HotName::Segment(seg).path();
    assert_eq!(sim.disk(1).fdatasyncs(&path), 2, "one fdatasync per batch");
    assert_eq!(sim.disk(2).fdatasyncs(&path), 2);
}

#[test]
fn a_batch_never_exceeds_batch_max() {
    let mut cfg = config(2, 2);
    cfg.batch_max = 8 << 10;
    let mut sim = Sim::new(2, cfg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    let key0 = sim.key_on(1, "first");
    let first = sim.append(1, &key0, b"x");
    sim.run();
    let data = vec![7u8; 3000];
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let k = format!("k{i}");
            sim.append(1, &k, &data)
        })
        .collect();
    sim.run();
    sim.net.unhold();
    while !sim.net.held().is_empty() {
        sim.net.release(0);
        sim.run();
    }
    assert!(first.take().unwrap().is_ok());
    for h in handles {
        assert!(h.take().unwrap().is_ok());
    }
    let batches = sim.net.batches(1, 2);
    assert!(
        batches.len() >= 5,
        "12 records of 3 KiB need at least 5 batches of 8 KiB"
    );
    assert!(
        batches.iter().all(|(_, len)| *len <= 8 << 10),
        "{batches:?}"
    );
}

#[test]
fn a_put_waits_for_min_acks_durable_copies_and_no_timer() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "k");
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let h = sim.append(1, &key, b"data");
    sim.run();
    assert!(!h.done(), "the writer's own copy is one of two");
    assert_eq!(sim.net.held().len(), 2);
    sim.net.release(0);
    sim.run();
    let acked = h.take().unwrap().unwrap();
    assert_eq!(acked.copies, 2);
    assert!(matches!(
        sim.node(1).index().get(key.as_bytes()).unwrap(),
        Lookup::Live(_)
    ));
}

#[test]
fn a_crash_before_the_sync_leaves_none_of_the_batch_visible_after_restart() {
    let mut sim = Sim::new(1, config(1, 1));
    let ok = sim.append(1, "kept", b"kept");
    sim.run();
    assert!(ok.take().unwrap().is_ok());
    sim.disk(1).inject(OpKind::Fdatasync, 0, Fault::CrashBefore);
    let lost = sim.append(1, "lost", b"lost");
    let also = sim.append(1, "also", b"also lost");
    sim.run();
    assert!(lost.take().unwrap().is_err());
    assert!(also.take().map_or(true, |r| r.is_err()));
    sim.crash(1);
    sim.restart(1);
    let index = sim.node(1).index().clone();
    assert!(matches!(index.get(b"kept").unwrap(), Lookup::Live(_)));
    assert_eq!(index.get(b"lost").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"also").unwrap(), Lookup::Absent);
}
