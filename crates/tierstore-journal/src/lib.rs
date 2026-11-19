//! The journal and snapshot of one node-cache entry (`docs/design/fetch-cache-serve.md`,
//! revision 7, section 8; section 18 row T1b), shared by the Linux entry store and the
//! in-process store of macOS (5.6 (e): "the journal, `recheck` and boot-id rules of section 8
//! are shared code"). The in-process store of `tierstore-entry` runs on it; the Linux store of
//! that crate does not yet.
//!
//! An entry has three files besides its state page: the sparse data file, the append-only
//! journal `<hash>.jnl` and the snapshot `<hash>.meta` (section 2). This crate owns the last two
//! and the order in which all three reach the disk:
//!
//! - **A cell's durability** ([`EntryJournal::commit_batch`], behind `Entry::commit_batch`):
//!   verify -> `pwrite` -> `present` (the entry does these) -> in a batch: one `fdatasync` of
//!   the data file -> the `durable` bit on each cell -> one `ADD` record per contiguous run ->
//!   one `fdatasync` of the journal. A journal record never precedes durable data.
//! - **Punches** ([`EntryJournal::punch`], behind `Entry::punch`): one `PUNCH` record per run,
//!   one journal `fdatasync` for the whole batch, and only then the punches. Replay applies `ADD`
//!   and `PUNCH` in order, so a punched cell is never restored.
//! - **Snapshot** ([`EntryJournal::fold`]): the committed cells, `pack_root`, size, last use and
//!   `boot_id` in `<hash>.meta`, by write-temp, `fdatasync`, rename and directory `fsync`, then a
//!   fresh journal of the next epoch; at close, after an eviction batch, and every
//!   [`FOLD_EVERY`] records.
//! - **Restart** ([`EntryJournal::recover`]): load the snapshot, replay the journal. Cells
//!   committed in this boot come back `present` (and durable); in another boot, or with no boot
//!   id on either side, every restored cell is `recheck`, which the mount does not serve.
//! - **Batching** ([`Committer`]): one committer per node commits every dirty entry every
//!   [`CommitPolicy::interval_ms`] or once [`CommitPolicy::pending_bytes`] are pending.
//!
//! The file operations go through [`EntryFiles`], so the same journal runs on the directory of
//! a cache on Linux or macOS ([`DirFiles`]) and on [`fake::MemFs`], a file system in memory that
//! keeps the durable and the volatile bytes of each file apart and can crash: a process crash
//! (the page cache survives) or a power loss (each write not yet synced survives or not, as a
//! test chooses). [`fake::FsEntry`] is a whole `Entry` over it, for the crash tests here and for
//! those of the evictor (section 7).
//!
//! Formats ([`format`]) are new in this crate and versioned for additive change only.

#![forbid(unsafe_code)]

mod boot;
mod cells;
mod committer;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod dir;
pub mod fake;
mod files;
pub mod format;
mod journal;

pub use boot::{BootId, BootIdError, same_boot};
pub use cells::{CellSet, CellStates, runs_of};
pub use committer::{CommitPolicy, CommitRound, Committer};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use dir::DirFiles;
pub use files::{EntryFiles, FileKind};
pub use journal::{DiscardReason, EntryJournal, FOLD_EVERY, Recovery, Trust};
