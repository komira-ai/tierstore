//! The embedded ordered store under the index: a few ordered keyspaces with atomic batches and an
//! explicit persist.
//!
//! The index needs very little of a store: point reads, ascending scans from a bound, an atomic
//! batch of puts and deletes across keyspaces, and a persist that makes every batch committed so
//! far durable. [`Backend`] is exactly that, so the same index code runs on `fjall` (an LSM-tree,
//! the design's choice), on `redb` (a copy-on-write B-tree, the measured alternative of 10.2) and
//! on [`MemBackend`] (in memory, which models a crash as the loss of any suffix of the batches
//! committed since the last persist).
//!
//! **Durability contract.** A committed batch is visible to every later read at once. It is
//! durable only after a later [`Backend::persist`] returns: a crash keeps the batches committed
//! before the last persist and an arbitrary prefix of the batches after it, each batch whole or
//! not at all. The index is written so that every such prefix is a consistent state from which a
//! replay from the replay mark gives the never-restarted index (10.2 recovery).

use std::ops::Bound;

pub mod fjall;
pub mod mem;
pub mod redb;

pub use mem::MemBackend;

/// Key-value pairs of a scan, in key order.
pub type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// A keyspace of the index's store. Each holds byte keys in ascending byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Space {
    /// Object key -> its entry (live or tombstone).
    Entries,
    /// Block id (8 bytes, big-endian) -> the live entries and bytes this node indexes in it.
    Live,
    /// Fixed names -> the store's own facts: format version, replay mark, tomb log cursors, boot
    /// epochs of the nodes whose promoted copies are hinted.
    Meta,
    /// Block id -> the block table's row.
    Blocks,
    /// Stripe id -> the stripe's descriptor and its free, once committed.
    Stripes,
    /// Object key -> the tombstones of the key held until their drop, by stamp.
    Tombs,
    /// A tomb log position (origin, 1 byte; sequence, 8 bytes big-endian) -> the tombstone at
    /// that position and the nodes that reported its drop.
    TombPos,
    /// A name -> nothing: the blocks and large files whose free this node has applied.
    Freed,
}

impl Space {
    /// Every keyspace, in a fixed order.
    pub const ALL: [Self; 8] = [
        Self::Entries,
        Self::Live,
        Self::Meta,
        Self::Blocks,
        Self::Stripes,
        Self::Tombs,
        Self::TombPos,
        Self::Freed,
    ];

    /// The keyspace's name in the store (a file or table name, ASCII).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Live => "live",
            Self::Meta => "meta",
            Self::Blocks => "blocks",
            Self::Stripes => "stripes",
            Self::Tombs => "tombs",
            Self::TombPos => "tomb_pos",
            Self::Freed => "freed",
        }
    }
}

/// One write of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Sets `key` of `space` to `value`.
    Put {
        /// The keyspace.
        space: Space,
        /// The key.
        key: Vec<u8>,
        /// The value.
        value: Vec<u8>,
    },
    /// Removes `key` from `space` (nothing if absent).
    Delete {
        /// The keyspace.
        space: Space,
        /// The key.
        key: Vec<u8>,
    },
}

/// Why the store failed.
#[derive(Debug, thiserror::Error)]
#[error("{engine}: {op}: {reason}")]
pub struct BackendError {
    /// The engine (`fjall`, `redb`, `memory`).
    pub engine: &'static str,
    /// What was being done.
    pub op: &'static str,
    /// What the engine said.
    pub reason: String,
}

impl BackendError {
    /// An error of `engine` while doing `op`.
    pub fn new(engine: &'static str, op: &'static str, reason: impl std::fmt::Display) -> Self {
        Self {
            engine,
            op,
            reason: reason.to_string(),
        }
    }
}

/// The ordered store the index runs on. See the module documentation for the durability contract.
pub trait Backend: Send + Sync {
    /// The engine's name, for errors and reports.
    fn engine(&self) -> &'static str;

    /// The value of `key` in `space`.
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError>;

    /// Up to `limit` pairs of `space` from `from` on, in ascending key order.
    fn scan(&self, space: Space, from: Bound<&[u8]>, limit: usize) -> Result<Pairs, BackendError>;

    /// Applies `ops` in order as one atomic batch, visible at once, durable after the next
    /// [`Backend::persist`].
    fn commit(&self, ops: Vec<Op>) -> Result<(), BackendError>;

    /// Makes every batch committed so far durable.
    fn persist(&self) -> Result<(), BackendError>;
}
