//! The leader's reap of the locks and clearings of owners that are gone, and its takeover from a
//! leader that stopped half way through a copy (5.13 and the crash rows of 7.2). The monotonic
//! clock is moved by hand, and registrations are written the way a heartbeat writes them.

use super::*;

/// What a reap leaves of the marker `key`: its state, `gone` and `dirty`, or `None` if it was
/// removed.
type Left = Option<(MarkerState, bool, bool)>;

const DIRTY: MarkerState = MarkerState::Dirty;

/// Asserts the table of 5.13 for a lock whose owner has no registration: an overwrite, a cold
/// landing and a reason this release does not know go to `DIRTY`, a delete goes to `DIRTY` keeping
/// the `gone` it had (false: it never decided, true: it decided), an eviction and a copy-back owe
/// nothing and go to `CLEARING` and are removed in the same pass; `gone` is kept in every case.
#[tokio::test]
async fn a_dead_owner_s_lock_is_reaped_to_the_state_its_why_requires() {
    let cases: [(MarkerWhy, bool, Left); 9] = [
        (MarkerWhy::Overwrite, false, Some((DIRTY, false, true))),
        (MarkerWhy::Overwrite, true, Some((DIRTY, true, true))),
        (MarkerWhy::Delete, true, Some((DIRTY, true, true))),
        (MarkerWhy::Delete, false, Some((DIRTY, false, true))),
        (MarkerWhy::ColdLand, false, Some((DIRTY, false, true))),
        (
            MarkerWhy::Other("MOVE".to_owned()),
            false,
            Some((DIRTY, false, true)),
        ),
        (MarkerWhy::Evict, false, None),
        (MarkerWhy::CopyBack, false, None),
        (MarkerWhy::Evict, true, None),
    ];
    for (why, gone, left) in cases {
        let rig = Rig::new();
        rig.activate().await;
        rig.plant(lock("k", why.clone(), "p-dead", gone)).await;
        let mut leader = rig.leader("r1");
        assert_eq!(leader.step().await, report(1, 0), "{why:?} {gone}");
        assert_eq!(rig.shape("k").await, left, "{why:?} {gone}");
    }
}

/// Asserts that an overwrite lock of a dead owner over a deleted key is reaped to `DIRTY` with
/// `gone` still true, and that the leader then goes on with the tombstone (cold is deleted), so
/// that the deleted cold object is never resurrected.
#[tokio::test]
async fn a_reaped_overwrite_lock_over_a_gone_marker_keeps_gone() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Overwrite, "p-dead", true))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, Some((DIRTY, true, true)));
    assert_eq!(leader.step().await, report(1, 0));
    assert!(rig.cold_body("k").await.is_none());
    assert_eq!(rig.shape("k").await, Some((DIRTY, true, true)));
}

/// Asserts the crash of a delete before it decided `gone` (the hot object was never touched): the
/// marker is reaped to `DIRTY(gone = false)` and the copier then copies hot over cold, so the
/// delete is rolled back and nothing was observed.
#[tokio::test]
async fn a_crash_after_the_delete_lock_before_the_head_changes_nothing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("kept")).await.unwrap();
    rig.cold.put("k", bytes("older")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Delete, "p-dead", false))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, Some((DIRTY, false, true)));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_body("k").await, Some(bytes("kept")));
    assert_eq!(rig.cold_body("k").await, Some(bytes("kept")));
}

/// Asserts the crash of a delete after its `gone` write and before the hot DELETE: hot is still
/// there, so the key rolls back, `gone` is kept until cold holds the hot bytes, and the hot object
/// is never deleted by the leader.
#[tokio::test]
async fn a_crash_after_the_gone_cas_before_the_hot_delete_rolls_back() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("kept")).await.unwrap();
    rig.cold.put("k", bytes("older")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Delete, "p-dead", true))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("kept")));
    assert_eq!(rig.shape("k").await, Some((DIRTY, true, true)));
    rig.pass(Duration::from_secs(120));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_body("k").await, Some(bytes("kept")));
    assert_eq!(rig.hot_calls.calls(Op::Delete), 0);
}

/// Asserts the crash of a delete after the hot DELETE and before the answer: hot is gone, so the
/// delete rolls forward: cold is deleted, the hold runs, and the marker is cleared.
#[tokio::test]
async fn a_crash_after_the_hot_delete_before_the_ack_rolls_forward_on_cold() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("deleted")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Delete, "p-dead", true))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(leader.step().await, report(1, 0));
    assert!(rig.cold_body("k").await.is_none());
    rig.pass(Duration::from_secs(120));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a crash holding the eviction lock before the hot DELETE keeps hot: the lock is
/// reaped to `CLEARING` and removed, and the hot object and the cold object are untouched.
#[tokio::test]
async fn a_dead_evict_lock_is_reaped_to_clearing_and_removed_and_hot_stays() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("kept")).await.unwrap();
    rig.cold.put("k", bytes("kept")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Evict, "p-dead", false))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_body("k").await, Some(bytes("kept")));
    assert_eq!(rig.cold_body("k").await, Some(bytes("kept")));
}

/// Asserts that a marker whose state this release does not know is reaped like a lock, to `DIRTY`
/// keeping `gone`, once its owner has departed; and that one with no owner is reaped by the lease
/// only.
#[tokio::test]
async fn a_marker_of_a_state_this_release_does_not_know_is_reaped_to_dirty_keeping_gone() {
    let rig = Rig::new();
    rig.activate().await;
    let mut unknown = lock("k", MarkerWhy::Overwrite, "p-dead", true);
    unknown.st = MarkerState::Other("PARKED".to_owned());
    rig.plant(unknown).await;
    let mut ownerless = marker("o", MarkerState::Other("PARKED".to_owned()), false, true);
    ownerless.why = None;
    rig.plant(ownerless).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 1));
    assert_eq!(rig.shape("k").await, Some((DIRTY, true, true)));
    assert_eq!(
        rig.shape("o").await,
        Some((MarkerState::Other("PARKED".to_owned()), false, true))
    );
    rig.pass(Duration::from_secs(120));
    assert_eq!(leader.step().await, report(2, 0));
    assert_eq!(rig.shape("o").await, Some((DIRTY, false, true)));
}

/// Asserts that a lock of a live owner (its registration changes with every beat) is left as long
/// as the lock moves, and that a lock which the owner refreshes every `lock_refresh` is never
/// reaped, however long it is held.
#[tokio::test]
async fn a_lock_whose_owner_refreshes_is_never_reaped() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    let mut seen = rig
        .plant(lock("k", MarkerWhy::Overwrite, "p1", false))
        .await;
    let owner = rig.writer("p1");
    let mut leader = rig.leader("r1");
    for _ in 0..10 {
        rig.beat("p1", Duration::from_secs(30)).await;
        assert_eq!(leader.step().await, report(0, 1));
        seen = rig.step_marker(&owner, &seen, MarkMove::Refresh).await;
    }
    assert_eq!(rig.shape("k").await, Some((MarkerState::Lock, false, true)));
}

/// Asserts that a lock whose holder beats but never writes it again is reaped once it has stood
/// unchanged for `lock_lease` (120 s) on the leader's clock, and not a millisecond before.
#[tokio::test]
async fn a_lock_whose_holder_lives_but_does_not_refresh_is_reaped_after_lock_lease() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    rig.plant(lock("k", MarkerWhy::Overwrite, "p1", false))
        .await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 1));
    for _ in 0..3 {
        rig.beat("p1", Duration::from_secs(30)).await;
        assert_eq!(leader.step().await, report(0, 1));
    }
    rig.beat("p1", Duration::from_secs(30) - Duration::from_millis(1))
        .await;
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(rig.shape("k").await, Some((MarkerState::Lock, false, true)));
    rig.beat("p1", Duration::from_millis(1)).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, Some((DIRTY, false, true)));
}

/// Asserts that a `CLEARING` whose owner stopped before it deleted the marker is deleted by the
/// leader at once if its registration is absent, by the successor with the same name or another,
/// and that one whose owner lives is left until its lease runs out.
#[tokio::test]
async fn a_crash_between_clearing_and_the_marker_delete_is_finished_by_the_successor() {
    let rig = Rig::new();
    rig.activate().await;
    let mut clearing = marker("a", MarkerState::Clearing, false, false);
    clearing.owner = Some("r1".to_owned());
    rig.plant(clearing.clone()).await;
    clearing.key = "b".to_owned();
    clearing.owner = Some("p-dead".to_owned());
    rig.plant(clearing.clone()).await;
    clearing.key = "c".to_owned();
    clearing.owner = Some("p1".to_owned());
    rig.register("p1").await;
    rig.plant(clearing).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(2, 1));
    assert_eq!(rig.shape("a").await, None);
    assert_eq!(rig.shape("b").await, None);
    assert_eq!(
        rig.shape("c").await,
        Some((MarkerState::Clearing, false, false))
    );
    rig.pass(Duration::from_secs(120));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("c").await, None);
}

/// Asserts that a leader that crashed after it wrote the cold object and before it cleared the
/// marker leaves the work to its successor, which finds cold with the same plain MD5 tag and
/// size, copies nothing, and clears the marker.
#[tokio::test]
async fn a_crash_after_the_cold_write_before_the_clear_is_finished_by_the_next_pass() {
    let rig = Rig::with_md5_tags();
    let mut first = owed(&rig, "v1", None).await;
    let error = Error::Other("the leader died".to_owned());
    rig.marks_calls.fail_next_on(Op::PutIfMatch, "k", error);
    let crashed = first.step().await;
    assert_eq!(crashed.changed, 0);
    assert_eq!(rig.cold_body("k").await, Some(bytes("v1")));
    assert_eq!(rig.shape("k").await, Some((DIRTY, false, true)));
    first.stop().await;

    let mut second = rig.leader("r2");
    assert_eq!(second.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::Put), 1);
    assert_eq!(rig.cold_calls.calls(Op::PutIfMatch), 0);
}

/// Asserts that a marker whose CAS to `CLEARING` landed and whose DELETE then failed is finished
/// by the next pass of the same leader: the marker is `CLEARING` owned by it, and is removed at
/// once, with nothing copied again.
#[tokio::test]
async fn a_marker_delete_that_failed_after_the_clear_is_finished_by_the_next_pass() {
    let rig = Rig::new();
    let mut leader = owed(&rig, "v1", None).await;
    rig.marks_calls
        .fail_next_on(Op::Delete, "k", Error::Unavailable("down".to_owned()));
    let first = leader.step().await;
    assert_eq!((first.changed, first.failed.len()), (0, 1));
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Clearing, false, false))
    );
    rig.pass(Duration::from_secs(1));
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.cold_calls.calls(Op::Put), 1);
}

/// Asserts that with the cold tier down the backlog stays: every marker stays `DIRTY`, the failed
/// copies are backed off, and the usage object's `copy_lag` is the sum of their sizes; after cold
/// is back and the delay has passed the markers clear and the lag is zero.
#[tokio::test]
async fn a_cold_tier_that_is_down_backs_the_copier_off_and_the_copy_lag_rises() {
    let rig = Rig::new();
    rig.activate().await;
    for (key, body) in [("a", "123"), ("b", "12345")] {
        rig.hot.put(key, bytes(body)).await.unwrap();
        rig.plant(dirty(key)).await;
    }
    rig.cold_calls.set_down(true);
    let mut leader = rig.leader("r1");
    let down = leader.step().await;
    assert_eq!((down.changed, down.failed.len()), (0, 2));
    assert_eq!(rig.usage().await.copy_lag, 8);
    rig.cold_calls.set_down(false);
    let again = leader.step().await;
    assert_eq!((again.backed_off, again.changed), (2, 0));
    rig.pass(Duration::from_secs(20));
    assert_eq!(leader.step().await, report(2, 0));
    assert_eq!(rig.shape("a").await, None);
    assert_eq!(rig.shape("b").await, None);
    assert_eq!(rig.usage().await.copy_lag, 0);
}
