//! The eviction over markers: the order of its requests, what it leaves alone, what it marks
//! `DIRTY`, and what a crash or a failure at each step leaves. Time is the monotonic clock of the
//! rig; the races are held by gates, one store call at a time.

use std::num::NonZeroU64;
use std::sync::Arc;

use crate::format::FormatPhase;
use crate::marks_leader_tests::{
    Rig, Shape, bytes, dirty, done, lock, secs, shape_of, until_parked, want,
};
use crate::{
    Backend, CopyRecords, Error, EvictConfig, Evicted, GateBackend, LeaderLease, MarkEvictor,
    MarkMove, MarkerState, MarkerWhy, Markers, Op, Phase,
};

const KEY: &str = "models/a.bin";
const LEADER: &str = "L";

/// The shape of the lock an eviction takes.
fn evict_lock() -> Shape {
    (
        MarkerState::Lock,
        false,
        false,
        Some(MarkerWhy::Evict),
        Some(LEADER.to_owned()),
    )
}

/// The shape of a marker the eviction left for the copier.
fn left_dirty() -> Shape {
    (MarkerState::Dirty, false, true, None, None)
}

fn evictor_over(
    rig: &Rig,
    hot: Arc<dyn Backend>,
    markers: Markers,
    lease: LeaderLease,
    records: &Arc<CopyRecords>,
    tweak: impl FnOnce(&mut EvictConfig),
) -> MarkEvictor {
    let mut config = EvictConfig::default();
    tweak(&mut config);
    MarkEvictor::new(
        hot,
        rig.cold.clone(),
        markers,
        lease,
        records.clone(),
        rig.mono.clone(),
        config,
    )
}

/// An evictor over the rig's wrappers, with the lease held, whose leader took over at time zero.
async fn evictor(rig: &Rig, records: &Arc<CopyRecords>) -> MarkEvictor {
    let lease = rig.lease(LEADER).await;
    evictor_over(
        rig,
        rig.hot.clone(),
        rig.markers(LEADER),
        lease,
        records,
        |_| {},
    )
}

/// The same bytes under `KEY` in both tiers.
async fn both(rig: &Rig, body: &str) {
    rig.hot_mem.put(KEY, bytes(body)).await.unwrap();
    rig.cold_mem.put(KEY, bytes(body)).await.unwrap();
}

/// Asserts the order of the requests of an eviction: the two HEADs and the compare come before the
/// lock exists (the marker does not yet when the create is parked), the lock is `LOCK(EVICT)`
/// owned by the leader and owing nothing while the hot DELETE is parked, and then the hot object
/// is deleted on its tag, the marker goes through `CLEARING` and is removed, and cold is untouched.
#[tokio::test]
async fn eviction_compares_before_the_lock_then_checks_the_tags_under_it_deletes_hot_and_clears() {
    let rig = Rig::new();
    both(&rig, "the bytes").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let marks = Arc::new(GateBackend::new(rig.marks.clone()));
    let create = marks.gate(Op::Put, KEY, Phase::Pre);
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let delete = hot.gate(Op::DeleteIfMatch, KEY, Phase::Pre);
    let ev = evictor_over(
        &rig,
        hot,
        rig.markers_over(marks, LEADER),
        lease,
        &records,
        |_| {},
    );
    rig.mono.advance(secs(600));

    let mut evicting = Box::pin(ev.evict_one(KEY));
    until_parked(&create, &mut evicting).await;
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert_eq!(rig.hot.calls_on(Op::Head, KEY), 1);
    assert_eq!(rig.hot.calls_on(Op::Get, KEY), 1);
    assert_eq!(rig.cold.calls_on(Op::Head, KEY), 1);
    assert_eq!(rig.cold.calls_on(Op::Get, KEY), 1);
    create.release();
    until_parked(&delete, &mut evicting).await;
    assert_eq!(shape_of(&rig, KEY).await, Some(evict_lock()));
    assert_eq!(rig.cold.calls_on(Op::Head, KEY), 2);
    assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
    delete.release();

    let freed = u64::try_from("the bytes".len()).unwrap();
    assert_eq!(
        evicting.await,
        Ok(Evicted::Removed {
            freed,
            unreleased: None
        })
    );
    assert!(!rig.hot_has(KEY).await);
    assert!(rig.cold_has(KEY).await);
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 1);
    assert_eq!(rig.hot.calls(Op::Delete), 0);
    assert_eq!(rig.cold.calls(Op::Delete), 0);
    assert_eq!(rig.marks.calls(Op::PutIfMatch), 1);
    assert_eq!(rig.marks.calls(Op::Delete), 1);
}

/// Asserts that a compare reads the two objects in ranges of `compare_range` and that a pair the
/// compare found equal is remembered (copied at takeover), so that a second eviction of the key,
/// after a failure that kept it, reads no byte.
#[tokio::test]
async fn a_pair_of_tags_the_leader_knows_is_not_compared_again() {
    let rig = Rig::new();
    both(&rig, "0123456789").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let ev = evictor_over(
        &rig,
        rig.hot.clone(),
        rig.markers(LEADER),
        lease,
        &records,
        |config| config.compare_range = NonZeroU64::new(4).unwrap(),
    );
    rig.mono.advance(secs(600));
    rig.hot
        .fail_next_with(Op::DeleteIfMatch, Error::Unsupported("no".into()));
    assert_eq!(
        ev.evict_one(KEY).await,
        Err(Error::Unsupported("no".into()))
    );
    assert_eq!(
        rig.hot.get_ranges(),
        [Some((0, 4)), Some((4, 4)), Some((8, 2))]
    );
    let (hot_tag, cold_tag) = rig.tags(KEY).await;
    let record = records.get(KEY).unwrap();
    assert_eq!(
        (record.hot, record.cold, record.copied_at),
        (hot_tag, cold_tag, 0)
    );

    let evicted = ev.evict_one(KEY).await.unwrap();
    assert!(matches!(evicted, Evicted::Removed { freed: 10, .. }));
    assert_eq!(rig.hot.get_ranges().len(), 3);
}

/// Asserts that an object whose cold copy was written less than `evict_min_age` ago is left,
/// however old the hot object (its time is the hot tier's and is not read), with no marker and no
/// byte read, and that it is evicted at exactly `evict_min_age`: the age counts from the cold write
/// the copier recorded.
#[tokio::test]
async fn an_object_whose_cold_copy_is_younger_than_evict_min_age_is_not_evicted_however_old_the_hot_object()
 {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    rig.clock.advance(100_000_000);
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    let (hot, cold) = rig.tags(KEY).await;
    rig.mono.advance(secs(1000));
    records.record(KEY, &hot, &cold, 1_000_000, 10);
    rig.mono.advance(secs(599));
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Young));
    assert_eq!(rig.marks.calls(Op::Put), 0);
    assert_eq!(rig.hot.calls_on(Op::Get, KEY), 0);
    assert!(rig.hot_has(KEY).await);

    rig.mono.advance(std::time::Duration::from_millis(999));
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Young));
    rig.mono.advance(std::time::Duration::from_millis(1));
    assert!(matches!(
        ev.evict_one(KEY).await,
        Ok(Evicted::Removed { .. })
    ));
}

/// Asserts that a pair with no record counts as copied at takeover: a key the leader never copied
/// is left for `evict_min_age` after the leader took over, and a leader that takes over again
/// starts the count again and forgets its records.
#[tokio::test]
async fn a_pair_with_no_record_counts_as_copied_at_takeover() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    rig.mono.advance(secs(599));
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Young));
    assert_eq!(rig.hot.calls_on(Op::Get, KEY), 0);
    rig.mono.advance(secs(1));
    ev.took_over();
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Young));
    rig.mono.advance(secs(600));
    assert!(matches!(
        ev.evict_one(KEY).await,
        Ok(Evicted::Removed { .. })
    ));
}

/// Asserts that a compare that finds other bytes (of the same size, or of another size, which
/// reads none) is not an anomaly: the key is marked `DIRTY` with `gone = false`, owing a copy, the
/// hot object is kept, and cold is untouched.
#[tokio::test]
async fn an_eviction_compare_that_finds_other_bytes_marks_the_key_dirty_and_keeps_hot() {
    for (hot, cold, reads) in [("aaaa", "bbbb", 1), ("aaaa", "bb", 0)] {
        let rig = Rig::new();
        rig.hot_mem.put(KEY, bytes(hot)).await.unwrap();
        rig.cold_mem.put(KEY, bytes(cold)).await.unwrap();
        let records = Arc::new(CopyRecords::default());
        let ev = evictor(&rig, &records).await;
        rig.mono.advance(secs(600));
        assert_eq!(
            ev.evict_one(KEY).await,
            Ok(Evicted::Dirtied),
            "{hot} {cold}"
        );
        assert_eq!(rig.hot.calls_on(Op::Get, KEY), reads);
        assert_eq!(shape_of(&rig, KEY).await, Some(left_dirty()));
        assert_eq!(rig.hot_mem.get(KEY, None).await.unwrap(), bytes(hot));
        assert_eq!(rig.cold_mem.get(KEY, None).await.unwrap(), bytes(cold));
        assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
        assert_eq!(records.get(KEY), None);
    }
}

/// Asserts that a hot object with no cold copy is the only copy: it is marked `DIRTY` for the
/// copier and kept.
#[tokio::test]
async fn a_hot_object_with_no_cold_copy_is_marked_dirty_and_kept() {
    let rig = Rig::new();
    rig.hot_mem.put(KEY, bytes("only")).await.unwrap();
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Dirtied));
    assert_eq!(shape_of(&rig, KEY).await, Some(left_dirty()));
    assert!(rig.hot_has(KEY).await);
    assert_eq!(rig.cold.calls_on(Op::Get, KEY), 0);
}

/// Asserts that an eviction never deletes a version that replaced the one it checked: a hot object
/// replaced after the compare and before the delete is kept and the key is marked `DIRTY` (the
/// delete on the tag read refuses it); and a cold object that was rewritten after the compare,
/// before the lock was looked at again, keeps hot too and nothing is deleted.
#[tokio::test]
async fn an_eviction_never_deletes_a_version_that_replaced_the_one_it_checked() {
    for replaced in ["hot", "cold"] {
        let rig = Rig::new();
        both(&rig, "old bytes").await;
        let records = Arc::new(CopyRecords::default());
        let lease = rig.lease(LEADER).await;
        let marks = Arc::new(GateBackend::new(rig.marks.clone()));
        let create = marks.gate(Op::Put, KEY, Phase::Post);
        let ev = evictor_over(
            &rig,
            rig.hot.clone(),
            rig.markers_over(marks, LEADER),
            lease,
            &records,
            |_| {},
        );
        rig.mono.advance(secs(600));
        let mut evicting = Box::pin(ev.evict_one(KEY));
        until_parked(&create, &mut evicting).await;
        let tier = if replaced == "hot" {
            &rig.hot_mem
        } else {
            &rig.cold_mem
        };
        tier.delete(KEY).await.unwrap();
        tier.put(KEY, bytes("new bytes")).await.unwrap();
        create.release();
        assert_eq!(evicting.await, Ok(Evicted::Dirtied), "{replaced}");
        assert_eq!(shape_of(&rig, KEY).await, Some(left_dirty()));
        assert!(rig.hot_has(KEY).await);
        assert!(rig.cold_has(KEY).await);
    }
}

/// Asserts that a key with a marker of any state is left: the create-only lock is refused (412),
/// nothing is deleted, and the marker is as it was.
#[tokio::test]
async fn a_key_with_any_marker_is_not_evicted() {
    let markers = [
        dirty(KEY),
        want(KEY),
        lock(KEY, MarkerWhy::Overwrite, "p1"),
        crate::marks_leader_tests::marker(KEY, MarkerState::Clearing, false, false),
    ];
    for marker in markers {
        let rig = Rig::new();
        both(&rig, "bytes").await;
        rig.seed(&marker).await;
        let before = rig.marker(KEY).await;
        let records = Arc::new(CopyRecords::default());
        let ev = evictor(&rig, &records).await;
        rig.mono.advance(secs(600));
        assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Kept), "{marker:?}");
        assert_eq!(rig.marker(KEY).await, before);
        assert!(rig.hot_has(KEY).await);
        assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
    }
}

/// Asserts that a key with no hot object needs nothing (and asks nothing of cold or the markers),
/// and that a hot object that is gone when the delete reaches the store is the same, with the lock
/// released and the marker removed.
#[tokio::test]
async fn a_key_with_no_hot_object_is_gone_and_one_that_goes_under_the_lock_releases_it() {
    let rig = Rig::new();
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Gone));
    assert_eq!(rig.cold.calls_on(Op::Head, KEY), 0);
    assert_eq!(rig.marks.calls(Op::Put), 0);

    both(&rig, "bytes").await;
    rig.mono.advance(secs(600));
    rig.hot
        .fail_next_with(Op::DeleteIfMatch, Error::NotFound(KEY.to_owned()));
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Gone));
    assert_eq!(shape_of(&rig, KEY).await, None);
}

/// Asserts that a hot DELETE whose outcome is not known (the store was unavailable) keeps the lock
/// and is the error, since the call may still land and the leader's reap ends the lock; that a
/// delete the store refuses for another reason releases the lock; and that a delete that finds the
/// tag changed marks the key `DIRTY`.
#[tokio::test]
async fn a_hot_delete_of_unknown_outcome_keeps_the_lock_and_another_refusal_releases_it() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    rig.mono.advance(secs(600));

    let down = Error::Unavailable("down".into());
    rig.hot.fail_next_with(Op::DeleteIfMatch, down.clone());
    assert_eq!(ev.evict_one(KEY).await, Err(down));
    assert_eq!(shape_of(&rig, KEY).await, Some(evict_lock()));
    assert!(rig.hot_has(KEY).await);

    // The reap of the lock (5.13) ends it, and the key can be evicted again.
    let other = rig.markers("R");
    let seen = other.read(KEY).await.unwrap().unwrap();
    let clearing = done(other.advance(&seen, MarkMove::Reap).await.unwrap());
    other.remove_departed(&clearing.seen).await.unwrap();

    let refused = Error::Reserved(KEY.to_owned());
    rig.hot.fail_next_with(Op::DeleteIfMatch, refused.clone());
    assert_eq!(ev.evict_one(KEY).await, Err(refused));
    assert_eq!(shape_of(&rig, KEY).await, None);

    rig.hot
        .fail_next_with(Op::DeleteIfMatch, Error::PreconditionFailed(KEY.to_owned()));
    assert_eq!(ev.evict_one(KEY).await, Ok(Evicted::Dirtied));
    assert_eq!(shape_of(&rig, KEY).await, Some(left_dirty()));
}

/// Asserts that a failure before the delete releases the lock and is the error: the second cold
/// HEAD (under the lock) failing, and a leader that does not hold its lease any more; the hot
/// object stays and no marker is left.
#[tokio::test]
async fn a_failure_before_the_hot_delete_releases_the_lock() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let marks = Arc::new(GateBackend::new(rig.marks.clone()));
    let create = marks.gate(Op::Put, KEY, Phase::Post);
    let ev = evictor_over(
        &rig,
        rig.hot.clone(),
        rig.markers_over(marks, LEADER),
        lease,
        &records,
        |_| {},
    );
    rig.mono.advance(secs(600));
    let mut evicting = Box::pin(ev.evict_one(KEY));
    until_parked(&create, &mut evicting).await;
    let down = Error::Unavailable("cold down".into());
    rig.cold.fail_next_with(Op::Head, down.clone());
    create.release();
    assert_eq!(evicting.await, Err(down));
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert!(rig.hot_has(KEY).await);

    let unheld = LeaderLease::new(rig.hot.clone(), &rig.base("Other"));
    let ev = evictor_over(
        &rig,
        rig.hot.clone(),
        rig.markers(LEADER),
        unheld,
        &records,
        |_| {},
    );
    let lapsed = ev.evict_one(KEY).await;
    assert!(matches!(lapsed, Err(Error::Unavailable(_))), "{lapsed:?}");
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
}

/// Asserts that a lock older than `lock_lease - queued_call_margin` is written again before the
/// delete (one more write of the marker, the delete still happens), and that a lock the leader's
/// reap took away in the meantime is not written again: nothing is deleted, hot is kept.
#[tokio::test]
async fn a_lock_older_than_the_lease_less_the_margin_is_written_again_and_a_lost_lock_deletes_nothing()
 {
    for lost in [false, true] {
        let rig = Rig::new();
        both(&rig, "bytes").await;
        let records = Arc::new(CopyRecords::default());
        let lease = rig.lease(LEADER).await;
        let marks = Arc::new(GateBackend::new(rig.marks.clone()));
        let create = marks.gate(Op::Put, KEY, Phase::Post);
        let ev = evictor_over(
            &rig,
            rig.hot.clone(),
            rig.markers_over(marks, LEADER),
            lease,
            &records,
            |_| {},
        );
        rig.mono.advance(secs(600));
        let mut evicting = Box::pin(ev.evict_one(KEY));
        until_parked(&create, &mut evicting).await;
        rig.mono.advance(secs(61));
        if lost {
            rig.marks_mem.delete(KEY).await.unwrap();
        }
        create.release();
        let done = evicting.await;
        if lost {
            assert_eq!(done, Ok(Evicted::Kept));
            assert!(rig.hot_has(KEY).await);
            assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
        } else {
            assert!(matches!(done, Ok(Evicted::Removed { .. })), "{done:?}");
            assert_eq!(rig.marks.calls(Op::PutIfMatch), 2);
            assert!(!rig.hot_has(KEY).await);
        }
    }
}

/// Asserts the crash of 7.2 before the delete: a leader that dies holding the lock leaves
/// `LOCK(EVICT)` and the hot object; the reap (M9) makes it `CLEARING`, the marker is removed and
/// hot stays.
#[tokio::test]
async fn a_crash_holding_the_evict_lock_before_the_delete_keeps_hot() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let delete = hot.gate(Op::DeleteIfMatch, KEY, Phase::Pre);
    let ev = evictor_over(&rig, hot, rig.markers(LEADER), lease, &records, |_| {});
    rig.mono.advance(secs(600));
    let mut evicting = Box::pin(ev.evict_one(KEY));
    until_parked(&delete, &mut evicting).await;
    drop(evicting);

    assert_eq!(shape_of(&rig, KEY).await, Some(evict_lock()));
    assert!(rig.hot_has(KEY).await);
    reap_and_clear(&rig).await;
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert!(rig.hot_has(KEY).await);
    assert!(rig.cold_has(KEY).await);
}

/// Asserts the crash of 7.2 after the delete and before the release: hot is absent, cold holds the
/// truth, the lock stays, and the reap makes it `CLEARING` and the marker is removed.
#[tokio::test]
async fn a_crash_after_the_evict_delete_before_the_release_leaves_cold_as_truth() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let marks = Arc::new(GateBackend::new(rig.marks.clone()));
    let release = marks.gate(Op::PutIfMatch, KEY, Phase::Pre);
    let ev = evictor_over(
        &rig,
        rig.hot.clone(),
        rig.markers_over(marks, LEADER),
        lease,
        &records,
        |_| {},
    );
    rig.mono.advance(secs(600));
    let mut evicting = Box::pin(ev.evict_one(KEY));
    until_parked(&release, &mut evicting).await;
    drop(evicting);

    assert!(!rig.hot_has(KEY).await);
    assert!(rig.cold_has(KEY).await);
    assert_eq!(shape_of(&rig, KEY).await, Some(evict_lock()));
    reap_and_clear(&rig).await;
    assert_eq!(shape_of(&rig, KEY).await, None);
    assert!(rig.cold_has(KEY).await);
}

/// The leader's reap of a lock whose owner departed, and the removal of the `CLEARING` it makes.
async fn reap_and_clear(rig: &Rig) {
    let other = rig.markers("R");
    let seen = other.read(KEY).await.unwrap().unwrap();
    let clearing = done(other.advance(&seen, MarkMove::Reap).await.unwrap());
    assert_eq!(clearing.seen.marker.st, MarkerState::Clearing);
    other.remove_departed(&clearing.seen).await.unwrap();
}

/// Asserts that a release that fails after the delete does not undo it: the object is gone, the
/// bytes are counted as freed, the error is given beside them, and the lock stays for the reap.
#[tokio::test]
async fn a_release_that_fails_after_the_delete_is_reported_and_the_delete_stands() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let lease = rig.lease(LEADER).await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let delete = hot.gate(Op::DeleteIfMatch, KEY, Phase::Post);
    let ev = evictor_over(&rig, hot, rig.markers(LEADER), lease, &records, |_| {});
    rig.mono.advance(secs(600));
    let mut evicting = Box::pin(ev.evict_one(KEY));
    until_parked(&delete, &mut evicting).await;
    rig.marks.set_down(true);
    delete.release();
    let evicted = evicting.await;
    assert!(
        matches!(
            evicted,
            Ok(Evicted::Removed {
                freed: 5,
                unreleased: Some(Error::Unavailable(_))
            })
        ),
        "{evicted:?}"
    );
    assert!(!rig.hot_has(KEY).await);
    assert_eq!(shape_of(&rig, KEY).await, Some(evict_lock()));
}

/// Asserts that a store that fails before the lock is taken is the error and leaves no marker: the
/// hot HEAD, the cold HEAD, a read of the compare, and the create of the lock.
#[tokio::test]
async fn a_store_that_fails_before_the_lock_leaves_no_marker() {
    for failing in ["hot head", "cold head", "compare", "lock"] {
        let rig = Rig::new();
        both(&rig, "bytes").await;
        let records = Arc::new(CopyRecords::default());
        let ev = evictor(&rig, &records).await;
        rig.mono.advance(secs(600));
        let down = Error::Unavailable(failing.to_owned());
        match failing {
            "hot head" => rig.hot.fail_next_with(Op::Head, down.clone()),
            "cold head" => rig.cold.fail_next_with(Op::Head, down.clone()),
            "compare" => rig.cold.fail_next_with(Op::Get, down.clone()),
            _ => rig.marks.fail_next_with(Op::Put, down.clone()),
        }
        assert_eq!(ev.evict_one(KEY).await, Err(down), "{failing}");
        assert_eq!(shape_of(&rig, KEY).await, None, "{failing}");
        assert!(rig.hot_has(KEY).await);
    }
}

/// Asserts that a pass does nothing under a format that is not `active-marks` (none, `drain`,
/// `drained`, `active`, and a phase this release does not know): it reads the format object and
/// nothing else, and says it was inert.
#[tokio::test]
async fn a_pass_does_nothing_unless_the_format_says_active_marks() {
    for phase in [
        None,
        Some(FormatPhase::Drain),
        Some(FormatPhase::Drained),
        Some(FormatPhase::Active),
        Some(FormatPhase::Unknown),
    ] {
        let rig = Rig::new();
        both(&rig, "bytes").await;
        if let Some(phase) = phase {
            rig.format(phase).await;
        }
        let records = Arc::new(CopyRecords::default());
        let ev = evictor(&rig, &records).await;
        rig.mono.advance(secs(600));
        let report = ev.evict_pass(vec![KEY.to_owned()], 4).await;
        assert!(!report.active, "{phase:?}");
        assert_eq!(report.evicted, 0);
        assert_eq!(report.failed, []);
        assert!(rig.hot_has(KEY).await);
        assert_eq!(rig.hot.calls_on(Op::Head, KEY), 0);
        assert_eq!(rig.marks.calls(Op::Put), 0);
    }
}

/// Asserts that a pass whose format object cannot be read fails every key with that error and
/// touches nothing.
#[tokio::test]
async fn a_pass_whose_format_cannot_be_read_fails_every_key() {
    let rig = Rig::new();
    both(&rig, "bytes").await;
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    let down = Error::Unavailable("hot down".into());
    rig.hot.fail_next_with(Op::Get, down.clone());
    let keys = vec![KEY.to_owned(), "other".to_owned()];
    let report = ev.evict_pass(keys, 4).await;
    assert!(!report.active);
    assert_eq!(
        report.failed,
        [(KEY.to_owned(), down.clone()), ("other".to_owned(), down)]
    );
    assert!(rig.hot_has(KEY).await);
}

/// Asserts that a pass counts what became of each key: one evicted (with the bytes freed), one
/// marked `DIRTY` for another cold copy, one young, one under a marker, one gone, one whose store
/// failed (given back with its error and the marker released), and an unreleased lock; and that
/// the keys are worked at most `parallel` at a time (a pass of one key at a time gives the same).
#[tokio::test]
async fn a_pass_counts_what_became_of_each_key_and_gives_back_those_that_failed() {
    let rig = Rig::new();
    rig.activate().await;
    let records = Arc::new(CopyRecords::default());
    let ev = evictor(&rig, &records).await;
    for key in ["evicted", "dirtied", "kept", "young", "failed"] {
        rig.hot_mem.put(key, bytes("12345")).await.unwrap();
        rig.cold_mem.put(key, bytes("12345")).await.unwrap();
    }
    rig.cold_mem.delete("dirtied").await.unwrap();
    rig.mono.advance(secs(600));
    // "young" was copied just now.
    let (hot, cold) = rig.tags("young").await;
    records.record("young", &hot, &cold, 600_000, 10);
    rig.seed(&dirty("kept")).await;
    rig.hot.fail_next_on(
        Op::DeleteIfMatch,
        "failed",
        Error::Reserved("failed".into()),
    );
    let keys: Vec<String> = ["evicted", "dirtied", "kept", "young", "failed", "gone"]
        .map(str::to_owned)
        .to_vec();
    let report = ev.evict_pass(keys, 1).await;
    assert!(report.active);
    assert_eq!(report.evicted, 1);
    assert_eq!(report.freed, 5);
    assert_eq!(report.dirtied, 1);
    assert_eq!(report.young, 1);
    assert_eq!(report.kept, 1);
    assert_eq!(report.gone, 1);
    assert_eq!(report.unreleased, []);
    assert_eq!(
        report.failed,
        [("failed".to_owned(), Error::Reserved("failed".into()))]
    );
}

/// Asserts that a pass that cannot release a lock after a delete reports the error with the count.
#[tokio::test]
async fn a_pass_reports_the_release_errors_of_the_locks_it_could_not_end() {
    let rig = Rig::new();
    rig.activate().await;
    let records = Arc::new(CopyRecords::default());
    both(&rig, "12345").await;
    let hot = Arc::new(GateBackend::new(rig.hot.clone()));
    let delete = hot.gate(Op::DeleteIfMatch, KEY, Phase::Post);
    let lease = rig.lease(LEADER).await;
    let ev = evictor_over(&rig, hot, rig.markers(LEADER), lease, &records, |_| {});
    rig.mono.advance(secs(600));
    let mut pass = Box::pin(ev.evict_pass(vec![KEY.to_owned()], 2));
    until_parked(&delete, &mut pass).await;
    rig.marks.set_down(true);
    delete.release();
    let report = pass.await;
    assert_eq!(report.evicted, 1);
    assert_eq!(report.unreleased.len(), 1);
}

// ---- the records ----

/// Asserts the records: a pair is found only with both tags, a record is replaced by the next of
/// its key (and its age with it), the oldest are forgotten beyond the limit, a limit of zero keeps
/// one, `forget` drops one and `clear` all.
#[test]
fn the_records_hold_the_newest_pairs_up_to_the_limit() {
    let records = CopyRecords::default();
    records.record("a", "h1", "c1", 10, 2);
    records.record("b", "h2", "c2", 20, 2);
    assert_eq!(records.copied_at("a", "h1", "c1"), Some(10));
    assert_eq!(records.copied_at("a", "h1", "other"), None);
    assert_eq!(records.copied_at("a", "other", "c1"), None);
    assert_eq!(records.copied_at("none", "h1", "c1"), None);
    records.record("a", "h3", "c3", 30, 2);
    assert_eq!(records.copied_at("a", "h1", "c1"), None);
    assert_eq!(records.copied_at("a", "h3", "c3"), Some(30));
    records.record("c", "h4", "c4", 40, 2);
    assert_eq!(records.get("b"), None);
    assert!(records.get("a").is_some());
    records.record("d", "h5", "c5", 50, 0);
    assert_eq!(records.get("a"), None);
    assert!(records.get("d").is_some());
    records.forget("d");
    records.forget("d");
    assert_eq!(records.get("d"), None);
    records.record("e", "h6", "c6", 60, 5);
    records.clear();
    assert_eq!(records.get("e"), None);
}
