//! The proxy over a cold tier that holds reservations (marked 56-byte objects, made here by hand
//! since nothing in this release writes one): reads, heads and listings do not show them, a
//! delete treats them by what the put they belong to has done, and a copy-back of a cold object
//! is journaled as a mirror of it.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, ByteRange, Error, JournalOp, MemoryBackend, Op, Proxy};
use tokio::sync::Notify;

use crate::churn::{Change, Churn, Ghost};
use crate::conformance::reservation;
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

/// Asserts that a get of a reservation is not found, copies nothing into the hot tier and
/// journals nothing, and is counted as a miss of the cold tier.
#[tokio::test(start_paused = true)]
async fn a_get_of_a_reservation_is_not_found_and_copies_nothing_back() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();

    assert_eq!(proxy.get("k", None).await, Err(not_found("k")));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );
    let miss = "tierstore_misses_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, miss), format!("{miss} 1"));
}

/// Asserts that a ranged get of a reservation is not found, at the normal level (the whole
/// object is read from the cold tier and the range cut from it) and above the critical mark (the
/// range alone is asked of the cold tier, which answers a range past the end of the 56 bytes
/// with an error that a HEAD tells from a failure), for a range inside it, at its end and past
/// its end.
#[tokio::test(start_paused = true)]
async fn a_ranged_get_of_a_reservation_is_not_found() {
    for critical in [false, true] {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        rig.cold_mem
            .put_reservation("k", reservation(1))
            .await
            .unwrap();
        if critical {
            set_usage(&rig, 960, 0).await;
        }
        for (offset, len) in [(4, 10), (56, 1), (57, 1), (100, 5)] {
            let range = Some(ByteRange::new(offset, len));
            let read = proxy.get("k", range).await;
            assert_eq!(read, Err(not_found("k")), "{critical} {offset}+{len}");
        }
        assert_eq!(
            journal_keys(&*rig.hot_mem, "p1").await,
            Vec::<String>::new()
        );
    }
}

/// Asserts that a range past the end of a real object, asked of the cold tier above the critical
/// mark, stays the error it is, though the proxy asks the cold tier about the key to tell it from a
/// reservation, and that the same error stays when that question cannot be answered.
#[tokio::test(start_paused = true)]
async fn a_range_past_the_end_of_a_real_object_is_still_an_error() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("0123456789")).await.unwrap();
    set_usage(&rig, 960, 0).await;
    let past = Some(ByteRange::new(11, 1));

    let error = proxy.get("k", past).await;
    assert!(matches!(error, Err(Error::Other(_))), "{error:?}");
    assert_eq!(rig.cold.calls(Op::Head), 1);

    rig.cold.fail_next(Op::Head, 1);
    let error = proxy.get("k", past).await;
    assert!(matches!(error, Err(Error::Other(_))), "{error:?}");
    assert_eq!(rig.cold.calls(Op::Head), 2);
}

/// Asserts that an object that is 56 bytes and looks like a reservation, but was written
/// plainly, is read as an object: whole, by range, and by a head, and is copied back.
#[tokio::test(start_paused = true)]
async fn a_real_56_byte_object_that_looks_like_a_reservation_is_read_as_an_object() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", reservation(1)).await.unwrap();

    assert_eq!(proxy.head("k").await.unwrap().size, 56);
    let part = proxy.get("k", Some(ByteRange::new(0, 4))).await.unwrap();
    assert_eq!(part, reservation(1).slice(0..4));
    assert_eq!(proxy.get("k", None).await.unwrap(), reservation(1));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(read(&*rig.hot_mem, "k").await, reservation(1));
}

/// Asserts that a head of a reservation is not found and counted as a miss of the cold tier,
/// and not as a hit.
#[tokio::test(start_paused = true)]
async fn a_head_of_a_reservation_is_not_found_and_counts_a_cold_miss() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();

    assert_eq!(proxy.head("k").await, Err(not_found("k")));
    let miss = "tierstore_misses_total{tier=\"cold\",op=\"head\"}";
    let hit = "tierstore_hits_total{tier=\"cold\",op=\"head\"}";
    assert_eq!(metric(&proxy, miss), format!("{miss} 1"));
    assert_eq!(metric(&proxy, hit), format!("{hit} 0"));
}

/// Asserts that an object the hot tier holds is read though the cold tier holds a reservation of
/// the key (the put is acknowledged and not copied yet), by get and by head, without a request to
/// the cold tier.
#[tokio::test(start_paused = true)]
async fn a_hot_object_is_read_even_if_cold_holds_a_reservation() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("k", body("value")).await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_eq!(rig.cold.calls(Op::Get), 0);
    assert_eq!(rig.cold.calls(Op::Head), 0);
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
/// removed or replaced by a reservation right after the read: the read is answered, the bytes are
/// put in the hot tier, and the mirror carries the tag of the object read (which the cold tier no
/// longer has, so the reconciler drops the copy).
#[tokio::test(start_paused = true)]
async fn a_copy_back_names_the_tag_of_the_bytes_read_when_the_key_changes_after_the_read() {
    let changes = [Change::Remove, Change::Reservation(reservation(1))];
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
    let cold = Churn::around_a_marked_read(
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

/// Asserts that a listing does not show a reservation that replaced a real object of the same
/// size within the same time of last write (the clock of the rig does not move): the listing
/// before showed the real object, and the listing after asks the cold tier again.
#[tokio::test(start_paused = true)]
async fn a_reservation_that_replaces_a_real_object_at_the_same_time_is_not_listed() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", reservation(1)).await.unwrap();
    assert_eq!(keys(&proxy).await, ["k"]);

    rig.cold_mem.delete("k").await.unwrap();
    let held = rig.cold_mem.put_reservation("k", reservation(2));
    held.await.unwrap();
    assert_eq!(keys(&proxy).await, Vec::<String>::new());
    let page = proxy.list_page("", None, 10).await.unwrap();
    assert_eq!(page.len(), 0, "by page too");
}

/// Asserts that a listing leaves out reservations of the cold tier, whole and by page, shows the
/// objects around them, and shows a key the hot tier holds though the cold tier holds a
/// reservation of it.
#[tokio::test(start_paused = true)]
async fn a_listing_leaves_out_reservations_in_cold() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("a", body("one")).await.unwrap();
    rig.cold_mem
        .put_reservation("b", reservation(1))
        .await
        .unwrap();
    rig.cold_mem.put("c", reservation(2)).await.unwrap();
    rig.cold_mem
        .put_reservation("d", reservation(3))
        .await
        .unwrap();
    rig.hot_mem.put("d", body("hot")).await.unwrap();
    rig.cold_mem
        .put_reservation("e", reservation(4))
        .await
        .unwrap();

    assert_eq!(keys(&proxy).await, ["a", "c", "d"]);
    assert_eq!(page(&proxy, None, 10).await, ["a", "c", "d"]);
    let listed = proxy.list("d").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, 3);
}

/// Asserts that a page of the listing is full when reservations were left out of the cold
/// tier's page, that the next page starts after the last key shown, and that a short page is the
/// last.
#[tokio::test(start_paused = true)]
async fn a_page_of_the_listing_is_full_when_reservations_were_filtered() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for (n, key) in ["a", "c", "e", "g"].into_iter().enumerate() {
        rig.cold_mem.put(key, body("v")).await.unwrap();
        let ghost = format!("{key}1");
        let hold = reservation(n as u32);
        rig.cold_mem.put_reservation(&ghost, hold).await.unwrap();
    }
    assert_eq!(page(&proxy, None, 2).await, ["a", "c"]);
    assert_eq!(page(&proxy, Some("c"), 2).await, ["e", "g"]);
    assert_eq!(page(&proxy, Some("g"), 2).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 0).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 3).await, ["a", "c", "e"]);
}

/// Asserts that a listing fails if the HEAD of a candidate (a cold object of the size of a
/// reservation that the hot listing does not show) fails, and that a candidate that is gone by
/// the time it is asked about is left out.
#[tokio::test(start_paused = true)]
async fn a_listing_fails_if_the_head_of_a_candidate_fails_and_leaves_out_a_gone_one() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("a", body("one")).await.unwrap();
    rig.cold_mem
        .put_reservation("b", reservation(1))
        .await
        .unwrap();
    rig.cold.fail_next(Op::Head, 1);
    assert!(proxy.list("").await.unwrap_err().is_unavailable());
    let failed = proxy.list_page("", None, 10).await;
    assert!(failed.is_ok());

    let ghost = Arc::new(Ghost::new(rig.cold_mem.clone(), "b0"));
    let hot = rig.hot.clone();
    let over_ghost = Proxy::open(hot, ghost, rig.config("p2", 1000))
        .await
        .unwrap();
    assert_eq!(keys(&over_ghost).await, ["a"]);
}

/// Asserts that a listing asks about an object of the size of a reservation at every listing, and
/// remembers no answer: a listing gives no version of the object, so the second listing makes its
/// HEAD again (the memory this test replaces was by key, size and time of last write).
#[tokio::test(start_paused = true)]
async fn a_listing_asks_again_about_an_object_of_the_size_of_a_reservation() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("a", reservation(1)).await.unwrap();
    rig.cold_mem
        .put_reservation("b", reservation(2))
        .await
        .unwrap();

    assert_eq!(keys(&proxy).await, ["a"]);
    assert_eq!(rig.cold.calls(Op::Head), 2);
    assert_eq!(keys(&proxy).await, ["a"]);
    assert_eq!(rig.cold.calls(Op::Head), 4);
}

/// Asserts that listings of more real objects of the size of a reservation than the proxy once
/// remembered list them all, every time, with one HEAD for each at each listing.
#[tokio::test(start_paused = true)]
async fn a_listing_of_many_real_objects_the_size_of_a_reservation_asks_about_each_every_time() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let count = 4100;
    for n in 0..count {
        rig.cold_mem
            .put(&format!("k{n:05}"), reservation(1))
            .await
            .unwrap();
    }
    assert_eq!(keys(&proxy).await.len(), count);
    assert_eq!(rig.cold.calls(Op::Head), count as u64);
    assert_eq!(keys(&proxy).await.len(), count);
    assert_eq!(rig.cold.calls(Op::Head), 2 * count as u64);
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

/// Asserts that a delete takes a reservation when the hot tier holds the key (the put is
/// acknowledged, or about to be), by its tag.
#[tokio::test(start_paused = true)]
async fn a_delete_removes_a_reservation_whose_hot_object_exists() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.hot_mem.put("k", body("value")).await.unwrap();

    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a delete leaves a bare reservation (the hot tier does not hold the key: the put
/// has not written its object) alone, and does not fail: the put it belongs to goes on, and is
/// acknowledged after the delete, which found no object.
#[tokio::test(start_paused = true)]
async fn a_delete_leaves_a_bare_reservation_alone() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let tag = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();

    proxy.delete("k").await.unwrap();
    let head = rig.cold_mem.head("k").await.unwrap();
    assert_eq!((head.reservation, head.tag), (true, tag));
    assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 0);
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
/// the look at the cold tier, the question of the hot tier for a reservation, the conditional delete
/// of the cold tier, and the delete of the hot tier; and that the hot tier is not deleted from when
/// the cold tier was not.
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

    rig.cold_mem.delete("k").await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.hot.fail_next(Op::Head, 1);
    assert!(proxy.delete("k").await.unwrap_err().is_unavailable());
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("one"));
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
