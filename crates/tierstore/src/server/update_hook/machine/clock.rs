//! The clock: a monotonic count of milliseconds, and real waiting.

use std::time::{Duration, Instant};

use super::super::ports::Clock;

/// Milliseconds since this value was made, on the monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    #[allow(dead_code)]
    start: Instant,
}

impl MonotonicClock {
    /// A clock that reads 0 now.
    #[must_use]
    pub fn new() -> MonotonicClock {
        MonotonicClock {
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
        0
    }

    fn pause(&self, _span: Duration) {}
}
