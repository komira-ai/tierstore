//! `Index`: a storage node's ordered map `key -> entry` (10.2). T-S3 implements it in an embedded
//! ordered store (`fjall`, with `redb` behind the same trait as the measured alternative). The
//! store is a cache of the bytes: every fact in it is rebuildable from segments, trailers,
//! `blk/*.idx` files, shard footers and the tombstone log.
//!
//! **The liveness rule.** A record `(K, generation, stamp s)` is live iff no tombstone of `K` has
//! a stamp greater than `s`. A relocate carries the stamp of the record it moves and, at equal
//! stamps, supersedes it; it applies only while the entry still holds that stamp. The newest fact
//! about a key wins.

use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LargeFile, LogRecord, Tombstone};

/// Where an entry's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryLocation {
    /// A record in a block (hot segment, pack block): the record header's offset. Destage does not
    /// change it (an object's offset in a sealed segment is its offset in the cold block).
    Block {
        /// The block.
        block: BlockId,
        /// The record's offset in it.
        offset: u64,
    },
    /// A large hot file (10.3).
    Large(LargeFile),
    /// A chain: consecutive blocks from `first`, each one slice (10.2).
    Chain {
        /// The first block.
        first: BlockId,
    },
}

/// A promoted copy (10.5): a cache hint, not a stamped fact. An index rebuild drops it, and so does
/// a change of the promoting node's boot epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotClean {
    /// The node holding the cache segment.
    pub node: NodeId,
    /// The node's boot epoch the segment was made in.
    pub boot: u64,
    /// The cache segment's sequence number.
    pub seq: u64,
    /// The record's offset in it.
    pub offset: u64,
}

/// An index entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// The object's generation.
    pub generation: Generation,
    /// Where its bytes are.
    pub location: EntryLocation,
    /// Its size (a chain's total size).
    pub length: u64,
    /// The stamp of the record (of the relocate, after one).
    pub stamp: Stamp,
    /// The record's flags.
    pub flags: RecordFlags,
    /// Blocks the entry was relocated from that are not freed yet: a tombstone of the key names
    /// them too (10.2).
    pub prior_blocks: Vec<BlockId>,
    /// The promoted copy, if any.
    pub promoted: Option<HotClean>,
}

/// What the index holds for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A live entry.
    Live(IndexEntry),
    /// A tombstone entry, with its stamp (never listed; a miss for reads).
    Deleted(Stamp),
    /// Nothing.
    Absent,
}

/// What `put_if_absent` did, under the key lock (10.2, 10.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// The entry is published.
    Created,
    /// The key holds the same generation: an identical PUT (200).
    Identical(IndexEntry),
    /// The key holds another generation: a conflict (412).
    Conflict(IndexEntry),
    /// The key's tombstone stamp is greater than `entry.stamp` (the record would be dead on
    /// arrival): the writer must stamp above it.
    Stale(Stamp),
}

/// What `delete` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// A live entry became a tombstone entry; the live counts of its block are decreased.
    Deleted(IndexEntry),
    /// There was no live entry; the tombstone entry is kept anyway (every node keeps every
    /// tombstone).
    NoLiveEntry,
    /// The live entry's stamp is not below the tombstone's: it stays live (a re-PUT after the
    /// delete). A tombstone at stamp `t` deletes the records with a stamp below `t`; equal stamps
    /// do not arise, because a writer stamps a tombstone above every record it found and a
    /// re-PUT above the tombstone.
    Newer(IndexEntry),
}

/// The replay mark (10.2 recovery step 1): what the flushed store reflects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayMark {
    /// Per open segment, the committed length the index reflects.
    pub open_segments: Vec<(BlockId, u64)>,
    /// Per origin, the position (sequence number) of the last entry of that origin's tombstone log
    /// applied; 0 is none yet. Replaying a [`Fact::Log`] moves its origin's cursor to the record's
    /// sequence number.
    pub tomb_cursors: Vec<(NodeId, u64)>,
}

/// One fact replay applies, in the order recovery reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    /// A record of a segment, a trailer, a `blk/*.idx` file or a large file name.
    Record {
        /// The key.
        key: Vec<u8>,
        /// The entry the record makes.
        entry: IndexEntry,
    },
    /// A relocate: a GC rewrite's record or a pack block's trailer entry, at the stamp of the
    /// record it moves.
    Relocate {
        /// The key.
        key: Vec<u8>,
        /// The generation moved.
        generation: Generation,
        /// The stamp moved.
        stamp: Stamp,
        /// The new location.
        to: EntryLocation,
    },
    /// An entry of a tombstone log with its position: a tombstone, a block drop (hot-only mode) or
    /// a tomb drop is applied, an entry of an unknown kind only moves the cursor of its origin.
    Log(LogRecord),
}

/// What a replay applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayStats {
    /// Facts read.
    pub facts: u64,
    /// Facts that changed an entry (the rest were older than what the index held).
    pub applied: u64,
}

/// The live entries and bytes a node indexes in one block (the stripe-free count, 10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveCount {
    /// Live entries.
    pub entries: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// Why the index failed.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The index is not built yet: the node answers `indexing` (never a miss).
    #[error("the index is being built")]
    Indexing,
    /// The embedded store failed.
    #[error("index store: {op}: {reason}")]
    Store {
        /// What was being done.
        op: &'static str,
        /// What the store said.
        reason: String,
    },
}

/// A storage node's index.
///
/// Section 14 rows: "Cluster stamp and liveness rule (deterministic simulation with skewed wall
/// clocks)" (a) to (g); "Create-only"; "Index recovery: restart from the replay mark ... the same
/// index results with `fjall` and `redb` behind the trait; until built it answers `indexing`";
/// "LIST: output equals a model ... a rolled-up prefix is skipped by one seek per node"; "Delete
/// and GC" (the live counts); "Hot-only mode" (a dropped key is a definitive miss, a rebuilt index
/// does not resurrect it).
pub trait Index: Send + Sync {
    /// The key's entry.
    fn get(&self, key: &[u8]) -> Result<Lookup, IndexError>;

    /// Publishes `entry` under `key` unless the key holds a live entry or a tombstone whose stamp
    /// is greater than `entry.stamp` ([`PutOutcome::Stale`]); the caller holds the key lock.
    fn put_if_absent(&self, key: &[u8], entry: IndexEntry) -> Result<PutOutcome, IndexError>;

    /// Applies a tombstone of the log (idempotent per key and stamp): the tombstone names the key,
    /// its stamp and the blocks and large files that may list an older record of the key (10.2),
    /// so the live counts of those blocks are decreased when a live entry is deleted.
    fn delete(&self, tombstone: &Tombstone) -> Result<DeleteOutcome, IndexError>;

    /// Up to `limit` live keys with `prefix`, in key order, strictly after `after` (a seek, so a
    /// rolled-up prefix is skipped by asking after its last possible key). Tombstone entries are
    /// never listed.
    fn list(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, IndexEntry)>, IndexError>;

    /// Flushes the store and records `mark` in the same flush (10.2 step 1).
    fn flush_mark(&self, mark: &ReplayMark) -> Result<(), IndexError>;

    /// The mark of the last flush, if any.
    fn replay_mark(&self) -> Result<Option<ReplayMark>, IndexError>;

    /// Applies facts in order; for each key the newest fact by stamp wins.
    fn replay(&self, facts: &mut dyn Iterator<Item = Fact>) -> Result<ReplayStats, IndexError>;

    /// The live entries and bytes this node indexes in `block`.
    fn live(&self, block: BlockId) -> Result<LiveCount, IndexError>;
}
