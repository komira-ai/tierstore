//! The clocks of the harness. [`MachineClock`] is the one place that reads the machine's
//! monotonic and wall clocks: the latency of a part and the time a tree took are measured with
//! it. Everything else is handed a [`Clock`], so a test moves a [`ManualClock`] and asserts what
//! was counted; no test asserts what the machine's clock read.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A source of time.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Microseconds since the clock was made.
    fn now_us(&self) -> u64;

    /// Milliseconds since the Unix epoch: the start time several nodes agree on.
    fn wall_ms(&self) -> u64;
}

/// The machine's clocks.
#[derive(Debug, Clone, Copy)]
pub struct MachineClock {
    base: Instant,
}

impl MachineClock {
    /// A clock that reads zero now.
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the clock module of the harness: the one place that reads the machine's clock"
    )]
    pub fn start() -> Self {
        Self {
            base: Instant::now(),
        }
    }

    /// The clock as the shared handle the modules take.
    #[must_use]
    pub fn shared() -> Arc<dyn Clock> {
        Arc::new(Self::start())
    }
}

impl Clock for MachineClock {
    fn now_us(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the clock module of the harness: the one place that reads the machine's clock"
    )]
    fn wall_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock a test moves by hand.
#[derive(Debug, Default)]
pub struct ManualClock {
    us: AtomicU64,
    wall_ms: AtomicU64,
}

impl ManualClock {
    /// A clock at zero, its wall clock at `wall_ms`.
    #[must_use]
    pub fn new(wall_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            us: AtomicU64::new(0),
            wall_ms: AtomicU64::new(wall_ms),
        })
    }

    /// Moves both clocks on by `us` microseconds.
    pub fn advance_us(&self, us: u64) {
        self.us.fetch_add(us, Ordering::SeqCst);
        self.wall_ms.fetch_add(us / 1000, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_us(&self) -> u64 {
        self.us.load(Ordering::SeqCst)
    }

    fn wall_ms(&self) -> u64 {
        self.wall_ms.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
