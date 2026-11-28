//! The one reader of the machine's clock: the time since the live rig started, which the cases
//! read through their rig (`Rig::now`), so that their tests move a time of their own.

use std::time::{Duration, Instant};

/// The time since it was made.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stopwatch(Instant);

impl Stopwatch {
    /// A stopwatch from now.
    #[expect(
        clippy::disallowed_methods,
        reason = "the clock of the live rig, the one reader of the machine's time"
    )]
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    /// The time since [`Stopwatch::start`].
    pub(crate) fn elapsed(self) -> Duration {
        self.0.elapsed()
    }
}
