//! `Verifier`: the bounded hash pool (3.3, 4.7). T2 implements it with `ring`'s sha256 on
//! dedicated threads.

use bytes::Bytes;

use crate::list::CellDigest;

/// The priority of a cell in the pool: fills first, `recheck` cells at a lower priority (4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerifyPriority {
    /// A cell of a fill.
    Fill,
    /// A `recheck` cell (section 8).
    Recheck,
}

/// One cell to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellJob {
    /// The cell's index in its pack.
    pub cell: u32,
    /// The digest from the cell table.
    pub expected: CellDigest,
    /// The cell's bytes (a whole cell, or the short last one).
    pub bytes: Bytes,
    /// Its priority.
    pub priority: VerifyPriority,
}

/// What the pool found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// The sha256 equals the table's digest.
    Match,
    /// It does not.
    Mismatch,
}

/// The hash pool. `verify` waits while the pool's queue (32 cells) is full: that wait is the
/// receive pause of 4.7 (`part_hash_wait_seconds`), and the paused receive is the slow consumer
/// the drive's read-ahead buffer and slot release absorb (10.7).
///
/// Section 14 rows: "Reverse-order part completion: a read on the last cell is served after only
/// that cell lands"; "A 1 GiB file span: a read at the end waits only for its cells"; "Corrupt
/// bytes in one cell"; and T2's own rows: the pool bound and pause, per-cell verification.
#[async_trait::async_trait]
pub trait Verifier: Send + Sync {
    /// The verdict on `job`'s bytes, and the bytes back (so the buffer returns to its pool).
    async fn verify(&self, job: CellJob) -> (Verdict, Bytes);
}
