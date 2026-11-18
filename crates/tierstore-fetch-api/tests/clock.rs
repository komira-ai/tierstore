//! The injected clock (`clock`).
//!
//! Proves the machine clock never reads backwards, and that the manual clock moves only by
//! `advance`, saturates, is shared by its clones, and that its sleeps complete exactly when the
//! clock reaches their deadline, are counted once in `sleepers()` however often they are polled,
//! wake only their latest waker, and leave the count when dropped. No assertion reads real time.
//!
//! Catches:
//! - a monotonic clock that goes backwards;
//! - a manual clock that moves by itself, wraps at `u64::MAX`, or does not share time between clones;
//! - a sleep that completes before its deadline, fails to complete at it, or stays registered after
//!   it completes or is dropped;
//! - a sleep polled twice that is counted twice, or whose wake goes to a stale waker;
//! - an already due sleep that waits for an `advance`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use tierstore_fetch_api::clock::{Clock, ManualClock, MonotonicClock, Sleep};

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn waker() -> (Arc<Counter>, Waker) {
    let c = Arc::new(Counter(AtomicUsize::new(0)));
    (c.clone(), Waker::from(c))
}

fn woken(c: &Counter) -> usize {
    c.0.load(Ordering::SeqCst)
}

fn poll(s: &mut Pin<Box<Sleep>>, w: &Waker) -> Poll<()> {
    s.as_mut().poll(&mut Context::from_waker(w))
}

#[test]
fn the_monotonic_clock_never_reads_backwards() {
    let c = MonotonicClock::new();
    let mut last = c.now_ms();
    for _ in 0..1000 {
        let now = c.now_ms();
        assert!(now >= last);
        last = now;
    }
    let copy = c;
    assert!(copy.now_ms() >= last);
    let d = MonotonicClock::default();
    let first = d.now_ms();
    assert!(d.now_ms() >= first);
}

#[test]
fn a_manual_clock_reads_zero_and_moves_only_by_advance() {
    let c = ManualClock::new();
    assert_eq!(c.now_ms(), 0);
    assert_eq!(c.now_ms(), 0);
    c.advance(5);
    assert_eq!(c.now_ms(), 5);
    c.advance(0);
    assert_eq!(c.now_ms(), 5);
    c.advance(10);
    assert_eq!(c.now_ms(), 15);
}

#[test]
fn a_manual_clock_saturates_at_u64_max() {
    let c = ManualClock::default();
    c.advance(u64::MAX);
    c.advance(1);
    assert_eq!(c.now_ms(), u64::MAX);
}

#[test]
fn clones_share_one_time_and_one_set_of_sleeps() {
    let a = ManualClock::new();
    let b = a.clone();
    a.advance(7);
    assert_eq!(b.now_ms(), 7);
    b.advance(3);
    assert_eq!(a.now_ms(), 10);
    let (_, w) = waker();
    let mut s = Box::pin(a.sleep_until(100));
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    assert_eq!(b.sleepers(), 1);
    b.advance(90);
    assert_eq!(poll(&mut s, &w), Poll::Ready(()));
    assert_eq!(a.sleepers(), 0);
}

#[test]
fn a_sleep_whose_deadline_has_passed_completes_at_once_without_registering() {
    let c = ManualClock::new();
    c.advance(10);
    let (n, w) = waker();
    for deadline in [0, 9, 10] {
        let mut s = Box::pin(c.sleep_until(deadline));
        assert_eq!(poll(&mut s, &w), Poll::Ready(()), "deadline {deadline}");
        assert_eq!(c.sleepers(), 0);
    }
    assert_eq!(woken(&n), 0);
}

#[test]
fn a_pending_sleep_is_counted_once_even_when_polled_twice() {
    let c = ManualClock::new();
    let (_, w) = waker();
    let mut s = Box::pin(c.sleep_until(50));
    assert_eq!(c.sleepers(), 0, "not registered before its first poll");
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    assert_eq!(c.sleepers(), 1);
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    assert_eq!(c.sleepers(), 1);
}

#[test]
fn two_sleeps_are_two_sleepers() {
    let c = ManualClock::new();
    let (_, w) = waker();
    let mut a = Box::pin(c.sleep_until(50));
    let mut b = Box::pin(c.sleep_until(50));
    assert_eq!(poll(&mut a, &w), Poll::Pending);
    assert_eq!(poll(&mut b, &w), Poll::Pending);
    assert_eq!(c.sleepers(), 2);
}

#[test]
fn a_sleep_completes_only_once_the_clock_reaches_its_deadline() {
    let c = ManualClock::new();
    let (n, w) = waker();
    let mut s = Box::pin(c.sleep_until(100));
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    c.advance(99);
    assert_eq!(woken(&n), 0, "not woken before the deadline");
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    assert_eq!(c.sleepers(), 1);
    c.advance(1);
    assert_eq!(woken(&n), 1, "woken exactly at the deadline");
    assert_eq!(
        c.sleepers(),
        0,
        "a due sleep leaves the set when it is woken"
    );
    assert_eq!(poll(&mut s, &w), Poll::Ready(()));
    assert_eq!(c.sleepers(), 0);
}

#[test]
fn an_advance_wakes_every_due_sleep_and_leaves_the_later_ones() {
    let c = ManualClock::new();
    let (n1, w1) = waker();
    let (n2, w2) = waker();
    let (n3, w3) = waker();
    let mut a = Box::pin(c.sleep_until(10));
    let mut b = Box::pin(c.sleep_until(20));
    let mut d = Box::pin(c.sleep_until(30));
    assert_eq!(poll(&mut a, &w1), Poll::Pending);
    assert_eq!(poll(&mut b, &w2), Poll::Pending);
    assert_eq!(poll(&mut d, &w3), Poll::Pending);
    c.advance(20);
    assert_eq!((woken(&n1), woken(&n2), woken(&n3)), (1, 1, 0));
    assert_eq!(c.sleepers(), 1);
    c.advance(10);
    assert_eq!(woken(&n3), 1);
    assert_eq!(c.sleepers(), 0);
}

#[test]
fn a_repolled_sleep_wakes_the_waker_of_its_last_poll_only() {
    let c = ManualClock::new();
    let (old, w_old) = waker();
    let (new, w_new) = waker();
    let mut s = Box::pin(c.sleep_until(5));
    assert_eq!(poll(&mut s, &w_old), Poll::Pending);
    assert_eq!(poll(&mut s, &w_new), Poll::Pending);
    c.advance(5);
    assert_eq!((woken(&old), woken(&new)), (0, 1));
}

#[test]
fn a_dropped_sleep_leaves_the_sleepers() {
    let c = ManualClock::new();
    let (n, w) = waker();
    let mut s = Box::pin(c.sleep_until(5));
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    assert_eq!(c.sleepers(), 1);
    drop(s);
    assert_eq!(c.sleepers(), 0);
    c.advance(5);
    assert_eq!(woken(&n), 0, "a dropped sleep is never woken");
}

#[test]
fn dropping_a_sleep_that_was_never_polled_or_has_completed_changes_nothing() {
    let c = ManualClock::new();
    let (_, w) = waker();
    drop(c.sleep_until(5));
    assert_eq!(c.sleepers(), 0);
    let mut s = Box::pin(c.sleep_until(5));
    assert_eq!(poll(&mut s, &w), Poll::Pending);
    let mut other = Box::pin(c.sleep_until(500));
    assert_eq!(poll(&mut other, &w), Poll::Pending);
    c.advance(5);
    assert_eq!(poll(&mut s, &w), Poll::Ready(()));
    drop(s);
    assert_eq!(c.sleepers(), 1, "the other sleep is still registered");
}

#[tokio::test]
async fn a_sleep_awaited_by_a_task_ends_when_the_test_moves_the_clock() {
    let c = ManualClock::new();
    let task = {
        let c = c.clone();
        tokio::spawn(async move {
            c.sleep_until(1000).await;
            c.now_ms()
        })
    };
    // The progress event: the task is waiting. Yield until it is, then move time.
    while c.sleepers() == 0 {
        tokio::task::yield_now().await;
    }
    c.advance(1000);
    assert_eq!(task.await.expect("the task completes"), 1000);
}

#[test]
fn the_clock_trait_is_usable_as_a_trait_object() {
    let manual = ManualClock::new();
    manual.advance(4);
    let clocks: Vec<Box<dyn Clock>> = vec![Box::new(manual), Box::new(MonotonicClock::new())];
    assert_eq!(clocks[0].now_ms(), 4);
    let first = clocks[1].now_ms();
    assert!(clocks[1].now_ms() >= first);
}
