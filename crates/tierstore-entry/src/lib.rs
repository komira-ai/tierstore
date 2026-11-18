//! The node cache's entry store on Linux: task T1 of `docs/design/fetch-cache-serve.md`
//! (revision 7), behind the [`Entry`](tierstore_fetch_api::entry::Entry) and
//! [`EntryStore`](tierstore_fetch_api::entry::EntryStore) traits that `tierstore-fetch-api`
//! froze.
//!
//! What is here, by module:
//! - [`FileEntryStore`]: one entry per pack under `<cache>/packs-v2/aa/bb/<hash>.pack`, each a
//!   **sparse** data file (`ftruncate` to the pack size, never `fallocate`) held with
//!   `flock(LOCK_EX)` by the process that opened it (section 2, 7). A second process (or a second
//!   store in one process: `flock` is per open file) cannot open, read or evict the entry.
//! - [`FileEntry`]: the cells of one pack. Their bytes live in a **state memfd** the store
//!   creates, maps read-write, fills with the header of the state page and then seals with
//!   [`MOUNT_SEALS`](tierstore_fetch_api::page::MOUNT_SEALS), so a mount it is passed to can map
//!   it read-only and nothing else (5.1). Every transition is a `SeqCst` CAS on that mapping from
//!   a named predecessor (section 2): claim, abandon, publish, bad, the evictor's three steps and
//!   the settling of `recheck` and `bad`. Holds are refcounts per holder (7).
//! - [`MemfdCounters`]: the server's read-only mapping of one holder's counter memfd (5.1),
//!   remapped at a larger size when an `open-pack` names a region past it, before the
//!   `open-pack` is acknowledged. The memfd must be sealed against shrinking, so the mapping can
//!   never fault.
//! - [`Lifeline`] and [`Lifelines`]: a holder's end of the lifeline socket pair; only `POLLHUP`
//!   ends the holder (its counters, regions and holds are dropped), a protocol connection that
//!   flaps does not (5.1).
//!
//! The journal of this crate is the minimal one the evictor's order needs (`PUNCH` records
//! appended and synced once per batch before the punch, `ADD` records after the batch
//! `fdatasync`); replay, the snapshot, `boot_id` and `recheck` on restart are task T1b. An entry
//! opened here starts with every cell `absent` and an empty data file and journal.
//!
//! Every method does blocking I/O: call it from a dedicated or blocking thread, never from an
//! async worker.
//!
//! The crate is Linux only (memfd seals, `fallocate` punch); on another system it is empty.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod counters;
mod entry;
mod journal;
mod lifeline;
mod map;
mod state;
mod store;

pub use counters::{CountersError, MemfdCounters};
pub use entry::FileEntry;
pub use lifeline::{Lifeline, Lifelines};
pub use store::{FileEntryStore, PACKS_DIR, StoreError};

#[cfg(test)]
mod tests;
