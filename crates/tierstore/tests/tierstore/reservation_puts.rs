//! The writers of a reservation: a put reserves its key in the cold tier with one create-only
//! write before it writes anything else, writes the object and its journal entry, and releases
//! the reservation (and removes its own object) if it does not get to its end.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, Error, JournalOp, Op, Proxy};
use tokio::sync::Notify;

use crate::conformance::reservation;
use crate::hold::Stall;
use crate::support::{
    Rig, assert_absent, assert_reserved, body, chunks, journal, journal_keys, keys, read, set_usage,
};

/// The three ways a node takes a put: a held body, a streamed body, and a streamed body that was
/// presigned (it is written to the hot tier or not at all).
const PATHS: [&str; 3] = ["put", "stream", "hot_only"];

/// Puts `key` with the body `new` by one of the three ways.
async fn put_by(proxy: &Proxy, path: &str, key: &str) -> Result<(), Error> {
    match path {
        "put" => proxy.put(key, body("new")).await,
        "stream" => proxy.put_stream(key, None, chunks(&["ne", "w"])).await,
        _ => {
            let new = chunks(&["ne", "w"]);
            proxy.put_stream_hot_only(key, None, new).await
        }
    }
}

/// Asserts that a put reserves its key with one create-only write to the cold tier (and asks
/// the cold tier nothing else: no HEAD before the create, none after it, no plain put), creates
/// the object in the hot tier, and journals a reserved put whose entry names the content tag of
/// that reservation, for a held, a streamed and a presigned put.
#[tokio::test(start_paused = true)]
async fn a_put_reserves_its_key_with_one_cold_write_then_creates_the_object_and_journals_it() {
    for path in PATHS {
        let rig = Rig::new();
        rig.clock.set(42);
        let proxy = rig.proxy("p1", 1000).await;

        put_by(&proxy, path, "k").await.unwrap();

        assert_eq!(rig.cold.calls(Op::PutReservation), 1, "{path}");
        for op in [Op::Head, Op::Get, Op::Put, Op::PutIfMatch, Op::Delete] {
            assert_eq!(rig.cold.calls(op), 0, "{path} {op:?}");
        }
        assert_eq!(read(&*rig.hot_mem, "k").await, body("new"), "{path}");
        assert_reserved(&*rig.cold_mem, "k").await;
        let tag = rig.cold_mem.head("k").await.unwrap().tag;
        let entries = journal(&*rig.hot_mem, "p1", 1).await;
        assert_eq!(entries.len(), 1, "{path}");
        assert_eq!(entries[0].op, JournalOp::PutReserved, "{path}");
        assert_eq!(entries[0].size, 3, "{path}");
        assert_eq!(entries[0].time, 42, "{path}");
        assert_eq!(entries[0].cold_tag, Some(tag), "{path}");
    }
}

/// Asserts that the body of a reservation is 56 bytes that start with the magic of one, that no
/// two reservations have the same bytes (so no two have the same content tag), and that two
/// reservations of one proxy end alike and those of two proxies do not: the end names the owner.
#[tokio::test(start_paused = true)]
async fn a_reservation_names_its_owner_and_differs_from_every_other() {
    let rig = Rig::new();
    let (one, two) = (rig.proxy("p1", 1000).await, rig.proxy("p2", 1000).await);
    one.put("a", body("1")).await.unwrap();
    one.put("b", body("2")).await.unwrap();
    two.put("c", body("3")).await.unwrap();

    let held = |key: &'static str| {
        let cold = rig.cold_mem.clone();
        async move { cold.get(key, None).await.unwrap() }
    };
    let (a, b, c) = (held("a").await, held("b").await, held("c").await);
    for reserved in [&a, &b, &c] {
        assert_eq!(reserved.len(), 56);
        assert!(reserved.starts_with(b"tierstore-reservation/1\n"));
    }
    assert_ne!(a[24..40], b[24..40]);
    assert_ne!(a[24..40], c[24..40]);
    assert_eq!(a[40..], b[40..]);
    assert_ne!(a[40..], c[40..]);
}

/// Asserts that a put of a key a reservation holds (the reservation of a put that was killed
/// before it did anything else) is refused with `AlreadyExists` by each way of putting, without
/// a chunk of a streamed body being asked for, and writes nothing to the hot tier or the
/// journal; that the reservation is left as it was; that the key reads as not found and is not
/// listed; that a delete of it (the key has no object) changes nothing; and that the key is
/// refused after the delete too: nothing takes a reservation over yet.
#[tokio::test(start_paused = true)]
async fn a_reservation_left_by_a_put_killed_after_it_reserved_refuses_the_key_and_stays() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let planted = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let hot_puts = rig.hot.calls(Op::Put);

    for path in PATHS {
        let refused = put_by(&proxy, path, "k").await;
        assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())), "{path}");
    }
    assert_eq!(rig.hot.calls(Op::Put), hot_puts);
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );
    assert_eq!(rig.cold_mem.head("k").await.unwrap().tag, planted);

    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await, Err(missing.clone()));
    assert_eq!(proxy.head("k").await.unwrap_err(), missing);
    assert_eq!(keys(&proxy).await, Vec::<String>::new());
    proxy.delete("k").await.unwrap();
    assert_eq!(rig.cold_mem.head("k").await.unwrap().tag, planted);
    let again = proxy.put("k", body("new")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
}

/// Asserts that a put whose key the hot tier holds as an object that no reservation and no copy
/// in the cold tier accounts for (a stale object) is refused with `AlreadyExists`, releases its
/// reservation (the cold tier holds nothing), leaves the hot object as it was, and journals
/// nothing, for each way of putting.
#[tokio::test(start_paused = true)]
async fn a_put_whose_hot_create_finds_an_object_releases_its_reservation_and_is_refused() {
    for path in PATHS {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        rig.hot_mem.put("k", body("stale")).await.unwrap();

        let refused = put_by(&proxy, path, "k").await;

        assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())), "{path}");
        assert_absent(&*rig.cold_mem, "k").await;
        assert_eq!(read(&*rig.hot_mem, "k").await, body("stale"), "{path}");
        let journals = journal_keys(&*rig.hot_mem, "p1").await;
        assert_eq!(journals, Vec::<String>::new(), "{path}");
    }
}

/// Asserts that a put whose journal object cannot be written fails with that error, removes the
/// object it created in the hot tier by its content tag (a conditional delete, never a plain
/// one) and releases its reservation by its tag, so neither tier holds the key, for a held and
/// a streamed put.
#[tokio::test(start_paused = true)]
async fn a_put_whose_journal_fails_removes_its_object_by_tag_and_releases_its_reservation() {
    for path in ["put", "stream"] {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        let down = Error::Unavailable("journal".to_owned());
        rig.hot.fail_next_on(Op::Put, "_journal/", down.clone());

        let failed = put_by(&proxy, path, "k").await;

        assert_eq!(failed, Err(down), "{path}");
        assert_absent(&*rig.hot_mem, "k").await;
        assert_absent(&*rig.cold_mem, "k").await;
        assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 1, "{path}");
        assert_eq!(rig.hot.calls(Op::Delete), 0, "{path}");
        assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 1, "{path}");
        assert_eq!(rig.cold.calls(Op::Delete), 0, "{path}");

        put_by(&proxy, path, "k").await.unwrap();
        assert_eq!(read(&*rig.hot_mem, "k").await, body("new"), "{path}");
    }
}

/// Asserts that a put whose reservation the cold tier cannot take (it is down) fails with that
/// error and writes nothing: no object in the hot tier, no journal entry, no reservation, for
/// each way of putting; and that the same put succeeds once the cold tier answers.
#[tokio::test(start_paused = true)]
async fn a_put_whose_reservation_fails_writes_nothing() {
    for path in PATHS {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        rig.cold.fail_next(Op::PutReservation, 1);

        let failed = put_by(&proxy, path, "k").await.unwrap_err();

        assert!(failed.is_unavailable(), "{path}: {failed}");
        assert_absent(&*rig.hot_mem, "k").await;
        assert_absent(&*rig.cold_mem, "k").await;
        let journals = journal_keys(&*rig.hot_mem, "p1").await;
        assert_eq!(journals, Vec::<String>::new(), "{path}");

        put_by(&proxy, path, "k").await.unwrap();
        assert_reserved(&*rig.cold_mem, "k").await;
    }
}

/// Asserts that with the hot tier full a put that goes to the cold tier writes the real object,
/// create-only, with no reservation (the cold tier's create decides), for a held body and a
/// streamed one; that a presigned put is refused with `Full` and writes no reservation; and that
/// the completion of a multipart upload writes none either.
#[tokio::test(start_paused = true)]
async fn at_full_a_put_to_the_cold_tier_writes_the_object_and_no_reservation() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    set_usage(&rig, 1000, 0).await;

    for path in ["put", "stream"] {
        let key = format!("direct-{path}");
        put_by(&proxy, path, &key).await.unwrap();
        let held = rig.cold_mem.head(&key).await.unwrap();
        assert!(!held.reservation, "{path}");
        assert_eq!(read(&*rig.cold_mem, &key).await, body("new"), "{path}");
    }
    let refused = put_by(&proxy, "hot_only", "presigned").await;
    assert_eq!(refused, Err(Error::Full("presigned".to_owned())));
    assert_absent(&*rig.cold_mem, "presigned").await;

    let id = proxy.create_upload("multi").await.unwrap();
    let tag = proxy
        .upload_part("multi", &id, 1, chunks(&["new"]))
        .await
        .unwrap();
    proxy
        .complete_upload("multi", &id, &[(1, tag)])
        .await
        .unwrap();
    assert!(!rig.cold_mem.head("multi").await.unwrap().reservation);
    assert_eq!(rig.cold.calls(Op::PutReservation), 0);
}

/// Asserts that the completion of a multipart upload is refused with `AlreadyExists` where the
/// cold tier holds the key as a reservation (the cold tier's create-only completion decides, with
/// no look at the hot tier's journal), and leaves the reservation as it was.
#[tokio::test(start_paused = true)]
async fn a_multipart_completion_is_refused_by_a_reservation_of_its_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let planted = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();

    let id = proxy.create_upload("k").await.unwrap();
    let tag = proxy
        .upload_part("k", &id, 1, chunks(&["new"]))
        .await
        .unwrap();
    let refused = proxy.complete_upload("k", &id, &[(1, tag)]).await;

    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.cold_mem.head("k").await.unwrap().tag, planted);
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);
}

/// Asserts that a held put writes its journal entry while the hot tier's create is still on its
/// way (the hot tier is held at the create): the journal object is in the hot tier before the
/// create answers, the put is not acknowledged until it does, and it then is, with the entry that
/// names its reservation.
#[tokio::test(start_paused = true)]
async fn a_held_put_journals_while_its_hot_create_is_on_its_way_and_waits_for_both() {
    let rig = Rig::new();
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.hot.clone(),
        "k",
        entered.clone(),
        Some(release.clone()),
    ));
    let proxy = Arc::new(
        Proxy::open(slow, rig.cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap(),
    );
    let putting = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.put("k", body("value")).await })
    };
    entered.notified().await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 1);
    assert_absent(&*rig.hot_mem, "k").await;
    assert!(!putting.is_finished());

    release.notify_one();
    putting.await.unwrap().unwrap();
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    let tag = rig.cold_mem.head("k").await.unwrap().tag;
    assert_eq!(entries[0].op, JournalOp::PutReserved);
    assert_eq!(entries[0].cold_tag, Some(tag));
}

/// Asserts that a put that is dropped (its caller gave up) while the hot tier's create is on its
/// way releases its reservation, so the key is free again, for a held and a streamed put.
#[tokio::test(start_paused = true)]
async fn a_dropped_put_releases_its_reservation() {
    for path in ["put", "stream"] {
        let rig = Rig::new();
        let entered = Arc::new(Notify::new());
        let stuck: Arc<dyn Backend> =
            Arc::new(Stall::new(rig.hot.clone(), "k", entered.clone(), None));
        let proxy = Arc::new(
            Proxy::open(stuck, rig.cold.clone(), rig.config("p1", 1000))
                .await
                .unwrap(),
        );
        let putting = {
            let proxy = proxy.clone();
            tokio::spawn(async move { put_by(&proxy, path, "k").await })
        };
        entered.notified().await;
        assert_reserved(&*rig.cold_mem, "k").await;

        putting.abort();
        assert!(putting.await.unwrap_err().is_cancelled());
        // The release is a background task: the paused clock moves on only when it, and every
        // other task, has nothing left to do.
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_absent(&*rig.cold_mem, "k").await;
        assert_absent(&*rig.hot_mem, "k").await;
        assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 1, "{path}");
        put_by(&proxy, path, "other").await.unwrap();
    }
}

/// Asserts that a put that is dropped after the hot tier took its object and before its journal
/// object was written removes that object, by its content tag, and then releases its reservation
/// (each a conditional delete, never a plain one), for a held and a streamed put.
#[tokio::test(start_paused = true)]
async fn a_dropped_put_removes_its_hot_object_by_tag_first_and_then_releases() {
    for path in ["put", "stream"] {
        let rig = Rig::new();
        let entered = Arc::new(Notify::new());
        let stuck: Arc<dyn Backend> = Arc::new(Stall::new(
            rig.hot.clone(),
            "_journal/",
            entered.clone(),
            None,
        ));
        let proxy = Arc::new(
            Proxy::open(stuck, rig.cold.clone(), rig.config("p1", 1000))
                .await
                .unwrap(),
        );
        let putting = {
            let proxy = proxy.clone();
            tokio::spawn(async move { put_by(&proxy, path, "k").await })
        };
        entered.notified().await;
        assert_eq!(read(&*rig.hot_mem, "k").await, body("new"), "{path}");

        putting.abort();
        assert!(putting.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_absent(&*rig.hot_mem, "k").await;
        assert_absent(&*rig.cold_mem, "k").await;
        assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 1, "{path}");
        assert_eq!(rig.hot.calls(Op::Delete), 0, "{path}");
        assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 1, "{path}");
    }
}

/// Asserts that a put whose journal cannot be written and whose own object cannot be found again
/// (the hot tier fails the HEAD that gives the tag to delete by) still fails with the journal's
/// error and releases its reservation: the object it made stays in the hot tier, as the limits
/// of the proxy say, and the cold tier holds nothing.
#[tokio::test(start_paused = true)]
async fn a_put_that_cannot_find_its_own_object_again_still_releases_its_reservation() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let down = Error::Unavailable("journal".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", down.clone());
    rig.hot
        .fail_next_on(Op::Head, "k", Error::Unavailable("head".to_owned()));

    let failed = proxy.put("k", body("value")).await;

    assert_eq!(failed, Err(down));
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(rig.hot.calls(Op::DeleteIfMatch), 0);
    assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 1);
}

/// Asserts that a copy-back whose journal entry cannot be written removes the copy it made in
/// the hot tier (an object the journals do not name would never be evicted), journals nothing,
/// and does not fail the read that made it.
#[tokio::test(start_paused = true)]
async fn a_copy_back_whose_journal_entry_cannot_be_written_removes_the_copy() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let down = Error::Unavailable("journal".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", down);

    assert_eq!(proxy.get("k", None).await, Ok(body("value")));
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );
}
