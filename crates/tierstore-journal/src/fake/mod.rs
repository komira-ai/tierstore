//! Fakes for crash tests of the journal and of what is built on it (the evictor of section 7,
//! the fake-backed suite of section 14):
//!
//! - [`MemFs`]: an entry's files in memory with durable and volatile bytes kept apart, injected
//!   failures, a crash point, and the file system a restart sees after a process crash or a
//!   power loss ([`Crash`], [`Keep`]); it counts every operation ([`Op`]).
//! - [`FsEntry`]: an `Entry` (the frozen trait) whose cells are atomics in memory and whose data
//!   file, journal and snapshot are a [`MemFs`], journaled by [`crate::EntryJournal`]; it can be
//!   reopened from a crashed file system.
//!
//! Neither reads a clock.

mod entry;
mod fs;

pub use entry::FsEntry;
pub use fs::{Crash, Keep, MemFs, Op};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a fake's mutex; a test that panicked while holding it has already failed, so the state
/// is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
