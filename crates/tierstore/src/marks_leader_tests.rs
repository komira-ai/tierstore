//! The lease of the leader and the two things every holder of a lock does besides its work: write
//! the lock again when it is old, and end it. The rig is shared by the tests of the eviction, the
//! copy-back and the sweep: a hot, a cold and a marks bucket in memory behind fault-injecting
//! wrappers, a wall clock and a monotonic clock that a test moves, and the format object.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;

use crate::format::{Format, FormatPhase, format_body};
use crate::journal::FORMAT_KEY;
use crate::marks_leader::{release, renewed};
use crate::{
    Backend, Creation, Error, FaultyBackend, Gate, LeaderLease, MARKER_VERSION, MarkBirth,
    MarkMove, MarkSeen, Marker, MarkerState, MarkerWhy, Markers, MarkersConfig, MemoryBackend,
    Mono, Op, Outcome, ReconcilerConfig, RecordingSleeper, TestClock, Written, decode_marker,
    encode_marker,
};

/// Three buckets in memory, each behind a wrapper that fails and counts, and the clocks.
pub(crate) struct Rig {
    pub(crate) clock: Arc<TestClock>,
    pub(crate) mono: Arc<Mono>,
    pub(crate) hot_mem: Arc<MemoryBackend>,
    pub(crate) cold_mem: Arc<MemoryBackend>,
    pub(crate) marks_mem: Arc<MemoryBackend>,
    pub(crate) hot: Arc<FaultyBackend>,
    pub(crate) cold: Arc<FaultyBackend>,
    pub(crate) marks: Arc<FaultyBackend>,
    pub(crate) waits: Arc<RecordingSleeper>,
}

pub(crate) fn bytes(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

pub(crate) fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

impl Rig {
    pub(crate) fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let marks_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        Self {
            hot: Arc::new(FaultyBackend::new(hot_mem.clone())),
            cold: Arc::new(FaultyBackend::new(cold_mem.clone())),
            marks: Arc::new(FaultyBackend::new(marks_mem.clone())),
            clock,
            mono: Arc::new(Mono::new()),
            hot_mem,
            cold_mem,
            marks_mem,
            waits: Arc::new(RecordingSleeper::new()),
        }
    }

    /// Writes the format object of `phase`, replacing any.
    pub(crate) async fn format(&self, phase: FormatPhase) {
        let format = Format {
            phase,
            epoch: Some(3),
            marks_bucket: Some("marks".to_owned()),
        };
        self.hot_mem.delete(FORMAT_KEY).await.unwrap();
        self.hot_mem
            .put(FORMAT_KEY, format_body(&format))
            .await
            .unwrap();
    }

    pub(crate) async fn activate(&self) {
        self.format(FormatPhase::ActiveMarks).await;
    }

    /// The settings of a leader `holder`, over the rig's clocks, with a lease that outlasts every
    /// test.
    pub(crate) fn base(&self, holder: &str) -> ReconcilerConfig {
        let mut base = ReconcilerConfig::new(holder, 1 << 20, self.clock.clone());
        base.mono = self.mono.clone();
        base.lease_ttl = secs(3600);
        base
    }

    /// The lease of `holder` over the hot tier, taken.
    pub(crate) async fn lease(&self, holder: &str) -> LeaderLease {
        let lease = LeaderLease::new(self.hot.clone(), &self.base(holder));
        assert!(lease.take().await);
        lease
    }

    /// The markers of the actor `id` over the marks bucket (nonces `<id>.0`, `<id>.1`, ...).
    pub(crate) fn markers(&self, id: &str) -> Markers {
        self.markers_over(self.marks.clone(), id)
    }

    pub(crate) fn markers_over(&self, store: Arc<dyn Backend>, id: &str) -> Markers {
        Markers::with_boot(
            store,
            id,
            MarkersConfig::default(),
            self.mono.clone(),
            Arc::new(RecordingSleeper::new()),
            id,
        )
        .expect("the default config holds together")
    }

    /// The marker object of `key` as the store holds it, read behind every wrapper.
    pub(crate) async fn marker(&self, key: &str) -> Option<MarkSeen> {
        let (body, tag) = self.marks_mem.get_tagged(key, None).await.ok()?;
        Some(MarkSeen {
            marker: decode_marker(&body).unwrap(),
            tag,
        })
    }

    pub(crate) async fn seed(&self, marker: &Marker) {
        self.marks_mem
            .put(&marker.key, encode_marker(marker))
            .await
            .unwrap();
    }

    pub(crate) async fn hot_has(&self, key: &str) -> bool {
        self.hot_mem.head(key).await.is_ok()
    }

    pub(crate) async fn cold_has(&self, key: &str) -> bool {
        self.cold_mem.head(key).await.is_ok()
    }

    /// The content tags of the hot and the cold object of `key`.
    pub(crate) async fn tags(&self, key: &str) -> (String, String) {
        let hot = self.hot_mem.head(key).await.unwrap().tag;
        let cold = self.cold_mem.head(key).await.unwrap().tag;
        (hot, cold)
    }
}

/// A marker of `key` in the state `st`, with nothing else.
pub(crate) fn marker(key: &str, st: MarkerState, gone: bool, dirty: bool) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty,
        why: None,
        owner: None,
        nonce: "seed".to_owned(),
        extra: serde_json::Map::new(),
    }
}

pub(crate) fn dirty(key: &str) -> Marker {
    marker(key, MarkerState::Dirty, false, true)
}

pub(crate) fn want(key: &str) -> Marker {
    marker(key, MarkerState::Want, false, false)
}

/// A lock of `owner` for `why` over `key`.
pub(crate) fn lock(key: &str, why: MarkerWhy, owner: &str) -> Marker {
    Marker {
        why: Some(why.clone()),
        owner: Some(owner.to_owned()),
        dirty: why.owes_cold_a_copy(),
        ..marker(key, MarkerState::Lock, false, false)
    }
}

/// Polls `call` until `gate` has taken it. The call must not finish on its own before that.
pub(crate) async fn until_parked<F: Future + Unpin>(gate: &Gate, call: &mut F) {
    tokio::select! {
        _ = &mut *call => panic!("the call finished instead of parking"),
        () = gate.parked() => {}
    }
}

pub(crate) fn made(created: Creation) -> Written {
    match created {
        Creation::Made(written) => written,
        Creation::Exists => panic!("the marker exists"),
    }
}

/// The value of an outcome that was done.
pub(crate) fn done<T>(outcome: Outcome<T>) -> T {
    match outcome {
        Outcome::Done(value) => value,
        Outcome::Lost => panic!("lost"),
        Outcome::Refused => panic!("refused"),
    }
}

/// The shape of a marker: state, `gone`, `dirty`, reason and owner.
pub(crate) type Shape = (MarkerState, bool, bool, Option<MarkerWhy>, Option<String>);

pub(crate) fn shape(marker: &Marker) -> Shape {
    (
        marker.st.clone(),
        marker.gone,
        marker.dirty,
        marker.why.clone(),
        marker.owner.clone(),
    )
}

/// The shape of the marker of `key`, or none.
pub(crate) async fn shape_of(rig: &Rig, key: &str) -> Option<Shape> {
    rig.marker(key).await.map(|seen| shape(&seen.marker))
}

// ---- the lease ----

/// Asserts that a lease is taken when it is free, that a step that destroys is allowed while it is
/// held with its margin, and refused once the margin of the ttl is gone and cannot be restored
/// because the lease lapsed on the clock.
#[tokio::test]
async fn the_leader_lease_allows_a_destructive_step_only_while_it_is_held_with_its_margin() {
    let rig = Rig::new();
    let mut base = rig.base("L");
    base.lease_ttl = secs(30);
    let lease = LeaderLease::new(rig.hot.clone(), &base);
    assert_eq!(lease.require().await, Err(crate::marks_leader::lapsed()));
    assert!(lease.take().await);
    assert_eq!(lease.require().await, Ok(()));
    rig.mono.advance(secs(31));
    assert_eq!(lease.require().await, Err(crate::marks_leader::lapsed()));
}

/// Asserts that a body fed through the lease passes its chunks while the margin is left and gives
/// the lapse of the lease as the chunk asked for after it is gone: with a ttl of 30 s and the margin
/// of a third (10 s), the chunk asked for after 21 s. A lease built with its own renewal and margin
/// is held by one holder only: another holder does not take it.
#[tokio::test]
async fn a_body_fed_through_the_lease_stops_at_the_first_chunk_after_the_margin_is_gone() {
    let rig = Rig::new();
    let mut base = rig.base("L");
    base.lease_ttl = secs(30);
    let lease = LeaderLease::new(rig.hot.clone(), &base);
    assert!(lease.take().await);
    let chunks: Vec<Result<Bytes, Error>> = vec![Ok(bytes("a")), Ok(bytes("b"))];
    let mut body = lease.while_held(futures::stream::iter(chunks).boxed());
    assert_eq!(body.next().await, Some(Ok(bytes("a"))));
    rig.mono.advance(secs(21));
    assert_eq!(body.next().await, Some(Err(crate::marks_leader::lapsed())));

    let mut base = rig.base("M");
    base.lease_ttl = secs(30);
    base.lease_renew = Some(secs(5));
    base.lease_margin = Some(secs(25));
    let other = LeaderLease::new(rig.hot.clone(), &base);
    assert!(!other.take().await, "the lease is held by L");
}

// ---- renewed and release ----

/// Asserts that a lock younger than `lock_lease - queued_call_margin` (60 s by default) is
/// returned as it is, and one older is written again with a fresh nonce and a new send time.
#[tokio::test]
async fn an_old_lock_is_written_again_before_a_destructive_call_and_a_young_one_is_not() {
    let rig = Rig::new();
    let markers = rig.markers("L");
    let held = made(
        markers
            .create("k", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    rig.mono.advance(secs(59));
    let same = renewed(&markers, held.clone()).await.unwrap();
    assert_eq!(same, Some(held.clone()));
    rig.mono.advance(secs(1));
    let again = renewed(&markers, held.clone()).await.unwrap().unwrap();
    assert_eq!(again.sent_at, 60_000);
    assert_eq!(again.seen.marker.nonce, "L.1");
    assert_eq!(rig.marker("k").await, Some(again.seen));
    assert_eq!(rig.marks.calls(Op::PutIfMatch), 1);
}

/// Asserts that a lock the leader's reap took away is not written again, and that a store that
/// fails the write is the error.
#[tokio::test]
async fn a_lock_that_was_reaped_is_not_renewed_and_a_failed_renewal_is_the_error() {
    let rig = Rig::new();
    let markers = rig.markers("L");
    let held = made(
        markers
            .create("k", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    rig.mono.advance(secs(61));
    rig.marks
        .fail_next_with(Op::PutIfMatch, Error::Full("no".into()));
    assert_eq!(
        renewed(&markers, held.clone()).await,
        Err(Error::Full("no".into()))
    );
    rig.marks_mem.delete("k").await.unwrap();
    assert_eq!(renewed(&markers, held).await, Ok(None));
}

/// Asserts the ends of a lock: an eviction's (owing nothing) goes to `CLEARING` and the marker is
/// removed; a holder that changed hot ends `DIRTY`; a lock that owes a copy ends `DIRTY`; a marker
/// that moved since is left as it is and is not an error; and a store that fails the remove is the
/// error, with the marker left `CLEARING`.
#[tokio::test]
async fn a_lock_ends_removed_or_dirty_and_a_marker_that_moved_is_left_alone() {
    let rig = Rig::new();
    let markers = rig.markers("L");
    let evict = made(
        markers
            .create("a", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    release(&markers, &evict, false).await.unwrap();
    assert_eq!(rig.marker("a").await, None);

    let evict = made(
        markers
            .create("b", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    release(&markers, &evict, true).await.unwrap();
    assert_eq!(shape_of(&rig, "b").await.unwrap().0, MarkerState::Dirty);

    let overwrite = made(
        markers
            .create("c", MarkBirth::Lock(MarkerWhy::Overwrite))
            .await
            .unwrap(),
    );
    release(&markers, &overwrite, false).await.unwrap();
    assert_eq!(shape_of(&rig, "c").await.unwrap().0, MarkerState::Dirty);

    let evict = made(
        markers
            .create("d", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    let other = rig.markers("O");
    let moved = other.read("d").await.unwrap().unwrap();
    let reaped = other.advance(&moved, MarkMove::Reap).await.unwrap();
    let clearing = done(reaped);
    release(&markers, &evict, false).await.unwrap();
    assert_eq!(rig.marker("d").await, Some(clearing.seen));

    let evict = made(
        markers
            .create("e", MarkBirth::Lock(MarkerWhy::Evict))
            .await
            .unwrap(),
    );
    rig.marks
        .fail_next_with(Op::Delete, Error::Full("no".into()));
    assert_eq!(
        release(&markers, &evict, false).await,
        Err(Error::Full("no".into()))
    );
    assert_eq!(shape_of(&rig, "e").await.unwrap().0, MarkerState::Clearing);
}
