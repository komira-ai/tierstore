//! A get that copies an object back into the hot tier, and a delete: the order of the delete's
//! two tiers, and what the copy-back does when the key left the cold tier while it worked.
//! Each interleaving is held at a point the test names with a gate, not with a delay.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, Error, JournalOp, Op, Proxy};
use tokio::sync::Notify;

use crate::hold::{HoldDelete, Stall};
use crate::support::{Rig, assert_absent, body, journal, journal_key, journal_keys, read};

/// Lets the background journal write of a copy-back, which waits a linger, finish (the clock
/// is paused, so this is virtual time).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// Asserts that a delete removes the cold tier's object before the hot tier's: while the
/// delete waits at the hot tier's removal, the cold tier is empty and the hot tier still holds
/// the object, and when it is let go both are empty and the delete is journaled.
#[tokio::test(start_paused = true)]
async fn a_delete_removes_the_cold_copy_before_the_hot_copy() {
    let rig = Rig::new();
    rig.hot_mem.put("k", body("value")).await.unwrap();
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let held_hot = Arc::new(HoldDelete::new(
        rig.hot.clone(),
        "k",
        entered.clone(),
        release.clone(),
    ));
    let proxy = Arc::new(
        Proxy::open(held_hot, rig.cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap(),
    );

    let deleter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.delete("k").await })
    };
    entered.notified().await;
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));

    release.notify_one();
    deleter.await.unwrap().unwrap();
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
}

/// Asserts that a key deleted while a get of it is copying it back from the cold tier into the
/// hot tier stays deleted: the get answers the bytes it read, the copy-back removes the copy it
/// wrote, nothing of it is journaled (the journal holds the delete entry alone), and after a
/// reconciler pass a get of the key is NotFound and neither tier holds it.
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

    // The copy-back goes on: the hot tier takes the object, the cold tier no longer holds the
    // key, and the copy is removed again.
    release.notify_one();
    let served = getter.await.unwrap();
    assert_eq!(served.unwrap(), body("value"));
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        [journal_key("p1", 1)]
    );
    assert_absent(&*rig.hot_mem, "k").await;

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

/// Asserts that a copy-back that checks the cold tier after the delete removed the cold
/// tier's object, but before the delete removed the hot tier's, removes its own copy: the get
/// answers the bytes, the hot tier is empty before the delete goes on, and the delete (which
/// then finds nothing to remove in the hot tier) journals once, with no put entry.
#[tokio::test(start_paused = true)]
async fn a_copy_back_that_loses_the_key_to_a_delete_before_its_check_removes_its_copy() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let put_entered = Arc::new(Notify::new());
    let put_release = Arc::new(Notify::new());
    let delete_entered = Arc::new(Notify::new());
    let delete_release = Arc::new(Notify::new());
    let held_hot = Arc::new(HoldDelete::new(
        rig.hot.clone(),
        "k",
        delete_entered.clone(),
        delete_release.clone(),
    ));
    let stalled_hot = Arc::new(Stall::new(
        held_hot,
        "k",
        put_entered.clone(),
        Some(put_release.clone()),
    ));
    let proxy = Arc::new(
        Proxy::open(stalled_hot, rig.cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap(),
    );

    let getter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.get("k", None).await })
    };
    put_entered.notified().await;
    let deleter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.delete("k").await })
    };
    delete_entered.notified().await;
    assert_absent(&*rig.cold_mem, "k").await;

    put_release.notify_one();
    assert_eq!(getter.await.unwrap().unwrap(), body("value"));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );

    delete_release.notify_one();
    deleter.await.unwrap().unwrap();
    assert_absent(&*rig.hot_mem, "k").await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].op, JournalOp::Delete);
}

/// Asserts that a copy-back whose check of the cold tier finds the key gone leaves alone an
/// object that a client put under the key after the delete, when its content differs (here the
/// same size): the get answers the bytes it read, the hot tier holds the put's object, and the
/// journal holds the delete entry and the put entry, and no more.
///
/// Catches a removal of the copy that takes whatever object the hot tier holds under the key,
/// and so loses a put that was acknowledged.
#[tokio::test(start_paused = true)]
async fn a_copy_back_does_not_remove_an_object_put_after_the_delete() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stalled_cold = Arc::new(Stall::on_head(
        rig.cold.clone(),
        "k",
        entered.clone(),
        release.clone(),
    ));
    let proxy = Arc::new(
        Proxy::open(rig.hot.clone(), stalled_cold, rig.config("p1", 1000))
            .await
            .unwrap(),
    );

    // The get copies the object into the hot tier and stops at its check of the cold tier.
    let getter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.get("k", None).await })
    };
    entered.notified().await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));

    // The key is deleted, and put again with other bytes of the same size.
    proxy.delete("k").await.unwrap();
    proxy.put("k", body("other")).await.unwrap();

    release.notify_one();
    assert_eq!(getter.await.unwrap().unwrap(), body("value"));
    settle().await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("other"));
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1), journal_key("p1", 2)]);
    let first = journal(&*rig.hot_mem, "p1", 1).await;
    let second = journal(&*rig.hot_mem, "p1", 2).await;
    assert_eq!(
        (first[0].op, second[0].op),
        (JournalOp::Delete, JournalOp::Put)
    );
}

/// Asserts that a copy-back whose check of the cold tier fails removes its copy and still
/// serves the read, journaling nothing; and that the next get, with the cold tier answering,
/// copies the object back and journals it.
#[tokio::test(start_paused = true)]
async fn a_copy_back_whose_cold_check_fails_removes_its_copy_and_still_serves_the_read() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.cold
        .fail_next_on(Op::Head, "k", Error::Unavailable("cold".to_owned()));

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    settle().await;
    assert_absent(&*rig.hot_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    settle().await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
}

/// Asserts that a copy-back that cannot take its copy away again (the hot tier fails the head,
/// the read, or the conditional delete) keeps it and journals it as a put, so that the
/// reconciler owns the object and can evict it, and still serves the read.
#[tokio::test(start_paused = true)]
async fn a_copy_back_that_cannot_remove_its_copy_journals_it() {
    let down = Error::Unavailable("hot".to_owned());
    for failing in 0..3 {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        rig.cold_mem.put("k", body("value")).await.unwrap();
        rig.cold
            .fail_next_on(Op::Head, "k", Error::Unavailable("cold".to_owned()));
        match failing {
            0 => rig.hot.fail_next_on(Op::Head, "k", down.clone()),
            1 => {
                // The first read of the key is the proxy's own miss.
                rig.hot
                    .fail_next_on(Op::Get, "k", Error::NotFound("k".to_owned()));
                rig.hot.fail_next_on(Op::Get, "k", down.clone());
            }
            _ => rig.hot.fail_next_on(Op::DeleteIfMatch, "k", down.clone()),
        }

        assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
        settle().await;
        assert_eq!(read(&*rig.hot_mem, "k").await, body("value"), "{failing}");
        let entries = journal(&*rig.hot_mem, "p1", 1).await;
        assert_eq!(entries.len(), 1, "{failing}");
        assert_eq!(entries[0].op, JournalOp::Put, "{failing}");
    }
}

/// Asserts that a copy-back whose conditional delete is told the object is gone, or is not the
/// one with the tag, counts the copy as taken away: nothing is journaled, and the read is served.
#[tokio::test(start_paused = true)]
async fn a_copy_back_whose_conditional_delete_finds_another_state_journals_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "b"] {
        rig.cold_mem.put(key, body("value")).await.unwrap();
        rig.cold
            .fail_next_on(Op::Head, key, Error::Unavailable("cold".to_owned()));
    }
    rig.hot
        .fail_next_on(Op::DeleteIfMatch, "a", Error::NotFound("a".to_owned()));
    rig.hot.fail_next_on(
        Op::DeleteIfMatch,
        "b",
        Error::PreconditionFailed("b".to_owned()),
    );

    for key in ["a", "b"] {
        assert_eq!(proxy.get(key, None).await.unwrap(), body("value"));
    }
    settle().await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());
}

/// Asserts that a put entry whose object is no longer in the hot tier, as when a copy-back's
/// journal write lands after the delete that removed its copy, copies nothing to the cold
/// tier: the reconciler skips the entry, so the deleted key does not come back.
#[tokio::test(start_paused = true)]
async fn a_put_entry_whose_hot_object_is_gone_copies_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();
    rig.hot_mem.delete("k").await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 0);
    assert!(report.failed.is_empty());
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "k").await;
}
