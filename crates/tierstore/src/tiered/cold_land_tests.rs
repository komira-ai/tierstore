//! The emergency landing in the cold tier: that it is off unless asked for, the requests it makes
//! and in what order, what the marker says while it works, and what is left when a step fails or
//! the proxy is lost.
//!
//! No test waits: the order of the landing's calls is made by a gate on one of them.

use std::time::Duration;

use super::marked_put_tests::{
    MARKS_FORMAT, Rig, Shape, held, marker, settle, streamed, until_parked,
};
use super::overwrite::PlainPut;
use crate::{
    Error, MarkMove, MarkerState, MarkerWhy, Markers, MarkersConfig, Op, Phase, RecordingSleeper,
};
use std::sync::Arc;

/// A rig whose landing is switched on.
async fn landing() -> Rig {
    Rig::with(MARKS_FORMAT, |config| config.emergency_cold_landing = true).await
}

fn locked() -> Option<Shape> {
    let owner = Some("p1".to_owned());
    let why = Some(MarkerWhy::ColdLand);
    Some((MarkerState::Lock, false, true, why, owner))
}

fn dirty() -> Option<Shape> {
    Some((MarkerState::Dirty, false, true, None, None))
}

fn log(calls: &[(Op, &str)]) -> Vec<(Op, String)> {
    calls
        .iter()
        .map(|(op, key)| (*op, (*key).to_owned()))
        .collect()
}

async fn reap(rig: &Rig) {
    let leader = Markers::new(
        rig.marks_mem.clone(),
        "leader",
        MarkersConfig::default(),
        rig.mono.clone(),
        Arc::new(RecordingSleeper::new()),
    )
    .unwrap();
    let seen = leader.read("k").await.unwrap().unwrap();
    leader.advance(&seen, MarkMove::Reap).await.unwrap();
}

/// Asserts that a put the hot store refused with `Full` is answered as a client retries, with
/// nothing asked of either tier or the marks bucket, unless the landing is switched on.
#[tokio::test]
async fn a_put_the_hot_store_refuses_with_full_is_503_with_emergency_cold_landing_off() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert!(rig.hot_log().is_empty());
    assert!(rig.marks_log().is_empty());
    assert!(rig.cold_faults.call_log().is_empty());
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
}

/// Asserts the landing of 5.5 in order: the cold HEAD and the marker together, the lock
/// `COLDLAND`, the cold write, the hot HEAD under the lock, the hot DELETE, then the answer, and
/// after the answer the lock settled to `CLEARING` and the marker removed. The answer is not given
/// before the hot DELETE has returned.
#[tokio::test]
async fn a_put_the_hot_store_refuses_with_full_lands_in_cold_under_the_lock_when_it_is_on() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let write = rig.cold.gate(Op::Put, "k", Phase::Pre);
    let delete = rig.hot.gate(Op::Delete, "k", Phase::Post);

    let mut put = Box::pin(rig.proxy.land_in_cold("k", held("new")));
    until_parked(&write, &mut put).await;
    assert_eq!(rig.shape("k").await, locked());
    assert_eq!(rig.cold_text("k").await, None);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert!(write.release());

    // The hot object is gone and the answer is still held back.
    until_parked(&delete, &mut put).await;
    assert_eq!(rig.hot_text("k").await, None);
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
    assert_eq!(rig.shape("k").await, locked());
    assert!(delete.release());
    let done = put.await.unwrap();
    settle().await;

    assert_eq!(
        done,
        PlainPut {
            tag: None,
            wrote: true
        }
    );
    assert_eq!(rig.marker("k").await, None);
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
    let cold = log(&[(Op::Head, "k"), (Op::Put, "k")]);
    assert_eq!(rig.cold_faults.call_log(), cold);
    assert_eq!(rig.hot_log(), log(&[(Op::Head, "k"), (Op::Delete, "k")]));
    let marks = log(&[
        (Op::Get, "k"),
        (Op::Put, "k"),
        (Op::PutIfMatch, "k"),
        (Op::Delete, "k"),
    ]);
    assert_eq!(rig.marks_log(), marks);
}

/// Asserts that the cold write is create-only where cold has no copy and `If-Match` its tag where
/// it has one, for a body held whole and for a stream, and that a key the hot tier does not hold
/// gets no DELETE.
#[tokio::test]
async fn a_cold_landing_writes_create_only_or_by_the_cold_tag_and_deletes_hot_only_if_it_has_the_key()
 {
    for (stream, cold_has, hot_has) in [
        (false, false, true),
        (false, true, true),
        (true, false, true),
        (true, true, true),
        (false, false, false),
    ] {
        let rig = landing().await;
        if cold_has {
            rig.seed_cold("k", "older").await;
        }
        if hot_has {
            rig.seed_hot("k", "old").await;
        }
        let body = if stream {
            streamed(&["ne", "w"])
        } else {
            held("new")
        };
        let put = rig.proxy.land_in_cold("k", body).await.unwrap();
        settle().await;
        assert!(put.wrote);
        assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
        assert_eq!(rig.hot_text("k").await, None);
        let case = (stream, cold_has, hot_has);
        assert_eq!(
            rig.cold_faults.calls(Op::PutIfMatch),
            u64::from(cold_has),
            "{case:?}"
        );
        assert_eq!(
            rig.cold_faults.calls(Op::Put),
            u64::from(!cold_has),
            "{case:?}"
        );
        assert_eq!(
            rig.hot_faults.calls(Op::Delete),
            u64::from(hot_has),
            "{case:?}"
        );
        assert_eq!(rig.marker("k").await, None);
    }
}

/// Asserts that a key the marker says is deleted is refused while the tombstone stands: the landing
/// could not end `gone` before cold holds the new object, so nothing is written to either tier or
/// the marker.
#[tokio::test]
async fn a_cold_landing_is_refused_while_the_marker_says_gone() {
    let rig = landing().await;
    let gone = marker("k", MarkerState::Dirty, true, None);
    rig.seed_marker(&gone).await;
    rig.seed_cold("k", "older").await;
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("older"));
    assert_eq!(rig.marker("k").await, Some(gone));
    assert_eq!(rig.cold_faults.calls(Op::Put), 0);
    assert_eq!(rig.cold_faults.calls(Op::PutIfMatch), 0);
}

/// Asserts that a landing that cannot ask the cold tier takes no lock and writes nothing.
#[tokio::test]
async fn a_cold_landing_while_the_cold_tier_is_down_is_unavailable_and_takes_no_lock() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.cold_faults.set_down(true);
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.marker("k").await, None);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
}

/// Asserts that a cold write the cold tier refuses because cold changed since the HEAD (the key was
/// created, or its tag moved) is a retry for the client, with hot as it was and the lock released
/// as `DIRTY`.
#[tokio::test]
async fn a_cold_write_that_finds_cold_changed_is_unavailable_and_hot_is_kept() {
    for cold_has in [false, true] {
        let rig = landing().await;
        rig.seed_hot("k", "old").await;
        if cold_has {
            rig.seed_cold("k", "older").await;
            let error = Error::PreconditionFailed("k".to_owned());
            rig.cold_faults.fail_next_with(Op::PutIfMatch, error);
        } else {
            let error = Error::AlreadyExists("k".to_owned());
            rig.cold_faults.fail_next_with(Op::Put, error);
        }
        let put = rig.proxy.land_in_cold("k", held("new")).await;
        settle().await;
        assert!(matches!(put, Err(Error::Unavailable(_))), "{cold_has}");
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
        assert_eq!(rig.shape("k").await, dirty());
    }
}

/// Asserts that a hot DELETE that fails for a reason that says it was not done answers 503, never
/// acknowledges, and releases the lock as `DIRTY`: hot is the truth, and cold already holds the
/// landed bytes, which the copier will replace.
#[tokio::test]
async fn a_cold_landing_whose_hot_delete_fails_is_503_and_dirty_and_never_acknowledged() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let error = Error::Unsupported("delete".to_owned());
    rig.hot_faults.fail_next_with(Op::Delete, error.clone());
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    settle().await;
    assert_eq!(put, Err(error));
    assert_eq!(rig.shape("k").await, dirty());
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
}

/// Asserts that a hot HEAD under the lock that fails answers 503 and releases the lock as `DIRTY`.
#[tokio::test]
async fn a_cold_landing_whose_hot_head_fails_is_unavailable_and_dirty() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.hot_faults.fail_next(Op::Head, 1);
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    settle().await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.shape("k").await, dirty());
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
}

/// Asserts that a hot DELETE whose outcome is unknown (it may still land at the store) keeps the
/// lock for `queued_call_margin` before it is released as `DIRTY`, so that a writer that arrives
/// meanwhile cannot have its object removed by the late DELETE.
#[tokio::test]
async fn a_cold_landing_whose_hot_delete_is_unknown_keeps_its_lock_for_the_queued_call_margin() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.pacer.hold(Duration::from_secs(60));
    rig.hot_faults.fail_next(Op::Delete, 1);
    let put = rig.proxy.land_in_cold("k", held("new")).await;
    settle().await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.pacer.asked(), vec![Duration::from_secs(60)]);
    assert_eq!(rig.shape("k").await, locked());

    rig.pacer.open();
    settle().await;
    assert_eq!(rig.shape("k").await, dirty());
}

/// Asserts that a landing whose lock is too old to destroy under writes nothing to cold, and one
/// whose lock grew too old during the cold write deletes nothing from hot: either way the lock is
/// released as `DIRTY`.
#[tokio::test]
async fn a_cold_landing_under_a_lock_too_old_destroys_nothing() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let lock = rig.marks.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.proxy.land_in_cold("k", held("new")));
    until_parked(&lock, &mut put).await;
    rig.mono.advance(Duration::from_secs(61));
    assert!(lock.release());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.cold_text("k").await, None);
    assert_eq!(rig.shape("k").await, dirty());

    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let written = rig.cold.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.proxy.land_in_cold("k", held("new")));
    until_parked(&written, &mut put).await;
    rig.mono.advance(Duration::from_secs(61));
    assert!(written.release());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
    assert_eq!(rig.shape("k").await, dirty());
}

/// Asserts that a settled lock whose marker cannot be removed is left as a `CLEARING` owned by the
/// proxy, which the leader finishes.
#[tokio::test]
async fn a_landing_whose_marker_cannot_be_removed_leaves_a_clearing_for_the_leader() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.fail_next(Op::Delete, 1);
    rig.proxy.land_in_cold("k", held("new")).await.unwrap();
    settle().await;
    let owner = Some("p1".to_owned());
    let clearing = Some((MarkerState::Clearing, false, false, None, owner));
    assert_eq!(rig.shape("k").await, clearing);
}

/// Asserts the crash of 7.2 after the lock and before the cold write: the leader reaps the lock to
/// `DIRTY(gone=false)`, hot (older) is the truth, and cold has nothing.
#[tokio::test]
async fn a_crash_holding_the_coldland_lock_before_the_cold_write_keeps_hot() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let write = rig.cold.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.proxy.land_in_cold("k", held("new")));
    until_parked(&write, &mut put).await;
    std::mem::forget(put);
    reap(&rig).await;
    assert_eq!(rig.shape("k").await, dirty());
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.cold_text("k").await, None);
}

/// Asserts the crash of 7.2 after the cold write and before the hot DELETE: reaped to `DIRTY`,
/// hot (older) is the truth and the copier will overwrite the cold landing, which no client was
/// told of.
#[tokio::test]
async fn a_crash_holding_the_coldland_lock_after_the_cold_write_before_the_hot_delete_keeps_hot() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    let head = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let mut put = Box::pin(rig.proxy.land_in_cold("k", held("new")));
    until_parked(&head, &mut put).await;
    std::mem::forget(put);
    reap(&rig).await;
    assert_eq!(rig.shape("k").await, dirty());
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
}

/// Asserts the crash of 7.2 after the hot DELETE and before the release (the release is made to
/// fail as far as it is tried): hot is absent and cold holds the landed bytes, which are the
/// truth; the leader's reap leaves `DIRTY`, which finds hot absent.
#[tokio::test]
async fn a_crash_holding_the_coldland_lock_after_the_hot_delete_before_the_release_leaves_cold_as_truth()
 {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 4);
    assert!(
        rig.proxy
            .land_in_cold("k", held("new"))
            .await
            .unwrap()
            .wrote
    );
    settle().await;
    assert_eq!(rig.shape("k").await, locked());
    reap(&rig).await;
    assert_eq!(rig.shape("k").await, dirty());
    assert_eq!(rig.hot_text("k").await, None);
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
}

/// Asserts that a landing over a `DIRTY` marker that is not `gone` takes the lock by a swap from the
/// version it read, with no failed create.
#[tokio::test]
async fn a_cold_landing_over_a_dirty_marker_locks_it_by_cas() {
    let rig = landing().await;
    rig.seed_hot("k", "old").await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false, None))
        .await;
    rig.proxy.land_in_cold("k", held("new")).await.unwrap();
    settle().await;
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);
    assert_eq!(rig.marker("k").await, None);
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
}
