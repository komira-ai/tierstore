//! The engine's timer: the injected clock (`tierstore_fetch_api::clock::Clock`) plus sleeping
//! until a time of that clock. The frozen `Clock` reads time only; the engine also waits (stall
//! and pre-first-byte timeouts, the hedge trigger, backoffs, the bad-cell expiry), so it is handed
//! a [`Timer`]. Production uses [`TokioTimer`]; tests use [`ManualTimer`], whose sleeps wake only
//! when the test moves its [`ManualClock`].

use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tierstore_fetch_api::clock::{Clock, ManualClock, MonotonicClock};

/// A clock the engine can also sleep on.
pub trait Timer: Clock {
    /// A future that completes once [`Clock::now_ms`] has reached `deadline_ms`.
    fn sleep_until(&self, deadline_ms: u64) -> BoxFuture<'static, ()>;
}

/// The machine's monotonic clock and tokio's sleep. It must be used inside a tokio runtime with
/// the time driver enabled.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTimer {
    clock: MonotonicClock,
}

impl TokioTimer {
    /// A timer that reads zero now.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clock for TokioTimer {
    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }
}

impl Timer for TokioTimer {
    fn sleep_until(&self, deadline_ms: u64) -> BoxFuture<'static, ()> {
        // A deadline already reached is ready at once (tokio's zero sleep waits for a turn of
        // its driver).
        match deadline_ms.saturating_sub(self.now_ms()) {
            0 => futures::future::ready(()).boxed(),
            wait => tokio::time::sleep(Duration::from_millis(wait)).boxed(),
        }
    }
}

/// A [`ManualClock`] as a timer.
#[derive(Debug, Clone, Default)]
pub struct ManualTimer {
    clock: ManualClock,
}

impl ManualTimer {
    /// A timer over `clock` (clones share one time).
    #[must_use]
    pub fn new(clock: ManualClock) -> Self {
        Self { clock }
    }

    /// The clock the test moves.
    #[must_use]
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }
}

impl Clock for ManualTimer {
    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }
}

impl Timer for ManualTimer {
    fn sleep_until(&self, deadline_ms: u64) -> BoxFuture<'static, ()> {
        self.clock.sleep_until(deadline_ms).boxed()
    }
}
