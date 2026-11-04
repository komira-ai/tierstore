//! Where a loop waits, so that a test can see the waits instead of sitting through them.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;

/// What a loop calls to wait between two rounds.
///
/// A loop that waits through this trait can be run by a test that never waits: the test hands it
/// a [`RecordingSleeper`] and asserts the durations it asked for, as values.
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Returns after `duration` has passed, or, for a sleeper made for tests, at once.
    async fn sleep(&self, duration: Duration);
}

/// The sleeper of a process: it waits on the timer of the runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// A sleeper that does not wait: it writes down the duration it was asked for and returns, after
/// it has given the other tasks of the runtime a turn, so that a loop with nothing but this to
/// wait on does not starve them.
#[derive(Debug, Default)]
pub struct RecordingSleeper {
    asked: Mutex<Vec<Duration>>,
}

impl RecordingSleeper {
    /// A sleeper that has not been asked for anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The duration of every call so far, in the order of the calls.
    #[must_use]
    pub fn asked(&self) -> Vec<Duration> {
        self.record().clone()
    }

    fn record(&self) -> MutexGuard<'_, Vec<Duration>> {
        // No method panics while holding the lock, so a poisoned list is still whole.
        self.asked.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Sleeper for RecordingSleeper {
    async fn sleep(&self, duration: Duration) {
        self.record().push(duration);
        tokio::task::yield_now().await;
    }
}
