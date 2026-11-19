//! The pinned generation of a fill (design 4.1, 4.6, section 9, 10.6): every GET of a fill carries
//! `If-Match` of one generation, and a copy of another generation is dropped for the fill.
//!
//! Catches: a GET without the generation of `LOCATE`; a `digest=none` object that does not pin the
//! ETag of its id; bytes of the other generation written to the entry; a 412 that fails the fill
//! while another copy holds the pinned generation, or that does not count the mismatch; a fill that
//! survives with no copy of the pinned generation, or of another size; a fill that stays failed
//! for a later request.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{MIB, Pack, Rig, generation, hot, location, pattern};
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::ids::JobId;
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::{Generation, NodeId};

const JOB: JobId = JobId(1);

fn generations(rig: &Rig, pack: &Pack) -> Vec<Generation> {
    rig.upstream
        .gets()
        .iter()
        .filter(|g| g.key == pack.key())
        .map(|g| g.generation)
        .collect()
}

fn devices(rig: &Rig, pack: &Pack) -> Vec<DeviceRef> {
    rig.upstream
        .gets()
        .iter()
        .filter(|g| g.key == pack.key())
        .map(|g| g.source.device)
        .collect()
}

/// Every GET of a stored pack carries the generation `LOCATE` gave, in every part of the plan.
#[tokio::test]
async fn every_get_of_a_stored_pack_carries_the_generation_of_locate() {
    let rig = Rig::default_rig();
    let pack = Pack::new("stored", 6 * MIB, 3);
    rig.upstream.add(
        &pack,
        location(generation(5), pack.spec.size, vec![hot(1, pack.spec.size)]),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        generations(&rig, &pack),
        vec![generation(5); 2],
        "both parts of the plan carry the generation"
    );
    assert_eq!(
        rig.entry_bytes(&pack, 0, pack.spec.size),
        pack.bytes.to_vec()
    );
}

/// A `digest=none` object pins the ETag of its id: every GET carries it.
#[tokio::test]
async fn a_digest_none_object_pins_the_etag_of_its_id_on_every_get() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-object", 6 * MIB, 4, generation(7));
    rig.upstream.add(
        &pack,
        location(generation(7), pack.spec.size, vec![hot(1, pack.spec.size)]),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(generations(&rig, &pack), vec![generation(7); 2]);
    assert_eq!(
        rig.entry_bytes(&pack, 0, pack.spec.size),
        pack.bytes.to_vec()
    );
}

/// A copy holding another generation answers 412: it is excluded and counted, the part goes to the
/// other replica, and the entry holds the bytes of the pinned generation.
#[tokio::test]
async fn a_copy_of_another_generation_is_excluded_and_the_part_goes_to_the_other_replica() {
    let rig = Rig::default_rig();
    let pack = Pack::new("two-copies", 2 * MIB, 5);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(generation(1), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.hold(
        &pack.key(),
        DeviceRef::Hot(NodeId(1)),
        generation(2),
        pattern(size, 99),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.bytes_published >= size).await;
    assert_eq!(stats.copy_generation_mismatch, 1);
    assert_eq!(stats.fills_failed, 0);
    assert_eq!(
        devices(&rig, &pack),
        vec![DeviceRef::Hot(NodeId(1)), DeviceRef::Hot(NodeId(2))],
        "the refused copy is asked once, then the other replica"
    );
    assert_eq!(generations(&rig, &pack), vec![generation(1); 2]);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
    assert_eq!(stats.bytes_published, size);
}

/// Two copies of a `digest=none` object hold different generations: the fill pins the ETag of its
/// id, every GET carries it, the other copy is excluded and counted, and none of its bytes land.
#[tokio::test]
async fn two_copies_of_a_digest_none_object_on_different_generations_yield_only_the_pinned_bytes() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-two-copies", 2 * MIB, 6, generation(7));
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(generation(7), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.hold(
        &pack.key(),
        DeviceRef::Hot(NodeId(1)),
        generation(8),
        pattern(size, 77),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(rig.engine.stats().copy_generation_mismatch, 1);
    assert_eq!(generations(&rig, &pack), vec![generation(7); 2]);
    assert_eq!(
        devices(&rig, &pack),
        vec![DeviceRef::Hot(NodeId(1)), DeviceRef::Hot(NodeId(2))]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// With every copy on another generation the wait gets EIO and the fill counts as failed, nothing
/// of the other bytes is written, and a later wait after the upstream is fixed starts a new fill
/// life and is Ready.
#[tokio::test]
async fn with_every_copy_on_another_generation_the_fill_fails_and_a_later_wait_starts_a_new_life() {
    let rig = Rig::default_rig();
    let pack = Pack::new("all-wrong", 2 * MIB, 8);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(generation(1), size, vec![hot(1, size), hot(2, size)]),
    );
    for node in [1, 2] {
        rig.upstream.hold(
            &pack.key(),
            DeviceRef::Hot(NodeId(node)),
            generation(2),
            pattern(size, 55),
        );
    }
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    let stats = rig.engine.stats();
    assert_eq!(stats.fills_failed, 1);
    assert_eq!(stats.copy_generation_mismatch, 2);
    assert_eq!(stats.bytes_published, 0);
    assert_eq!(rig.entry_bytes(&pack, 0, size), vec![0; size as usize]);

    for node in [1, 2] {
        rig.upstream.hold(
            &pack.key(),
            DeviceRef::Hot(NodeId(node)),
            generation(1),
            pack.bytes.clone(),
        );
    }
    let again = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(again, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.fills, 2, "the second wait is a new fill life");
    assert_eq!(stats.locates, 2);
    assert_eq!(stats.fills_failed, 1);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A location whose generation is neither the pinned ETag nor among its conflicts fails the fill
/// before any GET.
#[tokio::test]
async fn a_location_of_a_generation_other_than_the_pinned_etag_fails_the_fill() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-wrong-location", 2 * MIB, 9, generation(7));
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(generation(8), size, vec![hot(1, size)]));
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    assert_eq!(rig.engine.stats().fills_failed, 1);
    assert!(
        rig.upstream.gets().is_empty(),
        "no GET goes to a location without the pinned generation"
    );
}

/// A location whose generation differs from the pinned ETag but lists it among its conflicts is
/// served: the pinned generation is still held by the copies.
#[tokio::test]
async fn a_location_listing_the_pinned_etag_among_its_conflicts_is_served_at_that_etag() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-conflict", 2 * MIB, 10, generation(7));
    let size = pack.spec.size;
    let mut found = location(generation(8), size, vec![hot(1, size), hot(2, size)]);
    found.conflicts = vec![generation(7)];
    rig.upstream.add(&pack, found);
    for node in [1, 2] {
        rig.upstream.hold(
            &pack.key(),
            DeviceRef::Hot(NodeId(node)),
            generation(7),
            pack.bytes.clone(),
        );
    }
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(generations(&rig, &pack), vec![generation(7)]);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A location whose size differs from the pack's spec fails the fill with EIO before any GET.
#[tokio::test]
async fn a_location_of_another_size_fails_the_fill() {
    let rig = Rig::default_rig();
    let pack = Pack::new("wrong-size", 2 * MIB, 11);
    rig.upstream.add(
        &pack,
        location(generation(1), 3 * MIB, vec![hot(1, 3 * MIB)]),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    assert_eq!(rig.engine.stats().fills_failed, 1);
    assert!(rig.upstream.gets().is_empty());
}
