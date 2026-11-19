//! [`Pause`]: how the evictor waits between two reads of a group's reader counters.

use std::time::Duration;

/// The wait between two polls of the reader counters (5.1 evictor step 2: "wait with backoff").
/// The deadline is read from the injected clock, not from the pause, so a test's pause moves its
/// [`ManualClock`](tierstore_fetch_api::clock::ManualClock) (or lets a reader leave) and the
/// evictor's decisions follow the clock alone.
pub trait Pause: Send + Sync {
    /// Waits before poll `attempt + 1` (`attempt` counts from 0).
    fn pause(&self, attempt: u32);
}

/// The production pause: sleeps the thread 1 ms, doubling per attempt up to 64 ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct SleepPause;

impl SleepPause {
    /// The longest sleep.
    pub const MAX: Duration = Duration::from_millis(64);

    /// The sleep before poll `attempt + 1`.
    #[must_use]
    pub fn backoff(attempt: u32) -> Duration {
        Duration::from_millis(1u64 << attempt.min(6)).min(Self::MAX)
    }
}

impl Pause for SleepPause {
    fn pause(&self, attempt: u32) {
        std::thread::sleep(Self::backoff(attempt));
    }
}
