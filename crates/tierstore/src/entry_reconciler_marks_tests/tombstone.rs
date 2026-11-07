//! The leader's waits over the markers that have no hot object behind them: the tombstone hold of
//! a `gone` marker (5.9) and the orphan wait of a marker that is not `gone`, and the hold after a
//! copy over a cold object that a delete had left behind. The monotonic clock is moved by hand.

use super::*;

const HOLD: Duration = Duration::from_secs(120);

/// Asserts the tombstone of a `gone` marker with no hot object: the first pass deletes the cold
/// object and leaves the marker (so that no reader is served the deleted cold copy meanwhile),
/// passes before `tombstone_hold` has run out do not touch it, and the pass at the hold finds hot
/// still absent and cold absent and clears the marker.
#[tokio::test]
async fn a_gone_marker_with_no_hot_object_deletes_cold_holds_for_the_tombstone_hold_and_clears() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");

    assert_eq!(leader.step().await, report(1, 0));
    assert!(rig.cold_body("k").await.is_none());
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    let calls = rig.cold_calls.call_log().len();

    rig.pass(HOLD - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    assert_eq!(rig.cold_calls.call_log().len(), calls);

    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::Delete), 1);
    assert_eq!(rig.cold_calls.calls(Op::Put), 0);
}

/// Asserts that a tombstone inside its hold costs the store no request at all: after the pass
/// that deleted the cold object, every pass before `tombstone_hold` has run out reads neither the
/// marker nor hot nor cold for the key (the listing already gave the version the hold was noted
/// under), however many passes there are, and each still counts the marker as pinned. Catches a
/// leader that re-read the marker and HEADed hot for every gone marker on every pass of the hold.
#[tokio::test]
async fn a_tombstone_inside_its_hold_costs_no_request_per_pass() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    let requests = || {
        (
            rig.marks_calls.calls_on(Op::Get, "k"),
            rig.hot_calls.calls_on(Op::Head, "k"),
            rig.cold_calls.call_log().len(),
        )
    };
    let before = requests();

    for _ in 0..11 {
        rig.pass(Duration::from_secs(10));
        assert_eq!(leader.step().await, report(0, 1));
    }

    assert_eq!(requests(), before);
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
}

/// Asserts that the work of a tombstone is done once, when the hold ends, and not before: over
/// the whole life of a deleted key the leader reads the marker twice (the pass that starts the
/// hold and the pass that ends it), HEADs hot twice and cold once (at the end), deletes cold once,
/// and clears the marker.
#[tokio::test]
async fn a_tombstone_is_checked_once_when_its_hold_ends() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    for _ in 0..11 {
        rig.pass(Duration::from_secs(10));
        assert_eq!(leader.step().await, report(0, 1));
    }

    rig.pass(Duration::from_secs(10));
    assert_eq!(leader.step().await, report(1, 0));

    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.marks_calls.calls_on(Op::Get, "k"), 2);
    assert_eq!(rig.hot_calls.calls_on(Op::Head, "k"), 2);
    assert_eq!(rig.cold_calls.calls(Op::Head), 1);
    assert_eq!(rig.cold_calls.calls(Op::Delete), 1);
}

/// Asserts that a cold object that a stalled leader published late, during the hold, is deleted
/// again at the end of it and held again: the marker is cleared only by a pass that finds cold
/// absent.
#[tokio::test]
async fn a_cold_object_that_appears_during_the_hold_is_deleted_again_and_held_again() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    rig.cold.put("k", bytes("late publish")).await.unwrap();

    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert!(rig.cold_body("k").await.is_none());
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));

    rig.pass(HOLD - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that an object that lands in hot during the hold (a put whose marker landed before the
/// delete) is found by the hot HEAD of the pass after it, copied to cold, and never taken for a
/// deleted key: hot keeps it, cold gets it, and the marker is cleared only after a second hold.
#[tokio::test]
async fn an_object_that_lands_during_the_hold_is_copied_to_cold_and_not_deleted_with_the_tombstone()
{
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    rig.hot.put("k", bytes("landed")).await.unwrap();

    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("landed")));
    assert_eq!(rig.hot_body("k").await, Some(bytes("landed")));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));

    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_body("k").await, Some(bytes("landed")));
    assert_eq!(rig.hot_body("k").await, Some(bytes("landed")));
}

/// Asserts that a marker that says `gone` with hot present is a change to copy, never a marker
/// to delete: the cold object is overwritten by tag (not deleted), hot keeps its object, the
/// marker stays, and once the hold has run out it is cleared with hot and cold holding the new
/// bytes.
#[tokio::test]
async fn a_marker_that_says_gone_with_hot_present_is_copied_not_deleted() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("recreated")).await.unwrap();
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");

    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("recreated")));
    assert_eq!(rig.cold_calls.calls(Op::Delete), 0);
    assert_eq!(rig.hot_body("k").await, Some(bytes("recreated")));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));

    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::Delete), 0);
    assert_eq!(rig.hot_calls.calls_on(Op::Delete, "k"), 0);
    assert_eq!(rig.hot_body("k").await, Some(bytes("recreated")));
    assert_eq!(rig.cold_body("k").await, Some(bytes("recreated")));
}

/// Asserts that a marker that said `gone` is held after the cold overwrite: it stays, `gone` and
/// all, until `tombstone_hold` has run out since the cold write, so that a reader that read the
/// marker before the overwrite and the old cold object after it is not outlived by the clear; a
/// pass a millisecond short of the hold leaves it, and the pass at the hold copies nothing again
/// and clears it.
#[tokio::test]
async fn a_marker_that_said_gone_is_held_after_the_cold_overwrite_before_it_is_cleared() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("recreated")).await.unwrap();
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));

    rig.pass(HOLD - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), 1);

    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), 1);
}

/// Asserts the safety of a delete that overtakes a copy (the leader's half of a delete during a
/// copy): the copier's cold write lands after the key was deleted from hot, its clear fails on the
/// marker the delete wrote, and the next passes delete the late cold object and clear the marker
/// after the hold. The deleted key is never served from cold meanwhile, because the `gone`
/// marker stands until cold is absent.
#[tokio::test]
async fn a_cold_publish_that_lands_after_a_delete_is_deleted_by_the_tombstone_pass() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v1")).await.unwrap();
    let a = rig.plant(dirty("k")).await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Post);
    let mut leader = rig.leader_over(rig.hot_calls.clone(), gated, "r1");
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;

    let deleter = rig.writer("p1");
    let locked = rig
        .step_marker(&deleter, &a, MarkMove::Lock(MarkerWhy::Delete))
        .await;
    let decided = rig.step_marker(&deleter, &locked, MarkMove::Gone).await;
    rig.hot.delete("k").await.unwrap();
    let release = MarkMove::Release { changed: true };
    rig.step_marker(&deleter, &decided, release).await;
    gate.release();

    assert_eq!(step.await, report(0, 1));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    assert_eq!(leader.step().await, report(1, 0));
    assert!(rig.cold_body("k").await.is_none());
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert!(rig.hot_body("k").await.is_none());
}

/// Asserts that a write to the marker during the hold (a put that bumped it, keeping `gone`) is a
/// new version: the hold starts over, and the pass that would have cleared the old version
/// deletes cold again and waits a whole hold more.
#[tokio::test]
async fn a_marker_written_during_the_hold_starts_the_hold_over() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    let a = rig.plant(gone("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    let bumped = rig.writer("p1").advance(&a, MarkMove::Bump).await;
    assert!(matches!(bumped, Ok(Outcome::Done(_))));

    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    rig.pass(HOLD - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a hold which the leader never noted (a new leader found the marker) starts when
/// the leader first sees the pair, even if cold already holds the hot bytes: the leader that
/// overwrote cold may have died, and a reader may still be reading the old copy.
#[tokio::test]
async fn a_new_leader_holds_a_gone_marker_whose_cold_copy_is_already_the_hot_object() {
    let rig = Rig::with_md5_tags();
    rig.activate().await;
    rig.hot.put("k", bytes("recreated")).await.unwrap();
    rig.cold.put("k", bytes("recreated")).await.unwrap();
    rig.plant(gone("k")).await;
    let mut leader = rig.leader("r2");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
    assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), 0);
    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts the orphan wait: a `DIRTY` marker that is not `gone`, with nothing in hot behind it, is
/// left for `orphan_after` (120 s) from the pass that first saw it, with no write to anything, and
/// cleared by the first pass at or after it; cold is not touched.
#[tokio::test]
async fn a_marker_with_no_object_is_cleared_only_after_orphan_after() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("older")).await.unwrap();
    rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(HOLD - Duration::from_millis(1));
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.marks_calls.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.marks_calls.calls(Op::Delete), 0);
    rig.pass(Duration::from_millis(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_body("k").await, Some(bytes("older")));
    assert_eq!(
        rig.cold_calls.calls(Op::Delete) + rig.cold_calls.calls(Op::Put),
        0
    );
}

/// Asserts that an object that lands before the orphan clear is copied and not orphaned: the pass
/// at `orphan_after` HEADs hot, finds the object, and copies it.
#[tokio::test]
async fn an_object_that_lands_before_the_orphan_wait_ends_is_copied_not_cleared_as_an_orphan() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 1));
    rig.hot.put("k", bytes("landed")).await.unwrap();
    rig.pass(HOLD);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("landed")));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a marker rewritten during the wait (a put re-asserting its marker) starts the
/// wait over: the pass at `orphan_after` after the first sight leaves it, and the pass a whole
/// wait after the rewrite clears it.
#[tokio::test]
async fn an_orphan_marker_that_was_rewritten_starts_its_wait_over() {
    let rig = Rig::new();
    rig.activate().await;
    let a = rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(HOLD / 2);
    let bumped = rig.writer("p1").advance(&a, MarkMove::Bump).await;
    assert!(matches!(bumped, Ok(Outcome::Done(_))));
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(HOLD / 2);
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(HOLD / 2);
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a clear of an orphan loses to a write to the marker between the pass's read and
/// its CAS: the marker the writer left stays.
#[tokio::test]
async fn an_orphan_clear_that_races_a_write_to_the_marker_loses() {
    let rig = Rig::new();
    rig.activate().await;
    let a = rig.plant(dirty("k")).await;
    let gated = Arc::new(GateBackend::new(rig.hot_calls.clone()));
    let mut leader = rig.leader_over(gated.clone(), rig.cold_calls.clone(), "r1");
    assert_eq!(leader.step().await, report(0, 1));
    rig.pass(HOLD);
    let gate = gated.gate(Op::Head, "k", Phase::Post);
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;
    let bumped = rig.writer("p1").advance(&a, MarkMove::Bump).await;
    assert!(matches!(bumped, Ok(Outcome::Done(_))));
    gate.release();
    assert_eq!(step.await, report(0, 1));
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );
}

/// Asserts that the clear that ends a hold loses to a write to the marker between the pass's read
/// and its CAS (a put that bumped it): the marker the writer left stays, with `gone`, for a
/// hold of its own.
#[tokio::test]
async fn a_clear_after_the_hold_that_races_a_write_to_the_marker_loses() {
    let rig = Rig::new();
    rig.activate().await;
    let a = rig.plant(gone("k")).await;
    let gated = Arc::new(GateBackend::new(rig.cold_calls.clone()));
    let mut leader = rig.leader_over(rig.hot_calls.clone(), gated.clone(), "r1");
    assert_eq!(leader.step().await, report(1, 0));
    rig.pass(HOLD);
    let gate = gated.gate(Op::Head, "k", Phase::Post);
    let mut step = Box::pin(leader.step());
    park(&mut step, &gate).await;
    rig.step_marker(&rig.writer("p1"), &a, MarkMove::Bump).await;
    gate.release();
    assert_eq!(step.await, report(0, 1));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Dirty, true, true)));
}
