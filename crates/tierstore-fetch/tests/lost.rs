//! The LOCATE step and the lost path of the fetch engine (design 4.6, 10.6): a definitive miss
//! loses the pack, a miss that is not definitive (or an error) waits and asks again, a 404 asks
//! LOCATE once more, a promoted copy that answers 404 is only excluded, and a lost fill is
//! remembered for `lost_ttl` and then starts afresh.
//!
//! Catches: a definitive miss that does not lose the fill; a lost fill that fails the cells that
//! are already present; a lost fill that keeps issuing GETs for its other parts; a lost fill that
//! answers anything but `Lost` before the TTL, or that never starts a new fill after it; `ensure`
//! reviving a lost fill; a miss that is not definitive (or a LOCATE error) that fails the fill
//! at once or never asks again; a wait that outlives `unavailable_wait`; a 404 from a promoted copy
//! that re-LOCATEs or is not excluded for later parts; a 404 from a cold source that does not use
//! the new location; one LOCATE per waiter instead of one per fill; an entry that fails to open
//! that leaves the fill unable to resume.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::join_all;
use support::{
    MIB, Pack, Rig, Script, Sha256Verifier, TestBudget, TestUpstream, cold, drive, generation, hot,
    hot_clean, location,
};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, GetKind, TimerKind};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryInfo, EntrySpec, EntryStore, HolderCounters,
};
use tierstore_fetch_api::fake::{MemCounters, MemEntryStore};
use tierstore_fetch_api::ids::{CellRun, Class, HolderId, JobId, PackId, Rank};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::upstream::{DeviceRef, Located, Location, NotDefinitive, UpstreamError};
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

const JOB: JobId = JobId(1);

fn pack6() -> Pack {
    Pack::new("lost/six", 6 * MIB, 3)
}

/// One hot copy of the whole pack, generation 1.
fn one_hot(pack: &Pack) -> Location {
    location(generation(1), pack.spec.size, vec![hot(1, pack.spec.size)])
}

async fn wait_cells(rig: &Rig, pack: &Pack, first: u64, count: u64) -> WaitOutcome {
    rig.engine
        .wait(&pack.spec, pack.range(first * MIB, count * MIB), JOB)
        .await
}

fn not_found(pack: &Pack, device: DeviceRef) -> UpstreamError {
    UpstreamError::NotFound {
        key: pack.key(),
        device,
    }
}

/// A definitive miss at the first LOCATE loses the pack: the waiter gets `Lost`, nothing is
/// fetched, and the loss is counted once.
#[tokio::test]
async fn a_definitive_miss_at_the_first_locate_loses_the_pack_without_a_get() {
    let rig = Rig::default_rig();
    let pack = pack6();
    let outcome = wait_cells(&rig, &pack, 0, 6).await;
    let stats = rig.engine.stats();
    assert_eq!(outcome, WaitOutcome::Lost);
    assert_eq!(rig.upstream.locates(), vec![pack.key()]);
    assert_eq!(rig.upstream.gets().len(), 0);
    assert_eq!(stats.packs_lost, 1);
    assert_eq!(stats.fills, 1);
    assert_eq!(rig.cells(&pack), vec![CellState::Absent; 6]);
}

/// The object disappears after the first cells landed: the retry of the part that was waiting
/// gets a 404, LOCATE is asked exactly once more, the definitive miss loses the fill, and the
/// waiters of unsatisfied cells get `Lost` while a wait on the present cells is still `Ready`.
#[tokio::test]
async fn an_object_removed_after_some_cells_landed_loses_the_missing_cells_and_keeps_the_present_ones()
 {
    let rig = Rig::default_rig();
    let pack = pack6();
    let size = pack.spec.size;
    let two_extents = location(
        generation(1),
        size,
        vec![cold(1, 1, 0, 2 * MIB), cold(2, 2, 2 * MIB, 4 * MIB)],
    );
    rig.upstream.add(&pack, two_extents);
    // The second part's drive never answers, so the fill stays alive while the first part lands.
    rig.upstream.script(drive(2, 2), Script::NoAnswer);
    rig.engine.ensure(
        &pack.spec,
        &[
            CellRun { first: 0, count: 2 },
            CellRun { first: 2, count: 4 },
        ],
        Class::Prepare,
        JOB,
        Rank(0),
    );
    assert_eq!(wait_cells(&rig, &pack, 0, 2).await, WaitOutcome::Ready);
    rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
    assert_eq!(rig.upstream.locates().len(), 1);

    // The object is gone before the next GET is issued: the stalled part is retried and gets 404.
    rig.upstream.remove(&pack.key());
    rig.clock.advance(60_000);
    assert_eq!(wait_cells(&rig, &pack, 2, 4).await, WaitOutcome::Lost);

    assert_eq!(rig.upstream.locates(), vec![pack.key(), pack.key()]);
    assert_eq!(rig.upstream.gets().len(), 3);
    assert_eq!(rig.engine.stats().packs_lost, 1);
    assert_eq!(wait_cells(&rig, &pack, 0, 2).await, WaitOutcome::Ready);
    assert_eq!(wait_cells(&rig, &pack, 3, 1).await, WaitOutcome::Lost);
    assert_eq!(
        rig.cells(&pack),
        vec![
            CellState::Present,
            CellState::Present,
            CellState::Absent,
            CellState::Absent,
            CellState::Absent,
            CellState::Absent
        ]
    );
    assert_eq!(
        rig.entry_bytes(&pack, 0, 2 * MIB),
        pack.bytes[..(2 * MIB) as usize].to_vec()
    );
}

/// Two parts of one pack both get a 404 (one permit, so one after the other): the pack is lost
/// once, with one LOCATE more than the first, and the loss is counted once.
#[tokio::test]
async fn two_parts_that_both_get_a_404_lose_the_pack_once_with_one_more_locate() {
    let config = EngineConfig {
        acquire_slots: 1,
        ..EngineConfig::default()
    };
    let rig = Rig::new(1, config);
    let pack = pack6();
    // The first LOCATE finds the pack; the object is not there, so every GET answers 404 and the
    // second LOCATE is a definitive miss.
    rig.upstream
        .script_locate(&pack.key(), Ok(Located::Found(one_hot(&pack))));
    let outcome = wait_cells(&rig, &pack, 0, 6).await;
    assert_eq!(outcome, WaitOutcome::Lost);
    assert_eq!(rig.upstream.locates().len(), 2);
    assert_eq!(rig.engine.stats().packs_lost, 1);
    assert_eq!(rig.cells(&pack), vec![CellState::Absent; 6]);
}

/// A lost fill answers `Lost` to new waits without asking LOCATE until `lost_ttl` (60 s)
/// has passed; after it a new wait starts a new fill, which finds the pack.
#[tokio::test]
async fn a_lost_fill_answers_lost_until_the_ttl_and_then_a_new_wait_starts_a_new_fill() {
    let rig = Rig::default_rig();
    let pack = pack6();
    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Lost);
    rig.until(|s| s.armed(TimerKind::LostTtl) == 1).await;

    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Lost);
    assert_eq!(rig.upstream.locates().len(), 1);
    assert_eq!(rig.engine.stats().fills, 1);

    // The pack is back, but the fill is remembered as lost until the TTL ends.
    rig.upstream.add(&pack, one_hot(&pack));
    rig.clock.advance(59_999);
    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Lost);
    assert_eq!(rig.upstream.locates().len(), 1);

    rig.clock.advance(1);
    rig.until(|s| s.armed(TimerKind::LostTtl) == 0).await;
    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.fills, 2);
    assert_eq!(stats.packs_lost, 1);
    assert_eq!(rig.upstream.locates().len(), 2);
    assert_eq!(rig.entry_bytes(&pack, 0, 6 * MIB), pack.bytes.to_vec());
}

/// `ensure` on a lost fill starts nothing: no new LOCATE, no GET, no new fill, and a later wait
/// still answers `Lost`.
#[tokio::test]
async fn ensure_on_a_lost_fill_does_nothing() {
    let rig = Rig::default_rig();
    let pack = pack6();
    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Lost);
    rig.upstream.add(&pack, one_hot(&pack));

    rig.engine.ensure(
        &pack.spec,
        &[CellRun { first: 0, count: 6 }],
        Class::Demand,
        JOB,
        Rank(0),
    );
    assert_eq!(wait_cells(&rig, &pack, 0, 6).await, WaitOutcome::Lost);

    let stats = rig.engine.stats();
    assert_eq!(rig.upstream.gets().len(), 0);
    assert_eq!(rig.upstream.locates().len(), 1);
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.packs_lost, 1);
}

/// A miss that is not definitive (an index still building, too few answers) is not a loss: the
/// fill arms the LOCATE wait, asks again with backoff, and once the pack is found the waiter is
/// `Ready`.
#[tokio::test]
async fn a_miss_that_is_not_definitive_waits_asks_again_and_resumes_when_the_pack_is_found() {
    let rig = Rig::default_rig();
    let pack = pack6();
    rig.upstream.add(&pack, one_hot(&pack));
    for why in [NotDefinitive::Indexing, NotDefinitive::TooFewAnswers] {
        rig.upstream
            .script_locate(&pack.key(), Ok(Located::NotDefinitive(why)));
    }
    let drive = async {
        for asked in 1..=2u64 {
            rig.until(|s| s.locates == asked && s.armed(TimerKind::Locate) == 1)
                .await;
            assert_eq!(rig.engine.stats().packs_lost, 0);
            assert_eq!(rig.upstream.gets().len(), 0);
            rig.clock.advance(6_000);
        }
    };
    let (outcome, ()) = tokio::join!(wait_cells(&rig, &pack, 0, 6), drive);

    let stats = rig.engine.stats();
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.upstream.locates().len(), 3);
    assert_eq!(stats.packs_lost, 0);
    assert_eq!(stats.fills, 1);
    assert_eq!(rig.entry_bytes(&pack, 0, 6 * MIB), pack.bytes.to_vec());
}

/// A LOCATE that fails with an error is treated like a miss that is not definitive: the fill
/// waits, asks again, and resumes when the answer comes.
#[tokio::test]
async fn a_locate_error_waits_like_a_miss_that_is_not_definitive_and_resumes() {
    let rig = Rig::default_rig();
    let pack = pack6();
    rig.upstream.add(&pack, one_hot(&pack));
    rig.upstream.script_locate(
        &pack.key(),
        Err(UpstreamError::Transport {
            key: pack.key(),
            device: DeviceRef::Hot(NodeId(1)),
            reason: "the index node is down".to_owned(),
        }),
    );
    let drive = async {
        rig.until(|s| s.locates == 1 && s.armed(TimerKind::Locate) == 1)
            .await;
        rig.clock.advance(2_000);
    };
    let (outcome, ()) = tokio::join!(wait_cells(&rig, &pack, 0, 6), drive);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.upstream.locates().len(), 2);
    assert_eq!(rig.engine.stats().packs_lost, 0);
}

/// Four waiters of one pack during the wait share one LOCATE wait: one LOCATE was asked, one
/// timer is armed, and all four resume together when the pack is found.
#[tokio::test]
async fn waiters_during_a_locate_wait_share_one_timer_and_one_locate_per_round() {
    let rig = Rig::default_rig();
    let pack = pack6();
    rig.upstream.add(&pack, one_hot(&pack));
    rig.upstream.script_locate(
        &pack.key(),
        Ok(Located::NotDefinitive(NotDefinitive::Indexing)),
    );
    let waiters = join_all((0..4).map(|c| wait_cells(&rig, &pack, c, 1)));
    let drive = async {
        let seen = rig
            .until(|s| s.locates == 1 && s.armed(TimerKind::Locate) == 1)
            .await;
        assert_eq!(seen.fills, 1);
        rig.clock.advance(2_000);
    };
    let (outcomes, ()) = tokio::join!(waiters, drive);

    assert_eq!(outcomes, vec![WaitOutcome::Ready; 4]);
    assert_eq!(rig.upstream.locates().len(), 2);
    assert_eq!(rig.engine.stats().fills, 1);
}

/// A miss that stays not definitive is lost once `unavailable_wait` (600000 ms) has passed, and
/// not before: the fill asks again at the end of the wait (its last wait is cut to the deadline)
/// and then loses the pack.
#[tokio::test]
async fn a_miss_still_not_definitive_after_the_unavailable_wait_loses_the_pack() {
    let rig = Rig::default_rig();
    let pack = pack6();
    rig.upstream.add(&pack, one_hot(&pack));
    for _ in 0..3 {
        rig.upstream.script_locate(
            &pack.key(),
            Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers)),
        );
    }
    let drive = async {
        rig.until(|s| s.locates == 1 && s.armed(TimerKind::Locate) == 1)
            .await;
        // One millisecond before the deadline: not yet lost, asks once more.
        rig.clock.advance(599_999);
        rig.until(|s| s.locates == 2 && s.armed(TimerKind::Locate) == 1)
            .await;
        assert_eq!(rig.engine.stats().packs_lost, 0);
        rig.clock.advance(1);
    };
    let (outcome, ()) = tokio::join!(wait_cells(&rig, &pack, 0, 6), drive);

    let stats = rig.engine.stats();
    assert_eq!(outcome, WaitOutcome::Lost);
    assert_eq!(rig.upstream.locates().len(), 3);
    assert_eq!(stats.packs_lost, 1);
    assert_eq!(rig.upstream.gets().len(), 0);
}

/// A 404 from a promoted copy excludes that copy and the part goes to the hot copy: no new
/// LOCATE, and the next part of the same fill does not go back to the promoted copy.
#[tokio::test]
async fn a_404_from_a_promoted_copy_excludes_it_and_the_part_goes_to_the_hot_copy_without_a_locate()
{
    let rig = Rig::default_rig();
    let pack = pack6();
    let size = pack.spec.size;
    let promoted = DeviceRef::Hot(NodeId(1));
    let hot_copy = DeviceRef::Hot(NodeId(2));
    rig.upstream.add(
        &pack,
        location(generation(1), size, vec![hot_clean(1, size), hot(2, size)]),
    );
    rig.upstream
        .script(promoted, Script::Fail(not_found(&pack, promoted)));
    // The retry on the hot copy is held, so the fill is alive when the next part is asked for.
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream.script(hot_copy, Script::Gated(gate.clone()));
    rig.engine.ensure(
        &pack.spec,
        &[CellRun { first: 0, count: 2 }],
        Class::Prepare,
        JOB,
        Rank(0),
    );
    rig.until(|s| s.gets(GetKind::Retry) == 1).await;

    assert_eq!(wait_cells(&rig, &pack, 2, 4).await, WaitOutcome::Ready);
    gate.add_permits(1);
    assert_eq!(wait_cells(&rig, &pack, 0, 2).await, WaitOutcome::Ready);

    let devices: Vec<DeviceRef> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| g.source.device)
        .collect();
    assert_eq!(devices, vec![promoted, hot_copy, hot_copy]);
    let stats = rig.engine.stats();
    assert_eq!(rig.upstream.locates().len(), 1);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(stats.packs_lost, 0);
    assert_eq!(rig.entry_bytes(&pack, 0, 6 * MIB), pack.bytes.to_vec());
}

/// A 404 mid-fill on a cold source asks LOCATE again, and the retry goes to the new location the
/// answer names.
#[tokio::test]
async fn a_404_on_a_cold_source_asks_locate_again_and_retries_on_the_new_location() {
    let rig = Rig::default_rig();
    let pack = Pack::new("lost/four", 4 * MIB, 5);
    let size = pack.spec.size;
    let before = location(generation(1), size, vec![cold(1, 1, 0, size)]);
    let after = location(generation(1), size, vec![cold(2, 2, 0, size)]);
    rig.upstream.add(&pack, before.clone());
    rig.upstream
        .script_locate(&pack.key(), Ok(Located::Found(before)));
    rig.upstream
        .script_locate(&pack.key(), Ok(Located::Found(after)));
    let gone = drive(1, 1);
    rig.upstream
        .script(gone, Script::Fail(not_found(&pack, gone)));

    assert_eq!(wait_cells(&rig, &pack, 0, 4).await, WaitOutcome::Ready);

    let devices: Vec<DeviceRef> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| g.source.device)
        .collect();
    assert_eq!(devices, vec![gone, drive(2, 2)]);
    assert_eq!(rig.upstream.locates().len(), 2);
    assert_eq!(rig.engine.stats().gets(GetKind::Retry), 1);
    assert_eq!(rig.engine.stats().packs_lost, 0);
    assert_eq!(rig.entry_bytes(&pack, 0, 4 * MIB), pack.bytes.to_vec());
}

/// Many ensures and waits of one pack, from several jobs and over different cells, ask one
/// LOCATE and make one fill. The fill's first part is held at a gate until every call has been
/// made, so the fill is still in flight when each later call arrives whatever the scheduling: a
/// call after a fill has ended starts a new fill (and a new LOCATE) by design.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_concurrent_ensures_and_waits_of_one_pack_cause_exactly_one_locate() {
    let rig = Rig::default_rig();
    let pack = pack6();
    rig.upstream.add(&pack, one_hot(&pack));
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream
        .script(DeviceRef::Hot(NodeId(1)), Script::Gated(gate.clone()));
    let ensure = |job: u64| {
        rig.engine.ensure(
            &pack.spec,
            &[CellRun {
                first: (job - 10) as u32,
                count: 2,
            }],
            Class::Prepare,
            JobId(job),
            Rank(0),
        );
    };
    ensure(10);
    // The fill has located and its first part is held: the later calls find it running.
    rig.until(|s| s.total_gets() >= 1).await;
    ensure(11);
    ensure(12);
    let mut waits = Box::pin(join_all((0..6u64).map(|c| {
        rig.engine
            .wait(&pack.spec, pack.range(c * MIB, MIB), JobId(c))
    })));
    // One poll makes every wait attach; the cells of the held part are not present yet.
    assert!(futures::poll!(&mut waits).is_pending());
    gate.add_permits(1);
    let outcomes = waits.await;

    assert_eq!(outcomes, vec![WaitOutcome::Ready; 6]);
    assert_eq!(rig.upstream.locates(), vec![pack.key()]);
    let stats = rig.engine.stats();
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.locates, 1);
    assert_eq!(rig.entry_bytes(&pack, 0, 6 * MIB), pack.bytes.to_vec());
}

/// An entry store whose first `open` fails and which otherwise passes everything to an in-memory
/// store.
struct FlakyStore {
    inner: MemEntryStore,
    fail_next: AtomicBool,
}

impl FlakyStore {
    fn new(fail_first_open: bool) -> Self {
        Self {
            inner: MemEntryStore::new(),
            fail_next: AtomicBool::new(fail_first_open),
        }
    }
}

impl EntryStore for FlakyStore {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(EntryError::TooLarge {
                pack: spec.pack.clone(),
                size: spec.size,
            });
        }
        self.inner.open(spec)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        self.inner.get(pack)
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.inner.remove(pack)
    }

    fn packs(&self) -> Vec<PackId> {
        self.inner.packs()
    }

    fn entries(&self) -> Vec<EntryInfo> {
        self.inner.entries()
    }

    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        self.inner.register_holder(holder, counters)
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        self.inner.drop_holder(holder)
    }
}

/// An engine over `store`, on the rig's fakes (a manual clock, a 64-permit budget).
fn engine_on(store: Arc<FlakyStore>) -> (FetchEngine, Arc<TestUpstream>) {
    let upstream = Arc::new(TestUpstream::new());
    let parts = EngineParts {
        store,
        upstream: upstream.clone(),
        budget: Arc::new(TestBudget::new(64)),
        verifier: Arc::new(Sha256Verifier),
        timer: Arc::new(ManualTimer::new(ManualClock::new())),
    };
    let engine = FetchEngine::new(
        parts,
        EngineConfig::default(),
        &tokio::runtime::Handle::current(),
    );
    (engine, upstream)
}

/// An entry that fails to open gives the waiter `Eio` (not `Lost`) before any LOCATE; the next
/// wait starts a new fill, which opens the entry and is `Ready`.
#[tokio::test]
async fn an_entry_that_fails_to_open_gives_eio_and_a_later_wait_retries_and_is_ready() {
    let store = Arc::new(FlakyStore::new(true));
    let (engine, upstream) = engine_on(store);
    let pack = pack6();
    upstream.add(&pack, one_hot(&pack));

    let first = engine.wait(&pack.spec, pack.all(), JOB).await;
    let stats = engine.stats();
    assert_eq!(first, WaitOutcome::Eio { cell: 0 });
    assert_eq!(stats.fills_failed, 1);
    assert_eq!(stats.packs_lost, 0);
    assert_eq!(upstream.locates().len(), 0);
    assert_eq!(upstream.gets().len(), 0);

    let second = engine.wait(&pack.spec, pack.all(), JOB).await;
    let stats = engine.stats();
    assert_eq!(second, WaitOutcome::Ready);
    assert_eq!(stats.fills, 2);
    assert_eq!(stats.packs_lost, 0);
    assert_eq!(upstream.locates().len(), 1);
}

/// The wrapper above hands every other call to the in-memory store unchanged, so the tests
/// that use it test the engine and not the wrapper.
#[test]
fn the_flaky_store_passes_every_call_but_its_first_open_through() {
    let store = FlakyStore::new(true);
    let pack = pack6();
    let id = pack.spec.id.clone();
    let spec = EntrySpec {
        pack: id.clone(),
        size: pack.spec.size,
    };
    assert!(store.open(&spec).is_err());
    assert!(store.open(&spec).is_ok());
    assert!(store.get(&id).is_some());
    assert_eq!(store.packs(), vec![id.clone()]);
    assert_eq!(store.entries().len(), 1);
    assert!(
        store
            .register_holder(HolderId(1), Arc::new(MemCounters::new(64)))
            .is_ok()
    );
    let other = Pack::new("lost/other", MIB, 4).spec.id;
    let region = CounterRegion {
        offset: 0,
        groups: 1,
    };
    assert!(store.attach(&other, HolderId(1), region).is_err());
    assert_eq!(store.drop_holder(HolderId(1)), 0);
    assert!(store.remove(&id).unwrap());
}
