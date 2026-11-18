//! [`HotNode`]: the hot store of one storage node, the [`HotStore`] of `tierstore-store-api`.
//!
//! The node's state is in one mutex, held only for short critical sections and never across an
//! `.await`; every wait is on a segment's progress channel, a key lock or a seal-state lock.

mod converge;
mod keys;
mod large;
mod read;
mod reclaim;
mod replica;
mod scrub;
mod sealer;
mod startup;
mod tick;
mod vote;
mod writer;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use tierstore_fetch_api::clock::Clock;
use tierstore_format::alloc::IdAllocator;
use tierstore_format::hlc::Hlc;
use tierstore_format::ids::{BlockId, ExpiryClass, NodeId};
use tierstore_format::seal::{Ballot, SealProposal, SealState};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::LargeFile;
use tierstore_index::KvIndex;
use tierstore_index::backend::Backend;
use tierstore_metrics::Hot;
use tierstore_store_api::hot::{
    AppendRecord, Acked, Committed, HotError, HotStore, LargePut, Promise, ReplicaAck,
    ReplicaBatch, ReplicaReject, SealReason, SealRefused, Unavailable,
};
use tierstore_store_api::index::Index;
use tierstore_store_api::placement::Placement;
use tierstore_store_api::proto::Envelope;
use tierstore_store_api::view::{Admin, View};

use crate::config::HotConfig;
use crate::dev::HotDev;
use crate::peers::{HotPeers, Spawn};
use crate::sched::{HotSched, SchedError};

pub use read::ReadError;
pub use reclaim::Reclaimed;
pub use scrub::ScrubReport;
pub use startup::StartupReport;
pub use tick::TickReport;
pub use writer::SegmentInfo;

use writer::Seg;

/// `Retry-After` of a `503` the hot store answers (milliseconds).
pub const RETRY_AFTER_MS: u64 = 1000;

/// What a hot node is made of.
pub struct HotParts<B: Backend + 'static> {
    /// This node's id.
    pub id: NodeId,
    /// The hot settings.
    pub config: HotConfig,
    /// The hot device.
    pub dev: Arc<dyn HotDev>,
    /// Its I/O queue.
    pub sched: HotSched,
    /// The node's index and block table.
    pub index: Arc<KvIndex<B>>,
    /// The other storage nodes.
    pub peers: Arc<dyn HotPeers>,
    /// Node rank by rendezvous hashing (10.2).
    pub placement: Arc<dyn Placement>,
    /// Runs the node's background tasks.
    pub spawn: Arc<dyn Spawn>,
    /// The monotonic clock of ages and stalls.
    pub clock: Arc<dyn Clock>,
    /// The wall clock of the cluster stamp (milliseconds since the epoch).
    pub wall: Arc<dyn Clock>,
    /// The hot tier's metric families.
    pub metrics: Arc<Hot>,
    /// The membership view the node starts with.
    pub view: View,
}

/// What kind of hot file a [`FileInfo`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A sealed write segment.
    Segment(BlockId),
    /// A large object.
    Large(LargeFile),
    /// A cache segment of promoted copies.
    Cache,
}

/// A hot file the node scrubs and reclaims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) kind: FileKind,
    /// Its bytes on the device.
    pub(crate) bytes: u64,
    /// The bytes its records take: a segment's sealed length, a large object's size.
    pub(crate) data_len: u64,
    /// Not yet committed to the cold tier (hot-only mode: always).
    pub(crate) dirty: bool,
    pub(crate) last_read_ms: u64,
    pub(crate) last_scrub_ms: u64,
}

/// The node's mutable state.
#[derive(Default)]
pub(crate) struct State {
    /// The open segment of each replica set and expiry class this node writes.
    pub(crate) open: BTreeMap<(Vec<NodeId>, ExpiryClass), Arc<Seg>>,
    /// Every write segment of this node not yet sealed here, open or sealing.
    pub(crate) writing: BTreeMap<BlockId, Arc<Seg>>,
    /// The copies this node mirrors for other writers, while open.
    pub(crate) copies: BTreeMap<BlockId, replica::CopySt>,
    /// The sealed segments, large objects and cache segments, by path.
    pub(crate) files: BTreeMap<String, FileInfo>,
    /// Sealed blocks this node knows to have fewer copies than `hot.replicas`.
    pub(crate) under_replicated: BTreeSet<BlockId>,
    /// Peers whose replication failed, with when: left out of new replica sets for
    /// `replica_stall`.
    pub(crate) suspects: BTreeMap<NodeId, u64>,
    /// Open segments found at startup whose seal has not committed yet.
    pub(crate) unsealed: BTreeSet<BlockId>,
    /// Large PUTs in flight on this writer (their replica streams, for `put_copy_stall`).
    pub(crate) large: BTreeMap<u64, large::Tee>,
    /// The highest ballot round seen per segment (a sealer goes above it).
    pub(crate) rounds: BTreeMap<BlockId, u32>,
    /// Segments a background seal of this node is working on.
    pub(crate) sealing: BTreeSet<BlockId>,
    /// The seal state of every segment this node voted on or read since it started.
    pub(crate) votes: BTreeMap<BlockId, SealState>,
}

pub(crate) struct Node<B: Backend + 'static> {
    pub(crate) id: NodeId,
    pub(crate) cfg: HotConfig,
    pub(crate) dev: Arc<dyn HotDev>,
    pub(crate) sched: HotSched,
    pub(crate) index: Arc<KvIndex<B>>,
    pub(crate) peers: Arc<dyn HotPeers>,
    pub(crate) placement: Arc<dyn Placement>,
    pub(crate) spawn: Arc<dyn Spawn>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) wall: Arc<dyn Clock>,
    pub(crate) metrics: Arc<Hot>,
    pub(crate) hlc: Hlc,
    pub(crate) ids: IdAllocator<BlockId>,
    pub(crate) view: RwLock<View>,
    /// The key lock of 10.3 and 10.5.
    pub(crate) keys: keys::Locks,
    /// One lock per segment around its seal state, its copy's converge and its replica writes.
    pub(crate) seg_locks: keys::Locks,
    pub(crate) st: Mutex<State>,
}

/// The hot store of one node; clones share it.
pub struct HotNode<B: Backend + 'static> {
    pub(crate) n: Arc<Node<B>>,
}

impl<B: Backend + 'static> Clone for HotNode<B> {
    fn clone(&self) -> Self {
        Self {
            n: Arc::clone(&self.n),
        }
    }
}

/// An I/O error of `op` as the hot store answers it.
pub(crate) fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> HotError {
    move |source| HotError::Io { op, source }
}

/// Any other error of `op` (format, index, queue) as an I/O error of the hot store.
pub(crate) fn other_err<E: std::error::Error + Send + Sync + 'static>(
    op: &'static str,
) -> impl FnOnce(E) -> HotError {
    move |e| HotError::Io {
        op,
        source: io::Error::other(e),
    }
}

/// `503` for `reason`.
pub(crate) fn unavailable(reason: Unavailable) -> HotError {
    HotError::Unavailable {
        reason,
        retry_after_ms: RETRY_AFTER_MS,
    }
}

impl From<SchedError> for HotError {
    fn from(e: SchedError) -> Self {
        HotError::Io {
            op: "hot device",
            source: io::Error::other(e),
        }
    }
}

impl<B: Backend + 'static> Node<B> {
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn view(&self) -> View {
        self.view
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The storage nodes that vote on a seal: every node of the view whose hot device is not
    /// `out` (nodes without a copy vote too, 10.3 step 4).
    pub(crate) fn voters(&self) -> Vec<NodeId> {
        let view = self.view();
        let mut v: Vec<NodeId> = view
            .nodes
            .iter()
            .filter(|n| n.hot != Admin::Out)
            .map(|n| n.id)
            .collect();
        v.sort_unstable();
        v
    }

    /// A stamp of a local event.
    pub(crate) fn stamp(&self) -> Result<Stamp, HotError> {
        self.hlc
            .tick(self.wall.now_ms())
            .map_err(other_err("cluster stamp"))
    }

    /// Merges a stamp a peer sent.
    pub(crate) fn observe(&self, remote: Stamp) {
        // A stamp at the end of the range only stops the merge; the next tick still moves.
        let _ = self.hlc.observe(remote, self.wall.now_ms());
    }

    /// `body` in this node's envelope.
    pub(crate) fn env<T>(&self, body: T) -> Result<Envelope<T>, HotError> {
        Ok(Envelope {
            from: self.id,
            stamp: self.stamp()?,
            mem_epoch: self.view().epoch,
            body,
        })
    }

    /// A ballot above every one this node has seen for `segment`.
    pub(crate) fn next_ballot(&self, segment: BlockId) -> Ballot {
        let mut st = self.lock();
        let round = st.rounds.entry(segment).or_insert(1);
        *round += 1;
        Ballot {
            round: *round,
            node: self.id,
        }
    }

    /// Notes a ballot seen for `segment`.
    pub(crate) fn saw_ballot(&self, segment: BlockId, ballot: Ballot) {
        let mut st = self.lock();
        let round = st.rounds.entry(segment).or_insert(1);
        *round = (*round).max(ballot.round);
    }
}

impl<B: Backend + 'static> HotNode<B> {
    /// A node from its parts. Nothing is read from the device: [`HotNode::start`] runs the
    /// startup sweep and recovery before the node serves.
    pub fn new(parts: HotParts<B>) -> Self {
        Self {
            n: Arc::new(Node {
                id: parts.id,
                ids: IdAllocator::new(parts.id),
                cfg: parts.config,
                dev: parts.dev,
                sched: parts.sched,
                index: parts.index,
                peers: parts.peers,
                placement: parts.placement,
                spawn: parts.spawn,
                clock: parts.clock,
                wall: parts.wall,
                metrics: parts.metrics,
                hlc: Hlc::new(),
                view: RwLock::new(parts.view),
                keys: keys::Locks::default(),
                seg_locks: keys::Locks::default(),
                st: Mutex::new(State::default()),
            }),
        }
    }

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.n.id
    }

    /// The index the node publishes entries to.
    pub fn index(&self) -> &Arc<KvIndex<B>> {
        &self.n.index
    }

    /// The node's I/O queue.
    pub fn sched(&self) -> &HotSched {
        &self.n.sched
    }

    /// Adopts a new membership view (a higher epoch, or a state change this node observed). The
    /// writer seals early every open segment whose replica set holds a node that is now `down`
    /// or `out` (10.3 step 5); [`HotNode::tick`] does it.
    pub fn set_view(&self, view: View) {
        *self.n.view.write().unwrap_or_else(PoisonError::into_inner) = view;
    }

    /// The last stamp of the node's cluster clock.
    pub fn last_stamp(&self) -> Stamp {
        self.n.hlc.last()
    }

    /// The sealed blocks this node knows to have fewer copies than `hot.replicas`
    /// (`hot_under_replicated_blocks`).
    pub fn under_replicated(&self) -> Vec<BlockId> {
        self.n.lock().under_replicated.iter().copied().collect()
    }

    /// The open segments this node writes, by id.
    pub fn open_segments(&self) -> Vec<SegmentInfo> {
        self.n.lock().open.values().map(|s| s.info()).collect()
    }

    /// Flushes the index with the replay mark of 10.2 step 1: per open segment the length the
    /// index reflects (this writer's acknowledged offset, a copy's acknowledged prefix), and the
    /// tombstone cursors.
    pub fn flush_mark(&self) -> Result<(), HotError> {
        let open_segments = {
            let st = self.n.lock();
            st.writing
                .values()
                .map(|s| (s.id, s.lock().acked))
                .chain(st.copies.iter().map(|(id, c)| (*id, c.noticed)))
                .collect()
        };
        let tomb_cursors = self
            .n
            .index
            .tomb_cursors()
            .map_err(other_err("tomb cursors"))?;
        self.n
            .index
            .flush_mark(&tierstore_store_api::index::ReplayMark {
                open_segments,
                tomb_cursors,
            })
            .map_err(other_err("replay mark"))
    }

    /// The `sealed` broadcast of a committed seal (`POST /store/v1/seal`, kind `sealed`): the
    /// node records the commit and, if it holds a copy, converges it.
    pub async fn sealed(&self, segment: BlockId, proposal: SealProposal) -> Result<(), HotError> {
        self.n.learn(segment, proposal).await
    }
}

#[async_trait::async_trait]
impl<B: Backend + 'static> HotStore for HotNode<B> {
    async fn append(&self, record: AppendRecord) -> Result<Acked, HotError> {
        self.n.append(record).await
    }

    async fn put_file(&self, put: LargePut) -> Result<Acked, HotError> {
        large::put_file(&self.n, put).await
    }

    async fn seal(
        &self,
        segment: BlockId,
        ballot: Ballot,
        reason: SealReason,
    ) -> Result<Committed, HotError> {
        sealer::seal(&self.n, segment, ballot, reason).await
    }

    async fn replicate(&self, batch: ReplicaBatch) -> Result<ReplicaAck, ReplicaReject> {
        replica::replicate(&self.n, batch).await
    }

    async fn promise(&self, segment: BlockId, ballot: Ballot) -> Result<Promise, SealRefused> {
        self.n.promise(segment, ballot).await
    }

    async fn accept(
        &self,
        segment: BlockId,
        ballot: Ballot,
        proposal: SealProposal,
    ) -> Result<(), SealRefused> {
        self.n.accept(segment, ballot, proposal).await
    }
}
