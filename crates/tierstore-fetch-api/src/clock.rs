//! The injected clock (section 14: no test asserts timing; every "after X seconds" is a step of
//! an injected clock).
//!
//! Every timed decision of the read path and of the storage tier (stall and pre-first-byte
//! timeouts, controller windows, `evict_wait`, the grace window, `replica_stall`, `stall_after`,
//! `slot_release_after`, view states) reads a [`Clock`]. Production code is handed
//! [`MonotonicClock`]; tests are handed a [`ManualClock`] they move by hand, whose sleeps wake
//! only when the test moves it.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

/// A monotonic source of milliseconds. The origin is the clock's; only differences mean anything.
pub trait Clock: Send + Sync {
    /// The time, in milliseconds since the clock's origin.
    fn now_ms(&self) -> u64;
}

/// The machine's monotonic clock: milliseconds since it was made.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    /// A clock that reads zero now.
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the clock module: the one place that reads the machine's monotonic clock"
    )]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Default)]
struct ManualState {
    now: u64,
    next_id: u64,
    /// Each pending sleep by id: its deadline and the waker of its last poll.
    sleepers: BTreeMap<u64, (u64, Waker)>,
}

/// A clock a test moves by hand. It reads zero when made and moves only forward, by
/// [`ManualClock::advance`]. [`ManualClock::sleep_until`] returns a future that completes once the
/// clock has reached its deadline; [`ManualClock::sleepers`] is the progress event a test waits
/// for before it moves the clock (so it moves time only after the code under test is waiting).
/// Clones share one time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    state: Arc<Mutex<ManualState>>,
}

impl ManualClock {
    /// A clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves the clock forward by `ms` and wakes every sleep whose deadline it reached.
    pub fn advance(&self, ms: u64) {
        let due: Vec<Waker> = {
            let mut s = self.lock();
            s.now = s.now.saturating_add(ms);
            let now = s.now;
            let ids: Vec<u64> = s
                .sleepers
                .iter()
                .filter(|(_, (deadline, _))| *deadline <= now)
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| s.sleepers.remove(id))
                .map(|(_, w)| w)
                .collect()
        };
        for w in due {
            w.wake();
        }
    }

    /// A future that completes when the clock reaches `deadline_ms`.
    pub fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        Sleep {
            clock: self.clone(),
            deadline: deadline_ms,
            id: None,
        }
    }

    /// The number of sleeps waiting for the clock to move.
    #[must_use]
    pub fn sleepers(&self) -> usize {
        self.lock().sleepers.len()
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.lock().now
    }
}

/// The future of [`ManualClock::sleep_until`].
#[derive(Debug)]
#[must_use = "a sleep does nothing unless awaited"]
pub struct Sleep {
    clock: ManualClock,
    deadline: u64,
    /// The sleep's entry among the clock's sleepers, once it has been polled while pending.
    id: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut s = this.clock.lock();
        if s.now >= this.deadline {
            if let Some(id) = this.id.take() {
                s.sleepers.remove(&id);
            }
            return Poll::Ready(());
        }
        let id = *this.id.get_or_insert_with(|| {
            s.next_id += 1;
            s.next_id
        });
        s.sleepers.insert(id, (this.deadline, cx.waker().clone()));
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.clock.lock().sleepers.remove(&id);
        }
    }
}
