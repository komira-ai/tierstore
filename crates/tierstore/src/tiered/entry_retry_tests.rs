//! The retry of the last step of a put or a delete (`entry_retry.rs`): what a failed landing,
//! hand-over or removal of an entry becomes when the step is tried again, and what the put is
//! answered.
//!
//! The proxy sleeps through a `RecordingSleeper`, so no test waits or asserts a time: the sleeps
//! are asserted as the values the proxy asked for. A failure is placed on a call by the fault
//! injector (the store did not see the call) or by a gate after the call (the store did it and
//! the reply is lost), and the order of two calls is fixed by gates, not by sleeps. A read of an
//! entry is one tagged get (`Entries::read_path`), so a failed read-back is a failure placed on
//! the [`Op::Get`] of the entry.

use std::time::Duration;

use futures::StreamExt;
use futures::stream;

use crate::entry::{EntryOp, EntryState, Step, entry_key};
use crate::{Backend, DeleteWait, Error, Op, Phase, StepRetry};

use super::entry_put_tests::{ACTIVE_FORMAT, Rig, body, held_by, pending};

fn ms(count: u64) -> Duration {
    Duration::from_millis(count)
}

fn down(text: &str) -> Error {
    Error::Unavailable(text.to_owned())
}

/// The shape of an entry a delete handed over.
fn handed_over() -> Option<(EntryOp, EntryState, Option<String>)> {
    Some((EntryOp::Del, EntryState::Pending, None))
}

/// Puts `key` in both tiers.
async fn seed_both(rig: &Rig, key: &str) {
    rig.hot_mem.put(key, body("v")).await.unwrap();
    rig.cold_mem.put(key, body("v")).await.unwrap();
}

/// Plans `count` failures of `op` on the entries, one for each try.
fn fail_entries(rig: &Rig, op: Op, count: u32) {
    for round in 0..count {
        let error = down(&format!("try {round}"));
        rig.hot_faults.fail_next_on(op, "_entries/", error);
    }
}

/// The defaults of the retry: four tries, after sleeps of 100 ms, 200 ms and 400 ms, the longest
/// being 2 seconds.
#[test]
fn the_retry_of_a_step_defaults_to_four_tries_from_100_milliseconds() {
    let retry = StepRetry::default();
    assert_eq!(retry, StepRetry::new());
    assert_eq!(retry.attempts, 4);
    assert_eq!(retry.delay, ms(100));
    assert_eq!(retry.delay_max, Duration::from_secs(2));
}

/// Asserts that a landing the hot tier failed once (the store did not see it) is sent again after
/// one sleep and lands: the put is acknowledged, the entry is pending, and the replace was made
/// twice. Before the retry this put was answered 503 and left its entry held.
#[tokio::test]
async fn a_landing_that_fails_once_is_tried_again_and_the_put_is_acknowledged() {
    let rig = Rig::active().await;
    fail_entries(&rig, Op::PutIfMatch, 1);
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 2);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that a streamed put and a presigned put retry their landing alike.
#[tokio::test]
async fn a_streamed_put_and_a_presigned_put_retry_their_landing() {
    let rig = Rig::active().await;
    let chunks = super::entry_put_tests::chunks;
    fail_entries(&rig, Op::PutIfMatch, 1);
    let streamed = rig.proxy.put_stream("s", Some(1), chunks(&["v"])).await;
    streamed.unwrap();
    fail_entries(&rig, Op::PutIfMatch, 1);
    let presigned = rig.proxy.put_stream_hot_only("p", Some(1), chunks(&["v"]));
    presigned.await.unwrap();
    assert_eq!(rig.shape("s").await, pending());
    assert_eq!(rig.shape("p").await, pending());
    assert_eq!(rig.sleeper.asked(), vec![ms(100), ms(100)]);
}

/// Asserts that a landing the store did and whose reply was lost, with the read-back failing too,
/// is found by the read before the retry: the entry carries the nonce of the replace, so the put
/// is acknowledged and the replace is made once, not twice.
#[tokio::test]
async fn a_landing_that_landed_and_answered_an_error_is_found_by_the_read_and_not_repeated() {
    let rig = Rig::active().await;
    let entry = entry_key("k");
    let gate = rig.hot.gate(Op::PutIfMatch, &entry, Phase::Post);
    rig.hot_faults
        .fail_next_on(Op::Get, &entry, down("read-back down"));
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    assert!(gate.release_with(down("reply lost")));
    put.await.unwrap().unwrap();
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry), 1);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that when the read-back and the read before the retry both fail, the proxy sleeps
/// again, reads again, finds the entry as it was (the first replace never landed), sends the
/// replace once more, and acknowledges the put.
#[tokio::test]
async fn a_landing_whose_reads_fail_twice_is_read_again_before_it_is_sent_again() {
    let rig = Rig::active().await;
    let entry = entry_key("k");
    fail_entries(&rig, Op::PutIfMatch, 1);
    rig.hot_faults
        .fail_next_on(Op::Get, &entry, down("first read down"));
    rig.hot_faults
        .fail_next_on(Op::Get, &entry, down("second read down"));
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry), 2);
    assert_eq!(rig.sleeper.asked(), vec![ms(100), ms(200)]);
}

/// Asserts that a landing that fails in every try stops at `step_attempts`: the put is answered
/// `Unavailable` with the last error of the store, the replace was made four times, the sleeps
/// are 100, 200 and 400 ms, and the entry stays held for the reconciler.
#[tokio::test]
async fn a_landing_that_fails_in_every_try_stops_at_the_limit_and_leaves_the_entry_held() {
    let rig = Rig::active().await;
    fail_entries(&rig, Op::PutIfMatch, 4);
    let answer = rig.proxy.put("k", body("v")).await;
    assert_eq!(answer, Err(down("try 3")));
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 4);
    assert_eq!(rig.sleeper.asked(), vec![ms(100), ms(200), ms(400)]);
    assert_eq!(rig.shape("k").await, held_by("p1"));
}

/// Asserts the keys: a sleep doubles up to `delay_max` and stays there, and `attempts` is the
/// number of tries, the first included.
#[tokio::test]
async fn the_sleeps_double_up_to_the_longest_and_the_tries_are_the_attempts() {
    let retry = StepRetry {
        attempts: 5,
        delay: ms(100),
        delay_max: ms(250),
    };
    let rig = Rig::build_retrying(ACTIVE_FORMAT, 3, DeleteWait::new(), retry).await;
    fail_entries(&rig, Op::PutIfMatch, 5);
    assert!(rig.proxy.put("k", body("v")).await.is_err());
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 5);
    assert_eq!(
        rig.sleeper.asked(),
        vec![ms(100), ms(200), ms(250), ms(250)]
    );
}

/// Asserts that a landing the store refuses (an error that is not ambiguous) is not tried again:
/// one replace, no sleep, the entry held.
#[tokio::test]
async fn a_landing_the_store_refuses_is_not_tried_again() {
    let rig = Rig::active().await;
    let refused = Error::Full("x".to_owned());
    rig.hot_faults
        .fail_next_on(Op::PutIfMatch, "_entries/", refused);
    let answer = rig.proxy.put("k", body("v")).await;
    assert!(
        matches!(&answer, Err(Error::Unavailable(text)) if text.contains("could not be written")),
        "{answer:?}"
    );
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 1);
    assert!(rig.sleeper.asked().is_empty());
    assert_eq!(rig.shape("k").await, held_by("p1"));
}

/// Asserts that a retry whose entry was reaped to pending in between is not an acknowledgement:
/// the second replace names the old tag and is lost, the put is answered `Unavailable` ("the
/// entry changed"), and the entry is the reaped one.
#[tokio::test]
async fn a_landing_whose_entry_was_reaped_before_the_retry_is_not_acknowledged() {
    let rig = Rig::active().await;
    let entry = entry_key("k");
    let first = rig.hot.gate(Op::PutIfMatch, &entry, Phase::Pre);
    let second = rig.hot.gate(Op::PutIfMatch, &entry, Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    first.parked().await;
    assert!(first.release_with(down("not sent")));
    second.parked().await;
    let held = rig.entry("k").await.unwrap();
    rig.other().advance(&held, Step::Reap).await.unwrap();
    assert!(second.release());
    let answer = put.await.unwrap();
    assert!(
        matches!(&answer, Err(Error::Unavailable(text)) if text.contains("the entry changed")),
        "{answer:?}"
    );
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that no retry is made once the registration no longer allows a call to start: the
/// landing fails, the proxy stops being live, and the put is answered `Unavailable` after one
/// replace and no sleep.
#[tokio::test]
async fn a_landing_is_not_tried_again_once_the_proxy_is_not_live() {
    let rig = Rig::active().await;
    let gate = rig.hot.gate(Op::PutIfMatch, &entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release_with(down("not sent")));
    let answer = put.await.unwrap();
    assert_eq!(answer, Err(down("not sent")));
    assert!(rig.sleeper.asked().is_empty());
    assert_eq!(rig.shape("k").await, held_by("p1"));
}

/// Asserts that the removal of the entry of a put that failed before landing is tried again: the
/// body fails, the first removal fails, the second removes the entry, and the put is answered
/// with the body's error.
#[tokio::test]
async fn the_removal_of_the_entry_of_a_failed_put_is_tried_again() {
    let rig = Rig::active().await;
    fail_entries(&rig, Op::DeleteIfMatch, 1);
    let items = vec![Ok(body("a")), Err(Error::Other("reset".to_owned()))];
    let put = rig
        .proxy
        .put_stream("k", None, stream::iter(items).boxed())
        .await;
    assert_eq!(put, Err(Error::Other("reset".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that a removal that fails in every try stops at the limit and leaves the entry held.
#[tokio::test]
async fn the_removal_of_an_entry_that_fails_in_every_try_stops_at_the_limit() {
    let rig = Rig::active().await;
    fail_entries(&rig, Op::DeleteIfMatch, 4);
    rig.cold.set_down(true);
    let answer = rig.proxy.put("k", body("v")).await;
    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    let removals = rig.hot_faults.calls_on(Op::DeleteIfMatch, &entry_key("k"));
    assert_eq!(removals, 4);
    assert_eq!(rig.sleeper.asked(), vec![ms(100), ms(200), ms(400)]);
    assert_eq!(rig.shape("k").await, held_by("p1"));
}

/// Asserts that a removal the store did and whose reply was lost, with the read-back failing too,
/// is sent again, finds nothing at the tag and is taken as done: the delete is acknowledged, its
/// entry is gone, and it was not handed over.
#[tokio::test(start_paused = true)]
async fn a_removal_that_landed_and_answered_an_error_is_taken_as_done() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let entry = entry_key("k");
    let gate = rig.hot.gate(Op::DeleteIfMatch, &entry, Phase::Post);
    rig.hot_faults
        .fail_next_on(Op::Get, &entry, down("read-back down"));
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    assert!(gate.release_with(down("reply lost")));
    delete.await.unwrap().unwrap();
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_faults.calls_on(Op::DeleteIfMatch, &entry), 2);
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry), 0);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that the removal of the entry of a fresh delete that fails once is tried again and the
/// entry is removed, not handed over to the reconciler.
#[tokio::test(start_paused = true)]
async fn a_fresh_delete_whose_removal_fails_once_removes_its_entry_on_the_next_try() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    fail_entries(&rig, Op::DeleteIfMatch, 1);
    rig.proxy.delete("k").await.unwrap();
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 0);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that a hand-over the hot tier failed once is sent again and lands: the delete is
/// answered with the failure of its hot call, and the entry is handed over, not left held.
#[tokio::test(start_paused = true)]
async fn a_hand_over_that_fails_once_is_tried_again_and_the_entry_is_handed_over() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = down("hot delete down");
    rig.hot_faults
        .fail_next_on(Op::Delete, "k", failure.clone());
    fail_entries(&rig, Op::PutIfMatch, 1);
    assert_eq!(rig.proxy.delete("k").await, Err(failure));
    assert_eq!(rig.shape("k").await, handed_over());
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 2);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that a hand-over the store did and whose reply was lost, with the read-back failing
/// too, is found by the read before the retry and not made again.
#[tokio::test(start_paused = true)]
async fn a_hand_over_that_landed_and_answered_an_error_is_not_repeated() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let entry = entry_key("k");
    let failure = down("hot delete down");
    rig.hot_faults
        .fail_next_on(Op::Delete, "k", failure.clone());
    let gate = rig.hot.gate(Op::PutIfMatch, &entry, Phase::Post);
    rig.hot_faults
        .fail_next_on(Op::Get, &entry, down("read-back down"));
    let proxy = rig.proxy.clone();
    let delete = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    assert!(gate.release_with(down("reply lost")));
    assert_eq!(delete.await.unwrap(), Err(failure));
    assert_eq!(rig.shape("k").await, handed_over());
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry), 1);
    assert_eq!(rig.sleeper.asked(), vec![ms(100)]);
}

/// Asserts that a hand-over that fails in every try stops at the limit, and the delete is still
/// answered with the failure of its own call.
#[tokio::test(start_paused = true)]
async fn a_hand_over_that_fails_in_every_try_stops_at_the_limit() {
    let rig = Rig::active().await;
    seed_both(&rig, "k").await;
    let failure = down("hot delete down");
    rig.hot_faults
        .fail_next_on(Op::Delete, "k", failure.clone());
    fail_entries(&rig, Op::PutIfMatch, 4);
    assert_eq!(rig.proxy.delete("k").await, Err(failure));
    assert_eq!(rig.hot_faults.calls_on(Op::PutIfMatch, &entry_key("k")), 4);
    assert_eq!(rig.sleeper.asked(), vec![ms(100), ms(200), ms(400)]);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Del, EntryState::Held, Some("p1".to_owned())))
    );
}
