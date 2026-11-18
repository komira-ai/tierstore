//! Progress barriers (section 14: "progress barriers"): a test waits for an event to have happened
//! a number of times, never for time to pass.
//!
//! - [`Counter`]: one monotonic count. [`Counter::reached`] is a future that completes once the
//!   count is at least its target; [`Counter::waiting`] is the number of such futures that are
//!   pending, the progress event that tells a test the code under test is now blocked on it.
//! - [`Progress`]: counters by event name, shared by clones, so a fake marks `"get"` or `"punch"`
//!   and a test waits for the third one.
//! - [`Gate`]: a counter used as a latch: closed until [`Gate::open`], and every
//!   [`Gate::opened`] future completes from then on.
//!
//! Nothing here reads a clock or sleeps: a future completes only when another task moves the
//! count (AGENTS.md T10).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// Locks a harness mutex; a test that panicked while holding it has already failed, so the state
/// is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct Inner {
    count: u64,
    next_id: u64,
    /// Each pending [`Reached`] by id: its target and the waker of its last poll.
    waiters: BTreeMap<u64, (u64, Waker)>,
}

/// A count that only goes up, and futures that wait for it. Clones share one count.
#[derive(Debug, Clone, Default)]
pub struct Counter {
    inner: Arc<Mutex<Inner>>,
}

impl Counter {
    /// A count at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `n` and wakes every waiter whose target the count now reaches.
    pub fn add(&self, n: u64) {
        let due: Vec<Waker> = {
            let mut s = lock(&self.inner);
            s.count = s.count.saturating_add(n);
            let count = s.count;
            let ids: Vec<u64> = s
                .waiters
                .iter()
                .filter(|(_, (target, _))| *target <= count)
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| s.waiters.remove(id))
                .map(|(_, w)| w)
                .collect()
        };
        due.into_iter().for_each(Waker::wake);
    }

    /// The count.
    #[must_use]
    pub fn get(&self) -> u64 {
        lock(&self.inner).count
    }

    /// A future that completes once the count is at least `target`.
    pub fn reached(&self, target: u64) -> Reached {
        Reached {
            counter: self.clone(),
            target,
            id: None,
        }
    }

    /// The number of [`Reached`] futures that were polled and are still pending.
    #[must_use]
    pub fn waiting(&self) -> usize {
        lock(&self.inner).waiters.len()
    }
}

/// The future of [`Counter::reached`].
#[derive(Debug)]
#[must_use = "a progress barrier does nothing unless awaited"]
pub struct Reached {
    counter: Counter,
    target: u64,
    /// The future's entry among the counter's waiters, once it has been polled while pending.
    id: Option<u64>,
}

impl Future for Reached {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut s = lock(&this.counter.inner);
        if s.count >= this.target {
            if let Some(id) = this.id.take() {
                s.waiters.remove(&id);
            }
            return Poll::Ready(());
        }
        let id = *this.id.get_or_insert_with(|| {
            s.next_id += 1;
            s.next_id
        });
        s.waiters.insert(id, (this.target, cx.waker().clone()));
        Poll::Pending
    }
}

impl Drop for Reached {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.counter.inner).waiters.remove(&id);
        }
    }
}

/// Counters by event name. Clones share the counters.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    events: Arc<Mutex<BTreeMap<String, Counter>>>,
}

impl Progress {
    /// No event has happened.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The counter of `event` (made at zero on first use).
    #[must_use]
    pub fn counter(&self, event: &str) -> Counter {
        lock(&self.events)
            .entry(event.to_owned())
            .or_default()
            .clone()
    }

    /// Records one occurrence of `event`.
    pub fn mark(&self, event: &str) {
        self.counter(event).add(1);
    }

    /// How many times `event` happened.
    #[must_use]
    pub fn count(&self, event: &str) -> u64 {
        lock(&self.events).get(event).map_or(0, Counter::get)
    }

    /// A future that completes once `event` has happened `n` times.
    pub fn reached(&self, event: &str, n: u64) -> Reached {
        self.counter(event).reached(n)
    }

    /// Every event and its count, by name.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        lock(&self.events)
            .iter()
            .map(|(name, c)| (name.clone(), c.get()))
            .collect()
    }
}

/// A latch: closed until opened, then open for good. Clones share it.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    counter: Counter,
}

impl Gate {
    /// A closed gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the gate (a second open changes nothing) and wakes every waiter.
    pub fn open(&self) {
        if !self.is_open() {
            self.counter.add(1);
        }
    }

    /// Whether the gate is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.counter.get() > 0
    }

    /// A future that completes once the gate is open.
    pub fn opened(&self) -> Reached {
        self.counter.reached(1)
    }

    /// The number of [`Gate::opened`] futures blocked on the gate.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.counter.waiting()
    }
}
