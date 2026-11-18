//! The hybrid logical clock that assigns cluster stamps (10.2).
//!
//! Every storage node keeps one [`Hlc`]. It is advanced on every local event that needs a stamp
//! ([`Hlc::tick`]: a record appended, a tombstone, a block drop, a stripe free) and merged with
//! the stamp every peer message carries ([`Hlc::observe`]: `max`, then increment). So:
//! - the stamps one clock gives are strictly increasing, whatever the wall clock does (it may
//!   stand still or step back);
//! - a stamp taken after a node has heard of an event (directly, or through any chain of
//!   messages) is greater than that event's stamp, whatever the wall clocks of the nodes say:
//!   skew moves stamps away from wall time, never reorders causally related events;
//! - when the wall clock is ahead of every stamp seen, the next stamp is the wall clock's
//!   millisecond with counter 0, so stamps stay close to wall time on a node whose clock is right.
//!
//! The wall clock is injected: every call takes the caller's current wall-clock time in
//! milliseconds since the Unix epoch, so the clock is tested with any wall time, skew or step.
//! The next stamp is `max(last + 1, floor)`, where the floor is the wall clock's millisecond with
//! counter 0 and, for [`Hlc::observe`], also the received stamp plus one. A counter that passes
//! `0xffff` carries into the millisecond field, which keeps the order (stamps compare as `u64`)
//! and moves that clock at most one millisecond ahead per 65536 stamps in one millisecond.
//!
//! **After a restart** a node starts its clock above every stamp it can find in its own bytes
//! (records replayed, tombstone logs, seal and free records), either with [`Hlc::starting_after`]
//! or by [`Hlc::observe`] of each one, before it assigns a stamp: a wall clock stepped back
//! across the restart then still cannot give a stamp below one the node gave before.
//!
//! The clock is one `AtomicU64`; `tick` and `observe` are lock-free and may be called from any
//! number of threads, and no two calls on one clock return the same stamp.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::FormatError;
use crate::stamp::{STAMP_COUNTER_BITS, Stamp, WALL_MAX};

const WHAT: &str = "cluster stamp";

/// A node's hybrid logical clock.
#[derive(Debug, Default)]
pub struct Hlc {
    last: AtomicU64,
}

impl Hlc {
    /// A clock that has given no stamp: its first stamp is at least [`Stamp::ZERO`] plus one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }

    /// A clock whose next stamp is above `last` (a restart, see the module documentation).
    #[must_use]
    pub const fn starting_after(last: Stamp) -> Self {
        Self {
            last: AtomicU64::new(last.raw()),
        }
    }

    /// The last stamp the clock gave or merged.
    #[must_use]
    pub fn last(&self) -> Stamp {
        Stamp::from_raw(self.last.load(Ordering::Acquire))
    }

    /// The stamp of a local event at wall-clock time `wall_ms`: above every stamp the clock gave
    /// or merged before. An error only when the wall clock is beyond the stamp's range (the year
    /// 10889), or the clock has reached the largest stamp.
    pub fn tick(&self, wall_ms: u64) -> Result<Stamp, FormatError> {
        self.advance(wall_floor(wall_ms)?)
    }

    /// Merges `remote`, the stamp a peer message carries, received at wall-clock time `wall_ms`,
    /// and returns the stamp of the receipt: above `remote` and above every stamp the clock gave
    /// or merged before. Every later [`Hlc::tick`] is above `remote` too. The errors of
    /// [`Hlc::tick`], and an error for a `remote` that is the largest stamp.
    pub fn observe(&self, remote: Stamp, wall_ms: u64) -> Result<Stamp, FormatError> {
        let above_remote = remote.raw().checked_add(1).ok_or(overflow())?;
        self.advance(wall_floor(wall_ms)?.max(above_remote))
    }

    /// Sets the clock to `max(last + 1, floor)` and returns it.
    fn advance(&self, floor: u64) -> Result<Stamp, FormatError> {
        let next = |last: u64| last.checked_add(1).map(|n| n.max(floor));
        match self
            .last
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, next)
        {
            // `fetch_update` succeeded with this `last`, so `next(last)` is `Some`.
            Ok(last) => next(last).map(Stamp::from_raw).ok_or(overflow()),
            Err(_) => Err(overflow()),
        }
    }
}

/// The stamp value of `wall_ms` with counter 0.
fn wall_floor(wall_ms: u64) -> Result<u64, FormatError> {
    if wall_ms > WALL_MAX {
        return Err(FormatError::Invalid {
            what: WHAT,
            reason: "wall clock above 2^48 - 1 ms",
        });
    }
    Ok(wall_ms << STAMP_COUNTER_BITS)
}

fn overflow() -> FormatError {
    FormatError::Invalid {
        what: WHAT,
        reason: "the clock reached the largest stamp",
    }
}
