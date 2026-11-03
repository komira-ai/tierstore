//! The reconciler over a cold tier that holds reservations (made here by hand, with the journal
//! entries a reserving put would write): what a copy does for each kind of entry against each
//! state of the cold tier, the delete entries it applies, and the eviction by content tag.

use std::sync::Arc;

use tierstore::{
    Backend, Error, JournalEntry, JournalOp, MemoryBackend, Op, Reconciler, decode_journal,
    encode_journal,
};
use tokio::sync::Notify;

use crate::churn::{Change, Churn, Flaky};
use crate::conformance::reservation;
use crate::hold::HoldDelete;
use crate::support::{Rig, assert_absent, body, journal_key, read, text};

fn down() -> Error {
    Error::Unavailable("down".to_owned())
}

fn entry(op: JournalOp, key: &str, tag: Option<&str>) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size: 5,
        time: 0,
        cold_tag: tag.map(str::to_owned),
        // The two tiers of these tests tag bytes alike, so a mirror's hot tag is its cold tag.
        hot_tag: tag
            .filter(|_| op == JournalOp::PutMirror)
            .map(str::to_owned),
    }
}

/// A rig with proxy `p1` registered (so that the reconciler finds its journal), and `entries` as
/// its journal object 1.
async fn with_journal(entries: &[JournalEntry]) -> Rig {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    let object = encode_journal(entries);
    rig.hot_mem
        .put(&journal_key("p1", 1), object)
        .await
        .unwrap();
    rig
}

/// The tag that an object of `text` has in any backend: a tag names content.
async fn tag_of(text: &str) -> String {
    let scratch = MemoryBackend::new();
    scratch.put("k", body(text)).await.unwrap();
    scratch.head("k").await.unwrap().tag
}

/// A rig as `with_journal` makes it, whose journal object holds one mirror of `k`: it names the
/// tag of the bytes `hot!!`, which the tests put in the hot tier, so the mirror owns that object.
async fn with_mirror_of_hot() -> Rig {
    let tag = tag_of("hot!!").await;
    with_journal(&[entry(JournalOp::PutMirror, "k", Some(&tag))]).await
}

const STORED: &str = r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#;

/// The tags that reservations of `bodies` under one key have, each replacing the one before it:
/// a tag names content, so a cold tier gives the same ones.
async fn chain(bodies: &[u32]) -> Vec<String> {
    let scratch = MemoryBackend::new();
    let mut tags: Vec<String> = Vec::new();
    for n in bodies {
        let tag = match tags.last() {
            None => scratch.put_reservation("k", reservation(*n)).await,
            Some(last) => {
                let swap = scratch.put_reservation_if_match("k", reservation(*n), last);
                swap.await
            }
        };
        tags.push(tag.unwrap());
    }
    tags
}

/// The tag of the reservation of body 1.
async fn first_tag() -> String {
    chain(&[1]).await.remove(0)
}

/// Asserts that a reservation the entry names is replaced by the hot bytes, in one conditional
/// write: the cold object is then a real object of those bytes, the checkpoint moves, the object
/// is counted as copied and in the usage.
#[tokio::test(start_paused = true)]
async fn a_reservation_the_entry_names_is_replaced_by_the_hot_bytes() {
    let tag = first_tag().await;
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some(&tag))]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot!!"));
    assert!(!rig.cold_mem.head("k").await.unwrap().reservation);
    assert_eq!(rig.cold.calls(Op::PutIfMatch), 1);
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (5, 0));
    assert_eq!(text(&*rig.hot_mem, "_tiering/checkpoint/p1").await, STORED);
}

/// Asserts that a reservation no entry of the key names is another put's, and is left alone with
/// the hot object, and that the checkpoint still moves past the entry.
#[tokio::test(start_paused = true)]
async fn a_reservation_the_entries_do_not_name_is_left_alone() {
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some("not-its-tag"))]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let tag = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    let head = rig.cold_mem.head("k").await.unwrap();
    assert_eq!((head.reservation, head.tag), (true, tag));
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
    assert_eq!(text(&*rig.hot_mem, "_tiering/checkpoint/p1").await, STORED);
}

/// Asserts that a reservation that is replaced by another the entries name while the copy is under
/// way is looked at again, and that the copy then replaces the new one.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_reservation_was_replaced_reads_again_and_copies() {
    let tags = chain(&[1, 2]).await;
    let rig = with_journal(&[
        entry(JournalOp::PutReserved, "k", Some(&tags[0])),
        entry(JournalOp::PutReserved, "k", Some(&tags[1])),
    ])
    .await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let swap = vec![Change::Reservation(reservation(2))];
    let churn = Churn::new(rig.cold_mem.clone(), swap).on_key("k");
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = Reconciler::new(rig.hot.clone(), Arc::new(churn), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot!!"));
    assert!(!rig.cold_mem.head("k").await.unwrap().reservation);
}

/// Asserts that a reservation replaced under three looks in a row is an error that the pass
/// reports (the checkpoint stays before it), and that the next pass, which finds it still, copies.
#[tokio::test(start_paused = true)]
async fn three_changes_under_one_copy_is_an_error_the_next_pass_retries() {
    let tags = chain(&[1, 2, 3, 4]).await;
    let entries: Vec<JournalEntry> = tags
        .iter()
        .map(|tag| entry(JournalOp::PutReserved, "k", Some(tag)))
        .collect();
    let rig = with_journal(&entries).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let changes = (2..=4)
        .map(|n| Change::Reservation(reservation(n)))
        .collect();
    let churn = Churn::new(rig.cold_mem.clone(), changes).on_key("k");
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = Reconciler::new(rig.hot.clone(), Arc::new(churn), config);

    let report = reconciler.pass().await;
    let changed = Error::Unavailable("the cold copy of k changed under the copy".to_owned());
    assert_eq!(report.failed, [changed]);
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot!!"));
}

/// Asserts that a key the hot tier lost (a put and a delete that were both acknowledged) has the
/// reservations its entries name deleted, each by its own tag: one that is not there is
/// ignored, one with another tag is left, and a real object of the key is never touched.
#[tokio::test(start_paused = true)]
async fn a_put_and_a_delete_both_acked_leave_no_reservation() {
    let held = first_tag().await;
    let rig = with_journal(&[
        entry(JournalOp::PutReserved, "k", Some("not-its-tag")),
        entry(JournalOp::PutReserved, "k", Some(&held)),
        entry(JournalOp::PutReserved, "j", Some("nothing-to-delete")),
        entry(JournalOp::PutReserved, "m", Some("a-tag")),
    ])
    .await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.cold_mem.put("m", body("real")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.cold_mem, "j").await;
    assert_eq!(read(&*rig.cold_mem, "m").await, body("real"));
    assert_eq!(text(&*rig.hot_mem, "_tiering/checkpoint/p1").await, STORED);
}

/// Asserts that a reservation whose conditional delete fails is reported, and deleted by the next
/// pass.
#[tokio::test(start_paused = true)]
async fn a_reservation_whose_release_fails_is_released_by_the_next_pass() {
    let held = first_tag().await;
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some(&held))]).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.cold.fail_next_on(Op::DeleteIfMatch, "k", down());
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a reserved put whose cold tier holds nothing (a repointed or restored bucket)
/// keeps the hot object, reports the key, and does not move the checkpoint.
#[tokio::test(start_paused = true)]
async fn a_put_entry_whose_cold_bucket_was_replaced_keeps_the_hot_object_and_reports() {
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some("gone"))]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    let why = "the cold tier holds nothing for k; the hot object is kept";
    assert_eq!(report.failed, [Error::Other(why.to_owned())]);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
}

/// Asserts that a reserved put whose hot object vanishes between the look and the read is looked at
/// again, which finds the hot tier without the key and releases the reservation.
#[tokio::test(start_paused = true)]
async fn a_reserved_put_whose_object_vanishes_under_the_copy_releases_its_reservation() {
    let held = first_tag().await;
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some(&held))]).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let hot = Churn::before_a_read(rig.hot_mem.clone(), Change::Remove).on_key("k");
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = Reconciler::new(Arc::new(hot), rig.cold.clone(), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.cold_mem, "k").await;
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a copy whose read of the hot object fails, and one whose conditional write fails,
/// are reported and leave the reservation, and that the next pass copies.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_replacement_or_read_fails_is_retried_by_the_next_pass() {
    let held = first_tag().await;
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some(&held))]).await;
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    rig.hot.fail_next_on(Op::Get, "k", down());
    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);

    rig.cold.fail_next_on(Op::PutIfMatch, "k", down());
    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("hot!!"));
}

/// Asserts that a mirror (a copy-back) never replaces a reservation: with the cold tier holding
/// one, the hot copy is an orphan and is deleted by its tag, and the reservation stays.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_never_replaces_a_reservation() {
    let rig = with_mirror_of_hot().await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let tag = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.hot_mem, "k").await;
    let head = rig.cold_mem.head("k").await.unwrap();
    assert_eq!((head.reservation, head.tag), (true, tag));
}

/// Asserts that a mirror of the cold object it names counts as copied and stays in the hot tier
/// (and is in the usage), and that a mirror of an object the cold tier has since replaced with
/// another is a stale copy that is deleted from the hot tier, by its tag, and not copied.
#[tokio::test(start_paused = true)]
async fn a_late_copy_back_does_not_shadow_a_newer_cold_object() {
    let scratch = MemoryBackend::new();
    scratch.put("k", body("old")).await.unwrap();
    let old_tag = scratch.head("k").await.unwrap().tag;
    scratch.put("j", body("same")).await.unwrap();
    let same_tag = scratch.head("j").await.unwrap().tag;
    let rig = with_journal(&[
        entry(JournalOp::PutMirror, "k", Some(&old_tag)),
        entry(JournalOp::PutMirror, "j", Some(&same_tag)),
    ])
    .await;
    rig.cold_mem.put("k", body("new")).await.unwrap();
    rig.cold_mem.put("j", body("same")).await.unwrap();
    rig.hot_mem.put("k", body("old")).await.unwrap();
    rig.hot_mem.put("j", body("same")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
    assert_eq!(read(&*rig.hot_mem, "j").await, body("same"));
    assert_eq!(reconciler.hot_bytes(), 5);
}

/// Asserts that a mirror of an object the cold tier no longer holds is deleted from the hot tier.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_without_a_cold_object_removes_the_hot_copy() {
    let rig = with_mirror_of_hot().await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a mirror whose hot copy was replaced by another object after it was looked at is
/// not deleted: the delete is on the tag the copy had.
#[tokio::test(start_paused = true)]
async fn an_orphan_whose_hot_object_was_replaced_is_not_removed() {
    let rig = with_mirror_of_hot().await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let swap = vec![Change::Object(body("other"))];
    let hot = Churn::new(rig.hot_mem.clone(), swap).on_key("k");
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = Reconciler::new(Arc::new(hot), rig.cold.clone(), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("other"));
}

/// Asserts that a mirror whose hot copy cannot be deleted is reported, and dropped by the next
/// pass.
#[tokio::test(start_paused = true)]
async fn a_mirror_whose_removal_fails_is_removed_by_the_next_pass() {
    let rig = with_mirror_of_hot().await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    rig.hot.fail_next_on(Op::DeleteIfMatch, "k", down());
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts what a plain `Put` entry (the entry of a release without reservations, which names no
/// cold tag) does with each state of the cold tier: nothing there is copied create-only, an object
/// there counts as copied and is left, a reservation there is left alone.
#[tokio::test(start_paused = true)]
async fn a_legacy_put_entry_copies_create_only_and_leaves_a_reservation_alone() {
    let rig = with_journal(&[
        entry(JournalOp::Put, "a", None),
        entry(JournalOp::Put, "b", None),
        entry(JournalOp::Put, "c", None),
    ])
    .await;
    for key in ["a", "b", "c"] {
        rig.hot_mem.put(key, body("hot!!")).await.unwrap();
    }
    rig.cold_mem.put("b", body("cold!")).await.unwrap();
    let tag = rig
        .cold_mem
        .put_reservation("c", reservation(1))
        .await
        .unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 2);
    assert_eq!(read(&*rig.cold_mem, "a").await, body("hot!!"));
    assert_eq!(read(&*rig.cold_mem, "b").await, body("cold!"));
    let head = rig.cold_mem.head("c").await.unwrap();
    assert_eq!((head.reservation, head.tag), (true, tag));
}

/// Asserts that a plain `Put` whose create-only copy loses the race to another writer's object
/// counts as copied, and that a hot object that vanishes between the look and the read is
/// skipped.
#[tokio::test(start_paused = true)]
async fn a_plain_copy_that_loses_a_race_counts_as_copied_and_a_vanished_object_is_skipped() {
    let rig = with_journal(&[
        entry(JournalOp::Put, "a", None),
        entry(JournalOp::Put, "b", None),
    ])
    .await;
    rig.hot_mem.put("a", body("hot!!")).await.unwrap();
    rig.hot_mem.put("b", body("hot!!")).await.unwrap();
    let exists = Error::AlreadyExists("a".to_owned());
    rig.cold.fail_next_on(Op::Put, "a", exists);
    rig.hot
        .fail_next_on(Op::Get, "b", Error::NotFound("b".to_owned()));
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_absent(&*rig.cold_mem, "b").await;
}

/// Asserts that a plain `Put` and a delete of the key in one journal object remove the cold tier's
/// object that is not the hot tier's (a stale copy), by its tag, and keep one that is the hot
/// tier's.
#[tokio::test(start_paused = true)]
async fn a_delete_with_a_plain_put_in_its_object_removes_a_stale_cold_object_by_its_tag() {
    let rig = with_journal(&[
        entry(JournalOp::Put, "k", None),
        entry(JournalOp::Delete, "k", None),
        entry(JournalOp::Put, "j", None),
        entry(JournalOp::Delete, "j", None),
    ])
    .await;
    rig.hot_mem.put("k", body("new!!")).await.unwrap();
    rig.cold_mem.put("k", body("old!!")).await.unwrap();
    rig.hot_mem.put("j", body("same!")).await.unwrap();
    rig.cold_mem.put("j", body("same!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "j").await, body("same!"));
}

/// Asserts that a delete entry never touches a reservation of the key, nor the object that a
/// reserved put made in its own object (the proxy removed the cold tier's object itself).
#[tokio::test(start_paused = true)]
async fn a_delete_entry_leaves_a_reservation_and_the_object_of_a_reserved_put_alone() {
    let held = first_tag().await;
    let rig = with_journal(&[
        entry(JournalOp::Put, "k", None),
        entry(JournalOp::Delete, "k", None),
        entry(JournalOp::PutReserved, "j", Some(&held)),
        entry(JournalOp::Delete, "j", None),
    ])
    .await;
    rig.hot_mem.put("k", body("new!!")).await.unwrap();
    rig.cold_mem
        .put_reservation("k", reservation(2))
        .await
        .unwrap();
    rig.hot_mem.put("j", body("hot!!")).await.unwrap();
    rig.cold_mem
        .put_reservation("j", reservation(1))
        .await
        .unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert!(rig.cold_mem.head("k").await.unwrap().reservation);
    assert_eq!(read(&*rig.cold_mem, "j").await, body("hot!!"));
}

/// Asserts that each call of a delete entry that is applied fails the pass when it fails, leaves the
/// checkpoint before it, and that the next pass applies it: the second look at the hot tier, the
/// second look at the cold tier (the first of each is the copy's), and the conditional delete (which
/// fails every time here, so the next pass fails too).
#[tokio::test(start_paused = true)]
async fn each_failing_call_of_an_applied_delete_entry_fails_the_pass() {
    for case in ["hot", "cold", "delete"] {
        let rig = with_journal(&[
            entry(JournalOp::Put, "k", None),
            entry(JournalOp::Delete, "k", None),
        ])
        .await;
        rig.hot_mem.put("k", body("new!!")).await.unwrap();
        rig.cold_mem.put("k", body("old!!")).await.unwrap();
        let config = rig.reconciler_config("r1", 1000);
        let (hot, cold): (Arc<dyn Backend>, Arc<dyn Backend>) = match case {
            "hot" => (
                Arc::new(Flaky::head_fails_at(rig.hot_mem.clone(), "k", 2)),
                rig.cold.clone(),
            ),
            "cold" => (
                rig.hot.clone(),
                Arc::new(Flaky::head_fails_at(rig.cold_mem.clone(), "k", 2)),
            ),
            _ => (
                rig.hot.clone(),
                Arc::new(Flaky::conditional_delete_fails(rig.cold_mem.clone())),
            ),
        };
        let mut reconciler = Reconciler::new(hot, cold, config);

        let report = reconciler.pass().await;
        assert_eq!(report.failed, [Flaky::error()], "{case}");
        assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
        assert_eq!(read(&*rig.cold_mem, "k").await, body("old!!"), "{case}");

        let report = reconciler.pass().await;
        if case == "delete" {
            assert_eq!(report.failed, [Flaky::error()], "{case}");
        } else {
            assert_eq!(report.failed, Vec::<Error>::new(), "{case}");
            assert_absent(&*rig.cold_mem, "k").await;
        }
    }
}

/// Asserts that a delete entry whose cold object vanishes, or is replaced, between the look
/// and the conditional delete is done, and takes nothing more.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_cold_object_changes_under_it_leaves_the_new_object() {
    for change in [Change::Remove, Change::Object(body("other"))] {
        let removed = matches!(change, Change::Remove);
        let rig = with_journal(&[
            entry(JournalOp::Put, "k", None),
            entry(JournalOp::Delete, "k", None),
        ])
        .await;
        rig.hot_mem.put("k", body("new!!")).await.unwrap();
        rig.cold_mem.put("k", body("old!!")).await.unwrap();
        let churn = Churn::new(rig.cold_mem.clone(), vec![change]).on_key("k");
        let config = rig.reconciler_config("r1", 1000);
        let mut reconciler = Reconciler::new(rig.hot.clone(), Arc::new(churn), config);

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        if removed {
            assert_absent(&*rig.cold_mem, "k").await;
        } else {
            assert_eq!(read(&*rig.cold_mem, "k").await, body("other"));
        }
    }
}

/// Puts five objects of 21 bytes through a proxy, one journal object each.
pub async fn five_objects() -> Rig {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    rig
}

/// Asserts that an eviction deletes on the content tag the copy found, so that an object put again
/// after the copy (here: replaced just before the delete) is kept, with the other candidates still
/// evicted down to the low-water mark.
#[tokio::test(start_paused = true)]
async fn an_eviction_keeps_a_hot_object_replaced_after_the_copy() {
    let rig = five_objects().await;
    let swap = vec![Change::Object(body("put again"))];
    let hot = Churn::new(rig.hot_mem.clone(), swap).on_key("k1");
    let config = rig.reconciler_config("r1", 100);
    let mut reconciler = Reconciler::new(Arc::new(hot), rig.cold.clone(), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body("put again"));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_absent(&*rig.hot_mem, "k3").await;
}

/// Asserts that an object that vanishes from the hot tier between the copy and its eviction is
/// forgotten and not counted as evicted.
#[tokio::test(start_paused = true)]
async fn an_object_that_vanished_before_its_eviction_is_forgotten_not_counted() {
    let rig = five_objects().await;
    let hot = Churn::new(rig.hot_mem.clone(), vec![Change::Remove]).on_key("k1");
    let config = rig.reconciler_config("r1", 100);
    let mut reconciler = Reconciler::new(Arc::new(hot), rig.cold.clone(), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 1);
    assert_eq!(reconciler.hot_bytes(), 63);
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
}

/// Asserts that a reconciler that took over after another applied the journal reads the tag of
/// each object from the hot tier just before it evicts it: an object gone by then is forgotten, a
/// failed read is reported and retried, and the pass evicts down to the low-water mark.
#[tokio::test(start_paused = true)]
async fn an_eviction_of_an_entry_an_earlier_reconciler_applied_reads_the_tag_first() {
    let rig = five_objects().await;
    let mut first = rig.reconciler("r1", 1000);
    first.pass().await;
    first.stop().await;
    rig.hot_mem.delete("k1").await.unwrap();
    rig.hot.fail_next_on(Op::Head, "k2", down());
    let mut second = rig.reconciler("r2", 100);

    let report = second.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.evicted, 0);

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 1);
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_eq!(second.hot_bytes(), 63);
}

/// A rig whose five objects were copied by a reconciler that has stopped, and a second reconciler
/// over it with a budget that makes it evict three of them: it holds no tag of any object.
pub async fn after_a_takeover() -> (Rig, Reconciler) {
    let rig = five_objects().await;
    let mut first = rig.reconciler("r1", 1000);
    first.pass().await;
    first.stop().await;
    let second = rig.reconciler("r2", 100);
    (rig, second)
}

/// Asserts that a reconciler that took over after another applied the journal does not evict an
/// object whose key the cold tier holds no real object of (nothing, or a reservation): the hot
/// object may be the only copy. It is left in the hot tier, the pass reports no failure, and the
/// objects after it are evicted down to the low-water mark.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_a_hot_object_the_cold_tier_holds_no_real_object_of() {
    let (rig, mut second) = after_a_takeover().await;
    rig.cold_mem.delete("k1").await.unwrap();
    rig.cold_mem.delete("k2").await.unwrap();
    let held = rig.cold_mem.put_reservation("k2", reservation(1)).await;
    held.unwrap();

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    assert_eq!(read(&*rig.hot_mem, "k2").await, body(&"x".repeat(21)));
    assert_absent(&*rig.hot_mem, "k3").await;
    assert_absent(&*rig.hot_mem, "k4").await;
}

/// Asserts that a reconciler that took over does not evict a hot object when the cold tier holds a
/// real object of the key that is not of the hot object's content (an older copy, while the hot
/// tier holds acknowledged newer bytes): the hot object is kept, the pass reports no failure, and
/// the objects after it are evicted down to the low-water mark.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_a_hot_object_the_cold_tier_holds_another_object_of() {
    let (rig, mut second) = after_a_takeover().await;
    rig.cold_mem.delete("k1").await.unwrap();
    rig.cold_mem
        .put("k1", body("an older object"))
        .await
        .unwrap();

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("an older object"));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_absent(&*rig.hot_mem, "k3").await;
}

/// Asserts that a candidate an eviction left (the cold tier held no object of its content) is
/// asked about again by the next pass, in the same order: once the cold tier holds the objects,
/// the oldest two are evicted. A pass that left all of them evicts nothing and reports no failure.
#[tokio::test(start_paused = true)]
async fn a_candidate_that_an_eviction_left_is_evicted_by_a_later_pass_once_cold_holds_it() {
    let (rig, mut second) = after_a_takeover().await;
    let keys = ["k1", "k2", "k3", "k4", "k5"];
    for key in keys {
        rig.cold_mem.delete(key).await.unwrap();
    }

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 0);
    assert_eq!(second.hot_bytes(), 105);

    for key in keys {
        rig.cold_mem.put(key, body(&"x".repeat(21))).await.unwrap();
    }
    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_eq!(read(&*rig.hot_mem, "k3").await, body(&"x".repeat(21)));
}

/// Asserts that a failed look at the cold tier before an eviction without a tag is reported, evicts
/// nothing, and is retried by the next pass, which evicts.
#[tokio::test(start_paused = true)]
async fn a_takeover_whose_cold_check_fails_reports_it_and_evicts_on_the_next_pass() {
    let (rig, mut second) = after_a_takeover().await;
    rig.cold.fail_next_on(Op::Head, "k1", down());

    let report = second.pass().await;
    assert_eq!(report.failed, [down()]);
    assert_eq!(report.evicted, 0);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
}

/// Asserts that the new ops count in the usage, the copy lag and the eviction like a plain put: a
/// reserved put and a mirror add their sizes, are copied, and are evicted oldest first.
#[tokio::test(start_paused = true)]
async fn the_new_ops_count_in_usage_lag_and_eviction() {
    let scratch = MemoryBackend::new();
    scratch.put("m", body("mirror")).await.unwrap();
    let tag_m = scratch.head("m").await.unwrap().tag;
    let tag_a = first_tag().await;
    let sized = |op, key: &str, size, time, tag: &str| JournalEntry {
        op,
        key: key.to_owned(),
        size,
        time,
        cold_tag: Some(tag.to_owned()),
        hot_tag: (op == JournalOp::PutMirror).then(|| tag.to_owned()),
    };
    let rig = with_journal(&[
        sized(JournalOp::PutReserved, "a", 60, 1, &tag_a),
        sized(JournalOp::PutMirror, "m", 50, 2, &tag_m),
    ])
    .await;
    rig.cold_mem
        .put_reservation("a", reservation(1))
        .await
        .unwrap();
    rig.cold_mem.put("m", body("mirror")).await.unwrap();
    rig.hot_mem.put("a", body("a-hot")).await.unwrap();
    rig.hot_mem.put("m", body("mirror")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 2);
    assert_eq!(report.evicted, 1);
    assert_absent(&*rig.hot_mem, "a").await;
    assert_eq!(read(&*rig.hot_mem, "m").await, body("mirror"));
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (50, 0));
}

/// Asserts that the new ops and the tag round-trip through the journal's encoding, that an entry
/// without a tag encodes as before (no new field), and that an entry written before the field
/// existed decodes with none. The previous release reading the new ops, and this branch reading the
/// previous release's, run in `crates/compat/tests/journal.rs`.
#[test]
fn the_new_ops_and_the_tag_round_trip_and_an_entry_without_a_tag_encodes_as_before() {
    let plain = entry(JournalOp::Put, "a", None);
    let reserved = entry(JournalOp::PutReserved, "b", Some("t1"));
    let mut mirror = entry(JournalOp::PutMirror, "c", Some("t2"));
    mirror.hot_tag = Some("h2".to_owned());
    let delete = entry(JournalOp::Delete, "d", None);
    let entries = vec![plain.clone(), reserved, mirror, delete];

    let encoded = encode_journal(&entries);
    assert_eq!(decode_journal(&encoded).unwrap(), entries);
    let single = encode_journal(&[plain]);
    let legacy = r#"[{"op":"PUT","key":"a","size":5,"time":0}]"#;
    assert_eq!(std::str::from_utf8(&single).unwrap(), legacy);
    let decoded = decode_journal(legacy.as_bytes()).unwrap();
    assert_eq!(decoded[0].cold_tag, None);
    assert_eq!(decoded[0].hot_tag, None);
    let before_hot_tag = r#"[{"op":"PUTMIRROR","key":"c","size":5,"time":0,"cold_tag":"t2"}]"#;
    let decoded = decode_journal(before_hot_tag.as_bytes()).unwrap();
    assert_eq!(decoded[0].cold_tag.as_deref(), Some("t2"));
    assert_eq!(decoded[0].hot_tag, None);
    let json = std::str::from_utf8(&encoded).unwrap();
    assert!(json.contains(r#""op":"PUTRESERVED""#), "{json}");
    assert!(json.contains(r#""cold_tag":"t2""#), "{json}");
    assert!(json.contains(r#""hot_tag":"h2""#), "{json}");
    assert_eq!(json.matches("hot_tag").count(), 1, "{json}");
}

/// Asserts that an object that is deleted and put again with new content while the reconciler
/// is about to evict the old one from the hot tier is not lost: after the eviction and further
/// passes, a GET of the key through the proxy gives the new content.
/// Catches an eviction that deletes the hot tier's key without checking it is still the object
/// it chose, so the put that came after its last read of the journals is deleted before it was
/// copied, and the later passes, finding the hot tier empty, apply the delete entry to the cold
/// tier and leave no object at all.
#[tokio::test(start_paused = true)]
async fn an_eviction_racing_a_delete_and_a_reput_keeps_the_new_object() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let old = "o".repeat(21);
    for key in ["k", "f1", "f2", "f3", "f4"] {
        rig.clock.advance(10);
        proxy.put(key, body(&old)).await.unwrap();
    }
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let held = Arc::new(HoldDelete::new(
        rig.hot.clone(),
        "k",
        entered.clone(),
        release.clone(),
    ));
    let mut reconciler = Reconciler::new(held, rig.cold.clone(), rig.reconciler_config("r1", 100));

    let client = async {
        entered.notified().await;
        proxy.delete("k").await.unwrap();
        proxy.put("k", body("new content")).await.unwrap();
        release.notify_one();
    };
    let (report, ()) = tokio::join!(reconciler.pass(), client);
    assert!(report.held);
    assert!(report.evicted >= 1);

    for _ in 0..3 {
        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
    }
    assert_eq!(proxy.get("k", None).await.unwrap(), body("new content"));
}

/// Writes `entries` as journal object `seq` of proxy `p1`.
async fn add_journal(rig: &Rig, seq: u64, entries: &[JournalEntry]) {
    let object = encode_journal(entries);
    let key = journal_key("p1", seq);
    rig.hot_mem.put(&key, object).await.unwrap();
}

/// Asserts that a delete entry that is alone in its journal object is applied to the cold tier for a
/// key that only plain puts put (it removes a stale copy, as a release without reservations needs),
/// and is not for a key that a reserved put put: the proxy that wrote that entry removed the cold
/// tier's object itself, and an object a put made straight to the cold tier since is not its to take.
#[tokio::test(start_paused = true)]
async fn a_delete_entry_alone_is_applied_to_a_plain_key_and_not_to_a_reserved_one() {
    let rig = with_journal(&[
        entry(JournalOp::PutReserved, "k", Some("its-reservation")),
        entry(JournalOp::Put, "j", None),
    ])
    .await;
    add_journal(
        &rig,
        2,
        &[
            entry(JournalOp::Delete, "k", None),
            entry(JournalOp::Delete, "j", None),
        ],
    )
    .await;
    rig.cold_mem.put("k", body("cold!")).await.unwrap();
    rig.cold_mem.put("j", body("cold!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("cold!"));
    assert_absent(&*rig.cold_mem, "j").await;
}

/// Asserts that what the reconciler knows of a key a reserved put put is forgotten after the
/// journal retention: a delete entry of the key that comes later is applied as a plain key's is.
#[tokio::test(start_paused = true)]
async fn a_reserved_key_is_forgotten_after_the_retention() {
    let rig = with_journal(&[entry(JournalOp::PutReserved, "k", Some("its-reservation"))]).await;
    rig.cold_mem.put("k", body("cold!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());

    rig.clock.set(3_700_000);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    add_journal(&rig, 2, &[entry(JournalOp::Delete, "k", None)]).await;
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a mirror entry that names no tag deletes nothing: it owns no object, so the hot
/// copy stays, and nothing is copied.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_without_a_tag_leaves_the_hot_copy() {
    let rig = with_journal(&[entry(JournalOp::PutMirror, "k", None)]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a mirror entry whose tag is not the hot object's leaves the hot object when the
/// cold tier holds nothing: the object is not the mirror's.
#[tokio::test(start_paused = true)]
async fn a_putmirror_entry_does_not_delete_a_hot_object_of_another_tag() {
    let rig = with_journal(&[entry(JournalOp::PutMirror, "k", Some("old"))]).await;
    rig.hot_mem.put("k", body("hot!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
}
