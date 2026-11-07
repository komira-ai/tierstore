//! The leader's pass over the markers under the pause of a rollback from `active-marks`: the
//! record `drain` that still names the marks bucket (`tierstore format-epoch --drain` over
//! `active-marks`), under which writes are paused and the leader finishes the markers.

use super::*;

/// Asserts that the pass over the markers goes on under `drain` when the record still names the
/// marks bucket the leader has: a `DIRTY` marker is copied to cold and cleared, and a `gone` marker
/// is carried to its end; and that it does nothing under `drain` that names no bucket (the pause
/// before the activation) or another bucket, nor with `drained`, which ends the rollback.
#[tokio::test]
async fn the_markers_are_worked_under_a_drain_that_names_the_marks_bucket_and_not_otherwise() {
    let rig = Rig::new();
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant(dirty("k")).await;
    let mut leader = rig.leader("r1");
    for (phase, bucket) in [
        (FormatPhase::Drain, None),
        (FormatPhase::Drain, Some("another-marks")),
        (FormatPhase::Drained, Some(BUCKET)),
    ] {
        rig.format(phase, bucket).await;
        assert_eq!(leader.step().await, StepReport::default(), "{phase:?}");
    }
    assert!(rig.marks_calls.call_log().is_empty());
    assert_eq!(rig.cold_body("k").await, None);

    rig.format(FormatPhase::Drain, Some(BUCKET)).await;
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v")));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that the reap of a lock whose owner is gone also happens under the rollback, so that a
/// writer that died mid-write does not keep the marks bucket from emptying: the lock is reaped to
/// `DIRTY` and the next pass copies it away.
#[tokio::test]
async fn a_lock_of_a_departed_owner_is_reaped_and_copied_under_the_rollback() {
    let rig = Rig::new();
    rig.format(FormatPhase::Drain, Some(BUCKET)).await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant(lock("k", MarkerWhy::Overwrite, "gone-proxy", false))
        .await;
    let mut leader = rig.leader("r1");
    let mut shapes = Vec::new();
    for _ in 0..6 {
        rig.pass(Duration::from_secs(200));
        leader.step().await;
        shapes.push(rig.shape("k").await);
    }
    assert_eq!(shapes.last(), Some(&None));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v")));
}
