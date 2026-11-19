//! The in-process entry store (5.6; task T-M1): the store of macOS, where the NFS server runs
//! inside the tierstore process, so an entry needs no state memfd, no counter memfd and no
//! descriptor passing. It builds and runs on Linux too, where its tests and the `EntryStore`
//! conformance suite run it (section 14: the suite "passes on the memfd implementation (Linux)
//! and the in-process implementation (macOS path)").
//!
//! - [`InProcStore`]: one entry per pack under the same layout as the Linux store
//!   (`<cache>/packs-v2/aa/bb/<hash>.pack`, with the journal `.jnl` and the snapshot `.meta`
//!   beside it), each data file sparse and held under `flock(LOCK_EX)` by the store that opened
//!   it (7). Whether eviction may punch is probed once per store ([`punches_inside_a_file`]).
//! - [`InProcEntry`]: the cells of one pack as `SeqCst` atomics with the CAS rules of section 2,
//!   a reader counter per 16-cell group, holds and the regions of attached holders; its journal,
//!   snapshot, restart, `boot_id` and `recheck` are `tierstore-journal`'s, the shared code of
//!   5.6 (e). An entry opened again after a crash comes back from its journal: `present` in the
//!   same boot, `recheck` in another or with no boot id.
//! - [`read`] and [`InProcEntry::read_at`]: the reader protocol of 5.1 against the entry's own
//!   counters, with the post-read recheck, and a read that waits on the engine for cells that are
//!   not ready: the read of the NFS front.
//!
//! **What differs from Linux** (5.6): durability is `fcntl(F_FULLFSYNC)` on macOS and
//! `fdatasync` on Linux; a punch is `fallocate(PUNCH_HOLE | KEEP_SIZE)` on Linux, and on macOS,
//! where `F_PUNCHHOLE` has no safe wrapper and this workspace adds no `unsafe`, the probe finds
//! that the volume cannot punch inside a file, [`InProcStore::can_punch`] is `false`, and
//! eviction removes whole unheld entries only (the fallback 5.6 (b) names). Both are
//! `tierstore_journal::DirFiles`'.
//!
//! Every method of the store and of an entry may do blocking I/O: call it from a dedicated or
//! blocking thread, never from an async worker. [`read`] is async only in its waits.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, PoisonError};

mod entry;
mod read;
mod store;

pub use entry::InProcEntry;
pub use read::{MAX_ROUNDS, ReadError, ReadOutcome, read};
#[cfg(test)]
pub(crate) use store::PROBE_STEM;
pub use store::{InProcStore, punches_inside_a_file};

/// `SeqCst`, the one ordering of every access to a cell byte or counter (5.1: the reader and the
/// evictor each store to one location and then load the other's).
const SEQ: Ordering = Ordering::SeqCst;

/// The lock of a mutex whose data stays consistent across a panic of its holder (every critical
/// section here is a plain update of a map).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
