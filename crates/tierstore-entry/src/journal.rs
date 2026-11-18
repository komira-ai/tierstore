//! The journal of one entry, `<hash>.jnl` beside its data file (section 8): the records the
//! evictor and the committer append, each batch with one sync.
//!
//! This is only the write side the order of section 8 needs: a batch of `PUNCH` records is synced
//! before any of its runs is punched, and a batch of `ADD` records is appended only after the data
//! file's `fdatasync`. Replay, the snapshot `<hash>.meta`, `boot_id` and `recheck` belong to the
//! journal task (T1b), which owns the record format; the records here are internal to this crate
//! and no release reads them.
//!
//! Record (16 bytes, little-endian): kind (1 = `ADD`, 2 = `PUNCH`), three zero bytes, the run's
//! first cell (`u32`), its cell count (`u32`), and the crc32c of the 12 bytes before it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use tierstore_fetch_api::ids::CellRun;

/// What a record says about its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// The run's cells are durable on disk (after the batch `fdatasync`).
    Add = 1,
    /// The run is about to be punched (synced before the punch).
    Punch = 2,
}

/// The length of a record.
pub(crate) const RECORD_LEN: usize = 16;

/// An entry's journal, open for appending.
#[derive(Debug)]
pub(crate) struct Journal {
    file: File,
}

impl Journal {
    /// An empty journal at `path` (an existing one is truncated: this crate restores nothing).
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { file })
    }

    /// Appends one `op` record per non-empty run of `runs` and syncs the journal once; the number
    /// of records. No non-empty run appends nothing and does not sync.
    pub(crate) fn append(&mut self, op: Op, runs: &[CellRun]) -> io::Result<u32> {
        let records: Vec<[u8; RECORD_LEN]> = runs
            .iter()
            .filter(|r| r.count > 0)
            .map(|r| record(op, *r))
            .collect();
        if records.is_empty() {
            return Ok(0);
        }
        self.file.write_all(&records.concat())?;
        self.file.sync_all()?;
        // One record per run of a call; a caller never passes 2^32 runs.
        Ok(records.len() as u32)
    }
}

/// The bytes of one record.
pub(crate) fn record(op: Op, run: CellRun) -> [u8; RECORD_LEN] {
    let mut r = [0u8; RECORD_LEN];
    r[0] = op as u8;
    r[4..8].copy_from_slice(&run.first.to_le_bytes());
    r[8..12].copy_from_slice(&run.count.to_le_bytes());
    let crc = crc32c::crc32c(&r[..12]);
    r[12..16].copy_from_slice(&crc.to_le_bytes());
    r
}
