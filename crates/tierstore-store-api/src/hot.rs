//! `HotStore`: the replicated hot tier of one storage node (10.3). T-S4 implements it.
//!
//! **Ack rule.** A PUT is acknowledged when the record or file is durable on `hot.min_acks`
//! copies (the writer's own `fdatasync` counts as one). **Replication** streams the same bytes at
//! the same offsets to every other member of the segment's replica set: a batch carries the
//! segment, the ballot, the start offset, the bytes, the running crc32c of the segment through
//! the batch and the writer's stamp; a replica rejects a batch whose start offset is not its
//! committed length, whose running crc does not match, or whose ballot is below its promise.
//! **The majority seal** decides a segment's sealed length once: prepare (promises, accepted
//! proposals, copies' committed and verified lengths), choose (re-propose the highest accepted,
//! else the `r`-th longest verified length, `r = max(1, q - replicas + min_acks)`), accept
//! (committed when a majority accepted durably), converge.

use bytes::Bytes;
use futures::stream::BoxStream;
use tierstore_format::ids::{BlockId, ExpiryClass, Generation, NodeId};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;

use crate::index::EntryLocation;

/// A small object to append (at most `hot.large_min`, 4 MiB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendRecord {
    /// The key.
    pub key: Vec<u8>,
    /// Its generation, computed while the body streamed in.
    pub generation: Generation,
    /// The writer's stamp, above any tombstone of the key the key check found.
    pub stamp: Stamp,
    /// Its expiry class.
    pub expiry: ExpiryClass,
    /// The data.
    pub data: Bytes,
}

/// A large object to store as a whole file.
pub struct LargePut {
    /// The key.
    pub key: Vec<u8>,
    /// Its size, when the PUT had a Content-Length (then `fallocate`).
    pub size: Option<u64>,
    /// Its expiry class.
    pub expiry: ExpiryClass,
    /// The writer's stamp.
    pub stamp: Stamp,
    /// The body, streamed once: hashed and teed to every replica in one pass (no whole-file
    /// buffering).
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

impl std::fmt::Debug for LargePut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LargePut")
            .field("key", &self.key)
            .field("size", &self.size)
            .field("expiry", &self.expiry)
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

/// An acknowledged PUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acked {
    /// The object's generation (the `ETag`).
    pub generation: Generation,
    /// Where it is.
    pub location: EntryLocation,
    /// Its length.
    pub length: u64,
    /// The copies durable when it was acknowledged (at least `min_acks`).
    pub copies: u8,
}

/// Why a segment is sealed (10.3 step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SealReason {
    /// It reached `hot.seal_bytes`, or a record did not fit.
    Size,
    /// Past `hot.seal_max_age` above `hot.seal_min_bytes`, or past `destage.max_age`.
    Age,
    /// The writer seals early: a replica silent for `hot.replica_stall`, or `down`/`out`.
    Early,
    /// A replica takes over from a writer that stopped answering.
    TakeOver,
}

/// A committed seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The segment.
    pub segment: BlockId,
    /// The ballot it committed under.
    pub ballot: Ballot,
    /// The sealed length and running crc.
    pub proposal: SealProposal,
}

/// One replication batch (10.3 step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaBatch {
    /// The segment.
    pub segment: BlockId,
    /// The writer's ballot for it.
    pub ballot: Ballot,
    /// The offset the bytes start at: the replica's committed length.
    pub start: u64,
    /// The crc32c of the segment's bytes `[0, start + bytes.len())`.
    pub running_crc: u32,
    /// The writer's stamp.
    pub stamp: Stamp,
    /// The bytes.
    pub bytes: Bytes,
}

/// A replica's acknowledgement: its committed length after the batch's `fdatasync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaAck {
    /// The committed length.
    pub committed: u64,
}

/// Why a replica refused a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplicaReject {
    /// The start offset is not the replica's committed length.
    #[error("batch starts at {start}, the copy has {committed} committed")]
    Offset {
        /// The batch's start.
        start: u64,
        /// The copy's committed length.
        committed: u64,
    },
    /// The running crc does not match the copy's bytes and the batch.
    #[error("running crc32c {sent:#010x} does not match {computed:#010x}")]
    Crc {
        /// What the batch carried.
        sent: u32,
        /// What the copy computed.
        computed: u32,
    },
    /// The batch's ballot is below the copy's promise: the writer is fenced.
    #[error("ballot {ballot:?} is below the promise {promised:?}")]
    Ballot {
        /// The batch's ballot.
        ballot: Ballot,
        /// The copy's promise.
        promised: Ballot,
    },
    /// The segment is sealed on this copy.
    #[error("the segment is sealed")]
    Sealed,
}

/// A copy's state, returned with a promise (10.3 step 4, prepare).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyState {
    /// Its committed length.
    pub committed: u64,
    /// Its crc-verified prefix; below `committed` it counts as not answering for Choose.
    pub verified: u64,
    /// The running crc32c through `verified`.
    pub running_crc: u32,
}

/// A promise to a `seal-prepare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Promise {
    /// The ballot promised.
    pub ballot: Ballot,
    /// The proposal this node accepted earlier, with its ballot.
    pub accepted: Option<(Ballot, SealProposal)>,
    /// This node's copy of the segment, if it holds one (nodes without a copy vote too).
    pub copy: Option<CopyState>,
}

/// Why a seal step was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealRefused {
    /// The node promised a higher ballot.
    #[error("promised {promised:?}")]
    Promised {
        /// The higher ballot.
        promised: Ballot,
    },
    /// The segment's seal is committed: the committed proposal, which never changes.
    #[error("committed at {0:?}")]
    Committed(SealProposal),
}

/// Why the hot store refused or failed a PUT or a seal.
#[derive(Debug, thiserror::Error)]
pub enum HotError {
    /// `503 Retry-After`: fewer than `min_acks` copies could be made, the key check was not
    /// definitive, hot dirty bytes above `hot.dirty_slow`, or a minority partition.
    #[error("unavailable ({reason:?}), retry after {retry_after_ms} ms")]
    Unavailable {
        /// Why.
        reason: Unavailable,
        /// `Retry-After`.
        retry_after_ms: u64,
    },
    /// `412`: the key holds another generation.
    #[error("the key holds generation {existing}")]
    Conflict {
        /// The generation the key holds.
        existing: Generation,
    },
    /// `507`: hot fill above `hot.dirty_reject` after reclaim.
    #[error("the hot tier is full")]
    Full,
    /// A copy's generation trailer did not match the writer's hash: that copy is abandoned.
    #[error("node {node}: generation mismatch")]
    GenerationMismatch {
        /// The replica whose hash differed.
        node: NodeId,
    },
    /// The device failed.
    #[error("{op}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

/// Why a PUT is `503`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unavailable {
    /// Fewer than `hot.min_acks` copies could be made.
    TooFewCopies,
    /// The all-node key check was not definitive.
    NotDefinitive,
    /// Hot dirty bytes are above `hot.dirty_slow`.
    Throttled,
    /// This node is alone on its side of a partition: it cannot seal or acknowledge.
    Minority,
}

/// The hot store of one node.
///
/// Section 14 rows: "Group commit (fake NVMe, injected clock)"; "Replication protocol under
/// scripted crashes and message loss"; "Seal and roll"; "Two sealers under a partition
/// (deterministic simulation)"; "Seal triggers"; "Large object PUT"; "Create-only"; "Small or
/// large path"; "Weaker acknowledgement is documented behaviour"; "Backpressure marks on a
/// scripted hot fill"; "Hot-only mode".
#[async_trait::async_trait]
pub trait HotStore: Send + Sync {
    /// Appends a small object to the open segment of its writer, replica set and expiry class and
    /// returns when the ack rule is met; the index entry is visible from then.
    async fn append(&self, record: AppendRecord) -> Result<Acked, HotError>;

    /// Stores a large object as a whole file on every member of its replica set (`.up`, `fsync`,
    /// create-only check, `renameat2(RENAME_NOREPLACE)`, directory `fsync`).
    async fn put_file(&self, put: LargePut) -> Result<Acked, HotError>;

    /// Seals `segment` by majority under `ballot` (above any the sealer has seen).
    async fn seal(
        &self,
        segment: BlockId,
        ballot: Ballot,
        reason: SealReason,
    ) -> Result<Committed, HotError>;

    /// A replica's side of replication: appends the batch at its offset and `fdatasync`s.
    async fn replicate(&self, batch: ReplicaBatch) -> Result<ReplicaAck, ReplicaReject>;

    /// A voter's side of `seal-prepare`: promises `ballot` durably unless it promised higher.
    async fn promise(&self, segment: BlockId, ballot: Ballot) -> Result<Promise, SealRefused>;

    /// A voter's side of `seal-accept`: accepts the proposal durably unless it promised higher.
    async fn accept(
        &self,
        segment: BlockId,
        ballot: Ballot,
        proposal: SealProposal,
    ) -> Result<(), SealRefused>;
}
