//! The deterministic simulation the hot tier's tests run on: storage nodes made of the real
//! [`HotNode`] on the crash-point disk of `tierstore-harness`, an executor that runs every task
//! until nothing can progress, and a network between them that the test cuts (one way or both),
//! holds (to choose the order in which a node receives messages), and loses replies on.
//!
//! Nothing here reads a clock or sleeps: time is a `ManualClock` the test moves, and a test
//! waits for "nothing more can happen" ([`Sim::run`]), never for time to pass.

#![allow(dead_code, reason = "each test binary uses part of the shared support")]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Context;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::task::{ArcWake, waker_ref};
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::block::BlockGeometry;
use tierstore_format::descriptor::{Code, ShardLoc};
use tierstore_format::digest::hash64;
use tierstore_format::ids::{DriveId, ExpiryClass, Generation, NodeId, StripeId};
use tierstore_format::names::HotName;
use tierstore_format::stamp::Stamp;
use tierstore_harness::disk::FakeDisk;
use tierstore_hot::config::HotConfig;
use tierstore_hot::dev::HotDev;
use tierstore_hot::node::ReadError;
use tierstore_hot::peers::{HotPeers, LargeAnswer, LargeChunk, LargeOffer, Spawn};
use tierstore_hot::sched::HotSched;
use tierstore_hot::{HotNode, HotParts};
use tierstore_index::backend::mem::MemBackend;
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_metrics::{HotStoreConfig, Metrics};
use tierstore_store_api::hot::{AppendRecord, HotStore, ReplicaBatch, SealRefused};
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::placement::{Placement, PlacementError};
use tierstore_store_api::proto::{CopyWire, Envelope, ReplicaAnswer, SealAnswer, SealMessage};
use tierstore_store_api::view::{Admin, ClusterSettings, NodeState, NodeView, Role, View};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// The device.

/// [`HotDev`] on the crash-point disk, for one incarnation of a node: once that incarnation
/// crashed, its leftover tasks fail on every op even after the disk restarts for the next one.
#[derive(Clone)]
pub struct FakeDev(pub FakeDisk, pub Arc<AtomicBool>);

impl FakeDev {
    fn live(&self) -> io::Result<()> {
        if self.1.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(io::Error::other("a crashed incarnation"))
        }
    }
}

impl HotDev for FakeDev {
    fn create(&self, path: &str) -> io::Result<()> {
        self.live()?;
        Ok(self.0.create(path)?)
    }
    fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.live()?;
        Ok(self.0.write_at(path, offset as usize, bytes)?)
    }
    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.live()?;
        Ok(self.0.read_at(path, offset as usize, len as usize)?)
    }
    fn len(&self, path: &str) -> io::Result<u64> {
        self.live()?;
        Ok(self.0.len(path)? as u64)
    }
    fn set_len(&self, path: &str, len: u64) -> io::Result<()> {
        self.live()?;
        Ok(self.0.set_len(path, len as usize)?)
    }
    fn allocate(&self, _path: &str, _len: u64) -> io::Result<()> {
        self.live()?;
        Ok(())
    }
    fn fdatasync(&self, path: &str) -> io::Result<()> {
        self.live()?;
        Ok(self.0.fdatasync(path)?)
    }
    fn fsync_dir(&self, dir: &str) -> io::Result<()> {
        self.live()?;
        Ok(self.0.fsync_dir(dir)?)
    }
    fn rename(&self, from: &str, to: &str, noreplace: bool) -> io::Result<()> {
        self.live()?;
        Ok(self.0.rename(from, to, noreplace)?)
    }
    fn unlink(&self, path: &str) -> io::Result<()> {
        self.live()?;
        Ok(self.0.unlink(path)?)
    }
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        self.live()?;
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut names: Vec<String> = self
            .0
            .list(&prefix)?
            .into_iter()
            .filter_map(|p| {
                p.strip_prefix(&prefix)
                    .map(|r| r.split('/').next().unwrap_or("").to_owned())
            })
            .collect();
        names.dedup();
        Ok(names)
    }
}

// ---------------------------------------------------------------------------------------------
// The executor.

struct Task {
    fut: Mutex<Option<BoxFuture<'static, ()>>>,
    queue: Arc<Mutex<VecDeque<Arc<Task>>>>,
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        lock(&arc_self.queue).push_back(Arc::clone(arc_self));
    }
}

/// An executor that runs its tasks only inside [`Exec::run`], until none can progress.
#[derive(Clone, Default)]
pub struct Exec {
    queue: Arc<Mutex<VecDeque<Arc<Task>>>>,
}

impl Spawn for Exec {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        let t = Arc::new(Task {
            fut: Mutex::new(Some(task)),
            queue: Arc::clone(&self.queue),
        });
        lock(&self.queue).push_back(t);
    }
}

impl Exec {
    /// Polls every runnable task until nothing is runnable.
    pub fn run(&self) {
        loop {
            let next = lock(&self.queue).pop_front();
            let Some(task) = next else { return };
            let mut slot = lock(&task.fut);
            if let Some(mut fut) = slot.take() {
                let waker = waker_ref(&task);
                let mut cx = Context::from_waker(&waker);
                if fut.as_mut().poll(&mut cx).is_pending() {
                    *slot = Some(fut);
                }
            }
        }
    }
}

/// The result of a future spawned on the executor, once it finished.
pub struct Handle<T> {
    out: Arc<Mutex<Option<T>>>,
}

impl<T> Handle<T> {
    /// The result, if the future finished.
    pub fn take(&self) -> Option<T> {
        lock(&self.out).take()
    }

    /// Whether the future finished.
    pub fn done(&self) -> bool {
        lock(&self.out).is_some()
    }
}

impl Exec {
    /// Spawns `fut` and returns where its result lands.
    pub fn start<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> Handle<T> {
        let out = Arc::new(Mutex::new(None));
        let o = Arc::clone(&out);
        self.spawn(
            async move {
                let v = fut.await;
                *lock(&o) = Some(v);
            }
            .boxed(),
        );
        Handle { out }
    }

    /// Runs `fut` with everything else until nothing can progress; its result, which must be
    /// there by then.
    pub fn block<T: Send + 'static>(&self, fut: impl Future<Output = T> + Send + 'static) -> T {
        let h = self.start(fut);
        self.run();
        h.take()
            .expect("the future did not finish: something waits forever")
    }
}

// ---------------------------------------------------------------------------------------------
// The network.

/// The kind of a hot-tier message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Replicate,
    Prepare,
    Accept,
    Sealed,
    Large,
    Fetch,
}

/// A message the network holds until the test releases it.
pub struct Held {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: Kind,
    release: Option<futures::channel::oneshot::Sender<bool>>,
}

#[derive(Default)]
struct NetState {
    nodes: BTreeMap<NodeId, HotNode<MemBackend>>,
    /// Directed links that are cut: what `from` sends to `to` is lost.
    cuts: BTreeSet<(NodeId, NodeId)>,
    /// Kinds of messages to hold, by (from, to); `None` from matches any sender.
    holds: Vec<(Option<NodeId>, NodeId, Kind)>,
    held: Vec<Held>,
    /// Replies to lose: (from, to, kind, how many more).
    lose: Vec<(NodeId, NodeId, Kind, usize)>,
    /// Every message sent: (from, to, kind).
    log: Vec<(NodeId, NodeId, Kind)>,
    /// Every batch delivered: (from, to, start, bytes).
    batches: Vec<(NodeId, NodeId, u64, u64)>,
}

/// The simulated network.
#[derive(Clone, Default)]
pub struct Net {
    st: Arc<Mutex<NetState>>,
}

impl Net {
    pub fn cut(&self, from: u8, to: u8) {
        lock(&self.st).cuts.insert((NodeId(from), NodeId(to)));
    }
    pub fn cut_both(&self, a: u8, b: u8) {
        self.cut(a, b);
        self.cut(b, a);
    }
    pub fn heal(&self) {
        lock(&self.st).cuts.clear();
    }
    /// Holds every message of `kind` to `to` (from `from`, or from anyone).
    pub fn hold(&self, from: Option<u8>, to: u8, kind: Kind) {
        lock(&self.st)
            .holds
            .push((from.map(NodeId), NodeId(to), kind));
    }
    pub fn unhold(&self) {
        lock(&self.st).holds.clear();
    }
    /// The messages held, in the order they arrived.
    pub fn held(&self) -> Vec<(NodeId, NodeId, Kind)> {
        lock(&self.st)
            .held
            .iter()
            .map(|h| (h.from, h.to, h.kind))
            .collect()
    }
    /// Delivers the `i`-th held message.
    pub fn release(&self, i: usize) {
        let mut h = lock(&self.st).held.remove(i);
        if let Some(tx) = h.release.take() {
            let _ = tx.send(true);
        }
    }
    /// Loses the `i`-th held message (the sender sees no answer).
    pub fn drop_held(&self, i: usize) {
        let mut h = lock(&self.st).held.remove(i);
        if let Some(tx) = h.release.take() {
            let _ = tx.send(false);
        }
    }
    /// Loses the next `n` replies of `kind` from `from` to `to` (the message is delivered and
    /// handled; its answer never arrives).
    pub fn lose_replies(&self, from: u8, to: u8, kind: Kind, n: usize) {
        lock(&self.st)
            .lose
            .push((NodeId(from), NodeId(to), kind, n));
    }
    /// Messages sent so far of `kind` from `from` to `to`.
    pub fn sent(&self, from: u8, to: u8, kind: Kind) -> usize {
        lock(&self.st)
            .log
            .iter()
            .filter(|(f, t, k)| *f == NodeId(from) && *t == NodeId(to) && *k == kind)
            .count()
    }
    /// The batches delivered from `from` to `to` that carried bytes: (start, length).
    pub fn batches(&self, from: u8, to: u8) -> Vec<(u64, u64)> {
        lock(&self.st)
            .batches
            .iter()
            .filter(|(f, t, _, len)| *f == NodeId(from) && *t == NodeId(to) && *len > 0)
            .map(|(_, _, s, l)| (*s, *l))
            .collect()
    }
    pub fn sent_kind(&self, kind: Kind) -> usize {
        lock(&self.st)
            .log
            .iter()
            .filter(|(_, _, k)| *k == kind)
            .count()
    }

    fn add(&self, node: HotNode<MemBackend>) {
        lock(&self.st).nodes.insert(node.id(), node);
    }
    fn remove(&self, id: NodeId) {
        lock(&self.st).nodes.remove(&id);
    }

    fn unreachable(node: NodeId) -> PeerError {
        PeerError::Unreachable {
            node,
            reason: "simulated".into(),
        }
    }

    /// The target node if the request reaches it (after a hold, if one applies).
    async fn deliver(
        &self,
        from: NodeId,
        to: NodeId,
        kind: Kind,
    ) -> Result<HotNode<MemBackend>, PeerError> {
        let rx = {
            let mut st = lock(&self.st);
            st.log.push((from, to, kind));
            if st.cuts.contains(&(from, to)) {
                return Err(Self::unreachable(to));
            }
            let holds = st
                .holds
                .iter()
                .any(|(f, t, k)| *t == to && *k == kind && f.is_none_or(|f| f == from));
            if holds {
                let (tx, rx) = futures::channel::oneshot::channel();
                st.held.push(Held {
                    from,
                    to,
                    kind,
                    release: Some(tx),
                });
                Some(rx)
            } else {
                None
            }
        };
        if let Some(rx) = rx
            && !rx.await.unwrap_or(false)
        {
            return Err(Self::unreachable(to));
        }
        let st = lock(&self.st);
        if st.cuts.contains(&(from, to)) {
            return Err(Self::unreachable(to));
        }
        st.nodes
            .get(&to)
            .cloned()
            .ok_or_else(|| Self::unreachable(to))
    }

    /// Whether the reply of `to` to `from` arrives.
    fn reply(&self, from: NodeId, to: NodeId, kind: Kind) -> Result<(), PeerError> {
        let mut st = lock(&self.st);
        if st.cuts.contains(&(to, from)) || !st.nodes.contains_key(&from) {
            return Err(Self::unreachable(to));
        }
        if let Some(rule) = st
            .lose
            .iter_mut()
            .find(|(f, t, k, n)| *f == to && *t == from && *k == kind && *n > 0)
        {
            rule.3 -= 1;
            return Err(Self::unreachable(to));
        }
        Ok(())
    }
}

/// One node's view of the network.
pub struct SimPeers {
    me: NodeId,
    net: Net,
    alive: Arc<AtomicBool>,
}

impl SimPeers {
    fn live(&self, to: NodeId) -> Result<(), PeerError> {
        if self.alive.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Net::unreachable(to))
        }
    }
}

fn env<T>(node: &HotNode<MemBackend>, body: T) -> Envelope<T> {
    Envelope {
        from: node.id(),
        stamp: node.last_stamp(),
        mem_epoch: 1,
        body,
    }
}

#[async_trait::async_trait]
impl HotPeers for SimPeers {
    async fn replicate(
        &self,
        node: NodeId,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        self.live(node)?;
        let n = self.net.deliver(self.me, node, Kind::Replicate).await?;
        lock(&self.net.st).batches.push((
            self.me,
            node,
            batch.body.start,
            batch.body.bytes.len() as u64,
        ));
        let answer = ReplicaAnswer::from(n.replicate(batch.body).await);
        self.net.reply(self.me, node, Kind::Replicate)?;
        Ok(env(&n, answer))
    }

    async fn seal(
        &self,
        node: NodeId,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        let kind = match msg.body {
            SealMessage::Prepare { .. } => Kind::Prepare,
            SealMessage::Accept { .. } => Kind::Accept,
            SealMessage::Sealed { .. } => Kind::Sealed,
        };
        self.live(node)?;
        let n = self.net.deliver(self.me, node, kind).await?;
        let refused = |r: SealRefused| match r {
            SealRefused::Promised { promised } => SealAnswer::Promised { ballot: promised },
            SealRefused::Committed(proposal) => SealAnswer::Committed { proposal },
        };
        let answer = match msg.body {
            SealMessage::Prepare { segment, ballot } => match n.promise(segment, ballot).await {
                Ok(p) => SealAnswer::Promise {
                    accepted: p.accepted,
                    copy: p.copy.map(|c| CopyWire {
                        committed: c.committed,
                        verified: c.verified,
                        running_crc: c.running_crc,
                    }),
                },
                Err(r) => refused(r),
            },
            SealMessage::Accept {
                segment,
                ballot,
                proposal,
            } => match n.accept(segment, ballot, proposal).await {
                Ok(()) => SealAnswer::Ok,
                Err(r) => refused(r),
            },
            SealMessage::Sealed { segment, proposal } => {
                let _ = n.sealed(segment, proposal).await;
                SealAnswer::Ok
            }
        };
        self.net.reply(self.me, node, kind)?;
        Ok(env(&n, answer))
    }

    async fn put_large(
        &self,
        node: NodeId,
        offer: Envelope<LargeOffer>,
        body: BoxStream<'static, LargeChunk>,
    ) -> Result<Envelope<LargeAnswer>, PeerError> {
        self.live(node)?;
        let n = self.net.deliver(self.me, node, Kind::Large).await?;
        let answer = n.receive_large(offer, body).await;
        self.net.reply(self.me, node, Kind::Large)?;
        Ok(env(&n, answer))
    }

    async fn fetch(
        &self,
        node: NodeId,
        file: HotName,
        range: ByteRange,
    ) -> Result<Bytes, PeerError> {
        self.live(node)?;
        let n = self.net.deliver(self.me, node, Kind::Fetch).await?;
        let got = n.serve(file, range).await;
        self.net.reply(self.me, node, Kind::Fetch)?;
        got.map_err(|e| match e {
            ReadError::NotFound => PeerError::NotFound { node },
            other => PeerError::Protocol {
                node,
                status: 500,
                reason: other.to_string(),
            },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Placement.

/// Rendezvous ranking, `score(n) = H(K || node_id)`, highest first (10.2).
pub struct Rendezvous;

impl Placement for Rendezvous {
    fn rank(&self, key: &[u8], view: &View) -> Vec<NodeId> {
        let mut nodes: Vec<(u64, NodeId)> = view
            .nodes
            .iter()
            .map(|n| (hash64(&[key, &[n.id.0]]), n.id))
            .collect();
        nodes.sort_unstable_by(|a, b| b.cmp(a));
        nodes.into_iter().map(|(_, id)| id).collect()
    }

    fn stripe(
        &self,
        stripe: StripeId,
        _code: Code,
        _view: &View,
    ) -> Result<Vec<ShardLoc>, PlacementError> {
        Err(PlacementError::TooFewDomains { stripe, live: 0 })
    }
}

// ---------------------------------------------------------------------------------------------
// The cluster.

/// A small configuration for the tests: 2 MiB blocks with 256 KiB reserved, small marks of a
/// 64 MiB device.
pub fn config(replicas: u8, min_acks: u8) -> HotConfig {
    let geometry = BlockGeometry::new(2 << 20, 256 << 10).expect("geometry");
    let mut cfg = HotConfig::defaults(64 << 20, geometry);
    cfg.replicas = replicas;
    cfg.min_acks = min_acks;
    cfg.large_min = 64 << 10;
    cfg.seal_min_bytes = 256 << 10;
    cfg.batch_max = 16 << 10;
    cfg
}

/// One node's durable parts, which survive its crash.
pub struct Parts {
    pub disk: FakeDisk,
    pub metrics: Arc<tierstore_metrics::Hot>,
    pub index: Arc<KvIndex<MemBackend>>,
}

/// A cluster of `HotNode`s on one network and one executor.
pub struct Sim {
    pub exec: Exec,
    pub net: Net,
    pub clock: ManualClock,
    pub cfg: HotConfig,
    pub view: View,
    pub parts: BTreeMap<NodeId, Parts>,
    /// Wall-clock skew of each node, in milliseconds (added to the shared clock).
    pub skew: BTreeMap<NodeId, u64>,
    next_stamp: u64,
    /// The alive flag of each node's current incarnation.
    alive: BTreeMap<NodeId, Arc<AtomicBool>>,
    /// Wraps the peers of every node made from now on (a node restarted after it is set).
    pub peers_wrap: Option<PeersWrap>,
}

/// A function that puts a test's own [`HotPeers`] around a node's simulated network.
pub type PeersWrap = Arc<dyn Fn(Arc<dyn HotPeers>) -> Arc<dyn HotPeers> + Send + Sync>;

/// A wall clock: the shared manual clock plus an offset.
struct Wall {
    clock: ManualClock,
    offset: u64,
}

impl Clock for Wall {
    fn now_ms(&self) -> u64 {
        self.clock.now_ms() + self.offset
    }
}

pub fn view(nodes: u8, cfg: &HotConfig) -> View {
    View {
        epoch: 1,
        settings: ClusterSettings {
            hot_replicas: cfg.replicas,
            hot_min_acks: cfg.min_acks,
            cold_code: None,
        },
        nodes: (1..=nodes)
            .map(|i| NodeView {
                id: NodeId(i),
                domain: u16::from(i),
                role: Role::Storage,
                hot: Admin::In,
                state: NodeState::Up,
            })
            .collect(),
        drives: Vec::new(),
    }
}

impl Sim {
    /// `nodes` storage nodes with `cfg`, each started (empty disks).
    pub fn new(nodes: u8, cfg: HotConfig) -> Self {
        let mut sim = Self {
            exec: Exec::default(),
            net: Net::default(),
            clock: ManualClock::new(),
            view: view(nodes, &cfg),
            cfg,
            parts: BTreeMap::new(),
            skew: BTreeMap::new(),
            next_stamp: 1,
            alive: BTreeMap::new(),
            peers_wrap: None,
        };
        sim.clock.advance(1_000_000);
        for i in 1..=nodes {
            let id = NodeId(i);
            let peers: Vec<NodeId> = (1..=nodes).filter(|j| *j != i).map(NodeId).collect();
            let metrics = Metrics::new("test")
                .hot_store(&HotStoreConfig {
                    device: DriveId([i; 16]),
                    replicas: sim.cfg.replicas,
                    peers,
                })
                .hot;
            sim.parts.insert(
                id,
                Parts {
                    disk: FakeDisk::new(),
                    metrics,
                    index: Arc::new(new_index(id, &sim.cfg)),
                },
            );
            sim.boot(id);
        }
        sim
    }

    fn make(&mut self, id: NodeId) -> HotNode<MemBackend> {
        let alive = Arc::new(AtomicBool::new(true));
        self.alive.insert(id, Arc::clone(&alive));
        let p = &self.parts[&id];
        let clock: Arc<dyn Clock> = Arc::new(self.clock.clone());
        let peers: Arc<dyn HotPeers> = Arc::new(SimPeers {
            me: id,
            net: self.net.clone(),
            alive: Arc::clone(&alive),
        });
        let peers = match &self.peers_wrap {
            Some(wrap) => wrap(peers),
            None => peers,
        };
        HotNode::new(HotParts {
            id,
            config: self.cfg.clone(),
            dev: Arc::new(FakeDev(p.disk.clone(), Arc::clone(&alive))),
            sched: HotSched::inline(self.cfg.stall_after_ms, Arc::clone(&clock)),
            index: Arc::clone(&p.index),
            peers,
            placement: Arc::new(Rendezvous),
            spawn: Arc::new(self.exec.clone()),
            clock,
            wall: Arc::new(Wall {
                clock: self.clock.clone(),
                offset: self.skew.get(&id).copied().unwrap_or(0),
            }),
            metrics: Arc::clone(&p.metrics),
            view: self.view.clone(),
        })
    }

    /// Makes node `id` from its parts, adds it to the network and starts it.
    fn boot(&mut self, id: NodeId) {
        let node = self.make(id);
        self.net.add(node.clone());
        let n = node.clone();
        let report = self.exec.block(async move { n.start(None).await });
        report.expect("start");
        node.index().set_ready(true);
    }

    /// The node `id`, as the network reaches it.
    pub fn node(&self, id: u8) -> HotNode<MemBackend> {
        lock(&self.net.st).nodes[&NodeId(id)].clone()
    }

    /// The node's disk.
    pub fn disk(&self, id: u8) -> FakeDisk {
        self.parts[&NodeId(id)].disk.clone()
    }

    pub fn metrics(&self, id: u8) -> Arc<tierstore_metrics::Hot> {
        Arc::clone(&self.parts[&NodeId(id)].metrics)
    }

    /// Crashes node `id`: its disk keeps only what was durable, its index is lost, and it is
    /// gone from the network (messages to it fail; its tasks fail on the disk).
    pub fn crash(&mut self, id: u8) {
        let id = NodeId(id);
        self.parts[&id].disk.crash();
        if let Some(a) = self.alive.get(&id) {
            a.store(false, Ordering::SeqCst);
        }
        self.net.remove(id);
        self.exec.run();
    }

    /// Restarts a crashed node: the disk's durable layer, a fresh index rebuilt by its startup.
    pub fn restart(&mut self, id: u8) -> tierstore_hot::node::StartupReport {
        let id = NodeId(id);
        let cfg = self.cfg.clone();
        let p = self.parts.get_mut(&id).expect("node");
        p.disk.restart();
        p.index = Arc::new(new_index(id, &cfg));
        let node = self.make(id);
        self.net.add(node.clone());
        let n = node.clone();
        let report = self
            .exec
            .block(async move { n.start(None).await })
            .expect("start");
        node.index().set_ready(true);
        report
    }

    /// Runs every task until nothing can progress.
    pub fn run(&self) {
        self.exec.run();
    }

    /// Sets the state of node `id` in every node's view.
    pub fn set_state(&mut self, id: u8, state: NodeState) {
        for n in &mut self.view.nodes {
            if n.id == NodeId(id) {
                n.state = state;
            }
        }
        let nodes: Vec<_> = lock(&self.net.st).nodes.values().cloned().collect();
        for n in nodes {
            n.set_view(self.view.clone());
        }
    }

    /// A stamp above every stamp handed out before.
    pub fn stamp(&mut self) -> Stamp {
        self.next_stamp += 1;
        Stamp::from_parts(
            self.clock.now_ms() + 10_000_000,
            (self.next_stamp % 60_000) as u16,
        )
        .expect("stamp")
    }

    /// An append of `data` under `key`.
    pub fn record(&mut self, key: &str, data: &[u8]) -> AppendRecord {
        AppendRecord {
            key: key.as_bytes().to_vec(),
            generation: Generation::of(data),
            stamp: self.stamp(),
            expiry: ExpiryClass(0),
            data: Bytes::copy_from_slice(data),
        }
    }

    /// Starts an append on node `id`.
    pub fn append(
        &mut self,
        id: u8,
        key: &str,
        data: &[u8],
    ) -> Handle<Result<tierstore_store_api::hot::Acked, tierstore_store_api::hot::HotError>> {
        let rec = self.record(key, data);
        let node = self.node(id);
        self.exec.start(async move { node.append(rec).await })
    }

    /// The key a node of `rank` order writes: a key whose rendezvous rank starts with `first`.
    pub fn key_on(&self, first: u8, tag: &str) -> String {
        (0..10_000)
            .map(|i| format!("{tag}-{i}"))
            .find(|k| Rendezvous.rank(k.as_bytes(), &self.view)[0] == NodeId(first))
            .expect("a key ranked first on the node")
    }
}

fn new_index(id: NodeId, cfg: &HotConfig) -> KvIndex<MemBackend> {
    KvIndex::open(
        MemBackend::new(),
        IndexConfig {
            node: id,
            geometry: cfg.geometry,
        },
    )
    .expect("index")
}
