//! The clock: a monotonic count of milliseconds, and real waiting.

use std::time::{Duration, Instant};

use super::super::ports::Clock;

/// Milliseconds since this value was made, on the monotonic clock.
pub struct MonotonicClock {
    start: Instant,
    pause: Option<Duration>,
}

impl MonotonicClock {
    /// A clock that reads 0 now.
    #[must_use]
    pub fn new() -> MonotonicClock {
        MonotonicClock {
            start: Instant::now(),
            pause: None,
        }
    }

    /// The same, but every pause waits `pause` instead of the span it is asked for: the poll of a
    /// conformance run, which need not wait out a host's interval.
    #[must_use]
    pub fn with_pause(mut self, pause: Duration) -> MonotonicClock {
        self.pause = Some(pause);
        self
    }

    /// What a pause asked for `span` waits: the clock's own pause when it has one.
    #[must_use]
    pub fn waited(&self, span: Duration) -> Duration {
        self.pause.unwrap_or(span)
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
        std::thread::sleep(self.waited(span));
    }
}
