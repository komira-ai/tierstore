//! The one place the harness reads the machine's monotonic clock: the latency of a request and
//! the length of a phase are measured with it. No test asserts what it reads.

use std::time::Instant;

/// Nanoseconds since the clock was made.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    base: Instant,
}

impl Clock {
    /// A clock that reads zero now.
    #[expect(
        clippy::disallowed_methods,
        reason = "the clock module of the harness: the one place that reads the machine's clock"
    )]
    pub(crate) fn start() -> Self {
        Self {
            base: Instant::now(),
        }
    }

    /// The nanoseconds since [`Clock::start`], saturating at `u64::MAX` (584 years).
    pub(crate) fn ns(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}
