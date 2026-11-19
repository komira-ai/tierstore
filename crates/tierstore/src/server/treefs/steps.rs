//! The time each step of a start of `tierstore fs` takes, said as the step ends, so that the
//! journal of a start the service manager waits for shows where the time went.

use std::sync::Arc;

use crate::Clock;

/// The steps of one start, timed on a clock.
pub(super) struct Steps {
    clock: Arc<dyn Clock>,
    /// When the steps began, on the clock.
    begun: u64,
    /// When the step before ended, on the clock.
    last: u64,
}

impl Steps {
    /// Steps that begin now, on `clock`.
    pub(super) fn new(clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            clock,
            begun: now,
            last: now,
        }
    }

    /// Says that `step` is done, and how long it took: since the step before ended, or since the
    /// steps began. The milliseconds it took.
    pub(super) fn done(&mut self, step: &str) -> u64 {
        let now = self.clock.now();
        let took = now.saturating_sub(self.last);
        self.last = now;
        tracing::info!("tierstore fs: {step} took {took} ms");
        took
    }

    /// The milliseconds since the steps began.
    pub(super) fn total(&self) -> u64 {
        self.clock.now().saturating_sub(self.begun)
    }
}

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
