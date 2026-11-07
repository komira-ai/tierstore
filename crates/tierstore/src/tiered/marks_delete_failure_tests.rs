//! The delete of a key under the format `active-marks` when a call fails, is cut, answers late or
//! never comes back, and when the process that made it dies: what the client is told, and what the
//! marker, the hot tier and the cold tier hold afterwards.
//!
//! A process that dies is a delete whose future is leaked at a gate: nothing of it runs again, not
//! even its drop. The leader is not built yet, so the reap that ends such a lock is
//! `MarkMove::Reap` taken by an actor of the test's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::marks_delete_tests::{MARKS_FORMAT, Rig, dirty_shape, let_the_others_run, lock_shape};
use crate::faulty::Op;
use crate::{Backend, Clock, Error, MarkerWhy, Phase, ProxyConfig};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn is_slow_down<T: std::fmt::Debug>(answer: &Result<T, Error>) -> bool {
    matches!(answer, Err(Error::SlowDown(_)))
}

/// Asserts that a delete whose hot DELETE is cut at the margin of the registration answers
/// `SlowDown` at once, and keeps its lock, which says `gone`, for `queued_call_margin` (60 s):
/// while it does, the hot object is there, a second delete of the key waits and is `SlowDown`, and
/// when the margin is over the lock is released to `DIRTY`, keeping `gone`.
#[tokio::test(start_paused = true)]
async fn a_delete_whose_hot_delete_times_out_keeps_its_lock_until_the_queued_call_is_gone() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.hot_faults.set_delay(Op::Delete, Some(secs(30)));
    rig.sleeper.stop_at(secs(60));
    let (task, answer) = rig.start("k");

    let answer = answer.await.unwrap();
    assert!(is_slow_down(&answer), "{answer:?}");
    rig.sleeper.stopped().await;
    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, true, "p1")
    );
    assert!(rig.hot_has("k").await);
    let second = rig.proxy.delete("k").await;
    assert!(is_slow_down(&second), "{second:?}");

    rig.sleeper.let_go();
    task.await.unwrap();
    assert_eq!(rig.shape("k").await, dirty_shape(true));
    assert_eq!(rig.sleeper.asked()[0], secs(60));
}

/// Asserts the same for a hot DELETE that fails in a way that does not say whether the store did
/// it (the store is unavailable): `SlowDown`, the lock kept for the margin, then released.
#[tokio::test]
async fn a_delete_whose_hot_delete_fails_unavailable_keeps_its_lock_for_the_margin_too() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.hot_faults.fail_next(Op::Delete, 1);
    rig.sleeper.stop_at(secs(60));
    let (task, answer) = rig.start("k");
    let answer = answer.await.unwrap();
    assert!(is_slow_down(&answer), "{answer:?}");

    rig.sleeper.stopped().await;
    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, true, "p1")
    );
    rig.sleeper.let_go();
    task.await.unwrap();
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a hot DELETE the store refuses outright (an answer that says it did not do it) is
/// the store's error as it is, and releases the lock at once with no margin to keep.
#[tokio::test]
async fn a_delete_whose_hot_delete_is_refused_releases_its_lock_at_once() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let refusal = Error::Unsupported("no deletes".to_owned());
    rig.hot_faults.fail_next_with(Op::Delete, refusal.clone());
    assert_eq!(rig.delete("k").await, Err(refusal));

    assert!(rig.sleeper.asked().is_empty());
    assert_eq!(rig.shape("k").await, dirty_shape(true));
    assert!(rig.hot_has("k").await);
}

/// Asserts that a hot DELETE that finds the object already gone (the store says not found) is a
/// delete like any other: answered, and the lock released to `DIRTY(gone)` with no margin to keep.
#[tokio::test]
async fn a_hot_delete_that_finds_the_object_gone_is_answered_and_releases_its_lock_at_once() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gone = Error::NotFound("k".to_owned());
    rig.hot_faults.fail_next_with(Op::Delete, gone);
    assert_eq!(rig.delete("k").await, Ok(()));

    assert!(rig.sleeper.asked().is_empty());
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a HEAD that fails leaves the hot tier alone, releases the lock with the `gone` it
/// found, and gives the store's error.
#[tokio::test]
async fn a_delete_whose_head_fails_releases_its_lock_and_destroys_nothing() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.hot_faults.fail_next(Op::Head, 1);
    let answer = rig.delete("k").await;

    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    assert_eq!(rig.shape("k").await, dirty_shape(false));
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(rig.hot_has("k").await);
}

/// Asserts that a `gone` write that is lost because the lock was reaped under the delete answers
/// `SlowDown`, makes no hot DELETE, and does not touch the marker the reap left: the release it
/// tries finds another marker and is not sent again.
#[tokio::test]
async fn a_delete_whose_lock_was_reaped_before_its_gone_write_destroys_nothing() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.marks.gate(Op::PutIfMatch, "k", Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.reap("k").await;
    assert!(gate.release());

    let answer = answer.await.unwrap();
    assert!(is_slow_down(&answer), "{answer:?}");
    task.await.unwrap();
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(false));
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 2);
}

/// Asserts that a `gone` write whose reply was lost after the store did it is taken from the
/// marker read back, and the delete goes on to the DELETE and the answer.
#[tokio::test]
async fn a_gone_write_whose_reply_was_lost_after_it_landed_goes_on() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.marks.gate(Op::PutIfMatch, "k", Phase::Post);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    assert!(gate.fail_post());

    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a `gone` write that fails and cannot be read back is `Unavailable`, makes no hot
/// DELETE, and leaves the lock to be released by the delete itself, as it stood before the write.
#[tokio::test]
async fn a_gone_write_that_cannot_be_read_back_destroys_nothing_and_the_lock_is_released() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.marks.gate(Op::PutIfMatch, "k", Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 1);
    rig.marks_faults.fail_next(Op::Get, 1);
    assert!(gate.release());

    let answer = answer.await.unwrap();
    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    task.await.unwrap();
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(false));
}

/// Asserts that a delete whose lock is older than `lock_lease - queued_call_margin` when its
/// `gone` write is answered (the write took longer than the window) does not delete: `SlowDown`,
/// the hot object stays, and the lock is released to `DIRTY(gone)`, which the copier takes as a
/// change to copy.
#[tokio::test]
async fn a_delete_whose_lock_is_older_than_the_destroy_window_does_not_delete() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.marks.gate(Op::PutIfMatch, "k", Phase::Post);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.mono.advance(secs(61));
    assert!(gate.release());

    let answer = answer.await.unwrap();
    assert!(is_slow_down(&answer), "{answer:?}");
    task.await.unwrap();
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a delete whose hot DELETE was done but whose registration lapsed before the answer
/// is `Unavailable`, not 204: the object is gone, the lock is released to `DIRTY(gone)`, and the
/// client's retry is 204.
#[tokio::test]
async fn a_delete_acknowledged_after_the_registration_lapsed_is_unavailable_and_its_retry_is_204() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.hot.gate(Op::Delete, "k", Phase::Post);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.stop_being_live();
    assert!(gate.release());

    let answer = answer.await.unwrap();
    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    task.await.unwrap();
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));

    rig.live();
    assert_eq!(rig.delete("k").await, Ok(()));
}

/// Asserts that a proxy whose registration does not allow a call to start answers `Unavailable`
/// and asks no store anything.
#[tokio::test]
async fn a_delete_by_a_proxy_that_is_not_live_asks_no_store_anything() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.stop_being_live();
    let answer = rig.proxy.delete("k").await;

    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    assert_eq!(rig.marks_faults.call_log(), []);
    assert_eq!(rig.hot_log("k"), []);
}

/// Asserts that a delete is `Unavailable` and touches the hot tier not at all while the marks
/// bucket is down (it fails closed like any error of the hot store), and that the cold tier being
/// down changes nothing for a delete: it is answered, the cold tier is never asked.
#[tokio::test]
async fn a_delete_fails_closed_while_the_marks_bucket_is_down_and_ignores_a_cold_tier_that_is_down()
{
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.marks_faults.set_down(true);
    let answer = rig.proxy.delete("k").await;
    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    assert_eq!(rig.hot_log("k"), []);
    assert!(rig.hot_has("k").await);

    rig.marks_faults.set_down(false);
    rig.cold.set_down(true);
    assert_eq!(rig.delete("k").await, Ok(()));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts what a delete that dies after its lock leaves, for a death at each of the three places
/// the design lists, and what ends it. After the lock, before the HEAD: the lock stands with the
/// `gone` it found and the hot object is untouched. After the `gone` write, before the DELETE: the
/// lock says `gone` and the object is still there. After the DELETE, before the answer: the lock
/// says `gone` and the object is gone. In each, a second delete waits and is `SlowDown`; once the
/// lock is reaped to `DIRTY` the client's retry is 204 and leaves the object absent.
#[tokio::test]
async fn a_delete_that_dies_at_each_step_leaves_a_lock_that_blocks_the_key_until_it_is_reaped() {
    let places = [
        (Op::Head, Phase::Pre, false, true),
        (Op::Delete, Phase::Pre, true, true),
        (Op::Delete, Phase::Post, true, false),
    ];
    for (op, phase, gone, hot_stays) in places {
        let rig = Rig::new().await;
        rig.seed_both("k").await;
        let gate = rig.hot.gate(op, "k", phase);
        let (answer, _asked) = tokio::sync::oneshot::channel();
        let running = Box::pin(rig.make("k").run(answer));
        let stopped = futures::future::select(Box::pin(gate.parked()), running).await;
        // The process dies here: the delete is never polled or dropped again.
        std::mem::forget(stopped);

        let left = lock_shape(MarkerWhy::Delete, gone, "p1");
        assert_eq!(rig.shape("k").await, left, "{op:?} {phase:?}");
        assert_eq!(rig.hot_has("k").await, hot_stays, "{op:?} {phase:?}");
        assert!(rig.cold_has("k").await);
        let blocked = rig.proxy.delete("k").await;
        assert!(is_slow_down(&blocked), "{blocked:?}");
        assert_eq!(rig.shape("k").await, left, "{op:?} {phase:?}");

        rig.reap("k").await;
        assert_eq!(rig.shape("k").await, dirty_shape(gone), "{op:?} {phase:?}");
        assert_eq!(rig.delete("k").await, Ok(()));
        assert!(!rig.hot_has("k").await);
        assert_eq!(rig.shape("k").await, dirty_shape(true));
        assert!(rig.cold_has("k").await);
    }
}

/// Asserts that a client that drops its request after the lock was taken does not leak the lock:
/// the delete goes on on its own task, deletes the object and releases the lock.
#[tokio::test]
async fn a_client_that_disconnects_after_the_lock_was_taken_does_not_leak_it() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let proxy = Arc::clone(&rig.proxy);
    let client = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, false, "p1")
    );
    client.abort();
    assert!(client.await.unwrap_err().is_cancelled());
    assert!(gate.release());
    let_the_others_run().await;

    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// A clock that panics once, at the first reading after the test says so.
struct Dies {
    base: Arc<dyn Clock>,
    armed: Arc<AtomicBool>,
}

impl Clock for Dies {
    fn now(&self) -> u64 {
        assert!(!self.armed.swap(false, Ordering::SeqCst), "the clock died");
        self.base.now()
    }
}

/// Asserts that a delete whose task panics while it holds the lock tells the client `Unavailable`
/// and that the drop of the task releases the lock, as a task that ends any other way does.
#[tokio::test]
async fn a_delete_whose_task_panics_holding_the_lock_is_unavailable_and_releases_it() {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let rig = Rig::build(MARKS_FORMAT, move |config: &mut ProxyConfig| {
        let base = Arc::clone(&config.mono);
        config.mono = Arc::new(Dies { base, armed: flag });
    })
    .await;
    rig.seed_both("k").await;
    let gate = rig.hot.gate(Op::Head, "k", Phase::Pre);
    let proxy = Arc::clone(&rig.proxy);
    let client = tokio::spawn(async move { proxy.delete("k").await });
    gate.parked().await;
    armed.store(true, Ordering::SeqCst);
    assert!(gate.release());

    let answer = client.await.unwrap();
    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    let_the_others_run().await;
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(false));
}

/// Asserts that a delete task that is aborted while its hot DELETE is in flight releases its lock
/// after the margin of a queued call (the DELETE may still land), keeping `gone`; one aborted
/// before the DELETE releases it at once.
#[tokio::test]
async fn an_aborted_delete_releases_its_lock_and_waits_the_margin_only_if_a_delete_was_in_flight() {
    for (op, queued) in [(Op::Head, false), (Op::Delete, true)] {
        let rig = Rig::new().await;
        rig.seed_both("k").await;
        let gate = rig.hot.gate(op, "k", Phase::Pre);
        let (task, _answer) = rig.start("k");
        gate.parked().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let_the_others_run().await;

        let asked: Vec<Duration> = if queued { vec![secs(60)] } else { vec![] };
        assert_eq!(rig.sleeper.asked(), asked, "{op:?}");
        assert_eq!(rig.shape("k").await, dirty_shape(queued), "{op:?}");
        assert!(rig.hot_has("k").await, "{op:?}");
    }
}

/// Asserts that a delete dropped where no runtime runs leaves its lock where it is, for the leader
/// to reap.
#[tokio::test]
async fn a_delete_dropped_outside_a_runtime_leaves_its_lock_to_the_leader() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let mut delete = rig.make("k");
    delete.take_lock().await.unwrap();
    std::thread::spawn(move || drop(delete)).join().unwrap();
    let_the_others_run().await;

    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, false, "p1")
    );
}

/// The release of the lock a delete of `key` took and did nothing under.
async fn release_of_a_taken_lock(rig: &Rig, key: &str) -> super::marks_delete::Release {
    let mut delete = rig.make(key);
    delete.take_lock().await.unwrap();
    delete.release().unwrap()
}

/// Asserts that a release whose write failed and could not be read back is sent again after the
/// pause the step retry says (100 ms), and the lock is released.
#[tokio::test]
async fn a_release_that_fails_ambiguously_is_tried_again_after_the_pause() {
    let rig = Rig::new().await;
    let release = release_of_a_taken_lock(&rig, "k").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 1);
    rig.marks_faults.fail_next(Op::Get, 1);
    release.run().await;

    assert_eq!(rig.sleeper.asked(), [Duration::from_millis(100)]);
    assert_eq!(rig.shape("k").await, dirty_shape(false));
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 2);
}

/// Asserts that a release the store refused while the marker stands as the lock wrote it is sent
/// again, and that one that finds the marker changed, or gone, is not.
#[tokio::test]
async fn a_release_that_was_lost_is_sent_again_only_while_the_marker_is_the_one_it_wrote() {
    let rig = Rig::new().await;
    let release = release_of_a_taken_lock(&rig, "k").await;
    let refused = Error::PreconditionFailed("k".to_owned());
    rig.marks_faults.fail_next_with(Op::PutIfMatch, refused);
    release.run().await;
    assert_eq!(rig.shape("k").await, dirty_shape(false));
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 2);

    let release = release_of_a_taken_lock(&rig, "changed").await;
    rig.reap("changed").await;
    release.run().await;
    assert_eq!(rig.shape("changed").await, dirty_shape(false));
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 3);

    let release = release_of_a_taken_lock(&rig, "gone").await;
    rig.marks_mem.delete("gone").await.unwrap();
    release.run().await;
    assert_eq!(rig.shape("gone").await, None);
    assert_eq!(rig.marks_faults.calls(Op::PutIfMatch), 4);
}

/// Asserts that a release that fails in every try (one try is all the proxy is given here) leaves
/// the lock standing for the leader's reap, and the delete's answer was not held up for it.
#[tokio::test]
async fn a_release_that_fails_in_every_try_leaves_the_lock_to_the_leader() {
    let rig = Rig::build(MARKS_FORMAT, |config| config.step_retry.attempts = 1).await;
    let release = release_of_a_taken_lock(&rig, "k").await;
    rig.marks_faults.fail_next(Op::PutIfMatch, 1);
    rig.marks_faults.fail_next(Op::Get, 1);
    release.run().await;

    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, false, "p1")
    );
    assert!(rig.sleeper.asked().is_empty());
}
