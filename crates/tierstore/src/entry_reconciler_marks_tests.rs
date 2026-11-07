//! The leader's pass over the markers (`entry_reconciler/marks.rs`, `copier.rs`, `reap.rs`): its
//! gate on the format, the copy of a `DIRTY` marker and the races of a copy with the writes of
//! other actors, which are driven one store call at a time through a gate. The tombstone hold and
//! the orphan wait, the reap and the takeover, and the usage and the paging are in the modules
//! below. No test sleeps or reads a clock: the monotonic clock is moved by hand.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use super::{EntryReconciler, EntryReconcilerConfig, StepReport};
use crate::format::{Format, FormatPhase, format_body};
use crate::journal::{FORMAT_KEY, USAGE_KEY, Usage, proxy_key};
use crate::{
    Backend, Error, FaultyBackend, Gate, GateBackend, MARKER_VERSION, MarkMove, MarkSeen, Marker,
    MarkerState, MarkerWhy, Markers, MarkersConfig, MemoryBackend, Mono, Op, Outcome, Phase,
    ReconcilerConfig, RecordingSleeper, Sleeper, TestClock, decode_marker, encode_marker,
};

#[path = "entry_reconciler_marks_tests/pages.rs"]
mod pages;
#[path = "entry_reconciler_marks_tests/reap.rs"]
mod reap;
#[path = "entry_reconciler_marks_tests/rollback.rs"]
mod rollback;
#[path = "entry_reconciler_marks_tests/tombstone.rs"]
mod tombstone;

/// The name of the marks bucket in the format record and in the reconciler.
const BUCKET: &str = "tierstore-marks";

/// A wait that never ends, for the renewal timer of the lease: no test sees a renewal.
struct Pending;

#[async_trait]
impl Sleeper for Pending {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// A hot, a cold and a marks tier in memory, each behind a fault injector that counts its calls,
/// the wall clock they stamp their objects with and the monotonic clock of the leader.
struct Rig {
    clock: Arc<TestClock>,
    mono: Arc<Mono>,
    hot: Arc<MemoryBackend>,
    cold: Arc<MemoryBackend>,
    marks: Arc<MemoryBackend>,
    hot_calls: Arc<FaultyBackend>,
    cold_calls: Arc<FaultyBackend>,
    marks_calls: Arc<FaultyBackend>,
    planted: AtomicU64,
}

impl Rig {
    /// The tiers with the tags of the memory backend (SHA-256), which the leader cannot compare
    /// across tiers.
    fn new() -> Self {
        Self::over(false)
    }

    /// The tiers with tags of 32 hex digits, the shape of an S3 ETag, which it can.
    fn with_md5_tags() -> Self {
        Self::over(true)
    }

    fn over(short: bool) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let tier = |short: bool| {
            let tier = MemoryBackend::with_clock(clock.clone());
            Arc::new(if short { tier.with_short_tags() } else { tier })
        };
        let (hot, cold, marks) = (tier(short), tier(short), tier(false));
        Self {
            hot_calls: Arc::new(FaultyBackend::new(hot.clone())),
            cold_calls: Arc::new(FaultyBackend::new(cold.clone())),
            marks_calls: Arc::new(FaultyBackend::new(marks.clone())),
            clock,
            mono: Arc::new(Mono::new()),
            hot,
            cold,
            marks,
            planted: AtomicU64::new(0),
        }
    }

    fn config(&self, holder: &str) -> EntryReconcilerConfig {
        let mut base = ReconcilerConfig::new(holder, 1 << 20, self.clock.clone());
        base.mono = self.mono.clone();
        base.sleeper = Arc::new(Pending);
        let mut config = EntryReconcilerConfig::new(base);
        config.waiter = Arc::new(RecordingSleeper::new());
        config
    }

    /// The leader `holder` over the three tiers, given the marks bucket.
    fn leader(&self, holder: &str) -> EntryReconciler {
        self.leader_over(self.hot_calls.clone(), self.cold_calls.clone(), holder)
    }

    /// The leader `holder` over the three tiers, its settings changed by `tweak`.
    fn leader_with(
        &self,
        holder: &str,
        tweak: impl FnOnce(&mut EntryReconcilerConfig),
    ) -> EntryReconciler {
        let mut config = self.config(holder);
        tweak(&mut config);
        let reconciler =
            EntryReconciler::new(self.hot_calls.clone(), self.cold_calls.clone(), config);
        reconciler
            .with_marks(self.marks_calls.clone(), BUCKET)
            .expect("the settings hold together")
    }

    /// As [`Rig::leader`] over the given hot and cold tiers.
    fn leader_over(
        &self,
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        holder: &str,
    ) -> EntryReconciler {
        let reconciler = EntryReconciler::new(hot, cold, self.config(holder));
        reconciler
            .with_marks(self.marks_calls.clone(), BUCKET)
            .expect("the default settings of the markers hold together")
    }

    /// The format object of the hot store, replaced by one in `phase`.
    async fn format(&self, phase: FormatPhase, bucket: Option<&str>) {
        let format = Format {
            phase,
            epoch: Some(3),
            marks_bucket: bucket.map(str::to_owned),
        };
        self.hot.delete(FORMAT_KEY).await.ok();
        self.hot
            .put(FORMAT_KEY, format_body(&format))
            .await
            .unwrap();
    }

    /// The format of the activated store: `active-marks`, naming the bucket of the reconcilers.
    async fn activate(&self) {
        self.format(FormatPhase::ActiveMarks, Some(BUCKET)).await;
    }

    /// Moves the monotonic clock on.
    fn pass(&self, span: Duration) {
        self.mono.advance(span);
    }

    /// Stores `marker` as a writer would, under the name of its key, with a nonce of its own.
    async fn plant(&self, mut marker: Marker) -> MarkSeen {
        let n = self.planted.fetch_add(1, Ordering::SeqCst);
        marker.nonce = format!("planted-{n}");
        self.marks.delete(&marker.key).await.ok();
        self.marks
            .put(&marker.key, encode_marker(&marker))
            .await
            .unwrap();
        self.seen(&marker.key).await.expect("just planted")
    }

    /// The marker of `key` as stored, with its tag, read behind the back of every actor.
    async fn seen(&self, key: &str) -> Option<MarkSeen> {
        let (body, tag) = self.marks.get_tagged(key, None).await.ok()?;
        Some(MarkSeen {
            marker: decode_marker(&body).unwrap(),
            tag,
        })
    }

    /// The state, `gone` and `dirty` of the marker of `key`.
    async fn shape(&self, key: &str) -> Option<(MarkerState, bool, bool)> {
        let seen = self.seen(key).await?;
        let marker = seen.marker;
        Some((marker.st, marker.gone, marker.dirty))
    }

    /// The marker `from` after `actor` moved it by `mv`, as the store holds it. The move must
    /// have been made.
    async fn step_marker(&self, actor: &Markers, from: &MarkSeen, mv: MarkMove) -> MarkSeen {
        let outcome = actor.advance(from, mv).await.unwrap();
        assert!(matches!(outcome, Outcome::Done(_)));
        self.seen(&from.marker.key)
            .await
            .expect("the marker stands")
    }

    /// A writer `id` of markers over the marks bucket, not behind the fault injector.
    fn writer(&self, id: &str) -> Markers {
        let markers = Markers::with_boot(
            self.marks.clone(),
            id,
            MarkersConfig::default(),
            self.mono.clone(),
            Arc::new(RecordingSleeper::new()),
            id,
        );
        markers.expect("the default settings hold together")
    }

    /// Registers the proxy `id`, as a heartbeat of it would have.
    async fn register(&self, id: &str) {
        self.hot
            .put(&proxy_key(id), Bytes::from_static(b"{}"))
            .await
            .unwrap();
    }

    /// One heartbeat of the proxy `id`: the clocks move on and its registration is written again,
    /// so that the leader sees it change.
    async fn beat(&self, id: &str, span: Duration) {
        self.mono.advance(span);
        self.clock.advance(u64::try_from(span.as_millis()).unwrap());
        self.hot.delete(&proxy_key(id)).await.ok();
        self.register(id).await;
    }

    /// The usage object the leader published.
    async fn usage(&self) -> Usage {
        let body = self.hot.get(USAGE_KEY, None).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn cold_body(&self, key: &str) -> Option<Bytes> {
        self.cold.get(key, None).await.ok()
    }

    async fn hot_body(&self, key: &str) -> Option<Bytes> {
        self.hot.get(key, None).await.ok()
    }
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn marker(key: &str, st: MarkerState, gone: bool, dirty: bool) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty,
        why: None,
        owner: None,
        nonce: String::new(),
        extra: serde_json::Map::new(),
    }
}

/// A `DIRTY` marker that does not say `gone`.
fn dirty(key: &str) -> Marker {
    marker(key, MarkerState::Dirty, false, true)
}

/// A `DIRTY` marker that says `gone`: the key was deleted.
fn gone(key: &str) -> Marker {
    marker(key, MarkerState::Dirty, true, true)
}

/// A `LOCK` of `owner` for `why`.
fn lock(key: &str, why: MarkerWhy, owner: &str, gone: bool) -> Marker {
    let mut lock = marker(key, MarkerState::Lock, gone, why.owes_cold_a_copy());
    lock.why = Some(why);
    lock.owner = Some(owner.to_owned());
    lock
}

/// A step that has run until the gate took its call.
async fn park(step: &mut (impl Future + Unpin), gate: &Gate) {
    tokio::select! {
        _ = &mut *step => panic!("the step finished instead of parking"),
        () = gate.parked() => {}
    }
}

/// A leader `r1` over the three tiers of `rig`, with the hot object `k` of `hot`, the cold object
/// `k` of `cold` if any, and a `DIRTY` marker, under the activated format.
async fn owed(rig: &Rig, hot: &'static str, cold: Option<&'static str>) -> EntryReconciler {
    rig.activate().await;
    rig.hot.put("k", bytes(hot)).await.unwrap();
    if let Some(cold) = cold {
        rig.cold.put("k", bytes(cold)).await.unwrap();
    }
    rig.plant(dirty("k")).await;
    rig.leader("r1")
}

fn report(changed: usize, pinned: usize) -> StepReport {
    StepReport {
        active: true,
        held: true,
        changed,
        pinned,
        ..StepReport::default()
    }
}

/// Asserts that nothing is read or written in the marks bucket, and no marker acted on, unless
/// the format says `active-marks` with the marks bucket the leader was given: not with no format
/// object, `drain`, `active` (the entries are worked), `active-marks` with no name or another
/// name, nor by a leader that was given no marks bucket. With the right format the marker is
/// copied and cleared.
#[tokio::test]
async fn the_marker_pass_is_inert_until_the_format_says_active_marks_with_the_configured_bucket() {
    let rig = Rig::new();
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    let mut without = EntryReconciler::new(
        rig.hot_calls.clone(),
        rig.cold_calls.clone(),
        rig.config("r2"),
    );
    assert_eq!(leader.step().await, StepReport::default());
    for (phase, bucket) in [
        (FormatPhase::Drain, None),
        (FormatPhase::Active, None),
        (FormatPhase::ActiveMarks, None),
        (FormatPhase::ActiveMarks, Some("another-marks")),
    ] {
        rig.format(phase, bucket).await;
        let done = leader.step().await;
        assert_eq!(done.active, phase == FormatPhase::Active, "{phase:?}");
        assert!(done.failed.is_empty());
    }
    rig.activate().await;
    assert_eq!(without.step().await, StepReport::default());
    assert!(rig.marks_calls.call_log().is_empty());
    assert!(rig.cold_body("k").await.is_none());
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );

    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v")));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts the copy of a key cold lacks: one hot HEAD, a cold HEAD, the marker read again, the
/// hot object streamed to a create-only cold PUT, then two writes to the marker, the CAS to
/// `CLEARING` and the DELETE (design 5.9: two hot writes), and no other write anywhere.
#[tokio::test]
async fn the_copier_streams_hot_to_cold_by_create_only_and_clears_by_cas() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v1", None).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));
    assert_eq!(rig.hot_body("k").await, Some(bytes("v1")));
    assert_eq!(rig.shape("k").await, None);
    let cold = &rig.cold_calls;
    assert_eq!(
        (
            cold.calls(Op::Head),
            cold.calls(Op::Put),
            cold.calls(Op::PutIfMatch)
        ),
        (1, 1, 0)
    );
    assert_eq!(cold.calls(Op::Delete), 0);
    let marks = &rig.marks_calls;
    assert_eq!(
        (
            marks.calls(Op::PutIfMatch),
            marks.calls(Op::Delete),
            marks.calls(Op::Put)
        ),
        (1, 1, 0)
    );
    assert_eq!(marks.calls_on(Op::Get, "k"), 2);
    assert_eq!(
        rig.hot_calls.calls_on(Op::Put, "k") + rig.hot_calls.calls_on(Op::Delete, "k"),
        0
    );
}

/// Asserts that a cold object with another tag is replaced on its tag and never deleted first, so
/// that cold never shows a gap: one cold `If-Match` PUT, no cold DELETE and no create-only PUT.
#[tokio::test]
async fn the_copier_replaces_a_cold_version_by_its_tag_and_never_shows_a_gap() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v2", Some("v1")).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v2")));
    assert_eq!(rig.shape("k").await, None);
    let cold = &rig.cold_calls;
    assert_eq!(
        (
            cold.calls(Op::PutIfMatch),
            cold.calls(Op::Put),
            cold.calls(Op::Delete)
        ),
        (1, 0, 0)
    );
}

/// Asserts that two tags that are plain MD5s, equal, with equal sizes, mean there is nothing to
/// copy (no read of the hot bytes, no cold write), while tags of the other shape (here SHA-256, as
/// a `dir` hot tier has) that happen to be equal are copied over by tag; and that the marker is
/// cleared either way.
#[tokio::test]
async fn a_cold_copy_with_the_same_plain_md5_tag_and_size_is_not_copied_again() {
    for (rig, copies) in [(Rig::with_md5_tags(), 0), (Rig::new(), 1)] {
        let mut leader = owed(&rig, "v1", Some("v1")).await;
        assert_eq!(leader.step().await, report(1, 0));
        assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), copies);
        assert_eq!(rig.hot_calls.calls_on(Op::Get, "k"), copies);
        assert_eq!(rig.shape("k").await, None);
    }
}

/// Asserts that a key this leader copied and cleared, and whose marker comes back with the hot
/// object unchanged (a put of the same bytes), is not copied again: the pair of tags it recorded
/// says cold has the bytes, though the tags are not ones that tiers can compare.
#[tokio::test]
async fn a_key_this_leader_copied_and_that_is_marked_again_with_the_same_bytes_is_not_copied_twice()
{
    let rig = Rig::new();
    let mut leader = owed(&rig, "v1", None).await;
    assert_eq!(leader.step().await, report(1, 0));
    rig.plant(dirty("k")).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(
        rig.cold_calls.calls(Op::Put) + rig.cold_calls.calls(Op::PutIfMatch),
        1
    );
    assert_eq!(rig.hot_calls.calls_on(Op::Get, "k"), 1);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that the same size and another tag is copied: tags that cannot say the bytes are equal
/// are not taken to.
#[tokio::test]
async fn a_cold_copy_with_the_same_size_and_another_tag_is_replaced() {
    let rig = Rig::with_md5_tags();
    let mut leader = owed(&rig, "v2", Some("v1")).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v2")));
}

/// Asserts that a cold object of another size is replaced even if its tag equals the hot tag.
#[tokio::test]
async fn a_cold_copy_of_another_size_is_replaced_whatever_its_tag() {
    let rig = Rig::with_md5_tags();
    rig.activate().await;
    rig.hot.put("k", bytes("v1")).await.unwrap();
    rig.cold.put("k", bytes("v1-and-more")).await.unwrap();
    rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));
}

/// Asserts that when the marker moved after the cold HEAD (a cold landing locked the key, wrote
/// cold and left it `DIRTY`) the copier gives up on the key before it writes cold: the cold bytes
/// the landing wrote stay and the copier made no cold write, and the key is looked at again at
/// the next pass, which copies by the landing's tag.
#[tokio::test]
async fn the_copier_does_not_copy_over_a_cold_landing_when_the_marker_moved_after_its_cold_head() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("old-hot")).await.unwrap();
    let a = rig.plant(dirty("k")).await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let gate = gated.gate(Op::Head, "k", Phase::Post);
    let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;

    let landing = rig.writer("p1");
    let held = rig
        .step_marker(&landing, &a, MarkMove::Lock(MarkerWhy::ColdLand))
        .await;
    rig.cold.put("k", bytes("landed")).await.unwrap();
    let release = MarkMove::Release { changed: true };
    rig.step_marker(&landing, &held, release).await;
    gate.release();

    assert_eq!(step.await, report(0, 1));
    assert_eq!(rig.cold_body("k").await, Some(bytes("landed")));
    assert_eq!(
        rig.cold_calls.calls(Op::Put) + rig.cold_calls.calls(Op::PutIfMatch),
        0
    );
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );
}

/// Asserts that a write to the marker during the copy (an overwrite that locked the key, replaced
/// hot and released it `DIRTY`) makes the clear fail, leaves the marker in place with the cold
/// object written untrusted, and that the next pass copies the new hot object by the cold tag and
/// clears.
#[tokio::test]
async fn an_overwrite_during_a_copy_makes_the_clear_fail_and_the_key_is_copied_again() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v1")).await.unwrap();
    let a = rig.plant(dirty("k")).await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Post);
    let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));

    let overwriter = rig.writer("p1");
    let lock = MarkMove::Lock(MarkerWhy::Overwrite);
    let held = rig.step_marker(&overwriter, &a, lock).await;
    rig.hot.delete("k").await.unwrap();
    rig.hot.put("k", bytes("v2")).await.unwrap();
    let release = MarkMove::Release { changed: true };
    rig.step_marker(&overwriter, &held, release).await;
    gate.release();

    assert_eq!(step.await, report(0, 1));
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v2")));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), 1);
}

/// Asserts that a cold write by a leader whose view of cold is stale fails on its condition and
/// changes nothing: after its cold HEAD found no object another leader created one (the
/// create-only PUT is refused), and after its HEAD found an object another leader replaced it
/// (the `If-Match` PUT is refused). The cold object of the other stays, and the key is looked at
/// again.
#[tokio::test]
async fn a_cold_write_by_a_leader_that_lost_the_lease_fails_on_its_condition() {
    for before in [None, Some("old")] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("stale")).await.unwrap();
        if let Some(before) = before {
            rig.cold.put("k", bytes(before)).await.unwrap();
        }
        rig.plant(dirty("k")).await;
        let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
        let gate = gated.gate(Op::Head, "k", Phase::Post);
        let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
        let mut step = Box::pin(leader.step());
        park(&mut step, &gate).await;
        rig.cold.delete("k").await.ok();
        rig.cold.put("k", bytes("new leader")).await.unwrap();
        gate.release();
        assert_eq!(step.await, report(0, 1), "{before:?}");
        assert_eq!(rig.cold_body("k").await, Some(bytes("new leader")));
        assert_eq!(
            rig.shape("k").await,
            Some((MarkerState::Dirty, false, true))
        );
    }
}

/// Asserts that a failed copy is tried again after the retry delay and not before: the cold tier
/// is down for the first step, which fails and reports the error, the next step at once leaves the
/// key alone (backed off), and a step after the delay copies it.
#[tokio::test]
async fn a_copy_that_fails_is_tried_again_after_the_retry_delay() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v1", None).await;
    rig.cold_calls.set_down(true);
    let first = leader.step().await;
    assert_eq!(first.failed.len(), 1);
    assert_eq!(first.changed, 0);
    rig.cold_calls.set_down(false);
    let second = leader.step().await;
    assert_eq!((second.backed_off, second.changed), (1, 0));
    assert!(rig.cold_body("k").await.is_none());
    rig.pass(Duration::from_secs(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));
}

/// Asserts that a cold write that fails after the HEAD, and a cold DELETE of a `gone` key that
/// fails, are reported, leave the marker as it is, and are tried again after the retry delay.
#[tokio::test]
async fn a_cold_write_or_delete_that_fails_is_reported_and_tried_again() {
    for gone_key in [false, true] {
        let rig = Rig::new();
        rig.activate().await;
        if gone_key {
            rig.cold.put("k", bytes("deleted")).await.unwrap();
            rig.plant(gone("k")).await;
            rig.cold_calls.fail_next(Op::Delete, 1);
        } else {
            rig.hot.put("k", bytes("v")).await.unwrap();
            rig.plant(dirty("k")).await;
            rig.cold_calls.fail_next(Op::Put, 1);
        }
        let mut leader = rig.leader("r1");
        let failed = leader.step().await;
        assert_eq!((failed.changed, failed.failed.len()), (0, 1), "{gone_key}");
        assert!(rig.shape("k").await.is_some());
        rig.pass(Duration::from_secs(1));
        assert_eq!(leader.step().await, report(1, 0), "{gone_key}");
    }
}

/// Asserts that a leader that does not hold the lease reads and writes no marker: another holder
/// has it.
#[tokio::test]
async fn a_leader_without_the_lease_reads_no_marker() {
    let rig = Rig::new();
    let mut holder = owed(&rig, "v1", None).await;
    assert_eq!(holder.step().await, report(1, 0));
    rig.plant(dirty("k")).await;
    let calls = rig.marks_calls.call_log().len();
    let mut other = rig.leader("r2");
    let done = other.step().await;
    assert_eq!((done.active, done.held), (true, false));
    assert_eq!(rig.marks_calls.call_log().len(), calls);
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );
}

/// Asserts that a copy that reads the marker again and finds it gone (the marker was cleared by a
/// put's own cleanup between the pass's read and its second read) is given up on, and that a hot
/// object that vanishes after its HEAD is given up on too: nothing is written to cold.
#[tokio::test]
async fn a_marker_or_a_hot_object_that_vanishes_during_the_copy_is_given_up_on() {
    for marker_goes in [true, false] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.plant(dirty("k")).await;
        let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
        let gate = gated.gate(Op::Head, "k", Phase::Post);
        let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
        let mut step = Box::pin(leader.step());
        park(&mut step, &gate).await;
        if marker_goes {
            rig.marks.delete("k").await.unwrap();
        } else {
            rig.hot.delete("k").await.unwrap();
        }
        gate.release();
        assert_eq!(step.await, report(0, 1), "{marker_goes}");
        assert_eq!(rig.cold_calls.calls(Op::Put), 0);
        assert!(rig.cold_body("k").await.is_none());
    }
}
