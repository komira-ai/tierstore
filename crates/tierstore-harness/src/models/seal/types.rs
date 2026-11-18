//! The values of the seal model: what a node holds, what travels between nodes, and the whole
//! state the explorer fingerprints.

use std::collections::{BTreeMap, BTreeSet};

use tierstore_format::ids::NodeId;
use tierstore_format::seal::Ballot;
use tierstore_store_api::hot::ReplicaReject;

use crate::net::SimNet;

/// A seal proposal: the sealed length and the running crc32c of `[0, length)`. It mirrors
/// `tierstore_format::seal::SealProposal`, which has no total order, and a message in flight
/// must be ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Prop {
    /// The sealed length.
    pub length: u64,
    /// The crc32c of the segment's bytes `[0, length)`.
    pub crc: u32,
}

/// What a copy reports with its promise; it mirrors `tierstore_store_api::hot::CopyState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cp {
    /// The committed length.
    pub committed: u64,
    /// The crc-verified prefix; below `committed` the copy does not answer for Choose.
    pub verified: u64,
    /// The running crc32c through `verified`.
    pub crc: u32,
}

/// What one node answers to a prepare: the proposal it accepted and, if it holds a copy, the
/// copy's state; it mirrors `tierstore_store_api::hot::Promise` without the ballot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Answer {
    /// The accepted proposal with the ballot it was accepted under.
    pub accepted: Option<(Ballot, Prop)>,
    /// The node's copy, if it holds one (a voter holds none).
    pub copy: Option<Cp>,
}

/// Why a replica refused a batch: the kind of `ReplicaReject`, without its numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reject {
    /// The start offset was not the copy's committed length.
    Offset,
    /// The running crc did not match.
    Crc,
    /// The ballot was below the copy's promise.
    Ballot,
    /// The copy is sealed.
    Sealed,
}

impl From<ReplicaReject> for Reject {
    fn from(r: ReplicaReject) -> Self {
        match r {
            ReplicaReject::Offset { .. } => Self::Offset,
            ReplicaReject::Crc { .. } => Self::Crc,
            ReplicaReject::Ballot { .. } => Self::Ballot,
            ReplicaReject::Sealed => Self::Sealed,
        }
    }
}

/// A message between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Msg {
    /// A replication batch (10.3 step 1): the bytes are the segment's `[start, start + len)`.
    Replicate {
        /// The writer's ballot.
        ballot: Ballot,
        /// The offset the bytes start at.
        start: u64,
        /// The number of bytes.
        len: u64,
        /// The crc32c of the segment's bytes through the batch.
        crc: u32,
    },
    /// A replica's acknowledgement: its committed length after the batch.
    Ack {
        /// The committed length.
        committed: u64,
    },
    /// A replica's refusal of a batch.
    Reject(Reject),
    /// `seal-prepare`.
    Prepare {
        /// The sealer's ballot.
        ballot: Ballot,
    },
    /// A durable promise with what the node holds.
    Promise {
        /// The ballot promised.
        ballot: Ballot,
        /// The accepted proposal and the copy.
        answer: Answer,
    },
    /// A refusal of a prepare or an accept: the node promised a higher ballot.
    Refused {
        /// The ballot that was refused.
        ballot: Ballot,
        /// The higher promise.
        promised: Ballot,
    },
    /// A refusal because the seal is committed: the committed proposal.
    Committed(Prop),
    /// `seal-accept`.
    Accept {
        /// The sealer's ballot.
        ballot: Ballot,
        /// The proposal.
        prop: Prop,
    },
    /// A durable accept.
    Accepted {
        /// The ballot accepted.
        ballot: Ballot,
    },
    /// The broadcast of a committed seal.
    Sealed(Prop),
}

/// A sealer's volatile progress.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Sealer {
    /// Not sealing (never started, abandoned, superseded, or done).
    Idle,
    /// Collecting promises.
    Preparing {
        /// The ballot.
        ballot: Ballot,
        /// The answers received, its own included.
        promises: BTreeMap<NodeId, Answer>,
    },
    /// Collecting accepts of its proposal.
    Accepting {
        /// The ballot.
        ballot: Ballot,
        /// The proposal.
        prop: Prop,
        /// The nodes that accepted it durably, its own included.
        accepts: BTreeSet<NodeId>,
    },
}

impl Sealer {
    /// The ballot of the attempt in progress.
    #[must_use]
    pub fn ballot(&self) -> Option<Ballot> {
        match self {
            Self::Idle => None,
            Self::Preparing { ballot, .. } | Self::Accepting { ballot, .. } => Some(*ballot),
        }
    }
}

/// One storage node: its durable state, then its volatile state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Peer {
    /// Whether it is running (only the writer crashes).
    pub up: bool,
    /// The committed length of its copy (a voter has none and stays 0). Durable.
    pub len: u64,
    /// Whether its copy has damage inside its acknowledged prefix (verified length 0). Durable.
    pub bad: bool,
    /// The highest ballot promised. Durable.
    pub promised: Ballot,
    /// The proposal accepted. Durable.
    pub accepted: Option<(Ballot, Prop)>,
    /// The committed seal it knows. Durable.
    pub committed: Option<Prop>,
    /// The highest ballot it has seen refused, to pick a higher one. Volatile.
    pub seen: Ballot,
    /// Its seal attempt in progress. Volatile.
    pub sealer: Sealer,
    /// How many seal attempts it has started; a crash does not reset it, it bounds the scenario.
    pub attempts: u8,
}

/// The writer's volatile state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Writer {
    /// Whether it still appends batches.
    pub writing: bool,
    /// The end of the last batch it began.
    pub sent: u64,
    /// The batch (start, end) whose local `fdatasync` is not done.
    pub pending: Option<(u64, u64)>,
    /// The committed length each node last acknowledged to it.
    pub rep: [u64; 3],
    /// Its last acknowledged offset as it knows it; the floor of its own proposals.
    pub known: u64,
}

/// The whole state of the model: the three nodes, the network and the ghost facts the
/// properties are judged by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct World {
    /// Nodes 1, 2 and 3 (index = id - 1); node 1 is the writer.
    pub nodes: [Peer; 3],
    /// The messages in flight.
    pub net: SimNet<Msg>,
    /// The writer's volatile state.
    pub writer: Writer,
    /// Ghost: the length acknowledged to clients; a crash does not reset it.
    pub acked: u64,
    /// Ghost: the segment as the destager read it.
    pub destaged: Option<Prop>,
    /// Ghost: messages lost by the explorer so far.
    pub losses: u8,
    /// Ghost: the writer crashed once.
    pub crashed: bool,
    /// Ghost: a copy was damaged.
    pub damaged: bool,
    /// Ghost: a replica appended a batch whose ballot was below its promise (S4).
    pub s4: bool,
    /// Ghost: a node's committed seal was asked to change (S2).
    pub s2: bool,
    /// Ghost: a batch was refused for its ballot (the fence engaged).
    pub fenced: bool,
}

/// The index of a node in `World::nodes`.
#[must_use]
pub fn ix(n: NodeId) -> usize {
    usize::from(n.0) - 1
}

/// The id of the node at index `i`.
#[must_use]
pub fn nid(i: usize) -> NodeId {
    NodeId(u8::try_from(i + 1).unwrap_or(u8::MAX))
}
