//! The reconciler when the hot tier, the cold tier or a journal object fails.

use std::time::Duration;

use tierstore::{Backend, Error, JournalEntry, JournalOp, Op, encode_journal};

use crate::support::{Rig, assert_absent, assert_reserved, body, journal_key, keys, read, text};

fn down() -> Error {
    Error::Unavailable("on purpose".to_owned())
}

/// Asserts that a pass that cannot list the proxies reports the failure and does nothing
/// else, and that the next pass works through the journals.
#[tokio::test(start_paused = true)]
async fn a_pass_that_cannot_list_the_proxies_reports_it_and_the_next_one_works() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::List, "_tiering/proxies/", down());

    let report = reconciler.pass().await;
    assert!(report.held);
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.copied, 0);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
}

/// Asserts that a proxy whose checkpoint cannot be read is left out of the pass with the
/// failure reported, and picked up by the next pass.
#[tokio::test(start_paused = true)]
async fn a_checkpoint_that_cannot_be_read_leaves_the_proxy_to_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot
        .fail_next_on(Op::Get, "_tiering/checkpoint/", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.copied, 0);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
}

/// Asserts that a journal listing that fails, and a journal object that cannot be read, are
/// reported and leave the objects for the next pass.
#[tokio::test(start_paused = true)]
async fn a_journal_that_cannot_be_listed_or_read_is_read_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    rig.hot.fail_next_on(Op::List, "_journal/", down());
    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.copied, 0);

    rig.hot.fail_next_on(Op::Get, "_journal/", down());
    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.copied, 0);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
}

/// Asserts that a journal object that is not a list of entries stops the journal of its proxy
/// there, in every pass, and that the objects before it are still copied.
#[tokio::test(start_paused = true)]
async fn a_corrupt_journal_object_stops_the_journal_of_its_proxy() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("1")).await.unwrap();
    let garbage = body("not json");
    let second = journal_key("p1", 2);
    rig.hot_mem.put(&second, garbage).await.unwrap();
    proxy.put("k2", body("2")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    for copied in [1, 0] {
        let report = reconciler.pass().await;
        assert_eq!(report.copied, copied);
        assert_eq!(report.failed.len(), 1);
        assert!(matches!(report.failed[0], Error::Other(_)));
        // k1 was copied (its reservation replaced by the object); k2 is after the corrupt
        // object and keeps its reservation.
        assert_eq!(keys(&*rig.cold_mem).await, ["k1", "k2"]);
        assert!(!rig.cold_mem.head("k1").await.unwrap().reservation);
        assert_reserved(&*rig.cold_mem, "k2").await;
    }
}

/// Asserts that a journal object whose number does not follow the last one read (a listing
/// need not show the latest writes) is not read until the object before it is there.
#[tokio::test(start_paused = true)]
async fn a_gap_in_the_journal_stops_the_reading_until_it_is_filled() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("1")).await.unwrap();
    rig.hot_mem.put("x", body("x")).await.unwrap();
    let put_x = JournalEntry {
        op: JournalOp::Put,
        key: "x".to_owned(),
        size: 1,
        time: 0,
        cold_tag: None,
    };
    let third = encode_journal(&[put_x]);
    rig.hot_mem.put(&journal_key("p1", 3), third).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["k1"]);

    let second = encode_journal(&[]);
    rig.hot_mem
        .put(&journal_key("p1", 2), second)
        .await
        .unwrap();
    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(keys(&*rig.cold_mem).await, ["k1", "x"]);
}

/// Asserts that a copy whose read of the hot tier fails is reported, leaves the object for the
/// next pass, and does not move the checkpoint past it.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_hot_read_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Get, "k1", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_reserved(&*rig.cold_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("1"));
}

/// Asserts that a copy whose look at the hot tier fails is reported, leaves the checkpoint
/// before it, leaves the key's reservation in the cold tier, and is done by the next pass. (A delete entry makes looks of its own only if it is
/// applied to the cold tier: the same journal object puts the key with a plain `Put`, or has no put
/// of the key and the reconciler's books do not classify the key as one that a reserved put or a
/// mirror put, as a standalone delete of a plain key. Their failing looks are
/// `each_failing_call_of_an_applied_delete_entry_fails_the_pass` and
/// `a_delete_entry_alone_is_applied_to_a_plain_key_and_not_to_a_reserved_one`.)
#[tokio::test(start_paused = true)]
async fn a_copy_whose_hot_check_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Head, "k", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_reserved(&*rig.cold_mem, "k").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("1"));
}

/// Asserts that a copy whose look at the cold tier fails is reported, leaves the checkpoint
/// before it and the hot object where it is, and is done by the next pass.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_cold_check_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("new")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.cold.fail_next_on(Op::Head, "k", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("new"));

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
}

/// Asserts that an eviction whose last read of the journals fails is reported and evicts
/// nothing, and that the next pass evicts.
#[tokio::test(start_paused = true)]
async fn an_eviction_whose_journal_read_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    rig.cold
        .set_delay(Op::PutIfMatch, Some(Duration::from_secs(10)));
    let mut reconciler = rig.reconciler("r1", 100);

    let fail_the_next_listing = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.hot.fail_next_on(Op::List, "_journal/", down());
    };
    let (report, ()) = tokio::join!(reconciler.pass(), fail_the_next_listing);
    assert_eq!((report.copied, report.evicted), (5, 0));
    assert_eq!(report.failed, [down()]);
    assert_eq!(rig.hot_mem.head("k1").await.unwrap().size, 21);

    rig.cold.set_delay(Op::PutIfMatch, None);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
}

/// Asserts that an eviction whose delete from the hot tier (a delete on the content tag the
/// copy found) fails is reported and leaves the object in the hot tier and in the count, and
/// that the next pass evicts it.
#[tokio::test(start_paused = true)]
async fn an_eviction_whose_delete_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 100);
    rig.hot.fail_next_on(Op::DeleteIfMatch, "k1", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.evicted, 0);
    assert_eq!(reconciler.hot_bytes(), 105);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&value));

    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 2);
    assert_eq!(reconciler.hot_bytes(), 63);
    assert_absent(&*rig.hot_mem, "k1").await;
}

/// Asserts that a checkpoint that cannot be stored (the HEAD before the write fails, then the
/// write itself) is reported and tried again, and that nothing is lost meanwhile.
#[tokio::test(start_paused = true)]
async fn a_checkpoint_that_cannot_be_stored_is_stored_by_a_later_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    let hot = &rig.hot;
    hot.fail_next_on(Op::Head, "_tiering/checkpoint/", down());
    hot.fail_next_on(Op::Put, "_tiering/checkpoint/", down());

    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.failed), (1, vec![down()]));
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#);
}

/// Asserts that a usage that cannot be published is reported without stopping the rest of the
/// pass, and is published by the next pass.
#[tokio::test(start_paused = true)]
async fn a_usage_that_cannot_be_published_is_published_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("12")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Head, "_tiering/usage", down());

    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.failed), (1, vec![down()]));
    assert_absent(&*rig.hot_mem, "_tiering/usage").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    let usage = text(&*rig.hot_mem, "_tiering/usage").await;
    assert_eq!(usage, r#"{"hot_bytes":2,"copy_lag":0,"time":0}"#);
}

/// Asserts that a journal object whose delete fails is reported and kept, and is deleted by
/// the next pass (with no retention, objects are deleted as soon as the checkpoint is done
/// with them).
#[tokio::test(start_paused = true)]
async fn a_journal_object_whose_delete_fails_is_pruned_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut settings = rig.reconciler_config("r1", 100);
    settings.retention = Duration::ZERO;
    let mut reconciler = rig.reconciler_with(settings);
    rig.hot.fail_next_on(Op::Delete, "_journal/", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.pruned, 0);

    let report = reconciler.pass().await;
    assert_eq!(report.pruned, 2);
    assert_absent(&*rig.hot_mem, &journal_key("p1", 1)).await;
    assert_absent(&*rig.hot_mem, &journal_key("p1", 2)).await;
}

/// The entry that a proxy of an earlier release journals for a put: a plain `Put`, which names no
/// reservation.
fn legacy_put(key: &str, size: u64) -> JournalEntry {
    JournalEntry {
        op: JournalOp::Put,
        key: key.to_owned(),
        size,
        time: 0,
        cold_tag: None,
    }
}

/// Asserts that the copy of a plain `Put` entry whose hot object is gone between the look at it
/// and the read of it (the hot tier answers the read with not found) is skipped without a
/// failure: nothing is written to the cold tier, and the checkpoint moves past the entry.
#[tokio::test(start_paused = true)]
async fn a_plain_put_whose_hot_object_vanishes_before_the_read_is_skipped() {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("x", body("1")).await.unwrap();
    let entries = encode_journal(&[legacy_put("x", 1)]);
    rig.hot_mem
        .put(&journal_key("p1", 1), entries)
        .await
        .unwrap();
    let gone = Error::NotFound("x".to_owned());
    rig.hot.fail_next_on(Op::Get, "x", gone);
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;

    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#);
}

/// Asserts that the copy of a plain `Put` entry whose create in the cold tier fails is reported,
/// leaves the checkpoint before the entry and the cold tier without the key, and is done by the
/// next pass.
#[tokio::test(start_paused = true)]
async fn a_plain_put_whose_cold_create_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("x", body("1")).await.unwrap();
    let entries = encode_journal(&[legacy_put("x", 1)]);
    rig.hot_mem
        .put(&journal_key("p1", 1), entries)
        .await
        .unwrap();
    rig.cold.fail_next_on(Op::Put, "x", down());
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.cold_mem, "x").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "x").await, body("1"));
}
