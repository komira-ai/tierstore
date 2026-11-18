//! A simulated network between storage nodes (section 14: "scripted interleavings and message
//! loss between nodes"; the "deterministic simulation" of the two-sealers and stamp rows).
//!
//! [`SimNet`] is a value: the messages in flight and the links that are cut. Nothing is delivered
//! on its own. The driver (a test, or a model's successor function) picks which message arrives
//! next with [`SimNet::take`], loses one with [`SimNet::lose`], or cuts links with
//! [`SimNet::cut`] and [`SimNet::partition`]. Every ordering of deliveries is therefore a choice
//! the driver makes, and an exhaustive explorer enumerates them all.
//!
//! The messages in flight are kept sorted (a multiset), so two runs that sent the same messages in
//! another order are the same state: [`SimNet`] is `Eq + Hash` when the message type is, and an
//! explorer's state can hold it.
//!
//! A cut link is directed: `cut(a, b)` drops what `a` sends to `b` from then on (an asymmetric
//! partition), and also loses what was already in flight from `a` to `b`. A node that crashes
//! loses what was in flight to it ([`SimNet::crash`]).

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use tierstore_format::ids::NodeId;

/// One message in flight.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Envelope<M> {
    /// The sender.
    pub from: NodeId,
    /// The receiver.
    pub to: NodeId,
    /// The message.
    pub msg: M,
}

/// Messages in flight between nodes and the cut links.
///
/// Two networks are equal when the same messages are in flight and the same links are cut; the
/// delivered and lost counts are statistics and take no part, so an explorer does not tell apart
/// two paths that reach the same network.
#[derive(Debug, Clone)]
pub struct SimNet<M> {
    /// Sorted, so the order of sends does not make a different state.
    in_flight: Vec<Envelope<M>>,
    /// Directed links `(from, to)` that drop every message.
    cut: BTreeSet<(NodeId, NodeId)>,
    delivered: u64,
    lost: u64,
}

impl<M: PartialEq> PartialEq for SimNet<M> {
    fn eq(&self, other: &Self) -> bool {
        self.in_flight == other.in_flight && self.cut == other.cut
    }
}

impl<M: Eq> Eq for SimNet<M> {}

impl<M: Hash> Hash for SimNet<M> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.in_flight.hash(state);
        self.cut.hash(state);
    }
}

impl<M> Default for SimNet<M> {
    fn default() -> Self {
        Self {
            in_flight: Vec::new(),
            cut: BTreeSet::new(),
            delivered: 0,
            lost: 0,
        }
    }
}

impl<M: Ord + Clone> SimNet<M> {
    /// A network with nothing in flight and every link up.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends `msg` from `from` to `to`; whether it is in flight (a cut link loses it at once and
    /// counts it lost).
    pub fn send(&mut self, from: NodeId, to: NodeId, msg: M) -> bool {
        if self.is_cut(from, to) {
            self.lost += 1;
            return false;
        }
        let env = Envelope { from, to, msg };
        let at = self.in_flight.partition_point(|e| e <= &env);
        self.in_flight.insert(at, env);
        true
    }

    /// Sends a copy of `msg` from `from` to each of `to`; the number in flight.
    pub fn broadcast(
        &mut self,
        from: NodeId,
        to: impl IntoIterator<Item = NodeId>,
        msg: &M,
    ) -> usize {
        to.into_iter()
            .filter(|t| self.send(from, *t, msg.clone()))
            .count()
    }

    /// The messages in flight, sorted.
    #[must_use]
    pub fn in_flight(&self) -> &[Envelope<M>] {
        &self.in_flight
    }

    /// Whether nothing is in flight.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.in_flight.is_empty()
    }

    /// Delivers the message at `index` of [`SimNet::in_flight`]: removes and returns it, or
    /// `None` when there is no such message.
    pub fn take(&mut self, index: usize) -> Option<Envelope<M>> {
        (index < self.in_flight.len()).then(|| {
            self.delivered += 1;
            self.in_flight.remove(index)
        })
    }

    /// Loses the message at `index`: removes it and counts it lost; `None` when there is no such
    /// message.
    pub fn lose(&mut self, index: usize) -> Option<Envelope<M>> {
        (index < self.in_flight.len()).then(|| {
            self.lost += 1;
            self.in_flight.remove(index)
        })
    }

    /// Cuts the directed link `from -> to`: what was in flight on it is lost, and so is every
    /// later send on it until [`SimNet::heal`].
    pub fn cut(&mut self, from: NodeId, to: NodeId) {
        self.cut.insert((from, to));
        self.drop_where(|e| e.from == from && e.to == to);
    }

    /// Restores the directed link `from -> to`.
    pub fn heal(&mut self, from: NodeId, to: NodeId) {
        self.cut.remove(&(from, to));
    }

    /// Restores every link.
    pub fn heal_all(&mut self) {
        self.cut.clear();
    }

    /// Whether `from -> to` is cut.
    #[must_use]
    pub fn is_cut(&self, from: NodeId, to: NodeId) -> bool {
        self.cut.contains(&(from, to))
    }

    /// A symmetric partition: every link between a node of `a` and a node of `b` is cut both
    /// ways.
    pub fn partition(&mut self, a: &[NodeId], b: &[NodeId]) {
        for x in a {
            for y in b {
                self.cut(*x, *y);
                self.cut(*y, *x);
            }
        }
    }

    /// A crash of `node`: every message in flight to it is lost (a message it sent before the
    /// crash still arrives).
    pub fn crash(&mut self, node: NodeId) {
        self.drop_where(|e| e.to == node);
    }

    /// The messages delivered so far.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The messages lost so far (by [`SimNet::lose`], a cut link or a crash).
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost
    }

    fn drop_where(&mut self, gone: impl Fn(&Envelope<M>) -> bool) {
        let before = self.in_flight.len();
        self.in_flight.retain(|e| !gone(e));
        self.lost += (before - self.in_flight.len()) as u64;
    }
}
