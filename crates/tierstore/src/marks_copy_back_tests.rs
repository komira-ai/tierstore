//! The leader's copy-back over markers: the lock, the create-only landing, what each outcome leaves
//! in the marker, the bounds (age, count, size), the lock kept alive while a stream runs, and the
//! crashes and failures of 7.2. Time is the monotonic clock of the rig; the races are held by
//! gates.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use crate::backend::{ReadBody, Window};
use crate::format::FormatPhase;
use crate::marks_leader_tests::{
    MARKS_BUCKET, NOT_OPEN, Rig, Shape, bytes, dirty, done, lock, secs, shape_of, until_parked,
    want,
};
use crate::{
    Backend, ByteRange, CopyBackConfig, CopyBacker, Error, GateBackend, LeaderLease, Listed,
    MarkMove, MarkSeen, MarkerState, MarkerWhy, Meta, Mono, Op, Opened, Phase, Sleeper,
    encode_marker,
};

const KEY: &str = "models/a.bin";
const LEADER: &str = "L";

/// The shape of the lock a copy-back takes.
fn copy_back_lock() -> Shape {
    (
        MarkerState::Lock,
        false,
        false,
        Some(MarkerWhy::CopyBack),
        Some(LEADER.to_owned()),
    )
}

fn left_dirty() -> Shape {
    (MarkerState::Dirty, false, true, None, None)
}

async fn backer(
    rig: &Rig,
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    lease: LeaderLease,
    tweak: impl FnOnce(&mut CopyBackConfig),
) -> CopyBacker {
    let mut config = CopyBackConfig::default();
    tweak(&mut config);
    CopyBacker::new(
        hot,
        cold,
        rig.markers(LEADER),
        MARKS_BUCKET,
        lease,
        rig.mono.clone(),
        config,
    )
}

/// A rig with the format active, `KEY` in cold and a `WANT` marker of it.
async fn asked_rig() -> Rig {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("cold bytes")).await.unwrap();
    rig.seed(&want(KEY)).await;
    rig
}

/// As [`asked_rig`], with a copy-back over the rig's wrappers that holds the lease.
async fn asked() -> (Rig, CopyBacker) {
    let rig = asked_rig().await;
    let lease = rig.lease(LEADER).await;
    let cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), lease, |_| {}).await;
    (rig, cb)
}

async fn want_seen(rig: &Rig, key: &str) -> MarkSeen {
    rig.marker(key).await.expect("the marker exists")
}

/// A cold tier that gives its objects as chunks of a byte each and runs `on_chunk` with the number
/// of each chunk after the first before giving it, so that a test moves the clock or the marker
/// while a stream runs.
struct Chunky {
    inner: Arc<crate::MemoryBackend>,
    on_chunk: Arc<dyn Fn(usize) + Send + Sync>,
}

#[async_trait]
impl Backend for Chunky {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }
    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }
    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
    async fn open_read(&self, key: &str, _span: Option<crate::Span>) -> Result<Opened, Error> {
        let meta = self.inner.head(key).await?;
        let whole = self.inner.get(key, None).await?;
        let on_chunk = self.on_chunk.clone();
        let chunks = (0..whole.len()).map(move |at| (at, whole.slice(at..=at)));
        let body: ReadBody = futures::stream::iter(chunks)
            .map(move |(at, chunk)| {
                if at > 0 {
                    on_chunk(at);
                }
                Ok(chunk)
            })
            .boxed();
        Ok(Opened {
            meta,
            window: Window::Whole,
            body,
        })
    }
}

/// A sleeper that moves the monotonic clock by what it is asked to wait, so that a wait ends by the
/// time it has elapsed.
struct Ticking(Arc<Mono>);

#[async_trait]
impl Sleeper for Ticking {
    async fn sleep(&self, duration: Duration) {
        self.0.advance(duration);
    }
}

/// Asserts the copy-back: the `WANT` marker is locked by a CAS to `LOCK(COPYBACK)` owned by the
/// leader and owing nothing, before any byte is written; hot has no object then; the cold object is
/// then written into hot create-only, the marker goes through `CLEARING` and is removed, and cold
/// is untouched. Hot is asked once (HEAD) and written once.
#[tokio::test]
async fn a_copy_back_lands_by_create_only_and_clears_its_marker() {
    let rig = asked_rig().await;
    let lease = rig.lease(LEADER).await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let write = hot.gate(Op::Put, KEY, Phase::Pre);
    let mut cb = backer(&rig, hot, rig.cold.clone(), lease, |_| {}).await;
    let seen = want_seen(&rig, KEY).await;

    let mut pass = Box::pin(cb.pass(vec![seen], 4));
    until_parked(&write, &mut pass).await;
    assert_eq!(shape_of(&rig, KEY).await, Some(copy_back_lock()));
    assert!(!rig.hot_has(KEY).await);
    write.release();
    let report = pass.await;

    assert!(report.active);
    assert_eq!(report.landed, 1);
    assert_eq!(report.failed, []);
    assert_eq!(
        rig.hot_mem.get(KEY, None).await.unwrap(),
        bytes("cold bytes")
    );
    assert_eq!(
        rig.cold_mem.get(KEY, None).await.unwrap(),
        bytes("cold bytes")
    );
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert_eq!(rig.hot.calls_on(Op::Head, KEY), 1);
    assert_eq!(rig.hot.calls_on(Op::Put, KEY), 1);
    assert_eq!(rig.marks.calls(Op::PutIfMatch), 2);
}

/// Asserts that a copy-back that finds a newer put (hot has the key when its create-only write is
/// answered) leaves the hot object as it is and the marker `DIRTY`, for the copier to reconcile
/// cold with what hot holds.
#[tokio::test]
async fn a_copy_back_that_finds_a_newer_put_leaves_the_marker_dirty() {
    let rig = asked_rig().await;
    let lease = rig.lease(LEADER).await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let write = hot.gate(Op::Put, KEY, Phase::Pre);
    let mut cb = backer(&rig, hot, rig.cold.clone(), lease, |_| {}).await;
    let seen = want_seen(&rig, KEY).await;

    let mut pass = Box::pin(cb.pass(vec![seen], 4));
    until_parked(&write, &mut pass).await;
    rig.hot_mem.put(KEY, bytes("newer put")).await.unwrap();
    write.release();
    let report = pass.await;

    assert_eq!((report.landed, report.raced), (0, 1));
    assert_eq!(
        rig.hot_mem.get(KEY, None).await.unwrap(),
        bytes("newer put")
    );
    assert_eq!(shape_of(&rig, KEY).await, Some(left_dirty()));
}

/// Asserts that a copy-back cannot bring back a deleted key. A delete that took the lock of the
/// `WANT` marker first (M4) leaves the copy-back's CAS lost: nothing is written and the delete's
/// lock is as it was. A delete that comes while the copy-back holds the lock waits for it (the
/// wait ends at `lock_wait` with the lock still the copy-back's) and writes nothing.
#[tokio::test]
async fn a_copy_back_never_resurrects_a_deleted_key() {
    let (rig, mut cb) = asked().await;
    let seen = want_seen(&rig, KEY).await;
    let deleter = rig.markers("P");
    let deleting = done(
        deleter
            .advance(&seen, MarkMove::Lock(MarkerWhy::Delete))
            .await
            .unwrap(),
    );
    let report = cb.pass(vec![seen], 4).await;
    assert_eq!((report.lost, report.landed), (1, 0));
    assert!(!rig.hot_has(KEY).await);
    assert_eq!(rig.marker(KEY).await, Some(deleting.seen));

    let rig = asked_rig().await;
    let lease = rig.lease(LEADER).await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let write = hot.gate(Op::Put, KEY, Phase::Pre);
    let mut cb = backer(&rig, hot, rig.cold.clone(), lease, |_| {}).await;
    let seen = want_seen(&rig, KEY).await;
    let mut pass = Box::pin(cb.pass(vec![seen], 4));
    until_parked(&write, &mut pass).await;
    let writer = crate::Markers::new(
        rig.marks.clone(),
        "P",
        crate::MarkersConfig::default(),
        rig.mono.clone(),
        Arc::new(Ticking(rig.mono.clone())),
    )
    .unwrap();
    assert_eq!(writer.wait(KEY).await, Ok(crate::Waited::TimedOut));
    assert_eq!(shape_of(&rig, KEY).await, Some(copy_back_lock()));
    assert!(!rig.hot_has(KEY).await);
    write.release();
    assert_eq!(pass.await.landed, 1);
}

/// Asserts the size bound: an object above `copy_back_max_bytes` is not copied back (the marker is
/// released and removed, hot gets nothing, cold is read from again by the next cold read), and one
/// of exactly that size is.
#[tokio::test]
async fn an_object_above_copy_back_max_bytes_is_not_copied_back() {
    for (max, landed) in [(10, 1), (9, 0)] {
        let rig = asked_rig().await;
        let lease = rig.lease(LEADER).await;
        let mut cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), lease, |config| {
            config.copy_back_max_bytes = max;
        })
        .await;
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert_eq!(
            (report.landed, report.dropped),
            (landed, 1 - landed),
            "{max}"
        );
        assert_eq!(rig.hot_has(KEY).await, landed == 1);
        assert_eq!(shape_of(&rig, KEY).await, None);
        assert!(rig.cold_has(KEY).await);
        assert_eq!(rig.hot.calls_on(Op::Put, KEY), landed as u64);
    }
}

/// Asserts the bounds on the number and the age of the asks: with `copy_back_max` of 2 the oldest
/// two of three asks seen in one pass are copied and the third is dropped without a copy; an ask
/// that stays a `WANT` (its lock was lost to a failed write whose read-back says it did not
/// land) is copied while it is younger than `copy_back_ttl` and dropped at exactly the ttl,
/// counted from the pass that first saw it; and an ask a pass did not see is forgotten, so that it
/// starts again if it comes back.
#[tokio::test]
async fn asks_beyond_the_maximum_or_older_than_the_ttl_are_dropped_without_a_copy() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["k1", "k2", "k3"] {
        rig.cold_mem.put(key, bytes("v")).await.unwrap();
        rig.seed(&want(key)).await;
    }
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), lease, |config| {
        config.copy_back_max = 2;
    })
    .await;
    let mut seen = Vec::new();
    for key in ["k3", "k1", "k2"] {
        seen.push(want_seen(&rig, key).await);
    }
    let report = cb.pass(seen, 4).await;
    assert_eq!((report.landed, report.dropped), (2, 1));
    assert!(rig.hot_has("k1").await);
    assert!(rig.hot_has("k2").await);
    assert!(!rig.hot_has("k3").await);
    for key in ["k1", "k2", "k3"] {
        assert_eq!(shape_of(&rig, key).await, None, "{key}");
    }

    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("v")).await.unwrap();
    rig.seed(&want(KEY)).await;
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), lease, |_| {}).await;
    let seen = want_seen(&rig, KEY).await;
    // The CAS of the lock is lost though the marker stays a WANT: the ask is first seen now.
    rig.marks
        .fail_next_with(Op::PutIfMatch, Error::Unavailable("down".into()));
    assert_eq!(cb.pass(vec![seen.clone()], 4).await.lost, 1);
    assert_eq!(rig.marker(KEY).await, Some(seen.clone()));
    rig.mono.advance(Duration::from_millis(599_999));
    rig.marks
        .fail_next_with(Op::PutIfMatch, Error::Unavailable("down".into()));
    assert_eq!(cb.pass(vec![seen.clone()], 4).await.lost, 1);
    rig.mono.advance(Duration::from_millis(1));
    let report = cb.pass(vec![seen.clone()], 4).await;
    assert_eq!((report.dropped, report.landed), (1, 0));
    assert!(!rig.hot_has(KEY).await);
    assert_eq!(shape_of(&rig, KEY).await, None);

    // The same ask, made again as a new marker, and seen only after a pass that did not see it,
    // is as young as its first pass.
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("v")).await.unwrap();
    rig.seed(&want(KEY)).await;
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), lease, |_| {}).await;
    let seen = want_seen(&rig, KEY).await;
    rig.marks
        .fail_next_with(Op::PutIfMatch, Error::Unavailable("down".into()));
    assert_eq!(cb.pass(vec![seen.clone()], 4).await.lost, 1);
    rig.mono.advance(secs(600));
    assert_eq!(cb.pass(vec![], 4).await.landed, 0);
    assert_eq!(cb.pass(vec![seen], 4).await.landed, 1);
    cb.took_over();
}

/// Asserts that markers that are not a `WANT` are not touched by a pass.
#[tokio::test]
async fn a_pass_leaves_the_markers_that_are_not_asks_alone() {
    let (rig, mut cb) = asked().await;
    rig.seed(&dirty("d")).await;
    rig.seed(&lock("l", MarkerWhy::Overwrite, "p1")).await;
    let before = [want_seen(&rig, "d").await, want_seen(&rig, "l").await];
    let report = cb.pass(before.to_vec(), 4).await;
    assert_eq!((report.landed, report.dropped, report.lost), (0, 0, 0));
    assert_eq!(rig.marker("d").await, Some(before[0].clone()));
    assert_eq!(rig.marker("l").await, Some(before[1].clone()));
    assert_eq!(rig.marks.calls(Op::PutIfMatch), 0);
}

/// Asserts that an ask with nothing to copy is released and removed with no write to hot: the hot
/// tier has the key already (it is left as it is), the cold tier has not, and the hot tier is full.
#[tokio::test]
async fn an_ask_with_nothing_to_copy_is_removed_and_changes_no_object() {
    for case in ["hot has it", "cold has not", "hot is full"] {
        let (rig, mut cb) = asked().await;
        match case {
            "hot has it" => rig.hot_mem.put(KEY, bytes("hot")).await.unwrap(),
            "cold has not" => rig.cold_mem.delete(KEY).await.unwrap(),
            _ => rig
                .hot
                .fail_next_with(Op::Put, Error::Full("full".to_owned())),
        }
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert_eq!((report.nothing, report.landed), (1, 0), "{case}");
        assert_eq!(report.failed, [], "{case}");
        assert_eq!(shape_of(&rig, KEY).await, None, "{case}");
        assert_eq!(rig.hot_has(KEY).await, case == "hot has it", "{case}");
    }
}

/// Asserts the failures of 7.2: a hot HEAD, or a leader that has lost the lease, failing before
/// the write release the lock (the marker is removed, so a later cold read asks
/// again); a hot write that ends with an outcome that is not known (the write may still land) keeps
/// the lock, which the leader's reap ends.
#[tokio::test]
async fn a_copy_back_that_fails_releases_its_lock_unless_its_write_may_still_land() {
    for failing in ["hot head", "lease", "hot write"] {
        let (rig, mut cb) = asked().await;
        let down = Error::Unavailable(failing.to_owned());
        match failing {
            "hot head" => rig.hot.fail_next_with(Op::Head, down.clone()),
            "hot write" => rig.hot.fail_next_with(Op::Put, down.clone()),
            _ => {
                let unheld = LeaderLease::new(rig.hot.clone(), &rig.base("Other"));
                cb = backer(&rig, rig.hot.clone(), rig.cold.clone(), unheld, |_| {}).await;
            }
        }
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert_eq!(report.landed, 0, "{failing}");
        assert_eq!(report.failed.len(), 1, "{failing}");
        assert_eq!(report.failed[0].0, KEY);
        if failing == "lease" {
            assert!(matches!(report.failed[0].1, Error::Unavailable(_)));
        } else {
            assert_eq!(report.failed[0].1, down, "{failing}");
        }
        let kept = if failing == "hot write" {
            Some(copy_back_lock())
        } else {
            None
        };
        assert_eq!(shape_of(&rig, KEY).await, kept, "{failing}");
        assert!(!rig.hot_has(KEY).await);
    }
}

/// Asserts that a hot write the store refused for certain (nothing can land after it) releases the
/// lock, owing cold nothing, so a writer of the key does not wait out `lock_lease`; and that an
/// "other failure" answer, which leaves it open whether the write landed, keeps it.
#[tokio::test]
async fn a_hot_write_that_the_store_refused_releases_the_lock_and_an_unclear_failure_keeps_it() {
    let refusals = [
        (Error::Unsupported("no".into()), false),
        (Error::Reserved(KEY.to_owned()), false),
        (Error::InvalidKey(KEY.to_owned()), false),
        (Error::PreconditionFailed(KEY.to_owned()), false),
        (Error::Other("unclear".into()), true),
    ];
    for (refusal, kept) in refusals {
        let (rig, mut cb) = asked().await;
        rig.hot.fail_next_with(Op::Put, refusal.clone());
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert_eq!(report.failed, [(KEY.to_owned(), refusal.clone())]);
        assert_eq!(report.landed, 0, "{refusal}");
        let shape = if kept { Some(copy_back_lock()) } else { None };
        assert_eq!(shape_of(&rig, KEY).await, shape, "{refusal}");
        assert!(!rig.hot_has(KEY).await, "{refusal}");
    }
}

/// Asserts that a copy-back whose cold read fails releases its lock (7.2): the error is reported
/// with the key, the marker is removed (a later cold read asks again), and nothing is written to
/// hot.
#[tokio::test]
async fn a_copy_back_whose_cold_read_fails_releases_its_lock() {
    let (rig, mut cb) = asked().await;
    let down = Error::Unavailable("cold down".into());
    rig.cold.fail_next_with(Op::Get, down.clone());
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert_eq!(report.failed, [(KEY.to_owned(), down)]);
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert!(!rig.hot_has(KEY).await);
    assert_eq!(rig.hot.calls_on(Op::Put, KEY), 0);
}

/// Asserts that a lock older than `lock_lease - queued_call_margin` when the write is about to
/// start is written again first, and the copy goes on; and that a lock the leader's reap took away
/// before then is lost: nothing is written.
#[tokio::test]
async fn a_lock_that_aged_before_the_write_is_renewed_or_lost() {
    for lost in [false, true] {
        let rig = asked_rig().await;
        let lease = rig.lease(LEADER).await;
        let hot = Arc::new(GateBackend::new(rig.hot.clone()));
        let head = hot.gate(Op::Head, KEY, Phase::Post);
        let mut cb = backer(&rig, hot, rig.cold.clone(), lease, |_| {}).await;
        let seen = want_seen(&rig, KEY).await;
        let mut pass = Box::pin(cb.pass(vec![seen], 4));
        until_parked(&head, &mut pass).await;
        rig.mono.advance(secs(61));
        if lost {
            rig.marks_mem.delete(KEY).await.unwrap();
        }
        head.release();
        let report = pass.await;
        let (landed, lost_asks) = if lost { (0, 1) } else { (1, 0) };
        assert_eq!((report.landed, report.lost), (landed, lost_asks));
        assert_eq!(rig.hot_has(KEY).await, !lost);
        assert_eq!(rig.marks.calls(Op::PutIfMatch), if lost { 2 } else { 3 });
    }
}

/// Asserts that the lock is written again every `lock_refresh` while the stream runs (M14): a
/// stream whose chunks come 31 s apart on the clock makes the lock be written again before the
/// chunks that find it due, and the release at the end names the lock as last written (the marker
/// is removed, not left to the reap).
#[tokio::test]
async fn the_lock_is_refreshed_while_a_long_stream_runs_and_released_by_its_last_tag() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("abcd")).await.unwrap();
    rig.seed(&want(KEY)).await;
    let mono = rig.mono.clone();
    let cold = Arc::new(Chunky {
        inner: rig.cold_mem.clone(),
        on_chunk: Arc::new(move |_| mono.advance(secs(31))),
    });
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), cold, lease, |_| {}).await;
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert_eq!(report.landed, 1);
    assert_eq!(rig.hot_mem.get(KEY, None).await.unwrap(), bytes("abcd"));
    // The lock (1), three refreshes (chunks 2 to 4), the release (1).
    assert_eq!(rig.marks.calls(Op::PutIfMatch), 5);
    assert_eq!(shape_of(&rig, KEY).await, None);
}

/// Asserts that a lock taken away while the stream runs (the leader's reap, after the holder
/// stalled) ends the stream with an error and nothing lands in hot.
#[tokio::test]
async fn a_lock_taken_away_during_the_stream_stops_the_write_and_lands_nothing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("abcd")).await.unwrap();
    rig.seed(&want(KEY)).await;
    let mono = rig.mono.clone();
    let marks = rig.marks_mem.clone();
    let cold = Arc::new(Chunky {
        inner: rig.cold_mem.clone(),
        on_chunk: Arc::new(move |_| {
            mono.advance(secs(31));
            // The leader's reap ended the lock and a put marked the key again.
            let marks = marks.clone();
            futures::executor::block_on(async move {
                marks.delete(KEY).await.unwrap();
                marks.put(KEY, encode_marker(&dirty(KEY))).await.unwrap();
            });
        }),
    });
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), cold, lease, |_| {}).await;
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert_eq!(report.landed, 0);
    assert_eq!(report.failed.len(), 1);
    assert!(matches!(report.failed[0].1, Error::Unavailable(_)));
    assert!(!rig.hot_has(KEY).await);
}

/// Asserts that a stream is cut at the first chunk asked for after the leader's lease has lost its
/// margin: nothing lands, the error is the lapse of the lease, and the lock stays for the reap.
#[tokio::test]
async fn a_stream_is_cut_when_the_leader_loses_its_lease_and_lands_nothing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put(KEY, bytes("abcd")).await.unwrap();
    rig.seed(&want(KEY)).await;
    let mono = rig.mono.clone();
    let cold = Arc::new(Chunky {
        inner: rig.cold_mem.clone(),
        on_chunk: Arc::new(move |_| mono.advance(secs(3600))),
    });
    let lease = rig.lease(LEADER).await;
    let mut cb = backer(&rig, rig.hot.clone(), cold, lease, |_| {}).await;
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert_eq!(report.landed, 0);
    assert!(matches!(report.failed[0].1, Error::Unavailable(_)));
    assert!(!rig.hot_has(KEY).await);
    assert_eq!(shape_of(&rig, KEY).await, Some(copy_back_lock()));
}

/// Asserts that a pass does nothing under the phase `active-marks` when the record does not open
/// writes on markers in the copy-back's marks bucket: no epoch, an epoch below 3, no name, another
/// name. The same pass under the record that does copies back.
#[tokio::test]
async fn a_copy_back_pass_does_nothing_unless_the_record_names_its_bucket_at_epoch_3() {
    for record in NOT_OPEN {
        let (rig, mut cb) = asked().await;
        rig.format_json(record).await;
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert!(!report.active, "{record}");
        assert_eq!(report.landed + report.lost + report.dropped, 0, "{record}");
        assert_eq!(rig.marks.calls(Op::PutIfMatch), 0, "{record}");
        assert!(!rig.hot_has(KEY).await, "{record}");
    }
    let (rig, mut cb) = asked().await;
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert!(report.active);
    assert_eq!(report.landed, 1);
    assert!(rig.hot_has(KEY).await);
}

/// Asserts that a pass does nothing under a format that is not `active-marks`, and fails every
/// marker with the error when the format cannot be read.
#[tokio::test]
async fn a_copy_back_pass_does_nothing_unless_the_format_says_active_marks() {
    for phase in [
        FormatPhase::Drained,
        FormatPhase::Active,
        FormatPhase::Unknown,
    ] {
        let (rig, mut cb) = asked().await;
        rig.format(phase).await;
        let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
        assert!(!report.active, "{phase:?}");
        assert_eq!(report.landed + report.lost + report.dropped, 0);
        assert_eq!(rig.marks.calls(Op::PutIfMatch), 0);
        assert!(!rig.hot_has(KEY).await);
    }
    let (rig, mut cb) = asked().await;
    let down = Error::Unavailable("hot down".into());
    rig.hot.fail_next_with(Op::Get, down.clone());
    let report = cb.pass(vec![want_seen(&rig, KEY).await], 4).await;
    assert!(!report.active);
    assert_eq!(report.failed, [(KEY.to_owned(), down)]);
}
