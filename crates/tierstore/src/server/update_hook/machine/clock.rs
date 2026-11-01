//! The clock: a monotonic count of milliseconds, and real waiting.

use std::time::{Duration, Instant};

use super::super::ports::Clock;

/// Milliseconds since this value was made, on the monotonic clock.
pub struct MonotonicClock {
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
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn pause(&self, span: Duration) {
        std::thread::sleep(span);
    }
}
