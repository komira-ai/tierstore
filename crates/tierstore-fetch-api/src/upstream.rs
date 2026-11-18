//! `Upstream`: where the engine's parts come from (4.6, 10.6, 10.10).
//!
//! The engine asks an upstream for a pack's location once per fill (`locate`, the `LOCATE` step
//! of 10.6) and then for parts (`get`), each a contiguous byte range from one source on one
//! device. The storage tier's implementation is T-S6's `StoreUpstream` over the peer protocol;
//! every location carries the generation, so every part carries `If-Match` from the first.
//! Single flight, claims, verification, the controller and hedging are the engine's and do not
//! change with the upstream.

use bytes::Bytes;
use futures::stream::BoxStream;
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};

use crate::ids::{ByteRange, Class, JobId, PackKey, Rank};

/// The device a source reads from, the unit the per-drive budget of 4.3 charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceRef {
    /// A storage node's hot device (NVMe, or the SSD partition of the NOW profile).
    Hot(NodeId),
    /// One hard drive of a storage node.
    Drive(NodeId, DriveId),
}

impl DeviceRef {
    /// The storage node of the device.
    #[must_use]
    pub fn node(self) -> NodeId {
        match self {
            Self::Hot(n) | Self::Drive(n, _) => n,
        }
    }
}

/// How a source serves its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A hot copy: a record range of a segment, or a large file (`GET /store/v1/hot/<file>`).
    Hot {
        /// The file the peer serves (a segment or a large file name, 10.3), as the location named
        /// it.
        file: String,
        /// The object's offset in that file.
        offset: u64,
    },
    /// A promoted copy in a cache segment (10.5): a hint, listed first, never durable.
    HotClean {
        /// The cache segment.
        file: String,
        /// The object's offset in it.
        offset: u64,
    },
    /// A cold data shard (`GET /store/v1/cold/<drive_id>/<shard>`).
    Cold {
        /// The block the extent is in (a chain object has one source per block).
        block: BlockId,
        /// The shard file name on the drive (10.4).
        shard: String,
        /// The object's offset in the block (equal to its offset in the shard).
        offset: u64,
    },
    /// A degraded read by the reconstructor (`GET /store/v1/degraded/<block_id>`): `k` range reads
    /// of the source drives, charged `k` parts against the client ceiling and one against each
    /// source drive (4.3).
    Degraded {
        /// The block.
        block: BlockId,
        /// The object's offset in the block.
        offset: u64,
        /// The `k` drives the reconstructor would read.
        sources: Vec<DeviceRef>,
    },
}

/// One place the bytes of an object, or of one extent of it, can be read from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    /// The device that serves the bytes (the reconstructor's node, for a degraded read).
    pub device: DeviceRef,
    /// How.
    pub kind: SourceKind,
    /// The bytes of the object this source covers (the whole object, or one block of a chain).
    pub extent: ByteRange,
}

impl Source {
    /// The stream charges a part from this source costs at the client ceiling (4.3): `k` for a
    /// degraded read, else 1.
    #[must_use]
    pub fn charge(&self) -> u32 {
        match &self.kind {
            SourceKind::Degraded { sources, .. } => {
                u32::try_from(sources.len()).unwrap_or(u32::MAX)
            }
            _ => 1,
        }
    }
}

/// Where an object is: its generation and size and every source of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The generation, known before the first byte (10.6): every part of the fill carries it as
    /// `If-Match` (section 9).
    pub generation: Generation,
    /// The object's size, if known.
    pub size: Option<u64>,
    /// The sources, best first (a promoted copy first, then hot copies, then cold extents).
    pub sources: Vec<Source>,
    /// Every generation the location's copies hold, when more than one (10.5: a key conflict is
    /// detected, not prevented; the fill pins one and excludes the others).
    pub conflicts: Vec<Generation>,
}

/// Why a miss is not definitive (10.6 step 3): the engine waits and retries, never EIO at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotDefinitive {
    /// A deciding node's index is not built.
    Indexing,
    /// Fewer than `N - min_acks + 1` live storage nodes answered.
    TooFewAnswers,
}

/// The answer of `locate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// The object exists.
    Found(Location),
    /// `N - min_acks + 1` live storage nodes, none `indexing`, answered absent.
    DefinitiveMiss,
    /// Anything less: not a miss.
    NotDefinitive(NotDefinitive),
}

/// A drive's state as a storage node reports it (`x-ts-drive-state`, 10.7, 10.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveState {
    /// Serving.
    Up,
    /// The owner's index or drive list is not built.
    Indexing,
    /// The owner's watchdog saw a call outstanding past `stall_after`.
    Stalled,
    /// Failed pings or error responses (a viewer's state).
    Suspect,
    /// No ping answered for three intervals (a viewer's state).
    Down,
    /// `FAILED` written; cleared only by an operator.
    Failed,
    /// The membership drains it.
    Drain,
    /// Not a candidate.
    Out,
}

impl DriveState {
    /// The `x-ts-drive-state` value.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Indexing => "indexing",
            Self::Stalled => "stalled",
            Self::Suspect => "suspect",
            Self::Down => "down",
            Self::Failed => "failed",
            Self::Drain => "drain",
            Self::Out => "out",
        }
    }

    /// The state an `x-ts-drive-state` value names.
    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        [
            Self::Up,
            Self::Indexing,
            Self::Stalled,
            Self::Suspect,
            Self::Down,
            Self::Failed,
            Self::Drain,
            Self::Out,
        ]
        .into_iter()
        .find(|s| s.header() == value)
    }

    /// Whether an answer in this state is a load signal for the budget: `stalled` and
    /// `indexing` are not (4.3: the copy is skipped and no permit changes).
    #[must_use]
    pub const fn is_load_signal(self) -> bool {
        !matches!(self, Self::Stalled | Self::Indexing)
    }
}

/// What a storage node says about the drive that served a response (10.7): the inputs of the
/// per-drive controller (4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DriveSignals {
    /// `x-ts-queue-wait-ms`: time the request waited for a stream slot.
    pub queue_wait_ms: Option<u32>,
    /// `x-ts-service-ms`.
    pub service_ms: Option<u32>,
    /// `x-ts-drive-queue`: the drive's queued requests.
    pub drive_queue: Option<u32>,
    /// `x-ts-drive-state`.
    pub drive_state: Option<DriveState>,
}

/// A request for one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetRequest {
    /// The object.
    pub key: PackKey,
    /// The source chosen from the location.
    pub source: Source,
    /// The bytes of the object wanted (inside the source's extent).
    pub range: ByteRange,
    /// `If-Match`: the fill's pinned generation (section 9).
    pub generation: Generation,
    /// `x-ts-class`.
    pub class: Class,
    /// `x-ts-job`.
    pub job: JobId,
    /// `x-ts-rank`.
    pub rank: Rank,
}

/// The stream of a part's bytes, in order.
pub type PartStream = BoxStream<'static, Result<Bytes, UpstreamError>>;

/// A part's response: what the headers said, then the bytes.
pub struct PartResponse {
    /// The generation of the bytes (`ETag`).
    pub generation: Generation,
    /// The serving drive's signals.
    pub signals: DriveSignals,
    /// The bytes.
    pub body: PartStream,
}

impl std::fmt::Debug for PartResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartResponse")
            .field("generation", &self.generation)
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

/// What a client tells a serving node about a response it got (`x-ts-report`, 4.6, 10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Report {
    /// A cell failed the client's digest: the node quarantines and repairs that copy or shard.
    Bitrot,
}

impl Report {
    /// The `x-ts-report` value.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Bitrot => "bitrot",
        }
    }
}

/// Why a locate or a part failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// The source holds no such object (a 404 on a shard of a freed stripe, a copy gone): the
    /// engine treats it as a lost location and asks again (4.6, 10.5).
    #[error("{key:?}: not found at {device:?}")]
    NotFound {
        /// The object.
        key: PackKey,
        /// The device asked.
        device: DeviceRef,
    },
    /// `412`: the source holds another generation; the copy is excluded from the fill.
    #[error("{key:?}: {device:?} holds another generation ({actual:?})")]
    PreconditionFailed {
        /// The object.
        key: PackKey,
        /// The device asked.
        device: DeviceRef,
        /// The generation it holds, if it said.
        actual: Option<Generation>,
    },
    /// `503`: a queue over its bound, a `stalled` or `indexing` drive, an index not built.
    #[error("{key:?}: {device:?} unavailable (retry after {retry_after_ms} ms, {signals:?})")]
    Unavailable {
        /// The object.
        key: PackKey,
        /// The device asked.
        device: DeviceRef,
        /// `Retry-After`, in milliseconds.
        retry_after_ms: u64,
        /// The drive's signals, `x-ts-drive-state` among them.
        signals: DriveSignals,
    },
    /// The serving node found a crc mismatch in what it would have served (section 9).
    #[error("{key:?}: {device:?} found a crc mismatch at {offset}")]
    Corrupt {
        /// The object.
        key: PackKey,
        /// The device.
        device: DeviceRef,
        /// The object offset of the bad cell.
        offset: u64,
    },
    /// A transport failure: reset, connect error, node down, or the stream ended early.
    #[error("{key:?}: transport to {device:?}: {reason}")]
    Transport {
        /// The object.
        key: PackKey,
        /// The device.
        device: DeviceRef,
        /// What happened.
        reason: String,
    },
}

/// Where the engine's bytes come from.
///
/// Section 14 rows: "A copy answers 412 to its generation"; "S3-front object: a stored object's
/// parts all carry `If-Match` from `LOCATE`"; "Pinned generation"; "Pack lost mid-job";
/// "Definitive miss (rule `N - min_acks + 1`) ... one `LOCATE` per key per fill"; "Corrupt bytes
/// in one cell"; "`digest=none` object".
#[async_trait::async_trait]
pub trait Upstream: Send + Sync {
    /// The object's location (`LOCATE`, 10.6), asked once per fill and again, bypassing every
    /// negative cache, before a fill is declared lost (4.6).
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError>;

    /// One part: `range` of the object from `source`, with `If-Match`, class, job and rank.
    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError>;

    /// Tells the node that served `range` from `source` what the client found.
    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report);
}
