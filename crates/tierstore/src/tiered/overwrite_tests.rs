//! The overwrite and the same-bytes put: the requests each makes and in what order, what the marker
//! says at each step, and what is left when a step fails or an actor is lost.
//!
//! Each test sets the final state of the tiers and of the marker beside the answer given. No test
//! waits: the order of two writers is made by a gate on the call of one of them.

use std::time::Duration;

use super::plain_put_tests::{
    Rig, Shape, body, chunks, held, marker, settle, streamed, until_parked,
};
use super::overwrite::{PlainPut, PutBody, PutClaim};
use crate::Backend;
use crate::{
    Error, MarkMove, MarkerState, MarkerWhy, Markers, MarkersConfig, Meta, Op, Phase,
    RecordingSleeper,
};
use std::sync::Arc;

fn dirty(gone: bool) -> Option<Shape> {
    Some((MarkerState::Dirty, gone, true, None, None))
}

fn locked(gone: bool) -> Option<Shape> {
    let owner = Some("p1".to_owned());
    Some((
        MarkerState::Lock,
        gone,
        true,
        Some(MarkerWhy::Overwrite),
        owner,
    ))
}

fn log(calls: &[(Op, &str)]) -> Vec<(Op, String)> {
    calls
        .iter()
        .map(|(op, key)| (*op, (*key).to_owned()))
        .collect()
}

/// The leader's side of a reap: the marker of `k` as `Markers` of another actor, which takes the
/// lock of a holder it has judged departed to the state its reason requires.
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

/// Asserts the overwrite of 5.3 for a body held whole: the hot HEAD and the marker together, the
/// lock created, the replace by the tag of wave 1 (the body can be sent again, so no HEAD under
/// the lock), the answer given while the lock still stands, and the release after it, to `DIRTY`.
/// The cold tier is not asked.
#[tokio::test]
async fn an_overwrite_takes_the_lock_replaces_by_the_tag_read_under_it_and_releases_after_the_ack()
{
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let release = rig.marks.gate(Op::PutIfMatch, "k", Phase::Pre);

    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&replace, &mut put).await;
    assert_eq!(rig.shape("k").await, locked(false));
    assert!(replace.release());
    let done = put.await.unwrap();

    // The answer was given with the release still held back.
    release.parked().await;
    assert_eq!(rig.shape("k").await, locked(false));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    let tag = rig.hot_tag("k").await;
    assert_eq!(
        done,
        PlainPut {
            tag: Some(tag),
            wrote: true
        }
    );
    assert!(release.release());
    settle().await;

    assert_eq!(rig.shape("k").await, dirty(false));
    let hot = log(&[(Op::Head, "k"), (Op::PutIfMatch, "k")]);
    assert_eq!(rig.hot_log(), hot);
    let marks = log(&[(Op::Get, "k"), (Op::Put, "k"), (Op::PutIfMatch, "k")]);
    assert_eq!(rig.marks_log(), marks);
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Asserts that a stream reads the tag under the lock (its bytes cannot be sent twice, so the tag
/// it replaces by must be the one the lock protects), and that the bytes of its chunks are the
/// object.
#[tokio::test]
async fn a_streamed_overwrite_reads_the_tag_under_the_lock_and_replaces_by_it() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let put = rig
        .proxy
        .put_plain("k", streamed(&["ne", "w"]), &PutClaim::default())
        .await
        .unwrap();
    settle().await;
    assert!(put.wrote);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    let hot = log(&[(Op::Head, "k"), (Op::Head, "k"), (Op::PutIfMatch, "k")]);
    assert_eq!(rig.hot_log(), hot);
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a marker that exists is read in wave 1 and taken over by a compare-and-swap, with
/// no failed create before it, whether it is `DIRTY` or `WANT`, and that the lock owes cold a
/// copy.
#[tokio::test]
async fn an_overwrite_over_a_dirty_marker_locks_it_by_cas_without_a_failed_create() {
    for st in [MarkerState::Dirty, MarkerState::Want] {
        let rig = Rig::new().await;
        rig.seed_hot("k", "old").await;
        rig.seed_marker(&marker("k", st.clone(), false, None)).await;
        rig.put("k", "new").await.unwrap();
        settle().await;
        let marks = log(&[(Op::Get, "k"), (Op::PutIfMatch, "k"), (Op::PutIfMatch, "k")]);
        assert_eq!(rig.marks_log(), marks, "{st:?}");
        assert_eq!(rig.shape("k").await, dirty(false), "{st:?}");
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    }
}

/// Asserts that an overwrite of a key whose marker says `gone` (it was deleted, recreated, and
/// the copier has not replaced cold yet) keeps `gone` through the lock and the release: the copier
/// ends it, not the put (5.3).
#[tokio::test]
async fn an_overwrite_of_a_recreated_key_keeps_gone_through_lock_and_release() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, true, None))
        .await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&replace, &mut put).await;
    assert_eq!(rig.shape("k").await, locked(true));
    assert!(replace.release());
    put.await.unwrap();
    settle().await;
    assert_eq!(rig.shape("k").await, dirty(true));
}

/// Asserts that two overwrites of one key both succeed and the one that takes the lock second
/// wins: the second waits for the first's release (a pause of the first poll, 20 ms), finds that the
/// tag of its wave 1 is gone (the first replaced the object), reads the tag again and replaces by
/// it.
#[tokio::test]
async fn two_overwrites_of_one_key_both_succeed_and_the_second_to_take_the_lock_wins() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let first = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("one"), &PutClaim::default())
                .await
        }
    });
    replace.parked().await;
    let second = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("two"), &PutClaim::default())
                .await
        }
    });
    rig.pacer.slept_for(Duration::from_millis(20)).await;
    assert!(replace.release());

    assert!(first.await.unwrap().unwrap().wrote);
    assert!(second.await.unwrap().unwrap().wrote);
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("two"));
    assert_eq!(rig.shape("k").await, dirty(false));
    // The first replace, the second's refused one, and its replace by the tag read again.
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 3);
}

/// Asserts that a writer that finds a lock of someone else for the whole of `lock_wait` is
/// answered with the error that tells a client to retry, and writes nothing: neither the object nor
/// the marker. Both a `LOCK` and a `CLEARING` are waited for.
#[tokio::test]
async fn a_put_that_finds_a_lock_for_lock_wait_is_unavailable_and_writes_nothing() {
    for st in [MarkerState::Lock, MarkerState::Clearing] {
        let rig = Rig::new().await;
        rig.pacer.tick();
        rig.seed_hot("k", "old").await;
        let theirs = marker("k", st.clone(), false, Some("them"));
        rig.seed_marker(&theirs).await;
        let put = rig.put("k", "new").await;
        assert!(matches!(put, Err(Error::Unavailable(_))), "{st:?}");
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
        assert_eq!(rig.marker("k").await, Some(theirs));
        assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
        let total: Duration = rig.pacer.asked().iter().sum();
        assert_eq!(total, Duration::from_secs(5));
    }
}

/// Asserts that a lock create that loses (a marker appeared after wave 1 read none) reads the
/// marker again and takes it over by a swap, and that a swap that loses (the marker changed after
/// it was read) reads again and swaps again: each time the writer decides again, and it wins.
#[tokio::test]
async fn an_overwrite_whose_lock_write_loses_reads_the_marker_again_and_decides_again() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let create = rig.marks.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&create, &mut put).await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false, None))
        .await;
    assert!(create.release());
    let swap = rig.marks.gate(Op::PutIfMatch, "k", Phase::Pre);
    until_parked(&swap, &mut put).await;
    // Another writer changed the marker after this one read it.
    let theirs = marker("k", MarkerState::Want, false, None);
    rig.marks_mem.delete("k").await.unwrap();
    rig.seed_marker(&theirs).await;
    assert!(swap.release());
    put.await.unwrap();
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    assert_eq!(rig.shape("k").await, dirty(false));
    let marks = [(Op::Get, "k"), (Op::Put, "k"), (Op::Get, "k")]
        .into_iter()
        .chain([(Op::PutIfMatch, "k"), (Op::Get, "k"), (Op::PutIfMatch, "k")])
        .chain([(Op::PutIfMatch, "k")])
        .map(|(op, key)| (op, key.to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(rig.marks_log(), marks);
}

/// Asserts that a writer whose lock write keeps losing gives up after `entry_rounds` and the
/// client is told to retry, with the object as it was.
#[tokio::test]
async fn an_overwrite_gives_up_when_the_marker_keeps_changing() {
    let rig = Rig::with(super::plain_put_tests::MARKS_FORMAT, |config| {
        config.entry_rounds = 1;
    })
    .await;
    rig.seed_hot("k", "old").await;
    let create = rig.marks.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&create, &mut put).await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false, None))
        .await;
    assert!(create.release());
    let put = put.await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
}

/// Asserts the replace of a holder whose lock was reaped (its process stalled past the lease): the
/// replace names the tag, the tag has moved, and the store refuses it. A stream cannot be sent
/// again, so the client is told to retry, and the object is the other writer's; the holder's
/// release finds the marker changed and writes nothing.
#[tokio::test]
async fn a_replace_by_a_holder_whose_lock_was_reaped_fails_on_the_tag_and_changes_nothing() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let mut put = Box::pin(rig.put_kind("k", "new", true));
    until_parked(&replace, &mut put).await;

    reap(&rig).await;
    let reaped = rig.marker("k").await;
    let old = rig.hot_tag("k").await;
    rig.hot_mem
        .put_if_match("k", body("other"), &old)
        .await
        .unwrap();

    assert!(replace.release());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("other"));
    assert_eq!(rig.marker("k").await, reaped);
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a replace refused for its tag is not retried for a stream, which is told to retry
/// itself, and is for a body held whole, which reads the tag again and replaces by it.
#[tokio::test]
async fn a_streamed_overwrite_whose_replace_gets_412_is_503_and_a_replayable_one_retries() {
    for streamed_body in [true, false] {
        let rig = Rig::new().await;
        rig.seed_hot("k", "old").await;
        rig.hot_faults
            .fail_next_with(Op::PutIfMatch, Error::PreconditionFailed("k".to_owned()));
        let put = if streamed_body {
            let body = streamed(&["new"]);
            rig.proxy.put_plain("k", body, &PutClaim::default()).await
        } else {
            rig.put("k", "new").await
        };
        settle().await;
        if streamed_body {
            assert!(matches!(put, Err(Error::Unavailable(_))));
            assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
            assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 1);
        } else {
            assert!(put.unwrap().wrote);
            assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
            assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 2);
        }
        assert_eq!(rig.shape("k").await, dirty(false));
    }
}

/// Asserts that a body held whole that keeps meeting a changed object gives up after
/// `entry_rounds` replaces, and the client is told to retry.
#[tokio::test]
async fn a_held_replace_that_keeps_meeting_changes_is_unavailable_after_entry_rounds() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    for _ in 0..3 {
        rig.hot_faults
            .fail_next_with(Op::PutIfMatch, Error::PreconditionFailed("k".to_owned()));
    }
    let put = rig.put("k", "new").await;
    settle().await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 3);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that an object deleted after wave 1 is created under the lock: for a body held whole
/// the replace is refused as not found and the object is created, and for a stream the HEAD under
/// the lock finds nothing and the object is created.
#[tokio::test]
async fn an_overwrite_of_an_object_deleted_after_wave_1_creates_it_under_the_lock() {
    for streamed_body in [false, true] {
        let rig = Rig::new().await;
        rig.seed_hot("k", "old").await;
        let lock = rig.marks.gate(Op::Put, "k", Phase::Post);
        let mut put = Box::pin(rig.put_kind("k", "new", streamed_body));
        until_parked(&lock, &mut put).await;
        rig.hot_mem.delete("k").await.unwrap();
        assert!(lock.release());
        put.await.unwrap();
        settle().await;
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
        assert_eq!(rig.shape("k").await, dirty(false));
    }
}

/// Asserts that a stream whose object vanishes between the HEAD under the lock and the replace
/// (something deleted it without the lock) is told to retry.
#[tokio::test]
async fn a_streamed_overwrite_whose_object_vanishes_before_the_replace_is_unavailable() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.hot_faults
        .fail_next_with(Op::PutIfMatch, Error::NotFound("k".to_owned()));
    let put = rig
        .proxy
        .put_plain("k", streamed(&["new"]), &PutClaim::default())
        .await;
    settle().await;
    assert!(matches!(put, Err(Error::Unavailable(_))));
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a body held whole, created over an object that appeared meanwhile (a create that
/// finds the key), reads the tag and replaces by it.
#[tokio::test]
async fn a_create_under_the_lock_that_finds_the_key_replaces_by_the_tag() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.hot_faults
        .fail_next_with(Op::PutIfMatch, Error::NotFound("k".to_owned()));
    rig.hot_faults
        .fail_next_with(Op::Put, Error::AlreadyExists("k".to_owned()));
    // The replace is told the key is gone, the create is told it is there, and the tag read after
    // each is the object's own.
    let put = rig.put("k", "new").await.unwrap();
    settle().await;
    assert!(put.wrote);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
}

/// Asserts the same-bytes put of 5.4a: a claim that names the tag of the object, and a size that is
/// its size, is answered after the one HEAD with no write: not to the hot tier, not to the marks
/// bucket (which is read in wave 1 and not written), and not to the cold tier. A stream is read to
/// its end.
#[tokio::test]
async fn a_put_of_the_bytes_already_there_is_200_after_one_head_and_writes_nothing() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "same").await;
    let tag = rig.hot_tag("k").await;
    let claim = PutClaim {
        tags: vec!["other".to_owned(), tag.clone()],
    };

    let put = rig
        .proxy
        .put_plain("k", held("same"), &claim)
        .await
        .unwrap();
    assert_eq!(
        put,
        PlainPut {
            tag: Some(tag.clone()),
            wrote: false
        }
    );
    let streamed_put = rig
        .proxy
        .put_plain("k", streamed(&["sa", "me"]), &claim)
        .await
        .unwrap();
    assert!(!streamed_put.wrote);
    settle().await;

    let heads = log(&[(Op::Head, "k"), (Op::Head, "k")]);
    assert_eq!(rig.hot_log(), heads);
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 0);
    assert!(rig.cold_faults.call_log().is_empty());
    assert_eq!(rig.marker("k").await, None);
}

/// Asserts that a claim that is not met by the object is an overwrite: a tag the object does not
/// have (the bytes differ, though the size is the same), the tag with another size, the tag with a
/// length the caller does not know, and no tag at all.
#[tokio::test]
async fn a_claim_that_does_not_name_the_tag_and_size_of_the_object_is_an_overwrite() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "same").await;
    let tag = rig.hot_tag("k").await;
    let right = PutClaim { tags: vec![tag] };
    let wrong = PutClaim {
        tags: vec!["not-the-tag".to_owned()],
    };
    let unknown = |text: &'static str| PutBody::Stream {
        len: None,
        body: chunks(&[text]),
    };
    let cases = [
        (held("same"), &wrong),
        (held("sam"), &right),
        (unknown("same"), &right),
        (held("same"), &PutClaim::default()),
    ];
    for (put_body, claim) in cases {
        let put = rig.proxy.put_plain("k", put_body, claim).await.unwrap();
        assert!(put.wrote);
    }
    settle().await;
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 4);
}

/// Asserts that the same bytes are taken only for the tag and the size together: other bytes of
/// the same size have another tag and are an overwrite, and the tag of a multipart object, which
/// is the digest of its parts' digests and a part count, is never the digest of a body.
#[tokio::test]
async fn a_put_of_other_bytes_of_the_same_size_overwrites() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "same").await;
    let other = Rig::new().await;
    other.seed_hot("k", "sane").await;
    let claim = PutClaim {
        tags: vec![other.hot_tag("k").await],
    };
    let put = rig
        .proxy
        .put_plain("k", held("sane"), &claim)
        .await
        .unwrap();
    settle().await;
    assert!(put.wrote);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("sane"));
}

/// Asserts that the tag of a multipart object (`<digest>-<parts>`) never meets a claim, whatever
/// the size.
#[test]
fn a_multipart_etag_never_takes_the_same_bytes_shortcut() {
    let digest = "d41d8cd98f00b204e9800998ecf8427e".to_owned();
    let multipart = Meta {
        size: 4,
        tag: format!("{digest}-2"),
        modified: 0,
    };
    let claim = PutClaim { tags: vec![digest] };
    assert!(!claim.is_met_by(&multipart, Some(4)));
    let single = Meta {
        tag: claim.tags[0].clone(),
        ..multipart
    };
    assert!(claim.is_met_by(&single, Some(4)));
}

/// Asserts that a body that fails while it is read to the end (a digest that did not match, a
/// connection that was cut) is the error of the put, though nothing was to be written.
#[tokio::test]
async fn a_same_bytes_put_whose_body_fails_is_an_error_and_writes_nothing() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "same").await;
    let tag = rig.hot_tag("k").await;
    let items: Vec<Result<bytes::Bytes, Error>> =
        vec![Ok(body("sa")), Err(Error::BadDigest("k".to_owned()))];
    let failing = PutBody::Stream {
        len: Some(4),
        body: futures::StreamExt::boxed(futures::stream::iter(items)),
    };
    let claim = PutClaim { tags: vec![tag] };
    let put = rig.proxy.put_plain("k", failing, &claim).await;
    assert!(matches!(put, Err(Error::BadDigest(_))));
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);
}

/// Asserts that a lock whose create was written but whose answer was lost is read back by its
/// nonce and held: the put goes on and succeeds.
#[tokio::test]
async fn an_ambiguous_lock_create_is_read_back_by_nonce_and_held() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let create = rig.marks.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&create, &mut put).await;
    assert!(create.fail_post());
    assert!(put.await.unwrap().wrote);
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a lock whose create may have landed and cannot be read back is not assumed held:
/// nothing is replaced and the client is told to retry. The lock that may stand is the leader's to
/// reap.
#[tokio::test]
async fn an_ambiguous_lock_is_not_assumed_taken() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let create = rig.marks.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&create, &mut put).await;
    rig.marks_faults.fail_next(Op::Get, 1);
    assert!(create.fail_post());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.shape("k").await, locked(false));
}

/// Asserts that a long replace keeps its lock alive: each `lock_refresh` of the clock the lock is
/// written again with a fresh nonce, by the same owner, and the release that follows names the
/// last of those writes.
#[tokio::test]
async fn a_long_replace_refreshes_the_lock_every_lock_refresh() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.pacer.tick();
    rig.pacer.stop_after(3);
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&replace, &mut put).await;
    let first = rig.marker("k").await.unwrap();
    futures::future::select(Box::pin(rig.pacer.reached()), &mut put).await;

    let now = rig.marker("k").await.unwrap();
    assert_eq!(now.st, MarkerState::Lock);
    assert_eq!(now.owner.as_deref(), Some("p1"));
    assert_ne!(now.nonce, first.nonce);
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 3);
    let every = Duration::from_secs(30);
    assert_eq!(rig.pacer.asked(), vec![every; 4]);

    assert!(replace.release());
    put.await.unwrap();
    settle().await;
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a holder whose refresh finds the lock changed (the leader reaped it) goes on with
/// the call it has under way, which is safe because it names a tag, writes nothing to the marker
/// after that and does not release the lock it no longer holds.
#[tokio::test]
async fn a_lock_found_changed_by_a_refresh_is_not_released() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let put = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("new"), &PutClaim::default())
                .await
        }
    });
    replace.parked().await;
    reap(&rig).await;
    let reaped = rig.marker("k").await;
    rig.pacer.tick();
    let after = rig.pacer.asked().len() + 3;
    rig.pacer.slept(after).await;
    assert!(replace.release());
    assert!(put.await.unwrap().unwrap().wrote);
    settle().await;
    assert_eq!(rig.marker("k").await, reaped);
    // The lock write, the refresh that lost, and nothing after.
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 1);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
}

/// Asserts that a refresh the store refuses keeps the lock as it was written, and that the release
/// still names it.
#[tokio::test]
async fn a_refresh_the_store_refuses_keeps_the_lock_as_it_was() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    rig.marks_faults
        .fail_next_on(Op::PutIfMatch, "k", Error::Full("k".to_owned()));
    let put = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("new"), &PutClaim::default())
                .await
        }
    });
    replace.parked().await;
    rig.pacer.tick();
    let after = rig.pacer.asked().len() + 2;
    rig.pacer.slept(after).await;
    assert!(replace.release());
    assert!(put.await.unwrap().unwrap().wrote);
    settle().await;
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a holder whose lock is too old to destroy under (the last write of it is past
/// `lock_lease - queued_call_margin` by its clock) replaces nothing and releases the lock.
#[tokio::test]
async fn a_lock_too_old_to_destroy_under_is_released_and_nothing_is_replaced() {
    for streamed_body in [false, true] {
        let rig = Rig::new().await;
        rig.seed_hot("k", "old").await;
        let lock = rig.marks.gate(Op::Put, "k", Phase::Post);
        let mut put = Box::pin(rig.put_kind("k", "new", streamed_body));
        until_parked(&lock, &mut put).await;
        rig.mono.advance(Duration::from_secs(61));
        assert!(lock.release());
        assert!(matches!(put.await, Err(Error::Unavailable(_))));
        settle().await;
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
        assert_eq!(rig.hot_faults.calls(Op::PutIfMatch), 0);
        assert_eq!(rig.shape("k").await, dirty(false));
    }
}

/// Asserts that a client that goes away after the lock was taken does not leak it: the future of
/// the put is dropped while the replace is under way, the guard releases the lock to `DIRTY`, and
/// the object is as it was.
#[tokio::test]
async fn a_client_that_disconnects_after_the_lock_was_taken_does_not_leak_it() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let put = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("new"), &PutClaim::default())
                .await
        }
    });
    replace.parked().await;
    assert_eq!(rig.shape("k").await, locked(false));
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());
    settle().await;
    assert_eq!(rig.shape("k").await, dirty(false));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
}

/// Asserts the crash of 7.2: the proxy dies after the overwrite lock and before the replace. The
/// leader reaps the lock to `DIRTY`, and the old version stays the truth.
#[tokio::test]
async fn a_crash_after_the_overwrite_lock_before_the_replace_keeps_the_old_version() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&replace, &mut put).await;
    std::mem::forget(put);
    assert_eq!(rig.shape("k").await, locked(false));
    reap(&rig).await;
    assert_eq!(rig.shape("k").await, dirty(false));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
}

/// Asserts the crash of 7.2 after the replace and before the answer: the put is unacknowledged but
/// it landed; reaped to `DIRTY`, the new version is the truth.
#[tokio::test]
async fn a_crash_after_the_replace_before_the_ack_keeps_the_new_version() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Post);
    let mut put = Box::pin(rig.put("k", "new"));
    until_parked(&replace, &mut put).await;
    std::mem::forget(put);
    reap(&rig).await;
    assert_eq!(rig.shape("k").await, dirty(false));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
}

/// Asserts the crash of 7.2 after the answer and before the release (the release is made to fail
/// as far as it is tried): other writers wait `lock_wait` and are told to retry, the leader reaps
/// the lock to `DIRTY`, and then a writer is served.
#[tokio::test]
async fn a_crash_after_the_overwrite_ack_is_reaped_to_dirty_and_others_get_503_meanwhile() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 4);
    assert!(rig.put("k", "new").await.unwrap().wrote);
    settle().await;
    assert_eq!(rig.shape("k").await, locked(false));

    rig.pacer.tick();
    let blocked = rig.put("k", "newer").await;
    assert!(matches!(blocked, Err(Error::Unavailable(_))));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));

    reap(&rig).await;
    assert!(rig.put("k", "newer").await.unwrap().wrote);
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("newer"));
    assert_eq!(rig.shape("k").await, dirty(false));
}

/// Asserts that a stream whose HEAD under the lock fails is told to retry, replaces nothing and
/// releases the lock, and that a body held whole whose replace the store fails is told to retry
/// and releases the lock.
#[tokio::test]
async fn a_call_under_the_lock_that_fails_is_unavailable_and_releases_the_lock() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let lock = rig.marks.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.put_kind("k", "new", true));
    until_parked(&lock, &mut put).await;
    rig.hot_faults.fail_next(Op::Head, 1);
    assert!(lock.release());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.shape("k").await, dirty(false));

    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.hot_faults.fail_next(Op::PutIfMatch, 1);
    assert!(matches!(
        rig.put("k", "new").await,
        Err(Error::Unavailable(_))
    ));
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.shape("k").await, dirty(false));

    // A held body whose HEAD after a refused replace fails.
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let lock = rig.marks.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.put_kind("k", "new", false));
    until_parked(&lock, &mut put).await;
    rig.hot_faults.fail_next(Op::Head, 1);
    let refused = Error::PreconditionFailed("k".to_owned());
    rig.hot_faults.fail_next_with(Op::PutIfMatch, refused);
    assert!(lock.release());
    assert!(matches!(put.await, Err(Error::Unavailable(_))));
    settle().await;
    assert_eq!(rig.shape("k").await, dirty(false));
}
