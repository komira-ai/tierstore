//! Eviction and admission of the node cache: task T8 of `docs/design/fetch-cache-serve.md`
//! (revision 7), section 7, with the evictor's per-cell protocol of 5.1 and the journaled punch
//! of section 8. It runs against the frozen [`EntryStore`](tierstore_fetch_api::entry::EntryStore)
//! and [`Entry`](tierstore_fetch_api::entry::Entry) traits, so it drives the Linux store (T1) and
//! the in-process store of macOS (T-M1) alike.
//!
//! What is here:
//! - [`Evictor::evict`]`(target)`: brings the cache's use (`present + claimed + recheck` cells x
//!   the cell size, section 7) down to `target` bytes. First whole **unheld** entries, least
//!   recently used first, by [`EntryStore::remove`](tierstore_fetch_api::entry::EntryStore::remove).
//!   Then, if only held entries remain, **cell-group eviction**: the 16-cell groups untouched
//!   longest by their touch epoch, each `present` cell won by its own CAS `present -> evicting`,
//!   the groups' reader counters waited on until they read 0 or `evict_wait` (2 s on the injected
//!   clock) passes, the cells of a group that stayed busy put back `present`, and the rest
//!   released by **one** [`Entry::punch`](tierstore_fetch_api::entry::Entry::punch) per entry
//!   (one batch of `PUNCH` records, one journal `fsync`, then the hole punches) and CAS
//!   `evicting -> absent`. The storage tier is not told: it does not evict for capacity, so a
//!   hold sends no `touch` (7, 10.5).
//! - [`Admission::reserve`]`(estimate)`: the jobscheduler's question at job assignment. The
//!   estimate is the job's needed cells minus those already `present`, in bytes. It is refused
//!   (`oversize`) when it exceeds the budget, (`budget`) when the bytes already reserved plus it
//!   exceed 0.9 x the budget, and (`enospc`) when the cache device could not hold them even with
//!   every unreserved cached byte evicted. An admitted estimate is a [`Reservation`], charged
//!   until it is dropped.
//! - The **grace window**: [`Evictor::evict`] does nothing for 60 s after the evictor was made at
//!   process start, so the mounts rebind their holds first ([`EvictError::Grace`]).
//! - The **evictor lock** [`EvictorLock`]: `flock` on `packs-v2/.lock`, so exactly one process of
//!   a cache directory evicts. An entry another process has open is locked by it (T1) and so is
//!   never in this process's store: it is neither read nor evicted here.
//!
//! The metrics of section 12 this component updates are the node cache's `held_bytes`, `holds`,
//! `admission_refusals_total{reason}`, `punched_bytes_total`, `evictions_total` and
//! `evict_timeouts_total`.
//!
//! Every method of [`Evictor`] does blocking I/O through the entry store (the journal's `fsync`,
//! `fallocate`, unlinks) and waits for readers: call it from a dedicated thread, never from an
//! async worker.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod admission;
mod config;
mod evictor;
mod lock;
mod pause;
mod protocol;
mod space;

pub use admission::{Admission, Reservation, ReserveError};
pub use config::{CELL_BYTES, EvictConfig};
pub use evictor::{EvictError, EvictFailure, EvictReport, Evictor};
pub use lock::{EVICTOR_LOCK, EvictorLock, LockError};
pub use pause::{Pause, SleepPause};
pub use space::{CacheDevice, Statvfs};

#[cfg(test)]
mod tests;
