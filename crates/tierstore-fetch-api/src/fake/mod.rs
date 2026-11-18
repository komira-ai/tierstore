//! In-tree fakes of the read path's interfaces, so every task of section 18 can test against
//! them before the real implementations land (T10a grows them into the fake-backed suite of
//! section 14).
//!
//! - [`FakeUpstream`]: objects in memory, scripted answers per part (serve, hang, fail, truncate,
//!   corrupt), the `If-Match` rule, and a log of every request for count assertions.
//! - [`FakeEngine`]: waits answered from a script, or parked until the test completes them, and a
//!   log of every `ensure`.
//! - [`MemEntryStore`] and [`MemEntry`]: an in-memory entry store with atomic cell bytes that
//!   follows the CAS rules of section 2.
//!
//! None of them reads a clock: a test that needs time uses [`crate::clock::ManualClock`].

mod engine;
mod entry;
mod upstream;

pub use engine::FakeEngine;
pub use entry::{MemEntry, MemEntryStore};
pub use upstream::{FakeObject, FakeUpstream, PartScript};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a fake's mutex; a test that panicked while holding it has already failed, so the state
/// is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
