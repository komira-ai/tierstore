//! The proxy over a cold tier: reads and heads of the cold tier and what they count, listings of
//! both tiers, deletes, and the journal entry of a copy-back of a cold object, which is a mirror
//! of it.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, ByteRange, Error, JournalOp, MemoryBackend, Op, Proxy};
use tokio::sync::Notify;

use crate::churn::{Change, Churn};
use crate::hold::Stall;
use crate::support::{
    Rig, assert_absent, body, journal, journal_keys, keys, page, read, set_usage, text,
};

/// The line of a metric of the proxy, as `Proxy::metrics` renders it, with its value.
fn metric(proxy: &Proxy, line: &str) -> String {
    let rendered = proxy.metrics();
    let found = rendered.lines().find(|l| l.starts_with(line));
    found.unwrap_or("").to_owned()
}

fn not_found(key: &str) -> Error {
    Error::NotFound(key.to_owned())
}

/// Asserts that a range past the end of an object, asked of the cold tier above the critical mark,
/// stays the error it is, counted as an error of the cold tier, and that the proxy makes no HEAD
/// of the cold tier for it.
#[tokio::test(start_paused = true)]
async fn a_range_past_the_end_of_an_object_is_an_error_without_a_head() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("0123456789")).await.unwrap();
    set_usage(&rig, 960, 0).await;
    let past = Some(ByteRange::new(11, 1));

    let error = proxy.get("k", past).await;
    assert!(matches!(error, Err(Error::Other(_))), "{error:?}");
    assert_eq!(rig.cold.calls(Op::Head), 0);
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, errors), format!("{errors} 1"));
}

/// Asserts that a head that reaches the cold tier is counted as a hit of it when the object is
/// there, as a miss when it is not, and as an error when the call fails.
#[tokio::test(start_paused = true)]
async fn a_head_of_the_cold_tier_is_counted_as_a_hit_a_miss_or_an_error() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();

    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_eq!(proxy.head("absent").await, Err(not_found("absent")));
    rig.cold.fail_next(Op::Head, 1);
    assert!(proxy.head("k").await.unwrap_err().is_unavailable());
    for (line, count) in [
        ("tierstore_hits_total{tier=\"cold\",op=\"head\"}", 1),
        ("tierstore_misses_total{tier=\"cold\",op=\"head\"}", 1),
        ("tierstore_read_errors_total{tier=\"cold\",op=\"head\"}", 1),
    ] {
        assert_eq!(metric(&proxy, line), format!("{line} {count}"));
    }
}

/// Asserts that a listing shows the objects of both tiers once each, in key order, and pages
/// through them: a page is cut at `max_keys`, the next starts after the last key given, the hot
/// tier's entry is shown for a key both tiers hold, and a short page is the last.
#[tokio::test(start_paused = true)]
async fn a_listing_shows_the_objects_of_both_tiers_and_pages_through_them() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "c", "e", "g"] {
        rig.cold_mem.put(key, body("v")).await.unwrap();
    }
    rig.cold_mem.put("d", body("cold")).await.unwrap();
    rig.hot_mem.put("d", body("hot")).await.unwrap();

    assert_eq!(keys(&proxy).await, ["a", "c", "d", "e", "g"]);
    assert_eq!(page(&proxy, None, 2).await, ["a", "c"]);
    assert_eq!(page(&proxy, Some("c"), 2).await, ["d", "e"]);
    assert_eq!(page(&proxy, Some("e"), 2).await, ["g"]);
    assert_eq!(page(&proxy, Some("g"), 2).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 0).await, Vec::<String>::new());
    let listed = proxy.list("d").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, 3);
}

/// Asserts that a read error of the cold tier that is not "not found" is an error that is
/// counted, whether it is "unavailable" or another error, and a key the cold tier does not hold is
/// a miss.
#[tokio::test(start_paused = true)]
async fn a_failed_read_of_the_cold_tier_is_an_error_and_an_absent_key_is_a_miss() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.cold.fail_next(Op::Get, 1);
    assert!(proxy.get("k", None).await.unwrap_err().is_unavailable());
    rig.cold
        .fail_next_with(Op::Get, Error::Other("odd".to_owned()));
    let odd = proxy.get("k", None).await;
    assert_eq!(odd, Err(Error::Other("odd".to_owned())));
    assert_eq!(proxy.get("absent", None).await, Err(not_found("absent")));
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, errors), format!("{errors} 2"));
    let miss = "tierstore_misses_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, miss), format!("{miss} 1"));
}

/// Asserts that a copy-back is journaled as a mirror that names the content tag of the cold
/// object, which the read of the cold tier gives (no HEAD is made for it).
#[tokio::test(start_paused = true)]
async fn a_copy_back_is_journaled_as_a_mirror_of_the_cold_object() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let tag = rig.cold_mem.head("k").await.unwrap().tag;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].op, JournalOp::PutMirror);
    assert_eq!(entries[0].cold_tag.as_deref(), Some(tag.as_str()));
}

/// Asserts that a copy-back names the tag of the bytes that were read, when the cold object is
/// removed or replaced by another object right after the read: the read is answered, the bytes are
/// put in the hot tier, and the mirror carries the tag of the object read (which the cold tier no
/// longer has, so the reconciler drops the copy).
#[tokio::test(start_paused = true)]
async fn a_copy_back_names_the_tag_of_the_bytes_read_when_the_key_changes_after_the_read() {
    let changes = [Change::Remove, Change::Object(body("other"))];
    for (n, change) in changes.into_iter().enumerate() {
        let rig = Rig::new();
        rig.cold_mem.put("k", body("value")).await.unwrap();
        let read_tag = rig.cold_mem.head("k").await.unwrap().tag;
        let cold = Churn::after_a_read(rig.cold_mem.clone(), change);
        let id = format!("p{n}");
        let proxy = Proxy::open(rig.hot.clone(), Arc::new(cold), rig.config(&id, 1000))
            .await
            .unwrap();

        assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
        let entries = journal(&*rig.hot_mem, &id, 1).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].op, JournalOp::PutMirror);
        assert_eq!(entries[0].cold_tag.as_deref(), Some(read_tag.as_str()));
    }
}

/// Asserts that a copy-back names the tag of the object it read when the key holds A, then B (the
/// object read), then A again: the cold tier's one read gives B's bytes with B's tag, so the
/// mirror names B's tag and not A's, and no HEAD before or after the read can be taken for it.
/// With A's tag the reconciler would keep B's bytes in the hot tier as the copy of A.
#[tokio::test(start_paused = true)]
async fn a_copy_back_names_the_tag_of_the_object_read_when_the_key_goes_from_a_to_b_and_back() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("a-bytes")).await.unwrap();
    let tag_a = rig.cold_mem.head("k").await.unwrap().tag;
    let scratch = MemoryBackend::new();
    scratch.put("k", body("b-bytes")).await.unwrap();
    let tag_b = scratch.head("k").await.unwrap().tag;
    assert_ne!(tag_a, tag_b);
    let cold = Churn::around_a_tagged_read(
        rig.cold_mem.clone(),
        Change::Object(body("b-bytes")),
        Change::Object(body("a-bytes")),
    );
    let proxy = Proxy::open(rig.hot.clone(), Arc::new(cold), rig.config("p1", 1000))
        .await
        .unwrap();

    assert_eq!(proxy.get("k", None).await.unwrap(), body("b-bytes"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("a-bytes"));
    assert_eq!(read(&*rig.hot_mem, "k").await, body("b-bytes"));
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].op, JournalOp::PutMirror);
    assert_eq!(entries[0].cold_tag.as_deref(), Some(tag_b.as_str()));
}

/// Asserts that a delete takes a real cold object by its tag, then the hot object, and journals
/// the delete.
#[tokio::test(start_paused = true)]
async fn a_delete_removes_a_real_cold_object_by_its_tag() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.hot_mem.put("k", body("value")).await.unwrap();

    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 1);
    assert_eq!(rig.cold.calls(Op::Delete), 0);
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 1);
}

/// Asserts that a delete of a key the cold tier lacks clears what the hot tier holds under it
/// (an orphan, or a stale copy-back) and journals the delete.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_key_absent_from_cold_clears_the_hot_orphan() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("k", body("stale")).await.unwrap();

    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 0);
}

/// Asserts that a delete whose cold object changes between the look and the delete looks again
/// and deletes the object it finds, and gives up after three changes with an error that a retry
/// of the delete (which then sees the object, unchanged) finishes.
#[tokio::test(start_paused = true)]
async fn a_delete_retries_when_the_cold_object_changes_under_it_and_gives_up_after_three() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("one")).await.unwrap();
    let changes = vec![Change::Object(body("two"))];
    let cold = Arc::new(Churn::new(rig.cold_mem.clone(), changes));
    let config = rig.config("p1", 1000);
    let proxy = Proxy::open(rig.hot.clone(), cold, config).await.unwrap();
    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.cold_mem, "k").await;

    rig.cold_mem.put("j", body("one")).await.unwrap();
    let changes = vec![
        Change::Object(body("two")),
        Change::Object(body("three")),
        Change::Object(body("four")),
    ];
    let cold = Arc::new(Churn::new(rig.cold_mem.clone(), changes));
    let config = rig.config("p2", 1000);
    let proxy = Proxy::open(rig.hot.clone(), cold, config).await.unwrap();
    let error = proxy.delete("j").await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert_eq!(text(&*rig.cold_mem, "j").await, "four");
    proxy.delete("j").await.unwrap();
    assert_absent(&*rig.cold_mem, "j").await;
}

/// Asserts that a delete of an object that vanishes between the look and the delete is done.
#[tokio::test(start_paused = true)]
async fn a_delete_of_an_object_that_vanishes_under_it_is_done() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("one")).await.unwrap();
    let cold = Arc::new(Churn::new(rig.cold_mem.clone(), vec![Change::Remove]));
    let proxy = Proxy::open(rig.hot.clone(), cold, rig.config("p1", 1000))
        .await
        .unwrap();
    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 1);
}

/// Asserts that each call of a delete that fails fails the delete, before the calls after it:
/// the look at the cold tier, the conditional delete of the cold tier, and the delete of the hot
/// tier; and that the hot tier is not deleted from when the cold tier was not.
#[tokio::test(start_paused = true)]
async fn each_failing_call_of_a_delete_fails_it_before_the_calls_after_it() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("one")).await.unwrap();
    rig.hot_mem.put("k", body("one")).await.unwrap();

    rig.cold.fail_next(Op::Head, 1);
    assert!(proxy.delete("k").await.unwrap_err().is_unavailable());
    rig.cold.fail_next(Op::DeleteIfMatch, 1);
    assert!(proxy.delete("k").await.unwrap_err().is_unavailable());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("one"));
    assert_eq!(read(&*rig.cold_mem, "k").await, body("one"));

    rig.hot.fail_next(Op::Delete, 1);
    assert!(proxy.delete("k").await.unwrap_err().is_unavailable());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("one"));
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a key deleted while a get of it is copying it back from the cold tier into the
/// hot tier stays deleted: once the copy-back has written the key to the hot tier and journaled
/// it, and the reconciler has run, a get of the key is NotFound and neither tier holds it.
///
/// Catches a copy-back that brings a deleted object back: its put entry is journaled after the
/// delete entry, so the reconciler copies the stale hot object to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_delete_during_a_copy_back_keeps_the_key_deleted() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stalled_hot = Arc::new(Stall::new(
        rig.hot.clone(),
        "k",
        entered.clone(),
        Some(release.clone()),
    ));
    let proxy = Arc::new(
        Proxy::open(stalled_hot, rig.cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap(),
    );

    // The get misses hot, reads cold, and stops at the copy-back's hot put.
    let getter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.get("k", None).await })
    };
    entered.notified().await;

    // The delete completes in full: both tiers, and its journal entry.
    proxy.delete("k").await.unwrap();
    rig.clock.advance(10);
    assert_absent(&*rig.cold_mem, "k").await;

    // The copy-back goes on: the hot tier takes the object, and the put is journaled.
    release.notify_one();
    let served = getter.await.unwrap();
    assert_eq!(served.unwrap(), body("value"));
    // The journal write of the copy-back is a background task that waits a linger.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 2);

    let mut reconciler = rig.reconciler("r1", 1000);
    let first = reconciler.pass().await;
    assert_eq!(first.failed, Vec::<Error>::new());

    assert_eq!(
        proxy.get("k", None).await,
        Err(Error::NotFound("k".to_owned()))
    );
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
}
