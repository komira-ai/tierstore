//! The time an arm reads: the machine's clock in a run, a stepping clock in a test, so no test
//! waits on or asserts real time (AGENTS.md T10).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A monotonic clock and the wall time.
pub trait Clock: Send + Sync {
    /// Time since the clock was made.
    fn now(&self) -> Duration;
    /// The same time as [`Clock::now`], read without counting as a read (a stepping clock does
    /// not move for it): what a thread that only watches the time uses.
    fn peek(&self) -> Duration {
        self.now()
    }
    /// Seconds since the Unix epoch.
    fn unix_secs(&self) -> u64;
    /// Waits `d` (a stepping clock moves itself instead).
    fn sleep(&self, d: Duration);
}

/// The machine's clocks.
#[derive(Debug)]
pub struct RealClock {
    start: Instant,
}

impl RealClock {
    /// A clock whose zero is now.
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a run reads the machine's monotonic clock"
    )]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a run reads the wall clock (start times shared between nodes)"
    )]
    fn unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that moves `step` on every [`Clock::now`]; a peek reads it without moving it, and a
/// sleep only yields. A loop that runs "for 20 s" ends after a known number of reads, and a
/// thread that only watches the time (a sampler) never moves it for the threads doing work.
#[derive(Debug)]
pub struct StepClock {
    nanos: AtomicU64,
    step: u64,
    unix: u64,
}

impl StepClock {
    /// A clock at zero that moves `step` per read, at wall time `unix` seconds.
    #[must_use]
    pub fn new(step: Duration, unix: u64) -> Self {
        Self {
            nanos: AtomicU64::new(0),
            step: u64::try_from(step.as_nanos()).unwrap_or(u64::MAX),
            unix,
        }
    }
}

impl Clock for StepClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.fetch_add(self.step, Ordering::SeqCst))
    }

    fn peek(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }

    fn unix_secs(&self) -> u64 {
        self.unix + self.nanos.load(Ordering::SeqCst) / 1_000_000_000
    }

    fn sleep(&self, _d: Duration) {
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_clock_moves_a_step_per_read_and_not_for_a_peek_or_a_sleep() {
        let c = StepClock::new(Duration::from_secs(1), 1000);
        assert_eq!(c.now(), Duration::ZERO);
        assert_eq!(c.now(), Duration::from_secs(1));
        c.sleep(Duration::from_secs(5));
        assert_eq!(c.peek(), Duration::from_secs(2));
        assert_eq!(c.now(), Duration::from_secs(2));
        assert_eq!(c.unix_secs(), 1003);
    }

    #[test]
    fn real_clock_is_monotonic_and_after_the_epoch() {
        let c = RealClock::default();
        let a = c.now();
        c.sleep(Duration::ZERO);
        assert!(c.peek() >= a);
        assert!(c.unix_secs() > 0);
    }
}
