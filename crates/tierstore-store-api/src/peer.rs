//! `PeerClient`: the typed client of the peer protocol (10.6). T-S6 implements it over HTTP/1.1
//! keep-alive ([`crate::proto`]), and its `StoreUpstream` implements the read path's `Upstream`
//! on top of it; the fake storage node of [`crate::fake`] implements it in memory.

use bytes::Bytes;
use futures::stream::BoxStream;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::upstream::{DriveSignals, Report};
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};

use crate::hot::ReplicaBatch;
use crate::proto::{
    CommitAnswer, CommitRecord, Envelope, GetHeaders, KeyCheck, KeyFacts, LocateRequest,
    LocateResponse, PingAnswer, ReplicaAnswer, SealAnswer, SealMessage, SourceShard, TombPage,
    TombPull,
};

/// Why a peer call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    /// No answer: connect error, reset, timeout on the injected clock, node down.
    #[error("node {node}: unreachable ({reason})")]
    Unreachable {
        /// The node.
        node: NodeId,
        /// What happened.
        reason: String,
    },
    /// `503`: a queue over its bound, a `stalled` or `indexing` drive or node (never `404` for
    /// those).
    #[error("node {node}: unavailable, retry after {retry_after_ms} ms ({signals:?})")]
    Unavailable {
        /// The node.
        node: NodeId,
        /// `Retry-After`.
        retry_after_ms: u64,
        /// The drive's signals, `x-ts-drive-state` among them.
        signals: DriveSignals,
    },
    /// `412`: the copy holds another generation.
    #[error("node {node}: holds generation {actual:?}")]
    PreconditionFailed {
        /// The node.
        node: NodeId,
        /// The generation it holds.
        actual: Option<Generation>,
    },
    /// `404`: no such file or shard (a freed stripe, a dropped copy).
    #[error("node {node}: not found")]
    NotFound {
        /// The node.
        node: NodeId,
    },
    /// The serving node found a crc mismatch in the bytes it would serve.
    #[error("node {node}: crc mismatch at {offset}")]
    Corrupt {
        /// The node.
        node: NodeId,
        /// The offset of the bad cell.
        offset: u64,
    },
    /// Any other status, or a body that does not decode.
    #[error("node {node}: status {status}: {reason}")]
    Protocol {
        /// The node.
        node: NodeId,
        /// The HTTP status.
        status: u16,
        /// What was wrong.
        reason: String,
    },
}

/// The bytes of a GET.
pub struct PeerBody {
    /// The `ETag`.
    pub generation: Generation,
    /// The serving drive's signals.
    pub signals: DriveSignals,
    /// The bytes.
    pub body: BoxStream<'static, Result<Bytes, PeerError>>,
}

impl std::fmt::Debug for PeerBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerBody")
            .field("generation", &self.generation)
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

/// A GET of a hot copy, a cold data shard or a degraded read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetTarget {
    /// `GET /store/v1/hot/<file>`.
    Hot {
        /// The file.
        file: String,
    },
    /// `GET /store/v1/cold/<drive_id>/<shard>`.
    Cold {
        /// The drive.
        drive: DriveId,
        /// The shard file.
        shard: String,
    },
    /// `GET /store/v1/degraded/<block_id>` with `x-ts-sources`.
    Degraded {
        /// The block.
        block: BlockId,
        /// The source shards.
        sources: Vec<SourceShard>,
    },
}

/// A GET.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerGet {
    /// What.
    pub target: GetTarget,
    /// `Range`: bytes of the file (hot, cold) or of the block (degraded).
    pub range: ByteRange,
    /// `If-Match`, class, job, rank.
    pub headers: GetHeaders,
    /// `x-ts-report` about an earlier response of the same range, if any.
    pub report: Option<Report>,
}

/// The peer protocol's client. Every call names the storage node it goes to; internal calls carry
/// the caller's envelope (sender, stamp, membership epoch) and their answers the callee's.
///
/// Section 14 rows: "Definitive miss (rule `N - min_acks + 1`)"; "Pack lost mid-job"; "Pinned
/// generation"; "Replication protocol under scripted crashes and message loss"; "Two sealers
/// under a partition"; "Cluster stamp and liveness rule"; "Create-only"; "Delete and GC";
/// "Degraded read"; "View states on scripted pings"; "S3 subset"; "LIST"; "Deployment profiles: ...
/// a `combined` node serves a part whose bytes are local without a network hop".
#[async_trait::async_trait]
pub trait PeerClient: Send + Sync {
    /// `POST /store/v1/locate`.
    async fn locate(
        &self,
        node: NodeId,
        req: LocateRequest,
    ) -> Result<Envelope<LocateResponse>, PeerError>;

    /// A hot, cold or degraded GET.
    async fn get(&self, node: NodeId, req: PeerGet) -> Result<PeerBody, PeerError>;

    /// `PUT /store/v1/hot/replica`: one batch; the replica's committed length, or why it refused.
    async fn replicate(
        &self,
        node: NodeId,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError>;

    /// `POST /store/v1/seal`.
    async fn seal(
        &self,
        node: NodeId,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError>;

    /// `POST /store/v1/key`.
    async fn key_check(
        &self,
        node: NodeId,
        check: Envelope<KeyCheck>,
    ) -> Result<Envelope<KeyFacts>, PeerError>;

    /// `POST /store/v1/descriptor`: one node's durable copy of a majority-committed record.
    async fn commit(
        &self,
        node: NodeId,
        record: Envelope<CommitRecord>,
    ) -> Result<Envelope<CommitAnswer>, PeerError>;

    /// `GET /store/v1/tomb?origin=<n>&after=<seq>&limit=<n>` ([`TombPull::query`]).
    async fn tomb_pull(
        &self,
        node: NodeId,
        pull: TombPull,
    ) -> Result<Envelope<TombPage>, PeerError>;

    /// `GET /store/v1/ping`.
    async fn ping(&self, node: NodeId) -> Result<Envelope<PingAnswer>, PeerError>;
}
