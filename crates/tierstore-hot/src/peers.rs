//! What the hot store needs from the other storage nodes: the internal routes of 10.6 that carry
//! the hot tier's protocol, as one trait the peer service implements over HTTP (T-S6) and the
//! tests implement over a simulated network.
//!
//! - `PUT /store/v1/hot/replica`: a segment batch ([`HotPeers::replicate`]) or a large-file
//!   stream ([`HotPeers::put_large`]);
//! - `POST /store/v1/seal`: prepare, accept and the `sealed` broadcast ([`HotPeers::seal`]);
//! - `GET /store/v1/hot/<file>` with `Range`: the bytes a converging or repaired copy fetches
//!   ([`HotPeers::fetch`]).
//!
//! Every call carries the caller's [`Envelope`] (sender, cluster stamp, membership epoch) and its
//! answer the callee's, so the cluster stamps merge on every exchange (10.2). The node side of
//! each route is a method of [`crate::HotNode`].

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::ids::{ExpiryClass, Generation, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::stamp::Stamp;
use tierstore_store_api::hot::ReplicaBatch;
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::proto::{Envelope, ReplicaAnswer, SealAnswer, SealMessage};

/// The head of a large-file stream to a replica (10.3 "Large objects").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeOffer {
    /// The key.
    pub key: Vec<u8>,
    /// Its size when the PUT had a Content-Length (the replica then `fallocate`s).
    pub size: Option<u64>,
    /// Its expiry class.
    pub expiry: ExpiryClass,
    /// The writer's stamp.
    pub stamp: Stamp,
}

/// One piece of a large-file stream: the body's bytes in order, then the generation trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LargeChunk {
    /// The next bytes of the body.
    Data(Bytes),
    /// The generation the writer computed over the whole body: the replica checks it against
    /// its own hash and commits only on a match. A stream that ends without it is abandoned.
    Trailer(Generation),
}

/// A replica's answer to a large-file stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LargeAnswer {
    /// The copy is committed: `fsync`, create-only check, rename, directory `fsync`.
    Committed,
    /// The trailer's generation is not the replica's hash: its `.up` is unlinked.
    Mismatch {
        /// What the replica computed.
        computed: Generation,
    },
    /// The key holds another generation on the replica: nothing is left behind.
    Conflict {
        /// The generation the key holds there.
        existing: Generation,
    },
    /// The stream ended without a trailer (the writer abandoned the copy, or its connection
    /// closed): the `.up` is unlinked.
    Abandoned,
}

/// The other storage nodes, as the hot store calls them.
#[async_trait::async_trait]
pub trait HotPeers: Send + Sync {
    /// `PUT /store/v1/hot/replica`: one segment batch to `node`.
    async fn replicate(
        &self,
        node: NodeId,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError>;

    /// `POST /store/v1/seal`: one message of the majority seal to `node`.
    async fn seal(
        &self,
        node: NodeId,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError>;

    /// `PUT /store/v1/hot/replica` with a large-file stream: `node` writes its own `.up` as the
    /// chunks arrive and answers once the stream has ended.
    async fn put_large(
        &self,
        node: NodeId,
        offer: Envelope<LargeOffer>,
        body: BoxStream<'static, LargeChunk>,
    ) -> Result<Envelope<LargeAnswer>, PeerError>;

    /// `GET /store/v1/hot/<file>` with `Range`: bytes of `node`'s copy of `file`.
    async fn fetch(
        &self,
        node: NodeId,
        file: HotName,
        range: ByteRange,
    ) -> Result<Bytes, PeerError>;
}

/// Runs a background task of the hot store: a segment's committer, a replica's sender, an early
/// seal. The daemon passes its runtime's `spawn`; the tests pass a deterministic executor.
pub trait Spawn: Send + Sync {
    /// Runs `task` to completion in the background.
    fn spawn(&self, task: BoxFuture<'static, ()>);
}
