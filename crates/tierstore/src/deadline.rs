//! The time a process may destroy and acknowledge until, and the margin in front of its end.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::Clock;

/// A duration in whole milliseconds, stopping at the largest count instead of wrapping.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The time until which a holder (a proxy that heartbeats, a reconciler that holds the lease) may
/// still make the calls that destroy or acknowledge, on a monotonic clock of its own.
///
/// It is zero when made, which is a time already past: a holder that has not yet been told it may
/// act may not. Whoever renews the holder's hold (the heartbeat, the lease) moves it on with
/// [`Deadline::extend_to`], to the time its request was sent plus what the hold lasts less the
/// margin; the time of the answer is never used, because a reply that came late says nothing
/// about when the store saw the request.
///
/// A call may start only while at least the margin is left ([`Deadline::allows_start`]), and the
/// calls of a [`Bounded`](crate::Bounded) are cut by it. The clock is compared with nothing that
/// another machine wrote.
pub struct Deadline {
    clock: Arc<dyn Clock>,
    margin: Duration,
    valid_until: AtomicU64,
}

impl Deadline {
    /// A deadline that is past, on `clock`, with a margin of `margin`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, margin: Duration) -> Self {
        Self {
            clock,
            margin,
            valid_until: AtomicU64::new(0),
        }
    }

    /// The time that must be left for a destroying call to start.
    #[must_use]
    pub fn margin(&self) -> Duration {
        self.margin
    }

    /// Moves the deadline to `at`, a time of the clock, if that is later than it is now: a late
    /// renewal never shortens what an earlier one gave.
    pub fn extend_to(&self, at: u64) {
        self.valid_until.fetch_max(at, Ordering::SeqCst);
    }

    /// How much of the hold is left, none once it is past.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        let left = self
            .valid_until
            .load(Ordering::SeqCst)
            .saturating_sub(self.clock.now());
        Duration::from_millis(left)
    }

    /// Whether the deadline has not passed yet.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.remaining().is_zero()
    }

    /// Whether at least the margin is left, and something is, so that a call that destroys may
    /// start. A margin of zero therefore still refuses a deadline that was never renewed or has
    /// passed.
    #[must_use]
    pub fn allows_start(&self) -> bool {
        self.is_live() && self.remaining() >= self.margin
    }
}
