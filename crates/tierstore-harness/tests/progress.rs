//! Progress barriers: a waiter completes exactly when the count reaches its target, never before,
//! and a dropped waiter leaves nothing behind.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use tierstore_harness::progress::{Counter, Gate, Progress};

fn poll_once<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

/// A waiter for 2 stays pending at 0 and 1, counts as waiting, and is ready at 2; a waiter whose
/// target is already met is ready on its first poll and never registers.
#[test]
fn a_counter_waiter_completes_exactly_at_its_target() {
    let c = Counter::new();
    let mut r = pin!(c.reached(2));
    assert!(poll_once(r.as_mut()).is_pending());
    assert_eq!(c.waiting(), 1);
    c.add(1);
    assert_eq!(c.get(), 1);
    assert!(poll_once(r.as_mut()).is_pending());
    assert_eq!(c.waiting(), 1, "a second poll keeps one registration");
    c.add(1);
    assert_eq!(c.waiting(), 0, "reaching the target removes the waiter");
    assert!(poll_once(r.as_mut()).is_ready());
    let mut met = pin!(c.reached(1));
    assert!(poll_once(met.as_mut()).is_ready());
    assert_eq!(c.waiting(), 0);
}

/// A pending waiter that is ready on its next poll (the count moved without waking it first)
/// removes its registration; a dropped pending waiter removes it too.
#[test]
fn a_waiter_leaves_no_registration_when_ready_or_dropped() {
    let c = Counter::new();
    {
        let mut r = pin!(c.reached(1));
        assert!(poll_once(r.as_mut()).is_pending());
        assert_eq!(c.waiting(), 1);
    }
    assert_eq!(c.waiting(), 0, "dropping a pending waiter unregisters it");
    let mut r = Box::pin(c.reached(3));
    assert!(poll_once(r.as_mut()).is_pending());
    c.add(2);
    assert_eq!(c.waiting(), 1);
    c.add(5);
    assert_eq!(c.get(), 7);
    assert!(poll_once(r.as_mut()).is_ready());
    drop(r);
    assert_eq!(c.waiting(), 0);
    let never_polled = c.reached(100);
    drop(never_polled);
    assert_eq!(c.waiting(), 0);
}

/// The counter saturates instead of wrapping.
#[test]
fn a_counter_saturates_at_the_largest_count() {
    let c = Counter::new();
    c.add(u64::MAX);
    c.add(1);
    assert_eq!(c.get(), u64::MAX);
}

/// Named events count separately, clones share them, an unknown event is zero, and the snapshot
/// lists every event touched.
#[test]
fn progress_counts_each_named_event_across_clones() {
    let p = Progress::new();
    let q = p.clone();
    assert_eq!(p.count("get"), 0);
    p.mark("get");
    q.mark("get");
    q.mark("punch");
    assert_eq!(p.count("get"), 2);
    assert_eq!(q.count("punch"), 1);
    let snap = p.snapshot();
    assert_eq!(
        snap.into_iter().collect::<Vec<_>>(),
        vec![("get".to_owned(), 2), ("punch".to_owned(), 1)]
    );
    assert_eq!(p.counter("get").get(), 2);
}

/// A task waiting on an event wakes when another task marks it, with no clock involved.
#[tokio::test]
async fn a_task_waiting_on_an_event_wakes_when_it_is_marked() {
    let p = Progress::new();
    let waiter = tokio::spawn({
        let p = p.clone();
        async move { p.reached("landed", 3).await }
    });
    let landed = p.counter("landed");
    while landed.waiting() == 0 {
        tokio::task::yield_now().await;
    }
    for _ in 0..3 {
        p.mark("landed");
    }
    waiter.await.unwrap();
    assert_eq!(p.count("landed"), 3);
}

/// A gate is closed until opened, a second open changes nothing, and every waiter is released.
#[tokio::test]
async fn a_gate_releases_every_waiter_once_opened() {
    let g = Gate::new();
    assert!(!g.is_open());
    let mut early = Box::pin(g.opened());
    assert!(poll_once(early.as_mut()).is_pending());
    assert_eq!(g.waiting(), 1);
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let g = g.clone();
            tokio::spawn(async move { g.opened().await })
        })
        .collect();
    while g.waiting() < 5 {
        tokio::task::yield_now().await;
    }
    g.open();
    g.open();
    assert!(g.is_open());
    for w in waiters {
        w.await.unwrap();
    }
    assert!(poll_once(early.as_mut()).is_ready());
    assert!(poll_once(Box::pin(g.opened()).as_mut()).is_ready());
}
