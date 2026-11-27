//! The engine's less common paths: a location with no copy, part bodies that run long, end short
//! or come in chunks that do not fit cells, a copy whose `ETag` is another generation without a
//! 412, a fill lost while other parts are in flight or wait for permits, a part whose cells a
//! demand took while it waited for its permit, a cell delivered twice, `recheck` and evicting
//! cells under `ensure`, a `recheck` cell the list has no digest for, and a pack too large to
//! have cells.
//!
//! Catches:
//! - a fill with no copy at all that hangs instead of failing with EIO;
//! - cells cut wrongly from chunks that straddle cells, or a long or short body taken as a part;
//! - bytes of another generation published because the response's `ETag` was not checked;
//! - an op that keeps fetching (or a part that starts) for a fill that was lost;
//! - a part that refetches cells a demand already claimed, or one whose permit leaks;
//! - a duplicate delivery written twice, or not counted as waste;
//! - Prepare that leaves `recheck` or evicting cells unsettled for ever;
//! - a `recheck` cell trusted without a digest;
//! - a pack beyond `u32::MAX` cells that is filled, or panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use support::{
    MIB, Pack, Rig, Script, Sha256Verifier, TestBudget, TestUpstream, generation, hot, location,
};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, GetKind, TimerKind};
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Permit};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::{Engine, PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{EntrySpec, EntryStore, EvictStep};
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    GetRequest, Located, PartResponse, Report, Source, Upstream, UpstreamError,
};
use tokio::sync::Semaphore;

fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

/// Two hot copies of `pack` (generation 1).
fn two_copies(rig: &Rig, pack: &Pack) {
    let size = pack.spec.size;
    rig.upstream.add(
        pack,
        location(generation(1), size, vec![hot(1, size), hot(2, size)]),
    );
}

/// What the wrapper does to the first part it serves.
#[derive(Clone, Copy)]
enum Twist {
    /// One byte more than the range.
    Long,
    /// One byte less, then the end of the body.
    Short,
    /// The `ETag` of another generation, with the right status.
    OtherEtag,
}

/// An upstream that changes the first part it serves by `twist`, and serves the rest as the
/// test upstream does.
struct Twisted {
    inner: Arc<TestUpstream>,
    twist: Twist,
    served: AtomicUsize,
}

#[async_trait::async_trait]
impl Upstream for Twisted {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        self.inner.locate(key).await
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let mut r = self.inner.get(req).await?;
        let first = self.served.fetch_add(1, Ordering::SeqCst) == 0;
        let twist = [None, Some(self.twist)][usize::from(first)];
        let body: Vec<u8> = r
            .body
            .map(|c| c.unwrap().to_vec())
            .collect::<Vec<_>>()
            .await
            .concat();
        let bytes = match twist {
            Some(Twist::Long) => [body, vec![0]].concat(),
            Some(Twist::Short) => body[..body.len() - 1].to_vec(),
            Some(Twist::OtherEtag) => {
                r.generation = generation(9);
                body
            }
            None => body,
        };
        r.body = stream::iter(vec![Ok(Bytes::from(bytes))]).boxed();
        Ok(r)
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        self.inner.report(key, source, range, report).await;
    }
}

/// An engine over `upstream` and `budget`, with the rig's other fakes.
fn engine(
    upstream: Arc<dyn Upstream>,
    budget: Arc<dyn Budget>,
    config: EngineConfig,
) -> (FetchEngine, Arc<MemEntryStore>, ManualClock) {
    let clock = ManualClock::new();
    let store = Arc::new(MemEntryStore::new());
    let parts = EngineParts {
        store: store.clone(),
        upstream,
        budget,
        verifier: Arc::new(Sha256Verifier),
        timer: Arc::new(ManualTimer::new(clock.clone())),
        cache_metrics: None,
    };
    let e = FetchEngine::new(parts, config, &tokio::runtime::Handle::current());
    (e, store, clock)
}

async fn twisted(twist: Twist) -> (FetchEngine, Arc<MemEntryStore>, Pack, WaitOutcome) {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 3 * MIB, 4);
    two_copies(&rig, &pack);
    let up = Arc::new(Twisted {
        inner: rig.upstream.clone(),
        twist,
        served: AtomicUsize::new(0),
    });
    let (e, store, _clock) = engine(up, Arc::new(TestBudget::new(8)), EngineConfig::default());
    let out = e.wait(&pack.spec, pack.all(), JobId(1)).await;
    (e, store, pack, out)
}

/// A body one byte longer than the part fails the part, which is fetched again from the other
/// copy, and nothing of the first body is published.
#[tokio::test]
async fn a_body_longer_than_its_part_is_refetched_from_the_other_copy() {
    let (e, store, pack, out) = twisted(Twist::Long).await;
    assert_eq!(out, WaitOutcome::Ready);
    let s = e.until(|s| s.cells_published == 3).await;
    assert_eq!(s.gets(GetKind::Retry), 1);
    let entry = store.mem_entry(&pack.spec.id).unwrap();
    assert_eq!(entry.bytes(0, 3 * MIB as usize), pack.bytes.to_vec());
}

/// A body that ends one byte short fails the cell it left incomplete, which is fetched again.
#[tokio::test]
async fn a_body_that_ends_short_is_refetched_for_the_cells_it_left() {
    let (e, store, pack, out) = twisted(Twist::Short).await;
    assert_eq!(out, WaitOutcome::Ready);
    let s = e.until(|s| s.cells_published == 3).await;
    assert_eq!((s.gets(GetKind::Retry), s.bytes_wasted), (1, MIB - 1));
    let entry = store.mem_entry(&pack.spec.id).unwrap();
    assert_eq!(entry.bytes(0, 3 * MIB as usize), pack.bytes.to_vec());
}

/// A copy whose response carries another generation's `ETag` (no 412) is excluded and counted,
/// and none of its bytes is written.
#[tokio::test]
async fn a_response_of_another_generation_without_a_412_is_excluded() {
    let (e, store, pack, out) = twisted(Twist::OtherEtag).await;
    assert_eq!(out, WaitOutcome::Ready);
    let s = e.until(|s| s.cells_published == 3).await;
    assert_eq!((s.copy_generation_mismatch, s.bytes_fetched), (1, 3 * MIB));
    let entry = store.mem_entry(&pack.spec.id).unwrap();
    assert_eq!(entry.bytes(0, 3 * MIB as usize), pack.bytes.to_vec());
}

/// Chunks of 300 KiB, which straddle cells, are cut into whole cells and each published once.
#[tokio::test]
async fn chunks_that_straddle_cells_are_cut_into_whole_cells() {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 3 * MIB + 7, 5);
    two_copies(&rig, &pack);
    let chunked = Script::Serve {
        chunk: 300 << 10,
        signals: Default::default(),
    };
    rig.upstream.script(hot(1, 0).device, chunked);
    let out = rig.engine.wait(&pack.spec, pack.all(), JobId(1)).await;
    assert_eq!(out, WaitOutcome::Ready);
    let s = rig.until(|s| s.cells_published == 4).await;
    assert_eq!((s.total_gets(), s.bytes_wasted), (1, 0));
    assert_eq!(
        rig.entry_bytes(&pack, 0, pack.spec.size),
        pack.bytes.to_vec()
    );
}

/// A location that names no copy fails the fill: the waiter gets EIO and the fill counts as
/// failed, with no GET.
#[tokio::test]
async fn a_location_with_no_copy_fails_the_fill() {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 2 * MIB, 6);
    rig.upstream
        .add(&pack, location(generation(1), pack.spec.size, vec![]));
    let out = rig.engine.wait(&pack.spec, pack.all(), JobId(1)).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    let s = rig.engine.stats();
    assert_eq!((s.fills_failed, s.total_gets()), (1, 0));
}

/// A demand part, then the original's bytes: when the hedge delivered cell 0 first, the
/// original's copy of it is dropped and counted as waste, and cell 1 comes from the original.
#[tokio::test]
async fn a_cell_delivered_twice_is_published_once_and_the_second_copy_is_waste() {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 2 * MIB, 7);
    two_copies(&rig, &pack);
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream
        .script(hot(1, 0).device, Script::Gated(gate.clone()));
    rig.upstream
        .script(hot(2, 0).device, Script::StallAfter(MIB as usize));
    let (out, ()) = tokio::join!(rig.engine.wait(&pack.spec, pack.all(), JobId(1)), async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1).await;
        rig.clock.advance(30);
        rig.until(|s| s.cells_published == 1).await;
        gate.add_permits(1);
    });
    assert_eq!(out, WaitOutcome::Ready);
    let s = rig.until(|s| s.cells_published == 2).await;
    assert_eq!((s.gets(GetKind::Hedge), s.bytes_wasted), (1, MIB));
    assert_eq!(
        rig.entry_bytes(&pack, 0, pack.spec.size),
        pack.bytes.to_vec()
    );
}

/// A budget that grants `demand` at once and lets one other part at a time through.
struct DemandFirst {
    others: Arc<Semaphore>,
    log: TestBudget,
}

#[async_trait::async_trait]
impl Budget for DemandFirst {
    async fn acquire(&self, charge: Charge) -> Permit {
        let demand = charge.class == Class::Demand;
        self.log.charges.lock().unwrap().push(charge);
        let n = u32::from(!demand);
        let p = self.others.clone().acquire_many_owned(n).await.unwrap();
        Permit::new(move || drop(p))
    }

    fn observe(&self, observation: Observation) {
        self.log.observe(observation);
    }

    fn part_size(&self, pack_size: u64) -> u64 {
        self.log.part_size(pack_size)
    }
}

fn demand_first(up: &Arc<TestUpstream>) -> (FetchEngine, Arc<MemEntryStore>, Arc<DemandFirst>) {
    let budget = Arc::new(DemandFirst {
        others: Arc::new(Semaphore::new(1)),
        log: TestBudget::new(0),
    });
    let (e, store, _clock) = engine(up.clone(), budget.clone(), EngineConfig::default());
    (e, store, budget)
}

/// A prepare part waiting for its permit while a demand takes every one of its cells claims
/// nothing when the permit comes, and issues no GET.
#[tokio::test]
async fn a_part_whose_cells_a_demand_took_while_it_waited_for_its_permit_claims_nothing() {
    let up = Arc::new(TestUpstream::new());
    let pack = Pack::new("p", 8 * MIB, 8);
    let size = pack.spec.size;
    up.add(&pack, location(generation(1), size, vec![hot(1, size)]));
    let gate = Arc::new(Semaphore::new(0));
    up.script(hot(1, 0).device, Script::Gated(gate.clone()));
    let (e, store, budget) = demand_first(&up);
    e.ensure(&pack.spec, &[run(0, 8)], Class::Prepare, JobId(1), Rank(0));
    e.until(|s| s.total_gets() == 1).await;
    // The second prepare part [4, 8) waits for the one permit the first holds.
    while budget.log.charges.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let tail = e
        .wait(&pack.spec, pack.range(4 * MIB, 4 * MIB), JobId(2))
        .await;
    assert_eq!(tail, WaitOutcome::Ready);
    gate.add_permits(1);
    e.until(|s| s.cells_published == 8).await;
    e.until(|_| budget.others.available_permits() == 1).await;
    let gets = up.gets();
    assert_eq!(
        gets.iter()
            .map(|g| (g.range.offset, g.class))
            .collect::<Vec<_>>(),
        vec![(0, Class::Prepare), (4 * MIB, Class::Demand)]
    );
    let entry = store.mem_entry(&pack.spec.id).unwrap();
    assert_eq!(entry.bytes(0, size as usize), pack.bytes.to_vec());
}

/// While one part is in flight and another waits for its permit, a demand part's 404 loses the
/// pack: the waiting part claims nothing and issues no GET, the part in flight stops, and the
/// demand's waiter gets `lost`.
#[tokio::test]
async fn parts_of_a_fill_lost_meanwhile_stop_and_start_nothing() {
    let up = Arc::new(TestUpstream::new());
    let pack = Pack::new("p", 12 * MIB, 9);
    let size = pack.spec.size;
    let loc = location(generation(1), size, vec![hot(1, size)]);
    up.add(&pack, loc.clone());
    up.script_locate(&pack.key(), Ok(Located::Found(loc)));
    up.script_locate(&pack.key(), Ok(Located::DefinitiveMiss));
    let gate = Arc::new(Semaphore::new(0));
    up.script(hot(1, 0).device, Script::Gated(gate.clone()));
    let gone = UpstreamError::NotFound {
        key: pack.key(),
        device: hot(1, 0).device,
    };
    up.script(hot(1, 0).device, Script::Fail(gone));
    let (e, _store, budget) = demand_first(&up);
    e.ensure(&pack.spec, &[run(0, 12)], Class::Prepare, JobId(1), Rank(0));
    e.until(|s| s.total_gets() == 1).await;
    while budget.log.charges.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let last = e
        .wait(&pack.spec, pack.range(11 * MIB, MIB), JobId(2))
        .await;
    assert_eq!(last, WaitOutcome::Lost);
    gate.add_permits(1);
    let s = e.until(|_| budget.others.available_permits() == 1).await;
    assert_eq!((s.packs_lost, s.locates, up.gets().len()), (1, 2, 2));
}

/// Prepare of a pack whose cell is `recheck` with the right bytes settles it present with no
/// GET, and one whose cell the evictor finishes is fetched again after the poll.
#[tokio::test]
async fn prepare_settles_recheck_cells_and_refetches_a_cell_whose_eviction_finished() {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 2 * MIB, 10);
    two_copies(&rig, &pack);
    let entry = rig
        .store
        .open(&EntrySpec {
            pack: pack.spec.id.clone(),
            size: pack.spec.size,
        })
        .unwrap();
    let mem = rig.store.mem_entry(&pack.spec.id).unwrap();
    mem.restore_as_recheck(0, &pack.bytes[..MIB as usize])
        .unwrap();
    mem.restore_as_recheck(1, &pack.bytes[MIB as usize..])
        .unwrap();
    rig.engine
        .ensure(&pack.spec, &[run(0, 1)], Class::Prepare, JobId(1), Rank(0));
    rig.until(|_| rig.cells(&pack)[0] == CellState::Present)
        .await;
    assert_eq!(rig.engine.stats().total_gets(), 0);
    // Cell 1 settles present too (by a waiter), then the evictor takes it.
    let out = rig
        .engine
        .wait(&pack.spec, pack.range(MIB, MIB), JobId(1))
        .await;
    assert_eq!(out, WaitOutcome::Ready);
    assert!(entry.evict_cell_cas(1, EvictStep::Begin));
    rig.engine
        .ensure(&pack.spec, &[run(1, 1)], Class::Prepare, JobId(1), Rank(0));
    rig.until(|s| s.armed(TimerKind::EvictPoll) == 1).await;
    assert!(entry.evict_cell_cas(1, EvictStep::Finish));
    rig.clock.advance(50);
    rig.until(|s| s.cells_published == 1).await;
    assert_eq!(rig.cells(&pack), vec![CellState::Present; 2]);
    assert_eq!(rig.get_ranges(&pack), vec![(MIB, MIB)]);
}

/// A `recheck` cell the list has no digest for is not trusted: it is fetched again, and with no
/// digest to verify the fetched bytes either it ends `bad` (EIO), never present.
#[tokio::test]
async fn a_recheck_cell_with_no_digest_in_the_list_is_never_served() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("p", 2 * MIB, 11);
    let full = pack.spec.digests.clone().unwrap();
    let list = support::TestList {
        key: pack.key(),
        size: pack.spec.size,
        digests: vec![full.list.cell_digest(0, 0).unwrap()],
    };
    pack.spec.digests = Some(tierstore_fetch_api::engine::PackDigests {
        list: Arc::new(list),
        pack: 0,
    });
    two_copies(&rig, &pack);
    rig.store
        .open(&EntrySpec {
            pack: pack.spec.id.clone(),
            size: pack.spec.size,
        })
        .unwrap();
    let mem = rig.store.mem_entry(&pack.spec.id).unwrap();
    mem.restore_as_recheck(1, &pack.bytes[MIB as usize..])
        .unwrap();
    let out = rig
        .engine
        .wait(&pack.spec, pack.range(MIB, MIB), JobId(1))
        .await;
    assert_eq!(out, WaitOutcome::Eio { cell: 1 });
    assert_eq!(rig.cells(&pack)[1], CellState::Bad);
}

/// A pack of more than `u32::MAX` cells has no fill: `ensure` does nothing and a wait is EIO.
#[tokio::test]
async fn a_pack_beyond_u32_max_cells_is_eio_and_starts_nothing() {
    let rig = Rig::default_rig();
    let mut spec: PackSpec = Pack::new("p", MIB, 12).spec;
    spec.size = u64::MAX;
    rig.engine
        .ensure(&spec, &[run(0, 1)], Class::Prepare, JobId(1), Rank(0));
    let out = rig
        .engine
        .wait(&spec, ByteRange { offset: 0, len: 1 }, JobId(1))
        .await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    assert_eq!(rig.engine.stats().fills, 0);
}

/// A copy that answers 404 while `LOCATE` keeps naming it (a stale index) is excluded after the
/// one `LOCATE` it causes: the fill fails with EIO instead of asking again in a loop.
#[tokio::test]
async fn a_copy_that_404s_while_locate_still_names_it_fails_the_fill_after_one_more_locate() {
    let rig = Rig::default_rig();
    let pack = Pack::new("p", 2 * MIB, 13);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(generation(1), size, vec![support::hot(1, size)]),
    );
    let gone = UpstreamError::NotFound {
        key: pack.key(),
        device: support::hot(1, 0).device,
    };
    rig.upstream
        .script(support::hot(1, 0).device, Script::Fail(gone));
    let out = rig.engine.wait(&pack.spec, pack.all(), JobId(1)).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 0 });
    let s = rig.engine.stats();
    assert_eq!((s.locates, s.total_gets(), s.fills_failed), (2, 1, 1));
}

/// A copy that keeps refusing at once while `LOCATE` keeps finding it: the first retry after a
/// `LOCATE` goes at once, the next waits a backoff, so the copy is not asked in a tight loop.
#[tokio::test]
async fn retries_after_a_second_relocation_back_off() {
    let config = EngineConfig {
        max_attempts: 1,
        ..EngineConfig::default()
    };
    let rig = Rig::new(8, config);
    let pack = Pack::new("p", 2 * MIB, 14);
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(generation(1), size, vec![hot(1, size)]));
    let reset = || {
        Script::Fail(UpstreamError::Transport {
            key: pack.key(),
            device: hot(1, 0).device,
            reason: "reset".to_owned(),
        })
    };
    rig.upstream.script(hot(1, 0).device, reset());
    rig.upstream.script(hot(1, 0).device, reset());
    let (out, ()) = tokio::join!(rig.engine.wait(&pack.spec, pack.all(), JobId(1)), async {
        rig.until(|s| s.armed(TimerKind::Backoff) == 1).await;
        assert_eq!(rig.engine.stats().total_gets(), 2);
        rig.clock.advance(30_000);
    });
    assert_eq!(out, WaitOutcome::Ready);
    let s = rig.until(|s| s.cells_published == 2).await;
    assert_eq!((s.locates, s.total_gets()), (3, 3));
}
