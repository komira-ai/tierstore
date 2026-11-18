//! Stalls and retries of the fetch engine (design 4.6 and 4.3): when a part is retried, where the
//! retry goes, what it re-fetches, when it waits, and what the budget is told.
//!
//! Time moves only by the manual clock, and only after the engine says (by `Stats::armed`) that
//! it waits on a timer. Every test runs on the current-thread runtime with one budget permit
//! where the order of GETs or charges is asserted.
//!
//! Catches: a stalled part retried before the pre-first-byte timeout, or never; a headers answer
//! that says the request queued still timing out at the old deadline; a stall after the first
//! byte that re-fetches cells already delivered; a drive that says `stalled` being waited on; a
//! `503` retried before its `Retry-After`, or not marking the device busy; a `503` that is not a
//! load signal being waited on or taken for load; a transport error that waits; two stall
//! retries on one drive inside the window; a window that spans drives; a cold shard that retries
//! on the same drive instead of a degraded read; no `LOCATE` after the attempts ran out; a fill
//! that retries past `unavailable_wait` or cannot resume after its EIO; a retry that does not
//! take a budget permit; wrong observations told to the budget.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::{
    MIB, Pack, Rig, Script, Sha256Verifier, TestBudget, TestUpstream, cold, degraded, drive,
    generation, hot, location,
};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, GetKind, TimerKind};
use tierstore_fetch_api::budget::{Charge, Observation, Outcome};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, PackKey};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, DriveState, GetRequest, Located, PartResponse, Report, Source,
    SourceKind, Upstream, UpstreamError,
};
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

/// The first-choice hot replica of every test.
fn h1() -> DeviceRef {
    DeviceRef::Hot(NodeId(1))
}

/// The other hot replica.
fn h2() -> DeviceRef {
    DeviceRef::Hot(NodeId(2))
}

/// A pack of four cells: one part, wanted whole by a `wait` on all of it.
fn pack() -> Pack {
    Pack::new("retries/pack-a", 4 * MIB, 3)
}

/// A pack of eight cells: two parts of four cells.
fn pack8() -> Pack {
    Pack::new("retries/pack-b", 8 * MIB, 5)
}

/// One part at a time through the budget, and no hedge (so every second GET is a retry).
fn config() -> EngineConfig {
    EngineConfig {
        acquire_slots: 1,
        hedge_permits: 0,
        ..EngineConfig::default()
    }
}

fn rig() -> Rig {
    Rig::new(1, config())
}

/// A rig of `permits` budget permits that puts two parts of a pack through at once.
fn rig_of_two_parts(permits: usize) -> Rig {
    Rig::new(
        permits,
        EngineConfig {
            acquire_slots: 2,
            ..config()
        },
    )
}

/// `pack` held by the two hot replicas, `h1` listed first.
fn replicas(rig: &Rig, pack: &Pack) {
    let size = pack.spec.size;
    rig.upstream.add(
        pack,
        location(generation(1), size, vec![hot(1, size), hot(2, size)]),
    );
}

/// `pack` held by `h1` alone.
fn single(rig: &Rig, pack: &Pack) {
    let size = pack.spec.size;
    rig.upstream
        .add(pack, location(generation(1), size, vec![hot(1, size)]));
}

async fn wait_all(rig: &Rig, pack: &Pack) -> WaitOutcome {
    rig.engine.wait(&pack.spec, pack.all(), JobId(1)).await
}

/// Every cell of `pack` present with its bytes.
fn assert_pack_present(rig: &Rig, pack: &Pack) {
    let cells = pack.spec.size / MIB;
    assert_eq!(
        rig.cells(pack),
        vec![CellState::Present; cells as usize],
        "every cell is present"
    );
    assert_eq!(
        rig.entry_bytes(pack, 0, pack.spec.size),
        pack.bytes.to_vec(),
        "the entry holds the pack's bytes"
    );
}

/// The devices GETs went to, in order.
fn devices(rig: &Rig) -> Vec<DeviceRef> {
    rig.upstream
        .gets()
        .iter()
        .map(|g| g.source.device)
        .collect()
}

/// The charge of one demand part on hot node `n`.
fn charge(n: u8) -> Charge {
    Charge {
        class: Class::Demand,
        parts: 1,
        devices: vec![DeviceRef::Hot(NodeId(n))],
        node: Some(NodeId(n)),
    }
}

/// The answer `503` with `Retry-After` `retry_after_ms` and these signals, from `device`.
fn unavailable(
    pack: &Pack,
    device: DeviceRef,
    retry_after_ms: u64,
    signals: DriveSignals,
) -> Script {
    Script::Fail(UpstreamError::Unavailable {
        key: pack.key(),
        device,
        retry_after_ms,
        signals,
    })
}

fn transport(pack: &Pack, device: DeviceRef) -> Script {
    Script::Fail(UpstreamError::Transport {
        key: pack.key(),
        device,
        reason: "connection reset".to_owned(),
    })
}

fn state(drive_state: DriveState) -> DriveSignals {
    DriveSignals {
        drive_state: Some(drive_state),
        ..DriveSignals::default()
    }
}

/// A response observation of `device` (size class 0).
fn response(
    device: DeviceRef,
    first_byte_ms: Option<u32>,
    signals: DriveSignals,
    outcome: Outcome,
) -> Observation {
    Observation::Response {
        device,
        size_class: 0,
        first_byte_ms,
        signals,
        outcome,
    }
}

/// The network observation of a part served instantly by hot node `n`.
fn network(n: u8) -> Observation {
    Observation::Network {
        node: Some(NodeId(n)),
        ms_per_mib: 0,
    }
}

/// An upstream whose answers (the headers) come only when the test releases `gate`, one permit
/// per GET; `delivered` gets a permit once the headers were handed to the engine.
struct LateHeaders {
    inner: Arc<TestUpstream>,
    gate: Arc<Semaphore>,
    delivered: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl Upstream for LateHeaders {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        self.inner.locate(key).await
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        self.gate.acquire().await.expect("gate open").forget();
        let answer = self.inner.get(req).await;
        self.delivered.add_permits(1);
        answer
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        self.inner.report(key, source, range, report).await;
    }
}

/// A rig whose upstream answers only when the returned gate has a permit, and the semaphore that
/// tells (by a permit) that the engine was handed the headers of a GET.
fn late_headers_rig() -> (Rig, Arc<Semaphore>, Arc<Semaphore>) {
    let clock = ManualClock::new();
    let store = Arc::new(MemEntryStore::new());
    let upstream = Arc::new(TestUpstream::new());
    let budget = Arc::new(TestBudget::new(1));
    let gate = Arc::new(Semaphore::new(0));
    let delivered = Arc::new(Semaphore::new(0));
    let parts = EngineParts {
        store: store.clone(),
        upstream: Arc::new(LateHeaders {
            inner: upstream.clone(),
            gate: gate.clone(),
            delivered: delivered.clone(),
        }),
        budget: budget.clone(),
        verifier: Arc::new(Sha256Verifier),
        timer: Arc::new(ManualTimer::new(clock.clone())),
    };
    let engine = FetchEngine::new(parts, config(), &tokio::runtime::Handle::current());
    let rig = Rig {
        clock,
        store,
        upstream,
        budget,
        engine,
    };
    (rig, gate, delivered)
}

/// A GET that has not answered is still alone at 1999 ms; at 2000 ms the retry goes to the other
/// hot replica, whole, takes its own budget permit, and the stalled attempt is told to the budget.
#[tokio::test]
async fn a_stall_before_the_first_byte_is_not_retried_before_the_pre_first_byte_timeout() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(1_999);
        let s = rig.engine.stats();
        assert_eq!(
            (s.total_gets(), s.armed(TimerKind::FirstByte)),
            (1, 1),
            "no retry yet, the timeout is still pending"
        );
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(
        rig.get_ranges(&pack),
        vec![(0, 4 * MIB), (0, 4 * MIB)],
        "the retry asks for the whole part again"
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::GridPart), 1);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(stats.stall_retries, 1);
    assert_eq!(
        rig.budget.charges(),
        vec![charge(1), charge(2)],
        "the retry took a budget permit for the other replica"
    );
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), None, DriveSignals::default(), Outcome::Transport),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ]
    );
}

/// Headers that carry `x-ts-queue-wait-ms` restart the pre-first-byte clock at the headers: the
/// headers come at 1500 ms, so the stall is not retried at the original deadline of 2000 ms but
/// at 3500 ms.
#[tokio::test]
async fn headers_with_a_queue_wait_restart_the_pre_first_byte_clock() {
    let (rig, gate, delivered) = late_headers_rig();
    let pack = pack();
    replicas(&rig, &pack);
    let queued = DriveSignals {
        queue_wait_ms: Some(50),
        ..DriveSignals::default()
    };
    rig.upstream.script(h1(), Script::Hang(queued));
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(1_500);
        gate.add_permits(1);
        delivered.acquire().await.expect("open").forget();
        rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
        rig.clock.advance(500);
        assert_eq!(
            rig.engine.stats().total_gets(),
            1,
            "2000 ms is past the original deadline and the GET is still waited on"
        );
        rig.clock.advance(1_499);
        assert_eq!(rig.engine.stats().total_gets(), 1, "3499 ms: not yet");
        gate.add_permits(1);
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
}

/// The same late headers without a queue wait do not restart the clock: the part is retried at the
/// original deadline of 2000 ms.
#[tokio::test]
async fn headers_without_a_queue_wait_keep_the_original_pre_first_byte_deadline() {
    let (rig, gate, delivered) = late_headers_rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream
        .script(h1(), Script::Hang(DriveSignals::default()));
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(1_500);
        gate.add_permits(1);
        delivered.acquire().await.expect("open").forget();
        rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
        gate.add_permits(1);
        rig.clock.advance(500);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
}

/// With one source the retry goes to the same source again (a fresh connection), and the part is a
/// retry in the counts.
#[tokio::test]
async fn with_a_single_source_the_stall_retry_goes_to_the_same_source() {
    let rig = rig();
    let pack = pack();
    single(&rig, &pack);
    rig.upstream.script(h1(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(2_000);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    let gets = rig.upstream.gets();
    assert_eq!(gets.len(), 2);
    assert_eq!(gets[0].source, gets[1].source, "the same source twice");
    assert_eq!(rig.engine.stats().gets(GetKind::Retry), 1);
}

/// A drive whose headers say `stalled` ends the GET at once: the part is retried on the other
/// replica without the clock moving at all.
#[tokio::test]
async fn a_response_from_a_stalled_drive_is_retried_at_once() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream
        .script(h1(), Script::Hang(state(DriveState::Stalled)));

    let outcome = wait_all(&rig, &pack).await;

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(rig.clock.now_ms(), 0, "the clock never moved");
    assert_eq!(rig.engine.stats().stall_retries, 1);
}

/// After the first byte a stall is timed out after the 500 ms floor, not 2000 ms, and the retry
/// asks only for the cells not yet delivered, beginning at the first missing cell.
#[tokio::test]
async fn a_stall_after_the_first_byte_retries_after_the_stall_timeout_for_the_missing_cells() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), Script::StallAfter(MIB as usize));
    let driver = async {
        rig.until(|s| (s.cells_published, s.armed(TimerKind::Stall)) == (1, 1))
            .await;
        rig.clock.advance(499);
        let s = rig.engine.stats();
        assert_eq!(
            (s.total_gets(), s.armed(TimerKind::Stall)),
            (1, 1),
            "499 ms of silence is below the stall timeout"
        );
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(
        rig.get_ranges(&pack),
        vec![(0, 4 * MIB), (MIB, 3 * MIB)],
        "the retry begins at the first missing cell"
    );
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.cells_published >= 4).await;
    assert_eq!(stats.stall_retries, 1);
    assert_eq!(stats.cells_published, 4, "no cell was published twice");
    assert_eq!(
        stats.bytes_fetched,
        4 * MIB,
        "no delivered cell was fetched again"
    );
    assert_eq!(stats.bytes_wasted, 0);
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), Some(0), DriveSignals::default(), Outcome::Served),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ],
        "the stalled part was served (first byte) and did not finish, so it has no network sample"
    );
}

/// A stream that ends with a transport error after some bytes is retried at once, for the cells
/// not yet delivered only; the partly received cell is dropped and counted wasted.
#[tokio::test]
async fn a_truncated_body_is_retried_at_once_for_the_missing_cells() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream
        .script(h1(), Script::Truncate(MIB as usize + 1_000));

    let outcome = wait_all(&rig, &pack).await;

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(rig.get_ranges(&pack), vec![(0, 4 * MIB), (MIB, 3 * MIB)]);
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.cells_published >= 4).await;
    assert_eq!(stats.bytes_wasted, 1_000, "the half cell was dropped");
    assert_eq!(stats.cells_published, 4);
    assert_eq!(rig.clock.now_ms(), 0, "the clock never moved");
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), Some(0), DriveSignals::default(), Outcome::Served),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ]
    );
}

/// A transport error before any byte is retried at once on the other replica, and told to the
/// budget as a transport outcome.
#[tokio::test]
async fn a_transport_error_is_retried_at_once_on_the_other_replica() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), transport(&pack, h1()));

    let outcome = wait_all(&rig, &pack).await;

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(rig.get_ranges(&pack), vec![(0, 4 * MIB), (0, 4 * MIB)]);
    assert_eq!(rig.clock.now_ms(), 0, "the clock never moved");
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), None, DriveSignals::default(), Outcome::Transport),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ]
    );
}

/// A `503` with `Retry-After` 3000 ms is not retried before 3000 ms (a backoff timer is armed),
/// is told to the budget as busy, and is retried on the other replica once the time has passed.
#[tokio::test]
async fn a_503_waits_its_retry_after_before_the_retry() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream.script(
        h1(),
        unavailable(&pack, h1(), 3_000, DriveSignals::default()),
    );
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::Backoff)) == (1, 1))
            .await;
        rig.clock.advance(2_999);
        let s = rig.engine.stats();
        assert_eq!(
            (s.total_gets(), s.armed(TimerKind::Backoff)),
            (1, 1),
            "no retry before the Retry-After"
        );
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(rig.engine.stats().stall_retries, 0, "a 503 is not a stall");
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), None, DriveSignals::default(), Outcome::Busy),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ]
    );
}

/// A device that answered `503` is the last choice for the next part until the busy window (2 s)
/// has passed: at 1000 ms a new pack goes first to the other replica, at 2000 ms the first listed
/// replica is the first choice again.
#[tokio::test]
async fn a_503_marks_the_device_busy_so_the_next_part_avoids_it_until_the_busy_window_ends() {
    let rig = rig();
    let a = Pack::new("retries/busy-a", 4 * MIB, 7);
    let b = Pack::new("retries/busy-b", 4 * MIB, 8);
    let c = Pack::new("retries/busy-c", 4 * MIB, 9);
    for p in [&a, &b, &c] {
        replicas(&rig, p);
    }
    rig.upstream
        .script(h1(), unavailable(&a, h1(), 1_000, DriveSignals::default()));
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::Backoff)) == (1, 1))
            .await;
        rig.clock.advance(1_000);
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &a), driver);
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(wait_all(&rig, &b).await, WaitOutcome::Ready);
    rig.clock.advance(1_000);
    assert_eq!(wait_all(&rig, &c).await, WaitOutcome::Ready);

    let per_pack: Vec<(PackKey, DeviceRef)> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| (g.key.clone(), g.source.device))
        .collect();
    assert_eq!(
        per_pack,
        vec![
            (a.key(), h1()),
            (a.key(), h2()),
            (b.key(), h2()),
            (c.key(), h1()),
        ],
        "b avoids the busy h1; c, after the window, goes to h1 again"
    );
}

/// A `503` whose drive says `stalled` is not a load signal and not waited on: it is retried at
/// once on the other replica (its `Retry-After` is ignored) and told to the budget as not load.
#[tokio::test]
async fn a_503_from_a_stalled_drive_is_retried_at_once_and_is_not_a_load_signal() {
    not_load_503(DriveState::Stalled).await;
}

/// A `503` whose drive says `indexing` is not a load signal and not waited on: it is retried at
/// once on the other replica and told to the budget as not load.
#[tokio::test]
async fn a_503_from_an_indexing_drive_is_retried_at_once_and_is_not_a_load_signal() {
    not_load_503(DriveState::Indexing).await;
}

async fn not_load_503(drive_state: DriveState) {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    let signals = state(drive_state);
    rig.upstream
        .script(h1(), unavailable(&pack, h1(), 5_000, signals));

    let outcome = wait_all(&rig, &pack).await;

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2()]);
    assert_eq!(rig.clock.now_ms(), 0, "the clock never moved");
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), None, signals, Outcome::NotLoad),
            response(h2(), Some(0), DriveSignals::default(), Outcome::Served),
            network(2),
        ]
    );
}

/// While the only drive is in a decrease state (it answered `503` 1000 ms ago, the busy window is
/// 2000 ms) a transport error is still retried at once: the clock moves only for the `Retry-After`.
#[tokio::test]
async fn in_a_decrease_state_a_transport_error_is_retried_at_once() {
    let pack = pack();
    decrease_state_then(&pack, transport(&pack, h1())).await;
}

/// While the only drive is in a decrease state a response that says `stalled` is still retried at
/// once.
#[tokio::test]
async fn in_a_decrease_state_a_stalled_response_is_retried_at_once() {
    let pack = pack();
    decrease_state_then(&pack, Script::Hang(state(DriveState::Stalled))).await;
}

async fn decrease_state_then(pack: &Pack, second: Script) {
    let rig = rig();
    single(&rig, pack);
    rig.upstream.script(
        h1(),
        unavailable(pack, h1(), 1_000, DriveSignals::default()),
    );
    rig.upstream.script(h1(), second);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::Backoff)) == (1, 1))
            .await;
        rig.clock.advance(1_000);
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, pack);
    assert_eq!(devices(&rig), vec![h1(), h1(), h1()]);
    assert_eq!(
        rig.clock.now_ms(),
        1_000,
        "only the Retry-After was waited for"
    );
}

/// Two parts stalling on the same drive at the same moment: the first is retried at once, the
/// second waits for the 2 s window (a backoff timer) and is retried when it has passed.
#[tokio::test]
async fn two_parts_stalling_on_one_drive_get_one_stall_retry_per_window() {
    let rig = rig_of_two_parts(2);
    let pack = pack8();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), Script::NoAnswer);
    rig.upstream.script(h1(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (2, 2))
            .await;
        rig.clock.advance(2_000);
        rig.until(|s| (s.gets(GetKind::Retry), s.armed(TimerKind::Backoff)) == (1, 1))
            .await;
        rig.clock.advance(1_999);
        let s = rig.engine.stats();
        assert_eq!(
            (s.gets(GetKind::Retry), s.armed(TimerKind::Backoff)),
            (1, 1),
            "the second stall retry waits for the window"
        );
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Retry) == 2).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    let mut retries: Vec<(u64, DeviceRef)> = rig
        .upstream
        .gets()
        .iter()
        .skip(2)
        .map(|g| (g.range.offset, g.source.device))
        .collect();
    retries.sort();
    assert_eq!(
        retries,
        vec![(0, h2()), (4 * MIB, h2())],
        "both parts were retried on the other replica"
    );
    assert_eq!(rig.engine.stats().stall_retries, 2);
}

/// The window belongs to a drive: parts stalling on two different drives are both retried at
/// once, on their own source again (each holds a different half of the object).
#[tokio::test]
async fn stalls_on_two_different_drives_are_each_retried_at_once() {
    let rig = rig_of_two_parts(2);
    let pack = pack8();
    let second_half = Source {
        extent: ByteRange {
            offset: 4 * MIB,
            len: 4 * MIB,
        },
        ..hot(2, 0)
    };
    rig.upstream.add(
        &pack,
        location(
            generation(1),
            pack.spec.size,
            vec![hot(1, 4 * MIB), second_half],
        ),
    );
    rig.upstream.script(h1(), Script::NoAnswer);
    rig.upstream.script(h2(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (2, 2))
            .await;
        rig.clock.advance(2_000);
        rig.until(|s| s.gets(GetKind::Retry) == 2).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    let mut all: Vec<(u64, DeviceRef)> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| (g.range.offset, g.source.device))
        .collect();
    all.sort();
    assert_eq!(
        all,
        vec![(0, h1()), (0, h1()), (4 * MIB, h2()), (4 * MIB, h2())]
    );
    assert_eq!(rig.engine.stats().stall_retries, 2);
    assert_eq!(rig.engine.stats().armed(TimerKind::Backoff), 0);
}

/// A second stall on the same drive exactly one window after its last stall retry is retried at
/// once: the window is a floor between two stall retries of a drive, not a penalty.
#[tokio::test]
async fn a_stall_retry_on_a_drive_is_not_delayed_once_its_window_has_passed() {
    let rig = rig();
    let pack = pack();
    single(&rig, &pack);
    rig.upstream.script(h1(), Script::NoAnswer);
    rig.upstream.script(h1(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(2_000);
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (2, 1))
            .await;
        rig.clock.advance(2_000);
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h1(), h1()]);
    let stats = rig.engine.stats();
    assert_eq!(stats.stall_retries, 2);
    assert_eq!(stats.armed(TimerKind::Backoff), 0);
}

/// A cold shard part that stalls is retried as a degraded read (the reconstructor on another node
/// reading the stripe's other drives), which takes one charge per source drive.
#[tokio::test]
async fn a_stalled_cold_shard_is_retried_as_a_degraded_read() {
    let rig = Rig::new(4, config());
    let pack = pack();
    let size = pack.spec.size;
    let sources = [drive(2, 6), drive(3, 7)];
    rig.upstream.add(
        &pack,
        location(
            generation(1),
            size,
            vec![cold(1, 5, 0, size), degraded(9, &sources, 0, size)],
        ),
    );
    rig.upstream.script(drive(1, 5), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(2_000);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    let gets = rig.upstream.gets();
    assert_eq!(gets.len(), 2);
    assert!(matches!(gets[0].source.kind, SourceKind::Cold { .. }));
    assert!(matches!(gets[1].source.kind, SourceKind::Degraded { .. }));
    assert_eq!(gets[1].range, gets[0].range, "the whole part again");
    assert_eq!(
        rig.budget.charges(),
        vec![
            Charge {
                class: Class::Demand,
                parts: 1,
                devices: vec![drive(1, 5)],
                node: Some(NodeId(1)),
            },
            Charge {
                class: Class::Demand,
                parts: 2,
                devices: sources.to_vec(),
                node: Some(NodeId(9)),
            },
        ],
        "the degraded retry is charged to the stripe's source drives"
    );
}

/// After three failed attempts the engine asks `LOCATE` again, and on a hit continues: the fourth
/// GET (a retry like the others) succeeds and the part completes.
#[tokio::test]
async fn after_max_attempts_the_engine_asks_locate_again_and_continues_when_found() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), transport(&pack, h1()));
    rig.upstream.script(h1(), transport(&pack, h1()));
    rig.upstream.script(h2(), transport(&pack, h2()));

    let outcome = wait_all(&rig, &pack).await;

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h2(), h1(), h2()]);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::GridPart), 1);
    assert_eq!(stats.gets(GetKind::Retry), 3);
    assert_eq!(
        stats.locates, 2,
        "the first LOCATE and the one after three failures"
    );
    assert_eq!(rig.upstream.locates(), vec![pack.key(), pack.key()]);
}

/// Attempts that keep failing past `unavailable_wait` (10 minutes) end in EIO for the waiter while
/// the location keeps being found (one more `LOCATE` after the first three failures), and the
/// entry stays resumable: a later wait fetches the cells again and is Ready.
#[tokio::test]
async fn attempts_that_keep_failing_past_unavailable_wait_end_in_eio_and_the_entry_resumes() {
    let rig = rig();
    let pack = pack();
    single(&rig, &pack);
    for _ in 0..6 {
        rig.upstream.script(
            h1(),
            unavailable(&pack, h1(), 300_000, DriveSignals::default()),
        );
    }
    let driver = async {
        for gets in [1, 2, 4, 5] {
            rig.until(|s| (s.total_gets(), s.armed(TimerKind::Backoff)) == (gets, 1))
                .await;
            rig.clock.advance(300_000);
        }
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Eio { cell: 0 });
    let stats = rig.engine.stats();
    assert_eq!(stats.total_gets(), 6);
    assert_eq!(
        stats.locates, 2,
        "one LOCATE when the attempts first ran out"
    );
    assert_eq!(stats.cells_published, 0);
    assert_eq!(
        rig.cells(&pack),
        vec![CellState::Absent; 4],
        "the cells are released, not lost"
    );

    assert_eq!(wait_all(&rig, &pack).await, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(rig.engine.stats().total_gets(), 7);
}

/// The first-byte time of a served part is told to the budget in ms of the injected clock (40 ms
/// here), with the part's signals, and the finished part's network time follows.
#[tokio::test]
async fn a_served_part_is_observed_with_its_first_byte_time() {
    let rig = rig();
    let pack = pack();
    replicas(&rig, &pack);
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream.script(h1(), Script::Gated(gate.clone()));
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        rig.clock.advance(40);
        gate.add_permits(1);
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1()]);
    assert_eq!(
        rig.budget.observations(),
        vec![
            response(h1(), Some(40), DriveSignals::default(), Outcome::Served),
            network(1),
        ]
    );
}

/// A retry waits for a budget permit like any part: with one permit, the part of the pack's other
/// half that was already waiting is served first, and only then the retry of the stalled part.
#[tokio::test]
async fn a_retry_waits_for_a_budget_permit_behind_a_part_already_waiting() {
    let rig = rig_of_two_parts(1);
    let pack = pack8();
    replicas(&rig, &pack);
    rig.upstream.script(h1(), Script::NoAnswer);
    let driver = async {
        rig.until(|s| (s.total_gets(), s.armed(TimerKind::FirstByte)) == (1, 1))
            .await;
        assert_eq!(
            rig.budget.charges(),
            vec![charge(1), charge(1)],
            "the second part queued for the permit the stalled one holds"
        );
        rig.clock.advance(2_000);
        rig.until(|s| s.gets(GetKind::Retry) == 1).await;
    };
    let (outcome, ()) = tokio::join!(wait_all(&rig, &pack), driver);

    assert_eq!(outcome, WaitOutcome::Ready);
    assert_pack_present(&rig, &pack);
    assert_eq!(devices(&rig), vec![h1(), h1(), h2()]);
    assert_eq!(
        rig.get_ranges(&pack),
        vec![(0, 4 * MIB), (4 * MIB, 4 * MIB), (0, 4 * MIB)],
        "the waiting part went before the retry"
    );
    assert_eq!(
        rig.budget.charges(),
        vec![charge(1), charge(1), charge(2)],
        "the retry was charged after the part already waiting"
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::GridPart), 2);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}
