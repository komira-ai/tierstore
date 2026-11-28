//! The writer's side of a write segment (10.3 "Small objects", "Replication protocol" steps 1
//! to 3): group commit and the ack rule.
//!
//! **Group commit.** One committer per open segment, leader-follower with no timer: arriving
//! records queue while a batch is being made durable, and when it is acknowledged the next batch
//! starts at once with everything queued, up to `hot.batch_max`. A batch is one buffered write
//! and one `fdatasync` locally, in parallel with one round trip to each replica.
//!
//! **Replication.** Each replica has a sender that streams the segment's bytes from the replica's
//! committed length, one batch at a time: `(segment, ballot, start, bytes, running crc32c through
//! the batch, stamp)`. A batch to a replica starts at most at the writer's acknowledged offset,
//! so a batch that starts at `s` tells the replica that every record before `s` is acknowledged:
//! that is when a replica indexes them (and an empty batch says it when no next batch follows).
//! A lost acknowledgement shows as an `Offset` refusal that names a longer committed length the
//! writer sent before; the sender resumes from there, and the running crc proves the replica's
//! bytes are the writer's.
//!
//! **Ack rule.** A batch is acknowledged when it is durable on `hot.min_acks` copies, the
//! writer's own `fdatasync` counting as one, and the writer has promised no ballot above the
//! segment's (a takeover fences it). Its records' index entries are published first, under
//! their key locks, which the appenders hold. A replica that refuses, errs or is silent for
//! `hot.replica_stall` makes the writer seal the segment early and roll: the records not yet
//! acknowledged are appended again to a new segment of live nodes.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::FutureExt;
use tierstore_format::block::BlockBuilder;
use tierstore_format::crc32c_append;
use tierstore_format::ids::{BlockId, ExpiryClass, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{RecordFlags, RecordIn, record_len};
use tierstore_format::seal::Ballot;
use tierstore_format::trailer::BlockKind;
use tierstore_index::blocks::{BlockInfo, BlockState};
use tierstore_store_api::hot::{
    Acked, AppendRecord, HotError, ReplicaBatch, SealReason, Unavailable,
};
use tierstore_store_api::index::{EntryLocation, IndexEntry, IndexError, Lookup, PutOutcome};
use tierstore_store_api::proto::ReplicaAnswer;
use tierstore_store_api::sched::DriveClass;
use tokio::sync::{oneshot, watch};

use super::{Node, io_err, other_err, unavailable};

/// How many times an append moves to a new segment before it answers 503.
const APPEND_ATTEMPTS: usize = 3;

/// Why a segment takes no more appends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Close {
    /// The next record did not fit: a size seal follows.
    Size,
    /// A replica failed or stalled, or the membership says one is `down` or `out`: an early
    /// seal follows.
    Early,
    /// The writer promised a higher ballot, or a replica refused a batch for its ballot: another
    /// node is sealing the segment. Nothing in flight is acknowledged any more.
    Fenced,
    /// An age seal, or a seal another way, is under way.
    Sealing,
}

/// What an appender learns from the committer.
pub(crate) enum Outcome {
    Acked(Acked),
    /// The segment closed before the record was acknowledged: append it to another one.
    Retry,
    Failed(HotError),
}

struct Pending {
    rec: AppendRecord,
    tx: oneshot::Sender<Outcome>,
}

/// One replica of an open segment, as the writer sees it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Link {
    /// The replica's committed length.
    pub(crate) committed: u64,
    /// The start of the last batch it acknowledged: it knows everything before it is acked.
    told: u64,
    /// When the batch in flight was sent, if one is.
    pub(crate) since: Option<u64>,
    sending: bool,
    pub(crate) failed: bool,
}

pub(crate) struct SegSt {
    builder: BlockBuilder,
    /// Bytes written (or being written) locally.
    pub(crate) written: u64,
    /// Bytes durable locally.
    pub(crate) durable: u64,
    /// Bytes whose records are acknowledged.
    pub(crate) acked: u64,
    /// The running crc32c at each batch end not yet acknowledged by every replica.
    crcs: BTreeMap<u64, u32>,
    /// The batch being made durable: its start and bytes.
    cur: Option<(u64, Bytes)>,
    queue: VecDeque<Pending>,
    committer: bool,
    pub(crate) links: BTreeMap<NodeId, Link>,
    pub(crate) closed: Option<Close>,
    /// When it closed (the tick seals a closed segment again if no seal committed since).
    pub(crate) closed_at: Option<u64>,
    pub(crate) records: u64,
}

/// An open write segment of this node.
pub(crate) struct Seg {
    pub(crate) id: BlockId,
    /// Its replica set, sorted, this node included.
    pub(crate) set: Vec<NodeId>,
    pub(crate) class: ExpiryClass,
    /// The ballot it was opened under: round 1, this node.
    pub(crate) ballot: Ballot,
    pub(crate) path: String,
    pub(crate) created_ms: u64,
    st: Mutex<SegSt>,
    progress: watch::Sender<u64>,
}

/// An open segment, as [`crate::HotNode::open_segments`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInfo {
    /// The segment.
    pub id: BlockId,
    /// Its replica set.
    pub set: Vec<NodeId>,
    /// Its expiry class.
    pub class: ExpiryClass,
    /// Bytes acknowledged.
    pub acked: u64,
    /// Records appended.
    pub records: u64,
}

impl Seg {
    pub(crate) fn lock(&self) -> MutexGuard<'_, SegSt> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn bump(&self) {
        self.progress.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Closes the segment for `why` unless it is closed already; whether this call closed it.
    pub(crate) fn close(&self, why: Close) -> bool {
        let first = {
            let mut s = self.lock();
            match s.closed {
                None => {
                    s.closed = Some(why);
                    true
                }
                Some(_) if why == Close::Fenced => {
                    s.closed = Some(why);
                    false
                }
                Some(_) => false,
            }
        };
        self.bump();
        first
    }

    /// Waits until no committer of this segment runs, so no local write of a batch is still in
    /// flight (a converge then truncates and appends the trailer after the last of them).
    pub(crate) async fn idle(&self) {
        let mut rx = self.progress.subscribe();
        while self.lock().committer {
            let _ = rx.changed().await;
        }
    }

    pub(crate) fn info(&self) -> SegmentInfo {
        let s = self.lock();
        SegmentInfo {
            id: self.id,
            set: self.set.clone(),
            class: self.class,
            acked: s.acked,
            records: s.records,
        }
    }
}

impl Node {
    /// `HotStore::append` (10.3, 10.5 step 3).
    pub(crate) async fn append(self: &Arc<Self>, record: AppendRecord) -> Result<Acked, HotError> {
        if record.data.len() as u64 > self.cfg.large_min {
            return Err(HotError::Io {
                op: "append",
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a record above hot.large_min is a large file",
                ),
            });
        }
        self.admit_put().await?;
        let _key = self.keys.lock(&record.key).await;
        if let Some(acked) = self.existing(&record.key, &record)? {
            return Ok(acked);
        }
        self.observe(record.stamp);
        for _ in 0..APPEND_ATTEMPTS {
            let seg = self.segment_for(&record.key, record.expiry).await?;
            match self.enqueue(&seg, record.clone()).await {
                Ok(Outcome::Acked(acked)) => return Ok(acked),
                Ok(Outcome::Failed(e)) => return Err(e),
                Ok(Outcome::Retry) | Err(_) => {}
            }
        }
        Err(unavailable(Unavailable::TooFewCopies))
    }

    /// The create-only check under the key lock: the answer when the key already holds the
    /// object (identical, 200) or another one (412), `None` when the record is to be written.
    fn existing(&self, key: &[u8], rec: &AppendRecord) -> Result<Option<Acked>, HotError> {
        match self.index.get(key) {
            Ok(Lookup::Live(e)) if e.generation == rec.generation => Ok(Some(Acked {
                generation: e.generation,
                location: e.location,
                length: e.length,
                copies: self.cfg.min_acks,
            })),
            Ok(Lookup::Live(e)) => Err(HotError::Conflict {
                existing: e.generation,
            }),
            Ok(Lookup::Deleted(t)) if t > rec.stamp => Err(unavailable(Unavailable::NotDefinitive)),
            Ok(_) => Ok(None),
            Err(IndexError::Indexing) => Err(unavailable(Unavailable::NotDefinitive)),
            Err(e) => Err(other_err("index")(e)),
        }
    }

    /// The replica set of a new record of `key`: the first `hot.replicas` live nodes of
    /// `rank(key)` when this node is among them, else this node and the first live others; nodes
    /// whose replication failed within `replica_stall` are left out.
    pub(crate) fn replica_set(&self, key: &[u8]) -> Result<Vec<NodeId>, HotError> {
        let view = self.view();
        let now = self.clock.now_ms();
        let live: Vec<NodeId> = {
            let st = self.lock();
            view.live_nodes()
                .map(|n| n.id)
                .filter(|id| {
                    *id == self.id
                        || st
                            .suspects
                            .get(id)
                            .is_none_or(|t| now.saturating_sub(*t) >= self.cfg.replica_stall_ms)
                })
                .collect()
        };
        let ranked: Vec<NodeId> = self
            .placement
            .rank(key, &view)
            .into_iter()
            .filter(|id| live.contains(id) || *id == self.id)
            .collect();
        let want = usize::from(self.cfg.replicas);
        let mut set: Vec<NodeId> = ranked.iter().copied().take(want).collect();
        if !set.contains(&self.id) {
            set = std::iter::once(self.id)
                .chain(ranked.into_iter().filter(|id| *id != self.id))
                .take(want)
                .collect();
        }
        if set.len() < usize::from(self.cfg.min_acks) {
            return Err(unavailable(Unavailable::TooFewCopies));
        }
        set.sort_unstable();
        Ok(set)
    }

    /// The open segment of `key`'s replica set and `class`, opened now if there is none.
    async fn segment_for(
        self: &Arc<Self>,
        key: &[u8],
        class: ExpiryClass,
    ) -> Result<Arc<Seg>, HotError> {
        let set = self.replica_set(key)?;
        let k = (set.clone(), class);
        // Looked up twice when none is open: again under the lock of opening, which another
        // append may have held while it opened the segment.
        let mut opening = None;
        loop {
            if let Some(seg) = self.lock().open.get(&k) {
                return Ok(Arc::clone(seg));
            }
            if opening.is_some() {
                return self.open_segment(k, set, class).await;
            }
            opening = Some(self.seg_locks.lock(b"open").await);
        }
    }

    /// Creates and registers the open segment `k` of `set` and `class`; the caller holds the
    /// lock of opening and found none.
    async fn open_segment(
        self: &Arc<Self>,
        k: (Vec<NodeId>, ExpiryClass),
        set: Vec<NodeId>,
        class: ExpiryClass,
    ) -> Result<Arc<Seg>, HotError> {
        let id = self.ids.next().map_err(other_err("segment id"))?;
        let path = HotName::Segment(id).path();
        let dev = Arc::clone(&self.dev);
        let p = path.clone();
        self.sched
            .run(DriveClass::Write, move || {
                dev.create(&p)?;
                dev.fsync_dir("seg")
            })
            .await?
            .map_err(io_err("open segment"))?;
        let mut info = BlockInfo::new(BlockState::Open);
        info.hot_copies.clone_from(&set);
        self.index
            .put_block(id, &info)
            .map_err(other_err("block table"))?;
        let links = set
            .iter()
            .filter(|n| **n != self.id)
            .map(|n| (*n, Link::default()))
            .collect();
        let seg = Arc::new(Seg {
            id,
            set,
            class,
            ballot: Ballot::opening(self.id),
            path,
            created_ms: self.clock.now_ms(),
            st: Mutex::new(SegSt {
                builder: BlockBuilder::new(self.cfg.geometry, BlockKind::Segment)
                    .map_err(other_err("segment"))?,
                written: 0,
                durable: 0,
                acked: 0,
                crcs: BTreeMap::from([(0, 0)]),
                cur: None,
                queue: VecDeque::new(),
                committer: false,
                links,
                closed: None,
                closed_at: None,
                records: 0,
            }),
            progress: watch::channel(0).0,
        });
        let mut st = self.lock();
        st.open.insert(k, Arc::clone(&seg));
        st.writing.insert(id, Arc::clone(&seg));
        self.metrics
            .hot_open_segments
            .with(())
            .set(st.open.len() as f64);
        Ok(seg)
    }

    /// Queues `rec` on `seg` and starts its committer if none runs; a closed segment's
    /// committer answers the record [`Outcome::Retry`] at once.
    fn enqueue(self: &Arc<Self>, seg: &Arc<Seg>, rec: AppendRecord) -> oneshot::Receiver<Outcome> {
        let (tx, rx) = oneshot::channel();
        let start = {
            let mut s = seg.lock();
            s.queue.push_back(Pending { rec, tx });
            !std::mem::replace(&mut s.committer, true)
        };
        if start {
            let (n, seg) = (Arc::clone(self), Arc::clone(seg));
            self.spawn
                .spawn(async move { n.commit_loop(seg).await }.boxed());
        }
        rx
    }

    /// Removes `seg` from the open segments, so new records go to a new one.
    pub(crate) fn retire(&self, seg: &Seg) {
        let mut st = self.lock();
        let k = (seg.set.clone(), seg.class);
        if st.open.get(&k).is_some_and(|s| s.id == seg.id) {
            st.open.remove(&k);
        }
        self.metrics
            .hot_open_segments
            .with(())
            .set(st.open.len() as f64);
    }

    /// Closes `seg` for `why`, retires it, and seals it in the background unless another node
    /// is sealing it ([`Close::Fenced`]).
    pub(crate) fn roll(self: &Arc<Self>, seg: &Arc<Seg>, why: Close) {
        let first = seg.close(why);
        let now = self.clock.now_ms();
        seg.lock().closed_at.get_or_insert(now);
        self.retire(seg);
        if first && why != Close::Fenced {
            let reason = match why {
                Close::Size => SealReason::Size,
                Close::Sealing => SealReason::Age,
                Close::Early | Close::Fenced => SealReason::Early,
            };
            if why == Close::Early {
                self.metrics.hot_early_seals_total.with(()).inc();
            }
            let (n, id) = (Arc::clone(self), seg.id);
            self.spawn.spawn(
                async move {
                    // A seal that cannot commit now is retried by the tick.
                    let _ = super::sealer::seal_segment(&n, id, reason).await;
                }
                .boxed(),
            );
        }
    }

    /// The committer of `seg`: forms batches from the queue, makes each durable and acknowledged,
    /// until the queue is empty or the segment closes.
    async fn commit_loop(self: Arc<Self>, seg: Arc<Seg>) {
        loop {
            let Some(batch) = self.form_batch(&seg) else {
                return;
            };
            let Batch {
                start,
                bytes,
                taken,
                full,
            } = batch;
            let end = start + bytes.len() as u64;
            self.wake_links(&seg);
            let (dev, path, b) = (Arc::clone(&self.dev), seg.path.clone(), bytes.clone());
            let local = self
                .sched
                .run(DriveClass::Write, move || {
                    dev.write_at(&path, start, &b)?;
                    dev.fdatasync(&path)
                })
                .await
                .map_err(HotError::from)
                .and_then(|r| r.map_err(io_err("segment write")));
            if let Err(e) = local {
                // The device failed under the segment: nothing of the batch is acknowledged.
                self.roll(&seg, Close::Early);
                fail_all(taken, &e);
                continue;
            }
            seg.lock().durable = end;
            seg.bump();
            match self.ack_rule(&seg, end).await {
                Some(copies) => self.acknowledge(&seg, taken, end, copies),
                None => {
                    retry_all(taken);
                    continue;
                }
            }
            if full {
                self.roll(&seg, Close::Size);
            }
        }
    }

    /// The next batch of `seg`'s queue, written into its builder; `None` (and the committer
    /// stops) when the queue is empty or the segment is closed.
    fn form_batch(self: &Arc<Self>, seg: &Arc<Seg>) -> Option<Batch> {
        let mut s = seg.lock();
        if s.closed.is_some() || s.queue.is_empty() {
            s.committer = false;
            let left: Vec<Pending> = s.queue.drain(..).collect();
            drop(s);
            seg.bump();
            retry_all(left.into_iter().map(|p| (p, 0)).collect());
            return None;
        }
        let start = s.written;
        let mut bytes: Vec<u8> = Vec::new();
        let mut taken: Vec<(Pending, u64)> = Vec::new();
        let mut full = false;
        while let Some(p) = s.queue.pop_front() {
            let key_len = p.rec.key.len().min(usize::from(u16::MAX)) as u16;
            let len = record_len(key_len, p.rec.data.len() as u64);
            if !taken.is_empty() && bytes.len() as u64 + len > self.cfg.batch_max {
                s.queue.push_front(p);
                break;
            }
            let fits = start + bytes.len() as u64 + len <= self.cfg.seal_bytes
                && s.builder.fits(key_len, p.rec.data.len() as u64);
            if !fits {
                full = true;
                s.queue.push_front(p);
                break;
            }
            let offset = start + bytes.len() as u64;
            let rec = RecordIn {
                flags: RecordFlags::default(),
                key: &p.rec.key,
                data: &p.rec.data,
                generation: p.rec.generation,
                stamp: p.rec.stamp,
                object_offset: 0,
            };
            match s.builder.append(&rec) {
                Ok(out) => {
                    bytes.extend_from_slice(&out);
                    taken.push((p, offset));
                }
                Err(e) => {
                    let _ = p.tx.send(Outcome::Failed(other_err("record")(e)));
                }
            }
        }
        if taken.is_empty() {
            // Nothing fits after the records so far: seal by size and move the queue on. A
            // record that fits no segment (a key longer than a record allows) failed above.
            s.committer = false;
            let left: Vec<Pending> = s.queue.drain(..).collect();
            drop(s);
            seg.bump();
            if full {
                self.roll(seg, Close::Size);
            }
            retry_all(left.into_iter().map(|p| (p, 0)).collect());
            return None;
        }
        let end = start + bytes.len() as u64;
        let crc = crc32c_append(s.crcs.get(&start).copied().unwrap_or_default(), &bytes);
        s.crcs.insert(end, crc);
        s.written = end;
        s.records += taken.len() as u64;
        let bytes = Bytes::from(bytes);
        s.cur = Some((start, bytes.clone()));
        Some(Batch {
            start,
            bytes,
            taken,
            full,
        })
    }

    /// Waits until the batch ending at `end` is durable on `min_acks` copies; `None` when it
    /// cannot be (the segment is fenced, or too few replicas are left), after rolling.
    async fn ack_rule(self: &Arc<Self>, seg: &Arc<Seg>, end: u64) -> Option<u8> {
        let mut rx = seg.progress.subscribe();
        loop {
            let verdict = {
                let s = seg.lock();
                let copies = 1 + s.links.values().filter(|l| l.committed >= end).count();
                let possible = 1 + s.links.values().filter(|l| !l.failed).count();
                if s.closed == Some(Close::Fenced) || self.fenced(seg.id, seg.ballot) {
                    Err(Close::Fenced)
                } else if copies >= usize::from(self.cfg.min_acks) {
                    Ok(Some(copies as u8))
                } else if possible < usize::from(self.cfg.min_acks) {
                    Err(Close::Early)
                } else {
                    Ok(None)
                }
            };
            match verdict {
                Ok(Some(copies)) => return Some(copies),
                Err(why) => {
                    self.roll(seg, why);
                    return None;
                }
                Ok(None) => {
                    // The sender lives in `seg`, which this call holds.
                    let _ = rx.changed().await;
                }
            }
        }
    }

    /// Publishes the batch's index entries and acknowledges its records.
    fn acknowledge(
        self: &Arc<Self>,
        seg: &Arc<Seg>,
        taken: Vec<(Pending, u64)>,
        end: u64,
        copies: u8,
    ) {
        let mut acks = Vec::with_capacity(taken.len());
        for (p, offset) in taken {
            let entry = IndexEntry {
                generation: p.rec.generation,
                location: EntryLocation::Block {
                    block: seg.id,
                    offset,
                },
                length: p.rec.data.len() as u64,
                stamp: p.rec.stamp,
                flags: RecordFlags::default(),
                prior_blocks: Vec::new(),
                promoted: None,
            };
            // The key may already hold this very record: an earlier segment of the same PUT,
            // sealed early, made it visible. The answer then names the entry the index holds.
            let out = match self.index.put_if_absent(&p.rec.key, entry) {
                Ok(PutOutcome::Created) => Outcome::Acked(Acked {
                    generation: p.rec.generation,
                    location: EntryLocation::Block {
                        block: seg.id,
                        offset,
                    },
                    length: p.rec.data.len() as u64,
                    copies,
                }),
                Ok(PutOutcome::Identical(held)) => Outcome::Acked(Acked {
                    generation: held.generation,
                    location: held.location,
                    length: held.length,
                    copies,
                }),
                Ok(PutOutcome::Conflict(held)) => Outcome::Failed(HotError::Conflict {
                    existing: held.generation,
                }),
                Ok(PutOutcome::Stale(_)) => {
                    Outcome::Failed(unavailable(Unavailable::NotDefinitive))
                }
                Err(e) => Outcome::Failed(other_err("index")(e)),
            };
            acks.push((p.tx, out));
        }
        let notice = {
            let mut s = seg.lock();
            s.acked = end;
            s.cur = None;
            let low = s.links.values().map(|l| l.committed).min().unwrap_or(end);
            s.crcs.retain(|at, _| *at >= low);
            s.queue.is_empty()
        };
        for (tx, out) in acks {
            // An appender that went away needs no answer.
            let _ = tx.send(out);
        }
        if notice {
            self.wake_links(seg);
        }
    }

    /// Starts the sender of every replica of `seg` that has none running.
    fn wake_links(self: &Arc<Self>, seg: &Arc<Seg>) {
        let start: Vec<NodeId> = {
            let mut s = seg.lock();
            s.links
                .iter_mut()
                .filter(|(_, l)| !l.failed && !l.sending)
                .map(|(n, l)| {
                    l.sending = true;
                    *n
                })
                .collect()
        };
        for node in start {
            let (n, seg) = (Arc::clone(self), Arc::clone(seg));
            self.spawn
                .spawn(async move { n.link_loop(seg, node).await }.boxed());
        }
    }

    /// The sender of `seg` to `node`: one batch at a time from the replica's committed length.
    async fn link_loop(self: Arc<Self>, seg: Arc<Seg>, node: NodeId) {
        loop {
            let Some(job) = self.next_send(&seg, node) else {
                return;
            };
            let bytes = match self.send_bytes(&seg, &job).await {
                Ok(b) => b,
                Err(_) => {
                    self.link_failed(&seg, node);
                    return;
                }
            };
            let to = job.from + bytes.len() as u64;
            let Ok(env) = self.env(ReplicaBatch {
                segment: seg.id,
                ballot: seg.ballot,
                start: job.from,
                running_crc: job.crc,
                stamp: self.hlc.last(),
                bytes,
            }) else {
                self.link_failed(&seg, node);
                return;
            };
            let answer = self.peers.replicate(node, env).await;
            let failed = {
                let mut s = seg.lock();
                let mut failed = false;
                let mut fenced = false;
                match answer {
                    Ok(env) => {
                        self.observe(env.stamp);
                        match env.body {
                            ReplicaAnswer::Acked { committed } if committed == to => {
                                let link = s.links.entry(node).or_default();
                                link.committed = to;
                                link.told = link.told.max(job.from);
                                link.since = None;
                            }
                            ReplicaAnswer::Offset { committed, .. }
                                if committed != job.from
                                    && committed <= s.written
                                    && s.crcs.contains_key(&committed) =>
                            {
                                let link = s.links.entry(node).or_default();
                                link.committed = committed;
                                link.since = None;
                            }
                            ReplicaAnswer::Ballot { .. } | ReplicaAnswer::Sealed => fenced = true,
                            _ => failed = true,
                        }
                    }
                    Err(_) => failed = true,
                }
                if failed || fenced {
                    let link = s.links.entry(node).or_default();
                    link.failed = true;
                    link.sending = false;
                    link.since = None;
                }
                (failed, fenced)
            };
            seg.bump();
            match failed {
                (true, _) => {
                    self.mark_suspect(node);
                    self.roll(&seg, Close::Early);
                    return;
                }
                (_, true) => {
                    self.roll(&seg, Close::Fenced);
                    return;
                }
                _ => {}
            }
        }
    }

    fn link_failed(self: &Arc<Self>, seg: &Arc<Seg>, node: NodeId) {
        {
            let mut s = seg.lock();
            let link = s.links.entry(node).or_default();
            link.failed = true;
            link.sending = false;
        }
        seg.bump();
        self.mark_suspect(node);
        self.roll(seg, Close::Early);
    }

    pub(crate) fn mark_suspect(&self, node: NodeId) {
        let now = self.clock.now_ms();
        self.lock().suspects.insert(node, now);
    }

    /// What the sender of `node` sends next, or `None` (and it stops) when there is nothing to
    /// send or it must wait for the writer's next acknowledgement.
    fn next_send(&self, seg: &Seg, node: NodeId) -> Option<SendJob> {
        let now = self.clock.now_ms();
        let mut s = seg.lock();
        let job = plan_send(&s, node);
        let link = s.links.entry(node).or_default();
        if job.is_some() {
            link.since = Some(now);
        } else {
            link.sending = false;
        }
        job
    }

    /// The bytes `[job.from, job.to)`: the batch in memory, with any earlier bytes read back from
    /// the local copy (a replica that lags, with `min_acks` below `replicas`).
    async fn send_bytes(&self, seg: &Seg, job: &SendJob) -> Result<Bytes, HotError> {
        match &job.cur {
            Some((start, bytes)) if *start == job.from => Ok(bytes.clone()),
            cur => {
                let (head_end, tail) = cur
                    .as_ref()
                    .map_or((job.to, None), |(start, bytes)| (*start, Some(bytes)));
                let (dev, path, from) = (Arc::clone(&self.dev), seg.path.clone(), job.from);
                let mut out = self
                    .sched
                    .run(DriveClass::Write, move || {
                        dev.read_at(&path, from, head_end - from)
                    })
                    .await?
                    .map_err(io_err("segment read"))?;
                if let Some(t) = tail {
                    out.extend_from_slice(t);
                }
                Ok(Bytes::from(out))
            }
        }
    }

    /// Whether this node promised a ballot above `opened` for `segment`: a takeover fences the
    /// writer, which then acknowledges nothing more.
    pub(crate) fn fenced(&self, segment: BlockId, opened: Ballot) -> bool {
        self.promised(segment).is_some_and(|b| b > opened)
    }
}

struct Batch {
    start: u64,
    bytes: Bytes,
    taken: Vec<(Pending, u64)>,
    full: bool,
}

struct SendJob {
    from: u64,
    to: u64,
    crc: u32,
    cur: Option<(u64, Bytes)>,
}

/// What the sender of `node` sends next: the batch in memory, or the bytes from the replica's
/// committed length, or the bare notice that everything before it is acknowledged.
fn plan_send(s: &SegSt, node: NodeId) -> Option<SendJob> {
    let link = s.links.get(&node).copied().unwrap_or_default();
    if link.committed > s.acked {
        return None;
    }
    let from = link.committed;
    if from < s.written {
        // The running crc at the batch end is kept until every replica passes it.
        let crc = s.crcs.get(&s.written).copied().unwrap_or_default();
        Some(SendJob {
            from,
            to: s.written,
            crc,
            cur: s.cur.clone(),
        })
    } else if link.told < s.acked {
        let crc = s.crcs.get(&from).copied().unwrap_or_default();
        Some(SendJob {
            from,
            to: from,
            crc,
            cur: None,
        })
    } else {
        None
    }
}

fn retry_all(taken: Vec<(Pending, u64)>) {
    for (p, _) in taken {
        let _ = p.tx.send(Outcome::Retry);
    }
}

fn fail_all(taken: Vec<(Pending, u64)>, e: &HotError) {
    for (p, _) in taken {
        let err = HotError::Io {
            op: "segment write",
            source: std::io::Error::other(e.to_string()),
        };
        let _ = p.tx.send(Outcome::Failed(err));
    }
}
