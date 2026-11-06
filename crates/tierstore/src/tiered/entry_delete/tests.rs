//! The deletes of a proxy whose format is active: what each state of the entry of the key makes a
//! delete do, what it leaves in the entry, and what it answers when a call or the registration
//! fails.
//!
//! No test asserts a time: a delete that waits for a release is in `wait_tests`; a delete here that
//! meets an entry held by a client waits out its deadline on the paused clock of the runtime and is
//! answered `Unavailable`.

use std::time::Duration;

use crate::entry::{Created, EntryOp, EntryState, Outcome, Seen, Step, Take, entry_key};
use crate::journal::LEASE_KEY;
use crate::{Backend, DeleteWait, Error, Op, Phase};

use super::super::entry_put_tests::{ACTIVE_FORMAT, Rig, body};

/// The shape of an entry: operation, state and owner.
pub(super) type Shape = Option<(EntryOp, EntryState, Option<String>)>;

pub(super) fn is_unavailable<T: std::fmt::Debug>(answer: &Result<T, Error>) -> bool {
    matches!(answer, Err(Error::Unavailable(_)))
}

/// The shape of a delete entry that was handed over.
pub(super) fn handed_over() -> Shape {
    Some((EntryOp::Del, EntryState::Pending, None))
}

/// The shape of an entry held by `owner`.
fn held(op: EntryOp, owner: &str) -> Shape {
    Some((op, EntryState::Held, Some(owner.to_owned())))
}

/// Puts `key` in both tiers.
pub(super) async fn seed_both(rig: &Rig, key: &str) {
    rig.hot_mem.put(key, body("v")).await.unwrap();
    rig.cold_mem.put(key, body("v")).await.unwrap();
}

/// Makes `holder` the holder of the lease, as the first line of the lease object says.
pub(super) async fn seed_lease(rig: &Rig, holder: &str) {
    let text = format!("{holder}\nnonce\n0");
    rig.hot_mem.put(LEASE_KEY, body(&text)).await.unwrap();
}

/// What the reconciler `other`, holding the lease, has done when it copies the object of `key`:
/// the entry is `(PUT, HELD, other)`.
pub(super) async fn held_copy(rig: &Rig, key: &str) -> Seen {
    let other = rig.other();
    let Created::Taken(pending) = other.create(key, Take::Stray).await.unwrap() else {
        panic!("the entry of {key} exists");
    };
    let Outcome::Done(held) = other.advance(&pending, Step::HoldForCopy).await.unwrap() else {
        panic!("the hold of {key} was lost");
    };
    held
}

/// What the reconciler `other` does with a handed-over delete: it seals the entry, deletes both
/// tiers' objects and removes the entry.
pub(super) async fn reconcile_delete(rig: &Rig, key: &str) {
    let other = rig.other();
    let seen = rig.entry(key).await.unwrap();
    let Outcome::Done(sealed) = other.advance(&seen, Step::Seal).await.unwrap() else {
        panic!("the seal of {key} was lost");
    };
    let _ = rig.cold_mem.delete(key).await;
    let _ = rig.hot_mem.delete(key).await;
    assert_eq!(other.release(&sealed).await.unwrap(), Outcome::Done(()));
}

/// Asserts that a delete of a key that has no entry removes the object from both tiers and removes
/// its own entry.
#[tokio::test(start_paused = true)]
async fn a_delete_with_no_entry_removes_both_tiers_and_its_entry() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.proxy.delete("k").await.unwrap();
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a delete of a key that is nowhere is acknowledged and leaves no entry.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_missing_key_is_acknowledged_and_leaves_no_entry() {
    let rig = Rig::active().await;
    rig.proxy.delete("k").await.unwrap();
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a delete of a key whose put landed (pending entry) amends the entry, removes both
/// tiers' objects, hands the entry over as `(DEL, PENDING)` and is acknowledged without waiting.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_landed_put_amends_its_entry_and_hands_it_over_without_waiting() {
    let rig = Rig::active().await;
    rig.proxy.put("k", body("v")).await.unwrap();
    rig.cold_mem.put("k", body("v")).await.unwrap();
    rig.proxy.delete("k").await.unwrap();
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a delete of a key with a requested copy-back (pending) amends the entry, removes
/// the object and hands the entry over, without waiting.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_requested_copy_back_amends_its_entry_and_hands_it_over() {
    let rig = Rig::active().await;
    rig.cold_mem.put("k", body("v")).await.unwrap();
    let made = rig.other().create("k", Take::CopyBack).await.unwrap();
    assert!(matches!(made, Created::Taken(_)));
    rig.proxy.delete("k").await.unwrap();
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a delete that finds a put held by a client (not the reconciler) is answered
/// `Unavailable` and changes neither the entry nor the objects.
#[tokio::test(start_paused = true)]
async fn a_delete_is_refused_while_a_client_holds_a_put_of_the_key() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.other().create("k", Take::Put).await.unwrap();
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held(EntryOp::Put, "other"));
    assert!(rig.hot_has("k").await);
    assert!(rig.cold_has("k").await);
}

/// Asserts that a put held by an owner that is not the holder of the lease is a client's, so a
/// delete is refused.
#[tokio::test(start_paused = true)]
async fn a_delete_is_refused_for_a_held_put_whose_owner_is_not_the_lease_holder() {
    let rig = Rig::active().await;
    seed_lease(&rig, "someone-else").await;
    held_copy(&rig, "k").await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held(EntryOp::Put, "other"));
}

/// Asserts that a delete whose read of the lease fails is answered with that failure and changes
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_delete_that_cannot_read_the_lease_is_answered_with_the_failure() {
    let rig = Rig::active().await;
    held_copy(&rig, "k").await;
    let failure = Error::Unavailable("lease unreadable".to_owned());
    rig.hot_faults
        .fail_next_on(Op::Get, LEASE_KEY, failure.clone());
    let answer = rig.proxy.delete("k").await;
    assert_eq!(answer, Err(failure));
    assert_eq!(rig.shape("k").await, held(EntryOp::Put, "other"));
}

/// Asserts that a delete that finds an entry in a state this release does not know (a newer
/// writer's) is answered `Unavailable` at once and changes nothing.
#[tokio::test(start_paused = true)]
async fn a_delete_is_refused_while_the_entry_is_in_a_state_this_release_does_not_know() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let text =
        r#"{"v":2,"key":"k","op":"PUT","st":"FROZEN","owner":null,"nonce":"n","created_ms":9}"#;
    rig.hot_mem.put(&entry_key("k"), body(text)).await.unwrap();
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.cold_has("k").await);
    assert!(rig.hot_has("k").await);
}

/// Asserts that a delete that finds a delete held by another client is answered `Unavailable` and
/// changes nothing.
#[tokio::test(start_paused = true)]
async fn a_delete_is_refused_while_another_client_holds_a_delete_of_the_key() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.other().create("k", Take::Del).await.unwrap();
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held(EntryOp::Del, "other"));
    assert!(rig.cold_has("k").await);
}

/// Asserts that a delete that meets this proxy's own put in flight is answered `Unavailable`, and
/// that the put then lands whole.
#[tokio::test(start_paused = true)]
async fn a_delete_is_refused_while_this_proxy_is_putting_the_key() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(gate.release());
    put.await.unwrap().unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a delete whose proxy is not live is answered `Unavailable` before it reads or
/// writes anything.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_proxy_that_is_not_live_touches_nothing() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.stop_being_live();
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, None);
    assert!(rig.hot_has("k").await);
    assert!(rig.cold_has("k").await);
}

/// Asserts that a delete whose registration lapses after it took its entry deletes nothing, is
/// answered `Unavailable`, and cannot hand the entry over (it stays `(DEL, HELD)` for the
/// reconciler to reap).
#[tokio::test(start_paused = true)]
async fn a_delete_whose_registration_lapses_after_it_took_its_entry_deletes_nothing() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let gate = rig.hot.gate(Op::Put, "_entries/", Phase::Post);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.hot_has("k").await);
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, held(EntryOp::Del, "p1"));
}

/// Asserts that a delete whose registration lapses after it removed the objects and its entry is
/// not acknowledged: the objects are gone, and the answer is `Unavailable`.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_registration_lapses_before_its_answer_is_not_acknowledged() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let gate = rig.hot.gate(Op::DeleteIfMatch, "_entries/", Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
}

/// Asserts that a delete whose cold call fails is answered with the failure, leaves the hot
/// object (the cold tier is first), and hands its entry over.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_cold_call_fails_hands_its_entry_over_and_keeps_the_hot_object() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = Error::Unavailable("cold down".to_owned());
    rig.cold.fail_next_on(Op::Delete, "k", failure.clone());
    let answer = rig.proxy.delete("k").await;
    assert_eq!(answer, Err(failure));
    assert!(rig.hot_has("k").await);
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a delete whose hot call fails is answered with the failure, has removed the cold
/// object, and hands its entry over.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_hot_call_fails_hands_its_entry_over_after_the_cold_delete() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = Error::Unavailable("hot down".to_owned());
    rig.hot_faults
        .fail_next_on(Op::Delete, "k", failure.clone());
    let answer = rig.proxy.delete("k").await;
    assert_eq!(answer, Err(failure));
    assert!(rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a delete whose cold call does not answer within the margin is cut, answered
/// `Unavailable`, and hands its entry over; the call never reached the store.
#[tokio::test(start_paused = true)]
async fn a_delete_call_that_does_not_answer_within_the_margin_is_cut_and_handed_over() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.cold
        .set_delay(Op::Delete, Some(Duration::from_secs(3600)));
    let answer = rig.proxy.delete("k").await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.cold_has("k").await);
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a fresh delete that cannot remove its entry hands it over and is acknowledged:
/// the objects are gone.
#[tokio::test(start_paused = true)]
async fn a_fresh_delete_that_cannot_remove_its_entry_hands_it_over_and_is_acknowledged() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = Error::Unavailable("entry delete failed".to_owned());
    rig.hot_faults
        .fail_next_on(Op::DeleteIfMatch, "_entries/", failure);
    rig.proxy.delete("k").await.unwrap();
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, handed_over());
}

/// Asserts that a delete that finds the entry gone between its create and its read creates it
/// again and is acknowledged.
#[tokio::test(start_paused = true)]
async fn an_entry_that_vanishes_between_a_deletes_create_and_read_is_looked_for_again() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.other().create("k", Take::Put).await.unwrap();
    let gate = rig.hot.gate(Op::Get, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    assert_eq!(rig.other().release(&seen).await.unwrap(), Outcome::Done(()));
    assert!(gate.release());
    delete.await.unwrap().unwrap();
    assert_eq!(rig.shape("k").await, None);
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a delete allowed one look at the entry, which finds it gone, is answered
/// `Unavailable`.
#[tokio::test(start_paused = true)]
async fn a_delete_allowed_one_look_at_a_vanishing_entry_is_answered_unavailable() {
    let rig = Rig::build_waiting(ACTIVE_FORMAT, 1, DeleteWait::new()).await;
    seed_both(&rig, "k").await;
    rig.other().create("k", Take::Put).await.unwrap();
    let gate = rig.hot.gate(Op::Get, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    assert_eq!(rig.other().release(&seen).await.unwrap(), Outcome::Done(()));
    assert!(gate.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert!(rig.hot_has("k").await);
}

/// Asserts that a delete whose amend lost to a reconciler that took the entry for its copy in
/// between its read and its write looks again, finds a put held by an owner that is not the lease
/// holder, and is answered `Unavailable` with the entry as the other actor left it.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_amend_lost_looks_again_at_the_entry() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    rig.other().create("k", Take::Stray).await.unwrap();
    let gate = rig.hot.gate(Op::Get, &entry_key("k"), Phase::Post);
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    let hold = rig.other().advance(&seen, Step::HoldForCopy).await.unwrap();
    assert!(matches!(hold, Outcome::Done(_)));
    assert!(gate.release());
    let answer = delete.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held(EntryOp::Put, "other"));
    assert!(rig.cold_has("k").await);
}

/// Asserts the defaults of the wait of a delete: 30 seconds, from a sleep of 1 second doubling to
/// 10 seconds.
#[test]
fn the_wait_of_a_delete_defaults_to_30_seconds_with_sleeps_from_1_to_10_seconds() {
    let wait = DeleteWait::default();
    assert_eq!(wait.wait, Duration::from_secs(30));
    assert_eq!(wait.poll_min, Duration::from_secs(1));
    assert_eq!(wait.poll_max, Duration::from_secs(10));
}
