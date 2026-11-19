//! The node cache's entry stores, behind the [`Entry`](tierstore_fetch_api::entry::Entry) and
//! [`EntryStore`](tierstore_fetch_api::entry::EntryStore) traits that `tierstore-fetch-api` froze
//! (`docs/design/fetch-cache-serve.md`, revision 7): the Linux store of task T1 and the
//! in-process store of task T-M1 ([`inproc`]).
//!
//! **The in-process store** ([`inproc`], Linux and macOS): the store of macOS, where the NFS
//! server runs in the tierstore process (5.6): cells as atomics on the heap, the entry's own
//! reader counters, the journal, restart and `recheck` of `tierstore-journal`, `F_FULLFSYNC` on
//! macOS, and whole-entry eviction where the volume cannot punch.
//!
//! **The Linux store**, by module:
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
//! The Linux store is Linux only (memfd seals, `fallocate` punch); the in-process store builds on
//! Linux and macOS. On another system the crate is empty.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(target_os = "linux")]
mod counters;
#[cfg(target_os = "linux")]
mod entry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod inproc;
#[cfg(target_os = "linux")]
mod journal;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod layout;
#[cfg(target_os = "linux")]
mod lifeline;
#[cfg(target_os = "linux")]
mod map;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod store;

#[cfg(target_os = "linux")]
pub use counters::{CountersError, MemfdCounters};
#[cfg(target_os = "linux")]
pub use entry::FileEntry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use layout::{PACKS_DIR, StoreError};
#[cfg(target_os = "linux")]
pub use lifeline::{Lifeline, Lifelines};
#[cfg(target_os = "linux")]
pub use store::FileEntryStore;

#[cfg(test)]
#[macro_use]
mod conformance_cases;
#[cfg(test)]
mod inproc_tests;
#[cfg(test)]
mod tests;
