//! The pinned generation of a fill (design 4.1, 4.6, section 9, 10.6): every GET of a fill carries
//! `If-Match` of one generation, a copy of another generation is dropped for the fill, and a
//! legacy (silo) object learns its generation from its first part, which goes alone.
//!
//! Catches: a GET without the generation of `LOCATE`; a `digest=none` object that does not pin the
//! ETag of its id; bytes of the other generation written to the entry; a 412 that fails the fill
//! while another copy holds the pinned generation, or that does not count the mismatch; a fill that
//! survives with no copy of the pinned generation, or of another size; a fill that stays failed
//! for a later request; a legacy object whose second part starts before the first answered, or
//! whose later parts carry no `If-Match`; an overwrite between parts that mixes two generations;
//! upload-part boundaries ignored for a legacy pack, or applied to a storage-tier pack.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use support::{
    MIB, Pack, Rig, Script, Sha256Verifier, TestUpstream, generation, hot, legacy, location,
    pattern,
};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, TimerKind};
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::ids::{ByteRange, JobId, PackKey};
use tierstore_fetch_api::upstream::{
    DeviceRef, GetRequest, Located, PartResponse, Report, Source, Upstream, UpstreamError,
};
use tierstore_format::ids::{Generation, NodeId};
use tokio::sync::Semaphore;

const JOB: JobId = JobId(1);

/// One acquire slot: parts are dispatched one at a time, in queue order.
fn serial_config() -> EngineConfig {
    EngineConfig {
        acquire_slots: 1,
        ..EngineConfig::default()
    }
}

fn generations(rig: &Rig, pack: &Pack) -> Vec<Option<Generation>> {
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

/// What the gating upstream saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Entered(Option<Generation>, u64),
    Answered,
}

/// An upstream that holds the next GETs before they reach the scripted upstream, one gate per
/// GET in order, and logs when each GET came in and when it was answered.
struct HeaderGate {
    inner: Arc<TestUpstream>,
    gates: Mutex<VecDeque<Arc<Semaphore>>>,
    log: Mutex<Vec<Step>>,
}

#[async_trait::async_trait]
impl Upstream for HeaderGate {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        self.inner.locate(key).await
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let gate = self.gates.lock().unwrap().pop_front();
        self.log
            .lock()
            .unwrap()
            .push(Step::Entered(req.generation, req.range.offset));
        if let Some(g) = gate {
            g.acquire().await.unwrap().forget();
        }
        let answer = self.inner.get(req).await;
        self.log.lock().unwrap().push(Step::Answered);
        answer
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        self.inner.report(key, source, range, report).await;
    }
}

/// An engine over `rig`'s store, budget and clock whose upstream holds its first GET until `gate`
/// is released.
fn gated_engine(
    rig: &Rig,
    config: EngineConfig,
    gate: &Arc<Semaphore>,
) -> (FetchEngine, Arc<HeaderGate>) {
    let upstream = Arc::new(HeaderGate {
        inner: rig.upstream.clone(),
        gates: Mutex::new(VecDeque::from([gate.clone()])),
        log: Mutex::new(Vec::new()),
    });
    let parts = EngineParts {
        store: rig.store.clone(),
        upstream: upstream.clone(),
        budget: rig.budget.clone(),
        verifier: Arc::new(Sha256Verifier),
        timer: Arc::new(ManualTimer::new(rig.clock.clone())),
    };
    let engine = FetchEngine::new(parts, config, &tokio::runtime::Handle::current());
    (engine, upstream)
}

/// Every GET of a stored pack carries the generation `LOCATE` gave, in every part of the plan.
#[tokio::test]
async fn every_get_of_a_stored_pack_carries_the_generation_of_locate() {
    let rig = Rig::default_rig();
    let pack = Pack::new("stored", 6 * MIB, 3);
    rig.upstream.add(
        &pack,
        location(
            Some(generation(5)),
            pack.spec.size,
            vec![hot(1, pack.spec.size)],
        ),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        generations(&rig, &pack),
        vec![Some(generation(5)); 2],
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
        location(
            Some(generation(7)),
            pack.spec.size,
            vec![hot(1, pack.spec.size)],
        ),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(generations(&rig, &pack), vec![Some(generation(7)); 2]);
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
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.hold(
        &pack.key(),
        DeviceRef::Hot(NodeId(1)),
        generation(2),
        pattern(size, 99),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.copy_generation_mismatch, 1);
    assert_eq!(stats.fills_failed, 0);
    assert_eq!(
        devices(&rig, &pack),
        vec![DeviceRef::Hot(NodeId(1)), DeviceRef::Hot(NodeId(2))],
        "the refused copy is asked once, then the other replica"
    );
    assert_eq!(generations(&rig, &pack), vec![Some(generation(1)); 2]);
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
        location(Some(generation(7)), size, vec![hot(1, size), hot(2, size)]),
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
    assert_eq!(generations(&rig, &pack), vec![Some(generation(7)); 2]);
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
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
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
    rig.upstream.add(
        &pack,
        location(Some(generation(8)), size, vec![hot(1, size)]),
    );
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
    let mut found = location(Some(generation(8)), size, vec![hot(1, size), hot(2, size)]);
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
    assert_eq!(generations(&rig, &pack), vec![Some(generation(7))]);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A location whose size differs from the pack's spec fails the fill with EIO before any GET.
#[tokio::test]
async fn a_location_of_another_size_fails_the_fill() {
    let rig = Rig::default_rig();
    let pack = Pack::new("wrong-size", 2 * MIB, 11);
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), 3 * MIB, vec![hot(1, 3 * MIB)]),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    assert_eq!(rig.engine.stats().fills_failed, 1);
    assert!(rig.upstream.gets().is_empty());
}

/// A legacy location without a generation: the first GET carries none and goes alone (nothing else
/// starts while it is unanswered); every later GET carries the ETag of the first response.
#[tokio::test]
async fn a_legacy_first_part_goes_alone_and_every_later_part_carries_its_etag() {
    let rig = Rig::new(64, serial_config());
    let pack = Pack::new("silo-object", 6 * MIB, 12);
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(None, size, vec![legacy(size)]));
    let gate = Arc::new(Semaphore::new(0));
    let (engine, upstream) = gated_engine(&rig, serial_config(), &gate);
    let (out, ()) = tokio::join!(engine.wait(&pack.spec, pack.all(), JOB), async {
        engine
            .until(|s| s.total_gets() >= 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        gate.add_permits(1);
    });
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        *upstream.log.lock().unwrap(),
        vec![
            Step::Entered(None, 0),
            Step::Answered,
            Step::Entered(Some(generation(1)), 4 * MIB),
            Step::Answered,
        ]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// The same with the engine's default configuration (several parts may wait for a permit at
/// once): still no second GET starts before the first answered.
#[tokio::test]
async fn a_legacy_first_part_goes_alone_also_with_the_default_acquire_slots() {
    let rig = Rig::default_rig();
    let pack = Pack::new("silo-object-default", 6 * MIB, 13);
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(None, size, vec![legacy(size)]));
    let gate = Arc::new(Semaphore::new(0));
    let (engine, upstream) = gated_engine(&rig, EngineConfig::default(), &gate);
    let (out, ()) = tokio::join!(engine.wait(&pack.spec, pack.all(), JOB), async {
        engine
            .until(|s| s.total_gets() >= 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        gate.add_permits(1);
    });
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        *upstream.log.lock().unwrap(),
        vec![
            Step::Entered(None, 0),
            Step::Answered,
            Step::Entered(Some(generation(1)), 4 * MIB),
            Step::Answered,
        ]
    );
}

/// An overwrite between the parts of a silo object: the second part's `If-Match` of the first
/// response's ETag gets 412, no copy is left, the wait gets EIO and none of the new bytes land.
#[tokio::test]
async fn an_overwrite_between_parts_on_silo_fails_the_fill_and_writes_nothing_of_the_new_bytes() {
    let rig = Rig::new(1, serial_config());
    let pack = Pack::new("silo-overwritten", 6 * MIB, 14);
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(None, size, vec![legacy(size)]));
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream
        .script(DeviceRef::Legacy, Script::Gated(gate.clone()));
    let (out, ()) = tokio::join!(rig.engine.wait(&pack.spec, pack.all(), JOB), async {
        // The first part answered and its body is awaited: the first-byte timer is armed.
        rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
        rig.upstream.hold(
            &pack.key(),
            DeviceRef::Legacy,
            generation(2),
            pattern(size, 98),
        );
        gate.add_permits(1);
    });
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    let stats = rig.engine.stats();
    assert_eq!(stats.copy_generation_mismatch, 1);
    assert_eq!(stats.fills_failed, 1);
    assert_eq!(
        generations(&rig, &pack),
        vec![None, Some(generation(1))],
        "the second part carries the ETag of the first response"
    );
    assert_eq!(
        rig.entry_bytes(&pack, 4 * MIB, 2 * MIB),
        vec![0; (2 * MIB) as usize],
        "nothing of the overwriting bytes is written"
    );
}

/// A legacy pack with a recorded `upload_part_size` is cut into parts at those boundaries.
#[tokio::test]
async fn a_legacy_pack_is_cut_into_parts_at_its_upload_part_size() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("silo-upload-parts", 8 * MIB, 15);
    pack.spec.upload_part_size = 3 * MIB;
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(None, size, vec![legacy(size)]));
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    let mut ranges = rig.get_ranges(&pack);
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        vec![
            (0, 3 * MIB),
            (3 * MIB, MIB),
            (4 * MIB, 2 * MIB),
            (6 * MIB, 2 * MIB)
        ]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A storage-tier pack ignores `upload_part_size`: its parts follow the plan alone.
#[tokio::test]
async fn a_storage_tier_pack_ignores_the_upload_part_size() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("stored-upload-parts", 8 * MIB, 16);
    pack.spec.upload_part_size = 3 * MIB;
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    let mut ranges = rig.get_ranges(&pack);
    ranges.sort_unstable();
    assert_eq!(ranges, vec![(0, 4 * MIB), (4 * MIB, 4 * MIB)]);
}
