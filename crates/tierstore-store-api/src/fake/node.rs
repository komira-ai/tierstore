//! One fake storage node: its durable state, its scripts, and the entry every call goes through.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::sync::Mutex;

use bytes::Bytes;
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_format::FormatError;
use tierstore_format::descriptor::StripeDescriptor;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, Generation, NodeId, StripeId};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::LogRecord;

use super::lock;
use super::script::{Outcome, Route, Step};
use crate::peer::{PeerError, PeerGet};
use crate::proto::{ColdExtent, DrivePing, Envelope, HexBytes, HotCopy};

/// Where a stored object lives.
#[derive(Debug, Clone)]
pub(super) enum Tier {
    /// On the hot tier: the copies, as `LOCATE` lists them.
    Hot(Vec<HotCopy>),
    /// On the cold tier: the extents, as `LOCATE` lists them.
    Cold(Vec<ColdExtent>),
}

/// One stored object.
#[derive(Debug, Clone)]
pub(super) struct Stored {
    pub(super) generation: Generation,
    pub(super) bytes: Bytes,
    pub(super) tier: Tier,
    /// The node's clock when the object was put.
    pub(super) stamp: Stamp,
}

/// A replica's copy of a hot segment.
#[derive(Debug, Clone)]
pub(super) struct Replica {
    pub(super) bytes: Vec<u8>,
    pub(super) promised: Ballot,
    pub(super) sealed: Option<SealProposal>,
}

/// A voter's durable state for one segment's seal.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Voter {
    pub(super) promised: Option<Ballot>,
    pub(super) accepted: Option<(Ballot, SealProposal)>,
    pub(super) committed: Option<SealProposal>,
}

/// Everything a node holds, behind one mutex.
#[derive(Debug, Default)]
pub(super) struct State {
    pub(super) crashed: bool,
    pub(super) stamp: u64,
    pub(super) mem_epoch: u64,
    pub(super) indexing: bool,
    pub(super) objects: BTreeMap<String, Stored>,
    pub(super) tombstones: BTreeMap<String, Stamp>,
    pub(super) replicas: BTreeMap<BlockId, Replica>,
    pub(super) voters: BTreeMap<BlockId, Voter>,
    pub(super) descriptors: BTreeMap<StripeId, StripeDescriptor>,
    pub(super) frees: BTreeMap<StripeId, StripeFree>,
    pub(super) logs: BTreeMap<NodeId, Vec<(u64, HexBytes)>>,
    pub(super) drives: Vec<DrivePing>,
    pub(super) hot_queued: u32,
    pub(super) scripts: BTreeMap<Route, VecDeque<Step>>,
    pub(super) calls: BTreeMap<Route, usize>,
    pub(super) reports: Vec<PeerGet>,
}

/// What a call learned on its way in: how long it took and its place in the node's stamp.
pub(super) struct Entered {
    pub(super) service_ms: u64,
    pub(super) stamp: Stamp,
    pub(super) mem_epoch: u64,
}

/// An in-memory storage node answering the peer protocol, with scripted service times, hangs,
/// errors and crashes on an injected clock. See the module documentation of [`crate::fake`].
#[derive(Debug)]
pub struct FakeStorageNode {
    pub(super) id: NodeId,
    pub(super) clock: ManualClock,
    pub(super) state: Mutex<State>,
}

impl FakeStorageNode {
    /// A node `id` that is up, empty, indexed, and scripted to answer at once.
    #[must_use]
    pub fn new(id: NodeId, clock: ManualClock) -> Self {
        Self {
            id,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    /// The node's id.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Stores `bytes` as a hot object of `key`, with the copies `LOCATE` lists. The record's
    /// stamp is the node's clock after one tick.
    pub fn put_hot(&self, key: &str, generation: Generation, bytes: Bytes, copies: Vec<HotCopy>) {
        self.put(key, generation, bytes, Tier::Hot(copies));
    }

    /// Stores `bytes` as a cold object of `key`, with the extents `LOCATE` lists. The record's
    /// stamp is the node's clock after one tick.
    pub fn put_cold(
        &self,
        key: &str,
        generation: Generation,
        bytes: Bytes,
        extents: Vec<ColdExtent>,
    ) {
        self.put(key, generation, bytes, Tier::Cold(extents));
    }

    fn put(&self, key: &str, generation: Generation, bytes: Bytes, tier: Tier) {
        let mut st = lock(&self.state);
        st.stamp += 1;
        let stamp = Stamp::from_raw(st.stamp);
        st.objects.insert(
            key.to_owned(),
            Stored {
                generation,
                bytes,
                tier,
                stamp,
            },
        );
    }

    /// Deletes `key`: the object goes and `stamp` is kept as its tombstone, so `LOCATE` answers
    /// `Absent` and a key check reports the tombstone.
    pub fn delete(&self, key: &str, stamp: Stamp) {
        let mut st = lock(&self.state);
        st.objects.remove(key);
        st.tombstones.insert(key.to_owned(), stamp);
    }

    /// Sets whether the node's index is not built (`LOCATE` and key checks answer `Indexing`).
    pub fn set_indexing(&self, indexing: bool) {
        lock(&self.state).indexing = indexing;
    }

    /// Sets the membership epoch every answer carries.
    pub fn set_mem_epoch(&self, epoch: u64) {
        lock(&self.state).mem_epoch = epoch;
    }

    /// Sets the ping's entry of `ping.drive`, replacing the one of the same drive.
    pub fn set_drive(&self, ping: DrivePing) {
        let mut st = lock(&self.state);
        match st.drives.iter_mut().find(|d| d.drive == ping.drive) {
            Some(slot) => *slot = ping,
            None => st.drives.push(ping),
        }
    }

    /// Sets the hot device's queued requests in the ping.
    pub fn set_hot_queued(&self, queued: u32) {
        lock(&self.state).hot_queued = queued;
    }

    /// Appends `record` to the tombstone log of `record.origin`, kept in the order appended. The
    /// record must be the next of its origin's log: sequence 1 for an empty log, else the last
    /// sequence number plus one.
    pub fn append_tomb(&self, record: LogRecord) -> Result<(), FormatError> {
        let bytes = HexBytes(record.encode()?);
        let mut st = lock(&self.state);
        let log = st.logs.entry(record.origin).or_default();
        let next = log.last().map_or(1, |(seq, _)| seq + 1);
        if record.seq != next {
            return Err(FormatError::Invalid {
                what: "tomb log entry",
                reason: "not the next sequence number of its origin",
            });
        }
        log.push((record.seq, bytes));
        Ok(())
    }

    /// Queues `step` for the next call of `route` that no earlier step is waiting for.
    pub fn script(&self, route: Route, step: Step) {
        lock(&self.state)
            .scripts
            .entry(route)
            .or_default()
            .push_back(step);
    }

    /// The calls `route` received, failed and crashed ones included.
    #[must_use]
    pub fn calls(&self, route: Route) -> usize {
        lock(&self.state).calls.get(&route).copied().unwrap_or(0)
    }

    /// The GETs that carried an `x-ts-report` and were answered, in order.
    #[must_use]
    pub fn reports(&self) -> Vec<PeerGet> {
        lock(&self.state).reports.clone()
    }

    /// Crashes the node now: every call is `Unreachable` until [`FakeStorageNode::restart`].
    pub fn crash(&self) {
        lock(&self.state).crashed = true;
    }

    /// Whether the node is crashed.
    #[must_use]
    pub fn is_crashed(&self) -> bool {
        lock(&self.state).crashed
    }

    /// Restarts a crashed node: durable state and scripts stay.
    pub fn restart(&self) {
        lock(&self.state).crashed = false;
    }

    /// The bytes of the replica's copy of `segment`, if the node holds one.
    #[must_use]
    pub fn copy(&self, segment: BlockId) -> Option<Vec<u8>> {
        lock(&self.state)
            .replicas
            .get(&segment)
            .map(|r| r.bytes.clone())
    }

    /// The descriptor the node holds of `stripe`.
    #[must_use]
    pub fn descriptor(&self, stripe: StripeId) -> Option<StripeDescriptor> {
        lock(&self.state).descriptors.get(&stripe).cloned()
    }

    /// The free the node holds of `stripe`.
    #[must_use]
    pub fn freed(&self, stripe: StripeId) -> Option<StripeFree> {
        lock(&self.state).frees.get(&stripe).copied()
    }

    /// The node's current stamp value.
    #[must_use]
    pub fn stamp(&self) -> u64 {
        lock(&self.state).stamp
    }

    /// The common start of every call: counts it, refuses it when the node is crashed, ticks the
    /// stamp (after merging the envelope's), takes the route's next step, and acts on it: a crash
    /// or a hang at once, any other step after its service time on the injected clock.
    pub(super) async fn enter(
        &self,
        route: Route,
        seen: Option<Stamp>,
    ) -> Result<Entered, PeerError> {
        let (step, stamp, mem_epoch, now) = {
            let mut st = lock(&self.state);
            *st.calls.entry(route).or_default() += 1;
            if st.crashed {
                return Err(self.unreachable("the node is crashed"));
            }
            let seen = seen.map_or(0, Stamp::raw);
            st.stamp = st.stamp.max(seen) + 1;
            let step = st
                .scripts
                .get_mut(&route)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Step::answer(0));
            (
                step,
                Stamp::from_raw(st.stamp),
                st.mem_epoch,
                self.clock.now_ms(),
            )
        };
        match step.outcome {
            Outcome::Crash => {
                lock(&self.state).crashed = true;
                return Err(self.unreachable("the node crashed"));
            }
            Outcome::Hang => match std::future::pending::<Infallible>().await {},
            Outcome::Answer | Outcome::Fail(_) => {}
        }
        self.clock
            .sleep_until(now.saturating_add(step.service_ms))
            .await;
        if let Outcome::Fail(error) = step.outcome {
            return Err(error);
        }
        Ok(Entered {
            service_ms: step.service_ms,
            stamp,
            mem_epoch,
        })
    }

    pub(super) fn unreachable(&self, reason: &str) -> PeerError {
        PeerError::Unreachable {
            node: self.id,
            reason: reason.to_owned(),
        }
    }

    pub(super) fn protocol(&self, status: u16, reason: impl ToString) -> PeerError {
        PeerError::Protocol {
            node: self.id,
            status,
            reason: reason.to_string(),
        }
    }

    pub(super) fn envelope<T>(&self, entered: &Entered, body: T) -> Envelope<T> {
        Envelope {
            from: self.id,
            stamp: entered.stamp,
            mem_epoch: entered.mem_epoch,
            body,
        }
    }
}

/// The ballot a replica's copy opens under.
pub(super) fn opening(ballot: Ballot) -> Replica {
    Replica {
        bytes: Vec::new(),
        promised: ballot,
        sealed: None,
    }
}
