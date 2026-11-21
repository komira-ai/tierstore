//! The fetches: parked requests are answered by the pool's threads, never by the one that parks
//! them, as many at once as there are threads, the rest after them in turn.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::{Fetches, Job};
use crate::server::treefs::testing::until;

/// Asserts that a park returns while the request it parked is still waiting, that the pool's
/// threads answer as many requests at once as there are threads and the rest after them, and that
/// every request parked is answered in the end, on a thread of the pool.
#[test]
fn parked_requests_wait_on_the_pool_threads_and_never_on_the_one_that_parks() {
    let threads = 3;
    let fetches = Fetches::start(NonZeroUsize::new(threads).unwrap()).unwrap();
    let (open, gate) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    let started = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(Mutex::new(Vec::new()));
    let parked = threads * 2 + 1;
    for _ in 0..parked {
        let (gate, started, names) = (Arc::clone(&gate), Arc::clone(&started), Arc::clone(&names));
        // Each request waits until the test lets one through: parking it does not.
        fetches.park(Box::new(move || {
            let _ = started.fetch_add(1, Ordering::SeqCst);
            let name = std::thread::current().name().map(str::to_owned);
            names.lock().unwrap().push(name);
            let _ = gate.lock().unwrap().recv();
        }));
    }
    assert_eq!(fetches.busy(), parked);
    // As many start as there are threads; only one at a time takes the gate, the others wait on
    // its lock, so no more start until one ends.
    until(|| started.load(Ordering::SeqCst) == threads);
    for _ in 0..parked {
        open.send(()).unwrap();
    }
    until(|| fetches.busy() == 0);
    assert_eq!(started.load(Ordering::SeqCst), parked);
    let names = names.lock().unwrap();
    assert!(
        names.iter().all(|name| name
            .as_deref()
            .is_some_and(|name| name.starts_with("fs-fetch-"))),
        "{names:?}"
    );
}

/// Asserts that a request that panics is counted as answered, and that the pool goes on
/// answering the others.
#[test]
fn a_request_that_panics_is_counted_answered_and_the_others_still_are() {
    let fetches = Fetches::start(NonZeroUsize::new(2).unwrap()).unwrap();
    fetches.park(Box::new(|| panic!("a parked request that panics")));
    let answered = Arc::new(AtomicUsize::new(0));
    for _ in 0..4 {
        let answered = Arc::clone(&answered);
        fetches.park(Box::new(move || {
            let _ = answered.fetch_add(1, Ordering::SeqCst);
        }));
    }
    until(|| fetches.busy() == 0);
    assert_eq!(answered.load(Ordering::SeqCst), 4);
}

/// Asserts that a request parked when no thread of the pool is left is answered on the thread
/// that parks it, at once.
#[test]
fn a_request_with_no_thread_left_to_take_it_is_answered_by_the_one_that_parks_it() {
    let (queue, jobs) = mpsc::channel::<Job>();
    drop(jobs);
    let fetches = Fetches {
        queue,
        busy: Arc::default(),
    };
    let here = std::thread::current().id();
    let ran_on = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&ran_on);
    fetches.park(Box::new(move || {
        *seen.lock().unwrap() = Some(std::thread::current().id());
    }));
    assert_eq!(*ran_on.lock().unwrap(), Some(here));
    assert_eq!(fetches.busy(), 0);
}

/// Asserts that a drain returns only once every request parked is answered: the request it
/// waited for had ended when it returned.
#[test]
fn a_drain_returns_once_every_parked_request_is_answered() {
    let fetches = Arc::new(Fetches::start(NonZeroUsize::new(2).unwrap()).unwrap());
    let (open, gate) = mpsc::channel::<()>();
    let ended = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&ended);
    fetches.park(Box::new(move || {
        let _ = gate.recv();
        let _ = seen.fetch_add(1, Ordering::SeqCst);
    }));
    let (draining, ended_at) = (Arc::clone(&fetches), Arc::clone(&ended));
    let drain = std::thread::spawn(move || {
        draining.drain();
        ended_at.load(Ordering::SeqCst)
    });
    open.send(()).unwrap();
    assert_eq!(drain.join().unwrap(), 1);
    // With nothing parked, a drain returns at once.
    fetches.drain();
}
