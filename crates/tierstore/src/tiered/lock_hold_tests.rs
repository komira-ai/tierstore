//! The release of a lock a put holds: tried again while the store fails it, left to the leader
//! when it keeps failing, and not tried again for a lock that has changed.

use std::time::Duration;

use super::plain_put_tests::{Rig, held, settle};
use super::overwrite::PutClaim;
use crate::{MarkMove, MarkerState, Markers, MarkersConfig, Op, Phase, RecordingSleeper};
use std::sync::Arc;

fn millis(list: &[u64]) -> Vec<Duration> {
    list.iter().map(|n| Duration::from_millis(*n)).collect()
}

/// Asserts that a release the store fails is tried again after pauses of 100 ms doubling, as the
/// last steps of a put on an entry are, and that it is not sent again blindly: the marker is read
/// first and the write is sent again only if the lock is still as it was.
#[tokio::test]
async fn a_release_that_the_store_fails_is_tried_again_after_the_doubling_pauses() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 3);
    assert!(rig.put("k", "new").await.unwrap().wrote);
    settle().await;
    let dirty = rig.marker("k").await.unwrap();
    assert_eq!(dirty.st, MarkerState::Dirty);
    assert_eq!(rig.pacer.asked(), millis(&[100, 200, 400]));
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 4);
}

/// Asserts that a release that the store fails in every one of `step_attempts` tries is left: the
/// lock stands, and the leader reaps it by its lease.
#[tokio::test]
async fn a_release_the_store_keeps_failing_is_left_to_the_leader() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 4);
    assert!(rig.put("k", "new").await.unwrap().wrote);
    settle().await;
    let stands = rig.marker("k").await.unwrap();
    assert_eq!(stands.st, MarkerState::Lock);
    assert_eq!(rig.pacer.asked(), millis(&[100, 200, 400]));
}

/// Asserts that a release which finds the marker changed (the leader reaped the lock) is not tried
/// again: one write, no pause, and the marker is the leader's.
#[tokio::test]
async fn a_release_of_a_lock_that_changed_is_not_tried_again() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let replace = rig.hot.gate(Op::PutIfMatch, "k", Phase::Pre);
    let put = tokio::spawn({
        let proxy = rig.proxy.clone();
        async move {
            proxy
                .put_plain("k", held("new"), &PutClaim::default())
                .await
        }
    });
    replace.parked().await;
    let leader = Markers::new(
        rig.marks_mem.clone(),
        "leader",
        MarkersConfig::default(),
        rig.mono.clone(),
        Arc::new(RecordingSleeper::new()),
    )
    .unwrap();
    let seen = leader.read("k").await.unwrap().unwrap();
    leader.advance(&seen, MarkMove::Reap).await.unwrap();
    let reaped = rig.marker("k").await;
    assert!(replace.release());
    assert!(put.await.unwrap().unwrap().wrote);
    settle().await;
    assert_eq!(rig.marker("k").await, reaped);
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 1);
    assert!(!rig.pacer.asked().contains(&Duration::from_millis(100)));
}

/// Asserts that a release the store refuses outright (it is full, say) is tried again as one it
/// fails without a word is: the next try lands and the lock ends `DIRTY`.
#[tokio::test]
async fn a_release_the_store_refuses_is_tried_again() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let full = crate::Error::Full("k".to_owned());
    rig.marks_faults.fail_next_with(Op::PutIfMatch, full);
    assert!(rig.put("k", "new").await.unwrap().wrote);
    settle().await;
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
    assert_eq!(rig.pacer.asked(), millis(&[100]));
}
