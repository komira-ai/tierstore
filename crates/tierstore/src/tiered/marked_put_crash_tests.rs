//! A put on markers that dies, errs or is overtaken after each of its steps (section 7.1 of the
//! design): what the stores hold, what the client is told, and that nothing more is written.

use std::time::Duration;

use futures::StreamExt;
use futures::stream;

use super::marked_put_rig::{Effect, MarkRig, body, dirty, locked, marker, secs};
use crate::{Backend, Error, MarkerState, MarkerWhy, Op, Phase};

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Asserts that a put that dies after its hot HEAD (the task is dropped) leaves nothing: no
/// object, no marker.
#[tokio::test]
async fn a_put_that_dies_after_its_head_leaves_nothing() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());
    assert!(!rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());
}

/// Asserts that a put that dies after its marker landed and before its object has leaves a marker
/// with nothing behind it, and that a read of the key is not changed by it: it is the cold tier's
/// object, or none.
#[tokio::test]
async fn a_put_that_dies_after_its_marker_landed_leaves_a_dirty_marker_and_no_object() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());

    assert!(!rig.hot_has("k").await);
    let marker = rig.marker("k").await.unwrap();
    assert_eq!((marker.st, marker.gone), (MarkerState::Dirty, false));
    assert_eq!(
        rig.proxy.get("k", None).await,
        Err(Error::NotFound("k".to_owned()))
    );
    rig.cold_mem.put("k", body("old")).await.unwrap();
    assert_eq!(rig.proxy.get("k", None).await.unwrap(), body("old"));
}

/// Asserts that a put that dies after its object landed and before its marker has leaves the
/// object, unmarked, and that a read returns it.
#[tokio::test]
async fn a_put_that_dies_after_its_object_landed_leaves_it_unmarked() {
    let rig = MarkRig::new().await;
    let held = rig.marks.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    while !rig.hot_has("k").await {
        tokio::task::yield_now().await;
    }
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());

    assert!(rig.marker("k").await.is_none());
    assert_eq!(rig.proxy.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a put that is made again after its answer was lost is 412 and changes nothing: a
/// create-only retry that did land gets the answer S3 gives it.
#[tokio::test]
async fn a_retried_create_only_put_after_a_lost_answer_is_412() {
    let rig = MarkRig::new().await;
    rig.proxy.put("k", body("v")).await.unwrap();
    let (_, tag) = rig.stored("k").await.unwrap();
    let again = rig.proxy.put("k", body("v")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.stored("k").await.unwrap().1, tag);
}

/// Asserts that a put whose object landed and whose marker create met a `DIRTY` or a `WANT`
/// marker moves it to `DIRTY` by a compare-and-swap, keeping its `gone`, and acknowledges.
#[tokio::test]
async fn a_put_that_finds_a_marker_not_held_moves_it_to_dirty_keeping_gone() {
    let rig = MarkRig::new().await;
    let want = marker("w", MarkerState::Want, false, false);
    for (key, seeded) in [
        ("d", dirty("d", false)),
        ("g", dirty("g", true)),
        ("w", want),
    ] {
        rig.seed(&seeded).await;
        rig.proxy.put(key, body("v")).await.unwrap();
        let moved = rig.marker(key).await.unwrap();
        assert_eq!(moved.st, MarkerState::Dirty, "{key}");
        assert_eq!((moved.gone, moved.dirty), (seeded.gone, true), "{key}");
        assert_ne!(moved.nonce, seeded.nonce, "{key}");
        let ops = rig.marks_ops(key);
        assert_eq!(ops, [Op::Get, Op::Put, Op::Get, Op::PutIfMatch], "{key}");
    }
}

/// Asserts that a marker that the create was refused for and that is gone by the time the put
/// reads it is created, and that a create refused again is looked at again.
#[tokio::test]
async fn a_marker_that_vanishes_between_the_412_and_the_get_is_created() {
    let rig = MarkRig::new().await;
    let exists = Error::AlreadyExists("k".to_owned());
    rig.marks_faults.fail_next_with(Op::Put, exists.clone());
    rig.marks_faults.fail_next_with(Op::Put, exists);
    rig.proxy.put("k", body("v")).await.unwrap();
    let ops = rig.marks_ops("k");
    assert_eq!(ops, [Op::Get, Op::Put, Op::Get, Op::Put, Op::Get, Op::Put]);
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
}

/// Asserts that a put that finds a lock waits with the polls of the config; when the lock ends in
/// a `DIRTY` marker it moves it by a compare-and-swap and acknowledges; when the lock ends with the
/// marker gone and the hot object removed (a delete came after the put: the order is put, then
/// delete) it is acknowledged and writes nothing more.
#[tokio::test]
async fn a_put_that_finds_a_lock_waits_and_then_marks_dirty_or_is_overtaken_by_the_delete() {
    let rig = MarkRig::new().await;
    rig.seed(&locked("k", MarkerWhy::Overwrite, false)).await;
    rig.sleeper
        .then(2, Effect::Marker("k".to_owned(), Some(dirty("k", false))));
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.sleeper.asked(), [millis(20), millis(40)]);
    let marker = rig.marker("k").await.unwrap();
    assert_eq!((marker.st, marker.owner), (MarkerState::Dirty, None));
    assert_ne!(marker.nonce, "other");
    assert!(rig.hot_has("k").await);

    rig.seed(&locked("d", MarkerWhy::Delete, false)).await;
    rig.sleeper.then(3, Effect::Marker("d".to_owned(), None));
    rig.sleeper.then(3, Effect::DeleteHot("d".to_owned()));
    rig.proxy.put("d", body("v")).await.unwrap();
    assert!(!rig.hot_has("d").await);
    assert!(rig.marker("d").await.is_none());
}

/// Asserts that a put whose wait for a lock ends and finds hot's object and no marker creates the
/// marker.
#[tokio::test]
async fn a_put_whose_lock_ended_with_no_marker_and_hot_present_creates_the_marker() {
    let rig = MarkRig::new().await;
    rig.seed(&locked("k", MarkerWhy::Evict, false)).await;
    rig.sleeper.then(1, Effect::Marker("k".to_owned(), None));
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
}

/// Asserts that a lock that does not end within `lock_wait` is a 503 for a put whose object
/// landed: an unknown outcome, the object stays, the lock is not touched, and the polls add up to
/// `lock_wait`.
#[tokio::test]
async fn a_put_whose_object_landed_and_whose_lock_wait_ended_is_503_unknown() {
    let rig = MarkRig::new().await;
    let lock = locked("k", MarkerWhy::Overwrite, false);
    rig.seed(&lock).await;
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.marker("k").await, Some(lock));
    assert_eq!(rig.sleeper.asked().into_iter().sum::<Duration>(), secs(5));
}

/// Asserts that a failure to ask hot after a lock ended is retried like any failure of the marker
/// step, and the put is acknowledged.
#[tokio::test]
async fn a_hot_head_that_fails_after_a_lock_ended_is_tried_again() {
    let rig = MarkRig::new().await;
    rig.seed(&locked("k", MarkerWhy::Overwrite, false)).await;
    rig.sleeper
        .then(1, Effect::Marker("k".to_owned(), Some(dirty("k", false))));
    rig.sleeper
        .then(1, Effect::FailHotHead(Error::Unavailable("x".to_owned())));
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.sleeper.asked(), [millis(20), millis(100)]);
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
}

/// Asserts that a marker that is moved under the put on every try ends the put after
/// `entry_rounds` tries with 503, an unknown outcome: the object landed.
#[tokio::test]
async fn a_marker_cas_loop_that_exhausts_entry_rounds_is_503_unknown() {
    let rig = MarkRig::new().await;
    rig.seed(&dirty("k", false)).await;
    let lost = || Error::PreconditionFailed("k".to_owned());
    for _ in 0..3 {
        rig.marks_faults.fail_next_with(Op::PutIfMatch, lost());
    }
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(rig.hot_has("k").await);
    let again = [Op::Get, Op::PutIfMatch];
    let want: Vec<Op> = [Op::Get, Op::Put]
        .into_iter()
        .chain(again.repeat(3))
        .collect();
    assert_eq!(rig.marks_ops("k"), want);
}

/// Asserts that a marker write that keeps failing is tried again with the pauses of the step
/// retry, doubling to their longest, until `marker_deadline`, and then the put is 503, an unknown
/// outcome, with the object landed and no marker.
#[tokio::test]
async fn a_put_whose_marker_retries_pass_marker_deadline_is_503_unknown() {
    let rig = MarkRig::new().await;
    rig.marks_faults.fail_next(Op::Put, 1000);
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());

    let asked = rig.sleeper.asked();
    let start = [100, 200, 400, 800, 1600].map(millis);
    assert_eq!(asked[..5], start);
    assert!(
        asked[5..].iter().all(|pause| *pause == secs(2)),
        "{asked:?}"
    );
    let (last, before) = asked.split_last().unwrap();
    assert!(before.iter().sum::<Duration>() < secs(30));
    assert!(asked.iter().sum::<Duration>() >= secs(30), "{last:?}");
}

/// Asserts that a marker write that fails is tried again and the put is acknowledged when one
/// lands.
#[tokio::test]
async fn a_marker_write_that_fails_is_tried_again_and_acknowledged_when_it_lands() {
    let rig = MarkRig::new().await;
    rig.marks_faults.fail_next(Op::Put, 2);
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.sleeper.asked(), [millis(100)]);
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
}

/// Asserts that a marker create whose answer was lost after the store made it is read back by its
/// nonce: the put is acknowledged and no second marker is written.
#[tokio::test]
async fn an_ambiguous_marker_create_is_read_back_by_nonce() {
    let rig = MarkRig::new().await;
    let held = rig.marks.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    assert!(held.fail_post());
    put.await.unwrap().unwrap();

    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put, Op::Get]);
    assert!(rig.sleeper.asked().is_empty());
    assert!(rig.marker("k").await.unwrap().nonce.ends_with(".0"));
}

/// Asserts that a put whose object landed more than `reassert_after` after its marker was made
/// reads the marker again before it acknowledges and makes it again when it was cleared meanwhile
/// (the copier clears an orphan marker while the body streams).
#[tokio::test]
async fn a_put_whose_body_outlasts_orphan_after_re_asserts_its_marker_before_the_ack() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    rig.mono.advance(secs(31));
    rig.marks_mem.delete("k").await.unwrap();
    assert!(held.release());
    put.await.unwrap().unwrap();

    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put, Op::Get, Op::Put]);
}

/// Asserts that the re-assertion of a marker that is still there moves it by a compare-and-swap,
/// and that a put whose object landed exactly `reassert_after` after its marker does not re-assert.
#[tokio::test]
async fn a_slow_put_moves_a_marker_that_is_still_there_and_a_put_at_the_limit_does_not() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "slow", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("slow", body("v")).await });
    held.parked().await;
    let (_, before) = rig.stored("slow").await.unwrap();
    rig.mono.advance(secs(31));
    assert!(held.release());
    put.await.unwrap().unwrap();
    assert_eq!(
        rig.marks_ops("slow"),
        [Op::Get, Op::Put, Op::Get, Op::PutIfMatch]
    );
    assert_ne!(rig.stored("slow").await.unwrap().1, before);

    let held = rig.hot.gate(Op::Put, "edge", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("edge", body("v")).await });
    held.parked().await;
    rig.mono.advance(secs(30));
    assert!(held.release());
    put.await.unwrap().unwrap();
    assert_eq!(rig.marks_ops("edge"), [Op::Get, Op::Put]);
}

/// Asserts that a put that fails for good after its marker landed removes the marker it made by a
/// change to `CLEARING` on the version it made and a delete, and that a marker another writer took
/// over meanwhile (the change loses) is left alone.
#[tokio::test]
async fn a_put_that_fails_after_its_marker_landed_removes_the_marker_by_cas_and_delete() {
    let rig = MarkRig::new().await;
    let bad = || Error::BadDigest("k".to_owned());
    rig.hot_faults.fail_next_with(Op::Put, bad());
    assert_eq!(rig.proxy.put("k", body("v")).await, Err(bad()));
    assert!(rig.marker("k").await.is_none());
    assert_eq!(
        rig.marks_ops("k"),
        [Op::Get, Op::Put, Op::PutIfMatch, Op::Delete]
    );

    rig.hot_faults.fail_next_with(Op::Put, bad());
    let lost = Error::PreconditionFailed("k".to_owned());
    rig.marks_faults.fail_next_with(Op::PutIfMatch, lost);
    assert_eq!(rig.proxy.put("k", body("v")).await, Err(bad()));
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
}

/// Asserts that a put whose hot write may have landed (the store did not answer) is 503 and writes
/// nothing more: the marker it made stays, for the copier or the sweep to settle.
#[tokio::test]
async fn a_put_whose_hot_write_may_have_landed_is_503_and_keeps_its_marker() {
    let rig = MarkRig::new().await;
    rig.hot_faults.fail_next(Op::Put, 1);
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put]);
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Put]);
}

/// Asserts that a streamed put whose body ends with an error leaves no object and no marker, and
/// is that error.
#[tokio::test]
async fn a_streamed_put_whose_body_fails_leaves_no_object_and_no_marker() {
    let rig = MarkRig::new().await;
    let reset = || Error::Other("reset".to_owned());
    let body = stream::iter(vec![Ok(body("ab")), Err(reset())]).boxed();
    assert_eq!(rig.proxy.put_stream("k", Some(4), body).await, Err(reset()));
    assert!(!rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());
}

/// Asserts that a put that dies while it waits for a lock after its object landed leaves the
/// object and the lock of the other writer as they are: the object stands unmarked until the sweep
/// adopts it, and a read returns it.
#[tokio::test]
async fn a_put_that_dies_after_its_object_landed_and_its_marker_met_a_lock_leaves_the_object_unmarked()
 {
    let rig = MarkRig::new().await;
    let lock = locked("k", MarkerWhy::Overwrite, false);
    rig.seed(&lock).await;
    let landing = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    landing.parked().await;
    let waiting = rig.marks.gate(Op::Get, "k", Phase::Pre);
    assert!(landing.release());
    waiting.parked().await;
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());

    assert!(rig.hot_has("k").await);
    assert_eq!(rig.marker("k").await, Some(lock));
    assert_eq!(rig.proxy.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a marker change whose answer was lost after the store made it is read back by its
/// nonce: the put is acknowledged and the change is not made twice.
#[tokio::test]
async fn an_ambiguous_marker_cas_is_read_back_by_nonce() {
    let rig = MarkRig::new().await;
    rig.seed(&dirty("k", false)).await;
    let held = rig.marks.gate(Op::PutIfMatch, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    assert!(held.fail_post());
    put.await.unwrap().unwrap();

    let ops = rig.marks_ops("k");
    assert_eq!(ops, [Op::Get, Op::Put, Op::Get, Op::PutIfMatch, Op::Get]);
    assert!(rig.sleeper.asked().is_empty());
    assert!(rig.marker("k").await.unwrap().nonce.ends_with(".1"));
}
