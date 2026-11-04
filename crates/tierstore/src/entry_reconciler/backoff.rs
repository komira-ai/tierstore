//! The waits of the entry reconciler: the idle wait of its loop and the delay of an item that
//! failed. Both are plain values that a test reads.

use std::time::Duration;

use super::StepReport;

/// How long the loop waits after a step: none while it makes progress, the floor while a held
/// entry waits for its owner, and otherwise a wait that doubles from the floor to the ceiling
/// and starts again from the floor when progress is made.
pub(super) struct Idle {
    min: Duration,
    max: Duration,
    delay: Duration,
}

impl Idle {
    /// A loop that waits `min` first and at most `max`.
    pub(super) fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            delay: min,
        }
    }

    /// The wait after a step that came to `report`, or `None` to step again at once.
    pub(super) fn after(&mut self, report: &StepReport) -> Option<Duration> {
        if !(report.active && report.held) {
            return Some(self.min);
        }
        if report.progress() {
            self.delay = self.min;
            return None;
        }
        if report.pinned > 0 {
            return Some(self.min);
        }
        let wait = self.delay;
        self.delay = self.delay.saturating_mul(2).min(self.max);
        Some(wait)
    }
}

/// The delay before an item that has now failed `failures` times in a row (at least one) is
/// tried again: `min`, doubled for each failure after the first, and at most `max`.
pub(super) fn retry_delay(min: Duration, max: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(20);
    min.saturating_mul(1 << doublings).min(max)
}
