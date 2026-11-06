//! The paging and the pool of the pass over the markers, and what it does to the usage figure
//! (5.9 and 5.14).

use super::*;

/// Asserts that the marks bucket is listed in pages of `marks_page`: five markers at two a page
/// are five list calls' worth in three pages, each page of the size asked for, and every marker is
/// worked.
#[tokio::test]
async fn the_pass_lists_the_marks_bucket_in_pages_of_marks_page() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["a", "b", "c", "d", "e"] {
        rig.hot.put(key, bytes("v")).await.unwrap();
        rig.plant(dirty(key)).await;
    }
    let mut leader = rig.leader_with("r1", |config| config.marks_page = 2);
    assert_eq!(leader.step().await, report(5, 0));
    assert_eq!(rig.marks_calls.calls(Op::List), 3);
    assert_eq!(rig.marks_calls.listed_prefixes(), vec![String::new(); 3]);
    for key in ["a", "b", "c", "d", "e"] {
        assert_eq!(rig.shape(key).await, None);
        assert_eq!(rig.cold_body(key).await, Some(bytes("v")));
    }
}

/// Asserts that a listing that fails ends the work on the markers without forgetting what was
/// noted: the error is reported, no marker of the pages not reached is touched, and the next pass
/// goes on. (The usage is still tended and published after it.)
#[tokio::test]
async fn a_listing_that_fails_ends_the_pass_with_the_error() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v", None).await;
    rig.marks_calls.fail_next(Op::List, 1);
    let failed = leader.step().await;
    assert_eq!((failed.changed, failed.failed.len()), (0, 1));
    assert_eq!(leader.step().await, report(1, 0));
}

/// Asserts that a failure to list the registrations ends the pass with the error, and that a
/// marker that cannot be read is reported and tried again later.
#[tokio::test]
async fn a_marker_that_cannot_be_read_is_reported_and_tried_again() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v", None).await;
    rig.hot_calls.fail_next_on(
        Op::List,
        "_tiering/proxies/",
        Error::Unavailable("down".into()),
    );
    let blind = leader.step().await;
    assert_eq!((blind.changed, blind.failed.len()), (0, 1));
    rig.marks_calls
        .fail_next_on(Op::Get, "k", Error::Unavailable("down".into()));
    let unread = leader.step().await;
    assert_eq!((unread.changed, unread.failed.len()), (0, 1));
    rig.pass(Duration::from_secs(1));
    assert_eq!(leader.step().await, report(1, 0));
}

/// Asserts that a marker listed and removed before it was read (a put's cleanup) is skipped.
#[tokio::test]
async fn a_marker_that_vanishes_between_the_listing_and_its_read_is_skipped() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant(dirty("k")).await;
    let gated = Arc::new(GateBackend::new(rig.marks_calls.clone()));
    let gate = gated.gate(Op::List, "", Phase::Post);
    let reconciler = EntryReconciler::new(
        rig.hot_calls.clone(),
        rig.cold_calls.clone(),
        rig.config("r1"),
    );
    let mut leader = reconciler.with_marks(gated, BUCKET).unwrap();
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;
    rig.marks.delete("k").await.unwrap();
    gate.release();
    assert_eq!(step.await, report(0, 0));
    assert!(rig.cold_body("k").await.is_none());
}

/// Asserts that no more than `copy_concurrency` keys are worked at once: with two, the third
/// cold write does not start while two are held, and all three are written after they are let go.
#[tokio::test]
async fn the_copier_works_at_most_copy_concurrency_keys_at_once() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["a", "b", "c"] {
        rig.hot.put(key, bytes("v")).await.unwrap();
        rig.plant(dirty(key)).await;
    }
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let first = gated.gate(Op::Put, "", Phase::Pre);
    let second = gated.gate(Op::Put, "", Phase::Pre);
    let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
    leader.config.copy_concurrency = 2;
    let mut step = Box::pin(leader.step());
    park(&mut step, &first).await;
    park(&mut step, &second).await;
    assert_eq!(rig.cold_calls.calls(Op::Put), 0);
    first.release();
    second.release();
    assert_eq!(step.await, report(3, 0));
    assert_eq!(rig.cold_calls.calls(Op::Put), 3);
}

/// The leader `r1` over the cold tier `cold`, with a lease of an hour so that it stays held while
/// the monotonic clock moves minutes in the middle of a step, its settings changed by `tweak`.
fn long_lived(
    rig: &Rig,
    cold: Arc<dyn Backend>,
    tweak: impl FnOnce(&mut EntryReconcilerConfig),
) -> EntryReconciler {
    let mut config = rig.config("r1");
    config.base.lease_ttl = Duration::from_secs(3600);
    tweak(&mut config);
    EntryReconciler::new(rig.hot_calls.clone(), cold, config)
        .with_marks(rig.marks_calls.clone(), BUCKET)
        .expect("the settings hold together")
}

/// Asserts that a marker is first seen when its page is listed, not when the pass began: with
/// pages of one, the copy on page 1 takes longer than `lock_lease` (the monotonic clock moves
/// 121 s while its cold write is held), a live owner's lock first listed on page 2 is left by the
/// next pass, which counts 0 s of the 120 s since it was first seen, and the owner's registration,
/// rewritten meanwhile, is not departed.
#[tokio::test]
async fn a_lock_first_listed_late_in_a_long_pass_is_not_reaped_by_the_next_pass() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    rig.hot.put("a", bytes("v")).await.unwrap();
    rig.plant(dirty("a")).await;
    rig.plant(lock("b", MarkerWhy::Overwrite, "p1", false))
        .await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let held = gated.gate(Op::Put, "", Phase::Pre);
    let mut leader = long_lived(&rig, gated, |config| config.marks_page = 1);
    let mut step = Box::pin(leader.step());
    park(&mut step, &held).await;
    rig.pass(Duration::from_secs(121));
    held.release();
    assert_eq!(step.await, report(1, 1));
    rig.clock.advance(1000);
    rig.hot.delete(&proxy_key("p1")).await.unwrap();
    rig.register("p1").await;
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("b").await, Some((MarkerState::Lock, false, true)));
    rig.pass(Duration::from_secs(120));
    rig.clock.advance(1000);
    rig.hot.delete(&proxy_key("p1")).await.unwrap();
    rig.register("p1").await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(
        rig.shape("b").await,
        Some((MarkerState::Dirty, false, true))
    );
}

/// Asserts that the hold on a `gone` marker whose cold copy is already the hot object begins when
/// the leader looks at the cold object, not when the pass began: while the cold write of an
/// earlier marker of the page is held the monotonic clock moves 120 s, the hold of the second
/// marker is still on 119 999 ms after that, and over at 120 000.
#[tokio::test]
async fn a_hold_begins_when_the_cold_object_is_looked_at_and_not_when_the_pass_began() {
    let rig = Rig::with_md5_tags();
    rig.activate().await;
    rig.hot.put("a", bytes("v")).await.unwrap();
    rig.plant(dirty("a")).await;
    rig.hot.put("b", bytes("w")).await.unwrap();
    rig.cold.put("b", bytes("w")).await.unwrap();
    rig.plant(gone("b")).await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let held = gated.gate(Op::Put, "", Phase::Pre);
    let mut leader = long_lived(&rig, gated, |config| config.copy_concurrency = 1);
    let mut step = Box::pin(leader.step());
    park(&mut step, &held).await;
    rig.pass(Duration::from_secs(120));
    held.release();
    assert_eq!(step.await, report(2, 0));
    rig.pass(Duration::from_secs(120) - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("b").await, Some((MarkerState::Dirty, true, true)));
    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("b").await, None);
}

/// Asserts that the `DIRTY` markers of a page are worked before its locks: with a pool of one, the
/// order of the cold writes is that of the `DIRTY` markers although the lock's key sorts first,
/// and the lock of the dead owner is reaped after them.
#[tokio::test]
async fn the_dirty_markers_of_a_page_are_worked_before_its_locks() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant(lock("a", MarkerWhy::Overwrite, "p-dead", false))
        .await;
    rig.hot.put("b", bytes("v")).await.unwrap();
    rig.plant(dirty("b")).await;
    let mut leader = rig.leader_with("r1", |config| config.copy_concurrency = 1);
    assert_eq!(leader.step().await, report(2, 0));
    let log = rig.marks_calls.call_log();
    let touched = |op: Op, key: &str| log.iter().position(|call| *call == (op, key.to_owned()));
    let copied = touched(Op::PutIfMatch, "b");
    let reaped = touched(Op::PutIfMatch, "a");
    assert!(copied < reaped, "{log:?}");
}

/// Asserts that a `WANT` marker is left as it is: the leader neither locks it nor removes it
/// (the copy-back is the work of a later release), and counts it as nothing done.
#[tokio::test]
async fn a_want_marker_is_left_alone() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.plant(marker("k", MarkerState::Want, false, false))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 0));
    rig.pass(Duration::from_secs(600));
    assert_eq!(leader.step().await, report(0, 0));
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Want, false, false))
    );
    assert_eq!(
        rig.marks_calls.calls(Op::PutIfMatch) + rig.marks_calls.calls(Op::Delete),
        0
    );
}

/// Asserts that `copy_lag` is the bytes of the objects under `DIRTY` markers with hot present: a
/// marker with nothing behind it adds nothing, the lag falls as markers clear, and a marker that
/// is removed by someone else leaves the figure at the next pass.
#[tokio::test]
async fn copy_lag_is_the_dirty_bytes_with_hot_present() {
    let rig = Rig::new();
    rig.activate().await;
    for (key, body) in [("a", "123"), ("b", "12345")] {
        rig.hot.put(key, bytes(body)).await.unwrap();
        rig.plant(dirty(key)).await;
    }
    rig.plant(dirty("orphan")).await;
    rig.cold_calls.set_down(true);
    let mut leader = rig.leader("r1");
    leader.step().await;
    assert_eq!(rig.usage().await.copy_lag, 8);
    rig.marks.delete("b").await.unwrap();
    rig.pass(Duration::from_secs(20));
    leader.step().await;
    assert_eq!(rig.usage().await.copy_lag, 3);
}

/// Asserts that finishing a `gone` marker takes the key out of the figure of the hot tier's bytes:
/// the object was counted by the listing, is deleted from hot with its cold copy still there, and
/// the figure falls by its size once the marker is cleared.
#[tokio::test]
async fn a_finished_gone_marker_takes_the_key_out_of_the_tally() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("1234")).await.unwrap();
    rig.hot.put("other", bytes("12")).await.unwrap();
    let mut leader = rig.leader("r1");
    leader.step().await;
    assert_eq!(rig.usage().await.hot_bytes, 6);

    rig.hot.delete("k").await.unwrap();
    rig.cold.put("k", bytes("1234")).await.unwrap();
    rig.plant(gone("k")).await;
    rig.pass(Duration::from_secs(20));
    leader.step().await;
    rig.pass(Duration::from_secs(120));
    assert_eq!(leader.step().await, report(1, 0));
    rig.pass(Duration::from_secs(20));
    leader.step().await;
    assert_eq!(rig.usage().await.hot_bytes, 2);
}

/// Asserts the tally's rule for a deleted key: the size counted for it (by the copies since the
/// last listing, else by the listing) comes off the figure, behind a listing in progress as the
/// bytes that listing will end without, otherwise as the bytes that shrank; a key neither counted
/// takes nothing off, and the lagging keys of vanished markers are forgotten.
#[test]
fn the_tally_takes_a_deleted_key_off_the_figure_once() {
    use crate::entry_reconciler::usage::Tally;
    let tally = Tally::default();
    tally.start(100);
    tally.reach(Some("m"));
    tally.listed("a", 30, 150);
    tally.deleted("a");
    tally.deleted("a");
    tally.deleted("unknown");
    tally.deleted("z");
    assert_eq!(tally.finish(), (0, 30));

    tally.copied("b", 7, 150);
    tally.deleted("b");
    assert_eq!(tally.take_landed(), (7, 7));

    tally.start(0);
    tally.reach(Some("m"));
    tally.copied("c", 9, 150);
    assert_eq!(tally.finish(), (9, 0));
    tally.deleted("c");
    assert_eq!(tally.take_landed(), (0, 9));

    tally.lags("x", 4);
    tally.lags("y", 5);
    tally.keep_lagging(&std::iter::once("y".to_owned()).collect());
    assert_eq!(tally.lag(), 5);
}
