//! Wave 1 of a put on markers and the answer of a put that finds its key: 412, 409 and 503, and
//! the marker a put that lost leaves or removes.

use super::marked_put_rig::{MARKS_FORMAT, MarkRig, body, dirty, locked, marker, secs};
use crate::{Backend, Error, MarkerState, MarkerWhy, Op, Phase};

fn exists(key: &str) -> Error {
    Error::AlreadyExists(key.to_owned())
}

fn conflict(key: &str) -> Error {
    Error::Conflict(key.to_owned())
}

/// Asserts that a put of a key the hot tier has is 412 and writes nothing: after its first wave it
/// reads the marker once more, and the marker is as it was.
#[tokio::test]
async fn a_create_only_put_of_a_hot_key_is_412() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    assert_eq!(rig.proxy.put("k", body("new")).await, Err(exists("k")));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("old"));
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Get]);
    assert!(rig.marker("k").await.is_none());
    assert_eq!(rig.hot_ops("k"), [Op::Head]);
}

/// Asserts that a put of a key only the cold tier has is 412 and writes nothing, to the hot tier
/// or to the marks bucket.
#[tokio::test]
async fn a_create_only_put_of_a_cold_only_key_is_412_and_writes_nothing() {
    let rig = MarkRig::new().await;
    rig.cold_mem.put("k", body("old")).await.unwrap();
    assert_eq!(rig.proxy.put("k", body("new")).await, Err(exists("k")));
    assert!(!rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());
    assert_eq!(rig.hot_ops("k"), [Op::Head]);
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Get]);
}

/// Asserts that a put of a key that was deleted, whose cold copy remains under a `gone` marker, is
/// created: the cold copy is not the key. The marker keeps `gone`.
#[tokio::test]
async fn a_create_only_put_of_a_deleted_key_whose_cold_copy_remains_succeeds() {
    let rig = MarkRig::new().await;
    rig.cold_mem.put("k", body("old")).await.unwrap();
    rig.seed(&dirty("k", true)).await;
    rig.proxy.put("k", body("new")).await.unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("new"));
    let marker = rig.marker("k").await.unwrap();
    assert_eq!((marker.st, marker.gone), (MarkerState::Dirty, true));
}

/// Asserts that a put over a deleted key writes the object before it touches the marker, and that
/// the marker keeps `gone` when it does: with the object's write held no write has been made to the
/// marker, and after it the marker is the same marker changed by a compare-and-swap.
#[tokio::test]
async fn a_put_over_a_deleted_key_writes_the_object_before_the_marker_and_keeps_gone() {
    let rig = MarkRig::new().await;
    rig.seed(&dirty("k", true)).await;
    let (_, before) = rig.stored("k").await.unwrap();
    let held = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("new")).await });
    held.parked().await;
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.stored("k").await.unwrap().1, before);
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put]);
    assert!(held.release());
    put.await.unwrap().unwrap();

    let (marker, after) = rig.stored("k").await.unwrap();
    assert_ne!(after, before);
    assert_eq!(
        (marker.st, marker.gone, marker.dirty),
        (MarkerState::Dirty, true, true)
    );
    assert_eq!(
        rig.marks_ops("k"),
        [Op::Get, Op::Put, Op::Get, Op::PutIfMatch]
    );
}

/// Asserts that a put of a key that a delete holds (a lock for `DELETE`) is 409 when it finds the
/// key in the hot tier, and not 412.
#[tokio::test]
async fn a_create_only_put_is_409_when_the_marker_shows_a_delete_in_flight() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    rig.seed(&locked("k", MarkerWhy::Delete, false)).await;
    assert_eq!(rig.proxy.put("k", body("new")).await, Err(conflict("k")));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("old"));
}

/// Asserts that a put whose hot HEAD found the key, and whose key a delete then removed, is 409
/// and not 412: the re-check reads the marker, which says `gone`, and asks hot once more.
#[tokio::test]
async fn a_create_only_put_whose_key_was_deleted_after_its_head_is_409_and_not_412() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    let held = rig.hot.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("new")).await });
    held.parked().await;
    rig.hot_mem.delete("k").await.unwrap();
    rig.seed(&dirty("k", true)).await;
    assert!(held.release());
    assert_eq!(put.await.unwrap(), Err(conflict("k")));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Head]);
}

/// Asserts that a put of a key that was recreated after a delete is 412 and not 409: the marker
/// says `gone` and hot has the key.
#[tokio::test]
async fn a_create_only_put_of_a_recreated_key_is_412_not_409() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("new")).await.unwrap();
    rig.seed(&dirty("k", true)).await;
    assert_eq!(rig.proxy.put("k", body("newer")).await, Err(exists("k")));
    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Head]);
}

/// Asserts that the answer of a put whose re-check fails is that failure (503 at the S3 front), and
/// that a hot store that fails the HEAD of the re-check does the same.
#[tokio::test]
async fn a_recheck_that_fails_is_the_answer() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    rig.marks_faults.fail_next(Op::Get, 2);
    let put = rig.proxy.put("k", body("new")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Get]);

    rig.seed(&dirty("k", true)).await;
    rig.hot_faults
        .fail_next_with(Op::Head, Error::Other("x".to_owned()));
    // The first HEAD is wave 1's: it fails the put at once, before any re-check.
    assert_eq!(
        rig.proxy.put("k", body("new")).await,
        Err(Error::Other("x".to_owned()))
    );
    let held = rig.hot.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("new")).await });
    held.parked().await;
    rig.hot_faults
        .fail_next_with(Op::Head, Error::Other("y".to_owned()));
    assert!(held.release());
    assert_eq!(put.await.unwrap(), Err(Error::Other("y".to_owned())));
}

/// Asserts that a put that loses a creation race (hot has no object at its HEAD and one when it
/// writes) is 412, leaves the winner's object, and removes the marker it made.
#[tokio::test]
async fn a_create_only_put_that_loses_a_creation_race_is_412() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("loser")).await });
    held.parked().await;
    rig.hot_mem.put("k", body("winner")).await.unwrap();
    assert!(held.release());

    assert_eq!(put.await.unwrap(), Err(exists("k")));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("winner"));
    assert!(rig.marker("k").await.is_none());
    assert_eq!(
        rig.marks_ops("k"),
        [Op::Get, Op::Put, Op::Get, Op::PutIfMatch, Op::Delete]
    );
}

/// Asserts that a put that loses the race and finds another writer's marker (its own create was
/// refused) leaves that marker alone.
#[tokio::test]
async fn a_put_that_loses_a_creation_race_leaves_the_marker_of_the_writer_that_won() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("loser")).await });
    held.parked().await;
    rig.marks_mem.delete("k").await.unwrap();
    rig.hot_mem.put("k", body("winner")).await.unwrap();
    let theirs = marker("k", MarkerState::Dirty, false, true);
    rig.seed(&theirs).await;
    assert!(held.release());

    assert_eq!(put.await.unwrap(), Err(exists("k")));
    assert_eq!(rig.marker("k").await, Some(theirs));
}

/// Asserts that a put whose marker could not be written and whose object is refused removes
/// nothing: it made no marker, and the answer is the refusal.
#[tokio::test]
async fn a_put_whose_marker_write_failed_and_whose_object_was_refused_is_that_refusal() {
    let rig = MarkRig::new().await;
    rig.marks_faults.fail_next(Op::Put, 1);
    rig.hot_faults
        .fail_next_with(Op::Put, Error::BadDigest("k".to_owned()));
    assert_eq!(
        rig.proxy.put("k", body("v")).await,
        Err(Error::BadDigest("k".to_owned()))
    );
    assert!(rig.marker("k").await.is_none());
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put, Op::Get]);
}

/// Asserts that a create-only put of a key that only cold could have is 503 while the cold tier is
/// down, and writes nothing, and that a `gone` marker makes the cold tier's answer unneeded: the
/// put is made while cold is down.
#[tokio::test]
async fn a_create_only_put_and_a_presigned_put_are_503_while_the_cold_tier_is_down_unless_a_gone_marker_hides_cold()
 {
    let rig = MarkRig::new().await;
    rig.cold_faults.set_down(true);
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(!rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());
    let presigned =
        rig.proxy
            .put_stream_hot_only("p", Some(1), super::marked_put_rig::chunks(&["v"]));
    assert!(matches!(presigned.await, Err(Error::Unavailable(_))));
    assert!(!rig.hot_has("p").await);

    rig.seed(&dirty("g", true)).await;
    rig.proxy.put("g", body("v")).await.unwrap();
    assert!(rig.hot_has("g").await);
    assert_eq!(rig.cold_ops("g"), [Op::Head]);
}

/// Asserts that a failing hot HEAD, a failing read of the marker and a cold tier that answers with
/// a failure other than "down" fail the put with nothing written: it fails closed.
#[tokio::test]
async fn a_put_whose_wave_one_fails_is_refused_and_writes_nothing() {
    let rig = MarkRig::new().await;
    rig.hot_faults.fail_next(Op::Head, 1);
    let put = rig.proxy.put("a", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    rig.marks_faults.fail_next(Op::Get, 1);
    let put = rig.proxy.put("b", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    rig.cold_faults
        .fail_next_with(Op::Head, Error::Other("boom".to_owned()));
    let put = rig.proxy.put("c", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    for key in ["a", "b", "c"] {
        assert!(!rig.hot_has(key).await);
        assert!(rig.marker(key).await.is_none());
        assert!(!rig.hot_ops(key).contains(&Op::Put), "{key}");
    }
}

/// Asserts that a wave 1 slower than `read_window` is discarded and read again: the second wave
/// decides, and the put is made.
#[tokio::test]
async fn a_read_pair_slower_than_the_window_is_read_again() {
    let rig = MarkRig::new().await;
    let held = rig.cold.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    rig.mono.advance(secs(6));
    assert!(held.release());
    put.await.unwrap().unwrap();

    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Head, Op::Put]);
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Get, Op::Put]);
    assert!(rig.marker("k").await.is_some());
}

/// Asserts that a wave 1 that is slower than the window every time is read `read_retries` times
/// again and then the put is 503, with nothing written.
#[tokio::test]
async fn a_read_pair_slower_than_the_window_is_503_after_read_retries() {
    let rig = MarkRig::tuned(MARKS_FORMAT, |config| config.marks_put.read_retries = 2).await;
    let waves: Vec<_> = (0..3)
        .map(|_| rig.cold.gate(Op::Head, "k", Phase::Post))
        .collect();
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    for held in &waves {
        held.parked().await;
        rig.mono.advance(secs(6));
        assert!(held.release());
    }
    let put = put.await.unwrap();
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Head, Op::Head]);
    assert!(!rig.hot_has("k").await);
    assert!(rig.marker("k").await.is_none());
}

/// Asserts that a hot HEAD that found the key decides without the window: a slow wave that found
/// the key is 412 at once, without another wave.
#[tokio::test]
async fn a_slow_wave_that_found_the_key_in_hot_is_412_without_another_wave() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    let held = rig.cold.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("new")).await });
    held.parked().await;
    rig.mono.advance(secs(6));
    assert!(held.release());
    assert_eq!(put.await.unwrap(), Err(exists("k")));
    assert_eq!(rig.hot_ops("k"), [Op::Head]);
}

/// Asserts that a delete lock that is not for `DELETE` (an overwrite's) does not make a put
/// 409: the put finds the key and is 412.
#[tokio::test]
async fn a_create_only_put_of_a_key_under_another_lock_is_412() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    rig.seed(&locked("k", MarkerWhy::Overwrite, false)).await;
    assert_eq!(rig.proxy.put("k", body("new")).await, Err(exists("k")));
}
