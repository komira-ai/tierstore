//! Section 14 rows "Seal and roll" and "Seal triggers". Seal and roll: a replica silent for
//! `replica_stall` (injected clock) makes the writer seal early by majority and open a new
//! segment on live nodes, `hot_under_replicated_blocks` counts the early-sealed block, a member
//! `down` in the view does the same, and with fewer than `min_acks` live nodes small PUTs answer
//! 503. Seal triggers: a segment seals at `seal_bytes`; at `seal_max_age` only at or above
//! `seal_min_bytes`; below that floor at `destage.max_age`; an empty segment never seals; with
//! three nodes and one expiry class there are exactly three open segments, and with eight
//! classes at most 24.
//!
//! Catches: a stalled replica that stays in the next segment's replica set; a stall or a down
//! member that does not close the segment; a seal before the stall, age or floor boundary, or
//! one a tick late; the age rule applied without the size floor; a sealed block longer than
//! `block_bytes`; an empty segment sealed by age; more open segments than writer, replica set
//! and class give; a seal counted under the wrong reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Kind, Sim, config};
use tierstore_format::ids::{BlockId, ExpiryClass, NodeId};
use tierstore_hot::node::SegmentInfo;
use tierstore_index::blocks::BlockState;
use tierstore_metrics::labels::SealReason;
use tierstore_store_api::hot::{HotError, Unavailable};
use tierstore_store_api::index::{Index, Lookup};
use tierstore_store_api::view::{Admin, NodeState};

const DAY_MS: u64 = 24 * 3600 * 1000;

/// How many seals of each reason node `id` counted: size, age, early, take over.
fn seals(sim: &Sim, id: u8) -> [u64; 4] {
    let m = sim.metrics(id);
    [
        SealReason::Size,
        SealReason::Age,
        SealReason::Early,
        SealReason::TakeOver,
    ]
    .map(|r| m.hot_seal_total.with(r).get())
}

/// Appends `n` records of `len` bytes, each acknowledged before the next starts, to node `id`.
fn fill(sim: &mut Sim, id: u8, tag: &str, n: usize, len: usize) {
    for i in 0..n {
        let key = sim.key_on(id, &format!("{tag}{i}"));
        let data = vec![(i % 251) as u8; len];
        let h = sim.append(id, &key, &data);
        sim.run();
        h.take().expect("finished").expect("acknowledged");
    }
}

fn only_open(sim: &Sim, id: u8) -> SegmentInfo {
    let open = sim.node(id).open_segments();
    assert_eq!(open.len(), 1, "{open:?}");
    open.into_iter().next().unwrap()
}

fn block_state(sim: &Sim, id: u8, block: BlockId) -> Option<BlockState> {
    sim.node(id).index().block(block).unwrap().map(|b| b.state)
}

// ---------------------------------------------------------------------------------------------
// Seal and roll.

#[test]
fn a_replica_silent_for_replica_stall_makes_the_writer_seal_early_and_roll_to_live_nodes() {
    let mut sim = Sim::new(3, config(3, 2));
    sim.net.hold(Some(1), 2, Kind::Replicate);
    let first = sim.key_on(1, "a");
    let h = sim.append(1, &first, b"before the stall");
    sim.run();
    assert_eq!(
        h.take().unwrap().unwrap().copies,
        2,
        "node 3 and the writer"
    );
    let old = only_open(&sim, 1);
    assert_eq!(old.set, vec![NodeId(1), NodeId(2), NodeId(3)]);

    // One millisecond short of the stall: nothing closes.
    sim.clock.advance(sim.cfg.replica_stall_ms - 1);
    let report = sim.node(1).tick();
    sim.run();
    assert!(report.early.is_empty(), "{report:?}");
    assert_eq!(only_open(&sim, 1).id, old.id);
    assert_eq!(seals(&sim, 1), [0, 0, 0, 0]);

    // At the stall: the segment seals early by majority and is no longer open.
    sim.clock.advance(1);
    let report = sim.node(1).tick();
    sim.run();
    assert_eq!(report.early, vec![old.id]);
    assert!(sim.node(1).open_segments().is_empty());
    assert_eq!(seals(&sim, 1), [0, 0, 1, 0]);
    assert_eq!(sim.metrics(1).hot_early_seals_total.with(()).get(), 1);
    assert_eq!(block_state(&sim, 1, old.id), Some(BlockState::Sealed));
    assert_eq!(block_state(&sim, 3, old.id), Some(BlockState::Sealed));
    assert!(matches!(
        sim.node(1).index().get(first.as_bytes()).unwrap(),
        Lookup::Live(_)
    ));

    // The early-sealed block has two copies of three: it is counted until re-replicated.
    assert_eq!(sim.node(1).under_replicated(), vec![old.id]);
    assert_eq!(
        sim.metrics(1).hot_under_replicated_blocks.with(()).get(),
        1.0
    );

    // New PUTs go to a new segment of the live nodes, and are acknowledged at once.
    let second = sim.key_on(1, "b");
    let h = sim.append(1, &second, b"after the stall");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 2);
    let new = only_open(&sim, 1);
    assert_ne!(new.id, old.id);
    assert_eq!(
        new.set,
        vec![NodeId(1), NodeId(3)],
        "the silent node is left out"
    );
}

#[test]
fn a_member_set_down_in_the_view_makes_the_tick_seal_early_and_roll_without_it() {
    let mut sim = Sim::new(3, config(3, 2));
    let first = sim.key_on(1, "a");
    let h = sim.append(1, &first, b"on three copies");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 3);
    let old = only_open(&sim, 1);

    // Node 3 goes down: the view says so and it answers nobody.
    sim.set_state(3, NodeState::Down);
    sim.net.cut_both(1, 3);
    sim.net.cut_both(2, 3);
    let report = sim.node(1).tick();
    sim.run();
    assert_eq!(
        report.early,
        vec![old.id],
        "no clock advance is needed for a down member"
    );
    assert_eq!(seals(&sim, 1), [0, 0, 1, 0]);
    assert_eq!(block_state(&sim, 1, old.id), Some(BlockState::Sealed));
    assert_eq!(block_state(&sim, 2, old.id), Some(BlockState::Sealed));
    assert_eq!(sim.node(1).under_replicated(), vec![old.id]);
    assert_eq!(
        sim.metrics(1).hot_under_replicated_blocks.with(()).get(),
        1.0
    );

    let second = sim.key_on(1, "b");
    let h = sim.append(1, &second, b"after");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 2);
    let new = only_open(&sim, 1);
    assert_ne!(new.id, old.id);
    assert_eq!(new.set, vec![NodeId(1), NodeId(2)]);
    // Ticking again changes nothing: the new segment has no down member.
    assert!(sim.node(1).tick().early.is_empty());
}

#[test]
fn a_member_set_out_of_the_hot_tier_makes_the_tick_seal_early() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "a");
    let h = sim.append(1, &key, b"data");
    sim.run();
    h.take().unwrap().unwrap();
    let old = only_open(&sim, 1);
    for n in &mut sim.view.nodes {
        if n.id == NodeId(2) {
            n.hot = Admin::Out;
        }
    }
    for i in 1..=3 {
        sim.node(i).set_view(sim.view.clone());
    }
    let report = sim.node(1).tick();
    sim.run();
    assert_eq!(report.early, vec![old.id]);
    assert_eq!(seals(&sim, 1), [0, 0, 1, 0]);
    assert_eq!(block_state(&sim, 1, old.id), Some(BlockState::Sealed));
    let k2 = sim.key_on(1, "b");
    let h = sim.append(1, &k2, b"more");
    sim.run();
    h.take().unwrap().unwrap();
    assert_eq!(only_open(&sim, 1).set, vec![NodeId(1), NodeId(3)]);
}

#[test]
fn with_fewer_than_min_acks_live_nodes_small_puts_answer_503_too_few_copies() {
    let mut sim = Sim::new(3, config(3, 2));
    sim.set_state(2, NodeState::Down);
    // Two live nodes are enough for min_acks = 2.
    let ok = sim.key_on(1, "ok");
    let h = sim.append(1, &ok, b"two live");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 2);
    sim.set_state(3, NodeState::Down);
    let key = sim.key_on(1, "alone");
    let h = sim.append(1, &key, b"one live");
    sim.run();
    match h.take().unwrap() {
        Err(HotError::Unavailable {
            reason,
            retry_after_ms,
        }) => {
            assert_eq!(reason, Unavailable::TooFewCopies);
            assert!(retry_after_ms > 0);
        }
        other => panic!("expected 503 TooFewCopies, got {other:?}"),
    }
    assert_eq!(
        sim.node(1).index().get(key.as_bytes()).unwrap(),
        Lookup::Absent
    );
}

// ---------------------------------------------------------------------------------------------
// Seal triggers.

#[test]
fn a_record_that_does_not_fit_seals_the_segment_at_seal_bytes_and_opens_the_next() {
    let mut cfg = config(3, 3);
    cfg.seal_bytes = 100 << 10;
    cfg.seal_min_bytes = 64 << 10;
    let mut sim = Sim::new(3, cfg);
    fill(&mut sim, 1, "r", 2, 40_000);
    let old = only_open(&sim, 1);
    assert_eq!(old.records, 2);
    assert_eq!(seals(&sim, 1), [0, 0, 0, 0]);
    // 80 KB are in; a third of 40 KB does not fit below 100 KiB.
    fill(&mut sim, 1, "s", 1, 40_000);
    assert_eq!(seals(&sim, 1), [1, 0, 0, 0]);
    assert_eq!(block_state(&sim, 1, old.id), Some(BlockState::Sealed));
    let new = only_open(&sim, 1);
    assert_ne!(new.id, old.id);
    assert_eq!(
        new.records, 1,
        "the record that did not fit starts the next segment"
    );
    let sealed = sim.node(1).index().block(old.id).unwrap().unwrap();
    assert!(
        sealed.length <= sim.cfg.geometry.block_bytes(),
        "{}",
        sealed.length
    );
    assert!(
        sim.node(1).under_replicated().is_empty(),
        "a size seal has every copy"
    );
}

#[test]
fn a_sealed_block_is_never_longer_than_block_bytes() {
    let mut sim = Sim::new(3, config(3, 3));
    // 2.4 MB of records of 60 000 bytes cross the default seal_bytes of 2 MiB - 256 KiB.
    fill(&mut sim, 1, "big", 40, 60_000);
    let counts = seals(&sim, 1);
    assert!(counts[0] >= 1, "{counts:?}");
    assert_eq!(counts[1..], [0, 0, 0]);
    let block_bytes = sim.cfg.geometry.block_bytes();
    let mut checked = 0;
    for counter in 0..64 {
        let id = BlockId::new(NodeId(1), counter).unwrap();
        if let Some(info) = sim.node(1).index().block(id).unwrap()
            && info.state == BlockState::Sealed
        {
            assert!(
                info.length <= block_bytes,
                "{} > {block_bytes}",
                info.length
            );
            assert!(
                info.length > sim.cfg.seal_bytes / 2,
                "sealed far below seal_bytes"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, counts[0]);
}

/// The bytes one record of `len` bytes takes in a segment: what the writer reports acknowledged.
fn one_record_bytes(len: usize) -> u64 {
    let mut sim = Sim::new(3, config(3, 3));
    fill(&mut sim, 1, "m", 1, len);
    only_open(&sim, 1).acked
}

#[test]
fn a_segment_at_or_above_seal_min_bytes_seals_at_seal_max_age_and_not_a_millisecond_before() {
    let bytes = one_record_bytes(50_000);
    let mut cfg = config(3, 3);
    cfg.seal_min_bytes = bytes;
    let mut sim = Sim::new(3, cfg);
    fill(&mut sim, 1, "m", 1, 50_000);
    let seg = only_open(&sim, 1);
    assert_eq!(seg.acked, bytes, "exactly at the floor");
    sim.clock.advance(sim.cfg.seal_max_age_ms - 1);
    let report = sim.node(1).tick();
    sim.run();
    assert!(report.aged.is_empty(), "{report:?}");
    assert_eq!(only_open(&sim, 1).id, seg.id);
    sim.clock.advance(1);
    let report = sim.node(1).tick();
    sim.run();
    assert_eq!(report.aged, vec![seg.id]);
    assert!(sim.node(1).open_segments().is_empty());
    assert_eq!(seals(&sim, 1), [0, 1, 0, 0]);
    for i in 1..=3 {
        assert_eq!(
            block_state(&sim, i, seg.id),
            Some(BlockState::Sealed),
            "node {i}"
        );
    }
    assert!(sim.node(1).under_replicated().is_empty());
}

#[test]
fn a_segment_below_seal_min_bytes_waits_for_destage_max_age_and_seals_at_it() {
    let bytes = one_record_bytes(50_000);
    let mut cfg = config(3, 3);
    cfg.seal_min_bytes = bytes + 1;
    let mut sim = Sim::new(3, cfg);
    fill(&mut sim, 1, "m", 1, 50_000);
    let seg = only_open(&sim, 1);
    // Not just before seal_max_age, at it, a day after it, nor a millisecond before
    // destage.max_age.
    let mut age = 0;
    for at in [
        sim.cfg.seal_max_age_ms - 1,
        sim.cfg.seal_max_age_ms,
        sim.cfg.seal_max_age_ms + DAY_MS,
        sim.cfg.destage_max_age_ms - 1,
    ] {
        sim.clock.advance(at - age);
        age = at;
        let report = sim.node(1).tick();
        sim.run();
        assert!(
            report.aged.is_empty() && report.early.is_empty(),
            "age {at}: {report:?}"
        );
        assert_eq!(only_open(&sim, 1).id, seg.id);
    }
    assert_eq!(seals(&sim, 1), [0, 0, 0, 0]);
    sim.clock.advance(1);
    let report = sim.node(1).tick();
    sim.run();
    assert_eq!(report.aged, vec![seg.id]);
    assert_eq!(seals(&sim, 1), [0, 1, 0, 0]);
    assert_eq!(block_state(&sim, 1, seg.id), Some(BlockState::Sealed));
}

#[test]
fn an_empty_segment_never_seals() {
    let mut sim = Sim::new(3, config(3, 3));
    // A key no record can carry fails after the segment was opened for it: the segment stays
    // open with nothing in it.
    let key = "k".repeat(70_000);
    let h = sim.append(1, &key, b"x");
    sim.run();
    assert!(h.take().unwrap().is_err());
    let empty = only_open(&sim, 1);
    assert_eq!((empty.acked, empty.records), (0, 0));
    for days in [1, 3, 30] {
        sim.clock.advance(days * DAY_MS);
        let report = sim.node(1).tick();
        sim.run();
        assert!(
            report.aged.is_empty() && report.early.is_empty(),
            "{report:?}"
        );
        assert_eq!(only_open(&sim, 1).id, empty.id);
    }
    assert_eq!(seals(&sim, 1), [0, 0, 0, 0]);
    assert_eq!(block_state(&sim, 1, empty.id), Some(BlockState::Open));
}

fn open_total(sim: &Sim, nodes: u8) -> usize {
    (1..=nodes).map(|i| sim.node(i).open_segments().len()).sum()
}

fn append_class(sim: &mut Sim, id: u8, key: &str, class: u8) {
    let mut rec = sim.record(key, b"payload");
    rec.expiry = ExpiryClass(class);
    let node = sim.node(id);
    let h = sim
        .exec
        .start(async move { tierstore_store_api::hot::HotStore::append(&node, rec).await });
    sim.run();
    h.take().unwrap().unwrap();
}

#[test]
fn three_nodes_and_one_expiry_class_have_exactly_three_open_segments() {
    let mut sim = Sim::new(3, config(3, 3));
    for id in 1..=3 {
        fill(&mut sim, id, "w", 6, 100);
    }
    assert_eq!(open_total(&sim, 3), 3);
    for id in 1..=3 {
        let seg = only_open(&sim, id);
        assert_eq!(seg.set, vec![NodeId(1), NodeId(2), NodeId(3)]);
        assert_eq!(seg.class, ExpiryClass(0));
        assert_eq!(seg.records, 6);
    }
}

#[test]
fn three_nodes_and_eight_expiry_classes_have_at_most_24_open_segments() {
    let mut sim = Sim::new(3, config(3, 3));
    for round in 0..3 {
        for id in 1..=3u8 {
            for class in 0..8u8 {
                let key = sim.key_on(id, &format!("c{class}-{round}"));
                append_class(&mut sim, id, &key, class);
            }
        }
    }
    assert_eq!(
        open_total(&sim, 3),
        24,
        "one per writer and class, however many keys"
    );
    for id in 1..=3 {
        let mut classes: Vec<u8> = sim
            .node(id)
            .open_segments()
            .iter()
            .map(|s| s.class.0)
            .collect();
        classes.sort_unstable();
        assert_eq!(classes, (0..8).collect::<Vec<u8>>(), "node {id}");
    }
}
