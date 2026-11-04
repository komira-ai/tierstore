//! The deletes of a proxy whose format is active that wait for the reconciler: one that amends a
//! put the reconciler is copying, a retry that claims a delete handed over, and one that meets a
//! copy-back the reconciler has sealed.
//!
//! The reconciler is played by the test, through the calls on entries of another actor, and the
//! test decides when it acts: a gate on the delete's read of the entry holds the delete at the
//! point where it waits, so no assertion is about time. Where the reconciler never acts, the
//! runtime's clock is paused and the delete's sleeps pass at once.

use std::time::Duration;

use crate::entry::{Created, EntryOp, EntryState, Outcome, Seen, Step, Take, entry_key};
use crate::{Backend, DeleteWait, Error, Op, Phase};

use super::super::entry_put_tests::{ACTIVE_FORMAT, Rig, body};
use super::tests::{
    handed_over, held_copy, is_unavailable, reconcile_delete, seed_both, seed_lease,
};

/// Starts a delete of `k` and holds it where it has handed its entry over and is about to read it
/// to see whether the reconciler released it. Returns the delete and the gate that holds its read.
async fn delete_waiting(rig: &Rig) -> (tokio::task::JoinHandle<Result<(), Error>>, crate::Gate) {
    let before_hot = rig.hot.gate(Op::Delete, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    before_hot.parked().await;
    let poll = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    assert!(before_hot.release());
    poll.parked().await;
    (delete, poll)
}

/// Asserts the first counterexample of the design: a copy is in flight (the reconciler holds the
/// put), a delete amends it and deletes both tiers, and the copy then publishes to the cold tier.
/// The delete is not acknowledged until the reconciler has finished the handed-over delete (which
/// deletes both tiers again) and removed the entry, and after that neither tier has the object.
#[tokio::test(start_paused = true)]
async fn a_delete_that_amends_a_copy_in_flight_is_acknowledged_only_after_the_reconciler_releases()
{
    let rig = Rig::active().await;
    seed_lease(&rig, "other").await;
    rig.hot_mem.put("k", body("v")).await.unwrap();
    held_copy(&rig, "k").await;
    let (delete, poll) = delete_waiting(&rig).await;
    assert_eq!(rig.shape("k").await, handed_over());
    rig.cold_mem.put("k", body("v")).await.unwrap();
    assert!(!delete.is_finished());
    reconcile_delete(&rig, "k").await;
    assert!(poll.release());
    delete.await.unwrap().unwrap();
    assert!(!rig.cold_has("k").await);
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts the second counterexample of the design: a delete whose hot call failed hands its
/// entry over; its retry claims the entry, and is acknowledged only after the reconciler releases
/// the entry, not when the retry's own calls end.
#[tokio::test(start_paused = true)]
async fn a_retried_delete_claims_the_handed_over_entry_and_waits_for_the_release() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = Error::Unavailable("hot down".to_owned());
    rig.hot_faults
        .fail_next_on(Op::Delete, "k", failure.clone());
    assert_eq!(rig.proxy.delete("k").await, Err(failure));
    assert_eq!(rig.shape("k").await, handed_over());
    let (retry, poll) = delete_waiting(&rig).await;
    assert_eq!(rig.shape("k").await, handed_over());
    rig.cold_mem.put("k", body("v")).await.unwrap();
    assert!(!retry.is_finished());
    reconcile_delete(&rig, "k").await;
    assert!(poll.release());
    retry.await.unwrap().unwrap();
    assert!(!rig.cold_has("k").await);
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a delete whose reconciler never releases the entry reads it at each step of the
/// back-off (sleeps of 1, 2, 4, 4 and 1 seconds of a 12-second wait: six reads, and the one that
/// found the entry), and then is answered `Unavailable` with both tiers deleted and the entry
/// still `(DEL, PENDING)`.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_frozen_reconciler_is_answered_unavailable_at_the_deadline() {
    let wait = DeleteWait {
        wait: Duration::from_secs(12),
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(4),
    };
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 3, wait).await;
    seed_lease(&rig, "other").await;
    seed_both(&rig, "k").await;
    held_copy(&rig, "k").await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
    let reads = rig.hot_faults.calls_on(Op::Head, &entry_key("k"));
    assert_eq!(reads, 7);
}

/// Asserts that a sleep shorter than a millisecond is read as a millisecond: a wait of 3
/// milliseconds with a poll of zero reads the entry four times and ends.
#[tokio::test(start_paused = true)]
async fn a_poll_of_zero_still_ends_at_the_deadline() {
    let wait = DeleteWait {
        wait: Duration::from_millis(3),
        poll_min: Duration::ZERO,
        poll_max: Duration::ZERO,
    };
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 3, wait).await;
    seed_lease(&rig, "other").await;
    held_copy(&rig, "k").await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.hot_faults.calls_on(Op::Head, &entry_key("k")), 5);
}

/// Asserts that a waiting delete is acknowledged when the entry it waited on is replaced by
/// another operation's (a put took the key after the release).
#[tokio::test(start_paused = true)]
async fn a_waiting_delete_is_acknowledged_when_another_operations_entry_replaces_its_own() {
    let rig = Rig::active().await;
    seed_lease(&rig, "other").await;
    rig.hot_mem.put("k", body("v")).await.unwrap();
    held_copy(&rig, "k").await;
    let (delete, poll) = delete_waiting(&rig).await;
    reconcile_delete(&rig, "k").await;
    rig.other().create("k", Take::Put).await.unwrap();
    assert!(poll.release());
    delete.await.unwrap().unwrap();
    let shape = rig.shape("k").await;
    assert_eq!(
        shape,
        Some((EntryOp::Put, EntryState::Held, Some("other".to_owned())))
    );
}

/// Asserts that a waiting delete whose registration lapsed while it waited is not acknowledged,
/// although the reconciler released the entry.
#[tokio::test(start_paused = true)]
async fn a_waiting_delete_whose_registration_lapsed_is_not_acknowledged() {
    let rig = Rig::active().await;
    seed_lease(&rig, "other").await;
    rig.hot_mem.put("k", body("v")).await.unwrap();
    held_copy(&rig, "k").await;
    let (delete, poll) = delete_waiting(&rig).await;
    reconcile_delete(&rig, "k").await;
    rig.stop_being_live();
    assert!(poll.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a waiting delete whose read of the entry fails is answered with that failure,
/// and leaves the entry handed over.
#[tokio::test(start_paused = true)]
async fn a_waiting_delete_whose_read_of_the_entry_fails_is_answered_with_the_failure() {
    let rig = Rig::active().await;
    seed_lease(&rig, "other").await;
    rig.hot_mem.put("k", body("v")).await.unwrap();
    held_copy(&rig, "k").await;
    let (delete, poll) = delete_waiting(&rig).await;
    let failure = Error::Unavailable("down".to_owned());
    assert!(poll.release_with(failure.clone()));
    assert_eq!(delete.await.unwrap(), Err(failure));
    assert_eq!(rig.shape("k").await, handed_over());
}

/// What the reconciler `other` has done to a copy-back of `k` when it lands the copy: the cold
/// tier has the object, and the entry is `(CB, SEALED)`.
async fn sealed_copy_back(rig: &Rig) -> Seen {
    rig.cold_mem.put("k", body("v")).await.unwrap();
    let other = rig.other();
    let Created::Taken(asked) = other.create("k", Take::CopyBack).await.unwrap() else {
        panic!("the entry of k exists");
    };
    let Outcome::Done(sealed) = other.advance(&asked, Step::Seal).await.unwrap() else {
        panic!("the seal of k was lost");
    };
    sealed
}

/// Asserts that a delete that meets a sealed copy-back answers nothing and deletes nothing while
/// the entry stays, and that when the reconciler has landed the copy and removed the entry, the
/// delete takes the key afresh, removes the object from both tiers (the landed copy too) and is
/// acknowledged.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_copy_back_waits_for_the_release_and_then_deletes_both_tiers()
{
    let rig = Rig::active().await;
    let sealed = sealed_copy_back(&rig).await;
    let first = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    first.parked().await;
    let poll = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    assert!(first.release());
    poll.parked().await;
    assert!(!delete.is_finished());
    assert!(rig.cold_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Cb, EntryState::Sealed, None))
    );
    rig.hot_mem.put("k", body("v")).await.unwrap();
    assert_eq!(
        rig.other().release(&sealed).await.unwrap(),
        Outcome::Done(())
    );
    assert!(poll.release());
    delete.await.unwrap().unwrap();
    assert!(!rig.cold_has("k").await);
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a delete whose sealed copy-back is never released reads the entry at each step of
/// the back-off (the read that found it, then sleeps of 1, 2, 4, 4 and 1 seconds of a 12-second
/// wait: seven reads), and is then answered `Unavailable` with the object still in the cold tier
/// and the entry still `(CB, SEALED)`.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_copy_back_that_is_never_released_is_answered_unavailable_at_the_deadline()
 {
    let wait = DeleteWait {
        wait: Duration::from_secs(12),
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(4),
    };
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 3, wait).await;
    sealed_copy_back(&rig).await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.cold_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Cb, EntryState::Sealed, None))
    );
    assert_eq!(rig.hot_faults.calls_on(Op::Head, &entry_key("k")), 7);
}

/// Asserts that all the waits of one delete share its budget: a delete that waits seven seconds
/// of a 12-second budget on a sealed copy-back, and then claims a handed-over delete that is never
/// released, waits only the five seconds that are left for the release. It reads the entry five
/// times before the claim (the read that found the sealed entry, then after sleeps of 1, 2 and 4
/// seconds), once to claim, and three times for the release (after sleeps of 4 and 1 seconds, and
/// the one that ends it), and is answered `Unavailable` with the entry still `(DEL, PENDING)`.
/// Catches a release wait that starts its budget again at the claim, which reads the entry twelve times, not nine.
#[tokio::test(start_paused = true)]
async fn a_delete_that_waited_on_a_sealed_entry_and_then_claims_a_delete_waits_only_what_is_left() {
    let wait = DeleteWait {
        wait: Duration::from_secs(12),
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(4),
    };
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 3, wait).await;
    let sealed = sealed_copy_back(&rig).await;
    let mut gate = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    for _ in 0..4 {
        let next = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
        assert!(gate.release());
        next.parked().await;
        gate = next;
    }
    let other = rig.other();
    assert_eq!(other.release(&sealed).await.unwrap(), Outcome::Done(()));
    let Created::Taken(taken) = other.create("k", Take::Del).await.unwrap() else {
        panic!("the entry of k exists");
    };
    let Outcome::Done(_) = other.advance(&taken, Step::Handover).await.unwrap() else {
        panic!("the handover of k was lost");
    };
    assert!(gate.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.hot_faults.calls_on(Op::Head, &entry_key("k")), 9);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// The entry of `k` as the reconciler `other` leaves it while it works: the cold tier has the
/// object, and the entry is sealed with the operation `take` made (a put's or an eviction's).
async fn sealed_entry(rig: &Rig, take: Take) -> Seen {
    rig.cold_mem.put("k", body("v")).await.unwrap();
    let other = rig.other();
    let Created::Taken(made) = other.create("k", take).await.unwrap() else {
        panic!("the entry of k exists");
    };
    if made.entry.st == EntryState::Sealed {
        return made;
    }
    let Outcome::Done(sealed) = other.advance(&made, Step::Seal).await.unwrap() else {
        panic!("the seal of k was lost");
    };
    sealed
}

/// Asserts that a delete that meets a sealed entry of `take` waits through a poll while the entry
/// stays, and that once the reconciler has removed it, the delete takes the key afresh, deletes
/// both tiers and is acknowledged.
async fn waits_then_deletes(take: Take, op: EntryOp) {
    let rig = Rig::active().await;
    let sealed = sealed_entry(&rig, take).await;
    let first = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    first.parked().await;
    let poll = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    assert!(first.release());
    poll.parked().await;
    assert!(!delete.is_finished());
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, Some((op, EntryState::Sealed, None)));
    assert_eq!(
        rig.other().release(&sealed).await.unwrap(),
        Outcome::Done(())
    );
    assert!(poll.release());
    delete.await.unwrap().unwrap();
    assert!(!rig.cold_has("k").await);
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a delete whose sealed entry of `take` is never released is answered
/// `Unavailable` only at the deadline, after the reads of the back-off (seven for a 12-second wait
/// with sleeps of 1, 2, 4, 4 and 1 seconds), with the object and the entry as they were.
async fn unavailable_at_deadline(take: Take, op: EntryOp) {
    let wait = DeleteWait {
        wait: Duration::from_secs(12),
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(4),
    };
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 3, wait).await;
    sealed_entry(&rig, take).await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, Some((op, EntryState::Sealed, None)));
    assert_eq!(rig.hot_faults.calls_on(Op::Head, &entry_key("k")), 7);
}

/// Asserts that a delete that meets a sealed eviction waits for the release and then deletes both
/// tiers.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_eviction_waits_for_the_release_and_then_deletes_both_tiers() {
    waits_then_deletes(Take::Evict, EntryOp::Evict).await;
}

/// Asserts that a delete whose sealed eviction is never released is answered `Unavailable` at the
/// deadline and not before.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_eviction_that_is_never_released_is_answered_unavailable_at_the_deadline()
 {
    unavailable_at_deadline(Take::Evict, EntryOp::Evict).await;
}

/// Asserts that a delete that meets a sealed put waits for the release and then deletes both
/// tiers.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_put_waits_for_the_release_and_then_deletes_both_tiers() {
    waits_then_deletes(Take::Stray, EntryOp::Put).await;
}

/// Asserts that a delete whose sealed put is never released is answered `Unavailable` at the
/// deadline and not before.
#[tokio::test(start_paused = true)]
async fn a_delete_that_meets_a_sealed_put_that_is_never_released_is_answered_unavailable_at_the_deadline()
 {
    unavailable_at_deadline(Take::Stray, EntryOp::Put).await;
}
