//! Proves that an INTERRUPT answers a request exactly once, with `EINTR` on the request's own
//! device xor the real reply, and that an unknown target is answered `EAGAIN` on the device given
//! for the INTERRUPT (the session gives every device in turn).
//!
//! Catches:
//! - `EINTR` written on the interrupting clone instead of the originating one;
//! - both the real reply and `EINTR` written for one request;
//! - a cancel token not cancelled, or a `cancelled()` future that never wakes;
//! - an INTERRUPT that arrives before registration or after the reply left unanswered;
//! - a device error on the `EINTR` or `EAGAIN` write reported as success.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use common::FakeDevice;
use futures::executor::block_on;
use tsfuse::{Completion, Interrupted, Tokens};

#[test]
fn an_interrupt_read_by_another_clone_answers_eintr_on_the_originating_device_once() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    let r = tokens.register(11, origin.clone());
    let token = r.cancel_token();
    assert!(!token.is_cancelled());
    let res = tokens.interrupt(11, 12, &*reader);
    assert!(matches!(res, Interrupted::Cancelled));
    let m = origin.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].len, m[0].error, m[0].unique), (16, -4, 11));
    assert!(reader.raw().is_empty());
    assert!(token.is_cancelled());
    block_on(token.cancelled());
    assert_eq!(tokens.registered(), 0);
    assert!(matches!(r.ok(&[b"late"]), Completion::AlreadyAnswered));
    assert_eq!(origin.msgs().len(), 1);
    assert!(reader.raw().is_empty());
}

#[test]
fn an_interrupt_after_the_reply_is_answered_eagain_on_the_interrupting_device() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    let r = tokens.register(20, origin.clone());
    assert!(matches!(r.ok(&[b"data"]), Completion::Written));
    assert!(matches!(
        tokens.interrupt(20, 21, &*reader),
        Interrupted::Unknown
    ));
    assert_eq!(origin.msgs().len(), 1);
    assert_eq!(origin.msgs()[0].payload, b"data");
    let m = reader.msgs();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].len, m[0].error, m[0].unique), (16, -11, 21));
}

#[test]
fn an_interrupt_before_registration_is_answered_eagain_and_the_request_is_untouched() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    assert!(matches!(
        tokens.interrupt(30, 31, &*reader),
        Interrupted::Unknown
    ));
    let m = reader.msgs();
    assert_eq!((m.len(), m[0].error, m[0].unique), (1, -11, 31));
    let r = tokens.register(30, origin.clone());
    assert!(!r.cancel_token().is_cancelled());
    assert!(matches!(r.ok(&[]), Completion::Written));
    assert_eq!(origin.msgs().len(), 1);
}

#[test]
fn a_failing_device_makes_both_interrupt_paths_report_device_error() {
    let tokens = Tokens::new();
    let bad = FakeDevice::failing();
    let good = FakeDevice::new();
    // EINTR path: the request's own device fails.
    let r = tokens.register(40, bad.clone());
    let token = r.cancel_token();
    match tokens.interrupt(40, 41, &*good) {
        Interrupted::DeviceError(e) => assert_eq!(e.to_string(), "device gone"),
        other => panic!("expected DeviceError, got {other:?}"),
    }
    assert!(token.is_cancelled());
    assert!(good.raw().is_empty());
    // EAGAIN path: the interrupting device fails.
    match tokens.interrupt(99, 100, &*bad) {
        Interrupted::DeviceError(e) => assert_eq!(e.to_string(), "device gone"),
        other => panic!("expected DeviceError, got {other:?}"),
    }
}

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn the_cancelled_future_is_pending_until_the_interrupt_wakes_it() {
    let tokens = Tokens::new();
    let d = FakeDevice::new();
    let r = tokens.register(50, d.clone());
    let token = r.cancel_token();
    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(token.cancelled());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    assert!(matches!(
        tokens.interrupt(50, 51, &*d),
        Interrupted::Cancelled
    ));
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(()));
    // A token clone sees the same cancellation.
    assert!(token.clone().is_cancelled());
    assert_eq!(format!("{token:?}"), "CancelToken { cancelled: true }");
}

#[test]
fn a_task_blocked_on_cancelled_is_released_by_an_interrupt_from_another_thread() {
    let tokens = Tokens::new();
    let d = FakeDevice::new();
    let r = tokens.register(60, d.clone());
    let token = r.cancel_token();
    let t2 = tokens.clone();
    let d2 = d.clone();
    let waiter = thread::spawn(move || block_on(token.cancelled()));
    let intr = thread::spawn(move || t2.interrupt(60, 61, &*d2));
    waiter.join().unwrap();
    assert!(matches!(intr.join().unwrap(), Interrupted::Cancelled));
    assert_eq!(d.msgs().len(), 1);
}

#[test]
fn racing_a_reply_against_an_interrupt_writes_exactly_one_message_per_request() {
    let tokens = Tokens::new();
    let origin = FakeDevice::new();
    let reader = FakeDevice::new();
    let rounds = 300u64;
    let mut cancelled = 0u64;
    for unique in 0..rounds {
        let r = tokens.register(unique, origin.clone());
        let barrier = Arc::new(Barrier::new(2));
        let b2 = barrier.clone();
        let (t2, rd) = (tokens.clone(), reader.clone());
        let intr = thread::spawn(move || {
            b2.wait();
            t2.interrupt(unique, 10_000 + unique, &*rd)
        });
        barrier.wait();
        let done = r.ok(&[b"p"]);
        let res = intr.join().unwrap();
        match (done, res) {
            (Completion::Written, Interrupted::Unknown) => {}
            (Completion::AlreadyAnswered, Interrupted::Cancelled) => cancelled += 1,
            (a, b) => panic!("unique {unique}: {a:?} with {b:?}"),
        }
        let mine: Vec<_> = origin
            .msgs()
            .into_iter()
            .filter(|m| m.unique == unique)
            .collect();
        assert_eq!(mine.len(), 1, "unique {unique}");
        let eagain: Vec<_> = reader
            .msgs()
            .into_iter()
            .filter(|m| m.unique == 10_000 + unique)
            .collect();
        assert_eq!(
            eagain.len(),
            usize::from(mine[0].error == 0),
            "unique {unique}"
        );
        assert!(mine[0].error == 0 || mine[0].error == -4);
        assert_eq!(tokens.registered(), 0);
    }
    assert_eq!(origin.msgs().len() as u64, rounds);
    assert_eq!(reader.msgs().len() as u64, rounds - cancelled);
}
