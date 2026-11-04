//! The puts of a proxy whose format is active, against other actors of the same key: a second
//! put, a put that finished, a reconciler that took the entry, a proxy that stopped being live.
//!
//! Each test parks one call of the hot tier with a gate, does what the other actor does, and lets
//! the call go: the order is the test's, and nothing waits for a time.

use crate::entry::{Outcome, Step, entry_key};
use crate::{Backend, Error, Op, Phase};

use super::entry_put_tests::{Rig, body, held_by, pending};

fn is_unavailable<T: std::fmt::Debug>(answer: &Result<T, Error>) -> bool {
    matches!(answer, Err(Error::Unavailable(_)))
}

/// Asserts that of two puts of one key that meet, exactly one is acknowledged: while the first is
/// between its entry and its object, the second is answered `Unavailable` and changes nothing, the
/// first then lands and is acknowledged with its own bytes, and a third put is refused as existing.
#[tokio::test]
async fn of_two_puts_of_one_key_that_meet_exactly_one_is_acknowledged() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let first = tokio::spawn(async move { proxy.put("k", body("first")).await });
    gate.parked().await;
    let second = rig.proxy.put("k", body("second")).await;
    assert!(is_unavailable(&second), "{second:?}");
    assert_eq!(rig.shape("k").await, held_by("p1"));
    assert!(!rig.hot_has("k").await);
    assert!(gate.release());
    first.await.unwrap().unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("first"));
    assert_eq!(rig.shape("k").await, pending());
    let third = rig.proxy.put("k", body("third")).await;
    assert_eq!(third, Err(Error::AlreadyExists("k".to_owned())));
}

/// Asserts that a put that looked at the hot tier before another put finished, and then met the
/// other's pending entry, is refused as existing (the object is in the hot tier now) and does not
/// touch the other's object or entry.
#[tokio::test]
async fn a_put_that_lost_the_race_to_one_that_finished_is_refused_as_existing() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let slow = tokio::spawn(async move { proxy.put("k", body("slow")).await });
    gate.parked().await;
    rig.proxy.put("k", body("fast")).await.unwrap();
    assert!(gate.release());
    let slow = slow.await.unwrap();
    assert_eq!(slow, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("fast"));
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that a put that finds the entry of its key gone between its create and its read
/// creates it again and is acknowledged.
#[tokio::test]
async fn an_entry_that_vanishes_between_create_and_read_is_looked_for_again() {
    let rig = Rig::active().await;
    let held = rig
        .other()
        .create("k", crate::entry::Take::Put)
        .await
        .unwrap();
    assert!(matches!(held, crate::entry::Created::Taken(_)));
    let gate = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    let released = rig.other().release(&seen).await.unwrap();
    assert_eq!(released, Outcome::Done(()));
    assert!(gate.release());
    put.await.unwrap().unwrap();
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a put allowed one look at the entry, which finds it gone, is answered
/// `Unavailable` and writes nothing.
#[tokio::test]
async fn a_put_that_runs_out_of_looks_at_a_changing_entry_is_unavailable() {
    let rig = Rig::build(super::entry_put_tests::ACTIVE_FORMAT, 1).await;
    rig.other()
        .create("k", crate::entry::Take::Put)
        .await
        .unwrap();
    let gate = rig.hot.gate(Op::Head, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    rig.other().release(&seen).await.unwrap();
    assert!(gate.release());
    let answer = put.await.unwrap();
    let Err(Error::Unavailable(text)) = answer else {
        panic!("{answer:?}");
    };
    assert!(text.contains("keeps changing"), "{text}");
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a put whose proxy stops being live after its object landed is not acknowledged:
/// it is answered `Unavailable`, and its entry is left as it was, held, for the reconciler to take
/// once the proxy is gone.
#[tokio::test]
async fn a_put_that_stops_being_live_before_its_acknowledgement_leaves_its_entry_held() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held_by("p1"));
    assert!(rig.hot_has("k").await);
}

/// Asserts that a put whose proxy stops being live while the replace of its entry is with the
/// store is not acknowledged: the replace is parked after the store applied it, the monotonic
/// clock is moved past `live_until`, and when the replace answers the put is `Unavailable`; the
/// entry is pending (the store applied the replace) and the object is in the hot tier, for the
/// reconciler.
#[tokio::test]
async fn a_put_that_stops_being_live_while_its_acknowledgement_is_written_is_unavailable() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::PutIfMatch, &entry_key("k"), Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a put whose entry was taken (reaped to pending by another actor) before its
/// acknowledgement is answered `Unavailable` and does not touch the entry again.
#[tokio::test]
async fn a_put_whose_entry_was_reaped_before_its_acknowledgement_is_unavailable() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    let seen = rig.entry("k").await.unwrap();
    let reaped = rig.other().advance(&seen, Step::Reap).await.unwrap();
    assert!(matches!(reaped, Outcome::Done(_)));
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that a put whose landing was done by the store but not answered is answered
/// `Unavailable` and leaves its entry pending with the object in the hot tier, for the reconciler
/// to copy.
#[tokio::test]
async fn a_put_whose_landing_was_done_but_not_answered_hands_its_entry_over() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    assert!(gate.fail_post());
    let answer = put.await.unwrap();
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a put whose create-only write finds an object another writer made after the put
/// took its entry is refused as existing, removes its own entry and leaves the other's object.
#[tokio::test]
async fn a_put_that_finds_the_object_after_taking_its_entry_removes_the_entry() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("mine")).await });
    gate.parked().await;
    rig.hot_mem.put("k", body("theirs")).await.unwrap();
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("theirs"));
}

/// Asserts that a put that fails after its proxy stopped being live does not remove its entry
/// (a proxy that is not live destroys nothing): the put answers with its own error and the entry
/// stays held.
#[tokio::test]
async fn a_put_that_fails_when_its_proxy_is_not_live_leaves_its_entry() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("mine")).await });
    gate.parked().await;
    rig.hot_mem.put("k", body("theirs")).await.unwrap();
    rig.stop_being_live();
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.shape("k").await, held_by("p1"));
}

/// Asserts that the removal of a put's entry is cut when the hot tier does not answer it: the put
/// answers with its own error (the cold tier was down) and the entry stays held, for the
/// reconciler. The runtime's clock is paused, so the cut happens when nothing else can run.
#[tokio::test(start_paused = true)]
async fn the_removal_of_an_entry_that_does_not_answer_is_cut() {
    let rig = Rig::active().await;
    let _gate = rig.hot.gate(Op::DeleteIfMatch, &entry_key("k"), Phase::Pre);
    rig.cold.set_down(true);
    let answer = rig.proxy.put("k", body("v")).await;
    assert!(is_unavailable(&answer), "{answer:?}");
    assert_eq!(rig.shape("k").await, held_by("p1"));
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a streamed put whose hot tier answers `Full` after it took the body (it cannot be
/// offered to the cold tier again) is answered `Full` and removes its entry.
#[tokio::test]
async fn a_streamed_put_the_hot_tier_fills_after_taking_the_body_is_full_and_leaves_no_entry() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Put, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move {
        proxy
            .put_stream("k", Some(1), super::entry_put_tests::chunks(&["v"]))
            .await
    });
    gate.parked().await;
    assert!(gate.release_with(Error::Full("k".to_owned())));
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(Error::Full("k".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert!(!rig.cold_has("k").await);
}

/// Asserts that a put whose second look at the hot tier (under its entry) fails is answered with
/// that failure and removes its entry.
#[tokio::test]
async fn a_put_whose_look_at_the_hot_tier_under_its_entry_fails_removes_its_entry() {
    let rig = Rig::active().await;
    let first = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let second = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    first.parked().await;
    assert!(first.release());
    second.parked().await;
    assert!(second.release_with(Error::Unavailable("down".to_owned())));
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(Error::Unavailable("down".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a put whose first look at the hot tier found nothing, and whose entry was created
/// after an object appeared there (no entry came with it), is refused as existing under its entry,
/// removes its entry and leaves the object.
#[tokio::test]
async fn a_put_that_finds_an_object_with_no_entry_under_its_own_entry_removes_the_entry() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::Head, "k", Phase::Post);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("mine")).await });
    gate.parked().await;
    rig.hot_mem.put("k", body("theirs")).await.unwrap();
    assert!(gate.release());
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("theirs"));
}
