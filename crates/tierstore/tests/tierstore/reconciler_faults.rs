//! The reconciler when the hot tier, the cold tier or a journal object fails.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, Error, JournalEntry, JournalOp, Op, Reconciler, encode_journal};

use crate::hold::{AtHead, OnHead, Reads};
use crate::support::{Rig, assert_absent, body, journal_key, keys, read, text};

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
        assert_eq!(keys(&*rig.cold_mem).await, ["k1"]);
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
    assert_absent(&*rig.cold_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("1"));
}

/// A reconciler `r1` over the rig whose tiers do `hot` and `cold` at the HEADs of `key`.
fn scripted(
    rig: &Rig,
    key: &str,
    hot: Vec<Option<AtHead>>,
    cold: Vec<Option<AtHead>>,
) -> Reconciler {
    let hot_tier: Arc<dyn Backend> = Arc::new(OnHead::new(rig.hot.clone(), key, hot));
    let cold_tier: Arc<dyn Backend> = Arc::new(OnHead::new(rig.cold.clone(), key, cold));
    Reconciler::new(hot_tier, cold_tier, rig.reconciler_config("r1", 1000))
}

/// Asserts that a copy whose first read of the hot tier's tag fails is reported, leaves the
/// object for the next pass, and does not move the checkpoint past it.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_hot_tag_read_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k1", body("1")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Head, "k1", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.cold_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("1"));
}

/// Asserts that a copy whose object is deleted from the hot tier between the read of its tag
/// and the read of its bytes is skipped without a failure, writes nothing to the cold tier, and
/// lets the checkpoint move past it.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_object_is_deleted_between_the_tag_and_the_read_is_skipped() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("1")).await.unwrap();
    let mut reconciler = scripted(&rig, "k", vec![Some(AtHead::After(None))], vec![]);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_eq!(keys(&*rig.cold_mem).await, Vec::<String>::new());
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#);
}

/// Asserts that a copy whose second read of the hot tier's tag fails, while the object is still
/// there, takes its cold copy back, reports the failure, and leaves the object to the next pass,
/// which copies it.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_second_tag_read_fails_is_copied_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("1")).await.unwrap();
    let hot = vec![None, Some(AtHead::Fail(down()))];
    let mut reconciler = scripted(&rig, "k", hot, vec![]);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("1"));
}

/// Asserts that a copy whose second read of the hot tier's tag fails after the object was deleted
/// leaves no copy in the cold tier: the failure is reported, the cold tier is empty already, and
/// the next pass, which finds the hot tier without the key, has nothing to copy.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_second_tag_read_fails_after_a_delete_leaves_no_cold_copy() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("1")).await.unwrap();
    let hot = vec![None, Some(AtHead::Lost(down()))];
    let mut reconciler = scripted(&rig, "k", hot, vec![]);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a copy finds the cold object it wrote gone, or replaced by another writer, when
/// it comes to take it back after the hot object was deleted, and leaves the other writer's
/// object alone: no failure, nothing counted as copied, and the cold tier holds what the other
/// writer left (or nothing).
#[tokio::test(start_paused = true)]
async fn a_copy_takes_back_only_the_cold_object_it_wrote() {
    let cases = [
        (Some(AtHead::Before(None)), None),
        (
            Some(AtHead::Before(Some("other-bytes"))),
            Some("other-bytes"),
        ),
        (
            Some(AtHead::After(Some("other-bytes"))),
            Some("other-bytes"),
        ),
        (Some(AtHead::After(None)), None),
    ];
    for (step, want) in cases {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        proxy.put("k", body("1")).await.unwrap();
        let hot = vec![None, Some(AtHead::Before(None))];
        let mut reconciler = scripted(&rig, "k", hot, vec![step]);

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        assert_eq!(report.copied, 0);
        match want {
            Some(content) => assert_eq!(read(&*rig.cold_mem, "k").await, body(content)),
            None => assert_absent(&*rig.cold_mem, "k").await,
        }
    }
}

/// Asserts that a copy whose conditional delete of its own cold object is refused because the
/// object has another tag, or because it is gone, is not a failure: the copy is not counted, and
/// the checkpoint moves.
#[tokio::test(start_paused = true)]
async fn a_take_back_the_cold_tier_refuses_as_changed_or_gone_is_not_a_failure() {
    let refusals = [
        Error::PreconditionFailed("k".to_owned()),
        Error::NotFound("k".to_owned()),
    ];
    for refusal in refusals {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        proxy.put("k", body("1")).await.unwrap();
        let hot = vec![None, Some(AtHead::Before(None))];
        let mut reconciler = scripted(&rig, "k", hot, vec![]);
        rig.cold.fail_next_on(Op::DeleteIfMatch, "k", refusal);

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        assert_eq!(report.copied, 0);
        let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
        assert_eq!(stored, r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#);
    }
}

/// Asserts that a copy that cannot delete its own cold object when it takes the copy back (the
/// cold tier's conditional delete fails) reports the failure and does not move the checkpoint.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_take_back_fails_is_reported() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("1")).await.unwrap();
    let hot = vec![None, Some(AtHead::Before(None))];
    let mut reconciler = scripted(&rig, "k", hot, vec![]);
    rig.cold.fail_next_on(Op::DeleteIfMatch, "k", down());

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
}

/// Asserts that a copy that finds the cold tier holding the key with other bytes and cannot
/// read the cold object's tag, or cannot compare it because the read fails (before the first
/// chunk or after it), reports the failure, leaves the cold object alone, and replaces it in
/// the next pass.
#[tokio::test(start_paused = true)]
async fn a_comparison_that_fails_is_retried_by_the_next_pass() {
    for stage in ["head", "read"] {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        proxy.put("k", body("hot")).await.unwrap();
        rig.cold_mem.put("k", body("hat")).await.unwrap();
        let mut reconciler = rig.reconciler("r1", 1000);
        match stage {
            "head" => rig.cold.fail_next_on(Op::Head, "k", down()),
            _ => rig.cold.fail_next_on(Op::Get, "k", down()),
        }

        let report = reconciler.pass().await;
        assert_eq!(report.failed, [down()], "{stage}");
        assert_eq!(read(&*rig.cold_mem, "k").await, body("hat"));
        assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new(), "{stage}");
        assert_eq!(report.copied, 1);
        assert_eq!(read(&*rig.cold_mem, "k").await, body("hot"));
    }
}

/// Asserts that a copy whose comparison with the cold object fails in the middle of the cold
/// object's bytes reports that failure and leaves the cold object alone.
#[tokio::test(start_paused = true)]
async fn a_comparison_whose_cold_read_breaks_midway_is_reported() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("hot")).await.unwrap();
    rig.cold_mem.put("k", body("hat")).await.unwrap();
    let cold: Arc<dyn Backend> = Arc::new(Reads::new(rig.cold.clone(), 1, Some(down())));
    let mut reconciler = Reconciler::new(rig.hot.clone(), cold, rig.reconciler_config("r1", 1000));

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hat"));
    assert_eq!(rig.cold.calls(Op::DeleteIfMatch), 0);
}

/// Asserts that a copy that must replace a cold object of other bytes, and whose read of the hot
/// tier's tag (to see that the hot object is still there) or whose conditional delete of the cold
/// object fails, reports the failure, leaves the cold object, and replaces it in the next pass.
#[tokio::test(start_paused = true)]
async fn a_replace_that_fails_is_retried_by_the_next_pass() {
    for stage in ["hot tag", "delete"] {
        let rig = Rig::new();
        let proxy = rig.proxy("p1", 1000).await;
        proxy.put("k", body("hot")).await.unwrap();
        rig.cold_mem.put("k", body("cold")).await.unwrap();
        let mut reconciler = match stage {
            "hot tag" => {
                let hot = vec![None, Some(AtHead::Fail(down()))];
                scripted(&rig, "k", hot, vec![])
            }
            _ => {
                rig.cold.fail_next_on(Op::DeleteIfMatch, "k", down());
                rig.reconciler("r1", 1000)
            }
        };

        let report = reconciler.pass().await;
        assert_eq!(report.failed, [down()], "{stage}");
        assert_eq!(read(&*rig.cold_mem, "k").await, body("cold"));

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new(), "{stage}");
        assert_eq!(read(&*rig.cold_mem, "k").await, body("hot"));
    }
}

/// Asserts that a cold object that another writer replaces after the copy read its tag, and
/// before the copy deletes it, is not deleted: the conditional delete is refused, the copy's own
/// write is then refused as existing and reported, and the next pass replaces the new object.
#[tokio::test(start_paused = true)]
async fn a_cold_object_replaced_before_the_conditional_delete_is_not_deleted() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("hot")).await.unwrap();
    rig.cold_mem.put("k", body("cold")).await.unwrap();
    let cold = vec![Some(AtHead::After(Some("other-bytes")))];
    let mut reconciler = scripted(&rig, "k", vec![], cold);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [Error::AlreadyExists("k".to_owned())]);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("other-bytes"));

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot"));
}

/// Asserts that a cold object that is removed after the copy read its tag, and before the copy
/// deletes it, does not fail the copy: the object is written and counted as copied.
#[tokio::test(start_paused = true)]
async fn a_cold_object_removed_before_the_conditional_delete_is_written_over() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("hot")).await.unwrap();
    rig.cold_mem.put("k", body("cold")).await.unwrap();
    let mut reconciler = scripted(&rig, "k", vec![], vec![Some(AtHead::After(None))]);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot"));
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
    rig.cold.set_delay(Op::Put, Some(Duration::from_secs(10)));
    let mut reconciler = rig.reconciler("r1", 100);

    let fail_the_next_listing = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.hot.fail_next_on(Op::List, "_journal/", down());
    };
    let (report, ()) = tokio::join!(reconciler.pass(), fail_the_next_listing);
    assert_eq!((report.copied, report.evicted), (5, 0));
    assert_eq!(report.failed, [down()]);
    assert_eq!(rig.hot_mem.head("k1").await.unwrap().size, 21);

    rig.cold.set_delay(Op::Put, None);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
}

/// Asserts that an eviction whose delete from the hot tier fails is reported and leaves the
/// object in the hot tier and in the count, and that the next pass evicts it.
#[tokio::test(start_paused = true)]
async fn an_eviction_whose_delete_fails_is_retried_by_the_next_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 100);
    rig.hot.fail_next_on(Op::Delete, "k1", down());

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
